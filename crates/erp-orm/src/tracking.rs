//! Set-based capture of tracked-field changes (design note
//! , decisions D1-D8; behavior).
//!
//! A field declared `tracking = true` (or a display sequence) is a *tracked field*.
//!
//! - **Scalar columns (D1).** The flush `UPDATE` of a model with tracked columns returns the old
//!   and new value of the tracked columns it writes (`RETURNING WITH (OLD AS …, NEW AS …)`, the
//!   write-hook path of `version.rs`), so capture costs no statement of its own, and a write
//!   that touches no tracked column keeps the statement it had.
//! - **Prepared records (D2).** A tracking-enabled `write()` of a model with tracked fields marks
//!   each real record *prepared*. Only prepared records contribute changes: a record created in
//!   the transaction (`create` and upsert inserts) or deleted in it never does, and a write under
//!   the context key `tracking_disable` or `mail_notrack` does not prepare its records. Dirty
//!   tracked values left by such an unprepared write are flushed before the record is prepared
//!   (one extra statement, only then), so they never read as a tracked change.
//! - **Many2many (D3).** A many2many has no column: the old and new id sets are taken from the
//!   relation write in memory (no statement), as the visible targets the write saw.
//! - **Raw values (D5).** Changes carry storage cells (ids, selection keys, naive UTC datetimes,
//!   strings, id sets). Display names, formatting and the user's time zone are the mail side's.
//! - **Savepoints (D8).** The log is savepoint-scoped, the pattern of `transaction/scoped_ids.rs`:
//!   entries of a rolled back savepoint go with it, entries of a released one move to the
//!   enclosing level; everything is cleared at commit and rollback.
//!
//! The drain is [`Transaction::take_tracked_changes`] (D7); the mail addon calls it from a
//! [`PrecommitHook`], which the ORM runs at commit after the flush and before `COMMIT`.

use std::collections::BTreeMap;

use erp_fields::{Cell, Error, RealId, RecId, Result};
use erp_registry::{FieldId, ModelId, ModelSpec};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::dispatch::BoxFuture;
use crate::records::Records;

pub const TRACKING_DISABLE_KEYS: [&str; 2] = ["tracking_disable", "mail_notrack"];

/// The function of a [`PrecommitHook`].
pub type PrecommitHookFn = for<'a> fn(&'a mut Transaction) -> BoxFuture<'a, Result<()>>;

/// A hook the ORM runs at commit, after the flush and before `COMMIT` (D7). Register it with
/// [`crate::OrmBuilder::precommit_hook`]. Hooks run in registration order; what they write is
/// flushed, and they run again while tracked changes are still pending (a hook that drains with
/// [`Transaction::take_tracked_changes`] and writes tracked fields is run once more for those
/// writes), at most [`crate::OrmConfig::max_flush_rounds`] rounds. Hooks run again
/// only when one of them drained in the round that just ran; when no hook drains, the round ends
/// the loop and pending tracked changes stay unreported (as when no hook is registered). A hook
/// error comes back as `precommit hook '<name>': <message>` with its kind.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct PrecommitHook {
    name: &'static str,
    call: PrecommitHookFn,
}

impl PrecommitHook {
    /// A hook named `name` (shown in the error of a hook that does not settle).
    pub const fn new(name: &'static str, call: PrecommitHookFn) -> Self {
        Self { name, call }
    }
}

impl std::fmt::Debug for PrecommitHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrecommitHook")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// One tracked field of one record that changed: the value before the first tracked write and the
/// value after the last, as storage cells.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackedChange {
    field: &'static str,
    sequence: u32,
    old: Cell,
    new: Cell,
}

impl TrackedChange {
    /// The field name.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// The field's tracking sequence (the order of the changes of a record).
    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    /// The value before the first tracked write (`Cell::Null` for a column that was NULL; an id
    /// set for a many2many).
    pub fn old(&self) -> &Cell {
        &self.old
    }

    /// The value after the last tracked write.
    pub fn new_value(&self) -> &Cell {
        &self.new
    }
}

/// The tracked changes of one record, by tracking sequence then field name.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackedRecord {
    model: &'static str,
    id: RecId,
    changes: Vec<TrackedChange>,
}

impl TrackedRecord {
    /// The model name.
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// The record.
    pub fn id(&self) -> RecId {
        self.id
    }

    /// The changed fields, never empty.
    pub fn changes(&self) -> &[TrackedChange] {
        &self.changes
    }
}

fn equivalent(old: &Cell, new: &Cell) -> bool {
    if falsy(old) && falsy(new) {
        return true;
    }
    match (old, new) {
        (Cell::Ids(a), Cell::Ids(b)) => {
            let mut a = a.to_vec();
            let mut b = b.to_vec();
            a.sort_unstable();
            b.sort_unstable();
            a == b
        }
        _ => old.same_value(new),
    }
}

fn falsy(cell: &Cell) -> bool {
    match cell {
        Cell::Null | Cell::Bool(false) => true,
        Cell::Int(0) => true,
        Cell::Id(0) => true,
        Cell::Float(f) => *f == 0.0,
        Cell::Decimal(d) => d.is_zero(),
        Cell::Str(s) => s.is_empty(),
        Cell::Ids(ids) => ids.is_empty(),
        Cell::Json(v) => match v.as_ref() {
            serde_json::Value::Null | serde_json::Value::Bool(false) => true,
            serde_json::Value::String(s) => s.is_empty(),
            serde_json::Value::Array(a) => a.is_empty(),
            serde_json::Value::Object(o) => o.is_empty(),
            _ => false,
        },
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Prepared,
    Created,
    Gone,
}

#[derive(Debug, Clone)]
enum Event {
    Prepared(ModelId, RealId),
    Created(ModelId, RealId),
    Gone(ModelId, RealId),
    Change {
        model: ModelId,
        id: RealId,
        field: FieldId,
        old: Cell,
        new: Cell,
    },
}

/// The savepoint-scoped log of one transaction (D8).
#[derive(Debug, Default, Clone)]
pub(crate) struct TrackBuffer {
    /// `(savepoint depth, event)` in the order the events happened.
    log: Vec<(usize, Event)>,
    status: FxHashMap<(ModelId, RealId), Status>,
    /// How many times the log was drained (never reset): a precommit round that drained moved it.
    drains: u64,
}

impl TrackBuffer {
    fn status_of(&self, model: ModelId, id: RealId) -> Option<Status> {
        self.status.get(&(model, id)).copied()
    }

    fn rebuild(&mut self) {
        self.status.clear();
        for (_, event) in &self.log {
            match event {
                Event::Prepared(m, i) => {
                    self.status.entry((*m, *i)).or_insert(Status::Prepared);
                }
                Event::Created(m, i) => {
                    self.status.insert((*m, *i), Status::Created);
                }
                Event::Gone(m, i) => {
                    self.status.insert((*m, *i), Status::Gone);
                }
                Event::Change { .. } => {}
            }
        }
    }

    pub(crate) fn rolled_back_to(&mut self, depth: usize) {
        let before = self.log.len();
        self.log.retain(|(d, _)| *d <= depth);
        if self.log.len() != before {
            self.rebuild();
        }
    }

    pub(crate) fn released_to(&mut self, depth: usize) {
        for (d, _) in &mut self.log {
            *d = (*d).min(depth);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.log.clear();
        self.status.clear();
    }

    fn prepare(&mut self, depth: usize, model: ModelId, id: RealId) {
        if self.status_of(model, id).is_none() {
            self.status.insert((model, id), Status::Prepared);
            self.log.push((depth, Event::Prepared(model, id)));
        }
    }

    /// Records inserted by this transaction never enter as diffs.
    pub(crate) fn created(&mut self, depth: usize, model: ModelId, ids: &[RecId]) {
        for id in ids.iter().filter_map(|i| i.as_real()) {
            self.status.insert((model, id), Status::Created);
            self.log.push((depth, Event::Created(model, id)));
        }
    }

    /// Deleted records have nothing left to report.
    pub(crate) fn gone(&mut self, depth: usize, model: ModelId, ids: &[RecId]) {
        for id in ids.iter().filter_map(|i| i.as_real()) {
            self.status.insert((model, id), Status::Gone);
            self.log.push((depth, Event::Gone(model, id)));
        }
    }

    fn change(
        &mut self,
        depth: usize,
        model: ModelId,
        id: RealId,
        field: FieldId,
        old: Cell,
        new: Cell,
    ) {
        if self.status_of(model, id) == Some(Status::Prepared) && !equivalent(&old, &new) {
            self.log.push((
                depth,
                Event::Change {
                    model,
                    id,
                    field,
                    old,
                    new,
                },
            ));
        }
    }

    /// The events so far; afterwards only what says "created" or "deleted" remains, so a write
    /// after the drain cannot report a record the transaction inserted.
    fn drain(&mut self) -> Vec<Event> {
        self.drains += 1;
        let events: Vec<Event> = self.log.drain(..).map(|(_, e)| e).collect();
        self.log = events
            .iter()
            .filter(|e| matches!(e, Event::Created(..) | Event::Gone(..)))
            .cloned()
            .map(|e| (0, e))
            .collect();
        self.rebuild();
        events
    }
}

/// Whether the context of `records` switches tracking off.
fn disabled(records: &Records) -> bool {
    TRACKING_DISABLE_KEYS.iter().any(|key| {
        records
            .env()
            .context()
            .get(key)
            .is_some_and(|value| value.is_truthy())
    })
}

impl Transaction {
    /// Mark the real records about to be written as prepared (D2). Called by the base `write`
    /// before it stages anything; no statement unless dirty tracked values of unprepared records
    /// are pending (then one flush of the tracked fields, so they stay out of the changes).
    pub(crate) async fn track_prepare(&mut self, records: &Records) -> Result<()> {
        let registry = std::sync::Arc::clone(self.registry());
        let spec = registry.model_by_id(records.model_id());
        if spec.tracked.is_empty() || disabled(records) {
            return Ok(());
        }
        let model = spec.id;
        let fresh: Vec<RecId> = records
            .rec_ids()
            .iter()
            .copied()
            .filter(|id| {
                id.as_real()
                    .is_some_and(|real| self.tracking.status_of(model, real).is_none())
            })
            .collect();
        if fresh.is_empty() {
            return Ok(());
        }
        let fresh_set: FxHashSet<RecId> = fresh.iter().copied().collect();
        let tracked_set: FxHashSet<_> = spec.tracked.iter().copied().collect();
        let pending = self.cache.model(model).is_some_and(|mc| {
            mc.dirty().iter().any(|(index, ids)| {
                spec.fields
                    .get(*index)
                    .is_some_and(|f| tracked_set.contains(&f.id))
                    && ids.iter().any(|id| fresh_set.contains(id))
            })
        });
        if pending {
            // values written without tracking are still waiting for the flush: send them now,
            // while their records are unprepared, so they are not reported as tracked changes
            self.flush_fields(model, &spec.tracked).await?;
        }
        let depth = self.savepoint_depth();
        for id in fresh.iter().filter_map(|i| i.as_real()) {
            self.tracking.prepare(depth, model, id);
        }
        Ok(())
    }

    /// Record the old and new values a flush `UPDATE` returned for the tracked columns it wrote
    /// (`hooked` and the values of each row are in the order of the statement).
    pub(crate) fn track_capture(
        &mut self,
        spec: &ModelSpec,
        hooked: &[FieldId],
        rows: &[(RecId, Vec<(Cell, Cell)>)],
    ) {
        if spec.tracked.is_empty() {
            return;
        }
        let depth = self.savepoint_depth();
        for (id, values) in rows {
            let Some(real) = id.as_real() else { continue };
            for (field, (old, new)) in hooked.iter().zip(values) {
                if spec.tracked.contains(field) {
                    self.tracking
                        .change(depth, spec.id, real, *field, old.clone(), new.clone());
                }
            }
        }
    }

    /// Record a many2many change from the id sets the relation write saw (D3).
    pub(crate) fn track_relation(
        &mut self,
        spec: &ModelSpec,
        field: FieldId,
        id: RecId,
        old: &[RecId],
        new: &[RecId],
    ) {
        if !spec.tracked.contains(&field) {
            return;
        }
        let Some(real) = id.as_real() else { return };
        let depth = self.savepoint_depth();
        self.tracking.change(
            depth,
            spec.id,
            real,
            field,
            Cell::Ids(old.into()),
            Cell::Ids(new.into()),
        );
    }

    /// Records written outside `write` (an upsert update) are prepared too when tracking is on.
    pub(crate) fn track_prepare_direct(&mut self, records: &Records, ids: &[RecId]) {
        let spec = records.spec();
        if spec.tracked.is_empty() || disabled(records) {
            return;
        }
        let depth = self.savepoint_depth();
        for id in ids.iter().filter_map(|i| i.as_real()) {
            self.tracking.prepare(depth, spec.id, id);
        }
    }

    /// Flush, then take the tracked changes of the transaction and clear the log (D4, D7).
    /// Records come ordered by model and id, their changes by tracking sequence then name; a
    /// record without a net change is left out.
    ///
    /// The values are the raw old and new values the transaction's writes captured, **not
    /// filtered by the reader's field access**: the consumer filters them (the mail
    /// addon). Reader filtering of tracked values is a merge condition of the mail task that
    /// consumes this method, not. The drain is destructive: the log is cleared, so a second caller (or a second call)
    /// sees nothing for the changes already taken.
    ///
    /// ```ignore
    /// // in a PrecommitHook registered by the mail addon
    /// for record in tx.take_tracked_changes().await? {
    ///     for change in record.changes() {
    ///         // old()/new_value() are storage cells: format them for the chatter
    ///     }
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// `Programming` while a savepoint is open (`take_tracked_changes: a savepoint is open …`):
    /// the changes of an open savepoint can still be rolled back. A failed flush returns its
    /// error.
    pub async fn take_tracked_changes(&mut self) -> Result<Vec<TrackedRecord>> {
        self.require_record_scope()?;
        if self.savepoint_depth() != 0 {
            return Err(Error::programming(
                "take_tracked_changes: a savepoint is open; its changes can still be rolled \
                 back, take the tracked changes after it is released",
            ));
        }
        self.flush().await?;
        let events = self.tracking.drain();
        let registry = std::sync::Arc::clone(self.registry());
        let mut folded: BTreeMap<(ModelId, RealId), BTreeMap<FieldId, (Cell, Cell)>> =
            BTreeMap::new();
        for event in events {
            match event {
                Event::Prepared(..) => {}
                Event::Created(m, i) | Event::Gone(m, i) => {
                    folded.remove(&(m, i));
                }
                Event::Change {
                    model,
                    id,
                    field,
                    old,
                    new,
                } => {
                    folded
                        .entry((model, id))
                        .or_default()
                        .entry(field)
                        .and_modify(|(_, last)| *last = new.clone())
                        .or_insert((old, new));
                }
            }
        }
        let mut out = Vec::new();
        for ((model, id), fields) in folded {
            let spec = registry.model_by_id(model);
            let mut changes = Vec::new();
            for fid in &spec.tracked {
                let Some((old, new)) = fields.get(fid) else {
                    continue;
                };
                if equivalent(old, new) {
                    continue;
                }
                let field = spec.field_by_id(*fid);
                changes.push(TrackedChange {
                    field: field.name,
                    sequence: field.tracking.ok_or_else(|| {
                        Error::programming(format!(
                            "tracked field {}.{} has no tracking sequence",
                            spec.name, field.name
                        ))
                    })?,
                    old: old.clone(),
                    new: new.clone(),
                });
            }
            if changes.is_empty() {
                continue;
            }
            out.push(TrackedRecord {
                model: spec.name,
                id: RecId::from_real(id.get())
                    .ok_or_else(|| Error::programming(format!("tracked record id {id}")))?,
                changes,
            });
        }
        Ok(out)
    }

    /// Run the precommit hooks (D7): after the commit flush, repeated while tracked changes are
    /// still pending, at most `max_flush_rounds` rounds.
    pub(crate) async fn run_precommit_hooks(&mut self) -> Result<()> {
        let hooks = self.shared.precommit_hooks.clone();
        if hooks.is_empty() {
            return Ok(());
        }
        let rounds = self.config().max_flush_rounds.max(1);
        for _ in 0..rounds {
            let drains_before = self.tracking.drains;
            for hook in &hooks {
                if let Err(error) = (hook.call)(self).await {
                    // name the hook, keep the kind, SQLSTATE, constraint, condition, context
                    // and the cause (as `retained_search_error` does)
                    let sqlstate = error.sqlstate().map(str::to_owned);
                    let mut wrapped = Error::new(
                        error.kind(),
                        format!("precommit hook '{}': {}", hook.name, error.message()),
                    )
                    .with_sqlstate(sqlstate);
                    if let Some(constraint) = error.constraint() {
                        wrapped = wrapped.with_constraint(constraint);
                    }
                    if let Some(condition) = error.condition() {
                        wrapped = wrapped.with_condition(condition);
                    }
                    if let Some(context) = error.context() {
                        wrapped = wrapped.with_context(context.clone());
                    }
                    return Err(wrapped.with_source(error));
                }
            }
            self.flush().await?;
            let pending = self
                .tracking
                .log
                .iter()
                .any(|(_, e)| matches!(e, Event::Change { .. }));
            // the hooks run again only for changes a hook took in this round and whose own writes
            // produced more; when nobody drained, another round could not change anything
            if !pending || self.tracking.drains == drains_before {
                return Ok(());
            }
        }
        let names: Vec<&str> = hooks.iter().map(|h| h.name).collect();
        Err(Error::programming(format!(
            "the precommit hooks [{}] did not settle after {rounds} rounds: tracked changes are \
             still pending",
            names.join(", ")
        )))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    fn rid(i: i64) -> RealId {
        RealId::new(i).expect("positive test id")
    }

    #[test]
    fn falsy_values_are_equivalent_and_sets_ignore_order() {
        assert!(equivalent(&Cell::Null, &Cell::Int(0)));
        assert!(equivalent(&Cell::Str("".into()), &Cell::Bool(false)));
        assert!(!equivalent(&Cell::Int(1), &Cell::Int(2)));
        let a = Cell::Ids(vec![RecId::real(rid(1)), RecId::real(rid(2))].into());
        let b = Cell::Ids(vec![RecId::real(rid(2)), RecId::real(rid(1))].into());
        assert!(equivalent(&a, &b));
        assert!(equivalent(&Cell::Ids(Vec::new().into()), &Cell::Null));
    }
}
