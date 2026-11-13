//! The relational steps around `base.create` and `base.write` (CRUD §4.2, §4.4; REL §4.4.4,
//! §4.8, §4.9, §4.12): `_inherits` parents, x2many commands after the scalar values (write
//! sequence 20), coherence of cached one2many values when many2one values change,
//! reference checks and the company check.

use std::borrow::Cow;
use std::sync::Arc;

use erp_fields::{Cell, Error, FieldType, RecId, Result};
use erp_registry::{FieldId, ModelSpec};
use rustc_hash::FxHashSet;

use crate::Transaction;
use crate::commands::Command;
use crate::records::Records;
use crate::vals::RawVals;

fn invalid_field(spec: &ModelSpec, name: &str) -> Error {
    Error::programming(format!("Invalid field '{name}' in '{}'", spec.name))
}

/// x2many values out of `vals`: command lists, plus plain id lists given as cells (`SET`) and
/// empty cells (`CLEAR`).
fn split_x2many(spec: &ModelSpec, vals: &mut RawVals) -> Result<Vec<(FieldId, Vec<Command>)>> {
    let mut out: Vec<(FieldId, Vec<Command>)> = Vec::new();
    let names: Vec<String> = vals.iter_commands().map(|(n, _)| n.to_owned()).collect();
    for name in names {
        let field = spec
            .field(&name)
            .ok_or_else(|| invalid_field(spec, &name))?;
        if field.inherited.is_some() {
            continue;
        }
        if !field.ty.is_x2many() {
            return Err(Error::programming(format!(
                "Commands given for {}.{name}, which is not a one2many or many2many field",
                spec.name
            )));
        }
        if let Some(commands) = vals.remove_commands(&name) {
            out.push((field.id, commands));
        }
    }
    let names: Vec<String> = vals.iter().map(|(n, _)| n.to_owned()).collect();
    for name in names {
        let Some(field) = spec.field(&name) else {
            continue;
        };
        if !field.ty.is_x2many() || field.inherited.is_some() {
            continue;
        }
        let commands = match vals.remove(&name) {
            Some(Cell::Ids(ids)) => vec![Command::Set(ids.to_vec())],
            Some(Cell::Null) => vec![Command::Clear],
            Some(other) => {
                return Err(Error::validation(format!(
                    "Wrong value for {}.{name}: {other:?}",
                    spec.name
                )));
            }
            None => continue,
        };
        out.push((field.id, commands));
    }
    out.sort_by_key(|(f, _)| *f);
    Ok(out)
}

impl Transaction {
    /// One existence query per
    /// referenced model.
    pub(crate) async fn check_references(
        &mut self,
        records: &Records,
        rows: &mut [RawVals],
    ) -> Result<()> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let reference_fields: Vec<&erp_registry::FieldSpec> = spec
            .fields
            .iter()
            .filter(|f| matches!(f.ty, FieldType::Reference { .. }) && f.inherited.is_none())
            .collect();
        if reference_fields.is_empty() {
            return Ok(());
        }
        let mut wanted: Vec<(String, i64)> = Vec::new();
        for row in rows.iter() {
            for field in &reference_fields {
                let Some(Cell::Str(text)) = row.get(field.name) else {
                    continue;
                };
                let bad = || {
                    Error::validation(format!(
                        "Wrong value for {}.{}: '{text}'",
                        spec.name, field.name
                    ))
                };
                let (model, id) = erp_fields::convert::parse_reference(text).ok_or_else(bad)?;
                let declared = match &field.ty {
                    FieldType::Reference {
                        models: Some(models),
                        ..
                    } => models.iter().any(|(m, _)| m == model),
                    _ => true,
                };
                let target = registry.model(model).filter(|_| declared).ok_or_else(bad)?;
                if !target.has_table() {
                    return Err(Error::validation(format!(
                        "Wrong value for {}.{}: '{model}' is an abstract model without records",
                        spec.name, field.name
                    )));
                }
                wanted.push((model.to_owned(), id));
            }
        }
        let mut models: Vec<String> = wanted.iter().map(|(m, _)| m.clone()).collect();
        models.sort();
        models.dedup();
        for model in models {
            let Some(target) = registry.model(&model) else {
                continue;
            };
            let mut ids: Vec<i64> = wanted
                .iter()
                .filter(|(m, _)| *m == model)
                .map(|(_, i)| *i)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            let sql = erp_sql::dml::select_by_ids_cached(self.registry(), target, &[]);
            let mut existing: FxHashSet<i64> = FxHashSet::default();
            for row in self.query(&sql, &[&ids]).await? {
                let id: i64 = row.try_get(0).map_err(|e| {
                    Error::new(erp_fields::ErrorKind::Database, "could not decode an id")
                        .with_source(e)
                })?;
                existing.insert(id);
            }
            if let Some(missing) = ids.iter().find(|i| !existing.contains(i)) {
                return Err(Error::new(
                    erp_fields::ErrorKind::Missing,
                    format!(
                        "Record does not exist or has been deleted: {model},{missing} (referenced \
                         by {})",
                        spec.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Forget the cached one2many values over a many2one_reference whose model or id field was
    /// written (their owners cannot be named cheaply; F2).
    pub(crate) fn forget_reference_inverses(
        &mut self,
        model: erp_registry::ModelId,
        fields: &[FieldId],
    ) {
        let shared = Arc::clone(&self.shared);
        for field in fields {
            for &(owner, o2m) in shared.relations.reference_inverses(model, *field) {
                if self.cache.model(owner).is_some() {
                    self.cache
                        .model_mut(owner)
                        .invalidate(Some(&[o2m][..]), None);
                }
            }
        }
    }
}

/// `base.create` with its relational steps.
pub(crate) async fn create(
    tx: &mut Transaction,
    records: &Records,
    vals_list: Vec<RawVals>,
) -> Result<Records> {
    let before = tx.mutations();
    // The base method is also callable directly through dispatch. Its error/cancellation
    // boundary must not depend on Records::create being above it in the call stack.
    let guard = tx.guard();
    let previous_origin = tx.pending_capture.active.clone();
    let result = create_guarded(tx, records, vals_list).await;
    tx.pending_capture.active = previous_origin;
    if let Err(error) = &result {
        tx.poison_after(before, error);
    }
    guard.disarm();
    result
}

async fn create_guarded(
    tx: &mut Transaction,
    records: &Records,
    vals_list: Vec<RawVals>,
) -> Result<Records> {
    tx.check_capture_create(records, &vals_list)?;
    crate::event_namespace::check_create_capture(records, &vals_list)?;
    if vals_list.is_empty() {
        let (created, _, after) = crate::write::create_records(
            tx,
            records,
            vals_list,
            crate::write::AfterInsert::Nothing,
        )
        .await?;
        // deferred inverses that nobody runs would be lost without a word
        if after.is_some() {
            return Err(Error::programming(
                "create_records deferred work for a create that asked for none",
            ));
        }
        return Ok(created);
    }
    crate::database_generated::check_write_names(
        tx,
        records.spec(),
        vals_list
            .iter()
            .flat_map(|v| v.names())
            .chain(records.env().context().default_field_names()),
    )?;
    tx.require_writable(crate::WriteOp::Create, records.spec().name)?;
    let env = records.env().clone();
    tx.prepare_access(&env).await?;
    // a policy source or a membership: what is pending runs under the policy it started with
    let source = tx
        .before_policy_source_write(records.model_id(), None)
        .await?;
    let created = create_values(tx, records, vals_list).await;
    // also after a failure: rows may be inserted already (over-invalidation is safe)
    tx.policy_source_written(source);
    created
}

/// Inverse methods and editable related writes of the same create
/// then see the new rows, and a change they make reaches the new rows' dependents through those
/// one2many values.
pub(crate) async fn cache_created_relations(tx: &mut Transaction, created: &Records) -> Result<()> {
    let model = created.model_id();
    let registry = Arc::clone(created.env().registry());
    let spec = registry.model_by_id(model);
    for field in spec
        .fields
        .iter()
        .filter(|f| f.ty.is_x2many() && f.store && f.inherited.is_none())
    {
        let mc = tx.cache.model_mut(model);
        for id in created.rec_ids() {
            mc.set(
                field.id,
                field.storage(),
                *id,
                Cell::Ids(Arc::from([])),
                false,
            )?;
        }
    }
    let m2o_fields: Vec<&erp_registry::FieldSpec> = spec
        .fields
        .iter()
        .filter(|f| {
            matches!(f.ty, FieldType::Many2one { .. })
                && f.inherited.is_none()
                && !tx.shared.relations.inverses(model, f.id).is_empty()
        })
        .collect();
    for field in m2o_fields {
        let mut by_target: Vec<(Cell, Vec<RecId>)> = Vec::new();
        for id in created.rec_ids() {
            let Some(cell) = tx.cache.get(model, field.id, *id).filter(|c| !c.is_null()) else {
                continue;
            };
            match by_target.iter_mut().find(|(c, _)| *c == cell) {
                Some((_, ids)) => ids.push(*id),
                None => by_target.push((cell, vec![*id])),
            }
        }
        for (target, ids) in by_target {
            let recs = created.browse_ids(ids.iter().copied());
            let old = vec![Some(Cell::Null); ids.len()];
            tx.update_inverses(model, field.id, &recs, &old, &target)
                .await?;
        }
    }
    Ok(())
}

/// `base.create` once the access state is prepared.
async fn create_values(
    tx: &mut Transaction,
    records: &Records,
    mut vals_list: Vec<RawVals>,
) -> Result<Records> {
    let env = records.env().clone();
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(records.model_id());
    tx.check_model(&env, records.model_id(), crate::access::Operation::Create)?;
    let given = vals_list.iter().flat_map(|vals| vals.names());
    tx.check_field_names(
        &env,
        records.model_id(),
        given.chain(env.context().default_field_names()),
        crate::field_access::FieldOp::Write,
    )?;
    let mut rows: Vec<RawVals> = Vec::with_capacity(vals_list.len());
    let mut x2many: Vec<Vec<(FieldId, Vec<Command>)>> = Vec::with_capacity(vals_list.len());
    let mut inherited: Vec<Vec<(FieldId, RawVals)>> = Vec::with_capacity(vals_list.len());
    if let Some(path) = spec.parent_path {
        let path_name = spec.field_by_id(path).name;
        for vals in &mut vals_list {
            // maintained by the ORM, never taken from values (CRUD §4.11)
            vals.remove(path_name);
        }
    }
    // defaults of the fields left out, through the `default_get` override chain (CRUD §4.2 step 3).
    // Delegated fields are asked too (unless their parent is given): their defaults, the
    // context's `default_<field>` included, become inherited values that the `_inherits` step
    // below hands to the parent's `create` (`spec_crud_007_defaults_of_delegated_fields`
    // in erp-test-models/tests/crud.rs).
    crate::defaults::add_missing_defaults_batch(tx, records, &mut vals_list).await?;
    // A default_get override or default_* context may introduce relational work. Check the
    // resolved rows before split_x2many removes it and before delegated parents are mutated.
    crate::event_namespace::check_create_capture(records, &vals_list)?;
    for mut vals in vals_list {
        let mut x2m = split_x2many(spec, &mut vals)?;
        x2m.sort_by_key(|(f, _)| *f);
        x2many.push(x2m);
        inherited.push(crate::inherits::split_inherited(spec, &mut vals));
        rows.push(vals);
    }
    // `_inherits` parents: created in one batch per parent model when the delegate is not given,
    // written when it is (CRUD §4.2 step 5)
    for &(parent, delegate) in &spec.inherits {
        let delegate_name = spec.field_by_id(delegate).name;
        let parents = Records::empty(env.clone(), parent);
        let mut to_create: Vec<(usize, RawVals)> = Vec::new();
        // given parents with equal values are written together
        let mut to_write: Vec<(RawVals, Vec<RecId>)> = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            let values = inherited[r]
                .iter()
                .find(|(d, _)| *d == delegate)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            match row.get(delegate_name) {
                Some(Cell::Id(id)) => {
                    if !values.is_empty()
                        && let Some(id) = RecId::from_real(*id)
                    {
                        match to_write.iter_mut().find(|(v, _)| *v == values) {
                            Some((_, ids)) => ids.push(id),
                            None => to_write.push((values, vec![id])),
                        }
                    }
                }
                _ => to_create.push((r, values)),
            }
        }
        for (values, ids) in to_write {
            parents.browse_ids(ids).write(tx, values).await?;
        }
        if !to_create.is_empty() {
            let (positions, values): (Vec<usize>, Vec<RawVals>) = to_create.into_iter().unzip();
            let created = parents.create(tx, values).await?;
            for (r, id) in positions.iter().zip(created.rec_ids()) {
                rows[*r].insert(Cow::Borrowed(delegate_name), Cell::Id(id.get()));
            }
        }
    }
    tx.check_references(records, &mut rows).await?;
    let mut row_fields: Vec<FieldId> = rows
        .iter()
        .flat_map(|row| row.iter().filter_map(|(n, _)| spec.field_id(n)))
        .collect();
    row_fields.sort_unstable();
    row_fields.dedup();
    let postponed: FxHashSet<FieldId> = x2many.iter().flatten().map(|(f, _)| *f).collect();
    // fusing the record check into the INSERT is only sound while nothing writes the new rows'
    // columns afterwards: x2many values (inverses, line creates, totals over lines) may
    let after = if postponed.is_empty() {
        crate::write::AfterInsert::Nothing
    } else {
        crate::write::AfterInsert::X2many(&postponed)
    };
    let (created, check, after_create) =
        crate::write::create_records(tx, records, rows, after).await?;
    let model = created.model_id();
    // new rows: memoized checks whose predicates read this model are stale
    tx.access_written(model, None);
    tx.forget_reference_inverses(model, &row_fields);
    // one2many fields over a non-stored many2one of the new records (a related or computed
    // inverse, REL §4.6): their owners forget the cached values, and dependents are notified
    let over: Vec<(
        &erp_registry::FieldSpec,
        Vec<(erp_registry::ModelId, FieldId)>,
    )> = spec
        .fields
        .iter()
        .filter(|f| !f.store && matches!(f.ty, FieldType::Many2one { .. }) && f.inherited.is_none())
        .map(|f| (f, tx.inverse_one2manys((model, f.id))))
        .filter(|(_, o2ms)| {
            o2ms.iter().any(|k| {
                tx.shared.graph.has_dependents(*k)
                    || tx.cache.model(k.0).is_some_and(|m| m.has_values(k.1))
            })
        })
        .collect();
    if !over.is_empty() {
        let real: Vec<RecId> = created
            .rec_ids()
            .iter()
            .copied()
            .filter(|i| !i.is_new())
            .collect();
        let su = created.engine_sudo().browse_ids(real);
        for (field, o2ms) in over {
            let mut seen: FxHashSet<RecId> = FxHashSet::default();
            let owners: Vec<RecId> = su
                .cells(tx, field.name)
                .await?
                .iter()
                .filter_map(crate::relational::id_of)
                .filter(|o| seen.insert(*o))
                .collect();
            if owners.is_empty() {
                continue;
            }
            for (m, o2m) in o2ms {
                tx.cache
                    .model_mut(m)
                    .invalidate(Some(&[o2m][..]), Some(&owners));
                tx.modified(m, vec![(o2m, owners.clone())]).await?;
            }
        }
    }
    // x2many commands in create mode, in field order, without `default_*` context keys
    let clean = created.with_env(env.with_context_replaced(env.context().without_defaults()));
    let mut fields: Vec<FieldId> = x2many.iter().flatten().map(|(f, _)| *f).collect();
    fields.sort_unstable();
    fields.dedup();
    for &field in &fields {
        let groups: Vec<(Records, Vec<Command>)> = x2many
            .iter()
            .zip(clean.rec_ids())
            .filter_map(|(cmds, id)| {
                cmds.iter()
                    .find(|(f, _)| *f == field)
                    .map(|(_, c)| (clean.browse_ids([*id]), c.clone()))
            })
            .collect();
        let given = clean.browse_ids(groups.iter().flat_map(|(r, _)| r.rec_ids().to_vec()));
        tx.write_x2many(model, field, groups, true).await?;
        // a non-stored x2many with an inverse persists its new value (COMP §4.7)
        tx.inverse_x2many_cache(&given, field).await?;
    }
    if !fields.is_empty() {
        let given: FxHashSet<FieldId> = fields.iter().copied().collect();
        tx.run_constraints(&created, &given, &FxHashSet::default())
            .await?;
    }
    if let Some(after_create) = after_create {
        after_create.run(tx, &created).await?;
    }
    tx.check_company(&created, None).await?;
    // Fused
    // into the INSERT where the predicate qualifies: the same error, here.
    match check {
        crate::write::CreateCheck::After => {
            tx.check_record_access(&created, crate::access::Operation::Create)
                .await?;
        }
        crate::write::CreateCheck::Fused(denied) => {
            let env = created.env().clone();
            tx.fused_create_denied(&env, created.model_id(), &denied)
                .await?;
        }
    }
    Ok(created)
}

/// The first field name of delegated values `values` (for error messages).
fn first_delegated<'v>(spec: &ModelSpec, delegate: &str, values: &'v RawVals) -> Result<&'v str> {
    values
        .iter()
        .map(|(n, _)| n)
        .chain(values.iter_commands().map(|(n, _)| n))
        .next()
        .ok_or_else(|| {
            Error::programming(format!(
                "{}: empty delegated values for {delegate}",
                spec.name
            ))
        })
}

/// Where delegated values written on `records` go must be able to hold them: a delegate the same
/// write empties leaves them nowhere, and a new record as the delegate of database records would
/// keep them in memory only. New records' values go to their parents, which must be new records
/// too (an onchange gives every in-memory record an in-memory parent): a new record whose delegate
/// is empty or a database record is refused, as writing would lose the value or change the
/// database parent. The delegate the same write sets counts. Checked before anything is written.
async fn check_delegated_targets(
    tx: &mut Transaction,
    records: &Records,
    inherited: &[(FieldId, RawVals)],
    vals: &RawVals,
) -> Result<()> {
    let spec = records.spec();
    let registry = Arc::clone(records.env().registry());
    for (delegate, values) in inherited {
        let delegate_name = spec.field_by_id(*delegate).name;
        let field = first_delegated(spec, delegate_name, values)?;
        let set = vals.get(delegate_name).map(crate::relational::id_of);
        match set {
            Some(None) => {
                return Err(Error::programming(format!(
                    "{}: delegated values ({field}) cannot be written while {delegate_name} is \
                     emptied on {records}",
                    spec.name
                )));
            }
            Some(Some(target)) if target.is_new() => {
                let real: Vec<RecId> = records
                    .rec_ids()
                    .iter()
                    .copied()
                    .filter(|i| !i.is_new())
                    .collect();
                if !real.is_empty() {
                    return Err(Error::programming(format!(
                        "{}: delegated values ({field}) cannot be written through \
                         {delegate_name} = {target}, a new record, on database records {}",
                        spec.name,
                        records.browse_ids(real)
                    )));
                }
            }
            _ => {}
        }
        if !records.rec_ids().iter().any(|i| i.is_new()) {
            continue;
        }
        let links: Vec<Option<RecId>> = match set {
            Some(link) => vec![link; records.len()],
            None => records
                .cells(tx, delegate_name)
                .await?
                .iter()
                .map(crate::relational::id_of)
                .collect(),
        };
        if links.len() != records.len() {
            return Err(Error::programming(format!(
                "{}: {} values of {delegate_name} for {records}",
                spec.name,
                links.len()
            )));
        }
        let refused = records
            .rec_ids()
            .iter()
            .zip(&links)
            .find(|(id, link)| id.is_new() && !link.is_some_and(RecId::is_new));
        if let Some((id, link)) = refused {
            let parent = spec
                .inherits
                .iter()
                .find(|(_, d)| d == delegate)
                .map(|(m, _)| registry.model_by_id(*m).name)
                .ok_or_else(|| {
                    Error::programming(format!(
                        "{}.{delegate_name} is not a delegate field",
                        spec.name
                    ))
                })?;
            return Err(crate::newrec::delegated_on_new_without_parent(
                spec,
                field,
                *id,
                delegate_name,
                parent,
                *link,
            ));
        }
    }
    Ok(())
}

/// `base.write` with its relational steps.
pub(crate) async fn write(tx: &mut Transaction, records: &Records, vals: RawVals) -> Result<()> {
    let before = tx.mutations();
    let guard = tx.guard();
    let previous_origin = tx.pending_capture.active.clone();
    let result = write_guarded(tx, records, vals).await;
    tx.pending_capture.active = previous_origin;
    if let Err(error) = &result {
        tx.poison_after(before, error);
    }
    guard.disarm();
    result
}

async fn write_guarded(tx: &mut Transaction, records: &Records, vals: RawVals) -> Result<()> {
    tx.check_capture_write(records, &vals)?;
    crate::database_generated::check_write_names(tx, records.spec(), vals.names())?;
    if records.is_empty() {
        return Ok(());
    }
    tx.require_writable(crate::WriteOp::Write, records.spec().name)?;
    let env = records.env().clone();
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(records.model_id());
    let model = records.model_id();
    tx.prepare_access(&env).await?;
    tx.check_model(&env, model, crate::access::Operation::Write)?;
    tx.check_record_access(records, crate::access::Operation::Write)
        .await?;
    let names: Vec<&str> = vals
        .iter()
        .map(|(n, _)| n)
        .chain(vals.iter_commands().map(|(n, _)| n))
        .collect();
    // then every key of `vals`, before any value is compared, so a field the caller may not write
    // is refused even when the value would not change
    tx.check_field_names(
        &env,
        model,
        names.iter().copied(),
        crate::field_access::FieldOp::Write,
    )?;
    // a name that does not resolve (the write fails later) may still have reached the memo: all
    let touched: Option<Vec<FieldId>> = names.iter().map(|n| spec.field_id(n)).collect();
    tx.access_written(model, touched.as_deref());
    // a policy source or a membership: what is pending runs under the policy it started with
    let source = tx.before_policy_source_write(model, Some(&names)).await?;
    // tracked fields (D2): the records are prepared before anything is staged
    tx.track_prepare(records).await?;
    let result = write_values(tx, records, vals).await;
    // reads inside the write (delegated parents, constraints) may have memoized decisions from the
    // state before the buffered values: drop them again, also after a failure
    tx.access_written(model, touched.as_deref());
    // also after a failure: part of the write may be buffered (over-invalidation is safe)
    tx.policy_source_written(source);
    result
}

/// `base.write` once the checks passed.
async fn write_values(tx: &mut Transaction, records: &Records, mut vals: RawVals) -> Result<()> {
    let env = records.env().clone();
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(records.model_id());
    let model = records.model_id();
    if let Some(path) = spec.parent_path {
        // maintained by the ORM, never taken from values (CRUD §4.11)
        vals.remove(spec.field_by_id(path).name);
    }
    let x2many = split_x2many(spec, &mut vals)?;
    let inherited = crate::inherits::split_inherited(spec, &mut vals);
    check_delegated_targets(tx, records, &inherited, &vals).await?;
    // a parent store moves records whose parent changes: loops refused before any write
    let moves = match (spec.parent_path, spec.parent_name) {
        (Some(_), Some(parent)) => match vals.get(spec.field_by_id(parent).name).cloned() {
            Some(cell) => Some((tx.parent_store_moves(records, &cell).await?, cell)),
            None => None,
        },
        _ => None,
    };
    let mut one = [vals];
    tx.check_references(records, &mut one).await?;
    let [vals] = one;
    // many2one values whose inverse one2many values may be cached
    let mut m2o_changes: Vec<(FieldId, Vec<Option<Cell>>, Cell)> = Vec::new();
    for (name, cell) in vals.iter() {
        if let Some(field) = spec.field(name)
            && matches!(field.ty, FieldType::Many2one { .. })
            && !tx.shared.relations.inverses(model, field.id).is_empty()
        {
            let mut old = tx.old_values(model, field.id, records.rec_ids());
            if old.iter().any(Option::is_none) && tx.inverse_has_dependents(model, field.id) {
                records.engine_sudo().fetch(tx, Some(&[field.name])).await?;
                old = tx.old_values(model, field.id, records.rec_ids());
            }
            m2o_changes.push((field.id, old, cell.clone()));
        }
    }
    let mut written: Vec<FieldId> = vals.iter().filter_map(|(n, _)| spec.field_id(n)).collect();
    tx.forget_reference_inverses(model, &written);
    let mut delegated: FxHashSet<FieldId> = FxHashSet::default();
    for (delegate, values) in inherited {
        let delegate_name = spec.field_by_id(delegate).name;
        // the delegate the write sets (a new parent for new records), else the current one
        let target = vals.get(delegate_name).and_then(crate::relational::id_of);
        let parent = spec.inherits.iter().find(|(_, d)| *d == delegate);
        let parents = match (target, parent) {
            (Some(id), Some((parent, _))) => Records::empty(env.clone(), *parent).browse_ids([id]),
            _ => records.related(tx, delegate_name).await?,
        };
        delegated.extend(values.iter().filter_map(|(n, _)| spec.field_id(n)));
        parents.write(tx, values).await?;
    }
    let own: FxHashSet<FieldId> = written.iter().copied().collect();
    // non-stored many2one fields computed from the written fields that are the inverse of
    // one2many fields: their current values (the one2many's old owners) before the write
    let shared = Arc::clone(&tx.shared);
    let mut before: Vec<&'static str> = Vec::new();
    for fid in &written {
        for dep in shared.graph.model(model).dependents(*fid) {
            let name = spec.field_by_id(*dep).name;
            if !before.contains(&name)
                && tx
                    .inverse_one2manys((model, *dep))
                    .iter()
                    .any(|k| shared.graph.has_dependents(*k))
            {
                before.push(name);
            }
        }
    }
    let real: Vec<RecId> = records
        .rec_ids()
        .iter()
        .copied()
        .filter(|i| !i.is_new())
        .collect();
    if !real.is_empty() {
        let su = records.engine_sudo().browse_ids(real);
        for name in before {
            su.cells(tx, name).await?;
        }
    }
    // constraints that name an x2many field written here wait until its commands ran
    let postponed: FxHashSet<FieldId> = x2many.iter().map(|(f, _)| *f).collect();
    // Parent writes above have their own physical owner. Keep this source's immutable origin
    // through scalar staging and the inverse-owner notifications below.
    let relevant = tx.capture_write_relevant(records, &vals);
    tx.begin_capture_operation(records, relevant).await?;
    crate::write::write_records(tx, records, vals, &postponed).await?;
    if let Some((moved, cell)) = moves {
        tx.parent_store_move(records, &moved, &cell).await?;
    }
    for (field, old, new) in m2o_changes {
        tx.update_inverses(model, field, records, &old, &new)
            .await?;
    }
    if !delegated.is_empty() {
        let skip: FxHashSet<FieldId> = own.union(&postponed).copied().collect();
        tx.run_constraints(records, &delegated, &skip).await?;
        written.extend(delegated);
    }
    let mut x2many_fields: FxHashSet<FieldId> = FxHashSet::default();
    for (field, commands) in x2many {
        written.push(field);
        x2many_fields.insert(field);
        tx.write_x2many(model, field, vec![(records.clone(), commands)], false)
            .await?;
        // a non-stored x2many with an inverse persists its new value (COMP §4.7)
        tx.inverse_x2many_cache(records, field).await?;
    }
    tx.run_constraints(records, &x2many_fields, &FxHashSet::default())
        .await?;
    tx.check_company(records, Some(&written)).await?;
    // a record moved out of its generated company restriction fails now, with the new values in
    // a stored company field computed from a written field (a related company) moves too
    // (transitively: the graph lists direct dependents only; only the written records' own
    // fields, records reached through relations by the trigger tree are not rechecked)
    let mut moved: FxHashSet<FieldId> = written.iter().copied().collect();
    let mut pending: Vec<FieldId> = written.to_vec();
    while let Some(fid) = pending.pop() {
        for dep in tx.shared.graph.model(model).dependents(fid) {
            if moved.insert(*dep) {
                pending.push(*dep);
            }
        }
    }
    let moved: Vec<FieldId> = moved.into_iter().collect();
    tx.check_generated_restriction(records, &moved).await
}
