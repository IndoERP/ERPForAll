//! `unlink` (CRUD §4.6, REL §4.4.5) with targeted cache invalidation.
//!
//! Sequence: the model's `unlink` access check; pending recomputations of the records are dropped
//! and every pending write is flushed (the database applies `ON DELETE` actions to what it holds);
//! the dependents of what the deletion changes are collected (below); one
//! `DELETE FROM t WHERE id = ANY($1)` per chunk of [`erp_sql::relational::DELETE_CHUNK`] ids; then
//! the cache forgets exactly what the deletion can have changed, using the foreign-key metadata of
//! the registry:
//!
//! - the deleted records themselves (every field);
//! - for every many2one pointing to the model: `set null` targets become empty in cache;
//!   `cascade` rows cached with a deleted target are deleted too (recursively, so cascades chain);
//!   cached rows of a cascading model whose link value is not cached are forgotten, since they may
//!   be gone;
//! - a cascading model with nothing cached lost rows the cache cannot name: whatever references
//!   it is forgotten field by field (`set null` links) or model by model (further cascades),
//!   transitively;
//! - x2many values in cache lose the deleted ids; x2many fields whose comodel lost rows the cache
//!   cannot name are invalidated as a whole, field by field;
//! - above [`CACHE_SCAN_LIMIT`] cached rows of a model, whole fields are invalidated instead of
//!   scanning their values (no O(cache) work per unlink).
//!
//! After the `DELETE`, the changed
//! fields are notified (`modified`), so their dependents recompute on the next read or flush.
//! Deleted records are never recomputed.
//!
//! A `restrict` violation fails with the foreign-key message (a `Validation` error, which
//! poisons the transaction: the deletion did not happen).

use std::any::Any;
use std::sync::Arc;

use erp_fields::{Cell, Error, FieldType, OnDelete, RecId, Result};
use erp_registry::{FieldId, ModelId};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::access::Operation;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::env::Env;
use crate::records::Records;

/// Cached rows of a model above which a deletion invalidates whole fields of it instead of
/// scanning their cached values.
pub(crate) const CACHE_SCAN_LIMIT: usize = 10_000;

impl Records {
    /// `unlink()` through the method chain (`base` implements it). An empty recordset costs no
    /// query; deleting records that no longer exist is not an error. An error
    /// after rows were deleted poisons the transaction (F1).
    ///
    /// Eventful roots capture only IDs actually returned by DELETE, with bounded FK effect
    /// model names and the deleting environment's operation identity. Exact child
    /// IDs and unsupported indirect/relation effects rema.
    ///
    /// # Errors
    ///
    /// Access-policy refusals, `Programming` for missing identity or unsupported event-relevant
    /// effects, `ResourceExhausted` for capture bounds, and database/constraint failures. A
    /// failure after pending business work is discarded also prevents commit, before DELETE.
    pub async fn unlink(&self, tx: &mut Transaction) -> Result<()> {
        tx.check_records(self)?;
        if !self.is_empty() {
            check_delete_capture(tx, self)?;
        }
        let before = tx.mutations();
        let guard = tx.guard();
        let result = call_method::<(), ()>(tx, self, "unlink", ()).await;
        if let Err(error) = &result {
            tx.poison_after(before, error);
        }
        guard.disarm();
        result
    }
}

/// The database deleted relation rows of every many2many that points at `deleted` (the link
/// table's foreign keys cascade): the memoized access decisions of the models owning such a field
/// (and, through `Transaction::access_written`, of the mirror fields sharing the table) read a
/// changed link, whatever the dependents walk reports.
fn links_to_deleted(tx: &mut Transaction, registry: &erp_registry::Registry, deleted: ModelId) {
    let target = registry.model_by_id(deleted).name;
    for owner in registry.models() {
        let fields: Vec<FieldId> = owner
            .fields
            .iter()
            .filter(|f| matches!(&f.ty, FieldType::Many2many { comodel, .. } if *comodel == target))
            .map(|f| f.id)
            .collect();
        if !fields.is_empty() {
            tx.access_written(owner.id, Some(&fields));
        }
    }
}

/// A field whose values a deletion changes, and the records concerned (read before the delete).
struct Affected {
    model: ModelId,
    field: FieldId,
    ids: Vec<RecId>,
    /// A `set null` many2one: the records' cached value becomes empty.
    nulled: bool,
}

/// `base.unlink`: see the module documentation.
pub(crate) async fn unlink_records(tx: &mut Transaction, records: &Records) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    tx.check_records(records)?;
    check_delete_capture(tx, records)?;
    tx.require_writable(crate::WriteOp::Unlink, records.spec().name)?;
    let env = records.env().clone();
    let model = records.model_id();
    tx.prepare_access(&env).await?;
    tx.check_model(&env, model, Operation::Unlink)?;
    tx.check_record_access(records, Operation::Unlink).await?;
    // a policy source or a membership: what is pending runs under the policy it started with
    let source = tx.before_policy_source_write(model, None).await?;
    // dropped half-way (a timeout) after the DELETE: the dirty mark and the memo drop would be
    // skipped, so the guard poisons the transaction instead
    let before = tx.mutations();
    let guard = tx.guard();
    let previous_origin = tx.pending_capture.active.clone();
    let outcome = unlink_checked(tx, records, env, model).await;
    tx.pending_capture.active = previous_origin;
    if let Err(error) = &outcome {
        tx.poison_after(before, error);
    }
    // also after a failure: policy rows may be deleted already (over-invalidation is safe)
    tx.policy_source_written(source);
    guard.disarm();
    outcome
}

/// Admission is shared by public and directly dispatched base unlink. It is structural and
/// sends no query. An opted-in root's FK tombstone covers physical cascade/set-null effects;
/// other event-relevant relation/ancillary effects keep their explicit refusal.
fn check_delete_capture(tx: &mut Transaction, records: &Records) -> Result<()> {
    tx.check_records(records)?;
    if records.is_empty() {
        return Ok(());
    }
    let relevant = delete_capture_relevant(tx, records.model_id())?;
    if relevant && tx.pending_capture.active.is_some() {
        let error = Error::programming(
            "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented",
        );
        tx.poison_with(&error);
        return Err(error);
    }
    if relevant && records.env().operation_identity().is_none() {
        return Err(Error::programming(
            "outbox: eventful mutation requires an OperationIdentity",
        ));
    }
    // The bounded summary refuses overflow before any flush, override or destructive SQL.
    tx.shared
        .relations
        .deletion_capture_models(records.model_id())?;
    refuse_uncaptured_delete_effects(tx, records.model_id())
}

fn delete_capture_relevant(tx: &Transaction, root: ModelId) -> Result<bool> {
    let mut pending = vec![root];
    let mut seen = FxHashSet::default();
    while let Some(model) = pending.pop() {
        if !seen.insert(model) {
            continue;
        }
        if tx.capture_model_relevant(model) {
            return Ok(true);
        }
        for &(owner, field) in tx.shared.relations.x2many_to(model) {
            if tx.capture_relevant(owner, field) {
                return Ok(true);
            }
        }
        for &(owner, field) in tx.shared.relations.referencing(model) {
            let spec = tx.registry().model_by_id(owner);
            match &spec.field_by_id(field).ty {
                FieldType::Many2one {
                    ondelete: OnDelete::Cascade,
                    ..
                } => pending.push(owner),
                FieldType::Many2one {
                    ondelete: OnDelete::SetNull,
                    ..
                } => {
                    if tx.capture_relevant(owner, field) {
                        return Ok(true);
                    }
                }
                // Restrict does not mutate the referencing row; the registry index is typed.
                FieldType::Many2one {
                    ondelete: OnDelete::Restrict,
                    ..
                } => {}
                _ => {
                    return Err(Error::programming(
                        "outbox: the registry FK index contains a non-many2one field",
                    ));
                }
            }
        }
    }
    Ok(false)
}

fn refuse_uncaptured_delete_effects(tx: &Transaction, root: ModelId) -> Result<()> {
    let registry = &tx.shared.registry;
    let covered = registry.model_by_id(root).eventful;
    let ancillary = |model: ModelId| {
        Error::programming(format!(
            "a later release S8: event-relevant ancillary delete effects on model '{}' are not implemented",
            registry.model_by_id(model).name,
        ))
    };
    if crate::ir_default::is_available(registry)
        && let Some(defaults) = registry.model("ir.default")
        && tx.capture_model_relevant(defaults.id)
    {
        return Err(ancillary(defaults.id));
    }
    if let Some(effects) = tx.shared.relations.follow_deletes(root) {
        for &model in &effects.key_models {
            if tx.capture_model_relevant(model) {
                return Err(ancillary(model));
            }
        }
    }
    let mut pending = vec![root];
    let mut seen = FxHashSet::default();
    while let Some(model) = pending.pop() {
        if !seen.insert(model) {
            continue;
        }
        for &(owner, _) in tx.shared.relations.x2many_to(model) {
            crate::event_namespace::refuse_uncaptured_mutation(
                registry.model_by_id(owner),
                "relation",
            )?;
        }
        for &(owner, field) in tx.shared.relations.referencing(model) {
            let spec = registry.model_by_id(owner);
            let field = spec.field_by_id(field);
            // JSON company values are changed by cleanup SQL, not a physical FK action.
            if field.company_dependent {
                if tx.capture_relevant(owner, field.id) {
                    return Err(ancillary(owner));
                }
                continue;
            }
            match &field.ty {
                FieldType::Many2one {
                    ondelete: OnDelete::Cascade,
                    ..
                } => {
                    if !covered {
                        crate::event_namespace::refuse_uncaptured_mutation(spec, "unlink")?;
                    }
                    pending.push(owner);
                }
                FieldType::Many2one {
                    ondelete: OnDelete::SetNull,
                    ..
                } => {
                    if !covered {
                        crate::event_namespace::refuse_uncaptured_mutation(spec, "unlink")?;
                    }
                }
                FieldType::Many2one {
                    ondelete: OnDelete::Restrict,
                    ..
                } => {}
                _ => {
                    return Err(Error::programming(
                        "outbox: the registry FK index contains a non-many2one field",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `base.unlink` once the checks passed and the pending writes ran.
async fn unlink_checked(
    tx: &mut Transaction,
    records: &Records,
    env: Env,
    model: ModelId,
) -> Result<()> {
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(model);
    if !spec.has_table() {
        return Err(Error::programming(format!(
            "Cannot delete records of abstract model {}",
            spec.name
        )));
    }
    let mut seen = FxHashSet::default();
    let ids: Vec<RecId> = records
        .rec_ids()
        .iter()
        .copied()
        .filter(|i| !i.is_new() && seen.insert(*i))
        .collect();
    // Dropping pending business work changes transaction state even before a DELETE. A later
    // context/admission error must not allow a caught refusal to commit the earlier source
    // mutation with its stored output silently discarded (S8 review R1).
    let fields: Vec<FieldId> = spec.fields.iter().map(|f| f.id).collect();
    let dropped_pending = tx.cache.model(model).is_some_and(|cache| {
        fields
            .iter()
            .any(|field| ids.iter().any(|id| cache.is_pending(*field, *id)))
    });
    if dropped_pending {
        tx.note_mutation();
    }
    // Nothing to compute for records about to disappear.

    for f in &fields {
        tx.cache.model_mut(model).clear_pending(*f, &ids);
    }
    tx.pending_capture.prune(&tx.cache)?;
    let relevant = delete_capture_relevant(tx, model)?;
    tx.begin_capture_operation(records, relevant).await?;
    tx.flush().await?;
    if ids.is_empty() {
        return Ok(());
    }
    let force_delete = env
        .context()
        .get("force_delete")
        .is_some_and(|v| v.is_truthy());
    check_company_dependent_unlink(tx, &env, spec, &ids, force_delete).await?;

    if crate::ir_default::is_available(&registry) {
        crate::ir_default::discard_records(tx, records).await?;
    }
    if spec.name == "ir.sequence" || spec.name == "ir.sequence.date_range" {
        crate::sequence::on_records_unlinked(tx, records, &ids).await?;
    }
    if spec.name == "res.lang" {
        tx.active_langs = None;
        tx.langs_dirty = true;
    }

    let (affected, cascaded) = tx.deletion_dependents(model, &ids).await?;
    let sql = if spec.eventful {
        erp_sql::relational::delete_records_returning(&spec.table)
    } else {
        erp_sql::relational::delete_records(&spec.table)
    };
    for chunk in ids.chunks(erp_sql::relational::DELETE_CHUNK) {
        let values: Vec<i64> = chunk.iter().map(|i| i.get()).collect();
        // Followed effects are forgotten below, without a statement scan (C3-R4).
        if spec.eventful {
            let rows = tx.query_orm(&sql, &[&values]).await?;
            let allowed: FxHashSet<i64> = values.iter().copied().collect();
            let mut returned = FxHashSet::default();
            for row in rows {
                let id: i64 = row.try_get(0).map_err(|error| {
                    Error::new(
                        erp_fields::ErrorKind::Database,
                        "outbox: could not decode a DELETE id",
                    )
                    .with_source(error)
                })?;
                if RecId::from_real(id).is_none() || !allowed.contains(&id) || !returned.insert(id)
                {
                    return Err(Error::programming(
                        "outbox: DELETE returned an unexpected or duplicate id",
                    ));
                }
            }
            let mut actual: Vec<i64> = returned.into_iter().collect();
            actual.sort_unstable();
            let origin = tx.pending_capture.active.clone().ok_or_else(|| {
                Error::programming("outbox: eventful DELETE is missing deleting provenance")
            })?;
            for members in actual.chunks(crate::outbox::MAX_ID_ENTRIES) {
                let cascade_models = tx
                    .shared
                    .relations
                    .deletion_capture_models(model)?
                    .iter()
                    .map(|id| registry.model_by_id(*id).name.to_owned())
                    .collect();
                let envelope = origin.envelope(
                    crate::outbox::Kind::Unlink,
                    spec.name,
                    members
                        .iter()
                        .map(|id| crate::outbox::IdEntry::Single(*id))
                        .collect(),
                    Vec::new(),
                    cascade_models,
                )?;
                let depth = tx.savepoint_depth();
                tx.outbox.push(envelope, depth)?;
            }
        } else {
            tx.execute_orm(&sql, &[&values]).await?;
        }
    }
    tx.access_written(model, None);
    links_to_deleted(tx, &registry, model);
    let mut deleted = tx.forget_deleted(model, &ids);
    // rows the database deleted by cascade that the dependents walk named (cached or not)
    for (m, gone) in cascaded {
        tx.access_written(m, None);
        links_to_deleted(tx, &registry, m);
        let spec = registry.model_by_id(m);
        let gone_ids: Vec<RecId> = gone.iter().copied().collect();
        for f in &spec.fields {
            tx.cache.model_mut(m).clear_pending(f.id, &gone_ids);
        }
        tx.cache.model_mut(m).invalidate(None, Some(&gone_ids));
        tx.engine.context_mut().invalidate(m, None, Some(&gone_ids));
        deleted.entry(m).or_default().extend(gone);
    }
    // deleted records have nothing left to report as tracked changes
    let depth = tx.savepoint_depth();
    if !spec.tracked.is_empty() {
        tx.tracking.gone(depth, model, &ids);
    }
    for (m, gone) in &deleted {
        if !registry.model_by_id(*m).tracked.is_empty() {
            let gone_ids: Vec<RecId> = gone.iter().copied().collect();
            tx.tracking.gone(depth, *m, &gone_ids);
        }
    }
    // what the follow-delete triggers did (key rows, cached values of other processes): forgotten
    // here too, for this transaction
    let follows = Arc::clone(&tx.shared);
    if let Some(effects) = follows.relations.follow_deletes(model) {
        tx.forget_followed(effects);
    }
    for a in affected {
        let gone = deleted.get(&a.model);
        let ids: Vec<RecId> = a
            .ids
            .into_iter()
            .filter(|i| gone.is_none_or(|g| !g.contains(i)))
            .collect();
        if ids.is_empty() {
            continue;
        }
        if a.nulled {
            let storage = registry.model_by_id(a.model).field_by_id(a.field).storage();
            let mc = tx.cache.model_mut(a.model);
            for id in &ids {
                let set = mc.set(a.field, storage, *id, Cell::Null, false);
                debug_assert!(set.is_ok(), "cache column of another storage");
                if set.is_err() {
                    mc.invalidate(Some(&[a.field][..]), Some(&[*id][..]));
                }
            }
        }
        tx.modified(a.model, vec![(a.field, ids)]).await?;
    }
    tx.pending_capture.prune(&tx.cache)?;
    // Unlink hooks: once, with the ids of this call, after the cache is consistent.
    if !tx.shared.unlink_hooks.is_empty() {
        let hooks = tx.shared.unlink_hooks.clone();
        for hook in &hooks {
            hook.call(tx, spec.name, &ids).await?;
        }
    }
    Ok(())
}

fn decode_ids(rows: &[tokio_postgres::Row]) -> Result<Vec<RecId>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let raw: i64 = row.try_get(0).map_err(|e| {
            Error::new(erp_fields::ErrorKind::Database, "could not decode an id").with_source(e)
        })?;
        if let Some(id) = RecId::from_real(raw) {
            out.push(id);
        }
    }
    Ok(out)
}

impl Transaction {
    /// Forget what the follow-delete triggers changed for a deletion ([`FollowEffects`]): the
    /// named caches, and every record of the key-table models.
    pub(crate) fn forget_followed(&mut self, effects: &crate::relational::FollowEffects) {
        for cache in &effects.caches {
            self.invalidate_ormcache(cache);
        }
        for &m in &effects.key_models {
            self.access_written(m, None);
            self.cache.model_mut(m).invalidate(None, None);
            self.engine.context_mut().invalidate(m, None, None);
        }
    }

    /// [`Transaction::forget_followed`] for a deletion from `table`, when its deletions are
    /// followed (a many2many relation table the ORM removed pairs from).
    pub(crate) fn forget_followed_table(&mut self, table: &str) {
        let shared = Arc::clone(&self.shared);
        if let Some((_, effects)) = shared
            .relations
            .follow_tables()
            .iter()
            .find(|(followed, _)| followed == table)
        {
            self.forget_followed(effects);
        }
    }

    fn cascade_matters(&self, model: ModelId, visited: &mut FxHashSet<ModelId>) -> bool {
        if !visited.insert(model) {
            return false;
        }
        let shared = Arc::clone(&self.shared);
        let graph = &shared.graph;
        let spec = shared.registry.model_by_id(model);
        if spec
            .fields
            .iter()
            .any(|f| graph.has_dependents((model, f.id)))
        {
            return true;
        }
        if shared
            .relations
            .x2many_to(model)
            .iter()
            .any(|k| graph.has_dependents(*k))
        {
            return true;
        }
        for &(rm, rf) in shared.relations.referencing(model) {
            match &shared.registry.model_by_id(rm).field_by_id(rf).ty {
                FieldType::Many2one {
                    ondelete: OnDelete::SetNull,
                    ..
                } if graph.has_dependents((rm, rf)) => return true,
                FieldType::Many2one {
                    ondelete: OnDelete::Cascade,
                    ..
                } if self.cascade_matters(rm, visited) => return true,
                _ => {}
            }
        }
        false
    }

    /// The fields a deletion of `ids` changes that something depends on, with the records
    /// concerned, read before the `DELETE` (module documentation); and the rows the database
    /// will delete by cascade where that matters, per model. The dependents of the deleted
    /// records' own fields — and of the cascaded rows' — are notified here.
    async fn deletion_dependents(
        &mut self,
        model: ModelId,
        ids: &[RecId],
    ) -> Result<(Vec<Affected>, FxHashMap<ModelId, FxHashSet<RecId>>)> {
        let shared = Arc::clone(&self.shared);
        let registry = &shared.registry;
        let graph = &shared.graph;
        let mut out = Vec::new();
        let mut cascaded: FxHashMap<ModelId, FxHashSet<RecId>> = FxHashMap::default();
        let mut known: FxHashMap<ModelId, FxHashSet<RecId>> = FxHashMap::default();
        known.entry(model).or_default().extend(ids.iter().copied());
        let mut queue: Vec<(ModelId, Vec<RecId>)> = vec![(model, ids.to_vec())];
        while let Some((m, gone)) = queue.pop() {
            let spec = registry.model_by_id(m);
            let values: Vec<i64> = gone.iter().map(|i| i.get()).collect();
            // the deleted records' own fields: their dependents on the same records are dropped
            // with them; dependents through relations are marked
            self.modified(
                m,
                spec.fields.iter().map(|f| (f.id, gone.clone())).collect(),
            )
            .await?;
            for &(rm, rf) in shared.relations.referencing(m) {
                let field = registry.model_by_id(rm).field_by_id(rf);
                let table = &registry.model_by_id(rm).table;
                match &field.ty {
                    // `set null` many2one values of other records
                    FieldType::Many2one {
                        ondelete: OnDelete::SetNull,
                        ..
                    } if graph.has_dependents((rm, rf)) => {
                        let sql = erp_sql::relational::ids_referencing(table, field.name);
                        let rows = self.query(&sql, &[&values]).await?;
                        let found = decode_ids(&rows)?;
                        if !found.is_empty() {
                            out.push(Affected {
                                model: rm,
                                field: rf,
                                ids: found,
                                nulled: true,
                            });
                        }
                    }
                    // rows deleted by cascade, followed when something depends on them
                    FieldType::Many2one {
                        ondelete: OnDelete::Cascade,
                        ..
                    } if self.cascade_matters(rm, &mut FxHashSet::default()) => {
                        let sql = erp_sql::relational::ids_referencing(table, field.name);
                        let rows = self.query(&sql, &[&values]).await?;
                        let seen = known.entry(rm).or_default();
                        let fresh: Vec<RecId> = decode_ids(&rows)?
                            .into_iter()
                            .filter(|id| seen.insert(*id))
                            .collect();
                        if !fresh.is_empty() {
                            cascaded
                                .entry(rm)
                                .or_default()
                                .extend(fresh.iter().copied());
                            queue.push((rm, fresh));
                        }
                    }
                    _ => {}
                }
            }
            // x2many values losing the deleted records
            for &(xm, xf) in shared.relations.x2many_to(m) {
                if !graph.has_dependents((xm, xf)) {
                    continue;
                }
                let owner_spec = registry.model_by_id(xm);
                let sql = match &owner_spec.field_by_id(xf).ty {
                    FieldType::One2many { inverse, .. } => match spec.field(inverse) {
                        Some(inv) if inv.has_column() => {
                            erp_sql::relational::targets_of(&spec.table, inv.name)
                        }
                        Some(inv) => {
                            // an inverse without a column (a related many2one): its values,
                            // read before the rows go
                            let env = self
                                .env()
                                .sudo(true)
                                .with_context([("active_test", erp_fields::Value::Bool(false))]);
                            let gone_arc: Arc<[RecId]> = Arc::from(gone.clone());
                            let deleted =
                                Records::from_parts(env, m, Arc::clone(&gone_arc), gone_arc);
                            let mut found: Vec<RecId> = Vec::new();
                            for cell in self.field_cells(&deleted, inv.id).await? {
                                if let Some(owner) = crate::relational::id_of(&cell)
                                    && !found.contains(&owner)
                                {
                                    found.push(owner);
                                }
                            }
                            if !found.is_empty() {
                                out.push(Affected {
                                    model: xm,
                                    field: xf,
                                    ids: found,
                                    nulled: false,
                                });
                            }
                            continue;
                        }
                        None => {
                            return Err(Error::programming(format!(
                                "{}.{}: inverse field '{inverse}' missing on {}",
                                owner_spec.name,
                                owner_spec.field_by_id(xf).name,
                                spec.name
                            )));
                        }
                    },
                    FieldType::Many2many {
                        relation: Some(rel),
                        ..
                    } => erp_sql::relational::m2m_owners(erp_sql::relational::Relation {
                        table: &rel.table,
                        column1: &rel.column1,
                        column2: &rel.column2,
                    }),
                    _ => continue,
                };
                let rows = self.query(&sql, &[&values]).await?;
                let found = decode_ids(&rows)?;
                if !found.is_empty() {
                    out.push(Affected {
                        model: xm,
                        field: xf,
                        ids: found,
                        nulled: false,
                    });
                }
            }
        }
        Ok((out, cascaded))
    }

    /// Targeted invalidation after `ids` of `model` were deleted (module documentation). Returns
    /// the records known to be deleted, per model (cascades included).
    pub(crate) fn forget_deleted(
        &mut self,
        model: ModelId,
        ids: &[RecId],
    ) -> FxHashMap<ModelId, FxHashSet<RecId>> {
        let shared = Arc::clone(&self.shared);
        let registry = &shared.registry;
        // models whose rows were deleted, with the ids the cache can name
        let mut deleted: FxHashMap<ModelId, FxHashSet<RecId>> = FxHashMap::default();
        // models that may have lost rows the cache cannot name, with the cascading fields
        let mut unknown: FxHashMap<ModelId, Vec<FieldId>> = FxHashMap::default();
        // cascading models with nothing cached: what references them is followed transitively
        let mut uncached: Vec<ModelId> = Vec::new();
        let mut queue: Vec<(ModelId, Vec<RecId>)> = vec![(model, ids.to_vec())];
        while let Some((m, gone)) = queue.pop() {
            let entry = deleted.entry(m).or_default();
            let fresh: Vec<RecId> = gone.into_iter().filter(|i| entry.insert(*i)).collect();
            if fresh.is_empty() {
                continue;
            }
            let fresh_set: FxHashSet<RecId> = fresh.iter().copied().collect();
            for &(rm, rf) in shared.relations.referencing(m) {
                let field = registry.model_by_id(rm).field_by_id(rf);
                let FieldType::Many2one { ondelete, .. } = &field.ty else {
                    continue;
                };
                if *ondelete == OnDelete::Restrict {
                    continue;
                }
                let cascade = *ondelete == OnDelete::Cascade;
                if cascade {
                    // uncached rows of `rm` may be gone too
                    let vias = unknown.entry(rm).or_default();
                    if !vias.contains(&rf) {
                        vias.push(rf);
                    }
                }
                let Some(mc) = self.cache.model(rm).filter(|mc| !mc.is_empty()) else {
                    if cascade {
                        uncached.push(rm);
                    }
                    continue;
                };
                if mc.len() > CACHE_SCAN_LIMIT {
                    if cascade {
                        self.cache.model_mut(rm).invalidate(None, None);
                        self.engine.context_mut().invalidate(rm, None, None);
                    } else {
                        self.cache.model_mut(rm).invalidate(Some(&[rf][..]), None);
                    }
                    continue;
                }
                let values = mc.cached_values(rf);
                let hit: Vec<RecId> = values
                    .iter()
                    .filter(|(_, c)| {
                        crate::relational::id_of(c).is_some_and(|t| fresh_set.contains(&t))
                    })
                    .map(|(id, _)| *id)
                    .collect();
                if cascade {
                    // rows cached without their link value may be gone as well
                    let known: FxHashSet<RecId> = values.iter().map(|(id, _)| *id).collect();
                    let unsure: Vec<RecId> = mc
                        .cached_ids()
                        .into_iter()
                        .filter(|id| !known.contains(id))
                        .collect();
                    if !unsure.is_empty() {
                        self.cache.model_mut(rm).invalidate(None, Some(&unsure));
                    }
                    if !hit.is_empty() {
                        queue.push((rm, hit));
                    }
                } else {
                    let storage = field.storage();
                    let mc = self.cache.model_mut(rm);
                    for id in hit {
                        // flushed before the DELETE: never a pending write here
                        let set = mc.set(rf, storage, id, Cell::Null, false);
                        debug_assert!(set.is_ok(), "cache column of another storage");
                        if set.is_err() {
                            mc.invalidate(Some(&[rf][..]), Some(&[id][..]));
                        }
                    }
                }
            }
        }
        // whatever references an uncached cascading model, which lost rows the cache cannot
        // name: `set null` links are forgotten field by field, further cascades model by model,
        // transitively
        let mut pending: Vec<ModelId> = uncached;
        let mut visited: FxHashSet<ModelId> = FxHashSet::default();
        while let Some(m) = pending.pop() {
            if !visited.insert(m) {
                continue;
            }
            for &(rm, rf) in shared.relations.referencing(m) {
                let FieldType::Many2one { ondelete, .. } =
                    &registry.model_by_id(rm).field_by_id(rf).ty
                else {
                    continue;
                };
                match ondelete {
                    OnDelete::SetNull => {
                        if self.cache.model(rm).is_some() {
                            self.cache.model_mut(rm).invalidate(Some(&[rf][..]), None);
                        }
                    }
                    OnDelete::Cascade => {
                        if self.cache.model(rm).is_some() {
                            self.cache.model_mut(rm).invalidate(None, None);
                            self.engine.context_mut().invalidate(rm, None, None);
                        }
                        let vias = unknown.entry(rm).or_default();
                        if !vias.contains(&rf) {
                            vias.push(rf);
                        }
                        pending.push(rm);
                    }
                    OnDelete::Restrict => {}
                }
            }
        }
        for (m, gone) in &deleted {
            let gone: Vec<RecId> = gone.iter().copied().collect();
            let spec = registry.model_by_id(*m);
            let fields: Vec<FieldId> = spec.fields.iter().map(|f| f.id).collect();
            for f in &fields {
                self.cache.model_mut(*m).clear_pending(*f, &gone);
            }
            self.cache.model_mut(*m).invalidate(None, Some(&gone));
            self.engine.context_mut().invalidate(*m, None, Some(&gone));
        }
        // x2many values in cache
        let mut targets: Vec<ModelId> = deleted.keys().chain(unknown.keys()).copied().collect();
        targets.sort_unstable();
        targets.dedup();
        let empty = FxHashSet::default();
        for target in targets {
            let gone = deleted.get(&target).unwrap_or(&empty);
            for &(xm, xf) in shared.relations.x2many_to(target) {
                if let Some(vias) = unknown.get(&target) {
                    // a one2many over the cascading many2one only listed lines of deleted owners;
                    // any other x2many may list rows the cache cannot name
                    let over_via = match &registry.model_by_id(xm).field_by_id(xf).ty {
                        FieldType::One2many { inverse, .. } => registry
                            .model_by_id(target)
                            .field_id(inverse)
                            .is_some_and(|f| vias.contains(&f)),
                        _ => false,
                    };
                    if !over_via {
                        self.cache.model_mut(xm).invalidate(Some(&[xf][..]), None);
                        continue;
                    }
                }
                if gone.is_empty() {
                    continue;
                }
                let Some(mc) = self.cache.model(xm) else {
                    continue;
                };
                if mc.len() > CACHE_SCAN_LIMIT {
                    self.cache.model_mut(xm).invalidate(Some(&[xf][..]), None);
                    continue;
                }
                let values = mc.cached_values(xf);
                let storage = registry.model_by_id(xm).field_by_id(xf).storage();
                let mc = self.cache.model_mut(xm);
                for (id, cell) in values {
                    if let Cell::Ids(current) = cell
                        && current.iter().any(|i| gone.contains(i))
                    {
                        let kept: Vec<RecId> = current
                            .iter()
                            .copied()
                            .filter(|i| !gone.contains(i))
                            .collect();
                        let set = mc.set(xf, storage, id, Cell::Ids(kept.into()), false);
                        debug_assert!(set.is_ok(), "cache column of another storage");
                        if set.is_err() {
                            mc.invalidate(Some(&[xf][..]), Some(&[id][..]));
                        }
                    }
                }
            }
        }
        deleted
    }
}

async fn check_company_dependent_unlink(
    tx: &mut Transaction,
    env: &crate::Env,
    spec: &erp_registry::ModelSpec,
    ids: &[RecId],
    force_delete: bool,
) -> Result<()> {
    let registry = env.registry();
    let sub_ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
    let sub_ids_str: Vec<String> = ids.iter().map(|id| id.get().to_string()).collect();

    if !force_delete && crate::ir_default::is_available(registry) {
        let default_check_sql = r#"
            SELECT f.model, f.name, d.json_value
            FROM ir_default d
            JOIN ir_model_fields f ON d.field_id = f.id
            WHERE f.relation = $1 AND d.json_value = ANY($2)
            LIMIT 1
        "#;
        let rows = tx
            .query(default_check_sql, &[&spec.name, &sub_ids_str])
            .await?;
        if let Some(row) = rows.first() {
            let _m_name: String = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            let f_name: String = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
            let json_v: String = row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
            return Err(Error::user(format!(
                "Unable to delete {spec_name},{json_v} because it is used as the default value of {f_name}",
                spec_name = spec.name,
            )));
        }
    }

    for other_model in registry.models() {
        for f in &other_model.fields {
            if f.company_dependent && f.ty.comodel() == Some(spec.name) {
                // (a) ondelete = restrict
                if f.ty.ondelete() == Some(OnDelete::Restrict) && !force_delete {
                    let sql = format!(
                        "SELECT id, \"{field}\" FROM \"{table}\" WHERE \"{field}\" IS NOT NULL LIMIT 1",
                        field = f.name,
                        table = other_model.table,
                    );
                    {
                        let rows = tx.query(&sql, &[]).await?;
                        for row in rows {
                            let other_id: i64 =
                                row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                            let raw_json: serde_json::Value =
                                row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
                            if let Some(obj) = raw_json.as_object() {
                                for (_, v) in obj {
                                    if let Some(target_id) = v.as_i64()
                                        && sub_ids.contains(&target_id)
                                    {
                                        return Err(Error::user(format!(
                                            "You cannot delete {spec_name},{target_id}, as it is used by {other_model},{other_id}",
                                            spec_name = spec.name,
                                            other_model = other_model.name,
                                        )));
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // (b) default policy: set null in jsonb
                    let sql = format!(
                        "SELECT id, \"{field}\" FROM \"{table}\" WHERE \"{field}\" IS NOT NULL",
                        field = f.name,
                        table = other_model.table,
                    );
                    {
                        let rows = tx.query(&sql, &[]).await?;
                        let mut touched = false;
                        for row in rows {
                            let other_id: i64 =
                                row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                            let raw_json: serde_json::Value =
                                row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
                            if let Some(obj) = raw_json.as_object() {
                                let mut modified = false;
                                let mut new_obj = serde_json::Map::new();
                                for (k, v) in obj {
                                    if let Some(target_id) = v.as_i64()
                                        && sub_ids.contains(&target_id)
                                    {
                                        new_obj.insert(k.clone(), serde_json::Value::Null);
                                        modified = true;
                                    } else {
                                        new_obj.insert(k.clone(), v.clone());
                                    }
                                }
                                if modified {
                                    let update_sql = format!(
                                        "UPDATE \"{table}\" SET \"{field}\" = $1 WHERE id = $2",
                                        field = f.name,
                                        table = other_model.table,
                                    );
                                    tx.execute(
                                        &update_sql,
                                        &[&serde_json::Value::Object(new_obj), &other_id],
                                    )
                                    .await?;
                                    touched = true;
                                }
                            }
                        }
                        if touched {
                            // a rule may read the field this statement changed behind the
                            // cache: end the memoized record checks and the cached values
                            tx.access_written(other_model.id, Some(&[f.id]));
                            tx.cache
                                .model_mut(other_model.id)
                                .invalidate(Some(&[f.id]), None);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ---- base implementation of `unlink` ---------------------------------------------------------

type UnlinkFn = MethodFn<(), ()>;

fn base_unlink<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    _args: (),
    _sup: Super<(), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(unlink_records(tx, records))
}

fn unlink_entry() -> &'static (dyn Any + Send + Sync) {
    static F: UnlinkFn = base_unlink;
    &F
}

/// `base.unlink`, registered by [`crate::Dispatch::build`] with the other core implementations.
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 1] = [&BASE_UNLINK];

static BASE_UNLINK: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "unlink",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<UnlinkFn>,
    entry: unlink_entry,
    file: file!(),
    line: line!(),
};
