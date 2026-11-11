//! Marking the dependents of a change, on the model and across relations (COMP §4.4;
//! §4,–7).
//!
//! [`Transaction::modified`] starts from changed `(field, records)` pairs and visits the computed
//! fields they reach, **each once per record and in global rank order** (a field is visited
//! after every field it depends on), with the union of the records that reach it:
//!
//! - a stored computed field joins its *to-compute* set and stops the walk: its own dependents
//!   are marked only when its recomputed value actually changes;
//! - protected `(field, record)` pairs are skipped (COMP §4.6).
//!
//! Dependents on other models are reached through the compiled trigger trees
//! ([`crate::graph::TriggerTree`]). Each edge names a relational field `f` of a model `M`; the
//! records of `M` whose `f` points to the current records are found:
//!
//! - one2many `f`: the inverse many2one of the current records (cache, else one prefetch query);
//! - many2one `f` with an inverse one2many without domain whose values are cached: from the
//!   cache; otherwise one `SELECT id FROM M WHERE f = ANY($1)`, corrected by the pending writes
//!   of `f` in the cache (no flush, no ORM search, no access rules or active test:);
//! - many2many `f`: the mirror field's cached values, else one query of the relation table;
//!
//! An edge is not followed when every target behind it is non-stored, uncached and reaches
//! nothing stored: `modified` on an empty cache costs 0 statements.

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_fields::{Cell, Error, FieldType, RecId, Result};
use erp_registry::{FieldId, ModelId};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::dispatch::BoxFuture;
use crate::env::Env;
use crate::graph::{ComputeGraph, FieldKey, TriggerTree};
use crate::records::Records;

/// The work of one `modified()` call.
#[derive(Default)]
struct Walk {
    /// Fields to visit, by global rank, with the records that reach them.
    queue: BTreeMap<(u32, FieldKey), Vec<RecId>>,
    /// Records already visited per field.
    seen: FxHashMap<FieldKey, FxHashSet<RecId>>,
    /// The prefetch set of the changed records (model and ids): relational values read to follow
    /// an edge from those records are read for the whole set at once.
    prefetch: Option<(ModelId, Arc<[RecId]>)>,
    /// Edges already followed in this walk, by (edge, sorted ids): several dependents behind the
    /// same edge, or the same records reached twice, cost one lookup.
    edges: FxHashMap<(FieldKey, Vec<RecId>), Vec<RecId>>,
}

impl Walk {
    fn push(&mut self, graph: &ComputeGraph, key: FieldKey, ids: &[RecId]) {
        if ids.is_empty() {
            return;
        }
        self.queue
            .entry((graph.rank(key), key))
            .or_default()
            .extend_from_slice(ids);
    }
}

/// Ids per edge lookup statement (bounded `= ANY` arrays).
const EDGE_CHUNK: usize = 10_000;

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

fn ids_in(cell: &Cell) -> Vec<RecId> {
    match cell {
        Cell::Id(_) => crate::relational::id_of(cell).into_iter().collect(),
        Cell::Ids(ids) => ids.to_vec(),
        _ => Vec::new(),
    }
}

impl Transaction {
    /// Notify the dependents of fields changed on records of `model` (module documentation).
    pub(crate) fn modified(
        &mut self,
        model: ModelId,
        changes: Vec<(FieldId, Vec<RecId>)>,
    ) -> BoxFuture<'_, Result<()>> {
        self.modified_prefetch(model, changes, None)
    }

    /// [`Transaction::modified`] for records with a prefetch set: the relational values needed to
    /// follow edges from them are read for the whole set.
    pub(crate) fn modified_prefetch(
        &mut self,
        model: ModelId,
        changes: Vec<(FieldId, Vec<RecId>)>,
        prefetch: Option<Arc<[RecId]>>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if changes.iter().all(|(_, ids)| ids.is_empty()) {
                return Ok(());
            }
            let shared = Arc::clone(&self.shared);
            let graph = &shared.graph;
            let spec = shared.registry.model_by_id(model);
            let mut changes = changes;
            for field in spec.fields.iter().filter(|f| f.inherited.is_none()) {
                let Some(generated) = field.generated() else {
                    continue;
                };
                let ids: Vec<RecId> = changes
                    .iter()
                    .filter(|(source, _)| generated.dependencies().contains(source))
                    .flat_map(|(_, ids)| ids.iter().copied())
                    .collect();
                if !ids.is_empty() {
                    self.cache
                        .model_mut(model)
                        .invalidate(Some(&[field.id]), Some(&ids));
                    self.note_database_change(model, &[field.id]);
                    changes.push((field.id, ids));
                }
            }
            let mut walk = Walk {
                prefetch: prefetch.map(|p| (model, p)),
                ..Walk::default()
            };
            for (field, ids) in &changes {
                if !ids.is_empty() {
                    self.reach(graph, (model, *field), ids, &mut walk).await?;
                }
            }
            while let Some(((_, key), ids)) = walk.queue.pop_first() {
                let Some(fc) = graph.compute(key) else {
                    continue;
                };
                let seen = walk.seen.entry(key).or_default();
                let mut fresh: Vec<RecId> = Vec::with_capacity(ids.len());
                for id in ids {
                    if seen.insert(id) && !self.is_protected(key.0, key.1, id) {
                        fresh.push(id);
                    }
                }
                if fresh.is_empty() {
                    continue;
                }
                if fc.stored {
                    self.bind_capture(key.0, key.1, &fresh)?;
                    let storage = shared
                        .registry
                        .model_by_id(key.0)
                        .field_by_id(key.1)
                        .storage();
                    self.cache
                        .model_mut(key.0)
                        .mark_pending(key.1, storage, &fresh);
                    // a stored value that is stale until its recompute is a pending change of a
                    // database record: remembered rule decisions that read it end now, not when
                    // the recompute runs
                    if fresh.iter().any(|id| !id.is_new()) {
                        self.note_database_change(key.0, &[key.1]);
                    }
                    continue;
                }
                let next: Vec<RecId> = if graph.reaches_stored(key) {
                    fresh.clone()
                } else {
                    fresh
                        .iter()
                        .copied()
                        .filter(|id| self.cached_anywhere(key, *id))
                        .collect()
                };
                // a non-stored many2one that is the inverse of one2many fields: its old owners,
                // while its values are still cached
                let o2ms = self.inverse_one2manys(key);
                let old_owners: Option<Vec<RecId>> = (!o2ms.is_empty()).then(|| {
                    fresh
                        .iter()
                        .filter_map(|id| self.cache.get(key.0, key.1, *id))
                        .filter_map(|c| crate::relational::id_of(&c))
                        .collect()
                });
                let all_known = fresh
                    .iter()
                    .all(|id| self.cache.contains(key.0, key.1, *id));
                self.cache
                    .model_mut(key.0)
                    .invalidate(Some(&[key.1][..]), Some(&fresh));
                self.engine
                    .context_mut()
                    .invalidate(key.0, Some(&[key.1][..]), Some(&fresh));
                if let Some(old) = old_owners {
                    self.reach_inverse_owners(graph, key, &fresh, &o2ms, old, all_known, &mut walk)
                        .await?;
                }
                if !next.is_empty() {
                    self.reach(graph, key, &next, &mut walk).await?;
                }
            }
            Ok(())
        })
    }

    /// The columns of `key.0` that marking the dependents of `key` reads on the changed records:
    /// the inverse many2one (and its model field) of every stored one2many edge that leaves them
    /// (`follow_edge`). A compute's old-value load fetches them with the field
    /// (audit W12), so a changed value reaches its dependents through the cache, as it did when
    /// the load fetched the whole prefetch group.
    ///
    /// # Errors
    ///
    /// The comodel error of a broken edge (the walk would raise the same).
    pub(crate) fn trigger_columns(&self, key: FieldKey) -> Result<Vec<FieldId>> {
        let mut out: Vec<FieldId> = Vec::new();
        let Some(tree) = self.shared.graph.triggers(key) else {
            return Ok(out);
        };
        let registry = &self.shared.registry;
        for ((model, field), _) in &tree.children {
            let fs = registry.model_by_id(*model).field_by_id(*field);
            let FieldType::One2many { inverse, .. } = &fs.ty else {
                continue;
            };
            if !fs.store {
                continue;
            }
            let comodel = crate::relational::comodel_of(registry, fs)?;
            let model_field = self
                .shared
                .relations
                .model_field(*model, *field)
                .and_then(|mf| comodel.field_id(mf));
            let inverse = comodel.field(inverse).map(|f| f.id);
            for f in inverse.into_iter().chain(model_field) {
                if comodel.field_by_id(f).has_column() && !out.contains(&f) {
                    out.push(f);
                }
            }
        }
        Ok(out)
    }

    /// The stored one2many fields whose inverse is the non-stored many2one `key` (REL §4.6:
    /// their content follows a computed or related field).
    pub(crate) fn inverse_one2manys(&self, key: FieldKey) -> Vec<FieldKey> {
        let registry = &self.shared.registry;
        let fs = registry.model_by_id(key.0).field_by_id(key.1);
        if fs.store || !matches!(fs.ty, FieldType::Many2one { .. }) {
            return Vec::new();
        }
        self.shared
            .relations
            .inverses(key.0, key.1)
            .iter()
            .copied()
            .filter(|(m, f)| {
                let o2m = registry.model_by_id(*m).field_by_id(*f);
                matches!(o2m.ty, FieldType::One2many { .. }) && o2m.store
            })
            .collect()
    }

    /// After the non-stored many2one `key` of `ids` lost its cached values: the one2many fields
    /// over it forget the values of the old and new owners (all their values when an old owner is
    /// unknown) and their dependents are notified for those owners. The new owners are the
    /// field's values now (computed, when the one2many has dependents or cached values).
    #[allow(clippy::too_many_arguments)]
    async fn reach_inverse_owners(
        &mut self,
        graph: &ComputeGraph,
        key: FieldKey,
        ids: &[RecId],
        o2ms: &[FieldKey],
        old_owners: Vec<RecId>,
        all_known: bool,
        walk: &mut Walk,
    ) -> Result<()> {
        let needed = o2ms
            .iter()
            .any(|k| graph.has_dependents(*k) || self.has_cached(*k));
        if !needed {
            return Ok(());
        }
        let mut owners: Vec<RecId> = old_owners;
        {
            let ids_arc: Arc<[RecId]> = Arc::from(ids);
            let records =
                Records::from_parts(self.traversal_env(), key.0, Arc::clone(&ids_arc), ids_arc);
            for cell in self.field_cells(&records, key.1).await? {
                owners.extend(crate::relational::id_of(&cell));
            }
        }
        let mut seen: FxHashSet<RecId> = FxHashSet::default();
        owners.retain(|o| seen.insert(*o));
        for &(m, o2m) in o2ms {
            if all_known {
                self.cache
                    .model_mut(m)
                    .invalidate(Some(&[o2m][..]), Some(&owners));
            } else if self.cache.model(m).is_some() {
                self.cache.model_mut(m).invalidate(Some(&[o2m][..]), None);
            }
            if !owners.is_empty() {
                self.reach(graph, (m, o2m), &owners, walk).await?;
            }
        }
        Ok(())
    }

    /// Whether `id`'s value of `key` is cached, in the plain cache or in any context.
    fn cached_anywhere(&self, key: FieldKey, id: RecId) -> bool {
        self.cache.contains(key.0, key.1, id) || self.engine.context.contains(key.0, key.1, id)
    }

    /// Whether some record has a cached value of `key` (any context).
    fn has_cached(&self, key: FieldKey) -> bool {
        self.cache.model(key.0).is_some_and(|m| m.has_values(key.1))
            || self.engine.context.has_values(key.0, key.1)
    }

    /// Whether a trigger subtree can change anything observable.
    fn tree_needed(&self, node: &TriggerTree) -> bool {
        node.reaches_stored || node.all_targets.iter().any(|t| self.has_cached(*t))
    }

    /// Queue the dependents of `key` for `ids`: on the same model, then through relations.
    async fn reach(
        &mut self,
        graph: &ComputeGraph,
        key: FieldKey,
        ids: &[RecId],
        walk: &mut Walk,
    ) -> Result<()> {
        for dep in graph.model(key.0).dependents(key.1) {
            walk.push(graph, (key.0, *dep), ids);
        }
        if let Some(tree) = graph.triggers(key) {
            self.walk_tree(graph, tree, ids.to_vec(), walk).await?;
        }
        Ok(())
    }

    fn walk_tree<'a>(
        &'a mut self,
        graph: &'a ComputeGraph,
        node: &'a TriggerTree,
        ids: Vec<RecId>,
        walk: &'a mut Walk,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            for (edge, child) in &node.children {
                if !self.tree_needed(child) {
                    continue;
                }
                let mut key_ids = ids.clone();
                key_ids.sort_unstable();
                key_ids.dedup();
                let memo_key = (*edge, key_ids);
                let next = match walk.edges.get(&memo_key) {
                    Some(found) => found.clone(),
                    None => {
                        let prefetch = walk.prefetch.clone();
                        let found = self.follow_edge(*edge, &ids, prefetch.as_ref()).await?;
                        walk.edges.insert(memo_key, found.clone());
                        found
                    }
                };
                if next.is_empty() {
                    continue;
                }
                for target in &child.targets {
                    walk.push(graph, *target, &next);
                }
                if !child.children.is_empty() {
                    self.walk_tree(graph, child, next, walk).await?;
                }
            }
            Ok(())
        })
    }

    /// The environment of traversals: superuser, no active test.
    fn traversal_env(&self) -> Env {
        self.env()
            .sudo(true)
            .with_context([("active_test", erp_fields::Value::Bool(false))])
    }

    /// The records of `edge.0` whose relational field `edge.1` points to one of `ids` (records of
    /// the field's comodel); relational values are read for `prefetch` too when `ids` are records
    /// of its model.
    async fn follow_edge(
        &mut self,
        edge: FieldKey,
        ids: &[RecId],
        prefetch: Option<&(ModelId, Arc<[RecId]>)>,
    ) -> Result<Vec<RecId>> {
        let (model, field) = edge;
        let shared = Arc::clone(&self.shared);
        let registry = &shared.registry;
        let spec = registry.model_by_id(model);
        let fs = spec.field_by_id(field);
        let comodel = crate::relational::comodel_of(registry, fs)?;
        let env = self.traversal_env();
        let targets: FxHashSet<RecId> = ids.iter().copied().collect();
        let mut out: Vec<RecId> = Vec::new();
        let real: Vec<i64> = ids
            .iter()
            .filter_map(|i| i.as_real())
            .map(|i| i.get())
            .collect();
        if !fs.store {
            for chunk in real.chunks(EDGE_CHUNK) {
                let domain = erp_domain::Domain::condition(
                    fs.name,
                    erp_domain::Operator::In,
                    erp_domain::Value::list(chunk.iter().copied()),
                )
                .map_err(Error::from)?;
                let found = Records::empty(env.clone(), model)
                    .search_with(
                        self,
                        &domain,
                        &crate::SearchOptions::new().active_test(false),
                    )
                    .await?;
                out.extend(found.rec_ids().iter().copied());
            }
        } else {
            match &fs.ty {
                FieldType::One2many { inverse, .. } => {
                    let Some(inv) = comodel.field(inverse) else {
                        return Ok(Vec::new());
                    };
                    let cells = match prefetch.filter(|(m, _)| *m == comodel.id) {
                        Some((_, set)) => {
                            self.peek_cells_prefetch(&env, comodel.id, inv.id, ids, set)
                                .await?
                        }
                        None => self.peek_cells(&env, comodel.id, inv.id, ids).await?,
                    };
                    let model_field = shared.relations.model_field(model, field);
                    let models = match model_field.and_then(|mf| comodel.field_id(mf)) {
                        Some(mf) => Some(self.peek_cells(&env, comodel.id, mf, ids).await?),
                        None => None,
                    };
                    for (i, cell) in cells.iter().enumerate() {
                        if let Some(models) = &models
                            && !matches!(models.get(i), Some(Cell::Str(m)) if **m == *spec.name)
                        {
                            continue;
                        }
                        out.extend(ids_in(cell));
                    }
                }
                FieldType::Many2one { .. } => {
                    let inverse = shared
                        .relations
                        .inverses(model, field)
                        .iter()
                        .copied()
                        .find(|(m, f)| {
                            *m == comodel.id && shared.relations.domain(*m, *f).is_true()
                        });
                    let cached: Option<Vec<RecId>> = inverse.and_then(|(m, o2m)| {
                        let mut all = Vec::new();
                        for id in ids {
                            match self.cache.get(m, o2m, *id) {
                                Some(Cell::Ids(lines)) => all.extend(lines.iter().copied()),
                                _ => return None,
                            }
                        }
                        Some(all)
                    });
                    match (cached, inverse) {
                        (Some(found), _) => out = found,
                        (None, Some((m, o2m))) => {
                            // the inverse one2many, read for the prefetch set at once and cached
                            let mut set: Vec<RecId> = ids.to_vec();
                            if let Some((_, more)) = prefetch.filter(|(pm, _)| *pm == m) {
                                set.extend(more.iter().copied());
                            }
                            let records =
                                Records::from_parts(env.clone(), m, Arc::from(ids), Arc::from(set));
                            self.ensure_x2many(&records, o2m).await?;
                            for id in ids {
                                if let Some(Cell::Ids(lines)) = self.cache.get(m, o2m, *id) {
                                    out.extend(lines.iter().copied());
                                }
                            }
                        }
                        (None, None) => {
                            let mut found: Vec<RecId> = Vec::new();
                            if fs.has_column() {
                                let sql =
                                    erp_sql::relational::ids_referencing(&spec.table, fs.name);
                                for chunk in real.chunks(EDGE_CHUNK) {
                                    let chunk = chunk.to_vec();
                                    let rows = self.query(&sql, &[&chunk]).await?;
                                    found.extend(decode_ids(&rows)?);
                                }
                            }
                            // pending writes of the field win over the database (no flush needed)
                            if let Some(mc) = self.cache.model(model) {
                                let dirty = mc.dirty_values(field);
                                if !dirty.is_empty() {
                                    let changed: FxHashSet<RecId> =
                                        dirty.iter().map(|(id, _)| *id).collect();
                                    found.retain(|id| !changed.contains(id));
                                    found.extend(dirty.iter().filter_map(|(id, cell)| {
                                        crate::relational::id_of(cell)
                                            .filter(|t| targets.contains(t))
                                            .map(|_| *id)
                                    }));
                                }
                            }
                            out = found;
                        }
                    }
                }
                FieldType::Many2many { relation, .. } => {
                    let mirror = shared
                        .relations
                        .inverses(model, field)
                        .iter()
                        .copied()
                        .find(|(m, f)| {
                            *m == comodel.id && shared.relations.domain(*m, *f).is_true()
                        });
                    let cached: Option<Vec<RecId>> = mirror.and_then(|(m, f)| {
                        let mut all = Vec::new();
                        for id in ids {
                            match self.cache.get(m, f, *id) {
                                Some(Cell::Ids(owners)) => all.extend(owners.iter().copied()),
                                _ => return None,
                            }
                        }
                        Some(all)
                    });
                    match (cached, relation) {
                        (Some(found), _) => out = found,
                        (None, Some(rel)) => {
                            let sql =
                                erp_sql::relational::m2m_owners(erp_sql::relational::Relation {
                                    table: &rel.table,
                                    column1: &rel.column1,
                                    column2: &rel.column2,
                                });
                            for chunk in real.chunks(EDGE_CHUNK) {
                                let chunk = chunk.to_vec();
                                let rows = self.query(&sql, &[&chunk]).await?;
                                out.extend(decode_ids(&rows)?);
                            }
                        }
                        (None, None) => {}
                    }
                }
                _ => return Ok(Vec::new()),
            }
        }
        // new records whose cached value of the field holds one of the records
        if let Some(mc) = self.cache.model(model) {
            for (id, cell) in mc.new_values(field) {
                if ids_in(&cell).iter().any(|t| targets.contains(t)) {
                    out.push(id);
                }
            }
        }
        let mut seen: FxHashSet<RecId> = FxHashSet::default();
        out.retain(|id| seen.insert(*id));
        Ok(out)
    }

    /// [`Transaction::peek_cells`] loading uncached values for `prefetch` too.
    async fn peek_cells_prefetch(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
        prefetch: &[RecId],
    ) -> Result<Vec<Cell>> {
        let has_column = self
            .shared
            .registry
            .model_by_id(model)
            .field_by_id(field)
            .has_column();
        if has_column
            && ids
                .iter()
                .any(|i| !i.is_new() && !self.cache.contains(model, field, *i))
        {
            let records =
                Records::from_parts(env.clone(), model, Arc::from(ids), Arc::from(prefetch));
            if let Err(error) = self.load(&records, field).await
                && error.kind() != erp_fields::ErrorKind::Missing
            {
                return Err(error);
            }
        }
        self.peek_cells(env, model, field, ids).await
    }

    /// The current values of `field` for `ids` without computing anything: cached values, the
    /// database values of uncached database records (one prefetch query), the origin's value for
    /// new records with an origin; NULL otherwise. Records that no longer exist read NULL.
    pub(crate) async fn peek_cells(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<Vec<Cell>> {
        let ids_arc: Arc<[RecId]> = Arc::from(ids);
        let records = Records::from_parts(env.clone(), model, Arc::clone(&ids_arc), ids_arc);
        let has_column = self
            .shared
            .registry
            .model_by_id(model)
            .field_by_id(field)
            .has_column();
        if has_column {
            if let Err(error) = self.load(&records, field).await
                && error.kind() != erp_fields::ErrorKind::Missing
            {
                return Err(error);
            }
            let origins: Vec<RecId> = ids
                .iter()
                .filter(|id| id.is_new() && !self.cache.contains(model, field, **id))
                .filter_map(|id| id.origin().map(RecId::real))
                .collect();
            if !origins.is_empty() {
                let origins_arc: Arc<[RecId]> = Arc::from(origins);
                let origin_records =
                    Records::from_parts(env.clone(), model, Arc::clone(&origins_arc), origins_arc);
                if let Err(error) = self.load(&origin_records, field).await
                    && error.kind() != erp_fields::ErrorKind::Missing
                {
                    return Err(error);
                }
            }
        } else {
            self.ensure(&records, field).await?;
        }
        Ok(ids
            .iter()
            .map(|id| {
                self.cache
                    .get(model, field, *id)
                    .or_else(|| {
                        id.origin()
                            .filter(|_| id.is_new())
                            .and_then(|o| self.cache.get(model, field, RecId::real(o)))
                    })
                    .unwrap_or(Cell::Null)
            })
            .collect())
    }
}
