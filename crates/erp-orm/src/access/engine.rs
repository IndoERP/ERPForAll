//! The engine entry points (DN §2.2): the one place that builds principal
//! bindings, applies elevation and asks the pinned policy snapshot. No ORM call site decides
//! `su()` itself.

use std::cell::RefCell;
use std::sync::Arc;

use erp_domain::{Domain, ModelContext, Operator, PrincipalParam, Value};
use erp_fields::{Error, Result};
use erp_registry::{ModelId, ModelSpec};
use erp_sql::param::{Param, Scalar, ScalarKind};

use super::{AccessDomain, AccessPolicy, Operation};
use crate::env::Env;
use crate::field_chain::FieldContextValues;
use crate::principal::{ActiveCompanySet, Elevation, Principal, PrincipalBinding};
use crate::transaction::Transaction;

impl Transaction {
    /// The snapshot decisions are taken with: pinned when the transaction began, or the private
    /// one rebuilt after a policy write in it (SEC-018). Public for the principal resolver
    /// (`erp-security`), which closes re-resolved groups with it.
    pub fn policy(&self) -> &Arc<dyn AccessPolicy> {
        &self.policy
    }

    /// The binding of `env`'s principal for a decision in this transaction (DN §2.1): refuses a
    /// scoped elevation whose block is not open here, refuses an unresolved principal under a
    /// policy that decides on groups and companies, and interns the subject's group set in the
    /// pinned snapshot.
    ///
    /// # Errors
    ///
    /// `Programming` for a closed elevation block or an unresolved principal under a compiled
    /// policy; the policy's interning error.
    pub(crate) fn binding<'e>(&self, env: &'e Env) -> Result<PrincipalBinding<'e>> {
        self.check_not_cancelled()?;
        // an environment whose companies were refused decides nothing
        env.companies()?;
        let principal = env.principal();
        if let Elevation::Scoped { block, .. } = principal.elevation() {
            // blocks are opened by `Transaction::elevated`; none is open before
            return Err(Error::programming(format!(
                "elevated environment of block {}#{} used outside it",
                block.tx().get(),
                block.seq()
            )));
        }
        if self.refresh.stale() {
            return Err(Error::programming(
                "the access policy of this transaction is stale: a policy or membership change is \
                 pending and the operation decided without Transaction::ensure_policy_current",
            ));
        }
        let policy = self.policy();
        // the newest resolution read in this transaction wins over the environment's (older
        // `Env` clones see a membership change too, design note §3.4)
        let resolution = match self.resolved(principal.subject()) {
            Some(newest) => {
                let all = newest.all_companies();
                if principal
                    .companies()
                    .allowed()
                    .iter()
                    .any(|c| all.binary_search(c).is_err())
                {
                    return Err(Error::access(
                        "Access to unauthorized or invalid companies.",
                    ));
                }
                Arc::clone(newest)
            }
            None if policy.needs_resolved_principals() && self.shared.resolver.is_some() => {
                // a resolution the caller built is a hint, never evidence: with a resolver
                // installed the subject is read from the database (prepare_access) first
                return Err(Error::programming(format!(
                    "the {} principal of user {} was not resolved by the principal resolver in this \
                     transaction: a resolution built by the caller is not trusted, and the \
                     operation decided without Transaction::prepare_access",
                    principal.kind().as_str(),
                    principal.subject()
                )));
            }
            None => Arc::clone(principal.resolution()),
        };
        if !resolution.is_resolved() && policy.needs_resolved_principals() {
            return Err(unresolved_error(principal));
        }
        // the validated active set decides which variant of a generated company restriction
        // applies (design `m3-company-a1` §4.3): the same list parameters are bound from
        let active = ActiveCompanySet::of(self.companies_now(env)?);
        let binding =
            PrincipalBinding::bind(policy.as_ref(), principal, resolution)?.with_companies(active);
        // the context keys `#[field_access]` overrides declared: read once per binding
        let keys = policy.field_context_keys();
        Ok(if keys.is_empty() {
            binding
        } else {
            binding.with_field_ctx(FieldContextValues::read(keys, env.context()))
        })
    }

    /// # Errors
    ///
    /// `Access` when refused; the binding's errors.
    pub(crate) fn check_model(&self, env: &Env, model: ModelId, op: Operation) -> Result<()> {
        // refused companies stop every operation, also in superuser mode
        env.companies()?;
        let spec: &ModelSpec = self.shared.registry.model_by_id(model);
        let principal = env.principal();
        if let Some(scope) = principal.scope()
            && scope.read_only()
            && op != Operation::Read
        {
            return Err(Error::access(format!(
                "The API key '{}' is read-only: it may not {} '{}' ({}).",
                scope.name(),
                op.as_str(),
                spec.description,
                spec.name
            )));
        }
        if principal.elevation().is_full() {
            return Ok(());
        }
        let binding = self.binding(env)?;
        self.policy().check_model(&binding, spec, op)
    }

    /// The record predicate of `op` on `model` for `env`: `TRUE` in superuser mode, the policy's
    /// otherwise.
    ///
    /// # Errors
    ///
    /// The binding's and the policy's errors.
    pub(crate) fn access_domain(
        &self,
        env: &Env,
        model: ModelId,
        op: Operation,
    ) -> Result<AccessDomain> {
        env.companies()?;
        if env.principal().elevation().is_full() {
            return Ok(AccessDomain::allow_all());
        }
        let spec: &ModelSpec = self.shared.registry.model_by_id(model);
        let binding = self.binding(env)?;
        self.policy().access_domain(&binding, spec, op)
    }
}

impl Transaction {
    /// Bind the principal parameters of a planned statement (`Param::Principal`,.5)
    /// to `env`'s principal: the subject, its current and validated companies. "now" and user paths
    /// are lowered in SQL and never reach here.
    ///
    /// # Errors
    ///
    /// `Programming` for `company_id` without a current company, for link grant ids before
    /// , for a parameter without a bound form, and (as
    /// [`Transaction::companies_now`]) for a subject not resolved in this transaction; `Access`
    /// for a company the subject is not in.
    pub(crate) fn bind_principal_params(&self, env: &Env, params: &mut [Param]) -> Result<()> {
        if !params.iter().any(|p| matches!(p, Param::Principal { .. })) {
            return Ok(());
        }
        // the validated companies, as read, group-by and the cache key take them:
        // the subject's defaults for an environment that names none, and a company the subject
        // lost earlier in the transaction is refused.
        let principal = env.principal();
        let companies = if env.su() {
            let named = env.companies()?;
            match principal
                .company_authority()
                .and_then(|a| self.resolved(a.subject()))
            {
                Some(newest) if named.is_empty() => newest.default_companies(),
                _ => named,
            }
        } else {
            self.companies_now(env)?
        };
        for slot in params.iter_mut() {
            let Param::Principal { param, .. } = slot else {
                continue;
            };
            *slot = match param {
                PrincipalParam::Uid => Param::Scalar(Scalar::Int8(principal.subject().get())),
                PrincipalParam::CompanyId => {
                    let current = companies.first().copied().ok_or_else(|| {
                        Error::programming(format!(
                            "company_id cannot be bound: the {} principal of user {} has no \
                             current company",
                            principal.kind().as_str(),
                            principal.subject()
                        ))
                    })?;
                    Param::Scalar(Scalar::Int8(current.get()))
                }
                PrincipalParam::CompanyIds => Param::Array(
                    ScalarKind::Int8,
                    companies.iter().map(|c| Scalar::Int8(c.get())).collect(),
                ),
                PrincipalParam::GrantIds { model, op } => {
                    return Err(Error::programming(format!(
                        "grant_ids('{model}', '{op}') cannot be bound: link grants are not \
                         supported until a later release"
                    )));
                }
                other => {
                    return Err(Error::programming(format!(
                        "the principal parameter {} is lowered in SQL and has no bound value",
                        other.symbol()
                    )));
                }
            };
        }
        Ok(())
    }
}

/// The `access` operator's lookup over a transaction: the access domains of the
/// environment's principal outside superuser mode, with nested `access` conditions resolved
/// eagerly so that a chain coming back to a (model, operation) being resolved is detected
/// (A → B → A), never followed (R-S4).
pub(crate) struct TxAccess<'a> {
    tx: &'a Transaction,
    env: &'a Env,
    stack: RefCell<Vec<(ModelId, Operation)>>,
}

/// Most `access` hops one resolution may take (a bound besides the cycle check: a long acyclic
/// chain through many models is refused too).
const MAX_ACCESS_DEPTH: usize = 32;

impl<'a> TxAccess<'a> {
    pub(crate) fn new(tx: &'a Transaction, env: &'a Env) -> Self {
        Self {
            tx,
            env,
            stack: RefCell::new(Vec::new()),
        }
    }

    fn chain(&self, model: &ModelSpec, op: Operation) -> String {
        let registry = self.tx.registry();
        let mut names: Vec<String> = self
            .stack
            .borrow()
            .iter()
            .map(|(m, o)| format!("{} {}", registry.model_by_id(*m).name, o.as_str()))
            .collect();
        names.push(format!("{} {}", model.name, op.as_str()));
        names.join(" → ")
    }

    /// `domain` (on `model`) with its `access` conditions replaced, sub-domains included.
    fn resolve_nested(&self, model: &ModelSpec, domain: &Domain) -> Result<Domain> {
        let expanded = crate::search_method::expand_policy(self.tx, self.env, model, domain)?;
        let domain = &expanded;
        if !mentions_access(domain) {
            return Ok(domain.clone());
        }
        let registry = self.tx.registry();
        // paths become `any` sub-domains, so every `access` condition sits on its own model
        let schema = crate::optimize::RegistrySchema(registry);
        let basic = domain
            .optimize(&ModelContext::new(&schema, model.name))
            .map_err(Error::from)?;
        let mut failure: Option<Error> = None;
        let out = basic.map_conditions(|node| {
            if failure.is_some() {
                return node.clone();
            }
            let Some(condition) = node.as_condition() else {
                return node.clone();
            };
            match self.resolve_condition(model, condition, node, basic.is_external()) {
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

    fn resolve_condition(
        &self,
        model: &ModelSpec,
        condition: &erp_domain::Condition,
        node: &Domain,
        external: bool,
    ) -> Result<Option<Domain>> {
        // Only errors generated from this node use its provenance. Policy/access
        // lookup failures below retain their own kind and metadata.
        let invalid = |message: String| {
            if let Some(context) = node.programming_context() {
                Error::programming(format!("{context}: {message}"))
            } else if external || node.is_external() {
                Error::validation(message)
            } else {
                Error::programming(message)
            }
        };
        let Some(field) = model.field(condition.field_expr()) else {
            // an unknown field fails in the optimizer that sees the domain next
            return Ok(None);
        };
        let comodel = field.ty.comodel().and_then(|c| self.tx.registry().model(c));
        match (condition.operator(), condition.value()) {
            (Operator::Access, value) => {
                let target = match (&field.ty, comodel) {
                    (erp_fields::FieldType::Id, _) => model,
                    (erp_fields::FieldType::Many2one { .. }, Some(co)) => co,
                    _ => {
                        return Err(invalid(format!(
                            "The 'access' operator works only for many2one and 'id' fields in \
                             condition {condition:?}"
                        )));
                    }
                };
                let op = match value {
                    Value::Str(name) => Operation::parse(name),
                    _ => None,
                }
                .ok_or_else(|| {
                    invalid(format!(
                        "Invalid value for 'access' operator in condition {condition:?}"
                    ))
                })?;
                let condition_on = |operator: Operator, value: Value| {
                    Domain::condition(field.name, operator, value).map_err(Error::from)
                };
                Ok(Some(
                    match crate::optimize::AccessLookup::resolve(self, target, op)? {
                        None => condition_on(Operator::Ne, Value::FALSE)?,
                        Some(d) if d.is_false() => Domain::FALSE,
                        Some(d) if d.is_true() => condition_on(Operator::Ne, Value::FALSE)?,
                        Some(d) => condition_on(Operator::AnyBypass, Value::Domain(d))?,
                    },
                ))
            }
            (operator, Value::Domain(sub)) if operator.is_any() => {
                let Some(co) = comodel else {
                    return Ok(None);
                };
                let resolved = self.resolve_nested(co, sub)?;
                if resolved == *sub {
                    return Ok(None);
                }
                Domain::condition(field.name, operator, Value::Domain(resolved))
                    .map(Some)
                    .map_err(Error::from)
            }
            _ => Ok(None),
        }
    }
}

/// Whether an `access` condition occurs anywhere in `domain` (sub-domains included).
pub(crate) fn mentions_access(domain: &Domain) -> bool {
    match domain.kind() {
        erp_domain::DomainKind::True
        | erp_domain::DomainKind::False
        | erp_domain::DomainKind::Custom(_) => false,
        erp_domain::DomainKind::Not(child) => mentions_access(child),
        erp_domain::DomainKind::And(children) | erp_domain::DomainKind::Or(children) => {
            children.iter().any(mentions_access)
        }
        erp_domain::DomainKind::Condition(condition) => {
            condition.operator() == Operator::Access
                || matches!(condition.value(), Value::Domain(sub) if mentions_access(sub))
        }
    }
}

impl crate::optimize::AccessLookup for TxAccess<'_> {
    fn resolve(&self, model: &ModelSpec, op: Operation) -> Result<Option<Domain>> {
        let env = self.env.sudo(false);
        if env.su() {
            return Ok(None);
        }
        let on_stack = self.stack.borrow().contains(&(model.id, op));
        if on_stack {
            return Err(Error::programming(format!(
                "The 'access' operator forms a cycle: {}",
                self.chain(model, op)
            )));
        }
        if self.stack.borrow().len() >= MAX_ACCESS_DEPTH {
            return Err(Error::programming(format!(
                "The 'access' operator chain is longer than {MAX_ACCESS_DEPTH} hops: {}",
                self.chain(model, op)
            )));
        }
        self.stack.borrow_mut().push((model.id, op));
        let result = self
            .tx
            .access_domain(&env, model.id, op)
            .and_then(|access| self.resolve_nested(model, access.domain()));
        self.stack.borrow_mut().pop();
        result.map(Some)
    }
}

/// The refusal of a principal no resolver read, under a policy that needs its groups.
fn unresolved_error(principal: &Principal) -> Error {
    Error::programming(format!(
        "the {} principal of user {} was never resolved against the database: the installed \
         access policy decides on the subject's groups and companies, so the principal must come \
         from the policy's resolver",
        principal.kind().as_str(),
        principal.subject()
    ))
}
