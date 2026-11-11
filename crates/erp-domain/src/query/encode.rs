//! AST → canonical document tree. Every key of the kind is written, absent optional values in
//! their documented absent form; the byte form is `erp_fields::json::canonical` of this tree
//! (sorted keys, no whitespace).

use erp_fields::json::Json;

use crate::{Domain, NowKind, PrincipalParam, Value};

use super::error::{Bound, JsonPath, QueryAstError, QueryAstReason};
use super::limits::{QueryLimits, Widen};
use super::names::ModelName;
use super::parts::{Aggregate, NullsPos, OrderDir, QueryContext};
use super::{Body, QUERY_AST_VERSION};

fn member(key: &str, value: Json) -> (Box<str>, Json) {
    (Box::from(key), value)
}

fn strings<'a>(items: impl IntoIterator<Item = &'a str>) -> Json {
    Json::Array(items.into_iter().map(|s| Json::Str(Box::from(s))).collect())
}

fn text(value: &str) -> Json {
    Json::Str(Box::from(value))
}

fn nulls(nulls: Option<NullsPos>) -> Json {
    nulls.map_or(Json::Null, |n| text(n.as_str()))
}

fn dir(dir: OrderDir) -> Json {
    text(dir.as_str())
}

/// The canonical document of an AST's parts.
pub(crate) fn document(
    model: &ModelName,
    domain: &Domain,
    context: QueryContext,
    body: &Body,
    limit: u32,
    offset: u32,
    limits: &QueryLimits,
) -> Result<Json, QueryAstError> {
    let at = JsonPath::root().key("domain");
    let mut members = vec![
        member("version", Json::Int(i64::from(QUERY_AST_VERSION))),
        member("model", text(model.as_str())),
        member("domain", domain_json(domain, &at, limits)?),
        member(
            "context",
            Json::Object(
                context
                    .active_test
                    .map(|flag| member("active_test", Json::Bool(flag)))
                    .into_iter()
                    .collect(),
            ),
        ),
        member("limit", Json::Int(i64::from(limit))),
        member("offset", Json::Int(i64::from(offset))),
    ];
    match body {
        Body::SearchRead { fields, order } => {
            members.push(member("kind", text("search_read")));
            members.push(member("fields", strings(fields.iter().map(|f| f.as_str()))));
            members.push(member(
                "order",
                Json::Array(
                    order
                        .iter()
                        .map(|t| {
                            Json::Object(vec![
                                member("field", text(t.field.as_str())),
                                member("dir", dir(t.dir)),
                                member("nulls", nulls(t.nulls)),
                            ])
                        })
                        .collect(),
                ),
            ));
        }
        Body::ReadGroup {
            groupby,
            aggregates,
            having,
            order,
        } => {
            members.push(member("kind", text("read_group")));
            members.push(member(
                "groupby",
                Json::Array(
                    groupby
                        .iter()
                        .map(|g| {
                            Json::Object(vec![
                                member("path", text(&g.path.to_string())),
                                member(
                                    "granularity",
                                    g.granularity.map_or(Json::Null, |x| text(x.name())),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ));
            members.push(member(
                "aggregates",
                Json::Array(
                    aggregates
                        .iter()
                        .map(|a| match a {
                            Aggregate::Count => Json::Object(vec![
                                member("func", text("count")),
                                member("field", Json::Null),
                            ]),
                            Aggregate::Field { field, func } => Json::Object(vec![
                                member("func", text(func.as_str())),
                                member("field", text(field.as_str())),
                            ]),
                        })
                        .collect(),
                ),
            ));
            let mut conditions = Vec::with_capacity(having.len());
            for (index, h) in having.iter().enumerate() {
                let at = JsonPath::root().key("having").index(index).key("value");
                conditions.push(Json::Object(vec![
                    member("aggregate", text(&h.aggregate)),
                    member("op", text(h.op.as_str())),
                    member("value", value_json(&h.value, &at, 0, limits)?),
                ]));
            }
            members.push(member("having", Json::Array(conditions)));
            members.push(member(
                "order",
                Json::Array(
                    order
                        .iter()
                        .map(|t| {
                            Json::Object(vec![
                                member("key", text(&t.key)),
                                member("dir", dir(t.dir)),
                                member("nulls", nulls(t.nulls)),
                            ])
                        })
                        .collect(),
                ),
            ));
        }
    }
    Ok(Json::Object(members))
}

/// The domain in normalized prefix form; `TRUE` is `[]`.
fn domain_json(
    domain: &Domain,
    at: &JsonPath,
    limits: &QueryLimits,
) -> Result<Json, QueryAstError> {
    if domain.is_true() {
        return Ok(Json::Array(Vec::new()));
    }
    let mut items = Vec::new();
    for item in domain.to_list() {
        items.push(value_json(&item, at, 0, limits)?);
    }
    Ok(Json::Array(items))
}

fn value_json(
    value: &Value,
    at: &JsonPath,
    depth: usize,
    limits: &QueryLimits,
) -> Result<Json, QueryAstError> {
    let refused = |what: String, why: &'static str| {
        QueryAstError::new(at, QueryAstReason::RefusedFeature { what, why })
    };
    Ok(match value {
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(n) => Json::Int(*n),
        Value::Float(f) => Json::Float(*f),
        Value::Str(s) => text(s),
        Value::List(items) => {
            let inner = depth.saturating_add(1);
            if inner > limits.json_depth {
                return Err(QueryAstError::new(
                    at,
                    QueryAstReason::TooLarge {
                        bound: Bound::JsonDepth,
                        max: limits.json_depth.widened(),
                        found: inner.to_string(),
                    },
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(value_json(item, at, inner, limits)?);
            }
            Json::Array(out)
        }
        Value::Param(param) => {
            let name = match param {
                PrincipalParam::Uid => "uid",
                PrincipalParam::CompanyId => "company_id",
                PrincipalParam::CompanyIds => "company_ids",
                PrincipalParam::Now(NowKind::Date) => "today",
                PrincipalParam::Now(NowKind::Timestamp) => "now",
                other => {
                    return Err(refused(
                        format!("the parameter {}", other.symbol()),
                        "only uid, company_id, company_ids, today and now are stored",
                    ));
                }
            };
            Json::Object(vec![member("param", text(name))])
        }
        other => {
            return Err(refused(
                format!("a {:?} value", other.kind()),
                "a document holds JSON values and principal parameters only",
            ));
        }
    })
}
