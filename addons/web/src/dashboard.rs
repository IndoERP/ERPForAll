// SPDX-License-Identifier: MIT OR Apache-2.0
//! The saved declaration is the authority; the RPC only supplies scope and a selected period.
use crate::{dashboard_period, dashboard_settings};
#[cfg(test)]
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_sdk::views::wire::{DashboardItem, ViewNode, ViewType, WireDocument};
use erp_sdk::{GroupValue, ReadGroup};
use serde_json::{Value as Json, json};

fn fail(message: impl Into<String>) -> Error {
    Error::validation(message)
}
fn literal(expr: &erp_sdk::expr::Expr, uid: i64, depth: usize) -> Result<Json> {
    use erp_sdk::expr::{Expr, Literal, UnaryOp};
    if depth > 32 {
        return Err(fail("Dashboard domain exceeds the 32-level bound."));
    }
    Ok(match expr {
        Expr::Literal(Literal::None) => Json::Null,
        Expr::Literal(Literal::Bool(v)) => json!(v),
        Expr::Literal(Literal::Int(v)) => json!(v),
        Expr::Literal(Literal::Float(v)) if v.is_finite() => json!(v),
        Expr::Literal(Literal::Str(v)) => json!(v),
        Expr::Name(name) if name == "uid" => json!(uid),
        Expr::Unary {
            op: UnaryOp::USub,
            operand,
        } => match operand.as_ref() {
            Expr::Literal(Literal::Int(v)) => json!(
                v.checked_neg()
                    .ok_or_else(|| fail("Dashboard domain integer overflow."))?
            ),
            Expr::Literal(Literal::Float(v)) if v.is_finite() => json!(-v),
            _ => {
                return Err(fail(
                    "Dashboard domains support literal criteria and uid only.",
                ));
            }
        },
        Expr::List(items) | Expr::Tuple(items) => Json::Array(
            items
                .iter()
                .map(|e| literal(e, uid, depth + 1))
                .collect::<Result<_>>()?,
        ),
        _ => {
            return Err(fail(
                "Dashboard domains support literal criteria and uid only.",
            ));
        }
    })
}
fn domain(document: Option<&WireDocument>, uid: i64) -> Result<Domain> {
    match document {
        None => Ok(Domain::TRUE),
        Some(document) => {
            let ast = erp_sdk::expr::wire::ValidatedAst::try_from(document.clone())
                .map_err(|e| fail(e.to_string()))?;
            Domain::from_json(&literal(ast.as_expr(), uid, 0)?)
                .map_err(|e| fail(format!("Invalid dashboard domain: {e}")))
        }
    }
}
fn numeric(value: Option<&GroupValue>, aggregate: &str) -> Result<Option<Decimal>> {
    match value {
        Some(GroupValue::Null | GroupValue::False) if aggregate == "sum" => Ok(Some(Decimal::ZERO)),
        Some(GroupValue::Null | GroupValue::False) => Ok(None),
        Some(GroupValue::Int(v)) => Ok(Some(Decimal::from(*v))),
        Some(GroupValue::Decimal(v)) => Ok(Some(*v)),
        Some(GroupValue::Float(v)) => Decimal::try_from(*v)
            .map(Some)
            .map_err(|_| fail("Dashboard aggregate is not a finite decimal.")),
        _ => Err(Error::invariant(
            "Dashboard aggregate returned a nonnumeric value.",
        )),
    }
}
fn gain(current: Option<Decimal>, previous: Option<Decimal>) -> Result<Option<String>> {
    match (current, previous) {
        (Some(current), Some(previous)) if !previous.is_zero() => current
            .checked_sub(previous)
            .and_then(|delta| delta.checked_div(previous.abs()))
            .and_then(|v| v.checked_mul(Decimal::from(100)))
            .map(|v| Some(v.to_string()))
            .ok_or_else(|| fail("Dashboard percentage arithmetic overflow.")),
        _ => Ok(None), // no comparison for a missing or zero previous baseline,
    }
}
fn named_filter<'a>(
    nodes: impl Iterator<Item = &'a ViewNode>,
    filter: &str,
) -> Result<(&'a Option<WireDocument>, &'a Option<WireDocument>)> {
    // Other admitted search nodes are not named filter candidates.
    let mut matches = nodes.filter_map(|node| match node {
        ViewNode::SearchFilter {
            name,
            domain,
            context,
            ..
        } if name == filter => Some((domain, context)),
        _ => None,
    });
    let selected = matches.next().ok_or_else(|| {
        fail(format!(
            "Dashboard named filter '{filter}' is missing or unreadable."
        ))
    })?;
    if matches.next().is_some() {
        return Err(fail(format!(
            "Dashboard named filter '{filter}' is ambiguous."
        )));
    }
    Ok(selected)
}
#[extend(Base, addon = "web")]
struct DashboardBase {}
#[methods(Base, addon = "web")]
impl DashboardBase {
    /// Read all metrics of one saved, authorized list dashboard in the caller's transaction.
    #[method]
    #[rpc(effect = read, model)]
    async fn erp_read_dashboard(
        this: &RecordSet<Base>,
        tx: &mut Transaction,
        action_id: i64,
        view_id: i64,
        revision: String,
        period: String,
        domain_scope: Domain,
    ) -> Result<Value> {
        if action_id < 1
            || view_id < 1
            || revision.len() != 64
            || !revision.bytes().all(|b| b.is_ascii_hexdigit())
            || period.len() > 64
        {
            return Err(fail("Invalid dashboard identity, revision or period."));
        }
        let env = this.env();
        let model = this.as_records().model_name();
        let mut resolved =
            crate::metadata::dashboard_description(env, tx, model, action_id, view_id).await?;
        if resolved.search_unreadable {
            return resolved.wire(&json!({"version":1,"hidden":true}));
        }
        let description = &resolved.views;
        let action = &resolved.action;
        let list = description
            .views
            .iter()
            .find(|v| matches!(v.view_type, ViewType::List))
            .ok_or_else(|| fail("Dashboard list is unavailable."))?;
        let Some((saved_revision, items)) = list.nodes.iter().find_map(|n| match n {
            ViewNode::Dashboard {
                revision, items, ..
            } => Some((revision, items)),
            _ => None,
        }) else {
            return resolved.wire(&json!({"version":1,"hidden":true}));
        };
        if *saved_revision != revision {
            return Err(fail("Dashboard declaration changed. Reload this view."));
        }
        let policy = dashboard_settings::read(env, tx).await?;
        if items.len() > policy.max_items {
            return Err(fail("Dashboard exceeds web.dashboard_max_items."));
        }
        let selected = if period.is_empty() {
            policy.periods.first()
        } else {
            policy.periods.iter().find(|p| p.id == period)
        }
        .ok_or_else(|| fail("Dashboard period is not in web.dashboard_periods."))?;
        let base = domain_scope & domain(action.domain.as_ref(), env.uid())?;
        let timezone = match env.context().get("tz") {
            None | Some(Value::False) => "UTC".to_owned(),
            Some(Value::Str(s)) if s.len() <= 128 => s.to_string(),
            _ => return Err(fail("Dashboard timezone must be bounded text.")),
        }; // absent timezone follows existing UTC context default
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            match item {
                DashboardItem::Count {
                    name,
                    domain: own,
                    filter,
                    ..
                } => {
                    let filter_domain;
                    let own = if let Some(filter) = filter {
                        let found = named_filter(
                            description
                                .views
                                .iter()
                                .filter(|view| matches!(view.view_type, ViewType::Search))
                                .flat_map(|view| &view.nodes),
                            filter,
                        )?;
                        if found.1.is_some() {
                            return Err(fail(format!(
                                "Dashboard named filter '{filter}' cannot change search context."
                            )));
                        }
                        filter_domain = domain(found.0.as_ref(), env.uid())?;
                        &filter_domain
                    } else {
                        filter_domain = domain(own.as_ref(), env.uid())?;
                        &filter_domain
                    };
                    let count = this
                        .as_records()
                        .search_count(tx, &(base.clone() & own.clone()), None)
                        .await?;
                    values.push(json!({"name":name,"value":count.to_string(),"domain":own.to_json().map_err(|e| fail(e.to_string()))?}));
                }
                DashboardItem::Kpi {
                    name,
                    field,
                    date_field,
                    aggregate,
                    domain: own,
                    ..
                } => {
                    if field.model != model || date_field.model != model {
                        return Err(fail("Dashboard KPI handles must refer to the root model."));
                    }
                    let spec = env
                        .registry()
                        .model(model)
                        .ok_or_else(|| Error::invariant("Dashboard model disappeared."))?;
                    let date_kind = spec
                        .field(&date_field.name)
                        .ok_or_else(|| fail("Dashboard date field is unknown."))?
                        .ty
                        .type_name();
                    if !matches!(date_kind, "date" | "datetime") {
                        return Err(fail("Dashboard period field must be a date or datetime."));
                    }
                    let ranges = dashboard_period::ranges(
                        tx,
                        selected,
                        &timezone,
                        date_kind == "datetime",
                        policy.max_days,
                        None,
                    )
                    .await?;
                    let mut totals = Vec::with_capacity(2);
                    for (start, end) in [&ranges.current, &ranges.previous] {
                        let range = Domain::from_json(&json!([
                            [date_field.name, ">=", start],
                            [date_field.name, "<", end]
                        ]))
                        .map_err(|e| fail(e.to_string()))?;
                        let groups = this
                            .as_records()
                            ._read_group(
                                tx,
                                ReadGroup::new(
                                    base.clone() & domain(own.as_ref(), env.uid())? & range,
                                )
                                .aggregates([format!("{}:{aggregate}", field.name)]),
                            )
                            .await?;
                        totals.push(numeric(groups.column(0).first(), aggregate)?);
                    }
                    values.push(json!({"name":name,"value":totals[0].map(|v|v.to_string()),"previous":totals[1].map(|v|v.to_string()),"change":gain(totals[0],totals[1])?}));
                }
                _ => return Err(fail("Unknown dashboard metric kind.")),
            }
        }
        resolved.wire(&json!({"version":1,"hidden":false,"revision":revision,"period":selected.id,"periods":policy.periods,"values":values}))
    }
}
#[cfg(test)]
#[path = "tests/dashboard.rs"]
mod tests;
