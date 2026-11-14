//! Company consistency (`_check_company`, CRUD §4.9, REL §4.12,
//! ).
//!
//! On a model whose `check_company_auto` is on (declared, or by default on a company-scoped model
//! with a checked field), `create` checks every checked relational field and `write` the written
//! ones (all of them when `company_id`/`company_ids` is written). The targets of a field (archived
//! ones included) must satisfy the comodel's company match for the record's companies, or for the
//! current company when the field is company-dependent.
//!
//! The check is an integrity rule, not an access decision: it reads company ids without access
//! rules and runs under every elevation.
//!
//! # Cost
//!
//! The check decides from the transaction cache and reads **only what the cache lacks** in one
//! statement per call (a `UNION ALL` of id selections, parameters only; one statement per
//! [`COMPANY_CHECK_CHUNK`] distinct ids on huge batches): the records' companies, the targets'
//! companies, the links of many2many fields that are not cached, and the parent paths a
//! `parent_of` match needs. When everything is cached it costs nothing. A one2many field, or a
//! many2many with a field domain, that is not cached is read through the x2many cache first,
//! which can cost statements of its own (the default checks neither).
//!
//! A checked field whose target has its company by `_inherits` delegation (amendment A2,
//! `product.product` and its template) adds no branch to that statement: once the statement has
//! given every target, their companies are read through the ORM's delegated read, an extra read
//! only when such a field is checked and the companies are not cached. That read follows the
//! ordinary read path, which flushes only a database-generated column it reads.
//!
//! Pending writes are never flushed for the check: every value written in the transaction is in
//! the cache and wins over the database, and no marked id has a pending value.

mod context;
mod error;
mod plan;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use erp_fields::{Cell, Error, ErrorKind, FieldType, RecId, Result};
use erp_registry::{CompanyScope, FieldId, ModelId, ModelSpec, Registry};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio_postgres::types::ToSql;

pub use error::{CompanyError, CompanyInconsistency};
use error::{LISTED, py_repr};
use plan::{LinkKind, Linking, PlanField, TargetCompanies, TargetScope, allowed, path_ids};

use crate::records::Records;
use crate::{Env, Transaction};

/// Most distinct ids one statement of the check reads; a bigger batch runs one statement per
/// chunk.
pub const COMPANY_CHECK_CHUNK: usize = 100_000;

#[cfg(feature = "test-faults")]
fn chunk_size(tx: &Transaction) -> Result<usize> {
    Ok(tx.company_chunk.unwrap_or(COMPANY_CHECK_CHUNK))
}

#[cfg(not(feature = "test-faults"))]
fn chunk_size(_tx: &Transaction) -> Result<usize> {
    Ok(COMPANY_CHECK_CHUNK)
}

/// A column or relation: (model, field).
type Key = (ModelId, FieldId);

/// What the check looks at for one call.
struct Plan {
    model: ModelId,
    scope: CompanyScope,
    fields: Vec<PlanField>,
    /// Some field has a parent match: parent paths of the record companies are needed.
    parents: Option<Key>,
}

/// Values read by the statement that are not (or must not be) cached.
#[derive(Default)]
struct Local {
    /// Relation rows by (model, many2many field) and owner.
    links: FxHashMap<Key, FxHashMap<i64, Vec<i64>>>,
    /// `company_ids` of target records by (comodel, id).
    many: FxHashMap<(ModelId, i64), Vec<i64>>,
    /// Ancestor ids (self included) by company.
    paths: FxHashMap<i64, Vec<i64>>,
    /// The company (`None` for none) of target records whose company comes by delegation, by
    /// (comodel, id), read through the ORM after the statement (A2).
    delegated: FxHashMap<(ModelId, i64), Option<i64>>,
}

/// Ids to read, by purpose.
#[derive(Default, Clone)]
struct Requests {
    /// A many2one column of a table, by row id.
    cols: BTreeMap<Key, BTreeSet<i64>>,
    /// Relation rows of a many2many, by owner id.
    links: BTreeMap<Key, BTreeSet<i64>>,
    /// The `company_ids` array of rows, by id (key: the comodel's company field).
    many: BTreeMap<Key, BTreeSet<i64>>,
    /// Parent paths, by company id.
    paths: BTreeSet<i64>,
    /// `(col key, links key)`: the col branch also reads the targets of those links.
    via_cols: BTreeSet<(Key, Key)>,
    /// `(many key, links key)`: the same for the `company_ids` branch.
    via_many: BTreeSet<(Key, Key)>,
    /// Paths also read the companies in these columns / link tables.
    path_via_cols: BTreeSet<Key>,
    path_via_links: BTreeSet<Key>,
}

impl Requests {
    fn is_empty(&self) -> bool {
        self.cols.values().all(BTreeSet::is_empty)
            && self.links.values().all(BTreeSet::is_empty)
            && self.many.values().all(BTreeSet::is_empty)
            && self.paths.is_empty()
    }

    /// Distinct ids to read, summed over the branches.
    fn count(&self) -> usize {
        self.cols
            .values()
            .chain(self.links.values())
            .chain(self.many.values())
            .map(BTreeSet::len)
            .fold(self.paths.len(), usize::saturating_add)
    }

    /// Split into requests of at most `n` ids each (the vias go with every part: a branch built
    /// from them needs the links of its own part, see [`Transaction::company_statement`]).
    fn chunks(self, n: usize) -> Vec<Requests> {
        if self.count() <= n {
            return vec![self];
        }
        let shell = Requests {
            via_cols: self.via_cols.clone(),
            via_many: self.via_many.clone(),
            path_via_cols: self.path_via_cols.clone(),
            path_via_links: self.path_via_links.clone(),
            ..Requests::default()
        };
        let mut out: Vec<Requests> = Vec::new();
        let mut current = shell.clone();
        let mut size = 0usize;
        macro_rules! feed {
            ($map:ident) => {
                for (key, ids) in &self.$map {
                    for id in ids {
                        if size == n {
                            out.push(std::mem::replace(&mut current, shell.clone()));
                            size = 0;
                        }
                        current.$map.entry(*key).or_default().insert(*id);
                        size = size.saturating_add(1);
                    }
                }
            };
        }
        feed!(cols);
        feed!(links);
        feed!(many);
        for id in &self.paths {
            if size == n {
                out.push(std::mem::replace(&mut current, shell.clone()));
                size = 0;
            }
            current.paths.insert(*id);
            size = size.saturating_add(1);
        }
        if size > 0 {
            out.push(current);
        }
        out
    }
}

/// A statement of the check: its text, one id list per parameter, and what each branch is.
type Statement = (String, Vec<Vec<i64>>, Vec<Tag>);

/// What a row of the statement is about.
#[derive(Clone, Copy)]
enum Tag {
    Col(Key),
    Links(Key),
    Many(Key),
    Paths,
}

/// One inconsistency: the record, the plan field, the companies of the record, the targets.
struct Found {
    record: RecId,
    field: usize,
    companies: Option<Vec<i64>>,
    targets: Vec<i64>,
}

fn real_ids(ids: &[RecId]) -> Vec<RecId> {
    let mut seen = FxHashSet::default();
    ids.iter()
        .copied()
        // virtual records are never written to the database
        .filter(|i| !i.is_new() && seen.insert(*i))
        .collect()
}

fn id_values(cell: &Cell) -> Vec<i64> {
    match cell {
        Cell::Ids(ids) => ids.iter().map(|i| i.get()).collect(),
        other => crate::relational::id_of(other)
            .map(|i| i.get())
            .into_iter()
            .collect(),
    }
}

impl Transaction {
    /// `_check_company(fnames)`: `written` are the fields of a `write` (`None` for `create`).
    ///
    /// # Errors
    ///
    /// [`CompanyError`] converted to [`Error`]; failures of the single read statement.
    pub(crate) async fn check_company(
        &mut self,
        records: &Records,
        written: Option<&[FieldId]>,
    ) -> Result<()> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        if !spec.check_company_auto || records.is_empty() {
            return Ok(());
        }
        let ids = real_ids(records.rec_ids());
        if ids.is_empty() {
            return Ok(());
        }
        let Some(plan) = self.company_plan(&registry, spec, written)? else {
            return Ok(());
        };
        let env = records.env().clone();
        let su = records.engine_sudo();
        let su_env = su.env().clone();
        // the validated current company, also under sudo (review S1)
        let current: Option<i64> = self.companies(&env).await?.first().map(|c| c.get());

        // stored computes that decide a company must be settled before they are read
        let mut settle: Vec<Key> = Vec::new();
        if let Some(cf) = plan.scope.field() {
            settle.push((plan.model, cf));
        }
        for pf in &plan.fields {
            // the checked field itself may be a stored compute with a pending value
            settle.push((plan.model, pf.field));
            settle.push(Self::target_column(&registry, pf)?);
        }
        for (m, f) in settle {
            self.settle(&su_env, m, f).await?;
        }
        // x2many fields and columns the direct read cannot serve come through the x2many cache
        self.company_prefetch_other(&su, &plan, &ids).await?;

        let mut local = Local::default();
        for field in plan.fields.iter().filter(|field| field.company_dependent) {
            if current.is_none()
                && records.rec_ids().iter().all(|id| {
                    matches!(
                        self.cache.get(plan.model, field.field, *id),
                        Some(Cell::Null)
                    )
                })
            {
                // A create with omitted properties seeds known SQL NULLs. There is no link
                // to check and no company whose fallback to project.
                // Unknown or non-null storage must still go through strict preparation.
                let links = local.links.entry((plan.model, field.field)).or_default();
                for id in records.rec_ids() {
                    links.insert(id.get(), Vec::new());
                }
                continue;
            }
            self.ensure(&su, field.field).await?;
            let values = self.cells_of(&su_env, plan.model, field.field, records.rec_ids())?;
            let links = local.links.entry((plan.model, field.field)).or_default();
            for (id, value) in records.rec_ids().iter().zip(values) {
                links.insert(id.get(), id_values(&value));
            }
        }
        let requests = self.company_requests(&registry, &plan, &ids, current, &local)?;
        if !requests.is_empty() {
            let size = chunk_size(self)?;
            for part in requests.chunks(size) {
                self.company_run(&registry, &plan, &part, &mut local)
                    .await?;
            }
        }
        // every target is known now: the delegated companies are read through the ORM
        self.company_delegated(&su_env, &registry, &plan, &ids, &mut local)
            .await?;
        let found = self.company_decide(&registry, &plan, &ids, current, &local)?;
        if found.is_empty() {
            return Ok(());
        }
        Err(self.company_error(&env, &registry, &plan, &found).await?)
    }

    /// The fields to check and how to read them; `None` when there is nothing to check.
    fn company_plan(
        &self,
        registry: &Registry,
        spec: &ModelSpec,
        written: Option<&[FieldId]>,
    ) -> Result<Option<Plan>> {
        let scope = spec.company_scope.ok_or_else(|| {
            Error::programming(format!(
                "{} has check_company_auto but no company scope (registry invariant)",
                spec.name
            ))
        })?;
        if !matches!(
            scope,
            CompanyScope::Itself | CompanyScope::Single { .. } | CompanyScope::Many { .. }
        ) {
            // CompanyScope is non-exhaustive: a form this check does not know must not be
            // treated as a known one
            return Err(Error::programming(format!(
                "{} has a company scope the check does not support",
                spec.name
            )));
        }
        let company_fields: Vec<FieldId> = ["company_id", "company_ids"]
            .into_iter()
            .filter_map(|n| spec.field_id(n))
            .collect();
        let all = written.is_none_or(|w| w.iter().any(|f| company_fields.contains(f)));
        let mut fields = Vec::new();
        for f in &spec.fields {
            if !f.check_company.is_on() || !f.ty.is_relational() || f.inherited.is_some() {
                continue;
            }
            // A company-dependent link is matched against the current company, never the
            // record's (`company_decide`), so a write of the record's company fields alone
            // cannot change its verdict; it is checked on create and when it is written.
            let checked = if f.company_dependent {
                written.is_none_or(|w| w.contains(&f.id))
            } else {
                all || written.is_some_and(|w| w.contains(&f.id))
            };
            if !checked {
                continue;
            }
            let comodel = crate::relational::comodel_of(registry, f)?;
            // the registry refuses a checked field whose target has no company scope: reaching
            // here is a broken invariant, never a silent skip (review F3)
            let (target, matching) = match (comodel.company_scope, comodel.company_delegation) {
                (Some(CompanyScope::Single { field, matching }), _) => {
                    (TargetScope::Single(field), matching.into())
                }
                (Some(CompanyScope::Many { field, matching }), _) => {
                    (TargetScope::Many(field), matching.into())
                }
                // the target's company comes by `_inherits` delegation (A2)
                (None, Some(delegation)) => {
                    if f.company_dependent {
                        // checked against the current company through prepared links, a path
                        // that does not read through a delegation
                        return Err(Error::programming(format!(
                            "{}.{} is company-dependent and its target {} has its company through \
                             delegation; not supported",
                            spec.name, f.name, comodel.name
                        )));
                    }
                    (
                        TargetScope::Delegated(delegation.field),
                        delegation.matching.into(),
                    )
                }
                _ => {
                    return Err(Error::programming(format!(
                        "{}.{} is checked against {}, which has no company scope (registry invariant)",
                        spec.name, f.name, comodel.name
                    )));
                }
            };
            let kind = match &f.ty {
                FieldType::Many2one { .. } if f.has_column() => LinkKind::Many2one,
                FieldType::Many2many {
                    relation: Some(_), ..
                } if f.store && self.shared.relations.domain(spec.id, f.id).is_true() => {
                    LinkKind::Many2many
                }
                _ => LinkKind::Other,
            };
            fields.push(PlanField {
                field: f.id,
                comodel: comodel.id,
                kind,
                target,
                matching,
                company_dependent: f.company_dependent,
            });
        }
        if fields.is_empty() {
            return Ok(None);
        }
        let parents = if fields.iter().any(|f| f.matching.needs_parent_path()) {
            let company = registry.model("res.company").ok_or_else(|| {
                Error::programming("a parent company match needs the res.company model")
            })?;
            let path = company.parent_path.ok_or_else(|| {
                Error::programming("a parent company match needs res.company to keep a parent path")
            })?;
            Some((company.id, path))
        } else {
            None
        };
        Ok(Some(Plan {
            model: spec.id,
            scope,
            fields,
            parents,
        }))
    }

    /// Make the values the direct read cannot serve available through the x2many/field cache.
    async fn company_prefetch_other(
        &mut self,
        su: &Records,
        plan: &Plan,
        ids: &[RecId],
    ) -> Result<()> {
        let registry = Arc::clone(su.env().registry());
        let spec = registry.model_by_id(plan.model);
        let mut scalar: Vec<&str> = Vec::new();
        for pf in &plan.fields {
            if pf.kind != LinkKind::Other {
                continue;
            }
            let fs = spec.field_by_id(pf.field);
            if ids
                .iter()
                .all(|i| self.cache.contains(plan.model, pf.field, *i))
            {
                continue;
            }
            if fs.ty.is_x2many() {
                // every linked record, archived ones included whatever the field's context
                self.ensure_x2many(su, pf.field).await?;
            } else {
                scalar.push(fs.name);
            }
        }
        if !scalar.is_empty() {
            su.fetch(self, Some(&scalar)).await?;
        }
        Ok(())
    }

    /// The companies of record `id`; `None` while unknown.
    fn company_of_record(&self, plan: &Plan, local: &Local, id: RecId) -> Option<Vec<i64>> {
        match plan.scope {
            CompanyScope::Single { field, .. } => {
                self.cache.get(plan.model, field, id).map(|c| id_values(&c))
            }
            CompanyScope::Many { field, .. } => match self.cache.get(plan.model, field, id) {
                Some(c) => Some(id_values(&c)),
                None => local
                    .links
                    .get(&(plan.model, field))?
                    .get(&id.get())
                    .cloned(),
            },
            // `Itself` (the plan refuses any other form): a company is its own company
            _ => Some(vec![id.get()]),
        }
    }

    /// The targets of `pf` for record `id`; `None` while unknown.
    fn targets_of(
        &self,
        plan: &Plan,
        local: &Local,
        pf: &PlanField,
        id: RecId,
    ) -> Option<Vec<i64>> {
        if pf.company_dependent {
            return local
                .links
                .get(&(plan.model, pf.field))?
                .get(&id.get())
                .cloned();
        }
        match self.cache.get(plan.model, pf.field, id) {
            Some(c) => Some(id_values(&c)),
            None => local
                .links
                .get(&(plan.model, pf.field))?
                .get(&id.get())
                .cloned(),
        }
    }

    /// The companies of target `id` of `pf`; `None` while unknown.
    fn target_companies(
        &self,
        local: &Local,
        pf: &PlanField,
        id: i64,
    ) -> Option<(Option<i64>, Vec<i64>)> {
        let rec = RecId::from_real(id)?;
        match pf.target {
            TargetScope::Single(cf) => {
                let cell = self.cache.get(pf.comodel, cf, rec)?;
                Some((crate::relational::id_of(&cell).map(|c| c.get()), Vec::new()))
            }
            TargetScope::Many(cf) => match self.cache.get(pf.comodel, cf, rec) {
                Some(c) => Some((None, id_values(&c))),
                None => local.many.get(&(pf.comodel, id)).map(|v| (None, v.clone())),
            },
            // read by `company_delegated` after the statement
            TargetScope::Delegated(_) => local
                .delegated
                .get(&(pf.comodel, id))
                .map(|company| (*company, Vec::new())),
        }
    }

    /// The column that holds the companies of `pf`'s targets: the comodel's own, or for a target
    /// whose company comes by delegation the storing model's (A2).
    ///
    /// # Errors
    ///
    /// `Programming` when the comodel has no delegation or it does not end at a model scoped by
    /// a `company_id` (registry invariants).
    fn target_column(registry: &Registry, pf: &PlanField) -> Result<Key> {
        match pf.target {
            TargetScope::Single(field) | TargetScope::Many(field) => Ok((pf.comodel, field)),
            TargetScope::Delegated(_) => {
                let comodel = registry.model_by_id(pf.comodel);
                let delegation = comodel.company_delegation.ok_or_else(|| {
                    Error::programming(format!(
                        "{} is checked through a company delegation it does not have (registry \
                         invariant)",
                        comodel.name
                    ))
                })?;
                let storing = registry.model_by_id(delegation.storing_model);
                match storing.company_scope {
                    Some(CompanyScope::Single { field, .. }) => Ok((storing.id, field)),
                    _ => Err(Error::programming(format!(
                        "the company delegation of {} ends at {}, which is not scoped by a \
                         company_id (registry invariant)",
                        comodel.name, storing.name
                    ))),
                }
            }
        }
    }

    /// The companies of the targets of every plan field whose target has its company by
    /// delegation (A2), read through the ORM's delegated read as superuser, so a pending
    /// write on the storing model counts, into `local.delegated`. Runs after the statement: every
    /// target is known then.
    ///
    /// # Errors
    ///
    /// `Programming` when the read returns no value for a target, or a value that is not a company
    /// reference; failures of the read.
    async fn company_delegated(
        &mut self,
        su_env: &Env,
        registry: &Registry,
        plan: &Plan,
        ids: &[RecId],
        local: &mut Local,
    ) -> Result<()> {
        for pf in &plan.fields {
            let field = match pf.target {
                TargetScope::Delegated(field) => field,
                // read by the statement
                TargetScope::Single(_) | TargetScope::Many(_) => continue,
            };
            let mut seen: FxHashSet<i64> = FxHashSet::default();
            let mut wanted: Vec<Option<RecId>> = Vec::new();
            for id in ids {
                // a record the statement found no row for (deleted meanwhile): `company_decide`
                // reports it as a missing record, the same kind as without a delegated field
                let Some(targets) = self.targets_of(plan, local, pf, *id) else {
                    continue;
                };
                for t in targets {
                    if local.delegated.contains_key(&(pf.comodel, t)) || !seen.insert(t) {
                        continue;
                    }
                    // a target that is not a database record has no company to read:
                    // `company_decide` reports it as a missing target, as for a direct target
                    if let Some(rec) = RecId::from_real(t) {
                        wanted.push(Some(rec));
                    }
                }
            }
            if wanted.is_empty() {
                continue;
            }
            let comodel = registry.model_by_id(pf.comodel);
            let cells = self
                .cells_by_id(su_env, (pf.comodel, field), &wanted)
                .await?;
            for rec in wanted.iter().flatten() {
                let company = match cells.get(rec) {
                    Some(Cell::Id(company)) => Some(*company),
                    Some(Cell::Null) => None,
                    Some(other) => {
                        return Err(Error::programming(format!(
                            "{}.{} of record {} read as {other:?} through its delegation, not a \
                             company reference",
                            comodel.name,
                            comodel.field_by_id(field).name,
                            rec.get()
                        )));
                    }
                    None => {
                        return Err(Error::programming(format!(
                            "reading {}.{} through its delegation gave no value for record {}",
                            comodel.name,
                            comodel.field_by_id(field).name,
                            rec.get()
                        )));
                    }
                };
                local.delegated.insert((pf.comodel, rec.get()), company);
            }
        }
        Ok(())
    }

    /// The ancestor ids of company `id` (itself included); `None` while unknown.
    fn path_of(
        &self,
        plan: &Plan,
        local: &Local,
        id: i64,
    ) -> Option<std::result::Result<Vec<i64>, CompanyError>> {
        let (company, path_field) = plan.parents?;
        let rec = RecId::from_real(id)?;
        if let Some(Cell::Str(path)) = self.cache.get(company, path_field, rec) {
            return Some(path_ids(&path).ok_or(CompanyError::MalformedParentPath { company: id }));
        }
        local.paths.get(&id).cloned().map(Ok)
    }

    /// What the cache and the values read so far do not give: the ids to read.
    fn company_requests(
        &self,
        registry: &Registry,
        plan: &Plan,
        ids: &[RecId],
        current: Option<i64>,
        local: &Local,
    ) -> Result<Requests> {
        let spec = registry.model_by_id(plan.model);
        let mut req = Requests::default();
        let mut path_ids_wanted: BTreeSet<i64> = BTreeSet::new();
        let mut need_record_companies = false;
        // companies of the records (not needed when every checked field is company-dependent)
        let uses_record = plan.fields.iter().any(|f| !f.company_dependent);
        if uses_record {
            for id in ids {
                match self.company_of_record(plan, local, *id) {
                    Some(r) => path_ids_wanted.extend(r),
                    None => {
                        need_record_companies = true;
                        match plan.scope {
                            CompanyScope::Single { field, .. } => {
                                req.cols
                                    .entry((plan.model, field))
                                    .or_default()
                                    .insert(id.get());
                            }
                            CompanyScope::Many { field, .. } => {
                                req.links
                                    .entry((plan.model, field))
                                    .or_default()
                                    .insert(id.get());
                            }
                            // `Itself` is always known
                            _ => {}
                        }
                    }
                }
            }
        }
        for pf in &plan.fields {
            let fs = spec.field_by_id(pf.field);
            let links_key = (plan.model, pf.field);
            let mut via = false;
            let mut wanted: BTreeSet<i64> = BTreeSet::new();
            for id in ids {
                match self.targets_of(plan, local, pf, *id) {
                    Some(t) => wanted.extend(t),
                    None => match pf.kind {
                        _ if pf.company_dependent => {
                            // the links of every checked record were read into the map above
                            return Err(Error::programming(format!(
                                "the company check of {}.{} found no link for a checked record (internal invariant)",
                                spec.name, fs.name
                            )));
                        }
                        LinkKind::Many2one => {
                            req.cols.entry(links_key).or_default().insert(id.get());
                        }
                        LinkKind::Many2many => {
                            req.links.entry(links_key).or_default().insert(id.get());
                            via = true;
                        }
                        LinkKind::Other => {
                            return Err(Error::programming(format!(
                                "{}.{} is not cached after reading it (company check)",
                                spec.name, fs.name
                            )));
                        }
                    },
                }
            }
            let target_key = (pf.comodel, pf.target.field());
            let target_requests = match pf.target {
                TargetScope::Single(_) => Some(&mut req.cols),
                TargetScope::Many(_) => Some(&mut req.many),
                // no target branch: read through the delegation once the statement has given
                // every target (`company_delegated`)
                TargetScope::Delegated(_) => None,
            };
            if let Some(requests) = target_requests {
                let targets = requests.entry(target_key).or_default();
                for t in wanted {
                    if self.target_companies(local, pf, t).is_none() {
                        targets.insert(t);
                    }
                }
            }
            if via {
                match pf.target {
                    TargetScope::Single(_) => {
                        req.via_cols.insert((target_key, links_key));
                    }
                    TargetScope::Many(_) => {
                        req.via_many.insert((target_key, links_key));
                    }
                    // the link rows are read (they give the targets), no target branch
                    TargetScope::Delegated(_) => {}
                }
            }
        }
        if let Some((company, path_field)) = plan.parents {
            // current company of company-dependent fields
            if let Some(c) = current {
                path_ids_wanted.insert(c);
            }
            for c in path_ids_wanted {
                if self.path_of_unknown(company, path_field, local, c) {
                    req.paths.insert(c);
                }
            }
            if need_record_companies {
                match plan.scope {
                    CompanyScope::Single { field, .. } => {
                        req.path_via_cols.insert((plan.model, field));
                    }
                    CompanyScope::Many { field, .. } => {
                        req.path_via_links.insert((plan.model, field));
                    }
                    // `Itself` is always known
                    _ => {}
                }
            }
        }
        Ok(req)
    }

    fn path_of_unknown(
        &self,
        company: ModelId,
        path_field: FieldId,
        local: &Local,
        id: i64,
    ) -> bool {
        let Some(rec) = RecId::from_real(id) else {
            return false;
        };
        !matches!(self.cache.get(company, path_field, rec), Some(Cell::Str(_)))
            && !local.paths.contains_key(&id)
    }

    /// The statement for `req`: a branch for each purpose that has ids, tagged by position.
    fn company_statement(
        registry: &Registry,
        plan: &Plan,
        req: &Requests,
    ) -> Result<Option<Statement>> {
        let q = erp_sql::quote;
        let mut params: Vec<Vec<i64>> = Vec::new();
        let mut tags: Vec<Tag> = Vec::new();
        let mut parts: Vec<String> = Vec::new();
        let param_of = |ids: &BTreeSet<i64>, params: &mut Vec<Vec<i64>>| -> usize {
            params.push(ids.iter().copied().collect());
            params.len()
        };
        let relation = |key: Key| -> Result<(String, String, String)> {
            let fs = registry.model_by_id(key.0).field_by_id(key.1);
            match &fs.ty {
                FieldType::Many2many {
                    relation: Some(rel),
                    ..
                } => Ok((q(&rel.table), q(&rel.column1), q(&rel.column2))),
                _ => Err(Error::programming(format!(
                    "{}.{} has no relation table (company check)",
                    registry.model_by_id(key.0).name,
                    fs.name
                ))),
            }
        };
        // parameters of the id lists the vias refer to
        let mut links_param: BTreeMap<Key, usize> = BTreeMap::new();
        for (key, ids) in &req.links {
            if !ids.is_empty() {
                links_param.insert(*key, param_of(ids, &mut params));
            }
        }
        let mut cols_param: BTreeMap<Key, usize> = BTreeMap::new();
        for (key, ids) in &req.cols {
            if !ids.is_empty() {
                cols_param.insert(*key, param_of(ids, &mut params));
            }
        }
        // links
        for key in links_param.keys() {
            let (table, c1, c2) = relation(*key)?;
            let n = links_param[key];
            tags.push(Tag::Links(*key));
            parts.push(format!(
                "SELECT {}::int4, r.{c1}::int8, r.{c2}::int8, NULL::int8[], NULL::varchar \
                 FROM {table} r WHERE r.{c1} = ANY(${n}::int8[])",
                tags.len() - 1
            ));
        }
        // via conditions over link tables, for a target branch
        let link_vias = |vias: &BTreeSet<(Key, Key)>, key: Key| -> Result<Vec<String>> {
            let mut out = Vec::new();
            for (target, links) in vias {
                if *target != key {
                    continue;
                }
                if let Some(n) = links_param.get(links) {
                    let (table, c1, c2) = relation(*links)?;
                    out.push(format!(
                        "t.\"id\" IN (SELECT r.{c2} FROM {table} r WHERE r.{c1} = ANY(${n}::int8[]))"
                    ));
                }
            }
            Ok(out)
        };
        // the many2one columns: records' own, targets', each by purpose
        let mut col_keys: BTreeSet<Key> = req
            .cols
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|(k, _)| *k)
            .collect();
        for (target, links) in &req.via_cols {
            if links_param.contains_key(links) {
                col_keys.insert(*target);
            }
        }
        // the targets of a many2one column read in this statement
        for pf in &plan.fields {
            if pf.kind == LinkKind::Many2one
                && matches!(pf.target, TargetScope::Single(_))
                && cols_param.contains_key(&(plan.model, pf.field))
            {
                col_keys.insert((pf.comodel, pf.target.field()));
            }
        }
        for key in col_keys {
            let spec = registry.model_by_id(key.0);
            let col = q(spec.field_by_id(key.1).name);
            let table = q(&spec.table);
            let mut conditions: Vec<String> = Vec::new();
            if let Some(n) = cols_param.get(&key) {
                conditions.push(format!("t.\"id\" = ANY(${n}::int8[])"));
            }
            conditions.extend(link_vias(&req.via_cols, key)?);
            // a plan field that reads its own many2one column in this statement also reads the
            // targets' companies: the target branch selects through the column
            for pf in &plan.fields {
                if pf.kind != LinkKind::Many2one {
                    continue;
                }
                let own = (plan.model, pf.field);
                let target_key = (pf.comodel, pf.target.field());
                if target_key == key
                    && matches!(pf.target, TargetScope::Single(_))
                    && let Some(n) = cols_param.get(&own)
                {
                    let own_col = q(registry.model_by_id(own.0).field_by_id(own.1).name);
                    let own_table = q(&registry.model_by_id(own.0).table);
                    conditions.push(format!(
                        "t.\"id\" IN (SELECT o.{own_col} FROM {own_table} o WHERE o.\"id\" = \
                         ANY(${n}::int8[]))"
                    ));
                }
            }
            // one branch per condition: an `OR` of an `= ANY` and a sub-select cannot use an index
            // or a semi-join on a big comodel; every branch carries the same tag and the cache
            // inserts are idempotent (review D2)
            for condition in &conditions {
                tags.push(Tag::Col(key));
                parts.push(format!(
                    "SELECT {}::int4, t.\"id\"::int8, t.{col}::int8, NULL::int8[], \
                     NULL::varchar FROM {table} t WHERE {condition}",
                    tags.len() - 1,
                ));
            }
        }
        // company_ids arrays of target records
        let mut many_keys: BTreeSet<Key> = req
            .many
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|(k, _)| *k)
            .collect();
        for (target, links) in &req.via_many {
            if links_param.contains_key(links) {
                many_keys.insert(*target);
            }
        }
        for pf in &plan.fields {
            if pf.kind == LinkKind::Many2one
                && matches!(pf.target, TargetScope::Many(_))
                && cols_param.contains_key(&(plan.model, pf.field))
            {
                many_keys.insert((pf.comodel, pf.target.field()));
            }
        }
        for key in many_keys {
            let spec = registry.model_by_id(key.0);
            let (rel, c1, c2) = relation(key)?;
            let table = q(&spec.table);
            let mut conditions: Vec<String> = Vec::new();
            if let Some(ids) = req.many.get(&key).filter(|s| !s.is_empty()) {
                let n = param_of(ids, &mut params);
                conditions.push(format!("t.\"id\" = ANY(${n}::int8[])"));
            }
            conditions.extend(link_vias(&req.via_many, key)?);
            for pf in &plan.fields {
                if pf.kind != LinkKind::Many2one {
                    continue;
                }
                let own = (plan.model, pf.field);
                let target_key = (pf.comodel, pf.target.field());
                if target_key == key
                    && matches!(pf.target, TargetScope::Many(_))
                    && let Some(n) = cols_param.get(&own)
                {
                    let own_col = q(registry.model_by_id(own.0).field_by_id(own.1).name);
                    let own_table = q(&registry.model_by_id(own.0).table);
                    conditions.push(format!(
                        "t.\"id\" IN (SELECT o.{own_col} FROM {own_table} o WHERE o.\"id\" = \
                         ANY(${n}::int8[]))"
                    ));
                }
            }
            for condition in &conditions {
                tags.push(Tag::Many(key));
                parts.push(format!(
                    "SELECT {}::int4, t.\"id\"::int8, NULL::int8, ARRAY(SELECT c.{c2}::int8 FROM \
                     {rel} c WHERE c.{c1} = t.\"id\" ORDER BY 1), NULL::varchar FROM {table} t \
                     WHERE {condition}",
                    tags.len() - 1,
                ));
            }
        }
        // parent paths
        if let Some((company, path_field)) = plan.parents {
            let spec = registry.model_by_id(company);
            let path_col = q(spec.field_by_id(path_field).name);
            let table = q(&spec.table);
            let mut conditions: Vec<String> = Vec::new();
            if !req.paths.is_empty() {
                let n = param_of(&req.paths, &mut params);
                conditions.push(format!("c.\"id\" = ANY(${n}::int8[])"));
            }
            for key in &req.path_via_cols {
                if let Some(n) = cols_param.get(key) {
                    let owner = registry.model_by_id(key.0);
                    let col = q(owner.field_by_id(key.1).name);
                    let owner_table = q(&owner.table);
                    conditions.push(format!(
                        "c.\"id\" IN (SELECT o.{col} FROM {owner_table} o WHERE o.\"id\" = \
                         ANY(${n}::int8[]))"
                    ));
                }
            }
            for key in &req.path_via_links {
                if let Some(n) = links_param.get(key) {
                    let (rel, c1, c2) = relation(*key)?;
                    conditions.push(format!(
                        "c.\"id\" IN (SELECT r.{c2} FROM {rel} r WHERE r.{c1} = ANY(${n}::int8[]))"
                    ));
                }
            }
            for condition in &conditions {
                tags.push(Tag::Paths);
                parts.push(format!(
                    "SELECT {}::int4, c.\"id\"::int8, NULL::int8, NULL::int8[], \
                     c.{path_col}::varchar FROM {table} c WHERE {condition}",
                    tags.len() - 1,
                ));
            }
        }
        if parts.is_empty() {
            return Ok(None);
        }
        Ok(Some((parts.join(" UNION ALL "), params, tags)))
    }

    /// Run the statement for `req` and keep what it returns.
    async fn company_run(
        &mut self,
        registry: &Arc<Registry>,
        plan: &Plan,
        req: &Requests,
        local: &mut Local,
    ) -> Result<()> {
        // flush-before-read holds by construction: the check settled every stored compute
        // that decides a company or is itself checked, so no column this statement reads (also
        // through its sub-selects, which reach ids that are not listed) has a record to compute
        // (review D4)
        #[cfg(debug_assertions)]
        {
            let mut read: Vec<Key> = Vec::new();
            if let Some(cf) = plan.scope.field() {
                read.push((plan.model, cf));
            }
            for pf in &plan.fields {
                read.push((plan.model, pf.field));
                match pf.target {
                    TargetScope::Single(field) | TargetScope::Many(field) => {
                        read.push((pf.comodel, field));
                    }
                    // the statement reads nothing of a delegated target
                    TargetScope::Delegated(_) => {}
                }
            }
            if let Some(parents) = plan.parents {
                read.push(parents);
            }
            for (model, field) in read {
                debug_assert!(
                    !self
                        .cache
                        .model(model)
                        .is_some_and(|m| m.has_pending(field)),
                    "company check reads a column with a pending compute"
                );
            }
        }
        let Some((sql, params, tags)) = Self::company_statement(registry, plan, req)? else {
            return Ok(());
        };
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let rows = self.query(&sql, &refs).await?;
        // owners with no relation row have an empty list, not an unknown one
        for tag in &tags {
            if let Tag::Links(key) = tag
                && let Some(ids) = req.links.get(key)
            {
                let map = local.links.entry(*key).or_default();
                for id in ids {
                    map.entry(*id).or_default();
                }
            }
        }
        let decode = |e: tokio_postgres::Error| {
            Error::new(ErrorKind::Database, "could not decode a company row").with_source(e)
        };
        for row in &rows {
            let which: i32 = row.try_get(0).map_err(decode)?;
            let id: i64 = row.try_get(1).map_err(decode)?;
            let value: Option<i64> = row.try_get(2).map_err(decode)?;
            let array: Option<Vec<i64>> = row.try_get(3).map_err(decode)?;
            let text: Option<String> = row.try_get(4).map_err(decode)?;
            let tag = usize::try_from(which)
                .ok()
                .and_then(|w| tags.get(w))
                .copied()
                .ok_or_else(|| {
                    Error::new(ErrorKind::Database, "company row with an unknown branch")
                })?;
            match tag {
                Tag::Col(key) => {
                    let Some(rec) = RecId::from_real(id) else {
                        continue;
                    };
                    let storage = registry.model_by_id(key.0).field_by_id(key.1).storage();
                    self.cache.model_mut(key.0).insert_fetched(
                        key.1,
                        storage,
                        rec,
                        value.map_or(Cell::Null, Cell::Id),
                    )?;
                }
                Tag::Links(key) => {
                    if let Some(target) = value {
                        local
                            .links
                            .entry(key)
                            .or_default()
                            .entry(id)
                            .or_default()
                            .push(target);
                    }
                }
                Tag::Many(key) => {
                    let array = array.ok_or_else(|| {
                        Error::new(ErrorKind::Database, "company row without a company list")
                    })?;
                    local.many.insert((key.0, id), array);
                }
                Tag::Paths => {
                    let path = text
                        .as_deref()
                        .ok_or(CompanyError::MalformedParentPath { company: id })?;
                    let ids =
                        path_ids(path).ok_or(CompanyError::MalformedParentPath { company: id })?;
                    local.paths.insert(id, ids);
                    // the parent path is cached like any fetched column (the parent store keeps
                    // it current), so the next batch of the transaction reads nothing (review D3)
                    if let (Some((company, path_field)), Some(rec)) =
                        (plan.parents, RecId::from_real(id))
                    {
                        let storage = registry
                            .model_by_id(company)
                            .field_by_id(path_field)
                            .storage();
                        self.cache.model_mut(company).insert_fetched(
                            path_field,
                            storage,
                            rec,
                            Cell::Str(path.into()),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Compare every record's targets with its companies, in memory.
    fn company_decide(
        &self,
        registry: &Registry,
        plan: &Plan,
        ids: &[RecId],
        current: Option<i64>,
        local: &Local,
    ) -> Result<Vec<Found>> {
        let spec = registry.model_by_id(plan.model);
        let mut found = Vec::new();
        let mut closure: FxHashSet<i64> = FxHashSet::default();
        for id in ids {
            for (fi, pf) in plan.fields.iter().enumerate() {
                let fs = spec.field_by_id(pf.field);
                let targets =
                    self.targets_of(plan, local, pf, *id)
                        .ok_or(CompanyError::MissingRecord {
                            model: spec.name,
                            id: id.get(),
                        })?;
                if targets.is_empty() {
                    continue;
                }
                let companies: Vec<i64> = if pf.company_dependent {
                    match current {
                        Some(c) => vec![c],
                        None => {
                            return Err(Error::validation(
                                "company-dependent fields require a current company",
                            ));
                        }
                    }
                } else {
                    self.company_of_record(plan, local, *id)
                        .ok_or(CompanyError::MissingRecord {
                            model: spec.name,
                            id: id.get(),
                        })?
                };
                closure.clear();
                if pf.matching.needs_parent_path() {
                    for c in &companies {
                        let path = self.path_of(plan, local, *c).ok_or(
                            CompanyError::MissingTarget {
                                comodel: "res.company",
                                id: *c,
                                model: spec.name,
                                field: fs.name,
                            },
                        )??;
                        closure.extend(path);
                    }
                }
                let linking = Linking {
                    companies: &companies,
                    closure: &closure,
                };
                let comodel = registry.model_by_id(pf.comodel);
                let mut bad = false;
                for t in &targets {
                    let (one, many) = self.target_companies(local, pf, *t).ok_or(
                        CompanyError::MissingTarget {
                            comodel: comodel.name,
                            id: *t,
                            model: spec.name,
                            field: fs.name,
                        },
                    )?;
                    let target = match pf.target {
                        TargetScope::Single(_) | TargetScope::Delegated(_) => {
                            TargetCompanies::One(one)
                        }
                        TargetScope::Many(_) => TargetCompanies::Many(&many),
                    };
                    if !allowed(pf.matching, &linking, target)? {
                        bad = true;
                    }
                }
                if bad {
                    // a company-dependent field is matched against the current company, but the
                    // message names the record's companies
                    let record_companies = if pf.company_dependent {
                        self.company_of_record(plan, local, *id)
                    } else {
                        Some(companies)
                    };
                    found.push(Found {
                        record: *id,
                        field: fi,
                        companies: record_companies,
                        targets,
                    });
                }
            }
        }
        Ok(found)
    }

    /// Display names read as the request principal (the transaction's own, without the record
    /// environment's elevation); an unreadable record is `<model>(<id>,)`.
    async fn company_names(
        &mut self,
        env: &Env,
        wanted: Vec<(ModelId, Vec<RecId>)>,
    ) -> Result<FxHashMap<(ModelId, i64), Option<String>>> {
        let mut out = FxHashMap::default();
        // the request principal's own rights, never the record environment's elevation: under
        // `sudo()` the names of records the caller cannot read must not leak (review S3); the
        // language of the record environment is kept
        let mut reader = self.env();
        if let Some(lang) = env.lang() {
            reader = reader.with_context([("lang", erp_fields::Value::str(lang))]);
        }
        for (model, ids) in wanted {
            let ids: Arc<[RecId]> = Arc::from(ids);
            let all =
                Records::from_parts(reader.clone(), model, Arc::clone(&ids), Arc::clone(&ids));
            let readable = all
                .filtered_access(self, crate::access::Operation::Read)
                .await?;
            let cells = self.display_names(&readable).await?;
            for id in ids.iter() {
                out.insert((model, id.get()), None);
            }
            for (id, cell) in readable.rec_ids().iter().zip(cells) {
                // a display name that is not text is no name: the record is written as
                // `<model>(<id>,)` like an unreadable one, never as an empty name (review R9)
                if let Cell::Str(s) = cell {
                    out.insert((model, id.get()), Some(s.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// The record companies of the listed inconsistencies, in order: known ones as found, the
    /// others (matched against the current company only) read now, as superuser, in one read per
    /// company field (the error path, never a successful check).
    async fn record_companies(
        &mut self,
        env: &Env,
        plan: &Plan,
        shown: &[Found],
    ) -> Result<Vec<Vec<i64>>> {
        let unknown: Vec<RecId> = shown
            .iter()
            .filter(|f| f.companies.is_none())
            .map(|f| f.record)
            .collect();
        let mut read: FxHashMap<i64, Vec<i64>> = FxHashMap::default();
        if !unknown.is_empty() {
            let registry = Arc::clone(env.registry());
            let field = plan
                .scope
                .field()
                .map(|f| registry.model_by_id(plan.model).field_by_id(f));
            let su =
                crate::related_company::superuser_records(env, plan.model, unknown.iter().copied());
            match field {
                Some(field) => {
                    let cells = su.cells(self, field.name).await?;
                    if cells.len() != unknown.len() {
                        return Err(Error::programming(format!(
                            "reading {}.{} for {} records returned {} values",
                            registry.model_by_id(plan.model).name,
                            field.name,
                            unknown.len(),
                            cells.len()
                        )));
                    }
                    for (id, cell) in unknown.iter().zip(cells) {
                        // a company field holds a record, records or nothing (review SF6)
                        if !matches!(cell, Cell::Id(_) | Cell::Ids(_) | Cell::Null) {
                            return Err(Error::programming(format!(
                                "{}.{} of record {} read as {cell:?}, not a company reference",
                                registry.model_by_id(plan.model).name,
                                field.name,
                                id.get()
                            )));
                        }
                        read.insert(id.get(), id_values(&cell));
                    }
                }
                None => {
                    // `Itself`: a company is its own company
                    for id in &unknown {
                        read.insert(id.get(), vec![id.get()]);
                    }
                }
            }
        }
        shown
            .iter()
            .map(|f| match &f.companies {
                Some(known) => Ok(known.clone()),
                None => read.get(&f.record.get()).cloned().ok_or_else(|| {
                    Error::programming(format!(
                        "the companies of record {} were not read for the error listing",
                        f.record.get()
                    ))
                }),
            })
            .collect()
    }

    /// The error listing the first five inconsistencies.
    async fn company_error(
        &mut self,
        env: &Env,
        registry: &Registry,
        plan: &Plan,
        found: &[Found],
    ) -> Result<Error> {
        let spec = registry.model_by_id(plan.model);
        let company_model = registry.model("res.company").map(|m| m.id);
        let shown = &found[..found.len().min(LISTED)];
        let record_companies = self.record_companies(env, plan, shown).await?;
        let companies_of =
            |index: usize| -> &[i64] { record_companies.get(index).map_or(&[], Vec::as_slice) };
        let mut wanted: Vec<(ModelId, Vec<RecId>)> = Vec::new();
        let mut want = |model: ModelId, id: i64| {
            let Some(rec) = RecId::from_real(id) else {
                return;
            };
            match wanted.iter_mut().find(|(m, _)| *m == model) {
                Some((_, ids)) if !ids.contains(&rec) => ids.push(rec),
                Some(_) => {}
                None => wanted.push((model, vec![rec])),
            }
        };
        for (i, f) in shown.iter().enumerate() {
            if plan.scope != CompanyScope::Itself {
                want(plan.model, f.record.get());
            }
            if let Some(cm) = company_model {
                for c in companies_of(i) {
                    want(cm, *c);
                }
            }
            let pf = &plan.fields[f.field];
            for t in &f.targets {
                want(pf.comodel, *t);
            }
        }
        let names = self.company_names(env, wanted).await?;
        let name = |model: ModelId, id: i64, model_name: &str| -> (String, bool) {
            match names.get(&(model, id)) {
                Some(Some(n)) => (n.clone(), true),
                _ => (format!("{model_name}({id},)"), false),
            }
        };
        let mut items: Vec<CompanyInconsistency> = Vec::with_capacity(shown.len());
        for (i, f) in shown.iter().enumerate() {
            let pf = &plan.fields[f.field];
            let itself = plan.scope == CompanyScope::Itself;
            let record = if itself {
                String::new()
            } else {
                name(plan.model, f.record.get(), spec.name).0
            };
            let companies: Vec<String> = company_model.map_or_else(Vec::new, |cm| {
                companies_of(i)
                    .iter()
                    .map(|c| name(cm, *c, "res.company").0)
                    .collect()
            });
            let comodel = registry.model_by_id(pf.comodel);
            let values: Vec<String> = f
                .targets
                .iter()
                .map(|t| match name(pf.comodel, *t, comodel.name) {
                    (n, true) => py_repr(&n),
                    (placeholder, false) => placeholder,
                })
                .collect();
            let fs = spec.field_by_id(pf.field);
            items.push(CompanyInconsistency::new(
                itself,
                record,
                companies,
                fs.string.to_string(),
                fs.name,
                values,
            ));
        }
        Ok(CompanyError::Inconsistent {
            items,
            total: found.len(),
        }
        .into())
    }
}
