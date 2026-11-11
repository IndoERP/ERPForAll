//! Ordered, caller-authorized partner synchronization.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use erp_sdk::prelude::*;
use erp_sdk::{Operation, Operator};

use crate::partner_address::{cells, fields, has_address, replacement};
use crate::partner_address_budget::AssignmentBudget;

#[derive(Default)]
struct State {
    depth: usize,
    guards: usize,
    visited: HashSet<i64>,
    assignments: AssignmentBudget,
    draining: bool,
    logical_depth: usize,
    pending: Vec<SyncWork>,
    queued_units: usize,
    admitted_units: usize,
}

#[derive(Default)]
struct Slot(Arc<Mutex<State>>);

struct SyncWork {
    records: Records,
    changes: Vec<RawVals>,
    depth: usize,
    units: usize,
}

struct BatchWork {
    records: Records,
    address_fields: Vec<String>,
    remaining: VecDeque<(Records, RawVals)>,
    downstream: Vec<(i64, RawVals)>,
    depth: usize,
    units: usize,
}

enum Work {
    Synchronize(SyncWork),
    Batch(BatchWork),
    Downstream {
        records: Records,
        changes: Vec<(i64, RawVals)>,
        depth: usize,
        units: usize,
    },
}

/// A driver never retains queued values after returning, including when its future
/// is cancelled while an outer native write guard is still alive.
struct DriverGuard(Arc<Mutex<State>>);

impl Drop for DriverGuard {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.pending.clear();
        state.queued_units = 0;
        state.admitted_units = 0;
        state.logical_depth = 0;
        state.draining = false;
    }
}

fn enqueue(state: &mut State, records: &Records, changes: Vec<RawVals>) -> Result<()> {
    let depth = if state.draining {
        state
            .logical_depth
            .checked_add(1)
            .ok_or_else(depth_exceeded)?
    } else {
        // OperationGuard counts the first native entry; traversal starts at zero.
        state.depth.checked_sub(1).ok_or_else(|| {
            Error::programming("Partner address work requires an active operation")
        })?
    };
    if depth > 256 {
        return Err(depth_exceeded());
    }
    visit(state, records)?;
    let units = changes.iter().try_fold(0usize, |count, change| {
        count
            .checked_add(change.len().max(1))
            .ok_or_else(|| Error::programming("Partner address admitted input footprint overflow"))
    })?;
    admit_input(state, units)?;
    // Input is already admitted; move its maps into the queue without imposing
    // the scheduled-copy limit on input width or record bookkeeping.
    state.pending.push(SyncWork {
        records: records.clone(),
        changes,
        depth,
        units,
    });
    Ok(())
}

/// Input maps move through Sync and Batch without cloning. Each consumed map
/// leaves at most one record-bookkeeping unit plus a downstream field subset no
/// larger than that input map. Thus twice the cumulative admitted footprint bounds
/// all staging/stack overlap. This is an internal invariant, not an input limit.
/// Framework-generated inputs arise only after schedule's separate copy charge;
/// the accepted identity/depth limits and actual assignment budget remain unchanged.
fn admit_input(state: &mut State, units: usize) -> Result<()> {
    let admitted = state
        .admitted_units
        .checked_add(units)
        .ok_or_else(|| Error::programming("Partner address admitted input footprint overflow"))?;
    let queued = checked_reservation(state, units, admitted)?;
    state.admitted_units = admitted;
    state.queued_units = queued;
    Ok(())
}

fn reserve_units(state: &mut State, units: usize) -> Result<()> {
    state.queued_units = checked_reservation(state, units, state.admitted_units)?;
    Ok(())
}

fn checked_reservation(state: &State, units: usize, admitted: usize) -> Result<usize> {
    let envelope = admitted
        .checked_mul(2)
        .ok_or_else(|| Error::programming("Partner address buffered-work envelope overflow"))?;
    let next = state
        .queued_units
        .checked_add(units)
        .ok_or_else(|| Error::programming("Partner address buffered-work accounting overflow"))?;
    if next > envelope {
        return Err(Error::programming(
            "Partner address buffered work exceeds its admitted-input envelope",
        ));
    }
    Ok(next)
}

fn release_units(state: &mut State, units: usize) -> Result<()> {
    state.queued_units = state
        .queued_units
        .checked_sub(units)
        .ok_or_else(|| Error::programming("Partner address queued-work accounting underflow"))?;
    Ok(())
}

fn depth_exceeded() -> Error {
    Error::validation("Partner address maximum depth of 256 exceeded.")
}

fn take_pending(state: &Arc<Mutex<State>>, work: &mut Vec<Work>) -> Result<()> {
    let pending = {
        let mut state = locked(state)?;
        // Moving frames to the driver's stack retains their admission charge.
        std::mem::take(&mut state.pending)
    };
    // The first nested call finishes its propagation before later sibling calls.
    work.extend(pending.into_iter().rev().map(Work::Synchronize));
    Ok(())
}

fn locked(state: &Mutex<State>) -> Result<MutexGuard<'_, State>> {
    state
        .lock()
        .map_err(|_| Error::programming("Partner address operation state is poisoned"))
}

fn visit(state: &mut State, records: &Records) -> Result<()> {
    for id in records.rec_ids() {
        if !state.visited.contains(&id.get()) && state.visited.len() == 4096 {
            return Err(Error::validation(
                "Partner address maximum visited nodes limit of 4096 exceeded.",
            ));
        }
        state.visited.insert(id.get());
    }
    Ok(())
}

/// One operation's shared accounting, including reentrant native addon calls.
pub(crate) struct OperationGuard {
    state: Arc<Mutex<State>>,
    counted_depth: bool,
}

impl OperationGuard {
    pub(crate) fn enter(tx: &mut Transaction, records: &Records) -> Result<Self> {
        Self::scope(tx, records, true)
    }

    fn join(tx: &mut Transaction, records: &Records) -> Result<Self> {
        Self::scope(tx, records, false)
    }

    fn scope(tx: &mut Transaction, records: &Records, entry: bool) -> Result<Self> {
        let state = Arc::clone(&tx.extension::<Slot>().0);
        let counted_depth;
        {
            let mut current = locked(&state)?;
            if current.guards == 0 {
                *current = State::default();
            }
            counted_depth = entry || current.depth == 0;
            // The initial operation is depth zero, matching address_get traversal.
            if counted_depth && current.depth == 257 {
                return Err(Error::validation(
                    "Partner address maximum depth of 256 exceeded.",
                ));
            }
            visit(&mut current, records)?;
            if counted_depth {
                current.depth += 1;
            }
            current.guards += 1;
        }
        Ok(Self {
            state,
            counted_depth,
        })
    }

    pub(crate) fn admit(&self, records: &Records) -> Result<()> {
        let mut state = locked(&self.state)?;
        visit(&mut state, records)
    }

    fn schedule(&self, records: &Records, values: &RawVals) -> Result<()> {
        let mut state = locked(&self.state)?;
        visit(&mut state, records)?;
        let count = records.len().checked_mul(values.len()).ok_or_else(|| {
            Error::validation(
                "Partner address synchronization maximum field assignments of 65536 exceeded.",
            )
        })?;
        state.assignments.charge(count)
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        // A poisoned mutex remains poisoned for future callers; cleanup still releases
        // this guard's private state, including when an awaited write is cancelled.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.guards -= 1;
        if self.counted_depth {
            state.depth -= 1;
        }
        if state.guards == 0 {
            *state = State::default();
        }
    }
}

fn names(values: &RawVals) -> Vec<String> {
    values
        .iter()
        .map(|(name, _)| name.to_owned())
        .chain(values.iter_commands().map(|(name, _)| name.to_owned()))
        .collect()
}

/// Snapshot submitted fields before normal write dispatch, with ordinary read checks.
pub(crate) async fn before_write(
    tx: &mut Transaction,
    records: &Records,
    values: &RawVals,
) -> Result<Vec<RawVals>> {
    records.check_access(tx, Operation::Write).await?;
    cells(tx, records, &names(values)).await
}

pub(crate) async fn changed_values(
    tx: &mut Transaction,
    records: &Records,
    values: &RawVals,
    before: &[RawVals],
) -> Result<Vec<RawVals>> {
    let mut after = cells(tx, records, &names(values)).await?;
    if before.len() != after.len() {
        return Err(Error::programming(
            "res.partner.write: snapshot cardinality changed",
        ));
    }
    for (old, new) in before.iter().zip(&mut after) {
        let equal: Vec<String> = new
            .iter()
            .filter(|(name, value)| old.get(name) == Some(*value))
            .map(|(name, _)| name.to_owned())
            .collect();
        for name in equal {
            new.remove(&name);
        }
    }
    Ok(after)
}

/// Consume the related parent_name input before it can rename an existing parent.
pub(crate) async fn parent_names(
    tx: &mut Transaction,
    records: &Records,
    inputs: &mut [RawVals],
) -> Result<Vec<Option<i64>>> {
    let context_name = records.env().context().get("default_parent_name");
    if context_name.is_none() && inputs.iter().all(|values| !values.contains("parent_name")) {
        return Ok(vec![None; inputs.len()]);
    }
    records.check_access(tx, Operation::Create).await?;
    let address_fields = fields(tx, records, "_address_fields").await?;
    let mut requested = address_fields.clone();
    requested.extend(["vat".to_owned(), "lang".to_owned(), "parent_id".to_owned()]);
    let defaults: erp_sdk::ValueMap = records.call(tx, "default_get", (requested,)).await?;
    let defaults = records.vals_from_values(&defaults)?;
    let mut created_parents = Vec::with_capacity(inputs.len());
    for values in inputs {
        let name = match values.remove("parent_name") {
            Some(Cell::Null) => None,
            Some(Cell::Str(name)) => Some(name),
            Some(_) => return Err(Error::validation("res.partner.parent_name must be text")),
            None => match &context_name {
                Some(Value::Str(name)) => Some(Arc::clone(name)),
                Some(Value::False | Value::Bool(false)) | None => None,
                Some(_) => return Err(Error::validation("res.partner.parent_name must be text")),
            },
        };
        let parent = values
            .get("parent_id")
            .or_else(|| defaults.get("parent_id"));
        // A native RawVals caller must not lose an invalid cell when parent_name
        // creates a root. Preserve the ordinary many2one storage-type refusal.
        if let Some(parent) = parent
            && !matches!(parent, Cell::Null | Cell::Id(_))
        {
            return Err(Error::programming(format!(
                "invalid value {parent:?} for field {}.parent_id of type many2one",
                records.spec().name
            )));
        }
        if matches!(parent, Some(Cell::Id(_))) || name.as_ref().is_none_or(|name| name.is_empty()) {
            created_parents.push(None);
            continue;
        }
        let name =
            name.ok_or_else(|| Error::programming("Partner parent-name input disappeared"))?;
        let mut parent_values = RawVals::new()
            .set_cell("name", Cell::Str(name))
            .set_cell("parent_id", Cell::Null);
        for field in address_fields
            .iter()
            .map(String::as_str)
            .chain(["vat", "lang"])
        {
            if let Some(value) = values.get(field).or_else(|| defaults.get(field)) {
                parent_values.insert(field.to_owned().into(), value.clone());
            }
            if let Some(commands) = values.commands(field) {
                parent_values.insert_commands(field.to_owned().into(), commands.to_vec());
            }
        }
        let mut context = records.env().context().clone();
        for key in [
            "default_parent_id",
            "default_parent_name",
            "default_child_ids",
        ] {
            context.remove(key);
        }
        let parent_model = records.with_env(records.env().with_context_replaced(context));
        let parent = parent_model.create(tx, vec![parent_values]).await?;
        if parent.len() != 1 {
            return Err(Error::programming(
                "res.partner.create: parent creation must return one record",
            ));
        }
        let parent_id = parent.rec_ids()[0].get();
        values.insert("parent_id".into(), Cell::Id(parent_id));
        created_parents.push(Some(parent_id));
    }
    Ok(created_parents)
}

/// Create passes effective defaults as well as supplied values to the sync hook.
pub(crate) async fn created_changes(
    tx: &mut Transaction,
    records: &Records,
    inputs: &[RawVals],
) -> Result<Vec<RawVals>> {
    if records.len() != inputs.len() {
        return Err(Error::programming(
            "res.partner.create: result cardinality differs from inputs",
        ));
    }
    let mut names = fields(tx, records, "_address_fields").await?;
    names.extend(["parent_id".to_owned(), "type".to_owned()]);
    for values in inputs {
        for name in values
            .iter()
            .map(|(name, _)| name)
            .chain(values.iter_commands().map(|(name, _)| name))
        {
            if !names.iter().any(|present| present == name) {
                names.push(name.to_owned());
            }
        }
    }
    cells(tx, records, &names).await
}

async fn single_address(
    tx: &mut Transaction,
    records: &Records,
    address_fields: &[String],
) -> Result<RawVals> {
    cells(tx, records, address_fields)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::programming("Partner address snapshot requires one record"))
}

#[derive(Clone, Copy)]
enum WriteMode {
    Upstream,
    Inherited,
    Downstream,
}

async fn copy_values(
    tx: &mut Transaction,
    operation: &OperationGuard,
    records: &Records,
    desired: &RawVals,
    mode: WriteMode,
) -> Result<()> {
    if records.is_empty() || desired.is_empty() {
        return Ok(());
    }
    operation.admit(records)?;
    let snapshots = cells(tx, records, &names(desired)).await?;
    let mut groups: Vec<(RawVals, Vec<i64>)> = Vec::new();
    for (id, snapshot) in records.rec_ids().iter().zip(snapshots) {
        let mut delta = RawVals::new();
        for (name, value) in desired.iter() {
            if snapshot.get(name) != Some(value) {
                delta.insert(name.to_owned().into(), value.clone());
            }
        }
        if delta.is_empty() {
            continue;
        }
        if let Some((_, ids)) = groups.iter_mut().find(|(values, _)| values == &delta) {
            ids.push(id.get());
        } else {
            groups.push((delta, vec![id.get()]));
        }
    }
    for (values, ids) in groups {
        let targets = records.browse(ids)?;
        operation.schedule(&targets, &values)?;
        let payload = replacement(&targets, &values)?;
        match mode {
            WriteMode::Inherited | WriteMode::Downstream => {
                // Both are address copies, not new explicit edits. Base writes keep
                // ordinary authorization and constraints without rerunning write
                // overrides or turning an inherited copy into a grandchild delta.
                // An extension may deliberately include parent_id in its field list.
                // Preserve the base model constraint's existing serialization hint.
                targets
                    .with_context([(
                        "__base_partner_hierarchy_fence",
                        Value::Bool(values.contains("parent_id")),
                    )])
                    .write_base(tx, payload)
                    .await?;
            }
            WriteMode::Upstream => targets.write(tx, payload).await?,
        }
    }
    Ok(())
}

/// The base hook body. Native overrides compose around this ordinary method.
///
/// Upstream mutations run full write dispatch, but their nested base hook queues
/// its propagation for this iterative driver. This avoids a Rust poll stack per
/// ancestor. The queued parent propagation runs before the child's downstream
/// continuation. An upstream addon's after-super code therefore runs after its
/// own mutation and before that queued propagation; its edits are queued normally,
/// never suppressed or discarded as an inherited-address special case.
pub(crate) async fn synchronize(
    tx: &mut Transaction,
    records: &Records,
    changes: Vec<RawVals>,
) -> Result<()> {
    let before = tx.change_count();
    let result = synchronize_inner(tx, records, changes).await;
    if let Err(error) = &result
        && (error.kind() == erp_sdk::ErrorKind::Validation || tx.change_count() != before)
    {
        // A direct native hook call has no enclosing Records::write wrapper.
        // Preserve the same failed-mutation poisoning contract.
        tx.poison(error);
    }
    result
}

async fn synchronize_inner(
    tx: &mut Transaction,
    records: &Records,
    changes: Vec<RawVals>,
) -> Result<()> {
    if changes.len() != records.len() {
        return Err(Error::programming(format!(
            "res.partner._fields_sync: expected {} change maps, got {}",
            records.len(),
            changes.len()
        )));
    }
    if records.is_empty() || changes.iter().all(RawVals::is_empty) {
        return Ok(());
    }
    let operation = OperationGuard::join(tx, records)?;
    {
        let mut state = locked(&operation.state)?;
        enqueue(&mut state, records, changes)?;
        if state.draining {
            return Ok(());
        }
        state.draining = true;
    }
    let _driver = DriverGuard(Arc::clone(&operation.state));
    let mut work = Vec::new();
    take_pending(&operation.state, &mut work)?;
    while let Some(next) = work.pop() {
        match next {
            Work::Synchronize(next) => {
                locked(&operation.state)?.logical_depth = next.depth;
                let address_fields = fields(tx, &next.records, "_address_fields").await?;
                let mut all_fields = address_fields.clone();
                all_fields.extend(["parent_id".to_owned(), "type".to_owned()]);
                // Prefetch the whole input once; record continuations share its cache.
                cells(tx, &next.records, &all_fields).await?;
                let remaining = next.records.iter().zip(next.changes).collect();
                work.push(Work::Batch(BatchWork {
                    records: next.records,
                    address_fields,
                    remaining,
                    downstream: Vec::new(),
                    depth: next.depth,
                    units: next.units,
                }));
            }
            Work::Batch(mut batch) => {
                locked(&operation.state)?.logical_depth = batch.depth;
                if let Some((record, change)) = batch.remaining.pop_front() {
                    // Retain one bookkeeping unit for every original record while the
                    // batch and its downstream continuation retain the recordset.
                    let consumed = if change.len() > 1 {
                        change.len() - 1
                    } else {
                        0
                    };
                    {
                        let mut state = locked(&operation.state)?;
                        release_units(&mut state, consumed)?;
                    }
                    batch.units = batch.units.checked_sub(consumed).ok_or_else(|| {
                        Error::programming("Partner address batch accounting underflow")
                    })?;
                    let pending = synchronize_records(
                        tx,
                        &operation,
                        &record,
                        vec![change],
                        &batch.address_fields,
                    )
                    .await?;
                    for (id, values) in pending {
                        let existing = batch
                            .downstream
                            .iter_mut()
                            .find(|(parent, _)| *parent == id);
                        let additional = match &existing {
                            Some((_, previous)) => values
                                .iter()
                                .filter(|(name, _)| !previous.contains(name))
                                .count(),
                            None => values.len(),
                        };
                        {
                            let mut state = locked(&operation.state)?;
                            reserve_units(&mut state, additional)?;
                        }
                        batch.units = batch.units.checked_add(additional).ok_or_else(|| {
                            Error::programming("Partner address batch accounting overflow")
                        })?;
                        if let Some((_, previous)) = existing {
                            for (name, value) in values.iter() {
                                previous.insert(name.to_owned().into(), value.clone());
                            }
                        } else {
                            batch.downstream.push((id, values));
                        }
                    }
                    // Nested parent frames drain before the next input record is
                    // classified. In particular, the first child's parent fill may
                    // update later children in the same create batch.
                    work.push(Work::Batch(batch));
                } else if batch.downstream.is_empty() {
                    let mut state = locked(&operation.state)?;
                    release_units(&mut state, batch.units)?;
                } else {
                    work.push(Work::Downstream {
                        records: batch.records,
                        changes: batch.downstream,
                        depth: batch.depth,
                        units: batch.units,
                    });
                }
            }
            Work::Downstream {
                records,
                changes,
                depth,
                units,
            } => {
                {
                    let mut state = locked(&operation.state)?;
                    release_units(&mut state, units)?;
                    state.logical_depth = depth;
                }
                synchronize_downstream(tx, &operation, &records, changes).await?;
            }
        }
        // LIFO: nested upstream work executes ahead of the saved downstream phase.
        take_pending(&operation.state, &mut work)?;
    }
    Ok(())
}

async fn synchronize_records(
    tx: &mut Transaction,
    operation: &OperationGuard,
    records: &Records,
    changes: Vec<RawVals>,
    address_fields: &[String],
) -> Result<Vec<(i64, RawVals)>> {
    let mut downstream: Vec<(i64, RawVals)> = Vec::new();
    for (record, change) in records.iter().zip(changes) {
        if record.rec_ids()[0].is_new() || change.is_empty() {
            continue;
        }
        let mut own = single_address(tx, &record, address_fields).await?;
        let parent = record.related(tx, "parent_id").await?;
        operation.admit(&parent)?;
        let mut contact = record.value(tx, "type").await? == Value::str("contact");
        let parent_changed = change.contains("parent_id");
        let type_contact = change
            .get("type")
            .is_some_and(|cell| matches!(cell, Cell::Str(value) if value.as_ref() == "contact"));
        let mut inherited = false;
        if contact && !parent.is_empty() && (parent_changed || type_contact) {
            let parent_address = single_address(tx, &parent, address_fields).await?;
            if parent_changed
                && has_address(&own)
                && has_address(&parent_address)
                && own != parent_address
            {
                operation.schedule(
                    &record,
                    &RawVals::new().set_cell("type", Cell::Str("other".into())),
                )?;
                record
                    .write(
                        tx,
                        RawVals::new().set_cell("type", Cell::Str("other".into())),
                    )
                    .await?;
                contact = false;
            } else if has_address(&parent_address) {
                copy_values(
                    tx,
                    operation,
                    &record,
                    &parent_address,
                    WriteMode::Inherited,
                )
                .await?;
                own = single_address(tx, &record, address_fields).await?;
                inherited = true;
            }
        }
        let mut explicit_address = RawVals::new();
        if !inherited {
            for name in address_fields {
                if change.contains(name)
                    && let Some(value) = own.get(name)
                {
                    explicit_address.insert(name.to_owned().into(), value.clone());
                }
            }
        }
        if contact && !parent.is_empty() && (parent_changed || !explicit_address.is_empty()) {
            copy_values(tx, operation, &parent, &own, WriteMode::Upstream).await?;
        }
        if !explicit_address.is_empty() {
            let id = record.rec_ids()[0].get();
            if let Some((_, pending)) = downstream.iter_mut().find(|(parent, _)| *parent == id) {
                for (name, value) in explicit_address.iter() {
                    pending.insert(name.to_owned().into(), value.clone());
                }
            } else {
                downstream.push((id, explicit_address));
            }
        }
    }
    Ok(downstream)
}

async fn synchronize_downstream(
    tx: &mut Transaction,
    operation: &OperationGuard,
    records: &Records,
    downstream: Vec<(i64, RawVals)>,
) -> Result<()> {
    if !downstream.is_empty() {
        let parent_ids: Vec<i64> = downstream.iter().map(|(id, _)| *id).collect();
        let domain = Domain::and([
            Domain::condition("parent_id", Operator::In, parent_ids)?,
            Domain::condition("type", Operator::Eq, "contact")?,
        ]);
        let children = records
            .search_with(
                tx,
                &domain,
                &SearchOptions::new().limit(4097).fetch_fields(["parent_id"]),
            )
            .await?;
        operation.admit(&children)?;
        let parents = children.cells(tx, "parent_id").await?;
        let mut groups: Vec<(RawVals, Vec<i64>)> = Vec::new();
        for (child, parent) in children.rec_ids().iter().zip(parents) {
            let Cell::Id(parent) = parent else {
                return Err(Error::programming("Partner address child has no parent"));
            };
            let desired = downstream
                .iter()
                .find(|(id, _)| *id == parent)
                .map(|(_, values)| values)
                .ok_or_else(|| {
                    Error::programming("Partner address child has an unexpected parent")
                })?;
            if let Some((_, ids)) = groups.iter_mut().find(|(values, _)| values == desired) {
                ids.push(child.get());
            } else {
                groups.push((desired.clone(), vec![child.get()]));
            }
        }
        for (desired, ids) in groups {
            let targets = records.browse(ids)?;
            copy_values(tx, operation, &targets, &desired, WriteMode::Downstream).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_sdk::ErrorKind;

    fn assert_programming(result: Result<()>, message: &str) {
        let error = result.err();
        assert_eq!(
            error.as_ref().map(Error::kind),
            Some(ErrorKind::Programming)
        );
        assert_eq!(error.as_ref().map(Error::message), Some(message));
    }

    #[test]
    fn wide_input_and_its_saved_subset_do_not_consume_the_copy_budget() {
        let mut state = State::default();
        let input = 4096 * 17;
        assert!(admit_input(&mut state, input).is_ok());
        assert!(reserve_units(&mut state, input).is_ok());
        assert_eq!(state.admitted_units, input);
        assert_eq!(state.queued_units, input * 2);
        assert_programming(
            reserve_units(&mut state, 1),
            "Partner address buffered work exceeds its admitted-input envelope",
        );
        assert_eq!(state.queued_units, input * 2);
        assert!(release_units(&mut state, input * 2).is_ok());
        assert_eq!(state.queued_units, 0);
        // Buffering has not spent any actual scheduled-copy assignments.
        assert!(state.assignments.charge(65_536).is_ok());
        let error = state.assignments.charge(1).err();
        assert_eq!(error.as_ref().map(Error::kind), Some(ErrorKind::Validation));
        assert_eq!(
            error.as_ref().map(Error::message),
            Some("Partner address synchronization maximum field assignments of 65536 exceeded.")
        );
    }

    #[test]
    fn exact_input_can_overlap_a_saved_sixteen_field_continuation() {
        let mut state = State::default();
        assert!(admit_input(&mut state, 65_536).is_ok());
        // Consuming one sixteen-field input retains one bookkeeping unit; saving
        // its sixteen-field downstream subset must admit the resulting overlap.
        assert!(release_units(&mut state, 15).is_ok());
        assert!(reserve_units(&mut state, 16).is_ok());
        assert_eq!(state.queued_units, 65_537);
        assert_eq!(state.admitted_units, 65_536);
    }

    #[test]
    fn admitted_input_and_derived_envelope_overflows_are_internal_errors() {
        let mut input_overflow = State {
            admitted_units: usize::MAX,
            ..State::default()
        };
        assert_programming(
            admit_input(&mut input_overflow, 1),
            "Partner address admitted input footprint overflow",
        );
        assert_eq!(input_overflow.admitted_units, usize::MAX);
        assert_eq!(input_overflow.queued_units, 0);
        let mut envelope_overflow = State {
            admitted_units: usize::MAX / 2,
            ..State::default()
        };
        assert_programming(
            admit_input(&mut envelope_overflow, 1),
            "Partner address buffered-work envelope overflow",
        );
        assert_eq!(envelope_overflow.admitted_units, usize::MAX / 2);
        assert_eq!(envelope_overflow.queued_units, 0);
    }

    #[test]
    fn buffered_work_overflow_underflow_and_driver_drop_keep_accounting_safe() {
        let mut state = State {
            queued_units: usize::MAX,
            admitted_units: usize::MAX / 2,
            draining: true,
            logical_depth: 12,
            ..State::default()
        };
        assert_programming(
            reserve_units(&mut state, 1),
            "Partner address buffered-work accounting overflow",
        );
        assert_eq!(state.queued_units, usize::MAX);
        let shared = Arc::new(Mutex::new(state));
        drop(DriverGuard(Arc::clone(&shared)));
        let state = locked(&shared);
        assert!(state.as_ref().is_ok_and(|state| !state.draining
            && state.logical_depth == 0
            && state.queued_units == 0
            && state.admitted_units == 0
            && state.pending.is_empty()));
        let mut empty = State::default();
        assert_programming(
            release_units(&mut empty, 1),
            "Partner address queued-work accounting underflow",
        );
        assert_eq!(empty.queued_units, 0);
    }
}
