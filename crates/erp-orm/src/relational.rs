//! Relational fields at run time (REL §4.4–4.12): the relation index of a registry, reading
//! many2one / one2many / many2many / reference values, their record and read formats, and keeping
//! cached one2many values coherent when a many2one changes.
//!
//! - **Cache format.** A many2one caches the target id (`Cell::Id`); an x2many caches the ids of
//!   every linked record, archived ones included, in the comodel's order (`Cell::Ids`, REL
//!   §4.10). Reading an x2many filters on access: archived lines are hidden unless the field's
//!   `context` (then the environment) turns the active test off; outside superuser mode, lines
//!   the access policy hides are left out (never an error, REL §4.6.1).
//! - **Fetching.** A cache miss on an x2many loads the field for every uncached record of the
//!   prefetch set in one query: a superuser search on the comodel for a one2many, one join of the
//!   relation table for a many2many.
//! - **Relational prefetch.** The recordset reached through a relational field prefetches the
//!   values of the same field over the source's prefetch set, so a loop over records
//!   reading `partner_id.country_id.name` costs one query per hop for the whole set.
//! - **Inverse coherence.** Assigning a many2one removes the records from the cached one2many
//!   values of their old targets and appends them to the new target's, when that value is cached
//!   and the records satisfy the one2many's domain. No query is needed when the
//!   domain's fields are cached; otherwise they are fetched once for all the records.

use std::sync::Arc;

use erp_domain::{Domain, DomainKind, Operator, Value as DValue};
use erp_fields::{Cell, Error, FieldType, RecId, Result, Value};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::dispatch::BoxFuture;
use crate::env::Env;
use crate::records::Records;

type FieldKey = (ModelId, FieldId);

/// Relational facts of a registry, computed once per ORM (next to the dispatch tables).
#[derive(Debug, Default)]
pub(crate) struct Relations {
    /// Parsed `domain=` of relational fields, plus `model_field = '<model>'` for a one2many over
    /// a many2one_reference (F2).
    domains: FxHashMap<FieldKey, Domain>,
    /// The declared `domain=` alone, where it differs from `domains`.
    declared: FxHashMap<FieldKey, Domain>,
    /// One2many over a many2one_reference → the comodel's model field (F2).
    model_fields: FxHashMap<FieldKey, &'static str>,
    /// Many2one_reference and its model field → the one2many fields over it (cache coherence).
    reference_inverses: FxHashMap<FieldKey, Vec<FieldKey>>,
    /// Parsed `context=` of relational fields.
    contexts: FxHashMap<FieldKey, Vec<(String, Value)>>,
    /// Field → its inverse fields (one2many ↔ many2one, many2many ↔ mirror many2many, REL §4.5).
    inverses: FxHashMap<FieldKey, Vec<FieldKey>>,
    /// Many2many → other fields of the same model on the same relation and columns.
    siblings: FxHashMap<FieldKey, Vec<FieldId>>,
    /// Model → stored many2one fields pointing to it (FK metadata for `unlink`).
    referencing: FxHashMap<ModelId, Vec<FieldKey>>,
    /// Eventful root → bounded physical FK effects for names-only unlink tombstones.
    deletion_capture: FxHashMap<ModelId, DeletionCaptureModels>,
    /// Model → stored x2many fields whose comodel it is.
    x2many_to: FxHashMap<ModelId, Vec<FieldKey>>,
    /// Stored one2many over a stored inverse → the order of its inverse read,
    /// computed once. A comodel order that does not parse is left out: the read computes it
    /// again and raises the `UserError` a search in that order raises.
    inverse_orders: FxHashMap<FieldKey, Arc<str>>,
    /// Model → what deleting its rows changes beyond the record cache: the named caches and key
    /// tables of the follow-delete triggers its deletion fires, itself or through the rows a
    /// foreign key deletes with it (computed once).
    follow_deletes: FxHashMap<ModelId, FollowEffects>,
    /// The same effects by table, for statements sent as SQL (a `DELETE` the transaction runs
    /// itself): every table of [`Relations::follow_deletes`]' models and the relation tables of
    /// relation triggers.
    follow_tables: Vec<(String, FollowEffects)>,
}

/// Shared once per registry snapshot, with interned IDs rather than per-record model names.
#[derive(Debug, Default)]
struct DeletionCaptureModels {
    /// At most `MAX_CASCADE_MODELS` IDs, sorted by model name.
    models: Vec<ModelId>,
    /// The complete effect set exceeds the bound; `models` must never be emitted as complete.
    overflow: bool,
}

/// What a deletion of a model's rows makes the follow-delete triggers do
/// ([`erp_registry::FollowDeleteDecl`]), so the deleting transaction forgets it at once (the
/// triggers bump the generations every other process reads).
#[derive(Debug, Default)]
pub(crate) struct FollowEffects {
    /// Named caches to invalidate, sorted and unique.
    pub(crate) caches: Vec<&'static str>,
    /// Models on the key tables whose rows the triggers delete, sorted and unique.
    pub(crate) key_models: Vec<ModelId>,
}

fn json_to_value(json: &erp_fields::serde_json::Value) -> Value {
    use erp_fields::serde_json::Value as J;
    match json {
        J::Null => Value::False,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_f64().map(Value::Float))
            .unwrap_or(Value::False),
        J::String(s) => Value::str(s),
        J::Array(items) => Value::list(items.iter().map(json_to_value)),
        J::Object(map) => Value::Map(Arc::new(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect::<erp_fields::ValueMap>(),
        )),
    }
}

/// A domain rebuilt from its AST so that it is not tagged as external input (field domains are
/// declared by addon code: their errors are `Programming`).
pub(crate) fn internal(domain: &Domain) -> Domain {
    match domain.kind() {
        DomainKind::True => Domain::TRUE,
        DomainKind::False => Domain::FALSE,
        DomainKind::Not(child) => !internal(child),
        DomainKind::And(children) => Domain::and(children.iter().map(internal)),
        DomainKind::Or(children) => Domain::or(children.iter().map(internal)),
        DomainKind::Condition(c) => {
            let value = match c.value() {
                DValue::Domain(sub) => DValue::Domain(internal(sub)),
                other => other.clone(),
            };
            Domain::condition(c.field_expr(), c.operator(), value)
                .unwrap_or_else(|_| domain.clone())
        }
        DomainKind::Custom(_) => domain.clone(),
    }
}

fn parse_field_domain(model: &ModelSpec, field: &FieldSpec, text: &str) -> Result<Domain> {
    let bad = |why: String| {
        Error::programming(format!(
            "Field {}.{}: invalid domain {text:?}: {why}",
            model.name, field.name
        ))
    };
    let json: erp_fields::serde_json::Value =
        erp_fields::serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
    let items = match &json {
        erp_fields::serde_json::Value::Array(items) => items
            .iter()
            .map(DValue::from_json)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| bad(e.to_string()))?,
        _ => return Err(bad("expected a list".to_owned())),
    };
    let domain = Domain::from_list_internal(&items).map_err(|e| bad(e.to_string()))?;
    Ok(internal(&domain))
}

fn parse_field_context(
    model: &ModelSpec,
    field: &FieldSpec,
    text: &str,
) -> Result<Vec<(String, Value)>> {
    let json: erp_fields::serde_json::Value =
        erp_fields::serde_json::from_str(text).map_err(|e| {
            Error::programming(format!(
                "Field {}.{}: invalid context {text:?}: {e}",
                model.name, field.name
            ))
        })?;
    match json {
        erp_fields::serde_json::Value::Object(map) => Ok(map
            .iter()
            .map(|(k, v)| (k.clone(), json_to_value(v)))
            .collect()),
        _ => Err(Error::programming(format!(
            "Field {}.{}: the context {text:?} is not an object",
            model.name, field.name
        ))),
    }
}

impl Relations {
    /// Index a registry; fails on invalid field domains and contexts.
    pub(crate) fn build(registry: &Registry) -> Result<Self> {
        let mut rel = Self::default();
        let mut m2m_keys: FxHashMap<(String, String, String), Vec<FieldKey>> = FxHashMap::default();
        for model in registry.models() {
            for field in &model.fields {
                let key = (model.id, field.id);
                if let Some(text) = field.domain {
                    rel.domains
                        .insert(key, parse_field_domain(model, field, text)?);
                }
                if let Some(text) = field.context {
                    rel.contexts
                        .insert(key, parse_field_context(model, field, text)?);
                }
                if field.inherited.is_some() {
                    continue;
                }
                // the deletion indexes name tables: a model without one (abstract) has no rows a
                // deletion could reach, and querying it would name a table that does not exist
                let tabled = model.has_table();
                match &field.ty {
                    FieldType::Many2one { comodel, .. } if field.has_column() => {
                        if tabled && let Some(co) = registry.model_id(comodel) {
                            rel.referencing.entry(co).or_default().push(key);
                        }
                    }
                    FieldType::One2many { comodel, inverse } if field.store => {
                        if let Some(co) = registry.model(comodel) {
                            if tabled {
                                rel.x2many_to.entry(co.id).or_default().push(key);
                            }
                            if let Some(inv) = co.field(inverse).filter(|inv| inv.has_column())
                                && let Ok(order) = inverse_first_order(co, inv)
                            {
                                rel.inverse_orders.insert(key, Arc::from(order));
                            }
                            match co.field(inverse).map(|inv| (inv, &inv.ty)) {
                                Some((inv, FieldType::Many2one { .. })) => {
                                    rel.inverses.entry(key).or_default().push((co.id, inv.id));
                                    rel.inverses.entry((co.id, inv.id)).or_default().push(key);
                                }
                                Some((inv, FieldType::Many2oneReference { model_field })) => {
                                    // only the comodel's rows of this model belong to the field
                                    let mf = co.field(model_field).ok_or_else(|| {
                                        Error::programming(format!(
                                            "{}.{}: model field '{model_field}' missing",
                                            co.name, inv.name
                                        ))
                                    })?;
                                    let condition = Domain::condition(
                                        mf.name,
                                        Operator::Eq,
                                        DValue::Str(model.name.to_owned()),
                                    )
                                    .map_err(Error::from)?;
                                    let declared =
                                        rel.domains.get(&key).cloned().unwrap_or(Domain::TRUE);
                                    rel.declared.insert(key, declared.clone());
                                    rel.domains.insert(key, declared & condition);
                                    rel.model_fields.insert(key, mf.name);
                                    for k in [(co.id, inv.id), (co.id, mf.id)] {
                                        rel.reference_inverses.entry(k).or_default().push(key);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    FieldType::Many2many {
                        comodel,
                        relation: Some(r),
                        ..
                    } => {
                        if tabled && let Some(co) = registry.model_id(comodel) {
                            rel.x2many_to.entry(co).or_default().push(key);
                        }
                        m2m_keys
                            .entry((r.table.clone(), r.column1.clone(), r.column2.clone()))
                            .or_default()
                            .push(key);
                    }
                    _ => {}
                }
            }
        }
        for ((table, c1, c2), fields) in &m2m_keys {
            for &(m, f) in fields {
                let siblings: Vec<FieldId> = fields
                    .iter()
                    .filter(|(om, of)| *om == m && *of != f)
                    .map(|(_, of)| *of)
                    .collect();
                if !siblings.is_empty() {
                    rel.siblings.insert((m, f), siblings);
                }
                if let Some(mirrors) = m2m_keys.get(&(table.clone(), c2.clone(), c1.clone())) {
                    rel.inverses
                        .entry((m, f))
                        .or_default()
                        .extend(mirrors.iter().copied());
                }
            }
        }
        rel.deletion_capture = deletion_capture_models(registry);
        rel.follow_deletes = follow_deletes(registry);
        rel.follow_tables = follow_tables(registry, &rel.follow_deletes);
        Ok(rel)
    }

    /// The physical FK effect set for an eventful root, sorted by model name.
    ///
    /// Non-eventful roots have no capture summary. A theoretical graph over the envelope bound
    /// refuses capture here, rather than preventing construction of the whole ORM registry.
    pub(crate) fn deletion_capture_models(&self, model: ModelId) -> Result<&[ModelId]> {
        match self.deletion_capture.get(&model) {
            Some(summary) if summary.overflow => Err(Error::new(
                erp_fields::ErrorKind::ResourceExhausted,
                "outbox CaptureLimit: event exceeds 64 cascade models",
            )),
            Some(summary) => Ok(&summary.models),
            None => Ok(&[]),
        }
    }

    /// What deleting rows of `model` makes the follow-delete triggers do, when anything.
    pub(crate) fn follow_deletes(&self, model: ModelId) -> Option<&FollowEffects> {
        self.follow_deletes.get(&model)
    }

    /// What deleting rows of each table makes the follow-delete triggers do, by table name.
    pub(crate) fn follow_tables(&self) -> &[(String, FollowEffects)] {
        &self.follow_tables
    }

    /// The order of the inverse read of a stored one2many over a stored inverse,
    /// when its comodel order parses.
    pub(crate) fn inverse_order(&self, model: ModelId, field: FieldId) -> Option<&Arc<str>> {
        self.inverse_orders.get(&(model, field))
    }

    /// The field's `domain=` (`TRUE` when it has none).
    pub(crate) fn domain(&self, model: ModelId, field: FieldId) -> Domain {
        self.domains
            .get(&(model, field))
            .cloned()
            .unwrap_or(Domain::TRUE)
    }

    /// The field's declared `domain=` alone (without the model condition of a one2many over a
    /// many2one_reference): what a linked record must satisfy.
    pub(crate) fn declared_domain(&self, model: ModelId, field: FieldId) -> Domain {
        match self.declared.get(&(model, field)) {
            Some(domain) => domain.clone(),
            None => self.domain(model, field),
        }
    }

    /// For a one2many over a many2one_reference: the comodel's model field, which holds the
    /// owner's model name.
    pub(crate) fn model_field(&self, model: ModelId, field: FieldId) -> Option<&'static str> {
        self.model_fields.get(&(model, field)).copied()
    }

    /// The one2many fields over a many2one_reference (or its model field).
    pub(crate) fn reference_inverses(&self, model: ModelId, field: FieldId) -> &[FieldKey] {
        self.reference_inverses
            .get(&(model, field))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The field's `context=`.
    pub(crate) fn context(&self, model: ModelId, field: FieldId) -> &[(String, Value)] {
        self.contexts
            .get(&(model, field))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// `active_test` of the field's context, when it sets one.
    pub(crate) fn active_test(&self, model: ModelId, field: FieldId) -> Option<bool> {
        self.context(model, field)
            .iter()
            .find(|(k, _)| k == "active_test")
            .map(|(_, v)| v.is_truthy())
    }

    /// Inverse fields (REL §4.5).
    pub(crate) fn inverses(&self, model: ModelId, field: FieldId) -> &[FieldKey] {
        self.inverses
            .get(&(model, field))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Sibling many2many fields (same model, relation and columns).
    pub(crate) fn siblings(&self, model: ModelId, field: FieldId) -> &[FieldId] {
        self.siblings
            .get(&(model, field))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Stored many2one fields pointing to `model`.
    pub(crate) fn referencing(&self, model: ModelId) -> &[FieldKey] {
        self.referencing
            .get(&model)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Stored x2many fields whose comodel is `model`.
    pub(crate) fn x2many_to(&self, model: ModelId) -> &[FieldKey] {
        self.x2many_to
            .get(&model)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

/// The comodel of a relational field.
pub(crate) fn comodel_of<'a>(registry: &'a Registry, field: &FieldSpec) -> Result<&'a ModelSpec> {
    let name = field
        .ty
        .comodel()
        .ok_or_else(|| Error::programming(format!("Field {} is not relational", field.name)))?;
    registry
        .model(name)
        .ok_or_else(|| Error::programming(format!("Model '{name}' does not exist in registry.")))
}

/// The answer of [`Records::read_x2many_capped`]: the value when it holds at most the cap, or
/// only the fact that it holds more (never a truncated value).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum CappedIds {
    /// The raw ids of the value, in its order (at most the cap).
    Within(Vec<i64>),
    /// The value holds more than the cap; its ids were not collected.
    OverCap,
}

/// The bound of a capped x2many fetch ([`Records::read_x2many_capped`]): at most `rows` rows
/// (the cap plus one, so that a value over the cap is seen without reading it), only the targets
/// the active test leaves when `active_test` is on.
#[derive(Debug, Clone, Copy)]
struct RowCap {
    rows: u64,
    active_test: bool,
}

impl RowCap {
    /// `domain` with `active in {True}` on the comodel's active field when the active test applies
    /// (ANDed explicitly, whatever the domain says about `active`: the read format hides archived
    /// targets in any case, `x2many_visible`), so that the cap counts the targets the value shows.
    fn restrict(self, comodel: &ModelSpec, domain: Domain) -> Result<Domain> {
        match comodel.active_name {
            Some(active) if self.active_test => {
                let name = comodel.field_by_id(active).name;
                Ok(Domain::condition(name, Operator::In, DValue::set([true]))
                    .map_err(Error::from)?
                    & domain)
            }
            _ => Ok(domain),
        }
    }
}

fn dedup_first(ids: impl IntoIterator<Item = RecId>) -> Vec<RecId> {
    let mut seen = FxHashSet::default();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

fn ids_of(cell: &Cell) -> &[RecId] {
    match cell {
        Cell::Ids(ids) => ids,
        _ => &[],
    }
}

/// The record id of a cached many2one cell: a database record, or a new record when the cell
/// belongs to a new record (statements only ever bind the real ids, `RecId::as_real`).
pub(crate) fn id_of(cell: &Cell) -> Option<RecId> {
    match cell {
        Cell::Id(id) => RecId::from_raw(*id),
        _ => None,
    }
}

/// The records a relational cell points to, borrowed: a many2one's record, an x2many's records,
/// nothing for any other cell. [`ids_of_cell`] without the vector.
fn targets_of(cell: &Cell) -> impl Iterator<Item = RecId> + '_ {
    id_of(cell).into_iter().chain(ids_of(cell).iter().copied())
}

/// The targets of each owner, from `(owner, target)` pairs in query order: one shared slice per
/// owner, the targets in the order the pairs came (a stable sort by owner keeps it), so filling
/// the cache costs one allocation per owner instead of a growing vector and a copy.
fn group_targets(mut pairs: Vec<(RecId, RecId)>) -> FxHashMap<RecId, Arc<[RecId]>> {
    pairs.sort_by_key(|(owner, _)| *owner);
    let mut out: FxHashMap<RecId, Arc<[RecId]>> = FxHashMap::default();
    for run in pairs.chunk_by(|a, b| a.0 == b.0) {
        if let Some((owner, _)) = run.first() {
            out.insert(*owner, run.iter().map(|(_, target)| *target).collect());
        }
    }
    out
}

/// The records a relational cell points to (a many2one's record, an x2many's records).
pub(crate) fn ids_of_cell(cell: &Cell) -> Vec<RecId> {
    match cell {
        Cell::Ids(ids) => ids.to_vec(),
        _ => id_of(cell).into_iter().collect(),
    }
}

/// Which comodel columns a one2many read fetches with the lines (4).
#[derive(Debug, Clone, Copy)]
pub(crate) enum LineColumns<'a> {
    /// The comodel's prefetch columns (plain reads: the lines are usually read next).
    Prefetch,
    /// These comodel fields only (the columns a compute reads through the lines, as
    /// `(comodel, field)` steps of its dependency paths); fields without a column are skipped.
    Only(&'a [FieldKey]),
}

/// The order of a one2many read: the inverse first (its raw column, ascending),
/// then the comodel's order without any term on the inverse. Inside one owner's lines the
/// inverse is constant, so the lines of each owner keep the comodel's order; only the order
/// across owners changes, which grouping by owner discards.
///
/// # Errors
///
/// The `UserError` of an invalid order when the comodel's `_order` does not parse (the same error
/// a search in that order raises).
fn inverse_first_order(comodel: &ModelSpec, inverse: &FieldSpec) -> Result<String> {
    let lead = if matches!(inverse.ty, FieldType::Many2one { .. }) {
        format!("{}.id", inverse.name)
    } else {
        inverse.name.to_owned()
    };
    let base = comodel.order.trim();
    if base.is_empty() {
        return Ok(lead);
    }
    let terms = erp_sql::order::parse_order(base)
        .ok_or_else(|| erp_sql::order::invalid_order_error(base))?;
    let mut order = lead;
    for term in terms.iter().filter(|term| term.field != inverse.name) {
        order.push_str(", ");
        order.push_str(&term.to_string());
    }
    Ok(order)
}

// ----- reading ----------------------------------------------------------------------------------

impl Transaction {
    /// The environment an x2many field reads its comodel with: the record's environment updated
    /// with the field's `context=`.
    pub(crate) fn field_env(&self, env: &Env, model: ModelId, field: FieldId) -> Env {
        let ctx = self.shared.relations.context(model, field);
        if ctx.is_empty() {
            env.clone()
        } else {
            env.with_context(ctx.iter().cloned())
        }
    }

    /// The context stays the field environment's.
    pub(crate) fn command_env(
        &self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        comodel: &ModelSpec,
    ) -> Env {
        let env = self.field_env(env, model, field);
        if comodel.allow_sudo_commands {
            env
        } else {
            env.acting_as(&self.env())
        }
    }

    /// Make the cached value of an x2many field available for `records` (one query per field for
    /// the uncached part of the prefetch set). New records read as empty.
    pub(crate) fn ensure_x2many<'a>(
        &'a mut self,
        records: &'a Records,
        field: FieldId,
    ) -> BoxFuture<'a, Result<()>> {
        self.ensure_x2many_with(records, field, LineColumns::Prefetch)
    }

    /// [`Transaction::ensure_x2many`] fetching `columns` of the one2many lines with them (
    /// D2.4: a compute-driven read asks for its dependency columns only).
    pub(crate) fn ensure_x2many_with<'a>(
        &'a mut self,
        records: &'a Records,
        field: FieldId,
        columns: LineColumns<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let model = records.model_id();
            let registry = Arc::clone(records.env().registry());
            let spec = registry.model_by_id(model);
            let fs = spec.field_by_id(field);
            self.settle_computed_inverse(records, fs).await?;
            let mut seen: FxHashSet<RecId> = FxHashSet::default();
            let mut wanted: Vec<RecId> = records
                .rec_ids()
                .iter()
                .copied()
                .filter(|id| !self.cache.contains(model, field, *id) && seen.insert(*id))
                .collect();
            if wanted.is_empty() {
                return Ok(());
            }
            let (new, real): (Vec<RecId>, Vec<RecId>) =
                wanted.drain(..).partition(|id| id.is_new());
            if !new.is_empty() {
                // the origins' values as new records, plus pending patches (ENV §4.9)
                self.ensure_new_x2many(records, field, &new).await?;
            }
            if real.is_empty() || !fs.store {
                for id in real {
                    self.cache.model_mut(model).set(
                        field,
                        fs.storage(),
                        id,
                        Cell::Ids(Arc::from([])),
                        false,
                    )?;
                }
                return Ok(());
            }
            let mut wanted = real;
            let limit = self.config().prefetch_chunk.max(1).max(wanted.len());
            for id in records.prefetch_ids() {
                if wanted.len() >= limit {
                    break;
                }
                if !id.is_new() && !self.cache.contains(model, field, *id) && seen.insert(*id) {
                    wanted.push(*id);
                }
            }
            let values = match &fs.ty {
                FieldType::One2many { .. } => {
                    self.fetch_o2m(records.env(), spec, fs, &wanted, columns, None)
                        .await?
                }
                FieldType::Many2many { .. } => {
                    self.fetch_m2m(records.env(), spec, fs, &wanted, None)
                        .await?
                }
                _ => return Err(Error::programming(format!("{} is not an x2many", fs.name))),
            };
            let mc = self.cache.model_mut(model);
            let empty: Arc<[RecId]> = Arc::from([]);
            for id in wanted {
                let ids = values
                    .get(&id)
                    .map_or_else(|| Arc::clone(&empty), Arc::clone);
                mc.insert_fetched(field, fs.storage(), id, Cell::Ids(ids))?;
            }
            Ok(())
        })
    }

    /// Nothing for any other field.
    async fn settle_computed_inverse(&mut self, records: &Records, fs: &FieldSpec) -> Result<()> {
        let registry = Arc::clone(records.env().registry());
        if let FieldType::One2many { comodel, inverse } = &fs.ty
            && let Some(co) = registry.model(comodel)
            && let Some(inv) = co.field_id(inverse)
            && self
                .shared
                .graph
                .compute((co.id, inv))
                .is_some_and(|fc| fc.stored)
            && self.cache.model(co.id).is_some_and(|m| m.has_pending(inv))
        {
            let env = records.env().clone();
            self.settle(&env, co.id, inv).await?;
        }
        Ok(())
    }

    /// One2many lines of `ids` (REL §4.6.1): a superuser search on the comodel without active
    /// test, restricted by the field domain; the inverse and active fields are fetched in the same
    /// query, with the prefetch columns or the `columns` asked for.
    ///
    /// Over a stored inverse, the statement reads the comodel ordered by the inverse first, then
    /// by the comodel's order without its inverse terms, with or without a field
    /// domain: within one owner the inverse is constant, so each owner's lines come in the
    /// comodel's order, and a join remains only for an order term on another table (another
    /// many2one, an `_inherits` parent's field). When the id list is then its only literal it is
    /// a cached prepared statement reading the ids behind a sub-select (D2.5). An
    /// inverse without a column (a related many2one) keeps the search in the comodel's order,
    /// planned per call.
    ///
    /// With a [`RowCap`] (the bounded read of [`Records::read_x2many_capped`], one owner) the
    /// statement keeps only the lines the active test leaves when the cap says so and reads at
    /// most `cap.rows` of them, planned per call; without one nothing is limited.
    async fn fetch_o2m(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        field: &FieldSpec,
        ids: &[RecId],
        columns: LineColumns<'_>,
        cap: Option<RowCap>,
    ) -> Result<FxHashMap<RecId, Arc<[RecId]>>> {
        let registry = Arc::clone(env.registry());
        let comodel = comodel_of(&registry, field)?;
        let FieldType::One2many { inverse, .. } = &field.ty else {
            return Ok(FxHashMap::default());
        };
        let inverse_id = comodel.field_id(inverse).ok_or_else(|| {
            Error::programming(format!("no inverse {inverse} on {}", comodel.name))
        })?;
        let inverse_spec = comodel.field_by_id(inverse_id);
        let env = self
            .field_env(env, spec.id, field.id)
            .sudo(true)
            .with_context([("active_test", Value::Bool(false))]);
        let co = Records::empty(env, comodel.id);
        let id_values: Vec<i64> = ids
            .iter()
            .filter_map(|i| i.as_real())
            .map(|i| i.get())
            .collect();
        let field_domain = self.shared.relations.domain(spec.id, field.id);
        let mut domain = Domain::condition(inverse, Operator::In, DValue::list(id_values))
            .map_err(Error::from)?
            & field_domain;
        if let Some(cap) = cap {
            domain = cap.restrict(comodel, domain)?;
        }
        // the inverse and active fields always; then the comodel's prefetch columns, since the
        // lines are usually read next (and a change of their owner's fields needs nothing more,
        // without a column (a related many2one) is read after the search
        let inverse_stored = inverse_spec.has_column();
        let mut fetch: Vec<&str> = Vec::new();
        if inverse_stored {
            fetch.push(inverse_spec.name);
        }
        if let Some(active) = comodel.active_name {
            fetch.push(comodel.field_by_id(active).name);
        }
        let extra: Vec<FieldId> = match columns {
            LineColumns::Prefetch if co.env().context().prefetch_fields() => {
                comodel.prefetch_fields.clone()
            }
            LineColumns::Prefetch => Vec::new(),
            LineColumns::Only(steps) => {
                let mut fields = Vec::with_capacity(steps.len());
                for &(model, f) in steps {
                    if model != comodel.id {
                        return Err(Error::programming(format!(
                            "{}.{}: a dependency column of model {model:?} asked for on the lines \
                             of {}",
                            spec.name, field.name, comodel.name
                        )));
                    }
                    if comodel.field_by_id(f).has_column() {
                        fields.push(f);
                    }
                }
                fields
            }
        };
        for f in extra {
            let name = comodel.field_by_id(f).name;
            if !fetch.contains(&name) {
                fetch.push(name);
            }
        }
        let mut opts = crate::SearchOptions::new()
            .active_test(false)
            .fetch_fields(fetch);
        if inverse_stored {
            let order = match self.shared.relations.inverse_order(spec.id, field.id) {
                Some(order) => order.to_string(),
                None => inverse_first_order(comodel, inverse_spec)?,
            };
            opts = opts.order(order);
        }
        if let Some(cap) = cap {
            opts = opts.limit(cap.rows);
        }
        let (mut query, fetched) = co.search_plan(self, &domain, &opts).await?;
        if query.is_empty() {
            return Ok(FxHashMap::default());
        }
        // decided from the parameters (2): the id list as the only literal, over a
        // stored inverse (a related inverse's search is not the D2 shape); a capped read is
        // planned per call
        if cap.is_none() && inverse_stored && query.filter_params() == 1 {
            query.plan_as_id_list()?;
        }
        let line_ids = query.execute(self, &fetched).await?;
        if line_ids.is_empty() {
            return Ok(FxHashMap::default());
        }
        // `(owner, line)` in the lines' order; a line whose inverse is NULL (pending in the
        // cache) belongs to no record of `ids`
        let mut pairs: Vec<(RecId, RecId)> = Vec::with_capacity(line_ids.len());
        if inverse_stored {
            // the inverse came with every row of the statement: read in place from the cache
            // (design note `perf-stream.md` §2); a row without it is a cacher bug, never a line
            // to drop silently
            let missing = |line: RecId| {
                Error::programming(format!(
                    "line {line:?} of {} returned by the inverse read has no cached {inverse}",
                    comodel.name
                ))
            };
            let cache = self
                .cache
                .model(comodel.id)
                .ok_or_else(|| missing(line_ids[0]))?;
            for line in &line_ids {
                let owner = cache.get(inverse_id, *line).ok_or_else(|| missing(*line))?;
                if let Some(owner) = id_of(&owner) {
                    pairs.push((owner, *line));
                }
            }
        } else {
            let lines = co.browse_ids(line_ids.iter().copied());
            let owners = self.field_cells(&lines, inverse_id).await?;
            for (line, owner) in line_ids.iter().zip(&owners) {
                if let Some(owner) = id_of(owner) {
                    pairs.push((owner, *line));
                }
            }
        }
        Ok(group_targets(pairs))
    }

    /// Many2many targets of `ids` (REL §4.7.5): one query over the comodel joined
    /// with the relation table, restricted by the field domain (superuser, no active test), in the
    /// comodel's order.
    ///
    /// With a [`RowCap`] (the bounded read of [`Records::read_x2many_capped`], one owner) the
    /// statement keeps only the targets the active test leaves when the cap says so and reads at
    /// most `cap.rows` relation rows (`LIMIT`), planned per call; without one nothing is limited
    /// and the cached plan of the id list is kept.
    async fn fetch_m2m(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        field: &FieldSpec,
        ids: &[RecId],
        cap: Option<RowCap>,
    ) -> Result<FxHashMap<RecId, Arc<[RecId]>>> {
        let registry = Arc::clone(env.registry());
        let comodel = comodel_of(&registry, field)?;
        let FieldType::Many2many {
            relation: Some(rel),
            ..
        } = &field.ty
        else {
            return Ok(FxHashMap::default());
        };
        let env = self
            .field_env(env, spec.id, field.id)
            .sudo(true)
            .with_context([("active_test", Value::Bool(false))]);
        let co = Records::empty(env, comodel.id);
        let mut domain = self.shared.relations.domain(spec.id, field.id);
        if let Some(cap) = cap {
            domain = cap.restrict(comodel, domain)?;
        }
        let filter = crate::search::compile_for(self, &co, &domain, false).await?;
        let order = crate::search::order_for(self, &co, "")?;
        let mut flush = filter.fields.clone();
        flush.extend(order.fields.iter().copied());
        crate::search::flush_read_inputs(self, &flush).await?;
        let mut extra: Vec<&str> = Vec::new();
        if let Some(active) = comodel.active_name {
            extra.push(comodel.field_by_id(active).name);
        }
        let mut params = filter.params.clone();
        params.push(erp_sql::param::Param::Array(
            erp_sql::param::ScalarKind::Int8,
            ids.iter()
                .filter_map(|i| i.as_real())
                .map(|i| erp_sql::param::Scalar::Int8(i.get()))
                .collect(),
        ));
        erp_sql::order::check_join_conflicts(&[&filter.join_list, &order.joins])?;
        let joins = crate::search::merge_joins(&[&filter.join_list, &order.joins]);
        let predicate = (filter.constant != Some(true)).then_some(&*filter.sql);
        if filter.constant == Some(false) {
            return Ok(FxHashMap::default());
        }
        let mut sql = erp_sql::relational::select_m2m(
            erp_sql::relational::Relation {
                table: &rel.table,
                column1: &rel.column1,
                column2: &rel.column2,
            },
            &comodel.table,
            &extra,
            &joins,
            predicate,
            &order.sql,
            params.len(),
        );
        // the id list as the only literal: a cached statement that reads it behind a sub-select,
        // so every plan is the generic one (D2.5); with field-domain literals, per
        // call as any domain search; a capped read (an integer LIMIT after the order) per call
        let (sql, policy) = if let Some(cap) = cap {
            sql.push_str(&format!(" LIMIT {}", cap.rows));
            (sql, crate::search::PlanPolicy::PerCall)
        } else if predicate.is_some() && !filter.params.is_empty() {
            (sql, crate::search::PlanPolicy::PerCall)
        } else {
            let blind = erp_sql::select::blind_id_list(&sql).ok_or_else(|| {
                Error::programming(format!(
                    "the many2many read of {}.{} must bind its id list as `= ANY($1::int8[])` \
                     once: {sql}",
                    spec.name, field.name
                ))
            })?;
            (blind, crate::search::PlanPolicy::Cached)
        };
        // the pairs are decoded as the rows arrive (design note `perf-stream.md` §3.1); the
        // comodel's `active` goes to the cache, decoded only when not cached yet (W9)
        let mut pairs: Vec<(RecId, RecId)> = Vec::new();
        // the comodel is saved once in open snapshot layers, on the first row (not on every
        // row, review F5; not at all for an empty answer, review D3); room is made for the
        // comodel records the rows add, not for the pairs (review round 4 D1; pinned by
        // `cache::tests::a_stream_fill_reserves_for_new_records_not_rows`: keep this call on
        // `StreamFill`, never a writer with a pair-count estimate)
        let active = comodel.active_name;
        let mut fill = crate::cache::StreamFill::new(comodel.id);
        self.query_each_with(&sql, &params, policy, |cache, row| {
            let decode = |i: usize| -> Result<RecId> {
                let raw = row.try_get::<i64>(i).map_err(|e| {
                    Error::new(
                        erp_fields::ErrorKind::Database,
                        "could not decode a relation",
                    )
                    .with_source(e)
                })?;
                RecId::from_real(raw).ok_or_else(|| {
                    Error::programming(format!("the relation table returned the id {raw}"))
                })
            };
            let (x, y) = (decode(0)?, decode(1)?);
            if let Some(a) = active {
                fill.fill(cache, y, a, erp_fields::Storage::Bool, || {
                    crate::read::decode(row, 2, erp_fields::ColumnType::Bool)
                })?;
            }
            pairs.push((x, y));
            Ok(())
        })
        .await?;
        Ok(group_targets(pairs))
    }

    /// The record format of an x2many value (REL §4.6.1): `ids` without the lines the active test
    /// or (outside superuser mode, unless `bypass_search_access`) the access policy hides.
    pub(crate) async fn x2many_visible(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<Vec<RecId>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let registry = Arc::clone(env.registry());
        let fs = registry.model_by_id(model).field_by_id(field);
        let comodel = comodel_of(&registry, fs)?;
        let mut out: Vec<RecId> = ids.to_vec();
        let active_test = self
            .shared
            .relations
            .active_test(model, field)
            .unwrap_or_else(|| env.context().active_test());
        if active_test && let Some(active) = comodel.active_name {
            let holder =
                |i: &RecId| -> Option<RecId> { i.as_real().or(i.origin()).map(RecId::real) };
            let missing: Vec<RecId> = {
                let mut seen: FxHashSet<RecId> = FxHashSet::default();
                out.iter()
                    .filter_map(holder)
                    .filter(|h| !self.cache.contains(comodel.id, active, *h) && seen.insert(*h))
                    .collect()
            };
            if !missing.is_empty() {
                let co = Records::from_parts(
                    env.sudo(true),
                    comodel.id,
                    Arc::from(missing.clone()),
                    Arc::from(missing),
                );
                let name = comodel.field_by_id(active).name;
                co.fetch(self, Some(&[name])).await?;
            }
            out.retain(|id| {
                let own = if id.is_new() {
                    self.cache.get(comodel.id, active, *id)
                } else {
                    None
                };
                match own.or_else(|| holder(id).and_then(|h| self.cache.get(comodel.id, active, h)))
                {
                    Some(cell) => matches!(cell, Cell::Bool(true)),
                    None => id.is_new() && id.origin().is_none(),
                }
            });
        }
        if !fs.bypass_search_access {
            out = self.readable_ids(env, comodel.id, &out).await?;
        }
        Ok(out)
    }

    /// A new record with an origin is as readable
    /// as its origin; a new record without origin is kept.
    pub(crate) async fn readable_ids(
        &mut self,
        env: &Env,
        model: ModelId,
        ids: &[RecId],
    ) -> Result<Vec<RecId>> {
        let mut out: Vec<RecId> = ids.to_vec();
        // with none but origin-less new records there is nothing to ask the database
        if env.su() || out.iter().all(|i| i.is_new() && i.origin().is_none()) {
            return Ok(out);
        }
        let mut seen: FxHashSet<erp_fields::RealId> = FxHashSet::default();
        let keys: Vec<erp_fields::RealId> = out
            .iter()
            .filter_map(|i| i.as_real().or(i.origin()))
            .filter(|r| seen.insert(*r))
            .collect();
        // the policy is current before anything is answered from the memo (a membership or
        // policy change of this transaction, a cancelled refresh, an unresolved principal)
        self.prepare_access(env).await?;
        let key = self.readable_key(env, model)?;
        let entry = self.engine.readable.entry(&key);
        if entry.unrestricted {
            return Ok(out);
        }
        let unknown: Vec<erp_fields::RealId> = keys
            .iter()
            .copied()
            .filter(|k| !entry.decisions.contains_key(k))
            .collect();
        if !unknown.is_empty() {
            let registry = Arc::clone(env.registry());
            let spec = registry.model_by_id(model);
            let asked: Arc<[RecId]> = unknown.iter().map(|r| RecId::real(*r)).collect();
            let co = Records::from_parts(env.clone(), model, Arc::clone(&asked), asked);
            let restriction = crate::search::read_restriction(self, &co).await?;
            let Some(filter) = restriction else {
                self.engine.readable.entry(&key).unrestricted = true;
                return Ok(out);
            };
            // the flush may send statements, which end the remembered decisions (this entry
            // included): what is still undecided is worked out again after it, so the query
            // below decides every record the answer needs
            crate::search::flush_read_inputs(self, &filter.fields).await?;
            if filter.volatile {
                // a start set found by a search: asked again each time, never remembered
                let visible =
                    crate::search::fetch_rows(self, spec, &[], &keys, Some(&filter)).await?;
                out.retain(|id| match id.as_real().or(id.origin()) {
                    Some(r) => visible.contains(&RecId::real(r)),
                    None => true,
                });
                return Ok(out);
            }
            let entry = self.engine.readable.entry(&key);
            let to_ask: Vec<erp_fields::RealId> = keys
                .iter()
                .copied()
                .filter(|k| !entry.decisions.contains_key(k))
                .collect();
            if !to_ask.is_empty() {
                let visible =
                    crate::search::fetch_rows(self, spec, &[], &to_ask, Some(&filter)).await?;
                let entry = self.engine.readable.entry(&key);
                entry.inputs.extend(filter.fields.iter().copied());
                for k in &to_ask {
                    entry
                        .decisions
                        .insert(*k, visible.contains(&RecId::real(*k)));
                }
            }
        }
        let entry = self.engine.readable.entry(&key);
        debug_assert!(
            keys.iter().all(|k| entry.decisions.contains_key(k)),
            "every asked record has a decision"
        );
        out.retain(|id| match id.as_real().or(id.origin()) {
            // fail closed: a record without a decision (the debug_assert above says there is none
            // such) is hidden, never shown; hiding is the safe direction for a read restriction
            Some(r) => entry.decisions.get(&r).copied().unwrap_or(false),
            None => true,
        });
        Ok(out)
    }

    /// Remember what a query over the read restriction found: of `asked` (database ids of
    /// `model`), `visible` are readable by `env` and the others are not (hidden or deleted).
    /// The query ran after the flush of the fields `inputs`, and no statement came after it.
    ///
    /// # Errors
    ///
    /// The binding's refusals (see [`Transaction::readable_key`]).
    pub(crate) fn remember_readable(
        &mut self,
        env: &Env,
        model: ModelId,
        asked: &[erp_fields::RealId],
        visible: &FxHashSet<RecId>,
        inputs: &[(ModelId, FieldId)],
    ) -> Result<()> {
        let key = self.readable_key(env, model)?;
        let entry = self.engine.readable.entry(&key);
        entry.inputs.extend(inputs.iter().copied());
        for k in asked {
            entry
                .decisions
                .insert(*k, visible.contains(&RecId::real(*k)));
        }
        Ok(())
    }

    /// The key of the visibility memo for `env` and `model`: the effective binding (the
    /// transaction's newest resolution, not the environment's), so an older `Env` clone is never
    /// answered with decisions made for other groups.
    ///
    /// # Errors
    ///
    /// The binding's refusals: a cancelled transaction, an elevation block used outside its
    /// block, a stale policy, an unresolved principal.
    pub(crate) fn readable_key(&self, env: &Env, model: ModelId) -> Result<crate::engine::MemoKey> {
        let binding = self.binding(env)?;
        Ok(crate::engine::MemoKey::new(
            binding.key(self.policy().snapshot().id()),
            model,
        ))
    }

    /// Whether the visibility of some of `ids` (targets of x2many `field` of `model`) is still to
    /// be decided for `env`: an `active` flag not cached, or a record rule not asked yet.
    fn visibility_undecided(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<bool> {
        let registry = Arc::clone(env.registry());
        let fs = registry.model_by_id(model).field_by_id(field);
        let comodel = comodel_of(&registry, fs)?;
        let active_test = self
            .shared
            .relations
            .active_test(model, field)
            .unwrap_or_else(|| env.context().active_test());
        let holder = |i: &RecId| i.as_real().or(i.origin());
        if active_test && let Some(active) = comodel.active_name {
            for id in ids {
                if let Some(h) = holder(id)
                    && !self.cache.contains(comodel.id, active, RecId::real(h))
                {
                    return Ok(true);
                }
            }
        }
        if env.su() || fs.bypass_search_access {
            return Ok(false);
        }
        let key = self.readable_key(env, comodel.id)?;
        let entry = self.engine.readable.entry(&key);
        if entry.unrestricted {
            return Ok(false);
        }
        Ok(ids
            .iter()
            .filter_map(holder)
            .any(|h| !entry.decisions.contains_key(&h)))
    }

    /// The record-format value of an x2many field per record: the visible targets, decided once
    /// for the union of the records' targets (one active and one access query at most).
    pub(crate) async fn x2many_cells(
        &mut self,
        records: &Records,
        field: FieldId,
    ) -> Result<Vec<Cell>> {
        self.ensure_x2many(records, field).await?;
        let model = records.model_id();
        let env = records.env().clone();
        let raw: Vec<Cell> = records
            .rec_ids()
            .iter()
            .map(|id| self.cache.get(model, field, *id).unwrap_or(Cell::Null))
            .collect();
        let mut seen: FxHashSet<RecId> = FxHashSet::default();
        let mut union: Vec<RecId> = raw
            .iter()
            .flat_map(|c| ids_of(c).iter().copied())
            .filter(|id| seen.insert(*id))
            .collect();
        // Reading the value of one record at a time (a snapshot of each line in turn) must not
        // ask the database once per record: when some target is still undecided, the targets of
        // the cached values of the whole prefetch set are decided with it (and remembered), then
        // those of every other record whose value is cached, at most one prefetch chunk of them
        if !env.su() {
            self.prepare_access(&env).await?;
        }
        if self.visibility_undecided(&env, model, field, &union)? {
            let limit = self.config().prefetch_chunk.max(1);
            let own: FxHashSet<RecId> = records.rec_ids().iter().copied().collect();
            let mut taken: usize = 0;
            let mut widen = |ids: &[RecId], union: &mut Vec<RecId>, taken: &mut usize| {
                for t in ids {
                    if *taken >= limit {
                        break;
                    }
                    if seen.insert(*t) {
                        union.push(*t);
                        *taken += 1;
                    }
                }
            };
            for id in records.prefetch_ids() {
                if taken >= limit {
                    break;
                }
                if own.contains(id) {
                    continue;
                }
                if let Some(Cell::Ids(ids)) = self.cache.get(model, field, *id) {
                    widen(&ids, &mut union, &mut taken);
                }
            }
            if taken < limit
                && let Some(mc) = self.cache.model(model)
            {
                for (id, cell) in mc.cached_values(field) {
                    if taken >= limit {
                        break;
                    }
                    if !own.contains(&id)
                        && let Cell::Ids(ids) = &cell
                    {
                        widen(ids, &mut union, &mut taken);
                    }
                }
            }
        }
        let visible: FxHashSet<RecId> = self
            .x2many_visible(&env, model, field, &union)
            .await?
            .into_iter()
            .collect();
        // a value whose targets are all visible is shared, not copied (one list per record
        // otherwise); empty values share one empty list
        let empty: Arc<[RecId]> = Arc::from(Vec::new());
        Ok(raw
            .iter()
            .map(|c| match c {
                Cell::Ids(ids) if ids.iter().all(|id| visible.contains(id)) => {
                    Cell::Ids(Arc::clone(ids))
                }
                _ => {
                    let kept: Vec<RecId> = ids_of(c)
                        .iter()
                        .copied()
                        .filter(|id| visible.contains(id))
                        .collect();
                    if kept.is_empty() {
                        Cell::Ids(Arc::clone(&empty))
                    } else {
                        Cell::Ids(kept.into())
                    }
                }
            })
            .collect())
    }
}

impl Records {
    /// The records reached through a relational field (`records.partner_id`, `records.line_ids`):
    /// for several records the union of their values, without duplicates, in first-seen order
    /// , in the same environment. The result's prefetch set holds the
    /// values of the same field over this recordset's prefetch set.
    pub async fn related(&self, tx: &mut Transaction, field: &str) -> Result<Records> {
        let registry = Arc::clone(self.env().registry());
        let spec = registry.model_by_id(self.model_id());
        let fs = spec.field(field).ok_or_else(|| {
            Error::programming(format!("Invalid field '{field}' on '{}'", spec.name))
        })?;
        let comodel = comodel_of(&registry, fs)?;
        let cells = self.cells(tx, field).await?;
        let ids: Vec<RecId> = dedup_first(cells.iter().flat_map(targets_of));
        // prefetch: the cached values over the prefetch set (raw values; filtered on read)
        let model = self.model_id();
        let mut prefetch: Vec<RecId> = ids.clone();
        let mut seen: FxHashSet<RecId> = ids.iter().copied().collect();
        for id in self.prefetch_ids() {
            if let Some(cell) = tx.cache.get(model, fs.id, *id) {
                for m in targets_of(&cell) {
                    if seen.insert(m) {
                        prefetch.push(m);
                    }
                }
            }
        }
        Ok(Records::from_parts(
            self.env().clone(),
            comodel.id,
            Arc::from(ids),
            Arc::from(prefetch),
        ))
    }

    pub async fn reference(&self, tx: &mut Transaction, field: &str) -> Result<Option<Records>> {
        let cell = self.cell(tx, field).await?;
        let Cell::Str(text) = cell else {
            return Ok(None);
        };
        let Some((model, id)) = erp_fields::convert::parse_reference(&text) else {
            return Ok(None);
        };
        let records = self.env().records(model)?;
        Ok(Some(records.browse([id])?))
    }

    /// The raw ids of x2many `field` of this singleton when it holds at most `cap` of them, else
    /// [`CappedIds::OverCap`]: the bounded read of data expressions, whose recordsets have a size
    /// limit (`erp_expr::MAX_SEARCH_RESULTS`; review EXPR-CLOSE-1).
    ///
    /// An accepted value is the one `read_with(&[field], Load::Raw)` gives in the same
    /// environment: the field's domain, the active test (the field's `context`, else the
    /// environment's), the order (a one2many's comodel order with its inverse first or its
    /// configured inverse order, a many2many's comodel order), the pending writes the normal read
    /// flushes (the statement's predicate and order fields, a stored computed inverse settled
    /// first).
    ///
    /// Bounded admission: a stored, non-computed value not in the cache is read with one statement
    /// that applies the active test and the field domain in SQL and reads at most `cap + 1` rows;
    /// `cap + 1` rows give `OverCap`. That value is not cached (only a whole value is); the
    /// comodel columns of the rows read (at most `cap + 1`) are. A value already in the cache,
    /// including a pending one, is answered from it, as the normal read does. A computed,
    /// related, inherited or non-stored x2many, or a new record, is read whole through the normal
    /// path and then counted (its value is materialized by its compute or origin anyway).
    ///
    /// # Errors
    /// `Access` outside superuser mode; `Programming` for a recordset that is not one record, an
    /// unknown field, a field that is not an x2many, or a cap of `u64::MAX` or more; a database
    /// error.
    pub async fn read_x2many_capped(
        &self,
        tx: &mut Transaction,
        field: &str,
        cap: usize,
    ) -> Result<CappedIds> {
        const CALL: &str = "Records::read_x2many_capped";
        if !self.env().su() {
            return Err(Error::access(format!(
                "{CALL}: only a superuser-mode environment may read a capped x2many value (this \
                 one acts as user {})",
                self.env().uid()
            )));
        }
        let [id] = self.rec_ids() else {
            return Err(Error::programming(format!(
                "{CALL}: expected a singleton, got {self}"
            )));
        };
        let id = *id;
        let registry = Arc::clone(self.env().registry());
        let model = self.model_id();
        let spec = registry.model_by_id(model);
        let fs = crate::read::field_of(spec, field)?;
        if !fs.ty.is_x2many() {
            return Err(Error::programming(format!(
                "{CALL}: {}.{} is not a one2many or many2many field",
                spec.name, fs.name
            )));
        }
        let rows = u64::try_from(cap)
            .ok()
            .and_then(|c| c.checked_add(1))
            .ok_or_else(|| Error::programming(format!("{CALL}: the cap {cap} is too large")))?;
        tx.field_read_checks(self, fs.id).await?;
        tx.settle_computed_inverse(self, fs).await?;
        let own_path = fs.compute.is_none() && fs.related.is_none() && fs.inherited.is_none();
        if !own_path || !fs.store || id.is_new() || tx.cache.contains(model, fs.id, id) {
            let cells = tx.field_cells(self, fs.id).await?;
            let ids = cells.first().map_or(&[][..], ids_of);
            if ids.len() > cap {
                return Ok(CappedIds::OverCap);
            }
            return Ok(CappedIds::Within(ids.iter().map(|i| i.get()).collect()));
        }
        let active_test = tx
            .shared
            .relations
            .active_test(model, fs.id)
            .unwrap_or_else(|| self.env().context().active_test());
        let bound = Some(RowCap { rows, active_test });
        let mut values = match &fs.ty {
            FieldType::One2many { .. } => {
                tx.fetch_o2m(self.env(), spec, fs, &[id], LineColumns::Only(&[]), bound)
                    .await?
            }
            FieldType::Many2many { .. } => tx.fetch_m2m(self.env(), spec, fs, &[id], bound).await?,
            _ => {
                return Err(Error::programming(format!(
                    "{CALL}: {} is not an x2many",
                    fs.name
                )));
            }
        };
        let targets = values.remove(&id).unwrap_or_else(|| Arc::from([]));
        if targets.len() > cap {
            return Ok(CappedIds::OverCap);
        }
        // the statement applied the active test; the read format's filter (superuser mode: the
        // active test only) is still the one that decides, on the active values just fetched
        let visible = tx
            .x2many_visible(self.env(), model, fs.id, &targets)
            .await?;
        Ok(CappedIds::Within(visible.iter().map(|i| i.get()).collect()))
    }

    /// `records.field = value` with a recordset (one `write` on the whole set): a many2one takes a
    /// recordset of its comodel with at most one record (empty: no value), an x2many any
    /// recordset of its comodel (`SET`).
    pub async fn assign(&self, tx: &mut Transaction, field: &str, value: &Records) -> Result<()> {
        let registry = Arc::clone(self.env().registry());
        let spec = registry.model_by_id(self.model_id());
        let fs = spec.field(field).ok_or_else(|| {
            Error::programming(format!("Invalid field '{field}' in '{}'", spec.name))
        })?;
        let wrong = || {
            Error::programming(format!(
                "Wrong value for {}.{}: {value}",
                spec.name, fs.name
            ))
        };
        let comodel = comodel_of(&registry, fs).map_err(|_| wrong())?;
        if value.model_id() != comodel.id || !Arc::ptr_eq(value.env().registry(), &registry) {
            return Err(wrong());
        }
        let vals = match &fs.ty {
            FieldType::Many2one { .. } => {
                let cell = match value.rec_ids() {
                    [] => Cell::Null,
                    [one] => Cell::Id(one.get()),
                    _ => return Err(wrong()),
                };
                crate::vals::RawVals::new().set_cell(fs.name, cell)
            }
            _ => crate::vals::RawVals::new().set_commands(
                fs.name,
                [crate::commands::Command::Set(value.rec_ids().to_vec())],
            ),
        };
        self.write(tx, vals).await
    }

    pub fn as_query(&self) -> DValue {
        crate::prepare::RecordsQuery::value(self.model_id(), Some(self.ids().into_iter().collect()))
    }
}

// ----- many2one assignment and the inverse one2many caches --------------------------------------

impl Transaction {
    /// Whether a one2many over many2one `field` has computed dependents (its old values must be
    /// known before a write).
    pub(crate) fn inverse_has_dependents(&self, model: ModelId, field: FieldId) -> bool {
        self.shared
            .relations
            .inverses(model, field)
            .iter()
            .any(|(m, f)| self.shared.graph.has_dependents((*m, *f)))
    }

    /// The cached many2one values of `ids` before an assignment (for [`Self::update_inverses`]).
    pub(crate) fn old_values(
        &self,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Vec<Option<Cell>> {
        ids.iter()
            .map(|id| self.cache.get(model, field, *id))
            .collect()
    }

    /// After many2one `field` of `records` became `new` (REL §4.4.4): remove the records from the
    /// cached one2many values of their old targets, append them to the new target's when it is
    /// cached and they satisfy the one2many's domain. The dependents of those one2many fields are
    /// notified for the old and new targets (the caller reads the old values first when such
    /// dependents exist, so that `old` is complete).
    pub(crate) async fn update_inverses(
        &mut self,
        model: ModelId,
        field: FieldId,
        records: &Records,
        old: &[Option<Cell>],
        new: &Cell,
    ) -> Result<()> {
        let inverses: Vec<FieldKey> = self.shared.relations.inverses(model, field).to_vec();
        if inverses.is_empty() {
            return Ok(());
        }
        let ids: Vec<RecId> = records.rec_ids().to_vec();
        let id_set: FxHashSet<RecId> = ids.iter().copied().collect();
        let registry = Arc::clone(records.env().registry());
        for (co_model, o2m) in inverses {
            let storage = registry.model_by_id(co_model).field_by_id(o2m).storage();
            // remove from the old targets (all cached values when an old value is unknown): the
            // known old targets are looked up, not found by scanning every cached value, so
            // relinking lines owner by owner (nested onchange lines) stays linear
            let unknown = old.iter().any(Option::is_none);
            // the old targets in first-seen order (a hash set only for membership), so that the
            // order of the notifications below does not depend on hashing
            let mut old_set: FxHashSet<RecId> = FxHashSet::default();
            let olds: Vec<RecId> = old
                .iter()
                .flatten()
                // a cleared (non-id) old cell names no old target
                .filter_map(id_of)
                .filter(|t| old_set.insert(*t))
                .collect();
            let entries: Vec<(RecId, Cell)> = if unknown {
                self.cache
                    .model(co_model)
                    .map(|m| m.cached_values(o2m))
                    // a model with nothing cached has no value to remove the records from
                    .unwrap_or_default()
            } else {
                // an old target without a cached value has nothing to remove the records from
                olds.iter()
                    .filter_map(|t| self.cache.get(co_model, o2m, *t).map(|cell| (*t, cell)))
                    .collect()
            };
            let mc = self.cache.model_mut(co_model);
            for (coid, cell) in entries {
                let current = ids_of(&cell);
                if current.iter().any(|i| id_set.contains(i)) {
                    let kept: Vec<RecId> = current
                        .iter()
                        .copied()
                        .filter(|i| !id_set.contains(i))
                        .collect();
                    mc.set(o2m, storage, coid, Cell::Ids(kept.into()), false)?;
                }
            }
            // dependents of the one2many (a stored count of lines)
            if self.shared.graph.has_dependents((co_model, o2m)) {
                let mut targets: Vec<RecId> = olds.clone();
                if let Some(target) = id_of(new)
                    && !old_set.contains(&target)
                {
                    targets.push(target);
                }
                self.modified(co_model, vec![(o2m, targets)]).await?;
            }
            // add to the new target's cached value; a new target that is not cached yet gets a
            // patch, applied when its value is read (its origin's lines plus these). New records
            // never enter a database record's cached value.
            let Some(target) = id_of(new) else { continue };
            if !target.is_new() && ids.iter().any(|i| i.is_new()) {
                continue;
            }
            let Some(current) = self.cache.get(co_model, o2m, target) else {
                if target.is_new() {
                    let domain = self.shared.relations.domain(co_model, o2m);
                    let valid = self.filter_domain(records, &domain).await?;
                    self.patch_new_x2many(co_model, o2m, target, &valid)?;
                }
                continue;
            };
            let domain = self.shared.relations.domain(co_model, o2m);
            let valid = self.filter_domain(records, &domain).await?;
            let mut value: Vec<RecId> = ids_of(&current).to_vec();
            // membership by hash: the value may already hold every record (a form's lines,
            // linked to it right after its value was set), so a scan per record is quadratic
            let mut present: FxHashSet<RecId> = value.iter().copied().collect();
            for id in valid {
                if present.insert(id) {
                    value.push(id);
                }
            }
            self.cache.model_mut(co_model).set(
                o2m,
                storage,
                target,
                Cell::Ids(value.into()),
                false,
            )?;
        }
        Ok(())
    }
}

// ----- in-memory domain evaluation --------------------------------------------------------------

impl Transaction {
    /// The records of `records` satisfying `domain`, evaluated in memory as superuser without
    /// active test (`filtered_domain`: the fields the domain reads are loaded once for the whole
    /// recordset).
    pub(crate) async fn filter_domain(
        &mut self,
        records: &Records,
        domain: &Domain,
    ) -> Result<Vec<RecId>> {
        if domain.is_true() || records.is_empty() {
            return Ok(records.rec_ids().to_vec());
        }
        let su = records.sudo(true);
        let kept = crate::predicate::filter_records(self, &su, domain).await?;
        Ok(kept.rec_ids().to_vec())
    }
}

/// Physical FK effects only: the schema installs FKs for stored, non-inherited many2one
/// columns of generic tables, excluding company-dependent JSON relationships. Restrict edges
/// have no effect to capture; set-null owners are affected but do not propagate deletion.
fn deletion_capture_models(registry: &Registry) -> FxHashMap<ModelId, DeletionCaptureModels> {
    use erp_fields::OnDelete;

    use crate::outbox::MAX_CASCADE_MODELS;

    let mut inbound: FxHashMap<ModelId, Vec<(ModelId, OnDelete)>> = FxHashMap::default();
    for owner in registry
        .models()
        .iter()
        .filter(|model| model.has_generic_table())
    {
        for field in owner.column_fields() {
            if field.inherited.is_some() || field.company_dependent {
                continue;
            }
            let FieldType::Many2one { comodel, ondelete } = &field.ty else {
                continue;
            };
            if *ondelete == OnDelete::Restrict {
                continue;
            }
            if let Some(target) = registry.model(comodel).filter(|model| model.has_table()) {
                inbound
                    .entry(target.id)
                    .or_default()
                    .push((owner.id, *ondelete));
            }
        }
    }

    let mut summaries = FxHashMap::default();
    for root in registry.models().iter().filter(|model| model.eventful) {
        let mut summary = DeletionCaptureModels::default();
        let mut affected = FxHashSet::default();
        // A model first reached by SetNull may later be reached by Cascade. Track cascade
        // traversal separately from affected membership so its children are still included.
        let mut traversed = FxHashSet::default();
        traversed.insert(root.id);
        let mut pending = vec![root.id];
        'walk: while let Some(deleted) = pending.pop() {
            let Some(edges) = inbound.get(&deleted) else {
                continue;
            };
            for &(owner, ondelete) in edges {
                if !affected.contains(&owner) {
                    if summary.models.len() == MAX_CASCADE_MODELS {
                        summary.overflow = true;
                        break 'walk;
                    }
                    affected.insert(owner);
                    summary.models.push(owner);
                }
                if ondelete == OnDelete::Cascade && traversed.insert(owner) {
                    pending.push(owner);
                }
            }
        }
        summary
            .models
            .sort_unstable_by_key(|id| registry.model_by_id(*id).name);
        summaries.insert(root.id, summary);
    }
    summaries
}

/// For every model, the effects of the follow-delete triggers its deletion fires: the triggers on
/// its own table and on the tables whose rows a foreign key deletes with it (`ondelete =
/// "cascade"` many2one fields, followed transitively), and the triggers on the relation tables of
/// many2many fields whose comodel it is, or whose owner it is (the relation's rows are deleted
/// with either side).
fn follow_deletes(registry: &Registry) -> FxHashMap<ModelId, FollowEffects> {
    use erp_registry::{TableObjectKind, TriggerTable};
    let mut out: FxHashMap<ModelId, FollowEffects> = FxHashMap::default();
    for owner in registry.models() {
        for obj in &owner.table_objects {
            let TableObjectKind::FollowDelete(decl) = obj.kind else {
                continue;
            };
            let key_model = decl.key_table.and_then(|table| {
                registry
                    .models()
                    .iter()
                    .find(|m| m.has_table() && m.table == table)
                    .map(|m| m.id)
            });
            // the tables this trigger is on, as the models whose deletion removes their rows
            let mut seeds: Vec<ModelId> = vec![owner.id];
            if let TriggerTable::Relation(field) = decl.table
                && let Some(FieldType::Many2many { comodel, .. }) =
                    owner.field(field).map(|f| &f.ty)
                && let Some(co) = registry.model_id(comodel)
            {
                seeds.push(co);
            }
            // every model whose deletion reaches a seed through cascading many2one fields
            let mut reached: FxHashSet<ModelId> = FxHashSet::default();
            while let Some(m) = seeds.pop() {
                if !reached.insert(m) {
                    continue;
                }
                for field in &registry.model_by_id(m).fields {
                    if let FieldType::Many2one {
                        comodel,
                        ondelete: erp_fields::OnDelete::Cascade,
                    } = &field.ty
                        && field.has_column()
                        && let Some(target) = registry.model_id(comodel)
                    {
                        seeds.push(target);
                    }
                }
            }
            for m in reached {
                let effects = out.entry(m).or_default();
                effects.caches.push(decl.cache);
                effects.key_models.extend(key_model);
            }
        }
    }
    for effects in out.values_mut() {
        effects.caches.sort_unstable();
        effects.caches.dedup();
        effects.key_models.sort_unstable();
        effects.key_models.dedup();
    }
    out
}

/// [`Relations::follow_tables`]: the tables of the models of `by_model` and the relation tables
/// of relation triggers, each once.
fn follow_tables(
    registry: &Registry,
    by_model: &FxHashMap<ModelId, FollowEffects>,
) -> Vec<(String, FollowEffects)> {
    use erp_registry::{TableObjectKind, TriggerTable};
    let mut out: Vec<(String, FollowEffects)> = Vec::new();
    let mut add = |table: &str, caches: &[&'static str], key_models: &[ModelId]| {
        let at = match out.iter().position(|(t, _)| t == table) {
            Some(at) => at,
            None => {
                out.push((table.to_owned(), FollowEffects::default()));
                out.len() - 1
            }
        };
        let effects = &mut out[at].1;
        effects.caches.extend_from_slice(caches);
        effects.key_models.extend_from_slice(key_models);
    };
    for (model, effects) in by_model {
        let spec = registry.model_by_id(*model);
        if spec.has_table() {
            add(&spec.table, &effects.caches, &effects.key_models);
        }
    }
    for owner in registry.models() {
        for obj in &owner.table_objects {
            if let TableObjectKind::FollowDelete(decl) = obj.kind
                && let TriggerTable::Relation(_) = decl.table
                // checked when the registry was built (`follow_delete_rules`)
                && let Ok(table) = owner.follow_delete_table(&decl)
            {
                add(table, &[decl.cache], &[]);
            }
        }
    }
    for (_, effects) in &mut out {
        effects.caches.sort_unstable();
        effects.caches.dedup();
        effects.key_models.sort_unstable();
        effects.key_models.dedup();
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use erp_fields::{ErrorKind, OnDelete};
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, ModelKind, RegistryBuilder};

    fn capture_registry(models: Vec<ModelDecl>) -> Registry {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "capture_meta",
            depends: &["base"],
            exact_only: false,
        };
        let declarations: Vec<&'static ModelDecl> = models
            .into_iter()
            .map(|model| &*Box::leak(Box::new(model)))
            .collect();
        RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls(declarations)
            .build()
            .expect("valid cascade fixture")
    }

    fn capture_model(name: &'static str, fields: Vec<FieldDecl>) -> ModelDecl {
        ModelDecl::new("capture_meta", name).fields(Vec::leak(fields))
    }

    fn capture_names<'a>(
        registry: &'a Registry,
        relations: &Relations,
        root: &str,
    ) -> Vec<&'a str> {
        relations
            .deletion_capture_models(registry.model_id(root).unwrap())
            .unwrap()
            .iter()
            .map(|id| registry.model_by_id(*id).name)
            .collect()
    }

    #[test]
    fn deletion_capture_follows_only_physical_cascade_paths() {
        let registry = capture_registry(vec![
            capture_model("capture.root", vec![]).eventful(true),
            capture_model(
                "capture.z",
                vec![
                    FieldDecl::many2one("root_id", "capture.root").ondelete(OnDelete::Cascade),
                    FieldDecl::many2one("also_root_id", "capture.root").ondelete(OnDelete::Cascade),
                ],
            ),
            capture_model(
                "capture.a",
                vec![FieldDecl::many2one("z_id", "capture.z").ondelete(OnDelete::Cascade)],
            ),
            capture_model(
                "capture.null",
                vec![FieldDecl::many2one("z_id", "capture.z").ondelete(OnDelete::SetNull)],
            ),
            capture_model(
                "capture.after_null",
                vec![FieldDecl::many2one("null_id", "capture.null").ondelete(OnDelete::Cascade)],
            ),
            capture_model(
                "capture.restrict",
                vec![FieldDecl::many2one("root_id", "capture.root").ondelete(OnDelete::Restrict)],
            ),
            capture_model(
                "capture.after_restrict",
                vec![
                    FieldDecl::many2one("restrict_id", "capture.restrict")
                        .ondelete(OnDelete::Cascade),
                ],
            ),
            capture_model(
                "capture.json",
                vec![
                    FieldDecl::many2one("root_id", "capture.root")
                        .company_dependent(true)
                        .ondelete(OnDelete::SetNull),
                ],
            ),
            capture_model(
                "capture.virtual",
                vec![
                    FieldDecl::many2one("root_id", "capture.root")
                        .store(false)
                        .ondelete(OnDelete::Cascade),
                ],
            ),
            capture_model(
                "capture.abstract",
                vec![FieldDecl::many2one("root_id", "capture.root").ondelete(OnDelete::Cascade)],
            )
            .kind(ModelKind::Abstract),
        ]);
        let relations = Relations::build(&registry).unwrap();
        assert_eq!(
            capture_names(&registry, &relations, "capture.root"),
            ["capture.a", "capture.null", "capture.z"]
        );
        assert!(capture_names(&registry, &relations, "capture.a").is_empty());
    }

    #[test]
    fn deletion_capture_retains_root_reached_by_cycles_and_rebuilds() {
        let registry = capture_registry(vec![
            capture_model(
                "capture.root",
                vec![
                    FieldDecl::many2one("self_id", "capture.root").ondelete(OnDelete::Cascade),
                    FieldDecl::many2one("child_id", "capture.child").ondelete(OnDelete::Cascade),
                ],
            )
            .eventful(true),
            capture_model(
                "capture.child",
                vec![FieldDecl::many2one("root_id", "capture.root").ondelete(OnDelete::Cascade)],
            ),
        ]);
        let relations = Relations::build(&registry).unwrap();
        assert_eq!(
            capture_names(&registry, &relations, "capture.root"),
            ["capture.child", "capture.root"]
        );
        let rebuilt = capture_registry(vec![capture_model("capture.root", vec![]).eventful(true)]);
        assert!(
            capture_names(
                &rebuilt,
                &Relations::build(&rebuilt).unwrap(),
                "capture.root"
            )
            .is_empty()
        );
        assert_eq!(
            capture_names(&registry, &relations, "capture.root"),
            ["capture.child", "capture.root"]
        );
    }

    fn wide_capture_registry(count: usize, eventful: bool) -> Registry {
        let mut models = vec![capture_model("capture.root", vec![]).eventful(eventful)];
        for index in 0..count {
            let name = Box::leak(format!("capture.child_{index:03}").into_boxed_str());
            models.push(capture_model(
                name,
                vec![FieldDecl::many2one("root_id", "capture.root").ondelete(OnDelete::Cascade)],
            ));
        }
        capture_registry(models)
    }

    #[test]
    fn deletion_capture_admits_64_and_refuses_65_without_poisoning_registry_build() {
        let registry = wide_capture_registry(64, true);
        let relations = Relations::build(&registry).unwrap();
        let names = capture_names(&registry, &relations, "capture.root");
        assert_eq!(names.len(), 64);
        assert_eq!(names.first(), Some(&"capture.child_000"));
        assert_eq!(names.last(), Some(&"capture.child_063"));
        let registry = wide_capture_registry(65, true);
        let relations = Relations::build(&registry).unwrap();
        let error = relations
            .deletion_capture_models(registry.model_id("capture.root").unwrap())
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
        assert_eq!(
            error.message(),
            "outbox CaptureLimit: event exceeds 64 cascade models"
        );
        let registry = wide_capture_registry(65, false);
        let relations = Relations::build(&registry).unwrap();
        assert!(capture_names(&registry, &relations, "capture.root").is_empty());
    }

    fn id(raw: i64) -> RecId {
        RecId::from_raw(raw).expect("a record id")
    }

    /// The x2many cache fill keeps each owner's targets in query order and shares nothing
    /// between owners (first-seen order).
    #[test]
    fn grouped_targets_keep_the_query_order_per_owner() {
        let pairs = vec![
            (id(7), id(30)),
            (id(2), id(10)),
            (id(7), id(5)),
            (id(2), id(40)),
            (id(7), id(20)),
        ];
        let grouped = group_targets(pairs);
        assert_eq!(grouped.len(), 2);
        assert_eq!(&*grouped[&id(7)], &[id(30), id(5), id(20)]);
        assert_eq!(&*grouped[&id(2)], &[id(10), id(40)]);
        assert!(group_targets(Vec::new()).is_empty());
    }

    #[test]
    fn targets_of_reads_many2one_and_x2many_cells_only() {
        let ids: Vec<RecId> = targets_of(&Cell::Id(3)).collect();
        assert_eq!(ids, [id(3)]);
        let ids: Vec<RecId> = targets_of(&Cell::Ids(Arc::from([id(4), id(1)]))).collect();
        assert_eq!(ids, [id(4), id(1)]);
        assert_eq!(targets_of(&Cell::Null).count(), 0);
        assert_eq!(targets_of(&Cell::Id(0)).count(), 0);
    }
}
