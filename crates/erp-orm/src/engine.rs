//! The compute engine (COMP §4.4–4.6, §4.11, §6).
//!
//! - **Marking** ([`Transaction::modified`], `crate::trigger`): the dependents of changed fields,
//!   on the same model and through relations, are visited once per record in global rank order;
//!   stored ones join their *to-compute* set and stop the walk (their dependents are marked only
//!   when their recomputed value changes), non-stored ones lose their cached values.
//! - **Recompute.** Pending fields are computed once per field over the whole pending set, in
//!   global dependency order, in bounded batches ([`crate::OrmConfig::recompute_batch`]); a
//!   recursive field level by level. Reading or searching a computed field first settles the
//!   pending stored fields it depends on, on any model (pull recompute).
//! - **Inputs.** A sync compute gets its own records' fields and, per path step, the values of
//!   the records its paths reach (one prefetch per step for the batch, `Batch::value_of`);
//!   related fields follow their path the same way.
//! - **Protection.** `(field, record)` pairs being computed or written through an inverse are
//!   exempt from marking (COMP §4.6).
//! - **Cancellation.** Every scope that changes engine state (protection, compute, savepoint) holds
//!   a [`ScopeGuard`]; when its future is dropped before the scope ends, the transaction is
//!   poisoned and only `rollback()` (or dropping it) is accepted afterwards.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use erp_fields::{Cell, Error, RecId, Result, Value};
use erp_registry::{FieldId, ModelId, ModelSpec};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::compute::{BatchData, Outputs, RelColumn, run_async, run_candidate, run_sync};
use crate::dispatch::{BoxFuture, call_unit_method};
use crate::env::Env;
use crate::graph::{FieldCompute, FieldKey, Source};
use crate::records::Records;
use crate::related_company::{Bound, Lenient};

/// Internal related traversal mode. Currency result access is privileged while
/// native execution keeps its declared environment and the original company bound.
#[derive(Clone)]
struct IdentityReads<'a> {
    native: Option<Env>,
    bound: Option<Bound<'a>>,
    direct: bool,
    depth: usize,
}

/// One protection scope entry.
#[derive(Debug)]
struct Protection {
    model: ModelId,
    fields: Vec<FieldId>,
    ids: FxHashSet<RecId>,
}

/// Marks the transaction poisoned when dropped while armed: the future holding it was cancelled
/// in the middle of a scope that changes engine state.
#[derive(Debug)]
pub(crate) struct ScopeGuard {
    flag: Arc<AtomicBool>,
    armed: bool,
}

impl ScopeGuard {
    /// The scope ended normally (successfully or with an error).
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::SeqCst);
        }
    }
}

/// Records whose stored many2one a compute changed, per (field, new value), with their old values.
type Reassigned = (FieldId, Cell, Vec<RecId>, Vec<Option<Cell>>);

/// Levels a recursive field may take to settle in one recompute (a cycle in the data whose values
/// keep changing would otherwise loop).
const RECURSION_PASSES: usize = 10_000;

/// The values of one context-dependent field: per cache key, per record.
type PerKey = FxHashMap<Vec<Value>, FxHashMap<RecId, Cell>>;

/// Values of context-dependent fields: per (model, field), per cache key, per record. Few fields
/// are context-dependent, so hash maps are enough here; the columnar cache holds everything else.
#[derive(Debug, Default, Clone)]
pub(crate) struct ContextCache {
    values: FxHashMap<(ModelId, FieldId), PerKey>,
}

impl ContextCache {
    /// Whether `id` has a value of the field in some context.
    pub(crate) fn contains(&self, model: ModelId, field: FieldId, id: RecId) -> bool {
        self.values
            .get(&(model, field))
            .is_some_and(|per_key| per_key.values().any(|v| v.contains_key(&id)))
    }

    /// Whether some record has a value of the field in some context.
    pub(crate) fn has_values(&self, model: ModelId, field: FieldId) -> bool {
        self.values
            .get(&(model, field))
            .is_some_and(|per_key| per_key.values().any(|v| !v.is_empty()))
    }

    /// Drop the values of `fields` (all when `None`) for `ids` (all when `None`), in every context.
    pub(crate) fn invalidate(
        &mut self,
        model: ModelId,
        fields: Option<&[FieldId]>,
        ids: Option<&[RecId]>,
    ) {
        self.values.retain(|(m, f), per_key| {
            if *m != model || fields.is_some_and(|fs| !fs.contains(f)) {
                return true;
            }
            match ids {
                None => false,
                Some(ids) => {
                    for values in per_key.values_mut() {
                        for id in ids {
                            values.remove(id);
                        }
                    }
                    true
                }
            }
        });
    }
}

/// Engine data saved by [`EngineState::snapshot`].
#[derive(Debug)]
pub(crate) struct EngineSnapshot {
    properties: Arc<crate::company_dependent_runtime::State>,
    new_records: Arc<FxHashMap<RecId, ModelId>>,
    context: Arc<ContextCache>,
    refs: Arc<FxHashMap<String, RecId>>,
    patches: Arc<FxHashMap<(ModelId, FieldId, RecId), Vec<RecId>>>,
}

/// The error of a company-bound field whose compute source is not a related path (review RD6):
/// the registry only makes related fields company-bound, so this is a bug, not an input.
fn not_a_related_source(
    registry: &erp_registry::Registry,
    model: ModelId,
    field: FieldId,
) -> Error {
    let spec = registry.model_by_id(model);
    Error::programming(format!(
        "company-bound field {}.{} does not read a related path (its source is a compute method \
         or the display name)",
        spec.name,
        spec.field_by_id(field).name
    ))
}

/// Engine state owned by a transaction.
#[derive(Debug, Default)]
pub(crate) struct EngineState {
    pub(crate) properties: Arc<crate::company_dependent_runtime::State>,
    protected: Vec<Protection>,
    pub(crate) context: Arc<ContextCache>,
    pub(crate) next_new: i64,
    /// Active originless cache identities. Synthetic pre-INSERT IDs are never registered.
    pub(crate) new_records: Arc<FxHashMap<RecId, ModelId>>,
    pub(crate) refs: Arc<FxHashMap<String, RecId>>,
    /// Additions to x2many values of new records that are not cached yet (`newrec`).
    pub(crate) patches: Arc<FxHashMap<(ModelId, FieldId, RecId), Vec<RecId>>>,
    /// Time zones checked against PostgreSQL's (grouped reads bucket datetimes in them).
    pub(crate) pg_timezones: FxHashMap<String, bool>,
    pub(crate) ormcache: crate::ormcache::OrmCacheLayers,
    /// Cached decimal precisions (`decimal.precision`).
    pub(crate) precisions: FxHashMap<String, u32>,
    /// Only scales resolved from the stored service, never metadata overrides.
    pub(crate) write_precisions: FxHashMap<String, u32>,
    /// Set by a [`ScopeGuard`] dropped while armed.
    pub(crate) cancelled: Arc<AtomicBool>,
    /// Record-rule decisions of x2many reads, valid while nothing changed.
    pub(crate) readable: ReadableMemo,
}

/// Dropped on: a statement sent to the database that may change
/// rows or locks them (its write-tick target is not `Reads`, or it holds a row-lock clause; a raw
/// `SET`/`RESET`; anything the word scanner does not read with certainty; every `batch`; see
/// `transaction/write_gens.rs`), a pending write on
/// a database record, a recompute that changes one, a savepoint rollback, the end of an isolated
/// scope, `invalidate_all/model/records` and the end of the transaction. A write to an in-memory
/// record does not drop it: the decision for a new id comes from its origin's row.
///
/// Keyed by the effective binding and the model ((g), R-M1): a policy sees the whole
/// principal (companies, scope, grants, elevation, groups), so two principals with the same uid
/// share nothing, and an older `Env` clone is answered with the newest resolution. The policy is
/// brought up to date (`prepare_access`) before the memo is consulted, and a refresh clears it.
#[derive(Debug, Default)]
pub(crate) struct ReadableMemo {
    entries: FxHashMap<MemoKey, ReadableEntry>,
}

/// What the decisions depend on: the effective binding (the whole principal by value, with the
/// policy snapshot and the interned group set, (g), R-M1) and the model. The context is
/// no part of it: access predicates are compiled under a fixed context
/// ([`crate::search::predicate_records`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MemoKey {
    binding: crate::principal::BindingKey,
    model: ModelId,
}

impl MemoKey {
    pub(crate) fn new(binding: crate::principal::BindingKey, model: ModelId) -> Self {
        Self { binding, model }
    }
}

/// The decisions for one [`MemoKey`].
#[derive(Debug, Default)]
pub(crate) struct ReadableEntry {
    /// The access policy puts no restriction on the model: everything is readable.
    pub(crate) unrestricted: bool,
    /// Per database record: whether the rules let the environment read it.
    /// `false` decisions are kept as well (fail closed).
    pub(crate) decisions: FxHashMap<erp_fields::RealId, bool>,
    /// The fields the decisions' rule reads (flushed before each query): a pending write to one
    /// of them on a database record ends the decisions.
    pub(crate) inputs: FxHashSet<(ModelId, FieldId)>,
}

impl ReadableMemo {
    /// The decisions for `key` (created empty), one lookup.
    pub(crate) fn entry(&mut self, key: &MemoKey) -> &mut ReadableEntry {
        self.entries.entry(key.clone()).or_default()
    }

    /// The entry for `key` if any decision was remembered.
    #[cfg(test)]
    pub(crate) fn peek(&self, key: &MemoKey) -> Option<&ReadableEntry> {
        self.entries.get(key)
    }

    /// A pending write to `fields` of database records of `model`: the entries whose rule reads
    /// one of them (or an audit field, which every write changes) end. A rule that reads a
    /// computed field also ends when a field it is computed from, through any number of stored
    /// computes and models, is changed: a stored compute further down the chain is only marked
    /// when the one before it recomputes, which may be after the next decision.
    pub(crate) fn fields_written(
        &mut self,
        model: ModelId,
        fields: &[FieldId],
        is_audit: impl Fn(ModelId, FieldId) -> bool,
        graph: &crate::graph::ComputeGraph,
    ) {
        self.entries.retain(|_, e| {
            !e.inputs.iter().any(|(m, f)| {
                (*m == model && (fields.contains(f) || is_audit(*m, *f)))
                    || graph
                        .upstream((*m, *f))
                        .iter()
                        .any(|(um, uf)| *um == model && fields.contains(uf))
            })
        });
    }

    pub(crate) fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries.clear();
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The per-record decisions remembered, over every entry.
    pub(crate) fn decision_count(&self) -> usize {
        self.entries.values().map(|e| e.decisions.len()).sum()
    }

    /// Forget every per-record decision when there are more than `keep` (the end of a stream's
    /// window, `rows_stream.rs`: what the windows asked is not carried through the rest of a
    /// stream, so the memo stays within about one window). The `unrestricted` answers and the
    /// rules' inputs stay; a forgotten decision is asked again when a record comes back.
    pub(crate) fn shed_decisions(&mut self, keep: usize) {
        if self.decision_count() > keep {
            for entry in self.entries.values_mut() {
                entry.decisions.clear();
            }
        }
    }
}

impl EngineState {
    /// Forget cached data (savepoint rollback).
    pub(crate) fn clear(&mut self) {
        self.readable.clear();
        self.properties = Arc::default();
        self.context = Arc::default();
        self.refs = Arc::default();
        self.new_records = Arc::default();
        self.patches = Arc::default();
        self.pg_timezones.clear();
        self.precisions.clear();
        self.write_precisions.clear();
    }

    /// The cached data an isolated scope restores: context-dependent values, new
    /// record references and x2many patches. New-record ids keep increasing (never reused), and
    /// protections are scoped by their own guards. Copy on first write (review M5): the snapshot
    /// shares the maps (`Arc`), and a mutation inside the scope copies one through
    /// `Arc::make_mut` only then, so an onchange that does not touch them copies nothing.
    pub(crate) fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot {
            properties: Arc::clone(&self.properties),
            new_records: Arc::clone(&self.new_records),
            context: Arc::clone(&self.context),
            refs: Arc::clone(&self.refs),
            patches: Arc::clone(&self.patches),
        }
    }

    /// The context-dependent values, for writing (copied first if a snapshot shares them).
    pub(crate) fn context_mut(&mut self) -> &mut ContextCache {
        Arc::make_mut(&mut self.context)
    }

    /// The new-record references, for writing (copied first if a snapshot shares them).
    pub(crate) fn refs_mut(&mut self) -> &mut FxHashMap<String, RecId> {
        Arc::make_mut(&mut self.refs)
    }

    /// The x2many patches of new records, for writing (copied first if a snapshot shares them).
    pub(crate) fn patches_mut(&mut self) -> &mut FxHashMap<(ModelId, FieldId, RecId), Vec<RecId>> {
        Arc::make_mut(&mut self.patches)
    }

    /// Put back what [`EngineState::snapshot`] saved.
    pub(crate) fn restore(&mut self, snapshot: EngineSnapshot) {
        self.readable.clear();
        // Metadata read inside an isolated scope may depend on rows it rolled back.
        self.precisions.clear();
        self.write_precisions.clear();
        self.properties = snapshot.properties;
        self.new_records = snapshot.new_records;
        self.context = snapshot.context;
        self.refs = snapshot.refs;
        self.patches = snapshot.patches;
    }

    /// Forget everything, protections and the cancellation flag included (end of transaction).
    pub(crate) fn reset(&mut self) {
        self.clear();
        self.protected.clear();
        self.cancelled.store(false, Ordering::SeqCst);
    }

    pub(crate) fn has_protections(&self) -> bool {
        !self.protected.is_empty()
    }
}

/// The cache key of a context-dependent field in `env` (ENV §4.5). The `company`
/// key is the validated current company: a refused `allowed_company_ids` is an
/// error, never the "no company" key.
fn cache_key(tx: &Transaction, env: &Env, keys: &[&str], sudo: bool) -> Result<Vec<Value>> {
    let ctx = env.context();
    keys.iter()
        .map(|k| {
            Ok(match *k {
                "company" => tx
                    .companies_now(env)?
                    .first()
                    .map_or(Value::False, |company| Value::Int(company.get())),
                "company_bound" => crate::related_company::cache_key_part(tx, env)?,
                "uid" if sudo => Value::Int(env.uid()),
                "uid" => Value::list([Value::Int(env.uid()), Value::Bool(env.su())]),
                "lang" => ctx.lang().map_or(Value::False, Value::str),
                "active_test" => Value::Bool(ctx.active_test()),
                other => ctx.get(other).unwrap_or(Value::False),
            })
        })
        .collect()
}

/// Where the values of one field live for one environment: the columnar cache, or the context
/// cache under a key computed once per operation.
#[derive(Debug)]
pub(crate) enum Slot {
    Plain,
    Context(Vec<Value>),
}

fn dedup(ids: impl IntoIterator<Item = RecId>) -> Vec<RecId> {
    let mut seen = FxHashSet::default();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

impl Transaction {
    // ---- cancellation ------------------------------------------------------------------------

    /// A guard for a scope that changes engine state; disarm it when the scope ends.
    pub(crate) fn guard(&self) -> ScopeGuard {
        ScopeGuard {
            flag: Arc::clone(&self.engine.cancelled),
            armed: true,
        }
    }

    /// `Programming` error when an earlier operation was cancelled half-way.
    pub(crate) fn check_not_cancelled(&self) -> Result<()> {
        if self.engine.cancelled.load(Ordering::SeqCst) {
            return Err(Error::programming(
                "the transaction can no longer be used: an ORM operation was cancelled (its \
                 future was dropped) while it was changing the transaction's state; roll back",
            ));
        }
        Ok(())
    }

    // ---- protection -------------------------------------------------------------------------

    pub(crate) fn is_protected(&self, model: ModelId, field: FieldId, id: RecId) -> bool {
        self.engine
            .protected
            .iter()
            .any(|p| p.model == model && p.fields.contains(&field) && p.ids.contains(&id))
    }

    /// Push a protection scope; returns the depth to restore with [`Self::unprotect_to`].
    pub(crate) fn protect(&mut self, model: ModelId, fields: &[FieldId], ids: &[RecId]) -> usize {
        let depth = self.engine.protected.len();
        if !fields.is_empty() && !ids.is_empty() {
            self.engine.protected.push(Protection {
                model,
                fields: fields.to_vec(),
                ids: ids.iter().copied().collect(),
            });
        }
        depth
    }

    pub(crate) fn unprotect_to(&mut self, depth: usize) {
        self.engine.protected.truncate(depth);
    }

    /// Run `work` with `fields` of `records` protected from recomputation and invalidation
    /// (`env.protecting(fields, records)`). Scopes nest. Cancelling the returned
    /// future before it completes poisons the transaction.
    pub async fn protecting<T, F>(
        &mut self,
        fields: &[&str],
        records: &Records,
        work: F,
    ) -> Result<T>
    where
        F: AsyncFnOnce(&mut Transaction) -> Result<T>,
    {
        self.admit_records(records).await?;
        let spec = records.spec();
        let fids = fields
            .iter()
            .map(|n| {
                spec.field_id(n).ok_or_else(|| {
                    Error::programming(format!("Invalid field '{n}' on '{}'", spec.name))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let guard = self.guard();
        let depth = self.protect(records.model_id(), &fids, records.rec_ids());
        let result = work(self).await;
        self.unprotect_to(depth);
        guard.disarm();
        result
    }

    // ---- reading ----------------------------------------------------------------------------

    /// Where `field`'s values live as seen from `env`.
    pub(crate) fn slot(&self, env: &Env, model: ModelId, field: FieldId) -> Result<Slot> {
        Ok(match self.shared.graph.model(model).compute(field) {
            Some(fc) if !fc.stored && !fc.depends_context.is_empty() => {
                Slot::Context(cache_key(self, env, &fc.depends_context, fc.sudo)?)
            }
            _ => Slot::Plain,
        })
    }

    fn slot_get(&self, slot: &Slot, model: ModelId, field: FieldId, id: RecId) -> Option<Cell> {
        match slot {
            Slot::Plain => self.cache.get(model, field, id),
            Slot::Context(key) => self
                .engine
                .context
                .values
                .get(&(model, field))?
                .get(key)?
                .get(&id)
                .cloned(),
        }
    }

    /// Store values of `field` (not dirty).
    pub(crate) fn slot_set(
        &mut self,
        slot: &Slot,
        model: ModelId,
        field: FieldId,
        values: impl IntoIterator<Item = (RecId, Cell)>,
    ) -> Result<()> {
        match slot {
            Slot::Plain => {
                let storage = self
                    .shared
                    .registry
                    .model_by_id(model)
                    .field_by_id(field)
                    .storage();
                let mc = self.cache.model_mut(model);
                for (id, cell) in values {
                    mc.set(field, storage, id, cell, false)?;
                }
            }
            Slot::Context(key) => {
                let per_id = self
                    .engine
                    .context_mut()
                    .values
                    .entry((model, field))
                    .or_default()
                    .entry(key.clone())
                    .or_default();
                per_id.extend(values);
            }
        }
        Ok(())
    }

    /// The cached cells of `field` for `ids` as seen from `env` (NULL when not cached).
    pub(crate) fn cells_of(
        &self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<Vec<Cell>> {
        let spec = self.shared.registry.model_by_id(model);
        let fs = spec.field_by_id(field);
        if fs.translate {
            if let Some(active) = &self.active_langs {
                crate::lang::validate_lang(env.lang(), active.codes())?;
            } else if let Some(code) = env.lang().filter(|c| !c.is_empty() && *c != "en_US") {
                return Err(erp_fields::Error::programming(format!(
                    "{}.{}: translated field read in language {code} before the active \
                     languages were loaded (call Transaction::active_languages first)",
                    spec.name, fs.name
                )));
            }
        }
        let slot = self.slot(env, model, field)?;
        ids.iter()
            .map(|id| {
                let cell = self
                    .slot_get(&slot, model, field, *id)
                    .unwrap_or(Cell::Null);
                if fs.company_dependent {
                    let (company, fallback) = self.property_context(env, model, field)?;
                    crate::company_dependent::project(&fs.ty, &cell, company, &fallback)
                } else if fs.translate {
                    crate::translated::project(spec.name, fs.name, &cell, env.lang())
                } else {
                    Ok(cell)
                }
            })
            .collect::<Result<Vec<_>>>()
    }

    /// Recompute the pending stored fields `field` depends on (on any model, in rank order), and
    /// `field` itself: their changes mark `field` before it is read (marks dependents
    /// of a stored field only when its value changes).
    pub(crate) async fn settle(&mut self, env: &Env, model: ModelId, field: FieldId) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        for &(m, u) in shared.graph.upstream((model, field)) {
            let stored = shared.graph.compute((m, u)).is_some_and(|fc| fc.stored);
            if stored && self.cache.model(m).is_some_and(|mc| mc.has_pending(u)) {
                self.recompute_field(env, m, u, true).await?;
            }
        }
        Ok(())
    }

    /// The records whose upstream a read of `records` settles: the records, then their prefetch
    /// set, up to one recompute batch (or all of the records when they are more).
    fn settle_scope(&self, records: &Records) -> Vec<RecId> {
        let limit = self.config().recompute_batch.max(1);
        let mut seen: FxHashSet<RecId> = FxHashSet::default();
        let mut scope: Vec<RecId> = records
            .rec_ids()
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect();
        for id in records.prefetch_ids() {
            if scope.len() >= limit {
                break;
            }
            if seen.insert(*id) {
                scope.push(*id);
            }
        }
        scope
    }

    /// Settle what reading `field` for `ids` needs. While the pending stored fields upstream of
    /// `field` hold one recompute batch or less, they are recomputed whole ([`Self::settle`]:
    /// the same statements as computing a part of them). Beyond, only the records that reach
    /// `ids` are: the inputs of `field` for `ids` are followed forward (same-model dependencies,
    /// then each path step by step), every computed input is settled for the records reached,
    /// and pending stored fields are computed for those records only; the rest stays pending for
    /// the next flush or read. A recursive field is settled whole (its levels need the whole
    /// hierarchy), as is the computed inverse of a one2many read on the way.
    pub(crate) async fn settle_records(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: Vec<RecId>,
    ) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let pending: usize = shared
            .graph
            .upstream((model, field))
            .iter()
            .filter(|k| shared.graph.compute(**k).is_some_and(|fc| fc.stored))
            .map(|(m, u)| self.cache.model(*m).map_or(0, |mc| mc.pending_count(*u)))
            .sum();
        if pending == 0 {
            return Ok(());
        }
        if pending <= self.config().recompute_batch.max(1) {
            return self.settle(env, model, field).await;
        }
        let mut visited: FxHashSet<(FieldKey, RecId)> = FxHashSet::default();
        self.settle_reaching(env, (model, field), ids, &mut visited)
            .await
    }

    /// [`Self::settle_records`] beyond one batch: settle `key` for `ids` (each `(field, record)`
    /// pair once).
    fn settle_reaching<'a>(
        &'a mut self,
        env: &'a Env,
        key: FieldKey,
        ids: Vec<RecId>,
        visited: &'a mut FxHashSet<(FieldKey, RecId)>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.check_not_cancelled()?;
            let ids: Vec<RecId> = ids
                .into_iter()
                .filter(|id| visited.insert((key, *id)))
                .collect();
            if ids.is_empty() {
                return Ok(());
            }
            let shared = Arc::clone(&self.shared);
            let (model, field) = key;
            let spec = shared.registry.model_by_id(model);
            // a one2many is read through its inverse: a computed inverse is settled whole
            if let erp_fields::FieldType::One2many { comodel, inverse } =
                &spec.field_by_id(field).ty
                && let Some(co) = shared.registry.model(comodel)
                && let Some(inv) = co.field_id(inverse)
                && shared.graph.compute((co.id, inv)).is_some()
            {
                self.settle(env, co.id, inv).await?;
            }
            let Some(fc) = shared.graph.compute(key).cloned() else {
                return Ok(());
            };
            if fc.recursive {
                return self.settle(env, model, field).await;
            }
            let mut same: Vec<FieldId> = fc.depends.clone();
            for r in &fc.readable {
                if !fc.group.contains(r) && !same.contains(r) {
                    same.push(*r);
                }
            }
            for d in same {
                self.settle_reaching(env, (model, d), ids.clone(), visited)
                    .await?;
            }
            let su = env.engine_sudo();
            for path in fc.input_paths() {
                let mut reached = ids.clone();
                for (i, step) in path.iter().enumerate() {
                    if reached.is_empty() {
                        break;
                    }
                    self.settle_reaching(env, *step, reached.clone(), visited)
                        .await?;
                    if i + 1 == path.len() {
                        break;
                    }
                    let cells = self.peek_cells(&su, step.0, step.1, &reached).await?;
                    reached = dedup(cells.iter().flat_map(crate::relational::ids_of_cell));
                }
            }
            if fc.stored {
                let todo: Vec<RecId> = ids
                    .iter()
                    .copied()
                    .filter(|id| {
                        self.cache
                            .model(model)
                            .is_some_and(|m| m.is_pending(field, *id))
                    })
                    .collect();
                if !todo.is_empty() {
                    self.compute_group(env, model, field, todo).await?;
                }
            }
            Ok(())
        })
    }

    /// Make the values of `field` available for every record of `records` (loading, computing or
    /// taking defaults as the field requires; ENV §4.8, §4.9, COMP §4.5).
    pub(crate) fn ensure<'a>(
        &'a mut self,
        records: &'a Records,
        field: FieldId,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.check_not_cancelled()?;
            let shared = Arc::clone(&self.shared);
            let model = records.model_id();
            let spec = shared.registry.model_by_id(model);
            let fs = spec.field_by_id(field);
            if fs.company_dependent && !records.is_empty() {
                self.prepare_properties(records.env(), spec).await?;
            }
            crate::database_generated::check_persisted(spec, fs, records.rec_ids())?;
            if fs.is_generated() && fs.inherited.is_none() && !records.is_empty() {
                Box::pin(self.flush_fields(model, &[field])).await?;
            }
            if matches!(fs.ty, erp_fields::FieldType::Id)
                || shared.graph.model(model).is_builtin_display_name(field)
                || fs.inherited.is_some()
            {
                // inherited fields are read on their parent (`crate::inherits`)
                return Ok(());
            }
            if fs.ty.is_x2many() && fs.compute.is_none() && fs.related.is_none() {
                return self.ensure_x2many(records, field).await;
            }
            let env = records.env().clone();
            let graph = shared.graph.model(model);
            match graph.compute(field).cloned() {
                Some(fc) if fc.stored => {
                    let scope = self.settle_scope(records);
                    self.settle_records(&env, model, field, scope).await?;
                    self.load(records, field).await?;
                    let new_missing: Vec<RecId> =
                        dedup(
                            records.rec_ids().iter().copied().filter(|id| {
                                id.is_new() && !self.cache.contains(model, field, *id)
                            }),
                        );
                    if new_missing.is_empty() {
                        return Ok(());
                    }
                    let (from_origin, compute): (Vec<RecId>, Vec<RecId>) = new_missing
                        .into_iter()
                        .partition(|id| fc.editable && id.origin().is_some());
                    self.copy_from_origin(&env, model, field, &from_origin)
                        .await?;
                    // A fresh stored compute may read its own old value (for example
                    // partner language). As for nonstored fields below, an uncached
                    // protected read is null, not a recursive call to the same compute.
                    let (protected, compute): (Vec<RecId>, Vec<RecId>) = compute
                        .into_iter()
                        .partition(|id| self.is_protected(model, field, *id));
                    self.slot_set(
                        &Slot::Plain,
                        model,
                        field,
                        protected.into_iter().map(|id| (id, Cell::Null)),
                    )?;
                    self.compute_group(&env, model, field, compute).await?;
                }
                Some(_) => {
                    let scope = self.settle_scope(records);
                    self.settle_records(&env, model, field, scope.clone())
                        .await?;
                    let slot = self.slot(&env, model, field)?;
                    let missing: Vec<RecId> = dedup(
                        records
                            .rec_ids()
                            .iter()
                            .copied()
                            .filter(|id| self.slot_get(&slot, model, field, *id).is_none()),
                    );
                    if missing.is_empty() {
                        return Ok(());
                    }
                    // read while being computed: the null value (COMP §4.5)
                    let (protected, missing): (Vec<RecId>, Vec<RecId>) = missing
                        .into_iter()
                        .partition(|id| self.is_protected(model, field, *id));
                    self.slot_set(
                        &slot,
                        model,
                        field,
                        protected.into_iter().map(|id| (id, Cell::Null)),
                    )?;
                    if missing.is_empty() {
                        return Ok(());
                    }
                    // the records plus the uncached part of their prefetch set (as far as it
                    // was settled), up to one batch
                    let asked = missing.clone();
                    let mut batch = missing;
                    let limit = self.config().recompute_batch.max(1).max(batch.len());
                    let mut seen: FxHashSet<RecId> = batch.iter().copied().collect();
                    for id in &scope {
                        if batch.len() >= limit {
                            break;
                        }
                        if !seen.contains(id)
                            && self.slot_get(&slot, model, field, *id).is_none()
                            && !self.is_protected(model, field, *id)
                        {
                            seen.insert(*id);
                            batch.push(*id);
                        }
                    }
                    if batch.len() > asked.len() {
                        batch = self
                            .without_foreign_siblings(&env, model, field, batch, &asked)
                            .await?;
                    }
                    if batch.len() > asked.len() {
                        match self.compute_group(&env, model, field, batch.clone()).await {
                            Ok(()) => {}
                            // A missing asked record raises
                            // again from the retry.
                            Err(err) if err.kind() == erp_fields::ErrorKind::Missing => {
                                self.compute_group(&env, model, field, asked).await?;
                            }
                            Err(err) => return Err(err),
                        }
                    } else {
                        self.compute_group(&env, model, field, batch).await?;
                    }
                }
                None if fs.has_column() => {
                    self.load(records, field).await?;
                    let new_missing: Vec<RecId> =
                        dedup(
                            records.rec_ids().iter().copied().filter(|id| {
                                id.is_new() && !self.cache.contains(model, field, *id)
                            }),
                        );
                    let (mut from_origin, defaults): (Vec<RecId>, Vec<RecId>) = new_missing
                        .into_iter()
                        .partition(|id| id.origin().is_some());
                    if !from_origin.is_empty() {
                        // the other new records of the prefetch set read their origins in the
                        // same query
                        let mut seen: FxHashSet<RecId> = from_origin.iter().copied().collect();
                        for id in records.prefetch_ids() {
                            if id.origin().is_some()
                                && !self.cache.contains(model, field, *id)
                                && seen.insert(*id)
                            {
                                from_origin.push(*id);
                            }
                        }
                    }
                    self.copy_from_origin(&env, model, field, &from_origin)
                        .await?;
                    self.fill_defaults(records, field, &defaults).await?;
                }
                None => {
                    // cache-only field: the default value on a miss (COMP §4.5)
                    let missing: Vec<RecId> = dedup(
                        records
                            .rec_ids()
                            .iter()
                            .copied()
                            .filter(|id| !self.cache.contains(model, field, *id)),
                    );
                    self.fill_defaults(records, field, &missing).await?;
                }
            }
            Ok(())
        })
    }

    /// The values of `field` for `records`, as an input of a compute: what reading the field
    /// returns (x2many values in record format, fields of `_inherits` parents through the
    /// delegate).
    fn input_cells<'a>(
        &'a mut self,
        records: &'a Records,
        field: FieldId,
    ) -> BoxFuture<'a, Result<Vec<Cell>>> {
        // Input reads can compute other inputs. Keep their field-read future off
        // each nested native compute's polling frame without changing lazy reads.
        Box::pin(self.field_cells(records, field))
    }

    async fn fill_defaults(
        &mut self,
        records: &Records,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let fs = spec.field_by_id(field);
        let cell = crate::write::default_cell(records, spec, fs)?.unwrap_or(Cell::Null);
        // a default (static or a `default_<field>` context key from the caller) reaches the cache
        // like an assigned value, so HTML goes through the field's policy first (review r1 M3)
        let cell = self.admit_html_cell(spec, fs, cell)?;
        if matches!(fs.ty, erp_fields::FieldType::Monetary { .. }) {
            // A default fill (also reached by reads) is bounded like any monetary operation:
            // refuse before the compact staging buffer is allocated.
            let budget = crate::monetary::StagingBudget::new(ids.len());
            budget.check(ids.len())?;
            let shared = Arc::clone(&self.shared);
            let slots = crate::monetary::staging_slots(
                spec,
                shared.graph.model(records.model_id()),
                &[field],
            );
            let slot = slots
                .binary_search(&field)
                .map_err(|_| Error::programming("Monetary default staging has no slot"))?;
            let mut rows = vec![vec![None; slots.len()]; ids.len()];
            for row in &mut rows {
                row[slot] = Some(cell.clone());
            }
            crate::monetary::normalize(
                self,
                records.env(),
                crate::monetary::Stage {
                    initial: false,
                    spec,
                    ids: Some(ids),
                    fields: &slots,
                    rows: &mut rows,
                    included: None,
                    budget: &budget,
                    precharged: None,
                    kind: crate::monetary::StageKind::Direct,
                },
                &[field],
                false,
                true,
            )
            .await?;
            let cells =
                ids.iter()
                    .enumerate()
                    .map(|(i, id)| {
                        rows[i][slot].take().map(|cell| (*id, cell)).ok_or_else(|| {
                            Error::programming("Monetary default staging lost an input")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
            return self.slot_set(&Slot::Plain, records.model_id(), field, cells);
        }
        self.slot_set(
            &Slot::Plain,
            records.model_id(),
            field,
            ids.iter().map(|id| (*id, cell.clone())),
        )
    }

    /// New records with an origin take a stored field's value from their origin (ENV §4.9).
    async fn copy_from_origin(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let origins: Vec<RecId> = dedup(ids.iter().filter_map(|id| id.origin()).map(RecId::real));
        let origin_records = Records::from_parts(
            env.clone(),
            model,
            Arc::from(origins.clone()),
            Arc::from(origins),
        );
        self.load(&origin_records, field).await?;
        let values: Vec<(RecId, Cell)> = ids
            .iter()
            .filter_map(|id| {
                let origin = id.origin()?;
                let cell = self
                    .cache
                    .get(model, field, RecId::real(origin))
                    .unwrap_or(Cell::Null);
                Some((*id, cell))
            })
            .collect();
        self.slot_set(&Slot::Plain, model, field, values)
    }

    // ---- computing --------------------------------------------------------------------------

    /// Compute the pending records of stored field `field` (all of them, or only database records
    /// when `include_new` is false: a flush never computes new records).
    ///
    /// The pending set is taken as it is now: records marked while computing it (by computes that
    /// create or write records) are left for the next flush round, which bounds the work. A
    /// recursive field is the exception: its records are computed level by
    /// level — the records whose own inputs through the recursion are not pending first — until
    /// none is pending, so a deep hierarchy settles in one flush.
    pub(crate) async fn recompute_field(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        include_new: bool,
    ) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let recursive = shared
            .graph
            .model(model)
            .compute(field)
            .is_some_and(|fc| fc.recursive);
        let batch = self.config().recompute_batch.max(1);
        let pending = |tx: &Self| -> Vec<RecId> {
            tx.cache
                .model(model)
                .map(|m| m.pending_ids(field))
                .unwrap_or_default()
                .into_iter()
                .filter(|id| include_new || !id.is_new())
                .collect()
        };
        let mut passes: usize = 0;
        loop {
            let snapshot = pending(self);
            if snapshot.is_empty() {
                return Ok(());
            }
            let todo = if recursive {
                self.recursion_ready(env, model, field, &snapshot).await?
            } else {
                snapshot
            };
            for chunk in todo.chunks(batch) {
                // a compute of an earlier chunk may have computed some of these already
                let ids: Vec<RecId> = chunk
                    .iter()
                    .copied()
                    .filter(|id| {
                        self.cache
                            .model(model)
                            .is_some_and(|m| m.is_pending(field, *id))
                    })
                    .collect();
                self.compute_batch(env, model, field, ids).await?;
            }
            if !recursive {
                return Ok(());
            }
            passes += 1;
            if passes > RECURSION_PASSES {
                let spec = shared.registry.model_by_id(model);
                return Err(Error::programming(format!(
                    "the recursive field {}.{} was still being recomputed after {RECURSION_PASSES} \
                     levels (a cycle in the records' hierarchy whose values keep changing?)",
                    spec.name,
                    spec.field_by_id(field).name
                )));
            }
        }
    }

    /// Among `ids` (records of a recursive field to compute), those whose inputs through the
    /// recursion are not among `ids`: they can be computed first (one level). The recursion is
    /// every dependency path that comes back to the field (`parent_id.f`, `child_ids.f`, or
    /// several hops such as `partner_id.parent_id.f` and `_inherits` prefixes): its steps before
    /// the field are followed from each record, one read per step for all records. When every
    /// record waits for another one (a cycle in the data), the first one is computed alone, so
    /// each level makes progress and a cycle is broken one record at a time.
    async fn recursion_ready(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<Vec<RecId>> {
        let shared = Arc::clone(&self.shared);
        let Some(fc) = shared.graph.model(model).compute(field) else {
            return Ok(ids.to_vec());
        };
        let target: FieldKey = (model, field);
        let mut chains: Vec<Vec<FieldKey>> = Vec::new();
        for path in &fc.paths {
            if let Some(k) = path.iter().skip(1).position(|s| *s == target) {
                let chain = path[..=k].to_vec();
                if !chains.contains(&chain) {
                    chains.push(chain);
                }
            }
        }
        if chains.is_empty() || ids.len() < 2 {
            return Ok(ids.to_vec());
        }
        let todo: FxHashSet<RecId> = ids.iter().copied().collect();
        let mut blocked: FxHashSet<RecId> = FxHashSet::default();
        for chain in chains {
            // per record, the records its chain reaches so far
            let mut reach: Vec<(RecId, Vec<RecId>)> =
                ids.iter().map(|id| (*id, vec![*id])).collect();
            for step in &chain {
                let all: Vec<RecId> = dedup(reach.iter().flat_map(|(_, r)| r.iter().copied()));
                if all.is_empty() {
                    break;
                }
                let cells = self.peek_cells(env, step.0, step.1, &all).await?;
                let next: FxHashMap<RecId, Vec<RecId>> = all
                    .iter()
                    .copied()
                    .zip(cells.iter().map(crate::relational::ids_of_cell))
                    .collect();
                for (_, r) in &mut reach {
                    *r = dedup(
                        r.iter()
                            .flat_map(|x| next.get(x).map(Vec::as_slice).unwrap_or_default())
                            .copied(),
                    );
                }
            }
            for (id, r) in &reach {
                if r.iter().any(|t| t != id && todo.contains(t)) {
                    blocked.insert(*id);
                }
            }
        }
        let ready: Vec<RecId> = ids
            .iter()
            .copied()
            .filter(|id| !blocked.contains(id))
            .collect();
        Ok(if ready.is_empty() {
            ids.iter().take(1).copied().collect()
        } else {
            ready
        })
    }

    /// Run the compute of `field`'s group for `ids`, in batches of at most
    /// [`crate::OrmConfig::recompute_batch`] records; a recursive field level by
    /// level.
    pub(crate) async fn compute_group(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: Vec<RecId>,
    ) -> Result<()> {
        let mut remaining = dedup(ids);
        let batch = self.config().recompute_batch.max(1);
        let recursive = self
            .shared
            .graph
            .model(model)
            .compute(field)
            .is_some_and(|fc| fc.recursive);
        while !remaining.is_empty() {
            let level = if recursive {
                self.recursion_ready(env, model, field, &remaining).await?
            } else {
                std::mem::take(&mut remaining)
            };
            let done: FxHashSet<RecId> = level.iter().copied().collect();
            remaining.retain(|id| !done.contains(id));
            for chunk in level.chunks(batch) {
                self.compute_batch(env, model, field, chunk.to_vec())
                    .await?;
            }
        }
        Ok(())
    }

    /// Run the compute of `field`'s group for one batch and apply the values (COMP §4.5).
    async fn compute_batch(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: Vec<RecId>,
    ) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let graph = shared.graph.model(model);
        let spec = shared.registry.model_by_id(model);
        let Some(fc) = graph.compute(field).cloned() else {
            return Ok(());
        };
        let ids = dedup(ids);
        if ids.is_empty() {
            return Ok(());
        }
        // Pull attribution from this pending batch, independently of compute execution authority.
        let previous_origin = self.pending_capture.active.clone();
        let relevant = fc.stored && fc.group.iter().any(|f| self.capture_relevant(model, *f));
        let resolved = (|| -> Result<Option<Arc<crate::outbox::CaptureContext>>> {
            let mut origin = None;
            if relevant {
                for id in ids.iter().filter(|id| !id.is_new()) {
                    // Only the fields whose dependency changed need be pending; all outputs of
                    // this compute group inherit that same row's immutable origin.
                    let mut row_origin = None;
                    for f in fc.group.iter() {
                        if let Some(bound) = self.pending_capture.origin(model, *f, *id) {
                            if row_origin
                                .as_ref()
                                .is_some_and(|old| !Arc::ptr_eq(old, &bound))
                            {
                                return Err(Error::programming(
                                    "outbox: recompute batch contains unsettled distinct operations",
                                ));
                            }
                            row_origin = Some(bound);
                        }
                    }
                    let bound = row_origin.ok_or_else(|| {
                        Error::programming(
                            "outbox: event-relevant recompute is missing writer provenance",
                        )
                    })?;
                    if origin.as_ref().is_some_and(|old| !Arc::ptr_eq(old, &bound)) {
                        return Err(Error::programming(
                            "outbox: recompute batch contains unsettled distinct operations",
                        ));
                    }
                    origin = Some(bound);
                }
            }
            Ok(origin)
        })();
        let origin = match resolved {
            Ok(origin) => origin,
            Err(error) => {
                self.poison_with(&error);
                return Err(error);
            }
        };
        if let Some(origin) = origin {
            self.pending_capture.active = Some(origin);
        }
        let run_env = if fc.sudo {
            env.engine_sudo()
        } else {
            env.clone()
        };
        let ids_arc: Arc<[RecId]> = Arc::from(ids.clone());
        let records = Records::from_parts(run_env, model, Arc::clone(&ids_arc), ids_arc);
        // Marked done first, so reading the old value inside does not recompute (COMP §4.5).
        if fc.stored {
            let mc = self.cache.model_mut(model);
            for f in fc.group.iter() {
                mc.clear_pending(*f, &ids);
            }
        }
        let guard = self.guard();
        let depth = self.protect(model, &fc.group, &ids);
        // A compute may read another computed input through ensure. Keep this
        // recursive polling boundary boxed so each native compute does not carry
        // the complete input-read future in its caller's stack frame.
        let result = Box::pin(self.run_compute(&records, &fc, env)).await;
        self.unprotect_to(depth);
        let result = match result {
            Ok(out) => self.apply_outputs(env, model, spec, &fc, &ids, out).await,
            Err(error) => Err(error),
        };
        self.pending_capture.active = previous_origin;
        guard.disarm();
        if let Err(error) = result {
            if fc.stored {
                for f in fc.group.iter() {
                    let storage = spec.field_by_id(*f).storage();
                    self.cache.model_mut(model).mark_pending(*f, storage, &ids);
                    if ids.iter().any(|id| !id.is_new()) {
                        self.note_database_change(model, &[*f]);
                    }
                }
            }
            if relevant {
                self.poison_with(&error);
            } else {
                self.poison_on_validation(&error);
            }
            return Err(error);
        }
        Ok(())
    }

    /// Publish the result of a candidate compute that same-call monetary admission already ran
    /// : the group's values go to the cache as the compute's own recompute would put
    /// them, and the recomputation the mutation scheduled is cancelled, so the candidate runs
    /// once per accepted mutation.
    pub(crate) async fn publish_candidate(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
        out: Outputs,
    ) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let spec = shared.registry.model_by_id(model);
        let Some(fc) = shared.graph.model(model).compute(field) else {
            return Err(Error::programming(
                "a staged candidate result has no compute registration",
            ));
        };
        if fc.stored {
            let mc = self.cache.model_mut(model);
            for f in fc.group.iter() {
                mc.clear_pending(*f, ids);
            }
        }
        if let Err(error) = self.apply_outputs(env, model, spec, fc, ids, out).await {
            // as `compute_batch` does: the group is stale again and the transaction refuses commit
            if fc.stored {
                for f in fc.group.iter() {
                    let storage = spec.field_by_id(*f).storage();
                    self.cache.model_mut(model).mark_pending(*f, storage, ids);
                    if ids.iter().any(|id| !id.is_new()) {
                        self.note_database_change(model, &[*f]);
                    }
                }
            }
            self.poison_on_validation(&error);
            return Err(error);
        }
        Ok(())
    }

    /// Put computed values in the cache, notify the dependents of what changed and run the
    /// constraints of stored fields. A stored many2one keeps the cached one2many values of its old
    /// and new targets coherent, as an assignment does (REL §4.4.4).
    async fn apply_outputs(
        &mut self,
        env: &Env,
        model: ModelId,
        spec: &ModelSpec,
        fc: &Arc<FieldCompute>,
        ids: &[RecId],
        mut out: Outputs,
    ) -> Result<()> {
        out.validate_virtual(self, ids)?;
        out.admit_html(self)?;
        // Validate the whole computed group before publishing any of its outputs.
        for fid in fc.group.iter() {
            let field = spec.field_by_id(*fid);
            if field.store && crate::precision::usage(field).is_some() {
                for cell in out.column(*fid).iter().flatten() {
                    crate::precision::validate(self, env, spec, field, cell).await?;
                }
            }
        }
        let shared = Arc::clone(&self.shared);
        let mut changes: Vec<(FieldId, Vec<RecId>)> = Vec::new();
        let monetary = fc.group.iter().any(|f| {
            matches!(
                spec.field_by_id(*f).ty,
                erp_fields::FieldType::Monetary { .. }
            )
        });
        let mut staged = Vec::new();
        let mut slots: Vec<FieldId> = Vec::new();
        if monetary {
            let budget = crate::monetary::StagingBudget::new(ids.len());
            let monetary_group: Vec<FieldId> = fc
                .group
                .iter()
                .copied()
                .filter(|f| {
                    matches!(
                        spec.field_by_id(*f).ty,
                        erp_fields::FieldType::Monetary { .. }
                    )
                })
                .collect();
            slots =
                crate::monetary::staging_slots(spec, shared.graph.model(model), &monetary_group);
            // Only the group members the slot list holds are staged; the others stay in `out`.
            // The cells actually assigned are counted before the staging buffer is allocated.
            let mut columns: Vec<(usize, Vec<Option<Cell>>)> = Vec::new();
            let mut cells = 0usize;
            for &f in fc.group.iter() {
                if let Ok(slot) = slots.binary_search(&f) {
                    let column = out.take(f);
                    if monetary_group.contains(&f) {
                        cells = cells.saturating_add(column.iter().filter(|c| c.is_some()).count());
                    }
                    columns.push((slot, column));
                }
            }
            budget.check(cells)?;
            staged = vec![vec![None; slots.len()]; ids.len()];
            for (slot, column) in columns {
                for (i, cell) in column.into_iter().enumerate().take(ids.len()) {
                    staged[i][slot] = cell;
                }
            }
            let run_env = if fc.sudo {
                env.engine_sudo()
            } else {
                env.clone()
            };
            crate::monetary::normalize(
                self,
                &run_env,
                crate::monetary::Stage {
                    initial: false,
                    spec,
                    ids: Some(ids),
                    fields: &slots,
                    rows: &mut staged,
                    included: None,
                    budget: &budget,
                    precharged: None,
                    kind: crate::monetary::StageKind::Direct,
                },
                &fc.group,
                false,
                true,
            )
            .await?;
        }
        // stored many2one fields with inverse one2many fields: (field, id, old, new)
        let mut reassigned: Vec<(FieldId, RecId, Option<Cell>, Cell)> = Vec::new();
        // the old targets of a stored many2one whose one2many has dependents must be known, so
        // that the old owners are notified too (a related or other compute that did not load its
        // own old value)
        if fc.stored {
            let real: Vec<RecId> = ids.iter().copied().filter(|i| !i.is_new()).collect();
            for &f in fc.group.iter() {
                let fs = spec.field_by_id(f);
                if !real.is_empty()
                    && matches!(fs.ty, erp_fields::FieldType::Many2one { .. })
                    && self.inverse_has_dependents(model, f)
                    && real.iter().any(|id| !self.cache.contains(model, f, *id))
                {
                    let real_arc: Arc<[RecId]> = Arc::from(real.clone());
                    let records = Records::from_parts(
                        env.engine_sudo(),
                        model,
                        Arc::clone(&real_arc),
                        real_arc,
                    );
                    if let Err(error) = self.load(&records, f).await
                        && error.kind() != erp_fields::ErrorKind::Missing
                    {
                        return Err(error);
                    }
                }
            }
        }
        for &f in fc.group.iter() {
            let fs = spec.field_by_id(f);
            let values: Vec<Option<Cell>> = match slots.binary_search(&f) {
                Ok(slot) if monetary => staged.iter().map(|row| row[slot].clone()).collect(),
                _ => out.take(f),
            };
            let value = |i: usize| values.get(i).cloned().flatten();
            if fc.stored {
                let with_inverses = matches!(fs.ty, erp_fields::FieldType::Many2one { .. })
                    && !shared.relations.inverses(model, f).is_empty();
                let mc = self.cache.model_mut(model);
                let mut changed = Vec::new();
                for (i, id) in ids.iter().enumerate() {
                    let cell = match value(i) {
                        Some(cell) => cell,
                        None if !id.is_new() || mc.get(f, *id).is_some() => continue,
                        None => Cell::Null,
                    };
                    let old = mc.get(f, *id);
                    if old.as_ref().is_some_and(|old| old.same_value(&cell)) {
                        continue;
                    }
                    if with_inverses {
                        reassigned.push((f, *id, old, cell.clone()));
                    }
                    mc.set(f, fs.storage(), *id, cell, !id.is_new())?;
                    changed.push(*id);
                }
                if !changed.is_empty() {
                    // memoized record checks reading this stored value are stale now
                    self.access_written(model, Some(&[f]));
                    // a recompute that sets a database record's value is a pending write
                    if changed.iter().any(|id| !id.is_new()) {
                        self.note_database_change(model, &[f]);
                    }
                    changes.push((f, changed));
                }
            } else {
                if fs.readonly {
                    let unassigned: Vec<String> = ids
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| value(*i).is_none())
                        .map(|(_, id)| id.to_string())
                        .collect();
                    if !unassigned.is_empty() {
                        return Err(Error::programming(format!(
                            "Compute method failed to assign {}({}{}).{}",
                            spec.name,
                            unassigned.join(", "),
                            if unassigned.len() == 1 { "," } else { "" },
                            fs.name
                        )));
                    }
                }
                let slot = self.slot(env, model, f)?;
                let cells: Vec<(RecId, Cell)> = ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (*id, value(i).unwrap_or(Cell::Null)))
                    .collect();
                self.slot_set(&slot, model, f, cells)?;
            }
        }
        if !reassigned.is_empty() {
            // per (field, new value): the records and their old values
            let mut groups: Vec<Reassigned> = Vec::new();
            for (f, id, old, new) in reassigned {
                match groups.iter_mut().find(|(g, c, _, _)| *g == f && *c == new) {
                    Some((_, _, recs, olds)) => {
                        recs.push(id);
                        olds.push(old);
                    }
                    None => groups.push((f, new, vec![id], vec![old])),
                }
            }
            for (f, new, recs, olds) in groups {
                let recs = Records::from_parts(
                    env.clone(),
                    model,
                    Arc::from(recs.clone()),
                    Arc::from(recs),
                );
                self.update_inverses(model, f, &recs, &olds, &new).await?;
            }
        }
        for (field, changed) in &changes {
            self.bind_capture(model, *field, changed)?;
        }
        self.modified(model, changes).await?;
        if fc.stored {
            let real: Vec<RecId> = ids.iter().copied().filter(|id| !id.is_new()).collect();
            if !real.is_empty() {
                let names: FxHashSet<FieldId> = fc.group.iter().copied().collect();
                let records = Records::from_parts(
                    env.clone(),
                    model,
                    Arc::from(real.clone()),
                    Arc::from(real),
                );
                self.run_constraints(&records, &names, &FxHashSet::default())
                    .await?;
            }
        }
        Ok(())
    }

    /// The values a compute reads through its paths: per step after the first, the values of
    /// the records reached so far (one prefetch per step for the whole batch; x2many steps in
    /// record format, like the compute would see them).
    ///
    /// `first` holds, per field index of the records' model, values already read for the records
    /// (the first step of each path); missing ones are read.
    pub(crate) async fn path_inputs(
        &mut self,
        records: &Records,
        fc: &FieldCompute,
        first: &[Option<Vec<Cell>>],
    ) -> Result<Vec<RelColumn>> {
        let mut columns: Vec<RelColumn> = Vec::new();
        for path in fc.input_paths() {
            let mut current: Vec<RecId> = records.rec_ids().to_vec();
            for (i, step) in path.iter().enumerate().skip(1) {
                let link = path[i - 1];
                // the records reached through the previous step
                let given = (i == 1)
                    .then(|| first.get(link.1.index()).and_then(Option::as_ref))
                    .flatten();
                let cells = match given {
                    Some(cells) => cells.clone(),
                    None => {
                        self.column_cells(records.env(), link, &current, &columns)
                            .await?
                    }
                };
                let mut seen: FxHashSet<RecId> = FxHashSet::default();
                current = cells
                    .iter()
                    .flat_map(|c| match c {
                        Cell::Id(_) => crate::relational::id_of(c).into_iter().collect(),
                        Cell::Ids(ids) => ids.to_vec(),
                        _ => Vec::new(),
                    })
                    .filter(|id| seen.insert(*id))
                    .collect();
                if current.is_empty() {
                    break;
                }
                let values = self
                    .column_cells(records.env(), *step, &current, &columns)
                    .await?;
                let position = columns
                    .iter()
                    .position(|c| c.model == step.0 && c.field == step.1);
                let column = match position {
                    Some(c) => &mut columns[c],
                    None => {
                        columns.push(RelColumn::new(step.0, step.1));
                        let last = columns.len() - 1;
                        &mut columns[last]
                    }
                };
                for (id, cell) in current.iter().zip(values) {
                    column.values.insert(*id, cell);
                }
            }
        }
        Ok(columns)
    }

    /// The values of `key` for `ids`, from the columns read so far or by reading the field.
    async fn column_cells(
        &mut self,
        env: &Env,
        key: crate::graph::FieldKey,
        ids: &[RecId],
        columns: &[RelColumn],
    ) -> Result<Vec<Cell>> {
        let known = columns
            .iter()
            .find(|c| c.model == key.0 && c.field == key.1);
        if let Some(column) = known
            && ids.iter().all(|id| column.values.contains_key(id))
        {
            return Ok(ids
                .iter()
                .map(|id| column.values.get(id).cloned().unwrap_or(Cell::Null))
                .collect());
        }
        let ids_arc: Arc<[RecId]> = Arc::from(ids);
        let records = Records::from_parts(env.clone(), key.0, Arc::clone(&ids_arc), ids_arc);
        self.field_cells(&records, key.1).await
    }

    /// `reader` is the environment the field is read in: the records' own, before the
    /// `compute_sudo` elevation, whose company bound a related path applies.
    async fn run_compute(
        &mut self,
        records: &Records,
        fc: &Arc<FieldCompute>,
        reader: &Env,
    ) -> Result<Outputs> {
        let model = records.model_id();
        let env = records.env().clone();
        let ids: Arc<[RecId]> = Arc::from(records.rec_ids());
        let mut out = Outputs::new(env.clone(), model, fc.method, &fc.group, ids.len()).for_cache();
        let single = |out: &mut Outputs, cells: Vec<Cell>| -> Result<()> {
            if let Some(&f) = fc.group.first() {
                let name = records.spec().field_by_id(f).name;
                for (i, cell) in cells.into_iter().enumerate() {
                    out.assign(i, name, cell)?;
                }
            }
            Ok(())
        };
        match &fc.source {
            Source::DisplayName => {
                let cells = self.builtin_display_names_for(records, reader).await?;
                single(&mut out, cells)?;
                Ok(out)
            }
            Source::Related(target) => {
                let bound = fc
                    .group
                    .first()
                    .and_then(|f| Bound::of(reader, model, records.spec().field_by_id(*f)));
                let cells = if bound.is_some() {
                    // the target may itself be company-bound, and may point to records of a
                    // company-scoped model: a path of one step, read with the same bound
                    // (review RD3)
                    let current: Vec<Option<RecId>> = ids.iter().copied().map(Some).collect();
                    let direct = reader.is_direct();
                    self.related_from_as(
                        &env,
                        &[(model, *target)],
                        current.clone(),
                        &current,
                        direct,
                        bound,
                    )
                    .await?
                } else {
                    self.input_cells(records, *target).await?
                };
                single(&mut out, cells)?;
                Ok(out)
            }
            Source::RelatedPath(path) => {
                let bound = fc
                    .group
                    .first()
                    .and_then(|f| Bound::of(reader, model, records.spec().field_by_id(*f)));
                let cells = self.related_values(records, path, bound).await?;
                single(&mut out, cells)?;
                Ok(out)
            }
            Source::Chain(chain) if fc.is_async => {
                self.load_compute_lines(records, fc).await?;
                // the inputs first: an own stored input loads its prefetch group, which may bring
                // the group's old values along
                for &r in &fc.readable {
                    if !fc.group.contains(&r)
                        && !self.shared.graph.model(model).is_builtin_display_name(r)
                    {
                        self.input_cells(records, r).await?;
                    }
                }
                // the old values, so unchanged results notify nobody: whatever the
                // inputs did not bring, in one read for the group
                self.load_compute_old_values(records, fc).await?;
                for &r in &fc.readable {
                    if fc.group.contains(&r) {
                        // loaded above, this finds them cached
                        self.load_own(records, r).await?;
                    }
                }
                // prefetched for the batch, so the compute's own reads hit the cache
                self.path_inputs(records, fc, &[]).await?;
                if fc.candidate {
                    let scope =
                        crate::candidate::ReadScope::stored(model, Arc::clone(&ids), &fc.readable);
                    run_candidate(chain, self, records, &mut out, scope).await?;
                } else {
                    run_async(Arc::clone(chain), 0, self, records, &mut out).await?;
                }
                Ok(out)
            }
            Source::Chain(chain) => {
                let n = records.spec().fields.len();
                let mut inputs: Vec<Option<Vec<Cell>>> = (0..n).map(|_| None).collect();
                self.load_compute_lines(records, fc).await?;
                // the inputs first: an own stored input loads its prefetch group, which may bring
                // the group's old values along
                for &r in &fc.readable {
                    if !fc.group.contains(&r) {
                        inputs[r.index()] = Some(self.input_cells(records, r).await?);
                    }
                }
                // the group's own old values: loaded, never computed (COMP §4.5); whatever the
                // inputs did not bring, in one read for the group
                self.load_compute_old_values(records, fc).await?;
                for &r in &fc.readable {
                    if fc.group.contains(&r) {
                        // loaded above, this finds it cached
                        self.load_own(records, r).await?;
                        if records.spec().field_by_id(r).translate {
                            self.active_languages().await?;
                        }
                        inputs[r.index()] = Some(self.cells_of(&env, model, r, &ids)?);
                    }
                }
                let related = self.path_inputs(records, fc, &inputs).await?;
                let mut data = BatchData::new(ids, inputs, out).with_related(related);
                run_sync(chain, 0, &mut data)?;
                Ok(data.out)
            }
        }
    }

    /// The old values of the group fields the compute reads, in one statement for the whole group
    /// (audit W12: not the prefetch group; one read, not one per field).
    pub(crate) async fn load_compute_old_values(
        &mut self,
        records: &Records,
        fc: &FieldCompute,
    ) -> Result<()> {
        let group: Vec<FieldId> = fc
            .group
            .iter()
            .copied()
            .filter(|f| fc.readable.contains(f))
            .collect();
        self.load_own_group(records, &group).await
    }

    /// The one2many fields a compute reads lines through, read with the columns its paths need
    /// on the lines and nothing more (4, perf-write option 2): the compute reads
    /// those columns next, from the cache. A one2many the compute reads without a path keeps the
    /// plain read with the prefetch columns.
    async fn load_compute_lines(&mut self, records: &Records, fc: &FieldCompute) -> Result<()> {
        for &r in &fc.readable {
            if fc.group.contains(&r)
                || !matches!(
                    records.spec().field_by_id(r).ty,
                    erp_fields::FieldType::One2many { .. }
                )
            {
                continue;
            }
            let columns = line_columns(fc, (records.model_id(), r));
            if columns.is_empty() {
                continue;
            }
            self.ensure_x2many_with(records, r, crate::relational::LineColumns::Only(&columns))
                .await?;
        }
        Ok(())
    }

    /// `batch` without the prefetch siblings that fail the company bound of the company-bound
    /// related `field` for `env`: only the records `asked` for raise, a sibling that fails is left
    /// out and stays uncached (review SC2). The path is read once for the whole batch, so the
    /// compute that follows finds it cached. Other fields keep `batch` as it is.
    async fn without_foreign_siblings(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        batch: Vec<RecId>,
        asked: &[RecId],
    ) -> Result<Vec<RecId>> {
        let shared = Arc::clone(&self.shared);
        let Some(bound) = Bound::of(
            env,
            model,
            shared.registry.model_by_id(model).field_by_id(field),
        ) else {
            return Ok(batch);
        };
        let Some(fc) = shared.graph.compute((model, field)) else {
            return Err(Error::programming(format!(
                "company-bound related field {}.{} has no compute entry",
                shared.registry.model_by_id(model).name,
                shared.registry.model_by_id(model).field_by_id(field).name
            )));
        };
        let lenient = Lenient::new(asked.iter().copied());
        let run_env = env.engine_sudo();
        let current: Vec<Option<RecId>> = batch.iter().copied().map(Some).collect();
        let bound = bound.tolerating(&lenient);
        let direct = env.is_direct();
        match &fc.source {
            Source::RelatedPath(path) => {
                self.related_from_as(
                    &run_env,
                    path,
                    current.clone(),
                    &current,
                    direct,
                    Some(bound),
                )
                .await?;
            }
            Source::Related(target) => {
                self.related_from_as(
                    &run_env,
                    &[(model, *target)],
                    current.clone(),
                    &current,
                    direct,
                    Some(bound),
                )
                .await?;
            }
            Source::Chain(_) | Source::DisplayName => {
                return Err(not_a_related_source(&shared.registry, model, field));
            }
        }
        let failed = lenient.into_failed();
        Ok(batch
            .into_iter()
            .filter(|id| !failed.contains(id))
            .collect())
    }

    /// The values of a related field through relations (COMP §4.8): the path is followed one step
    /// at a time for all records at once, taking the first record of each intermediate value.
    pub(crate) async fn related_values(
        &mut self,
        records: &Records,
        path: &[crate::graph::FieldKey],
        bound: Option<Bound<'_>>,
    ) -> Result<Vec<Cell>> {
        let current: Vec<Option<RecId>> = records.rec_ids().iter().copied().map(Some).collect();
        self.related_from(records.env(), path, current, bound).await
    }

    /// The values at the end of `path` for each start record (`None`: no value).
    pub(crate) async fn related_from(
        &mut self,
        env: &Env,
        path: &[crate::graph::FieldKey],
        current: Vec<Option<RecId>>,
        bound: Option<Bound<'_>>,
    ) -> Result<Vec<Cell>> {
        let starts = current.clone();
        let direct = env.is_direct();
        self.related_from_as(env, path, current, &starts, direct, bound)
            .await
    }

    /// [`Self::related_from`] for `current` records that stand for the `starts` the field is read
    /// on (the same positions): the records of an inner company-bound related path are reached
    /// through the outer one, so a refusal still names the record read (review SC1). `direct`:
    /// `current` are the records read on, not hops.
    ///
    /// With `bound`, every record reached passes the company bound of the reader (rule
    /// 6): the hops of the steps, the inner paths of company-bound related fields on the way
    /// (read with the same bound), and the records the last field points to when it is relational
    /// (review SC4).
    pub(crate) fn related_from_as<'a>(
        &'a mut self,
        env: &'a Env,
        path: &'a [crate::graph::FieldKey],
        current: Vec<Option<RecId>>,
        starts: &'a [Option<RecId>],
        direct: bool,
        bound: Option<Bound<'a>>,
    ) -> BoxFuture<'a, Result<Vec<Cell>>> {
        self.related_from_mode(
            env,
            path,
            current,
            starts,
            IdentityReads {
                native: None,
                bound,
                direct,
                depth: 0,
            },
        )
    }

    /// Currency identity traversal preserves native compute authorization at every hop.
    pub(crate) fn currency_related_from_as<'a>(
        &'a mut self,
        env: &'a Env,
        path: &'a [crate::graph::FieldKey],
        current: Vec<Option<RecId>>,
        starts: &'a [Option<RecId>],
        direct: bool,
        bound: Option<Bound<'a>>,
    ) -> BoxFuture<'a, Result<Vec<Cell>>> {
        self.related_from_mode(
            env,
            path,
            current,
            starts,
            IdentityReads {
                native: Some(env.clone()),
                bound,
                direct,
                depth: 0,
            },
        )
    }

    fn related_from_mode<'a>(
        &'a mut self,
        env: &'a Env,
        path: &'a [crate::graph::FieldKey],
        current: Vec<Option<RecId>>,
        starts: &'a [Option<RecId>],
        reads: IdentityReads<'a>,
    ) -> BoxFuture<'a, Result<Vec<Cell>>> {
        Box::pin(async move {
            let bound = reads.bound;
            let direct = reads.direct;
            let access_env = if reads.native.is_some() {
                env.engine_sudo()
            } else {
                env.clone()
            };
            let Some((last, steps)) = path.split_last() else {
                return Ok(vec![Cell::Null; current.len()]);
            };
            let ends = self
                .related_walk(env, steps, current, starts, reads.clone())
                .await?;
            let mut cells = self.bound_cells(env, *last, &ends, starts, reads).await?;
            if let Some(bound) = bound {
                self.check_final_targets(
                    &access_env,
                    bound,
                    *last,
                    starts,
                    &mut cells,
                    direct && steps.is_empty(),
                )
                .await?;
            }
            Ok(cells)
        })
    }

    /// The values of `key` for each of `ends` (`Null` for none). A company-bound related field
    /// met inside a bound path is followed along its own path with the same bound instead of
    /// being read as superuser, which would drop the bound (review SC1).
    fn bound_cells<'a>(
        &'a mut self,
        env: &'a Env,
        key: crate::graph::FieldKey,
        ends: &'a [Option<RecId>],
        starts: &'a [Option<RecId>],
        reads: IdentityReads<'a>,
    ) -> BoxFuture<'a, Result<Vec<Cell>>> {
        Box::pin(async move {
            if let Some(native_env) = &reads.native {
                return self
                    .currency_bound_cells(native_env, key, ends, starts, reads.clone())
                    .await;
            }
            let bound = reads.bound;
            if bound.is_some() {
                let shared = Arc::clone(&self.shared);
                let field = shared.registry.model_by_id(key.0).field_by_id(key.1);
                if field.company_bound() {
                    let Some(fc) = shared.graph.compute(key) else {
                        return Err(Error::programming(format!(
                            "company-bound related field {}.{} has no compute entry",
                            shared.registry.model_by_id(key.0).name,
                            field.name
                        )));
                    };
                    match &fc.source {
                        Source::RelatedPath(inner) => {
                            return self
                                .related_from_as(env, inner, ends.to_vec(), starts, false, bound)
                                .await;
                        }
                        Source::Related(target) => {
                            return self
                                .related_from_as(
                                    env,
                                    &[(key.0, *target)],
                                    ends.to_vec(),
                                    starts,
                                    false,
                                    bound,
                                )
                                .await;
                        }
                        Source::Chain(_) | Source::DisplayName => {
                            return Err(not_a_related_source(&shared.registry, key.0, key.1));
                        }
                    }
                }
            }
            // a record with no value (`None`) reads as no value, as `related_from` always did
            let by_id = self.cells_by_id(env, key, ends).await?;
            Ok(ends
                .iter()
                .map(|id| {
                    id.and_then(|id| by_id.get(&id).cloned())
                        .unwrap_or(Cell::Null)
                })
                .collect())
        })
    }

    /// All currency identity reads, including inherited and related aliases, pass
    /// here. The existing engine still owns pending/slot/protection semantics.
    fn currency_bound_cells<'a>(
        &'a mut self,
        env: &'a Env,
        key: FieldKey,
        ends: &'a [Option<RecId>],
        starts: &'a [Option<RecId>],
        mut reads: IdentityReads<'a>,
    ) -> BoxFuture<'a, Result<Vec<Cell>>> {
        Box::pin(async move {
            // A positional null column has no records to compute. In particular,
            // settle_records(empty) must not settle unrelated pending native work.
            if ends.iter().all(Option::is_none) {
                return Ok(vec![Cell::Null; ends.len()]);
            }
            if reads.depth >= 256 {
                return Err(Error::programming(
                    "Monetary currency identity traversal exceeded 256 nested relations",
                ));
            }
            reads.depth += 1;
            let shared = Arc::clone(&self.shared);
            let spec = shared.registry.model_by_id(key.0);
            let field = spec.field_by_id(key.1);
            if let Some(inherited) = field.inherited {
                let links = self
                    .bound_cells(
                        env,
                        (key.0, inherited.delegate),
                        ends,
                        starts,
                        reads.clone(),
                    )
                    .await?;
                let parents: Vec<Option<RecId>> =
                    links.iter().map(crate::relational::id_of).collect();
                return self
                    .bound_cells(
                        env,
                        (inherited.parent, inherited.parent_field),
                        &parents,
                        starts,
                        reads,
                    )
                    .await;
            }
            let fc = shared.graph.compute(key);
            if let Some(fc) = fc {
                let path: Option<Vec<FieldKey>> = match &fc.source {
                    Source::Related(target) => Some(vec![(key.0, *target)]),
                    Source::RelatedPath(path) => Some(path.to_vec()),
                    Source::Chain(_) | Source::DisplayName => None,
                };
                if let Some(path) = path {
                    reads.bound = reads.bound.or_else(|| Bound::of(env, key.0, field));
                    let run_env = if fc.sudo {
                        env.engine_sudo()
                    } else {
                        env.clone()
                    };
                    reads.native = Some(run_env.clone());
                    reads.direct = false;
                    return self
                        .related_from_mode(&run_env, &path, ends.to_vec(), starts, reads)
                        .await;
                }
            }
            let mut seen = FxHashSet::default();
            let ids: Vec<RecId> = ends
                .iter()
                .flatten()
                .copied()
                .filter(|id| seen.insert(*id))
                .collect();
            let values = if let Some(fc) = fc.filter(|fc| matches!(fc.source, Source::Chain(_))) {
                if fc.stored {
                    self.settle_records(env, key.0, key.1, ids).await?;
                    self.cells_by_id(&env.engine_sudo(), key, ends).await?
                } else {
                    let records = Records::empty(env.clone(), key.0).browse_ids(ids.clone());
                    let cells = self.field_cells_checked(&records, key.1).await?;
                    if cells.len() != ids.len() {
                        return Err(Error::programming(
                            "Monetary native currency read returned an incomplete column",
                        ));
                    }
                    ids.into_iter().zip(cells).collect()
                }
            } else {
                self.cells_by_id(&env.engine_sudo(), key, ends).await?
            };
            ends.iter()
                .map(|id| match id {
                    None => Ok(Cell::Null),
                    Some(id) => values.get(id).cloned().ok_or_else(|| {
                        Error::programming("Monetary currency identity read lost a record")
                    }),
                })
                .collect()
        })
    }

    /// Privileged currency identity access with native computations in `env`.
    pub(crate) async fn currency_cells_by_id(
        &mut self,
        env: &Env,
        key: FieldKey,
        ids: &[Option<RecId>],
    ) -> Result<FxHashMap<RecId, Cell>> {
        let cells = self
            .bound_cells(
                env,
                key,
                ids,
                ids,
                IdentityReads {
                    native: Some(env.clone()),
                    bound: None,
                    direct: false,
                    depth: 0,
                },
            )
            .await?;
        Ok(ids
            .iter()
            .zip(cells)
            .filter_map(|(id, cell)| id.map(|id| (id, cell)))
            .collect())
    }

    /// The records a relational last field of a company-bound path points to count as one more
    /// hop: a company-scoped target of another company is refused like any hop, so the path never
    /// ends on a record it could not pass through (review SC4). A failing sibling of a lenient
    /// batch reads as empty.
    async fn check_final_targets(
        &mut self,
        env: &Env,
        bound: Bound<'_>,
        last: crate::graph::FieldKey,
        starts: &[Option<RecId>],
        cells: &mut [Cell],
        direct: bool,
    ) -> Result<()> {
        let registry = Arc::clone(&self.shared.registry);
        let field = registry.model_by_id(last.0).field_by_id(last.1);
        if field.ty.comodel().is_none() {
            return Ok(());
        }
        let mut flat_starts: Vec<Option<RecId>> = Vec::new();
        let mut flat_hops: Vec<Option<RecId>> = Vec::new();
        for (start, cell) in starts.iter().zip(cells.iter()) {
            for id in crate::relational::ids_of_cell(cell) {
                flat_starts.push(*start);
                flat_hops.push(Some(id));
            }
        }
        self.check_related_hop(env, bound, last, &flat_starts, &mut flat_hops, direct)
            .await?;
        if let Some(lenient) = bound.lenient() {
            for (start, cell) in starts.iter().zip(cells.iter_mut()) {
                if start.is_some_and(|s| lenient.has_failed(s)) {
                    *cell = Cell::Null;
                }
            }
        }
        Ok(())
    }

    /// With `bound`, every record reached must
    /// pass the company bound of the reader.
    pub(crate) async fn related_ends(
        &mut self,
        env: &Env,
        steps: &[crate::graph::FieldKey],
        current: Vec<Option<RecId>>,
        bound: Option<Bound<'_>>,
    ) -> Result<Vec<Option<RecId>>> {
        let starts = current.clone();
        self.related_walk(
            env,
            steps,
            current,
            &starts,
            IdentityReads {
                native: None,
                bound,
                direct: true,
                depth: 0,
            },
        )
        .await
    }

    async fn related_walk(
        &mut self,
        env: &Env,
        steps: &[crate::graph::FieldKey],
        mut current: Vec<Option<RecId>>,
        starts: &[Option<RecId>],
        reads: IdentityReads<'_>,
    ) -> Result<Vec<Option<RecId>>> {
        let bound = reads.bound;
        let direct = reads.direct;
        let access_env = if reads.native.is_some() {
            env.engine_sudo()
        } else {
            env.clone()
        };
        for (i, step) in steps.iter().enumerate() {
            let cells = self
                .bound_cells(env, *step, &current, starts, reads.clone())
                .await?;
            current = cells
                .iter()
                .map(|cell| match cell {
                    Cell::Id(_) => crate::relational::id_of(cell),
                    Cell::Ids(ids) => ids.first().copied(),
                    _ => None,
                })
                .collect();
            if let Some(bound) = bound {
                self.check_related_hop(
                    &access_env,
                    bound,
                    *step,
                    starts,
                    &mut current,
                    direct && i == 0,
                )
                .await?;
            }
        }
        Ok(current)
    }

    /// The values of `key` for the distinct records among `ids`, read for all at once.
    pub(crate) async fn cells_by_id(
        &mut self,
        env: &Env,
        key: crate::graph::FieldKey,
        ids: &[Option<RecId>],
    ) -> Result<FxHashMap<RecId, Cell>> {
        let mut seen: FxHashSet<RecId> = FxHashSet::default();
        let distinct: Vec<RecId> = ids
            .iter()
            .flatten()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect();
        if distinct.is_empty() {
            return Ok(FxHashMap::default());
        }
        let ids_arc: Arc<[RecId]> = Arc::from(distinct.clone());
        let recs = Records::from_parts(env.clone(), key.0, Arc::clone(&ids_arc), ids_arc);
        let cells = self.field_cells(&recs, key.1).await?;
        Ok(distinct.into_iter().zip(cells).collect())
    }

    /// The inverse of an editable related field through relations (COMP §4.8): `cell` is written
    /// on the record each of `records` reaches, when both are database records or both are new.
    pub(crate) async fn write_related_targets(
        &mut self,
        records: &Records,
        path: &[crate::graph::FieldKey],
        cell: Cell,
    ) -> Result<()> {
        let Some((last, steps)) = path.split_last() else {
            return Ok(());
        };
        let starts: Vec<Option<RecId>> = records.rec_ids().iter().copied().map(Some).collect();
        let ends = self
            .related_ends(records.env(), steps, starts, None)
            .await?;
        let mut seen: FxHashSet<RecId> = FxHashSet::default();
        let targets: Vec<RecId> = records
            .rec_ids()
            .iter()
            .zip(ends)
            .filter_map(|(record, target)| target.filter(|t| t.is_new() == record.is_new()))
            .filter(|t| seen.insert(*t))
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let name = self
            .shared
            .registry
            .model_by_id(last.0)
            .field_by_id(last.1)
            .name;
        let targets_arc: Arc<[RecId]> = Arc::from(targets);
        let target_records = Records::from_parts(
            records.env().clone(),
            last.0,
            Arc::clone(&targets_arc),
            targets_arc,
        );
        target_records
            .write(self, crate::RawVals::new().set_cell(name, cell))
            .await
    }

    /// Run the constraint methods whose fields intersect `names` and not `excluded`, once on the
    /// whole recordset, as superuser, on database records only (COMP §4.1,/019).
    pub(crate) async fn run_constraints(
        &mut self,
        records: &Records,
        names: &FxHashSet<FieldId>,
        excluded: &FxHashSet<FieldId>,
    ) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let graph = shared.graph.model(records.model_id());
        if graph.constraints.is_empty() || names.is_empty() {
            return Ok(());
        }
        let real: Vec<RecId> = records
            .rec_ids()
            .iter()
            .copied()
            .filter(|id| !id.is_new())
            .collect();
        if real.is_empty() {
            return Ok(());
        }
        let real: Arc<[RecId]> = Arc::from(real);
        let sudo = Records::from_parts(
            records.env().engine_sudo(),
            records.model_id(),
            Arc::clone(&real),
            Arc::clone(&real),
        );
        // A constraint of a package is not elevated by the ORM: its guest runs in the environment
        // that triggered it, with a read-only bridge (security review r1 H1).
        let plain = Records::from_parts(
            records.env().clone(),
            records.model_id(),
            Arc::clone(&real),
            real,
        );
        for c in &graph.constraints {
            if c.fields.iter().any(|f| names.contains(f))
                && !c.fields.iter().any(|f| excluded.contains(f))
            {
                let target = if crate::dispatch::chain_has_wasm(self, records.model_id(), c.method)
                {
                    &plain
                } else {
                    &sudo
                };
                call_unit_method(self, target, c.method).await?;
            }
        }
        Ok(())
    }

    // ---- flush ------------------------------------------------------------------------------

    /// True when a database record is waiting for a recompute.
    pub(crate) fn has_real_pending(&self) -> bool {
        self.cache.pending_fields().into_iter().any(|(m, f)| {
            self.registry()
                .models()
                .get(m)
                .and_then(|spec| {
                    let fid = spec.fields.get(f)?.id;
                    self.cache
                        .model(spec.id)
                        .map(|mc| mc.pending_ids(fid).iter().any(|id| !id.is_new()))
                })
                .unwrap_or(false)
        })
    }

    /// Recompute every pending field of `model` for database records, after the pending fields
    /// they depend on (on any model), in rank order.
    pub(crate) async fn recompute_model(&mut self, env: &Env, model: ModelId) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let fields: Vec<FieldId> = shared.graph.model(model).order.clone();
        self.recompute_fields(env, model, &fields).await
    }

    /// Recompute the pending computations of `fields` and of the computed fields they depend on
    /// (on any model), in rank order, for database records.
    pub(crate) async fn recompute_fields(
        &mut self,
        env: &Env,
        model: ModelId,
        fields: &[FieldId],
    ) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let graph = &shared.graph;
        let mut todo: FxHashSet<crate::graph::FieldKey> = FxHashSet::default();
        for f in fields {
            todo.extend(graph.upstream((model, *f)).iter().copied());
        }
        if todo.is_empty() {
            return Ok(());
        }
        for &(m, f) in graph.order() {
            if todo.contains(&(m, f))
                && graph.compute((m, f)).is_some_and(|fc| fc.stored)
                && self.has_real_pending_of(m, f)
            {
                self.recompute_field(env, m, f, false).await?;
            }
        }
        Ok(())
    }

    /// Whether a database record waits for a recompute of `field`.
    fn has_real_pending_of(&self, model: ModelId, field: FieldId) -> bool {
        self.cache.model(model).is_some_and(|m| {
            m.has_pending(field) && m.pending_ids(field).iter().any(|id| !id.is_new())
        })
    }

    /// Recompute every pending field of every model for database records, in global rank order
    /// (one pass: a field's changes mark the fields after it, which this pass reaches).
    pub(crate) async fn recompute_all(&mut self, env: &Env) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        for &(m, f) in shared.graph.order() {
            if shared.graph.compute((m, f)).is_some_and(|fc| fc.stored)
                && self.has_real_pending_of(m, f)
            {
                self.recompute_field(env, m, f, false).await?;
            }
        }
        Ok(())
    }

    /// `env.fields_to_compute()`: `model.field` names with records waiting for a recompute.
    pub fn fields_to_compute(&self) -> Vec<String> {
        let registry = self.registry();
        self.cache
            .pending_fields()
            .into_iter()
            .filter_map(|(m, f)| {
                let spec: &ModelSpec = registry.models().get(m)?;
                Some(format!("{}.{}", spec.name, spec.fields.get(f)?.name))
            })
            .collect()
    }

    /// `model.field` names with pending writes.
    pub(crate) fn fields_to_write(&self) -> Vec<String> {
        let registry = self.registry();
        self.cache
            .dirty_models()
            .into_iter()
            .filter_map(|m| {
                let spec: &ModelSpec = registry.models().get(m)?;
                let mc = self.cache.model(spec.id)?;
                Some(
                    mc.dirty()
                        .into_iter()
                        .filter_map(|(f, _)| {
                            Some(format!("{}.{}", spec.name, spec.fields.get(f)?.name))
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect()
    }

    /// `env.records_to_compute(field)`: the records of `model` waiting for a recompute of `field`.
    pub fn records_to_compute(&self, model: &Records, field: &str) -> Result<Records> {
        let spec = model.spec();
        let fid = spec.field_id(field).ok_or_else(|| {
            Error::programming(format!("Invalid field '{field}' on '{}'", spec.name))
        })?;
        let ids = self
            .cache
            .model(model.model_id())
            .map(|m| m.pending_ids(fid))
            .unwrap_or_default();
        Ok(model.browse_ids(ids))
    }

    /// The cache key of a context-dependent field in `env` (`env.cache_key(field)`,
    pub fn cache_key(&self, env: &Env, model: &str, field: &str) -> Result<Vec<Value>> {
        let spec = self.registry().model(model).ok_or_else(|| {
            Error::programming(format!("Model '{model}' does not exist in registry."))
        })?;
        let fid = spec
            .field_id(field)
            .ok_or_else(|| Error::programming(format!("Invalid field '{field}' on '{model}'")))?;
        match self.shared.graph.model(spec.id).compute(fid) {
            Some(fc) => cache_key(self, env, &fc.depends_context, fc.sudo),
            None => Ok(Vec::new()),
        }
    }

    /// The resolved dependencies of a computed field (`registry.field_depends`), in resolution
    /// order (base implementation first).
    pub fn field_depends(&self, model: &str, field: &str) -> Result<Vec<&'static str>> {
        let spec = self.registry().model(model).ok_or_else(|| {
            Error::programming(format!("Model '{model}' does not exist in registry."))
        })?;
        let fid = spec
            .field_id(field)
            .ok_or_else(|| Error::programming(format!("Invalid field '{field}' on '{model}'")))?;
        Ok(match self.shared.graph.model(spec.id).compute(fid) {
            Some(fc) => fc
                .depends
                .iter()
                .map(|d| spec.field_by_id(*d).name)
                .collect(),
            None => Vec::new(),
        })
    }
}

/// The comodel fields a compute reads through `field` (a `(model, field)` of its own records):
/// the second step of every input path that starts with `field`, as `(comodel, field)`, in path
/// order, without repeats.
fn line_columns(fc: &FieldCompute, field: (ModelId, FieldId)) -> Vec<(ModelId, FieldId)> {
    let mut out: Vec<(ModelId, FieldId)> = Vec::new();
    for path in fc.input_paths() {
        if path.first().is_some_and(|step| *step == field)
            && let Some(step) = path.get(1)
            && !out.contains(step)
        {
            out.push(*step);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review M5: an engine snapshot copies nothing; the first write inside the scope copies the
    /// one map it changes, and the restore brings the shared original back.
    #[test]
    fn engine_snapshot_is_copy_on_first_write() {
        let mut engine = EngineState::default();
        engine
            .refs_mut()
            .insert("a".to_owned(), RecId::new_unique(1).unwrap());
        let snapshot = engine.snapshot();
        assert!(Arc::ptr_eq(&engine.refs, &snapshot.refs));
        assert!(Arc::ptr_eq(&engine.context, &snapshot.context));
        assert!(Arc::ptr_eq(&engine.patches, &snapshot.patches));
        engine
            .refs_mut()
            .insert("b".to_owned(), RecId::new_unique(2).unwrap());
        assert!(!Arc::ptr_eq(&engine.refs, &snapshot.refs));
        assert!(Arc::ptr_eq(&engine.context, &snapshot.context));
        engine.restore(snapshot);
        assert_eq!(engine.refs.len(), 1);
    }

    use crate::context::Context;
    use crate::principal::{CompanyId, GroupSet, Principal, PrincipalBuilder, Resolution, UserId};
    use erp_registry::{AddonDescriptor, ModelDecl, RegistryBuilder};

    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "t",
        depends: &["base"],
        exact_only: false,
    };
    static MODEL: ModelDecl = ModelDecl::new("t", "t.partner");
    static OTHER: ModelDecl = ModelDecl::new("t", "t.other");

    fn env_of(principal: Principal, context: Context) -> Env {
        let registry = RegistryBuilder::new([&ADDON])
            .with_decls([&MODEL, &OTHER])
            .build()
            .unwrap();
        Env::new(Arc::new(registry), principal, context)
    }

    fn user(companies: &[i64], debug: bool) -> Principal {
        let all = [1, 2].map(|c| CompanyId::new(c).unwrap());
        PrincipalBuilder::user(UserId::new(5).unwrap())
            .resolution(
                Resolution::in_id_order(GroupSet::default(), all, std::num::NonZeroU64::MIN)
                    .unwrap(),
            )
            .companies(
                companies
                    .iter()
                    .map(|c| CompanyId::new(*c).unwrap())
                    .collect(),
            )
            .debug(debug)
            .build()
            .unwrap()
    }

    /// (g), R-M1: the memo is keyed by the whole binding and the model, so two bindings
    /// that differ in anything a decision depends on share nothing, and equal ones share their
    /// entry.
    #[test]
    fn readable_memo_keys_hold_the_whole_binding() {
        use crate::principal::{GroupSetId, KindClass, PrincipalBinding, SnapshotId};
        let env = env_of(user(&[1], false), Context::new());
        let model = env.registry().model_id("t.partner").unwrap();
        let other = env.registry().model_id("t.other").unwrap();
        let key = |principal: &Principal, groups: u64, snapshot: SnapshotId, model: ModelId| {
            let resolution = Arc::clone(principal.resolution());
            let binding = PrincipalBinding::new(
                principal,
                resolution,
                GroupSetId::new(std::num::NonZeroU64::new(groups).unwrap()),
                KindClass::derive(principal.kind(), false),
            );
            MemoKey::new(binding.key(snapshot), model)
        };
        let snap = SnapshotId::next().unwrap();
        let base = user(&[1], false);
        assert_eq!(
            key(&base, 1, snap, model),
            key(&user(&[1], false), 1, snap, model),
            "equal bindings share an entry"
        );
        let others = [
            key(&user(&[2], false), 1, snap, model),
            key(&user(&[1, 2], false), 1, snap, model),
            key(&user(&[2, 1], false), 1, snap, model),
            key(&user(&[1], true), 1, snap, model),
            key(&base, 2, snap, model),
            key(&base, 1, SnapshotId::next().unwrap(), model),
            key(&base, 1, snap, other),
        ];
        for (i, other) in others.iter().enumerate() {
            assert_ne!(key(&base, 1, snap, model), *other, "variant {i}");
        }
        // one entry per key, and clearing empties all
        let mut memo = ReadableMemo::default();
        memo.entry(&key(&base, 1, snap, model)).unrestricted = true;
        assert!(
            memo.peek(&key(&user(&[1], false), 1, snap, model))
                .is_some_and(|e| e.unrestricted)
        );
        assert!(memo.peek(&others[0]).is_none());
        memo.clear();
        assert!(memo.is_empty());
    }
}
