// SPDX-License-Identifier: MIT OR Apache-2.0
//! Plan a bounded assistant aggregation query for the audited `web_read_group` dispatch.
use erp_sdk::{
    FieldType,
    access::Operation,
    ai::{DataSource, LabelledData, ToolCall, ToolName, ToolResult, ToolSpec},
    prelude::*,
    settings::ParameterDecl,
};
use serde_json::{Map, Value as Json, json};
use std::collections::BTreeSet;

const TOOL: &str = "aggregate_records";
const ROWS: &str = "ai_base.aggregate_rows";
const AGG_FIELDS: &str = "ai_base.aggregate_fields";
const GROUPBY_FIELDS: &str = "ai_base.aggregate_groupby";
const TERMS: &str = "ai_base.aggregate_terms";
const BYTES: &str = "ai_base.aggregate_result_bytes";
const OPERAND: &str = "ai_base.aggregate_operand_bytes";

/// Maximum groups that can also be looked up in a single authorized record search.
///
/// # Errors
/// Returns the underlying configuration error if the administrator's read limits cannot be read.
pub async fn ranked_lookup_limit(tx: &mut Transaction) -> Result<usize> {
    let groups = crate::catalog::limit(tx, ROWS).await?;
    let rows = crate::catalog::limit(tx, crate::search::ROWS).await?;
    Ok(groups.min(rows))
}

fn parameters() -> Result<Vec<ParameterDecl>> {
    Ok(vec![
        ParameterDecl::integer(
            ROWS,
            "Assistant: maximum groups per aggregation",
            50,
            (1, 80),
        )?,
        ParameterDecl::integer(
            AGG_FIELDS,
            "Assistant: maximum aggregate measures",
            8,
            (1, 32),
        )?,
        ParameterDecl::integer(
            GROUPBY_FIELDS,
            "Assistant: maximum groupby fields",
            4,
            (1, 16),
        )?,
        ParameterDecl::integer(
            TERMS,
            "Assistant: maximum aggregate filter conditions",
            16,
            (1, 64),
        )?,
        ParameterDecl::integer(
            BYTES,
            "Assistant: maximum labelled aggregate result bytes",
            32768,
            (4096, 65536),
        )?,
        ParameterDecl::integer(
            OPERAND,
            "Assistant: maximum aggregate operand bytes",
            256,
            (1, 2048),
        )?,
    ])
}
erp_sdk::system_parameters!(parameters);

fn invalid() -> Error {
    Error::validation(
        "aggregate_records requires model, domain, groupby array, aggregates array, limit, and order.",
    )
}

fn denied() -> Error {
    Error::access("Assistant aggregation cannot access this model or field.")
}

fn name() -> Result<ToolName> {
    ToolName::new(TOOL).map_err(|e| Error::programming(e.to_string()))
}

fn ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.as_bytes()[0].is_ascii_digit()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A provider tool with a strict schema for group-by and aggregate calculations.
///
/// # Errors
/// Returns a programming error for an invalid built-in declaration.
pub fn aggregate_records_spec() -> Result<ToolSpec> {
    let scalar = json!({"anyOf":[{"type":"string"},{"type":"number"},{"type":"boolean"}]});
    let Json::Object(schema) = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "model": {
                "type": "string",
                "description": "Installed target model identifier (such as purchase.order or account.move)",
            },
            "domain": {
                "type": "array",
                "description": "List of AND conditions [[field, operator, scalar value]], or [] for all records in the company",
                "items": {"type": "array", "items": scalar}
            },
            "groupby": {
                "type": "array",
                "description": "Group fields such as state, partner_id or date_order:month; use [] for a whole-query KPI. Group monetary measures by their currency field or filter to one currency.",
                "items": {"type": "string"}
            },
            "aggregates": {
                "type": "array",
                "description": "List of aggregate measures (e.g. ['amount_total:sum', 'id:count'])",
                "items": {"type": "string"}
            },
            "limit": {
                "type": "integer",
                "description": "Maximum groups to return (1-50)"
            },
            "order": {
                "type": "string",
                "description": "Sort order for the groups (e.g. 'amount_total desc' or 'id:count desc')"
            }
        },
        "required": ["model", "domain", "groupby", "aggregates", "limit", "order"]
    }) else {
        return Err(Error::programming("Invalid aggregate_records schema."));
    };

    ToolSpec::new(
        name()?,
        "Compute aggregate sums, counts and group-by metrics across business records of an installed model.",
        schema,
    )
    .map_err(|e| Error::programming(e.to_string()))
}

/// Approved aggregation plan for standard `web_read_group` execution.
#[derive(Clone, PartialEq)]
pub struct AggregatePlan {
    model: String,
    groupby: Vec<String>,
    aggregates: Vec<String>,
    domain: Json,
    order: String,
    limit: usize,
    company: i64,
    byte_limit: usize,
}

impl std::fmt::Debug for AggregatePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregatePlan")
            .field("model", &self.model)
            .field("groupby", &self.groupby)
            .field("aggregates", &self.aggregates)
            .finish_non_exhaustive()
    }
}

impl AggregatePlan {
    /// The installed model for the RPC call.
    pub fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn columns(&self) -> impl Iterator<Item = &str> {
        self.groupby
            .iter()
            .chain(&self.aggregates)
            .map(String::as_str)
    }

    /// Multi-axis analytics use the flat pivot query; the list API groups only the first axis.
    pub fn rpc_method(&self) -> &'static str {
        if self.groupby.len() != 1 || self.order.contains(':') {
            "formatted_read_group"
        } else {
            "web_read_group"
        }
    }

    /// The arguments for the standard list or pivot RPC binder.
    pub fn rpc_arguments(&self) -> Json {
        let mut map = serde_json::Map::new();
        map.insert("domain".to_string(), self.domain.clone());
        map.insert("groupby".to_string(), json!(self.groupby));
        let mut aggregates = self.aggregates.clone();
        if self.rpc_method() == "formatted_read_group" {
            aggregates.push("__count".to_owned());
        }
        map.insert("aggregates".to_owned(), json!(aggregates));
        map.insert(
            "limit".to_string(),
            json!(self.limit + usize::from(self.rpc_method() == "formatted_read_group")),
        );
        map.insert("offset".to_string(), json!(0));
        if !self.order.is_empty() {
            // A replayed top-N query must pick the same groups when measure values tie.
            let order = match self.groupby.as_slice() {
                [group]
                    if self.order.split_whitespace().next().is_some_and(|key| {
                        self.aggregates.iter().any(|measure| measure == key)
                    }) =>
                {
                    format!("{}, {group} asc", self.order)
                }
                _ => self.order.clone(),
            };
            map.insert("order".to_string(), json!(order));
        }
        map.insert(
            "context".to_string(),
            json!({ "allowed_company_ids": [self.company] }),
        );
        Json::Object(map)
    }

    /// Reduce an audited successful `web_read_group` response to bounded labelled data.
    ///
    /// # Errors
    /// Enforces response structure and result byte limits.
    pub fn project(&self, call: &ToolCall, reply: &Json) -> Result<ToolResult> {
        let malformed =
            || Error::internal("Unexpected web_read_group result for assistant aggregation.");
        let (groups, total_groups) = if self.rpc_method() == "formatted_read_group" {
            let groups = reply
                .as_array()
                .filter(|v| v.len() <= self.limit + 1)
                .ok_or_else(malformed)?;
            (
                groups,
                (groups.len() <= self.limit).then_some(groups.len() as u64),
            )
        } else {
            let groups = reply["groups"]
                .as_array()
                .filter(|v| v.len() <= self.limit)
                .ok_or_else(malformed)?;
            let length = reply["length"]
                .as_u64()
                .filter(|&n| n >= groups.len() as u64)
                .ok_or_else(malformed)?;
            (groups, Some(length))
        };
        let truncated = total_groups.is_none_or(|n| n > self.limit as u64);
        let mut projected_groups = Vec::new();
        for group in groups.iter().take(self.limit) {
            let obj = group.as_object().ok_or_else(malformed)?;
            let mut row = Map::new();
            for g in &self.groupby {
                row.insert(g.clone(), obj.get(g).ok_or_else(malformed)?.clone());
            }
            for agg in &self.aggregates {
                let value = obj.get(agg).ok_or_else(malformed)?;
                row.insert(agg.clone(), value.clone());
                // Retain the original short name only when it denotes exactly one measure.
                let short = agg.split(':').next().unwrap_or(agg);
                if !row.contains_key(short)
                    && self
                        .aggregates
                        .iter()
                        .filter(|other| other.split(':').next() == Some(short))
                        .count()
                        == 1
                {
                    row.insert(short.to_owned(), value.clone());
                }
            }
            row.insert(
                "__count".into(),
                obj.get("__count").ok_or_else(malformed)?.clone(),
            );
            projected_groups.push(Json::Object(row));
        }

        let source = DataSource::new(name()?, Some(self.model.clone()), Vec::new())
            .map_err(|e| Error::programming(e.to_string()))?;

        let data = json!({
            "model": self.model,
            "groups": projected_groups,
            "total_groups": total_groups,
            "truncated":truncated,
            "query": {"model":self.model,"domain":self.domain,"groupby":self.groupby,
                "aggregates":self.aggregates,"order":self.order,"limit":self.limit},
            "groupby": self.groupby,
            "aggregates": self.aggregates
        });

        let serialized_len = serde_json::to_vec(&data).map_err(|_| malformed())?.len();
        if serialized_len > self.byte_limit {
            return Err(Error::validation(
                "Assistant aggregation result exceeds byte budget.",
            ));
        }

        Ok(ToolResult::new(
            call.id().clone(),
            LabelledData::new(source, data),
            false,
        ))
    }
}

/// Validate a typed `aggregate_records` call against the transaction's user context,
/// company permissions, model access rights, and field visibility.
///
/// # Errors
/// Refuses unprivileged or missing company context, inaccessible models, invalid fields,
/// or malformed argument structures.
pub async fn prepare(tx: &mut Transaction, call: &ToolCall) -> Result<AggregatePlan> {
    let env = tx.env();
    if !crate::ordinary_user(&env) {
        return Err(denied());
    }
    prepare_authorized(tx, call).await
}

// MCP may save a definition only for sources its own bearer could aggregate.
// The returned plan is always checked again under the actual viewer before any read.
pub(crate) async fn prepare_saved(tx: &mut Transaction, call: &ToolCall) -> Result<AggregatePlan> {
    let env = tx.env();
    if !crate::ordinary_user(&env) {
        let p = env.principal();
        let model = call.arguments()["model"].as_str().ok_or_else(invalid)?;
        let plan = if p.kind() == erp_sdk::PrincipalKind::Agent
            && p.actor().is_none()
            && !env.is_superuser()
            && p.scope().is_some_and(|s| s.models().allows(model))
        {
            prepare_authorized(tx, call).await?
        } else {
            return Err(denied());
        };
        let method = plan.rpc_method();
        if !p
            .scope()
            .is_some_and(|scope| scope.methods().allows(method))
        {
            return Err(denied());
        }
        return Ok(plan);
    }
    prepare_authorized(tx, call).await
}

async fn prepare_authorized(tx: &mut Transaction, call: &ToolCall) -> Result<AggregatePlan> {
    let env = tx.env();
    let company = env.company()?.ok_or_else(denied)?.get();
    let args = call.arguments().as_object().ok_or_else(invalid)?;
    const KEYS: [&str; 7] = [
        "model",
        "domain",
        "groupby",
        "group_by",
        "aggregates",
        "limit",
        "order",
    ];
    if call.name() != TOOL
        || args.keys().any(|k| !KEYS.contains(&k.as_str()))
        || (args.contains_key("group_by") && args.contains_key("groupby"))
    {
        return Err(invalid());
    }

    let model = args
        .get("model")
        .and_then(Json::as_str)
        .ok_or_else(invalid)?;
    if model.len() > 128 || !model.split('.').all(ident) {
        return Err(invalid());
    }

    if env
        .registry()
        .model(model)
        .is_none_or(|spec| spec.is_abstract())
    {
        return Err(denied());
    }

    let recs = env.records(model)?;
    if !recs.has_access(tx, Operation::Read).await? {
        return Err(denied());
    }

    let limit = match args.get("limit") {
        Some(raw) => raw
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(invalid)?,
        None => 20,
    };
    let max_rows = crate::catalog::limit(tx, ROWS).await?;
    if limit == 0 || limit > max_rows {
        return Err(Error::validation(format!(
            "aggregate_records limit must be between 1 and {max_rows}."
        )));
    }

    let raw_groupby = args
        .get("groupby")
        .or_else(|| args.get("group_by"))
        .and_then(Json::as_array)
        .ok_or_else(invalid)?;
    let max_groupby = crate::catalog::limit(tx, GROUPBY_FIELDS).await?;
    if raw_groupby.len() > max_groupby {
        return Err(Error::validation(format!(
            "aggregate_records groupby may include at most {max_groupby} fields."
        )));
    }

    let mut groupby = Vec::new();
    for g in raw_groupby {
        let name = g.as_str().ok_or_else(invalid)?;
        let (field, bucket) = name
            .split_once(':')
            .map_or((name, None), |(f, b)| (f, Some(b)));
        if !ident(field)
            || bucket.is_some_and(|b| {
                !matches!(b, "hour" | "day" | "week" | "month" | "quarter" | "year")
            })
            || groupby.iter().any(|g| g == name)
        {
            return Err(invalid());
        }
        groupby.push(name.to_string());
    }

    let raw_aggregates = args
        .get("aggregates")
        .and_then(Json::as_array)
        .ok_or_else(invalid)?;
    let max_aggs = crate::catalog::limit(tx, AGG_FIELDS).await?;
    if raw_aggregates.is_empty() || raw_aggregates.len() > max_aggs {
        return Err(Error::validation(format!(
            "aggregate_records aggregates must have between 1 and {max_aggs} measures."
        )));
    }

    let mut aggregates = Vec::new();
    for agg in raw_aggregates {
        let text = match agg {
            Json::String(s) => s.clone(),
            Json::Object(o) => {
                let f = o.get("field").and_then(Json::as_str).unwrap_or("");
                let func = o.get("function").and_then(Json::as_str).unwrap_or("sum");
                if f.is_empty() {
                    return Err(invalid());
                }
                format!("{f}:{func}")
            }
            _ => return Err(invalid()),
        };
        let mut parts = text.split(':');
        let clean_field = parts.next().unwrap_or("");
        let func = parts.next().unwrap_or("sum");
        if parts.next().is_some()
            || !ident(clean_field)
            || !matches!(
                func,
                "sum" | "avg" | "min" | "max" | "count" | "bool_and" | "bool_or"
            )
        {
            return Err(invalid());
        }
        let spec = format!("{clean_field}:{func}");
        if aggregates.contains(&spec) {
            return Err(Error::validation("Aggregate measures must be unique."));
        }
        aggregates.push(spec);
    }

    let empty_domain = vec![];
    let raw_domain = args
        .get("domain")
        .map(|d| d.as_array().ok_or_else(invalid))
        .transpose()?
        .unwrap_or(&empty_domain);
    let max_terms = crate::catalog::limit(tx, TERMS).await?;
    if raw_domain.len() > max_terms {
        return Err(Error::validation(format!(
            "aggregate_records domain exceeds {max_terms} conditions."
        )));
    }

    let order_input = args
        .get("order")
        .map(|o| o.as_str().ok_or_else(invalid))
        .transpose()?
        .unwrap_or("");
    let order = normalize_order(order_input, &groupby, &aggregates)?;
    if order.len() > 70 {
        return Err(invalid());
    }

    // Verify fields exist and are accessible on the model
    let mut check_fields: BTreeSet<&str> = BTreeSet::new();
    for g in &groupby {
        check_fields.insert(g.split(':').next().unwrap_or(g));
    }
    for agg in &aggregates {
        let clean = agg.split(':').next().unwrap_or(agg.as_str());
        check_fields.insert(clean);
    }
    let operand_limit = crate::catalog::limit(tx, OPERAND).await?;
    for item in raw_domain {
        let triple = item.as_array().ok_or_else(invalid)?;
        let [Json::String(field), Json::String(op), rhs] = triple.as_slice() else {
            return Err(invalid());
        };
        if !ident(field) || !matches!(op.as_str(), "=" | "!=" | ">" | "<" | ">=" | "<=" | "ilike") {
            return Err(invalid());
        }
        match rhs {
            Json::Bool(_) | Json::Number(_) if op != "ilike" => {}
            Json::String(v) if v.len() <= operand_limit => {}
            _ => return Err(invalid()),
        }
        check_fields.insert(field.as_str());
    }

    let check_vec: Vec<&str> = check_fields.into_iter().collect();
    let readable = recs
        .fields_get(tx, Some(&check_vec), Some(&["readonly"]))
        .await?;

    let is_safe_field = |field: &str| {
        readable.contains_key(field)
            && recs.spec().field(field).is_some_and(|s| {
                s.store
                    && !s.secret
                    && !s.is_generated()
                    && matches!(
                        s.ty,
                        FieldType::Id
                            | FieldType::Boolean
                            | FieldType::Integer
                            | FieldType::BigInt
                            | FieldType::Float { .. }
                            | FieldType::Monetary { .. }
                            | FieldType::Char { .. }
                            | FieldType::Selection { .. }
                            | FieldType::Date
                            | FieldType::Datetime
                            | FieldType::Many2one { .. }
                    )
            })
    };

    for f in &groupby {
        let (field, bucket) = f
            .split_once(':')
            .map_or((f.as_str(), None), |(field, bucket)| (field, Some(bucket)));
        if !is_safe_field(field) {
            return Err(denied());
        }
        let date = recs
            .spec()
            .field(field)
            .is_some_and(|field| matches!(field.ty, FieldType::Date | FieldType::Datetime));
        if date != bucket.is_some()
            || (bucket == Some("hour")
                && recs
                    .spec()
                    .field(field)
                    .is_some_and(|field| matches!(field.ty, FieldType::Date)))
        {
            return Err(Error::validation(
                "Date groupings need an appropriate time interval, such as date_order:month.",
            ));
        }
    }
    for agg in &aggregates {
        let clean = agg.split(':').next().unwrap_or(agg.as_str());
        if !is_safe_field(clean) {
            return Err(denied());
        }
        let metadata = recs.spec().field(clean).ok_or_else(denied)?;
        let operation = agg.split_once(':').ok_or_else(invalid)?.1;
        validate_measure(&metadata.ty, operation)?;
        if agg.split(':').nth(1) != Some("count")
            && let Some(FieldType::Monetary {
                currency_field: Some(currency),
            }) = recs.spec().field(clean).map(|f| &f.ty)
            && !single_currency(&groupby, raw_domain, currency)
        {
            return Err(Error::validation(format!(
                "Group or filter on {currency} before combining currency amounts."
            )));
        }
    }
    for item in raw_domain {
        let triple = item.as_array().ok_or_else(invalid)?;
        let [Json::String(field), _, _] = triple.as_slice() else {
            return Err(invalid());
        };
        if !is_safe_field(field.as_str()) {
            return Err(denied());
        }
    }
    let domain = Json::Array(raw_domain.clone());
    if !raw_domain.is_empty() {
        Domain::from_json(&domain).map_err(|_| invalid())?;
    }

    Ok(AggregatePlan {
        model: model.to_string(),
        groupby,
        aggregates,
        domain,
        order,
        limit,
        company,
        byte_limit: crate::catalog::limit(tx, BYTES).await?,
    })
}

fn validate_measure(ty: &FieldType, operation: &str) -> Result<()> {
    let supported = !matches!(ty, FieldType::BigInt)
        && match operation {
            "count" => true,
            "avg" | "sum" => matches!(
                ty,
                FieldType::Integer | FieldType::Float { .. } | FieldType::Monetary { .. }
            ),
            "bool_and" | "bool_or" => matches!(ty, FieldType::Boolean),
            "min" | "max" => !matches!(ty, FieldType::Boolean | FieldType::Many2one { .. }),
            _ => false,
        };
    if supported {
        Ok(())
    } else {
        Err(Error::validation(
            "This aggregation is unsupported for the selected field type.",
        ))
    }
}

// Pivot ordering accepts qualified measures; the single-axis list endpoint orders by bare fields.
fn normalize_order(raw: &str, groups: &[String], measures: &[String]) -> Result<String> {
    let mut words = raw.split_ascii_whitespace();
    let Some(key) = words.next() else {
        return Ok(String::new());
    };
    let direction = words.next().unwrap_or("asc").to_ascii_lowercase();
    if !matches!(direction.as_str(), "asc" | "desc") || words.next().is_some() {
        return Err(invalid());
    }
    let matched = if key == "__count" || groups.iter().chain(measures).any(|item| item == key) {
        key
    } else {
        let mut matches = groups
            .iter()
            .chain(measures)
            .filter(|item| item.split_once(':').is_some_and(|(field, _)| field == key));
        let item = matches.next().ok_or_else(invalid)?;
        if matches.next().is_some() {
            return Err(Error::validation(
                "Specify the exact group or measure to order by.",
            ));
        }
        item
    };
    Ok(format!("{matched} {direction}"))
}

pub(crate) fn single_currency(groupby: &[String], domain: &[Json], currency: &str) -> bool {
    groupby.iter().any(|field| field == currency)
        || domain.iter().any(|term| {
            term.as_array().is_some_and(|parts| {
                parts.len() == 3
                    && parts[0] == currency
                    && parts[1] == "="
                    && parts[2].as_i64().is_some_and(|id| id > 0)
            })
        })
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use erp_sdk::ai::ToolCallId;
    #[test]
    fn measures_are_checked_against_supported_source_types() {
        assert!(validate_measure(&FieldType::Integer, "avg").is_ok());
        assert!(validate_measure(&FieldType::BigInt, "count").is_err());
        assert!(validate_measure(&FieldType::Boolean, "sum").is_err());
        assert!(validate_measure(&FieldType::Boolean, "bool_and").is_ok());
        assert!(validate_measure(&FieldType::Date, "max").is_ok());
    }
    #[test]
    fn a_limited_ranking_breaks_measure_ties_before_slicing_the_groups() {
        let query = AggregatePlan {
            model: "sale.order.line".into(),
            groupby: vec!["product_id".into()],
            aggregates: vec!["product_uom_qty:sum".into()],
            domain: json!([]),
            order: "product_uom_qty:sum desc".into(),
            limit: 3,
            company: 1,
            byte_limit: 4096,
        };
        assert_eq!(
            query.rpc_arguments()["order"],
            "product_uom_qty:sum desc, product_id asc"
        );
        assert_eq!(query.rpc_arguments()["limit"], 4);
    }

    #[test]
    fn multi_axis_queries_keep_distinct_measures_and_do_not_guess_a_total() {
        let query = AggregatePlan {
            model: "sale.order".into(),
            groupby: vec!["state".into(), "currency_id".into()],
            aggregates: vec!["amount:sum".into(), "amount:avg".into()],
            domain: json!([]),
            order: String::new(),
            limit: 1,
            company: 1,
            byte_limit: 4096,
        };
        let call = ToolCall::new(
            ToolCallId::new("metric").unwrap(),
            "aggregate_records".into(),
            json!({}),
        )
        .unwrap();
        assert_eq!(query.rpc_method(), "formatted_read_group");
        assert_eq!(query.rpc_arguments()["limit"], 2);
        assert_eq!(
            query.rpc_arguments()["aggregates"],
            json!(["amount:sum", "amount:avg", "__count"])
        );
        assert!(normalize_order("amount desc", &query.groupby, &query.aggregates).is_err());
        assert_eq!(
            normalize_order("amount:sum desc", &query.groupby, &query.aggregates).unwrap(),
            "amount:sum desc"
        );
        let group = json!({"state":"sale","currency_id":[1,"USD"],"amount:sum":100,"amount:avg":25,"__count":4});
        let reply = query.project(&call, &json!([group, group])).unwrap();
        let val = reply.data().value();
        assert_eq!(val["groups"][0]["amount:sum"], 100);
        assert_eq!(val["groups"][0]["amount:avg"], 25);
        assert!(val["groups"][0].get("amount").is_none());
        assert_eq!(val["truncated"], true);
        assert!(val["total_groups"].is_null());
    }
}
