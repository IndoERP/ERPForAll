//! Completing relational sub-domains before SQL planning (REL §4.14, DOM §4.10).
//!
//! Here that happens once, on the FULL-optimized domain, so that
//! `erp_sql::filter` plans exactly what it is given:
//!
//! | Condition | Sub-domain planned |
//! |---|---|
//! | many2one `any!` / `not any!` | `sub` |
//! | x2many `any` / `not any` (and `!` forms) | `sub AND field domain [AND inverse != False] [AND active] [AND access]` |
//! | x2many with a records query (`x2m in ids`) | `id in ids AND field domain` (no active test, no access) |
//!
//! Each completed sub-domain is optimized to FULL for its comodel and completed recursively. A
//! sub-domain that becomes `FALSE` turns the condition into a constant (`any` → `FALSE`, `not any`
//! → `TRUE`), so a known-empty query costs no sub-select.
//!
//! A comodel's rule can hold a hierarchy condition (`company_id parent_of company_ids`, the
//! product rules). Resolving one needs queries and completion runs none, so the rule comes from
//! [`NestedRules`], resolved beforehand the way a search resolves its own rule; a rule that is not
//! there stops completion, and the caller resolves it and completes again.

use std::any::Any;
use std::cell::Cell;
use std::sync::Arc;

use erp_domain::{Domain, ExternalValue, Opaque, OpaqueKind, Operator, OptimizationLevel, Value};
use erp_fields::{Error, FieldType, Result};
use erp_registry::{ModelId, ModelSpec};
use erp_sql::reference::ReferenceQuery;
use time::PrimitiveDateTime;

use crate::Transaction;
use crate::access::Operation;
use crate::env::Env;
use crate::optimize::{AccessLookup, OptimizeCtx, ZoneTz, optimize_domain};

#[derive(Debug)]
pub(crate) struct RecordsQuery {
    pub(crate) model: ModelId,
    pub(crate) ids: Option<Vec<i64>>,
}

impl ExternalValue for RecordsQuery {
    fn kind(&self) -> OpaqueKind {
        OpaqueKind::Query
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl RecordsQuery {
    /// A domain value holding the query.
    pub(crate) fn value(model: ModelId, ids: Option<Vec<i64>>) -> Value {
        Value::Opaque(Opaque::new(Arc::new(Self { model, ids })))
    }

    /// The query held by a domain value, if it is one.
    pub(crate) fn of(value: &Value) -> Option<&Self> {
        match value {
            Value::Opaque(o) => o.get().as_any().downcast_ref::<Self>(),
            _ => None,
        }
    }

    /// The query as a domain on its model: `id in ids`, or `TRUE`.
    pub(crate) fn domain(&self) -> Result<Domain> {
        match &self.ids {
            Some(ids) => Domain::condition("id", Operator::In, Value::list(ids.iter().copied()))
                .map_err(Error::from),
            None => Ok(Domain::TRUE),
        }
    }
}

/// Which inlined rule a [`NestedRules`] entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NestedRule {
    /// The read rule of a comodel, inlined into the sub-domain of an `any` / `not any`.
    Read(ModelId),
    /// The predicate an `access` condition inlines: the model's rules of the
    /// operation, its own `access` conditions resolved.
    Access(ModelId, Operation),
}

/// The inlined rules whose hierarchy conditions are resolved before completion (see the module
/// documentation), and the first one completion found missing.
#[derive(Default)]
pub(crate) struct NestedRules {
    resolved: Vec<(NestedRule, Domain)>,
    missing: Cell<Option<NestedRule>>,
}

impl NestedRules {
    /// The rule completion stopped on, cleared.
    pub(crate) fn take_missing(&self) -> Option<NestedRule> {
        self.missing.take()
    }

    /// Add the resolved form of `key`; `false` when it was already there (completion cannot
    /// have stopped on it then).
    pub(crate) fn insert(&mut self, key: NestedRule, rule: Domain) -> bool {
        if self.resolved.iter().any(|(k, _)| *k == key) {
            return false;
        }
        self.resolved.push((key, rule));
        true
    }

    /// `rule` (the inlined rule `key`), or its resolved form when it holds a hierarchy
    /// condition; refused (and recorded as missing) when that form is not here yet.
    pub(crate) fn rule(&self, key: NestedRule, model: &ModelSpec, rule: Domain) -> Result<Domain> {
        if !crate::hierarchy::has_hierarchy(&rule) {
            return Ok(rule);
        }
        if let Some((_, resolved)) = self.resolved.iter().find(|(k, _)| *k == key) {
            return Ok(resolved.clone());
        }
        self.missing.set(Some(key));
        Err(Error::programming(format!(
            "the rule of '{}' holds a hierarchy condition that was not resolved before it was \
             inlined into another condition",
            model.name
        )))
    }
}

/// What completing sub-domains needs.
pub(crate) struct PrepareCtx<'a> {
    pub(crate) tx: &'a Transaction,
    pub(crate) env: &'a Env,
    pub(crate) tz: &'a ZoneTz,
    pub(crate) now: PrimitiveDateTime,
    /// The `access` operator's lookup.
    pub(crate) access: &'a dyn AccessLookup,
    /// Comodel rules with their hierarchy conditions resolved.
    pub(crate) rules: &'a NestedRules,
}

impl PrepareCtx<'_> {
    fn full(&self, model: &ModelSpec, domain: &Domain) -> Result<Domain> {
        let expanded =
            crate::search_method::expand_for_model(self.tx, self.env, model, domain, true)?;
        optimize_domain(
            &OptimizeCtx {
                registry: self.env.registry(),
                tz: self.tz,
                now: self.now,
                su: self.env.su(),
                access: Some(self.access),
            },
            model,
            &expanded,
            OptimizationLevel::Full,
        )
    }

    /// The read predicate of `comodel` for the user's environment, completed under the context
    /// every access predicate is compiled under (`active_test` off, UTC; see
    /// [`crate::search::predicate_records`]): the comodel's rule decides the same whether it is
    /// enforced on its own or inlined into the user's search. Already complete, so the caller
    /// adds it after its own completion and never completes it again.
    fn access(&self, comodel: &ModelSpec) -> Result<Domain> {
        let predicate = self
            .tx
            .access_predicate(self.env, comodel.id, Operation::Read)?;
        let mut context = crate::context::Context::new();
        context.set("active_test", erp_fields::Value::Bool(false));
        let env = self.env.with_context_replaced(context);
        let tz = ZoneTz::new(env.tz());
        let fixed = PrepareCtx {
            tx: self.tx,
            env: &env,
            tz: &tz,
            now: self.now,
            access: self.access,
            rules: self.rules,
        };
        let predicate = crate::search_method::expand_policy(self.tx, &env, comodel, &predicate)?;
        let predicate = self
            .rules
            .rule(NestedRule::Read(comodel.id), comodel, predicate)?;
        let optimized = fixed.full(comodel, &predicate)?;
        prepare(&fixed, comodel, &optimized)
    }
}

/// Complete the relational sub-domains of a FULL-optimized `domain` on `model` (see the module
/// documentation).
pub(crate) fn prepare(ctx: &PrepareCtx<'_>, model: &ModelSpec, domain: &Domain) -> Result<Domain> {
    let mut failure: Option<Error> = None;
    let out = domain.map_conditions(|node| {
        if failure.is_some() {
            return node.clone();
        }
        let Some(condition) = node.as_condition() else {
            return node.clone();
        };
        if !condition.operator().is_any() {
            return node.clone();
        }
        match prepare_condition(ctx, model, condition) {
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

/// Whether a domain constrains `field` itself (the active test is then not added).
fn mentions(domain: &Domain, field: &str) -> bool {
    domain
        .iter_conditions()
        .filter_map(Domain::as_condition)
        .any(|c| c.field_expr() == field)
}

/// Complete the predicate of a reference query on its target model, as `prepare` completes the
/// sub-domain of a bypass many2one condition. The query is already FULL-optimized (the optimizer
/// runs first and marks it); one that is not means a pass skipped it, which is refused.
fn prepare_reference(
    ctx: &PrepareCtx<'_>,
    condition: &erp_domain::Condition,
    query: &ReferenceQuery,
) -> Result<Domain> {
    if !query.is_optimized() {
        return Err(Error::programming(format!(
            "reference query in condition {condition:?} reached completion unoptimized: a pass \
             over the access predicate did not descend into it"
        )));
    }
    let target = crate::search_method::reference_target(ctx.env.registry(), query)?;
    let sub = prepare(ctx, target, query.sub())?;
    if sub.is_false() {
        return Ok(Domain::FALSE);
    }
    Domain::condition(
        condition.field_expr(),
        condition.operator(),
        query.with_sub(sub).into_value(),
    )
    .map_err(Error::from)
}

fn prepare_condition(
    ctx: &PrepareCtx<'_>,
    model: &ModelSpec,
    condition: &erp_domain::Condition,
) -> Result<Option<Domain>> {
    let Some(field) = model.field(condition.field_expr()) else {
        return Ok(None);
    };
    // a hierarchy sub-query on a many2one or id column is compiled as is (`erp-sql`)
    if erp_sql::relational::HierarchyQuery::of(condition.value()).is_some() {
        return Ok(None);
    }
    // a reference query completes its predicate on its target model (Amendment 1)
    if let Some(query) = ReferenceQuery::of(condition.value()) {
        return prepare_reference(ctx, condition, query).map(Some);
    }
    let Some(comodel_name) = field.ty.comodel() else {
        return Ok(None);
    };
    let registry = ctx.env.registry();
    let comodel = registry.model(comodel_name).ok_or_else(|| {
        Error::programming(format!(
            "Model '{comodel_name}' does not exist in registry."
        ))
    })?;
    let operator = condition.operator();
    let bypass = matches!(operator, Operator::AnyBypass | Operator::NotAnyBypass);
    let positive = matches!(operator, Operator::Any | Operator::AnyBypass);
    // the comodel's read rule, complete (not completed again below)
    let mut rule: Option<Domain> = None;
    let sub = match (&field.ty, condition.value()) {
        (FieldType::Many2one { .. }, Value::Domain(sub)) => {
            if bypass {
                sub.clone()
            } else {
                rule = Some(ctx.access(comodel)?);
                ctx.full(comodel, sub)?
            }
        }
        (FieldType::Many2one { .. }, value) => {
            return Err(Error::programming(format!(
                "many2one condition with an unexpected value {value} on {}.{}",
                model.name, field.name
            )));
        }
        (_, value) => {
            // x2many
            let relations = &ctx.tx.shared.relations;
            let field_domain = relations.domain(model.id, field.id);
            let mut parts = vec![];
            if let Some(query) = RecordsQuery::of(value) {
                if query.model != comodel.id {
                    return Err(Error::programming(format!(
                        "a query of model '{}' used in a condition on {}.{} (comodel '{}')",
                        registry.model_by_id(query.model).name,
                        model.name,
                        field.name,
                        comodel.name
                    )));
                }
                parts.push(query.domain()?);
                parts.push(field_domain);
            } else if let Value::Domain(sub) = value {
                parts.push(sub.clone());
                parts.push(field_domain);
                if let FieldType::One2many { inverse, .. } = &field.ty
                    && let Some(inv) = comodel.field(inverse)
                    && inv.has_column()
                    && erp_sql::filter::can_be_null(inv)
                {
                    parts.push(
                        Domain::condition(inverse, Operator::Ne, false).map_err(Error::from)?,
                    );
                }
                let active_test = relations
                    .active_test(model.id, field.id)
                    .unwrap_or_else(|| ctx.env.context().active_test());
                if let Some(active) = comodel.active_name.map(|f| comodel.field_by_id(f).name)
                    && active_test
                    && !mentions(sub, active)
                {
                    parts.push(
                        Domain::condition(active, Operator::In, Value::set([true]))
                            .map_err(Error::from)?,
                    );
                }
                if !bypass {
                    rule = Some(ctx.access(comodel)?);
                }
            } else {
                return Err(Error::programming(format!(
                    "x2many condition with an unexpected value {value} on {}.{}",
                    model.name, field.name
                )));
            }
            ctx.full(comodel, &Domain::and(parts))?
        }
    };
    let mut sub = prepare(ctx, comodel, &sub)?;
    if let Some(rule) = rule {
        // optimized together for one conjunct order, but not completed again: the rule is
        // already complete under its own context
        sub = ctx.full(comodel, &(sub & rule))?;
    }
    if sub.is_false() {
        return Ok(Some(Domain::constant(!positive)));
    }
    // the condition with its completed sub-domain (the operator is kept)
    Domain::condition(condition.field_expr(), operator, Value::Domain(sub))
        .map(Some)
        .map_err(Error::from)
}
