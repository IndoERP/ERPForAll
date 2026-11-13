//! `child_of` / `parent_of` (DOM §4.8), resolved into id conditions before a domain is optimized,
//! because they need queries: the start records are searched by name (with the user's rights) and
//! the hierarchy is walked as superuser without active test.
//!
//! For SQL (`embed`), the condition becomes `f IN (WITH RECURSIVE …)` with the start ids as one
//! array parameter ([`erp_sql::relational::HierarchyQuery`]): the hierarchy is never materialized in
//! the application. For in-memory evaluation the CTE runs once and its ids are inlined. Cycles
//! terminate (`UNION`); ids that do not exist simply contribute nothing. A parent link that is an
//! x2many field is walked one level per query.
//!
//! Start records named by principal parameters (an access rule's
//! `[('message_partner_ids', 'child_of', [user.partner_id.commercial_partner_id.id])]`,
//! §3.1) have values only where the statement runs: the condition becomes a sub-query that
//! selects them there ([`erp_sql::relational::HierarchyStart::Selected`]) and walks down by
//! `parent_path` ranges (`child_of` on a `_parent_store` tree) or recursively otherwise. One
//! compiled text serves every user.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use erp_domain::{Domain, DomainKind, Operator, OptimizationLevel, PrincipalParam, Value};
use erp_fields::{Error, FieldType, Result};
use erp_registry::ModelSpec;
use erp_sql::reference::ReferenceQuery;
use erp_sql::relational::HierarchyStart;

use crate::Transaction;
use crate::dispatch::BoxFuture;
use crate::env::Env;
use crate::records::Records;

/// Whether a domain (sub-domains included) holds a hierarchy condition.
pub(crate) fn has_hierarchy(domain: &Domain) -> bool {
    match domain.kind() {
        DomainKind::Not(child) => has_hierarchy(child),
        DomainKind::And(children) | DomainKind::Or(children) => children.iter().any(has_hierarchy),
        DomainKind::Condition(c) => {
            matches!(c.operator(), Operator::ChildOf | Operator::ParentOf)
                || matches!(c.value(), Value::Domain(sub) if has_hierarchy(sub))
                || ReferenceQuery::of(c.value()).is_some_and(|query| has_hierarchy(query.sub()))
        }
        _ => false,
    }
}

/// `domain` for `records`' model with its hierarchy conditions replaced by id conditions: a
/// recursive sub-query when `embed` (the domain is compiled to SQL), the ids otherwise.
pub(crate) async fn resolve(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    embed: bool,
) -> Result<Domain> {
    Ok(resolve_tracking(tx, records, domain, embed).await?.0)
}

/// [`resolve`], also telling whether a start set was found by a search (start records given as
/// names, or many2many ids searched under the comodel's rules). The ids of such a set depend on
/// data the resolved domain does not name, so a decision made with it cannot be remembered.
pub(crate) async fn resolve_tracking(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    embed: bool,
) -> Result<(Domain, bool)> {
    if !has_hierarchy(domain) {
        return Ok((domain.clone(), false));
    }
    let searched = AtomicBool::new(false);
    let env = records.env().clone();
    // paths become `any` conditions first, so every hierarchy condition is on a field of its model
    let basic = records.optimize_domain(domain, OptimizationLevel::Basic, None)?;
    let registry = Arc::clone(env.registry());
    let external = domain.is_external();
    let ctx = Ctx {
        external,
        embed,
        searched: &searched,
    };
    let resolved = resolve_node(
        tx,
        &env,
        registry.model_by_id(records.model_id()),
        basic,
        ctx,
    )
    .await?;
    let resolved = if external {
        resolved.into_external()
    } else {
        resolved
    };
    Ok((resolved, searched.load(Ordering::Relaxed)))
}

/// How conditions are resolved: provenance of the domain (error kinds) and SQL embedding.
#[derive(Clone, Copy)]
struct Ctx<'c> {
    external: bool,
    embed: bool,
    /// Set when a start set is found by a search.
    searched: &'c AtomicBool,
}

fn resolve_node<'a>(
    tx: &'a mut Transaction,
    env: &'a Env,
    model: &'a ModelSpec,
    domain: Domain,
    ctx: Ctx<'a>,
) -> BoxFuture<'a, Result<Domain>> {
    Box::pin(async move {
        let ctx = Ctx {
            external: domain.programming_context().is_none()
                && (ctx.external || domain.is_external()),
            ..ctx
        };
        let resolved = match domain.kind() {
            DomainKind::Not(child) => !resolve_node(tx, env, model, child.clone(), ctx).await?,
            DomainKind::And(children) => {
                let mut out = Vec::with_capacity(children.len());
                for c in children {
                    out.push(resolve_node(tx, env, model, c.clone(), ctx).await?);
                }
                Domain::and(out)
            }
            DomainKind::Or(children) => {
                let mut out = Vec::with_capacity(children.len());
                for c in children {
                    out.push(resolve_node(tx, env, model, c.clone(), ctx).await?);
                }
                Domain::or(out)
            }
            DomainKind::Condition(c)
                if matches!(c.operator(), Operator::ChildOf | Operator::ParentOf) =>
            {
                resolve_condition(
                    tx,
                    env,
                    model,
                    c,
                    ctx,
                    domain.programming_context().map(AsRef::as_ref),
                )
                .await?
            }
            DomainKind::Condition(c) => match c.value() {
                Value::Domain(sub) if has_hierarchy(sub) => {
                    let Some(comodel) = model
                        .field(c.field_expr())
                        .and_then(|f| f.ty.comodel())
                        .and_then(|name| env.registry().model(name))
                    else {
                        return Ok(domain.clone());
                    };
                    let sub = resolve_node(tx, env, comodel, sub.clone(), ctx).await?;
                    Domain::condition(c.field_expr(), c.operator(), Value::Domain(sub))
                        .map_err(Error::from)?
                }
                // a reference query (Amendment 1): its predicate is on its target model;
                // paths become `any` conditions first, as for the domain itself
                value
                    if let Some(query) = ReferenceQuery::of(value)
                        && has_hierarchy(query.sub()) =>
                {
                    let target = crate::search_method::reference_target(env.registry(), query)?;
                    let schema = crate::optimize::RegistrySchema(env.registry());
                    let basic = query
                        .sub()
                        .optimize(&erp_domain::ModelContext::new(&schema, target.name))
                        .map_err(Error::from)?;
                    let sub = resolve_node(tx, env, target, basic, ctx).await?;
                    Domain::condition(
                        c.field_expr(),
                        c.operator(),
                        query.with_sub(sub).into_value(),
                    )
                    .map_err(Error::from)?
                }
                _ => domain.clone(),
            },
            _ => domain.clone(),
        };
        Ok(if let Some(context) = domain.programming_context() {
            resolved.with_programming_context(Arc::clone(context))
        } else if ctx.external {
            resolved.into_external()
        } else {
            resolved
        })
    })
}

async fn resolve_condition(
    tx: &mut Transaction,
    env: &Env,
    model: &ModelSpec,
    condition: &erp_domain::Condition,
    ctx: Ctx<'_>,
    programming_context: Option<&str>,
) -> Result<Domain> {
    let external = ctx.external;
    let registry = Arc::clone(env.registry());
    let operator = condition.operator();
    let expr = condition.field_expr();
    let value = condition.value();
    if matches!(value, Value::Bool(false)) {
        return Ok(Domain::FALSE);
    }
    let invalid = |message: String| {
        if let Some(context) = programming_context {
            Error::programming(format!("{context}: {message}"))
        } else if external {
            Error::validation(message)
        } else {
            Error::programming(message)
        }
    };
    let field = model.field(expr).ok_or_else(|| {
        invalid(format!(
            "Invalid field {}.{expr} in condition {condition:?}",
            model.name
        ))
    })?;
    let (comodel, comodel_env) = match &field.ty {
        FieldType::Many2one { comodel, .. } => (
            registry.model(comodel),
            env.with_context([("active_test", erp_fields::Value::Bool(false))]),
        ),
        FieldType::One2many { comodel, .. } | FieldType::Many2many { comodel, .. } => (
            registry.model(comodel),
            tx.field_env(env, model.id, field.id),
        ),
        FieldType::Id => (Some(model), env.clone()),
        _ => {
            return Err(invalid(format!(
                "Cannot execute {} for {}.{}, works only for relational fields",
                operator.as_str(),
                model.name,
                field.name
            )));
        }
    };
    let comodel = comodel.ok_or_else(|| {
        Error::programming(format!(
            "Cannot determine the comodel of {}.{}",
            model.name, field.name
        ))
    })?;
    let mut parent = comodel.parent_name;
    let mut result_on_id = matches!(field.ty, FieldType::Id);
    if comodel.id == model.id {
        if expr != "id" {
            parent = Some(field.id);
        }
        if matches!(field.ty, FieldType::Many2one { .. }) {
            result_on_id = true;
        }
    }
    let items: Vec<Value> = match value {
        Value::List(items) => items.clone(),
        Value::Set(set) => set.iter().cloned().collect(),
        Value::Int(_) | Value::Str(_) | Value::Param(_) | Value::Domain(_) => {
            vec![value.clone()]
        }
        other => {
            return Err(invalid(format!(
                "Value of type {other} is not supported for {} in condition {condition:?}",
                operator.as_str()
            )));
        }
    };
    let mut ints: Vec<i64> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut params: Vec<PrincipalParam> = Vec::new();
    let mut seed_domains: Vec<Domain> = Vec::new();
    for item in items {
        match item {
            Value::Domain(seed) => seed_domains.push(seed),
            Value::Int(i) => ints.push(i),
            Value::Str(s) => names.push(s),
            Value::Param(param) => params.push(param),
            other => {
                return Err(invalid(format!(
                    "Value of type {other} is not supported for {} in condition {condition:?}",
                    operator.as_str()
                )));
            }
        }
    }
    if !params.is_empty() && !ctx.embed {
        // principal parameters have values only where a statement runs (R-S6)
        return Err(Error::programming(format!(
            "the hierarchy condition {condition:?} names principal parameters: it is decided in \
             SQL only"
        )));
    }
    if let Some(now) = params.iter().find(|p| matches!(p, PrincipalParam::Now(_))) {
        return Err(Error::programming(format!(
            "The principal parameter {} in {condition:?} names no record: {} starts from record \
             ids",
            now.symbol(),
            operator.as_str()
        )));
    }
    let mut search: Vec<Domain> = Vec::new();
    if matches!(field.ty, FieldType::Many2many { .. }) && !ints.is_empty() {
        search.push(
            Domain::condition("id", Operator::In, Value::list(std::mem::take(&mut ints)))
                .map_err(Error::from)?,
        );
    }
    for name in names {
        search.push(Domain::condition("display_name", Operator::ILike, name).map_err(Error::from)?);
    }
    search.extend(seed_domains);
    let mut start: Vec<i64> = ints;
    if !search.is_empty() {
        ctx.searched.store(true, Ordering::Relaxed);
        let co = Records::empty(comodel_env, comodel.id);
        let found = co
            .search_with(
                tx,
                &Domain::or(search),
                &crate::SearchOptions::new().order("id"),
            )
            .await?;
        start.extend(found.ids());
    }
    start.retain(|i| *i > 0);
    if start.is_empty() && params.is_empty() {
        return Ok(Domain::FALSE);
    }
    let Some(parent) = parent else {
        return Err(Error::programming(format!(
            "Cannot execute {} on {}: the model has no parent field",
            operator.as_str(),
            comodel.name
        )));
    };
    let parent_field = comodel.field_by_id(parent);
    let target = if result_on_id { "id" } else { expr };
    let on_x2many = !result_on_id && field.ty.is_x2many();
    if !params.is_empty() {
        // the start records are named by principal parameters (`user.partner_id.id`,
        // `company_ids`): selected and walked in the statement itself
        if !parent_field.has_column() {
            return Err(Error::programming(format!(
                "Cannot execute {} over principal parameters on {}: its parent link {} is not a \
                 stored many2one",
                operator.as_str(),
                comodel.name,
                parent_field.name
            )));
        }
        let by_path = operator == Operator::ChildOf
            && comodel.parent_path.is_some()
            && comodel.parent_name == Some(parent);
        let query = erp_sql::relational::HierarchyQuery {
            model: comodel.id,
            parent,
            descendants: operator == Operator::ChildOf,
            start: HierarchyStart::Selected {
                ids: start,
                params,
                by_path,
            },
        }
        .into_value();
        return if on_x2many {
            let ids = Domain::condition("id", Operator::In, query).map_err(Error::from)?;
            Domain::condition(target, Operator::AnyBypass, Value::Domain(ids))
        } else {
            Domain::condition(target, Operator::In, query)
        }
        .map_err(Error::from);
    }
    if let Some(path) = comodel
        .parent_path
        .filter(|_| comodel.parent_name == Some(parent))
    {
        // `_parent_store`: child_of matches the start records' path prefixes,
        // parent_of the ids their paths list; the paths are always current in the database
        let ids: Vec<erp_fields::RecId> = start
            .iter()
            .filter_map(|i| erp_fields::RecId::from_real(*i))
            .collect();
        let su = env
            .engine_sudo()
            .with_context([("active_test", erp_fields::Value::Bool(false))]);
        let paths: Vec<String> = tx
            .peek_cells(&su, comodel.id, path, &ids)
            .await?
            .into_iter()
            .filter_map(|c| match c {
                erp_fields::Cell::Str(p) => Some(p.to_string()),
                _ => None,
            })
            .collect();
        let path_name = comodel.field_by_id(path).name;
        if operator == Operator::ChildOf && ctx.embed {
            // in SQL: an index range per path, in byte order (the index is `COLLATE "C"`)
            if paths.is_empty() {
                return Ok(Domain::FALSE);
            }
            let query = erp_sql::relational::HierarchyQuery {
                model: comodel.id,
                parent,
                descendants: true,
                start: HierarchyStart::Paths(paths),
            }
            .into_value();
            return if on_x2many {
                let ids = Domain::condition("id", Operator::In, query).map_err(Error::from)?;
                Domain::condition(target, Operator::AnyBypass, Value::Domain(ids))
            } else {
                Domain::condition(target, Operator::In, query)
            }
            .map_err(Error::from);
        }
        if operator == Operator::ChildOf {
            let mut prefixes = Vec::with_capacity(paths.len());
            for p in &paths {
                prefixes.push(
                    Domain::condition(path_name, Operator::EqLike, Value::Str(format!("{p}%")))
                        .map_err(Error::from)?,
                );
            }
            let below = Domain::or(prefixes);
            return if result_on_id {
                Ok(below)
            } else {
                Domain::condition(expr, Operator::AnyBypass, Value::Domain(below))
                    .map_err(Error::from)
            };
        }
        let mut above: Vec<i64> = paths
            .iter()
            .flat_map(|p| crate::parent_store::path_ids(p))
            .collect();
        above.sort_unstable();
        above.dedup();
        return Domain::condition(target, Operator::In, Value::list(above)).map_err(Error::from);
    }
    if parent_field.has_column() && ctx.embed {
        // one recursive sub-query in the statement itself
        tx.flush_fields(comodel.id, &[parent]).await?;
        let query = erp_sql::relational::HierarchyQuery {
            model: comodel.id,
            parent,
            descendants: operator == Operator::ChildOf,
            start: HierarchyStart::Ids(start),
        }
        .into_value();
        return if on_x2many {
            let ids = Domain::condition("id", Operator::In, query).map_err(Error::from)?;
            Domain::condition(target, Operator::AnyBypass, Value::Domain(ids))
        } else {
            Domain::condition(target, Operator::In, query)
        }
        .map_err(Error::from);
    }
    let mut ids: Vec<i64> = if parent_field.has_column() {
        // a many2one link: one recursive query, whatever the depth
        tx.flush_fields(comodel.id, &[parent]).await?;
        let sql = if operator == Operator::ChildOf {
            erp_sql::relational::descendants(&comodel.table, parent_field.name)
        } else {
            erp_sql::relational::ancestors(&comodel.table, parent_field.name)
        };
        let rows = tx.query(&sql, &[&start]).await?;
        let mut ids: Vec<i64> = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.try_get(0).map_err(|e| {
                Error::new(erp_fields::ErrorKind::Database, "could not decode an id").with_source(e)
            })?;
            ids.push(id);
        }
        ids
    } else {
        // an x2many link (a field of the model pointing to itself): one level per round
        expand(tx, env, comodel, parent_field.name, operator, start).await?
    };
    ids.sort_unstable();
    Domain::condition(target, Operator::In, Value::list(ids)).map_err(Error::from)
}

async fn expand(
    tx: &mut Transaction,
    env: &Env,
    comodel: &ModelSpec,
    link: &str,
    operator: Operator,
    start: Vec<i64>,
) -> Result<Vec<i64>> {
    let su = Records::empty(
        env.engine_sudo()
            .with_context([("active_test", erp_fields::Value::Bool(false))]),
        comodel.id,
    );
    let mut seen: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    let mut frontier: Vec<i64> = start;
    if operator == Operator::ParentOf {
        // existing start records only
        let found = su
            .search_with(
                tx,
                &Domain::condition("id", Operator::In, Value::list(frontier.clone()))
                    .map_err(Error::from)?,
                &crate::SearchOptions::new().active_test(false),
            )
            .await?;
        frontier = found.ids();
    }
    while !frontier.is_empty() {
        seen.extend(frontier.iter().copied());
        let next: Vec<i64> = if operator == Operator::ChildOf {
            let found = su
                .search_with(
                    tx,
                    &Domain::condition(link, Operator::In, Value::list(frontier.clone()))
                        .map_err(Error::from)?,
                    &crate::SearchOptions::new().active_test(false),
                )
                .await?;
            found.ids()
        } else {
            let records = su.browse(frontier.iter().copied())?;
            records.related(tx, link).await?.ids()
        };
        frontier = next.into_iter().filter(|i| !seen.contains(i)).collect();
    }
    Ok(seen.into_iter().collect())
}
