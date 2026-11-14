//! Record checks and their per-transaction memo (design note §3.3).
//!
//! A record check is one `SELECT id FROM t WHERE id = ANY($1) AND (<predicate>)` per chunk of
//! ids, after flushing the fields the predicate reads, on the transaction's own connection
//! (check-then-write is race-free under `REPEATABLE READ`, R-S18). `NULL` predicate results deny
//! (a `WHERE` keeps only `TRUE` rows).
//!
//! The memo keeps, per (model, operation, binding key), the ids a check let through: keyed by
//! the binding **by value** (subject, interned groups and their snapshot, companies, grants,
//! scope, elevation, debug), so two principals never share an entry and older `Env` clones need
//! no special care (R-M1). `Read` and `Write` results are kept (open question 2: the
//! write check of an `onchange` is then free the second time; create and unlink checks are
//! one-shot). An entry is dropped when the transaction writes, creates or deletes something its
//! predicate reads, on a savepoint rollback, at the end of an isolated scope, and never because
//! of an error (a denial is never memoized, SEC-017). The memo holds at most [`MEMO_MAX_IDS`]
//! ids; beyond that a check simply runs again.

use std::sync::Arc;

use erp_domain::{Domain, DomainKind, Operator, Value};
use erp_fields::{Cell, Error, ErrorKind, FieldType, RealId, RecId, Result};
use erp_registry::{FieldId, ModelId, ModelSpec};
use erp_sql::param::{Param, Scalar, ScalarKind};
use erp_sql::quote;
use erp_sql::reference::ReferenceQuery;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    AccessDecision, EvidenceColumn, EvidenceCompany, EvidenceRow, ExplainRequest, MAX_EXPLAIN_IDS,
    Operation, RecordEvidence,
};
use crate::env::Env;
use crate::principal::{BindingKey, CompanyId};
use crate::records::Records;
use crate::search::{Compiled, PlanPolicy};
use crate::transaction::Transaction;

/// Most record ids the memo of one transaction holds.
pub(crate) const MEMO_MAX_IDS: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MemoKey {
    model: ModelId,
    op: Operation,
    binding: BindingKey,
}

#[derive(Debug)]
struct MemoEntry {
    allowed: FxHashSet<RealId>,
    /// What the predicate reads: writing one of these drops the entry.
    reads: Arc<[(ModelId, FieldId)]>,
}

/// The access state of one transaction.
#[derive(Debug, Default)]
pub(crate) struct AccessState {
    memo: FxHashMap<MemoKey, MemoEntry>,
    memo_ids: usize,
}

/// The memo as it was when an isolated scope opened.
#[derive(Debug)]
pub(crate) struct MemoSnapshot {
    memo: FxHashMap<MemoKey, MemoEntry>,
    memo_ids: usize,
}

impl AccessState {
    /// A copy of the memo, to restore when an isolated scope ends.
    pub(crate) fn snapshot(&self) -> MemoSnapshot {
        MemoSnapshot {
            memo: self
                .memo
                .iter()
                .map(|(k, e)| {
                    (
                        k.clone(),
                        MemoEntry {
                            allowed: e.allowed.clone(),
                            reads: Arc::clone(&e.reads),
                        },
                    )
                })
                .collect(),
            memo_ids: self.memo_ids,
        }
    }

    /// The memo of `snapshot` back.
    pub(crate) fn restore(&mut self, snapshot: MemoSnapshot) {
        self.memo = snapshot.memo;
        self.memo_ids = snapshot.memo_ids;
    }

    /// Forget every memoized check.
    pub(crate) fn clear_memo(&mut self) {
        self.memo.clear();
        self.memo_ids = 0;
    }

    /// Forget every memoized check when the memo holds more than `keep` record ids (the end of
    /// a stream's window: what the windows remembered is not carried through the whole stream).
    pub(crate) fn shed(&mut self, keep: usize) {
        if self.memo_ids > keep {
            self.clear_memo();
        }
    }

    /// The record ids memoized now, over every entry.
    #[cfg(feature = "test-faults")]
    pub(crate) fn memo_len(&self) -> usize {
        self.memo_ids
    }

    /// The transaction changed `fields` of `model` (`None`: rows were created or deleted, so
    /// every field): drop the entries whose predicates read one of them. An audit field (which
    /// every write changes, `is_audit`) counts as changed by any write. A predicate that reads a
    /// computed field also ends when a field it is computed from, through any number of stored
    /// computes and models (`graph`), is changed: a stored compute further down the chain is only
    /// marked when the one before it recomputes, which may be after the next decision.
    pub(crate) fn written(
        &mut self,
        model: ModelId,
        fields: Option<&[FieldId]>,
        is_audit: &dyn Fn(ModelId, FieldId) -> bool,
        graph: &crate::graph::ComputeGraph,
    ) {
        if self.memo.is_empty() {
            return;
        }
        let touched = |m: ModelId, f: FieldId| {
            m == model
                && match fields {
                    Some(fields) => fields.contains(&f) || is_audit(m, f),
                    None => true,
                }
        };
        let hit = |reads: &[(ModelId, FieldId)]| {
            reads.iter().any(|(m, f)| {
                touched(*m, *f)
                    || graph
                        .upstream((*m, *f))
                        .iter()
                        .any(|(um, uf)| touched(*um, *uf))
            })
        };
        let mut dropped = 0usize;
        self.memo.retain(|_, entry| {
            let keep = !hit(&entry.reads);
            if !keep {
                dropped += entry.allowed.len();
            }
            keep
        });
        self.memo_ids = self.memo_ids.saturating_sub(dropped);
    }

    fn known(&self, key: &MemoKey) -> Option<&FxHashSet<RealId>> {
        self.memo.get(key).map(|e| &e.allowed)
    }

    fn remember(
        &mut self,
        key: MemoKey,
        allowed: impl IntoIterator<Item = RealId>,
        reads: &Arc<[(ModelId, FieldId)]>,
    ) {
        let entry = self.memo.entry(key).or_insert_with(|| MemoEntry {
            allowed: FxHashSet::default(),
            reads: Arc::clone(reads),
        });
        for id in allowed {
            if self.memo_ids >= MEMO_MAX_IDS {
                return;
            }
            if entry.allowed.insert(id) {
                self.memo_ids += 1;
            }
        }
    }
}

/// Whether the results of `op` checks are memoized.
fn memoized(op: Operation) -> bool {
    matches!(op, Operation::Read | Operation::Write)
}

/// The ids of the rows a company restriction probe read: they come from the table, so an id that
/// is not a database id is a bug, never dropped.
fn real_rows(rows: FxHashSet<RecId>, spec: &ModelSpec) -> Result<FxHashSet<RealId>> {
    rows.into_iter()
        .map(|id| {
            id.as_real().ok_or_else(|| {
                Error::programming(format!(
                    "the company restriction probe of {} returned a non-database id",
                    spec.name
                ))
            })
        })
        .collect()
}

/// `domain` is on `model`.
pub(crate) fn bypass_traversals(
    registry: &erp_registry::Registry,
    model: &ModelSpec,
    domain: &Domain,
) -> Result<Domain> {
    if !has_checked_traversal(registry, model, domain) {
        return Ok(domain.clone());
    }
    let mut failure: Option<Error> = None;
    let out = domain.map_conditions(|node| {
        if failure.is_some() {
            return node.clone();
        }
        let Some(condition) = node.as_condition() else {
            return node.clone();
        };
        match bypass_condition(registry, model, condition) {
            Ok(Some(done)) => done,
            Ok(None) => node.clone(),
            Err(error) => {
                failure = Some(error);
                node.clone()
            }
        }
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(out),
    }
}

/// The relational head of a path condition (`head.rest`) on `model`: its comodel and the rest.
fn relational_path<'r>(
    registry: &'r erp_registry::Registry,
    model: &ModelSpec,
    field_expr: &'r str,
) -> Option<(&'r str, &'r str, &'r ModelSpec)> {
    let (head, rest) = field_expr.split_once('.')?;
    if rest.is_empty() {
        return None;
    }
    let comodel = registry.model(model.field(head)?.ty.comodel()?)?;
    Some((head, rest, comodel))
}

/// The model a sub-domain of `field_expr` on `model` is on, when the registry knows it (an
/// unknown field fails in the optimizer that sees the domain next): the comodel of a relational
/// field, `model` itself for `id` (a hierarchy seed `('id', 'child_of', <domain>)`).
fn sub_model<'r>(
    registry: &'r erp_registry::Registry,
    model: &ModelSpec,
    field_expr: &str,
) -> Option<&'r ModelSpec> {
    let field = model.field(field_expr)?;
    if matches!(field.ty, FieldType::Id) {
        return Some(registry.model_by_id(model.id));
    }
    registry.model(field.ty.comodel()?)
}

fn bypass_condition(
    registry: &erp_registry::Registry,
    model: &ModelSpec,
    condition: &erp_domain::Condition,
) -> Result<Option<Domain>> {
    if let Some((head, rest, comodel)) = relational_path(registry, model, condition.field_expr()) {
        let sub = Domain::condition(rest, condition.operator(), condition.value().clone())?;
        let sub = bypass_traversals(registry, comodel, &sub)?;
        return Domain::condition(head, Operator::AnyBypass, Value::Domain(sub))
            .map(Some)
            .map_err(Error::from);
    }
    let operator = match condition.operator() {
        Operator::Any => Operator::AnyBypass,
        Operator::NotAny => Operator::NotAnyBypass,
        other => other,
    };
    let value = match condition.value() {
        Value::Domain(sub) => match sub_model(registry, model, condition.field_expr()) {
            Some(comodel) => Value::Domain(bypass_traversals(registry, comodel, sub)?),
            // an unknown field: the optimizer refuses it next
            None => Value::Domain(sub.clone()),
        },
        // a reference query holds a predicate on its target model: its traversals bypass too
        // (Amendment 1)
        value if let Some(query) = ReferenceQuery::of(value) => {
            let target = crate::search_method::reference_target(registry, query)?;
            query
                .with_sub(bypass_traversals(registry, target, query.sub())?)
                .into_value()
        }
        other => other.clone(),
    };
    Domain::condition(condition.field_expr(), operator, value)
        .map(Some)
        .map_err(Error::from)
}

fn has_checked_traversal(
    registry: &erp_registry::Registry,
    model: &ModelSpec,
    domain: &Domain,
) -> bool {
    match domain.kind() {
        DomainKind::True | DomainKind::False | DomainKind::Custom(_) => false,
        DomainKind::Not(child) => has_checked_traversal(registry, model, child),
        DomainKind::And(children) | DomainKind::Or(children) => children
            .iter()
            .any(|c| has_checked_traversal(registry, model, c)),
        DomainKind::Condition(condition) => {
            matches!(condition.operator(), Operator::Any | Operator::NotAny)
                || relational_path(registry, model, condition.field_expr()).is_some()
                || matches!(condition.value(), Value::Domain(sub)
                    if sub_model(registry, model, condition.field_expr())
                        .is_some_and(|co| has_checked_traversal(registry, co, sub)))
                || ReferenceQuery::of(condition.value()).is_some_and(|query| {
                    crate::search_method::reference_target(registry, query)
                        .is_ok_and(|target| has_checked_traversal(registry, target, query.sub()))
                })
        }
    }
}

/// Whether every condition of `domain` compares a field of the model itself (no path, no
/// sub-domain, no hierarchy or `access` operator): the shapes a create's INSERT may decide, told
/// apart before anything is compiled (a hierarchy would run its queries for nothing).
fn on_own_fields(domain: &Domain) -> bool {
    match domain.kind() {
        DomainKind::True | DomainKind::False => true,
        DomainKind::Custom(_) => false,
        DomainKind::Not(child) => on_own_fields(child),
        DomainKind::And(children) | DomainKind::Or(children) => children.iter().all(on_own_fields),
        DomainKind::Condition(condition) => {
            !condition.field_expr().contains('.')
                && !matches!(condition.value(), Value::Domain(_))
                && !matches!(
                    condition.operator(),
                    Operator::ChildOf
                        | Operator::ParentOf
                        | Operator::Access
                        | Operator::Any
                        | Operator::NotAny
                        | Operator::AnyBypass
                        | Operator::NotAnyBypass
                )
        }
    }
}

impl Transaction {
    /// The record predicate of `op` on `model` for `env`, its own relational traversals
    /// bypassing the comodels' rules: what searches AND in and record checks select with.
    ///
    /// # Errors
    ///
    /// The engine's and the policy's errors.
    pub(crate) fn access_predicate(
        &self,
        env: &Env,
        model: ModelId,
        op: Operation,
    ) -> Result<Domain> {
        let access = self.access_domain(env, model, op)?;
        bypass_traversals(
            self.registry(),
            self.registry().model_by_id(model),
            access.domain(),
        )
    }

    /// New records are kept (their origin
    /// is checked by the caller).
    ///
    /// # Errors
    ///
    /// The engine's, the policy's and the statement's errors.
    pub(crate) async fn filtered_access(
        &mut self,
        env: &Env,
        model: ModelId,
        ids: &[RecId],
        op: Operation,
    ) -> Result<FxHashSet<RecId>> {
        let mut out: FxHashSet<RecId> = ids.iter().copied().filter(|i| i.is_new()).collect();
        let mut seen = FxHashSet::default();
        let real: Vec<RealId> = ids
            .iter()
            .filter_map(|i| i.as_real())
            .filter(|i| seen.insert(*i))
            .collect();
        if real.is_empty() {
            return Ok(out);
        }
        if env.su() {
            out.extend(real.into_iter().map(RecId::real));
            return Ok(out);
        }
        self.prepare_access(env).await?;
        let predicate = self.access_predicate(env, model, op)?;
        if predicate.is_true() {
            out.extend(real.into_iter().map(RecId::real));
            return Ok(out);
        }
        if predicate.is_false() {
            return Ok(out);
        }
        let key = self.memo_key(env, model, op)?;
        let unknown: Vec<RealId> = match key.as_ref().and_then(|k| self.access.known(k)) {
            Some(known) => {
                let (hit, miss): (Vec<RealId>, Vec<RealId>) =
                    real.iter().partition(|id| known.contains(id));
                out.extend(hit.into_iter().map(RecId::real));
                miss
            }
            None => real,
        };
        if unknown.is_empty() {
            return Ok(out);
        }
        let registry = Arc::clone(env.registry());
        let spec = registry.model_by_id(model);
        let probe = Records::from_parts(
            env.clone(),
            model,
            Arc::from(Vec::<RecId>::new()),
            Arc::from(Vec::<RecId>::new()),
        );
        let filter = crate::search::compile_predicate(self, &probe, &predicate).await?;
        crate::search::flush_read_inputs(self, &filter.fields).await?;
        let visible =
            crate::search::fetch_rows(self, spec, &[], &unknown, Some(filter.as_ref())).await?;
        // a decision made with a start set found by a search is never remembered
        if let Some(key) = key
            && !filter.volatile
        {
            let reads: Arc<[(ModelId, FieldId)]> = Arc::from(filter.fields.clone());
            self.access
                .remember(key, visible.iter().filter_map(|id| id.as_real()), &reads);
        }
        out.extend(visible);
        Ok(out)
    }

    /// Records that no longer
    /// exist are left to the operation itself (it reports them missing).
    ///
    /// # Errors
    ///
    /// `Access` naming the first existing record the predicate hides; the statements' errors.
    pub(crate) async fn check_record_access(
        &mut self,
        records: &Records,
        op: Operation,
    ) -> Result<()> {
        let env = records.env().clone();
        if env.su() || records.is_empty() {
            return Ok(());
        }
        let allowed = self
            .filtered_access(&env, records.model_id(), records.rec_ids(), op)
            .await?;
        let mut seen = FxHashSet::default();
        let denied: Vec<RealId> = records
            .rec_ids()
            .iter()
            .filter(|id| !allowed.contains(id))
            .filter_map(|id| id.as_real())
            .filter(|id| seen.insert(*id))
            .collect();
        if denied.is_empty() {
            return Ok(());
        }
        match self
            .record_access_error(&env, records.model_id(), op, &denied)
            .await?
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The new state of records a write moved (design `m3-company-a1` §6.5): when `written` holds
    /// an anchor of the model's generated company restriction (the company field or the owner
    /// edge), every database record of `records` must satisfy the restriction **now**, after the
    /// values are in. One statement per chunk of the written records; nothing for superuser mode, for a
    /// model without a generated restriction and for a write of other fields. Permissions and
    /// handwritten restrictions are checked before the write, not here.
    ///
    /// # Errors
    ///
    /// `Access` with the record-level text of a denied `write` (the generated rule blamed, the
    /// multi-company hint); the policy's and the statements' errors.
    pub(crate) async fn check_generated_restriction(
        &mut self,
        records: &Records,
        written: &[FieldId],
    ) -> Result<()> {
        let env = records.env().clone();
        if env.su() || records.is_empty() {
            return Ok(());
        }
        let registry = Arc::clone(env.registry());
        let model = records.model_id();
        let spec = registry.model_by_id(model);
        let erp_registry::CompanyAccessState::Declared(declared) = spec.company_access else {
            return Ok(());
        };
        if !declared.generates() || !declared.anchors().any(|a| written.contains(&a)) {
            return Ok(());
        }
        let mut seen = FxHashSet::default();
        let ids: Vec<RealId> = records
            .rec_ids()
            .iter()
            .filter_map(|i| i.as_real())
            .filter(|i| seen.insert(*i))
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        self.prepare_access(&env).await?;
        let restriction = {
            let binding = self.binding(&env)?;
            self.policy().generated_restriction(&binding, spec)?
        };
        let Some(restriction) = restriction else {
            return Ok(());
        };
        let visible = self
            .restriction_visible(&env, spec, &restriction, &ids)
            .await?;
        let denied: Vec<RealId> = ids.into_iter().filter(|id| !visible.contains(id)).collect();
        if denied.is_empty() {
            return Ok(());
        }
        // the denied records were just written: an explanation that finds none of them is a bug,
        // never an allowance
        match self
            .record_access_error(&env, model, Operation::Write, &denied)
            .await?
        {
            Some(error) => Err(error),
            None => Err(Error::programming(format!(
                "the generated company restriction of {} denied records that no longer exist \
                 after the write",
                spec.name
            ))),
        }
    }

    /// The records among `ids` of `model` that `principal` may see under the model's declared
    /// (generated) company restriction, with `companies` as the active-company set, decided by
    /// `policy`, not by this transaction's policy. Missing and hidden ids are both
    /// absent from the answer, so a caller cannot tell them apart. Evaluated on this transaction's
    /// snapshot, one statement per chunk of ids; writes no access memo and grants nothing.
    ///
    /// A check for core services that hold a policy and a principal built from the same snapshot
    /// (Auth's administrator password reset); not an addon API. It can only narrow: the answer is a
    /// subset of `ids`.
    ///
    /// The probe acts as `principal` with `companies` named through the context (an empty context
    /// would fall back to the principal's own companies), in superuser mode: the restriction's own
    /// relational traversals bypass the comodels' rules as in every record check, and
    /// `company_ids` binds to `companies`. An empty `companies` answers nothing after asking the
    /// policy for its deny variant: the deny variant of a generated restriction admits nothing,
    /// and no environment can name an empty company set.
    ///
    /// Returns the visible ids in input order, each once.
    ///
    /// Public for the core services that decide for a human in a transaction running under
    /// another policy (the administrator password reset of `erp-security`); it is not
    /// an addon API. It only narrows: it grants nothing and writes nothing, so a caller-supplied
    /// policy and principal cannot widen access.
    ///
    /// # Errors
    ///
    /// `Programming`: an unknown model; a model whose `company_access` declares no generated
    /// restriction; a policy answering `None` for one that does; an elevated or `System`
    /// principal; a non-positive id. `Access`: a company outside the principal's companies
    /// ("One of the selected companies does not exist or is not one of yours."); the policy's drift refusal. The
    /// statements' errors.
    pub async fn visible_under_company_restriction(
        &mut self,
        model: &str,
        ids: &[i64],
        policy: &dyn super::AccessPolicy,
        principal: &crate::principal::Principal,
        companies: &[CompanyId],
    ) -> Result<Vec<i64>> {
        const CALL: &str = "visible_under_company_restriction";
        if principal.kind() == crate::principal::PrincipalKind::System
            || !matches!(principal.elevation(), crate::principal::Elevation::None)
        {
            return Err(Error::programming(format!(
                "{CALL} decides for a human principal: the {} principal of user {} is elevated or \
                 the system",
                principal.kind().as_str(),
                principal.subject()
            )));
        }
        let registry = Arc::clone(self.registry());
        let spec = registry
            .model(model)
            .ok_or_else(|| Error::programming(format!("{CALL}: unknown model '{model}'")))?;
        let generates = matches!(
            spec.company_access,
            erp_registry::CompanyAccessState::Declared(declared) if declared.generates()
        );
        if !generates {
            return Err(Error::programming(format!(
                "{CALL}: model '{model}' declares no generated company restriction"
            )));
        }
        let mut seen = FxHashSet::default();
        let mut wanted: Vec<RealId> = Vec::with_capacity(ids.len());
        for id in ids {
            let real = RealId::new(*id)
                .ok_or_else(|| Error::programming(format!("{CALL}: invalid record id {id}")))?;
            if seen.insert(real) {
                wanted.push(real);
            }
        }
        let active = crate::principal::ActiveCompanySet::of(companies);
        let restriction = {
            let binding = crate::principal::PrincipalBinding::bind(
                policy,
                principal,
                Arc::clone(principal.resolution()),
            )?
            .with_companies(active);
            policy.generated_restriction(&binding, spec)?
        };
        let Some(restriction) = restriction else {
            return Err(Error::programming(format!(
                "the access policy answers no generated company restriction for '{model}', which \
                 declares one"
            )));
        };
        if companies.is_empty() {
            return Ok(Vec::new());
        }
        let mut context = crate::context::Context::new();
        context.set(
            "allowed_company_ids",
            erp_fields::Value::List(
                companies
                    .iter()
                    .map(|c| erp_fields::Value::Int(c.get()))
                    .collect(),
            ),
        );
        let probe = self.env_for(principal.clone(), context).engine_sudo();
        // the refusal of a company outside the principal's is lazy: surface it before any shortcut
        probe.companies()?;
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let visible = if restriction.is_true() {
            // a constant-true restriction admits every row, but the answer still names only rows
            // that exist: a missing id stays absent, as the contract says
            let rows = crate::search::fetch_rows(self, spec, &[], &wanted, None).await?;
            real_rows(rows, spec)?
        } else {
            self.restriction_visible(&probe, spec, &restriction, &wanted)
                .await?
        };
        Ok(wanted
            .into_iter()
            .filter(|id| visible.contains(id))
            .map(RealId::get)
            .collect())
    }

    /// The records among `ids` that `restriction` (a generated company restriction of `spec`'s
    /// model) admits, evaluated in `env` (shared by the write path's new-state check and
    /// [`Transaction::visible_under_company_restriction`]): constants answer at once, otherwise
    /// one statement per chunk after flushing what the restriction reads.
    async fn restriction_visible(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        restriction: &super::AccessDomain,
        ids: &[RealId],
    ) -> Result<FxHashSet<RealId>> {
        if restriction.is_false() {
            return Ok(FxHashSet::default());
        }
        if restriction.is_true() {
            return Ok(ids.iter().copied().collect());
        }
        let predicate = bypass_traversals(env.registry(), spec, restriction.domain())?;
        let probe = Records::from_parts(
            env.clone(),
            spec.id,
            Arc::from(Vec::<RecId>::new()),
            Arc::from(Vec::<RecId>::new()),
        );
        let filter = crate::search::compile_predicate(self, &probe, &predicate).await?;
        crate::search::flush_read_inputs(self, &filter.fields).await?;
        let rows = crate::search::fetch_rows(self, spec, &[], ids, Some(filter.as_ref())).await?;
        real_rows(rows, spec)
    }

    /// The error of a record check that let none of `denied` through (
    /// (each id once, in order of first occurrence, whatever the caller passes).
    /// `None` when none of `denied`
    /// exists any more (the operation reports them missing). Callers ran the model check.
    ///
    /// # Errors
    ///
    /// The statements' errors; `Programming` when the policy explains the denial as allowed or
    /// renders no text for it.
    pub(crate) async fn record_access_error(
        &mut self,
        env: &Env,
        model: ModelId,
        op: Operation,
        denied: &[RealId],
    ) -> Result<Option<Error>> {
        // the evidence statement lists each record once, in the order of first occurrence: a
        // repeated id would repeat its row (`unnest` keeps duplicates, unlike `= ANY`)
        let mut seen = FxHashSet::default();
        let denied: Vec<RealId> = denied
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect();
        let denied = denied.as_slice();
        if denied.is_empty() {
            return Ok(None);
        }
        let registry = Arc::clone(env.registry());
        let spec = registry.model_by_id(model);
        let asked: Vec<i64> = denied
            .iter()
            .take(MAX_EXPLAIN_IDS)
            .map(|id| id.get())
            .collect();
        let (columns, list_names) = {
            let binding = self.binding(env)?;
            let request = ExplainRequest::new(&binding, spec, op).ids(&asked)?;
            let answer = self.policy().explain(&request)?;
            if let Some(message) = answer.message() {
                return Ok(Some(Error::access(message)));
            }
            if answer.decision() != AccessDecision::Deny {
                return Err(Error::programming(format!(
                    "the access policy explains {op} on {} as allowed for records its predicate \
                     denied",
                    spec.name
                )));
            }
            (answer.columns().to_vec(), answer.lists_records())
        };
        // boxed: the evidence reads display names, and a read may raise this error again
        let Some(evidence) =
            Box::pin(self.access_evidence(env, spec, columns, list_names, denied)).await?
        else {
            // no evidence row: either the records are gone, or the statement lost an existing
            // one (a join): the latter stays a denial, never an allowance
            return self.denial_without_evidence(spec, op, denied).await;
        };
        let ids: Vec<i64> = evidence.rows().iter().map(EvidenceRow::id).collect();
        let binding = self.binding(env)?;
        let request = ExplainRequest::new(&binding, spec, op)
            .ids(&ids)?
            .evidence(&evidence);
        let explained = self.policy().explain(&request)?;
        let message = explained.message().ok_or_else(|| {
            Error::programming(format!(
                "the access policy rendered no text for the record-level denial of {op} on {}",
                spec.name
            ))
        })?;
        let error = Error::access(message);
        Ok(Some(match explained.context() {
            Some(context) => error.with_context(context.clone()),
            None => error,
        }))
    }

    /// A denial the evidence statement returned no row for: the first of `denied` that still
    /// exists in the table, as an `Access` error that names the model and the operation (fail
    /// closed: the records were denied, so a missing explanation must not turn into an allowance);
    /// `None` when none of them exists any more.
    async fn denial_without_evidence(
        &mut self,
        spec: &ModelSpec,
        op: Operation,
        denied: &[RealId],
    ) -> Result<Option<Error>> {
        let (t, id) = (quote(&spec.table), quote("id"));
        let sql =
            format!("SELECT {id} FROM {t} WHERE {id} = ANY($1::int8[]) ORDER BY {id} LIMIT 1");
        let params = [Param::Array(
            ScalarKind::Int8,
            denied.iter().map(|id| Scalar::Int8(id.get())).collect(),
        )];
        let rows = self
            .run_params_with(&sql, &params, PlanPolicy::PerCall)
            .await?;
        let existing = match rows.first() {
            Some(row) => Some(row.try_get::<_, i64>(0).map_err(|e| {
                Error::new(
                    ErrorKind::Database,
                    "could not decode an existing record id",
                )
                .with_source(e)
            })?),
            None => None,
        };
        Ok(unexplained_denial(spec.name, op, existing))
    }

    /// `None` when none of `denied` exists.
    ///
    /// ```sql
    /// SELECT "t"."id", (<rule 1>) IS TRUE, …, (SELECT "__u"."name" FROM "res_users" AS "__u"
    ///   WHERE "__u"."id" = $k::int8) FROM unnest($n::int8[]) WITH ORDINALITY AS "__d"("id", "ord")
    ///   JOIN "t" ON "t"."id" = "__d"."id" <joins> ORDER BY "__d"."ord" LIMIT 6
    /// ```
    ///
    /// # Errors
    ///
    /// The statements' errors; `Programming` when `res.users` has no stored `name` or the subject
    /// has no `res.users` row.
    async fn access_evidence(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        columns: Vec<EvidenceColumn>,
        list_names: bool,
        denied: &[RealId],
    ) -> Result<Option<RecordEvidence>> {
        let registry = Arc::clone(env.registry());
        let probe = Records::from_parts(
            env.clone(),
            spec.id,
            Arc::from(Vec::<RecId>::new()),
            Arc::from(Vec::<RecId>::new()),
        );
        let mut params: Vec<Param> = Vec::new();
        let mut selects: Vec<String> = Vec::with_capacity(columns.len());
        let mut compiled: Vec<Arc<Compiled>> = Vec::with_capacity(columns.len());
        for column in &columns {
            let predicate = bypass_traversals(&registry, spec, column.domain())?;
            let c =
                crate::search::compile_predicate_at(self, &probe, &predicate, params.len()).await?;
            match c.constant {
                Some(true) => selects.push("TRUE".to_owned()),
                Some(false) => selects.push("FALSE".to_owned()),
                None => {
                    params.extend(c.params.iter().cloned());
                    selects.push(format!("({}) IS TRUE", c.sql));
                    compiled.push(c);
                }
            }
        }
        let fields: Vec<(ModelId, FieldId)> = compiled
            .iter()
            .flat_map(|c| c.fields.iter().copied())
            .collect();
        crate::search::flush_read_inputs(self, &fields).await?;
        let joins = crate::search::merge_joins_checked(
            &compiled
                .iter()
                .map(|c| &*c.join_list)
                .collect::<Vec<&[(String, String)]>>(),
        )?;
        let users = registry.model("res.users").ok_or_else(|| {
            Error::programming("access evidence needs the res.users model (the subject's name)")
        })?;
        let name = users
            .field("name")
            .filter(|f| f.has_column())
            .ok_or_else(|| {
                Error::programming("access evidence needs the stored res.users.name field")
            })?;
        params.push(Param::Scalar(Scalar::Int8(env.uid())));
        let uid_param = params.len();
        params.push(Param::Array(
            ScalarKind::Int8,
            denied.iter().map(|id| Scalar::Int8(id.get())).collect(),
        ));
        let ids_param = params.len();
        let (t, id) = (quote(&spec.table), quote("id"));
        let company_read = company_evidence_sql(&registry, spec, env, &mut params)?;
        let mut sql = format!("SELECT {t}.{id}");
        for select in &selects {
            sql.push_str(", ");
            sql.push_str(select);
        }
        sql.push_str(&format!(
            ", (SELECT \"__u\".{} FROM {} AS \"__u\" WHERE \"__u\".{id} = ${uid_param}::int8){} \
             FROM unnest(${ids_param}::int8[]) WITH ORDINALITY AS \"__d\"({id}, \"ord\") \
             JOIN {t} ON {t}.{id} = \"__d\".{id}{joins} ORDER BY \"__d\".\"ord\" \
             LIMIT {MAX_EXPLAIN_IDS}",
            quote(name.name),
            quote(&users.table),
            company_read.sql()
        ));
        let rows = self
            .run_params_with(&sql, &params, PlanPolicy::PerCall)
            .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let decode = |e: tokio_postgres::Error| {
            Error::new(ErrorKind::Database, "could not decode the access evidence").with_source(e)
        };
        let mut found: Vec<Found> = Vec::with_capacity(rows.len());
        let mut subject_name: Option<String> = None;
        for row in &rows {
            let record: i64 = row.try_get(0).map_err(decode)?;
            let mut results = Vec::with_capacity(selects.len());
            for i in 0..selects.len() {
                results.push(row.try_get::<_, bool>(i + 1).map_err(decode)?);
            }
            subject_name = row
                .try_get::<_, Option<String>>(selects.len() + 1)
                .map_err(decode)?;
            let company = company_read.decode(row, selects.len() + 2, decode)?;
            found.push(Found {
                record,
                results,
                company,
            });
        }
        let subject_name = subject_name.ok_or_else(|| {
            Error::programming(format!(
                "the subject {} of a record-level access check has no res.users row",
                env.uid()
            ))
        })?;
        // a record of a company outside the allowed ones is listed without its name (rule 7.5)
        let allowed = env.companies()?;
        let redacted: Vec<bool> = found
            .iter()
            .map(|f| f.company.is_hidden_from(allowed))
            .collect();
        let names = if list_names {
            self.evidence_display_names(env, spec, &found, &redacted)
                .await?
        } else {
            vec![None; found.len()]
        };
        let first_company = found.iter().find_map(|f| f.company.first_of_many());
        let rows = found
            .into_iter()
            .zip(names)
            .zip(redacted)
            .map(|((f, name), hidden)| {
                EvidenceRow::new(f.record, name, f.results)
                    .with_company(f.company.single())
                    .with_name_redacted(hidden)
            })
            .collect();
        let evidence = RecordEvidence::new(columns, rows, subject_name)?;
        Ok(Some(if company_read.is_many() {
            evidence.with_first_company(first_company)
        } else {
            evidence
        }))
    }

    async fn evidence_display_names(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        found: &[Found],
        redacted: &[bool],
    ) -> Result<Vec<Option<String>>> {
        let mut shown: Vec<(usize, RecId)> = Vec::with_capacity(found.len());
        for (i, f) in found.iter().enumerate() {
            if redacted[i] {
                continue;
            }
            let id = RecId::from_real(f.record).ok_or_else(|| {
                Error::programming(format!("access evidence returned the id {}", f.record))
            })?;
            shown.push((i, id));
        }
        let mut names: Vec<Option<String>> = vec![None; found.len()];
        if shown.is_empty() {
            return Ok(names);
        }
        let su = Records::empty(env.engine_sudo(), spec.id)
            .browse_ids(shown.iter().map(|(_, id)| *id).collect::<Vec<_>>());
        // read for the asking reader: a label that depends on its reader (a vendor code in a product
        // label) must not answer with the superuser's view in an access error
        let cells = self.display_names_for(&su, env).await?;
        for ((i, _), cell) in shown.into_iter().zip(cells) {
            names[i] = match cell {
                Cell::Str(name) => Some(name.to_string()),
                _ => None,
            };
        }
        Ok(names)
    }

    /// The create check fused into the INSERT: the compiled create
    /// predicate, its placeholders after `offset` (the INSERT's own parameters), when it can be
    /// decided on the inserted row itself. That is: not superuser, a shared policy snapshot (no
    /// statement of a private one is kept on the connection), a predicate that restricts, needs no
    /// join, reads no other row of the model's table (the INSERT's sub-queries do not see the rows
    /// it inserts; also the policy's `reads_own_table`), and reads only the model's stored columns
    /// that the INSERT writes for good (`late`: columns the create writes after the INSERT, its
    /// stored computes and parent paths). `None`: the check runs after the create, as before.
    ///
    /// # Errors
    ///
    /// The engine's and the policy's errors, the compilation's.
    pub(crate) async fn fused_create_check(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        late: &FxHashSet<FieldId>,
        offset: usize,
    ) -> Result<Option<Arc<Compiled>>> {
        if env.su() || !self.policy().snapshot().is_shared() {
            return Ok(None);
        }
        let access = self.access_domain(env, spec.id, Operation::Create)?;
        if access.domain().is_true()
            || access.domain().is_false()
            || access.reads_own_table()
            || !on_own_fields(access.domain())
        {
            return Ok(None);
        }
        let predicate = bypass_traversals(env.registry(), spec, access.domain())?;
        let probe = Records::from_parts(
            env.clone(),
            spec.id,
            Arc::from(Vec::<RecId>::new()),
            Arc::from(Vec::<RecId>::new()),
        );
        let compiled =
            crate::search::compile_predicate_at(self, &probe, &predicate, offset).await?;
        let own_columns = compiled.fields.iter().all(|(model, field)| {
            *model == spec.id && !late.contains(field) && spec.field_by_id(*field).has_column()
        });
        if compiled.constant.is_some()
            || !compiled.join_list.is_empty()
            || compiled.reads_own_rows
            || !own_columns
        {
            return Ok(None);
        }
        Ok(Some(compiled))
    }

    /// The end of a create whose check was fused into its INSERT: `denied` are the inserted
    /// records the predicate let through as false or NULL; the same record-level error as a
    /// separate check raises.
    ///
    /// # Errors
    ///
    /// The record-level `Access` error; the error path's statements' errors.
    pub(crate) async fn fused_create_denied(
        &mut self,
        env: &Env,
        model: ModelId,
        denied: &[RealId],
    ) -> Result<()> {
        if denied.is_empty() {
            return Ok(());
        }
        match self
            .record_access_error(env, model, Operation::Create, denied)
            .await?
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The memo key of `op` checks on `model` for `env` (`None` for operations not memoized).
    fn memo_key(&self, env: &Env, model: ModelId, op: Operation) -> Result<Option<MemoKey>> {
        if !memoized(op) {
            return Ok(None);
        }
        let binding = self.binding(env)?;
        Ok(Some(MemoKey {
            model,
            op,
            binding: binding.key(self.policy().snapshot().id()),
        }))
    }

    /// Whether any id among `ids` is not known to be readable by `env` (no copy of `ids`).
    pub(crate) fn has_unknown_read(
        &self,
        env: &Env,
        model: ModelId,
        ids: &[RealId],
    ) -> Result<bool> {
        if ids.is_empty() {
            return Ok(false);
        }
        let Some(key) = self.memo_key(env, model, Operation::Read)? else {
            return Ok(true);
        };
        Ok(match self.access.known(&key) {
            Some(known) => ids.iter().any(|i| !known.contains(i)),
            None => true,
        })
    }

    /// Remember that `env` may read `ids` (a fetch through the read predicate found them).
    pub(crate) fn remember_read(
        &mut self,
        env: &Env,
        model: ModelId,
        ids: impl IntoIterator<Item = RealId>,
        reads: &[(ModelId, FieldId)],
    ) -> Result<()> {
        if let Some(key) = self.memo_key(env, model, Operation::Read)? {
            let reads: Arc<[(ModelId, FieldId)]> = Arc::from(reads);
            self.access.remember(key, ids, &reads);
        }
        Ok(())
    }

    /// The transaction changed `fields` of `model` (`None`: created or deleted rows): drop the
    /// memoized checks whose predicates read them.
    fn access_written_in(&mut self, model: ModelId, fields: Option<&[FieldId]>) {
        let shared = Arc::clone(&self.shared);
        self.access.written(
            model,
            fields,
            &|m, f| shared.registry.model_by_id(m).field_by_id(f).magic,
            &shared.graph,
        );
    }

    /// The transaction changed `fields` of `model` (`None`: created or deleted rows): drop the
    /// memoized checks whose predicates read them (see [`AccessState::written`]).
    pub(crate) fn access_written(&mut self, model: ModelId, fields: Option<&[FieldId]>) {
        // Generic default CRUD has the same cache contract as the helper API. Property
        // consumers depend on these rows without a record-field graph edge.
        if self.shared.registry.model_by_id(model).name == "ir.default" {
            self.invalidate_ormcache("ir.default");
        }
        // Named digits are transaction metadata derived from these rows. This
        // boundary covers create, write, unlink, upsert and explicit model invalidation.
        if self.shared.registry.model_by_id(model).name == "decimal.precision" {
            self.engine.precisions.clear();
            self.engine.write_precisions.clear();
        }
        self.access_written_in(model, fields);
        let Some(fields) = fields else { return };
        // a many2many is one link table seen from both models: writing a field also changes
        // every sibling many2many field on the comodel that shares the relation table
        let registry = Arc::clone(&self.shared.registry);
        let spec = registry.model_by_id(model);
        for fid in fields {
            let field = spec.field_by_id(*fid);
            let FieldType::Many2many {
                comodel,
                relation: Some(rel),
                ..
            } = &field.ty
            else {
                continue;
            };
            let Some(co) = registry.model(comodel) else {
                // an unknown comodel: every entry reading it may be stale
                self.access.clear_memo();
                continue;
            };
            // the fields sharing the relation table, on the comodel (the mirror) and on the
            // model itself (a second field over the same table)
            for target in [co, spec] {
                let siblings: Vec<FieldId> = target
                    .fields
                    .iter()
                    .filter(|f| {
                        matches!(
                            &f.ty,
                            FieldType::Many2many { relation: Some(r), .. } if r.table == rel.table
                        )
                    })
                    .map(|f| f.id)
                    .collect();
                if !siblings.is_empty() {
                    self.access_written_in(target.id, Some(&siblings));
                }
            }
        }
    }
}

/// The error of a denied record whose evidence statement returned no row (`existing`: the first
/// denied record that still exists), `None` when none exists. `Access`, not `Programming`: the
/// policy denied the record, and callers such as `has_access` must see a refusal.
fn unexplained_denial(model: &str, op: Operation, existing: Option<i64>) -> Option<Error> {
    existing.map(|id| {
        Error::access(format!(
            "Access denied: the {op} operation on '{model}' (record {id}) is refused by the access \
             policy; the reason could not be listed."
        ))
    })
}

impl Records {
    /// Superuser mode passes; new records are not checked
    /// (their origin is).
    ///
    /// ```no_run
    /// # async fn example(tx: &mut erp_orm::Transaction, records: &erp_orm::Records) -> erp_fields::Result<()> {
    /// records.check_access(tx, erp_orm::Operation::Write).await?;
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// `Access`: the model-level refusal, or the record-level one naming the first existing
    /// record the predicate hides.
    pub async fn check_access(&self, tx: &mut Transaction, op: Operation) -> Result<()> {
        tx.admit_records(self).await?;
        let env = self.env().clone();
        tx.prepare_access(&env).await?;
        tx.check_model(&env, self.model_id(), op)?;
        tx.check_record_access(self, op).await
    }

    /// # Errors
    ///
    /// Only failures that are not refusals (a policy error, a lost connection).
    pub async fn has_access(&self, tx: &mut Transaction, op: Operation) -> Result<bool> {
        // a refused company context is an error, never "no access" (review F1/S5)
        self.env().companies()?;
        match self.check_access(tx, op).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == erp_fields::ErrorKind::Access => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// # Errors
    ///
    /// Failures that are not refusals.
    pub async fn filtered_access(&self, tx: &mut Transaction, op: Operation) -> Result<Records> {
        tx.admit_records(self).await?;
        let env = self.env().clone();
        // a refused company context is an error, never "no records" (review F1/S5)
        env.companies()?;
        tx.prepare_access(&env).await?;
        match tx.check_model(&env, self.model_id(), op) {
            Ok(()) => {}
            Err(error) if error.kind() == erp_fields::ErrorKind::Access => {
                return Ok(self.derive(Vec::new()));
            }
            Err(error) => return Err(error),
        }
        let allowed = tx
            .filtered_access(&env, self.model_id(), self.rec_ids(), op)
            .await?;
        let kept: Vec<RecId> = self
            .rec_ids()
            .iter()
            .copied()
            .filter(|id| allowed.contains(id))
            .collect();
        Ok(self.derive(kept))
    }
}

/// One row of the evidence statement before it becomes an [`EvidenceRow`].
struct Found {
    record: i64,
    results: Vec<bool>,
    company: RecordCompany,
}

/// The companies of one denied record as the statement read them.
enum RecordCompany {
    /// The model has no company field.
    Unscoped,
    /// `company_id`: the id (`None` when unset) and its name when the subject is in it.
    Single(Option<(CompanyId, Option<String>)>),
    /// `company_ids`: every company of the record, and the first one the subject is in.
    Many {
        all: Vec<CompanyId>,
        first_own: Option<(CompanyId, String)>,
    },
}

impl RecordCompany {
    /// Whether the record belongs to companies and none of them is allowed: its name is
    /// withheld. A record without company is shown.
    fn is_hidden_from(&self, allowed: &[CompanyId]) -> bool {
        let is_allowed = |id: &CompanyId| allowed.contains(id);
        match self {
            Self::Unscoped | Self::Single(None) => false,
            Self::Single(Some((id, _))) => !is_allowed(id),
            Self::Many { all, .. } => !all.is_empty() && !all.iter().any(is_allowed),
        }
    }

    /// The `company_id` of the record, for the row.
    fn single(&self) -> Option<EvidenceCompany> {
        match self {
            Self::Single(Some((id, Some(name)))) => Some(EvidenceCompany::own(*id, name.clone())),
            Self::Single(Some((id, None))) => Some(EvidenceCompany::foreign(*id)),
            _ => None,
        }
    }

    /// The first own company of a `company_ids` record.
    fn first_of_many(&self) -> Option<EvidenceCompany> {
        match self {
            Self::Many {
                first_own: Some((id, name)),
                ..
            } => Some(EvidenceCompany::own(*id, name.clone())),
            _ => None,
        }
    }
}

/// The company columns of the evidence statement for one model, and how to decode them.
enum CompanyRead {
    None,
    Single,
    Many,
}

/// The SQL of the company columns appended to the evidence statement (empty for a model that is
/// not company-scoped) and its decoding. Zero extra statements: the columns ride on the
/// statement that already reads the denied records.
struct CompanyColumns {
    read: CompanyRead,
    sql: String,
}

impl CompanyColumns {
    fn sql(&self) -> &str {
        &self.sql
    }

    fn is_many(&self) -> bool {
        matches!(self.read, CompanyRead::Many)
    }

    fn decode(
        &self,
        row: &tokio_postgres::Row,
        at: usize,
        decode: impl Fn(tokio_postgres::Error) -> Error,
    ) -> Result<RecordCompany> {
        let company = |id: i64| {
            CompanyId::new(id).map_err(|e| {
                Error::new(
                    ErrorKind::Database,
                    format!("access evidence returned the company id {id}"),
                )
                .with_source(e)
            })
        };
        Ok(match self.read {
            CompanyRead::None => RecordCompany::Unscoped,
            CompanyRead::Single => {
                let id: Option<i64> = row.try_get(at).map_err(&decode)?;
                let name: Option<String> = row.try_get(at + 1).map_err(&decode)?;
                RecordCompany::Single(id.map(|id| company(id).map(|c| (c, name))).transpose()?)
            }
            CompanyRead::Many => {
                let all: Vec<i64> = row.try_get(at).map_err(&decode)?;
                let id: Option<i64> = row.try_get(at + 1).map_err(&decode)?;
                let name: Option<String> = row.try_get(at + 2).map_err(&decode)?;
                RecordCompany::Many {
                    all: all.into_iter().map(company).collect::<Result<_>>()?,
                    first_own: id
                        .zip(name)
                        .map(|(id, name)| company(id).map(|c| (c, name)))
                        .transpose()?,
                }
            }
        })
    }
}

/// Build the company columns of the evidence statement for `spec`; pushes the subject's companies
/// as a parameter when the model is company-scoped. `res.company`'s display name is its `name`.
///
/// # Errors
///
/// `Programming` when the model's company field is not what the registry's scope says, or
/// `res.company` has no stored `name`.
fn company_evidence_sql(
    registry: &erp_registry::Registry,
    spec: &ModelSpec,
    env: &Env,
    params: &mut Vec<Param>,
) -> Result<CompanyColumns> {
    use erp_registry::CompanyScope;
    let none = || CompanyColumns {
        read: CompanyRead::None,
        sql: String::new(),
    };
    let (field_id, many) = match spec.company_scope {
        Some(CompanyScope::Single { field, .. }) => (field, false),
        Some(CompanyScope::Many { field, .. }) => (field, true),
        Some(CompanyScope::Itself) | None => return Ok(none()),
        Some(other) => {
            return Err(Error::programming(format!(
                "access evidence does not know the company scope {other:?} of {}",
                spec.name
            )));
        }
    };
    let companies = registry.model("res.company").ok_or_else(|| {
        Error::programming(format!(
            "access evidence of {} needs the res.company model",
            spec.name
        ))
    })?;
    let name = companies
        .field("name")
        .filter(|f| f.has_column())
        .ok_or_else(|| {
            Error::programming("access evidence needs the stored res.company.name field")
        })?;
    let field = spec.field_by_id(field_id);
    params.push(Param::Array(
        ScalarKind::Int8,
        env.principal()
            .companies()
            .all_of_subject()
            .iter()
            .map(|c| Scalar::Int8(c.get()))
            .collect(),
    ));
    let own = params.len();
    let (t, id) = (quote(&spec.table), quote("id"));
    let (co, name_col) = (quote(&companies.table), quote(name.name));
    if !many {
        let col = quote(field.name);
        return Ok(CompanyColumns {
            read: CompanyRead::Single,
            sql: format!(
                ", {t}.{col}::int8, (SELECT COALESCE(\"__c\".{name_col}::text, '') FROM {co} AS \
                 \"__c\" WHERE \"__c\".{id} = {t}.{col} AND \"__c\".{id} = ANY(${own}::int8[]))"
            ),
        });
    }
    let FieldType::Many2many {
        relation: Some(rel),
        ..
    } = &field.ty
    else {
        return Err(Error::programming(format!(
            "access evidence: {}.{} is not a stored many2many field",
            spec.name, field.name
        )));
    };
    let (table, c1, c2) = (quote(&rel.table), quote(&rel.column1), quote(&rel.column2));
    let order = if companies.field("sequence").is_some_and(|f| f.has_column()) {
        format!(
            "\"__c\".{}, \"__c\".{name_col}, \"__c\".{id}",
            quote("sequence")
        )
    } else {
        format!("\"__c\".{id}")
    };
    let first = |expr: &str| {
        format!(
            "(SELECT {expr} FROM {co} AS \"__c\" JOIN {table} AS \"__r\" ON \
             \"__r\".{c2} = \"__c\".{id} WHERE \"__r\".{c1} = {t}.{id} AND \
             \"__c\".{id} = ANY(${own}::int8[]) ORDER BY {order} LIMIT 1)"
        )
    };
    Ok(CompanyColumns {
        read: CompanyRead::Many,
        sql: format!(
            ", ARRAY(SELECT \"__a\".{c2}::int8 FROM {table} AS \"__a\" WHERE \"__a\".{c1} = \
             {t}.{id} ORDER BY 1), {}::int8, {}::text",
            first(&format!("\"__c\".{id}")),
            first(&format!("COALESCE(\"__c\".{name_col}::text, '')"))
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_denied_record_without_evidence_stays_denied() {
        let error = unexplained_denial("res.partner", Operation::Write, Some(7)).unwrap();
        assert_eq!(
            (error.kind(), error.message()),
            (
                ErrorKind::Access,
                "Access denied: the write operation on 'res.partner' (record 7) is refused by \
                 the access policy; the reason could not be listed."
            )
        );
    }

    #[test]
    fn records_that_are_gone_are_not_denied() {
        assert!(unexplained_denial("res.partner", Operation::Read, None).is_none());
    }
}
