// SPDX-License-Identifier: MIT OR Apache-2.0
//! Plan a bounded assistant read for the normal audited `web_search_read` dispatch.
use erp_sdk::{
    FieldType,
    access::Operation,
    ai::{DataSource, LabelledData, ToolCall, ToolName, ToolResult, ToolSpec},
    prelude::*,
    settings::ParameterDecl,
};
use serde_json::{Map, Value as Json, json};
use std::collections::BTreeSet;

const TOOL: &str = "search_records";
pub(crate) const ROWS: &str = "ai_base.search_rows";
const FIELDS: &str = "ai_base.search_fields";
const TERMS: &str = "ai_base.search_terms";
const TEXT: &str = "ai_base.search_text_chars";
const BYTES: &str = "ai_base.search_result_bytes";
const OPERAND: &str = "ai_base.search_operand_bytes";
fn parameters() -> Result<Vec<ParameterDecl>> {
    Ok(vec![
        ParameterDecl::integer(ROWS, "Assistant: maximum records per search", 20, (1, 80))?,
        ParameterDecl::integer(FIELDS, "Assistant: maximum requested fields", 16, (1, 64))?,
        ParameterDecl::integer(TERMS, "Assistant: maximum search conditions", 12, (1, 64))?,
        ParameterDecl::integer(
            TEXT,
            "Assistant: maximum characters per result value",
            512,
            (1, 2000),
        )?,
        ParameterDecl::integer(
            BYTES,
            "Assistant: maximum labelled search result bytes",
            32768,
            (4096, 65536),
        )?,
        ParameterDecl::integer(
            OPERAND,
            "Assistant: maximum search operand bytes",
            256,
            (1, 2048),
        )?,
    ])
}
erp_sdk::system_parameters!(parameters);

fn invalid() -> Error {
    Error::validation(
        "search_records requires model, nonempty domain, explicit fields, limit and a single sort field.",
    )
}
fn denied() -> Error {
    Error::access("Assistant search cannot access this model or field.")
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
fn valid_id_list(value: &Json, max_rows: usize) -> bool {
    value.as_array().is_some_and(|ids| {
        !ids.is_empty()
            && ids.len() <= max_rows
            && ids.iter().all(|id| id.as_i64().is_some_and(|n| n > 0))
    })
}
/// A provider tool with a closed schema for direct scalar searches.
///
/// # Errors
/// Returns a programming error for an invalid built-in declaration.
pub fn search_records_spec() -> Result<ToolSpec> {
    let operand = json!({"anyOf":[{"type":"string"},{"type":"number"},{"type":"boolean"},{"type":"array","items":{"type":"integer"}}]});
    let Json::Object(schema) = json!({"type":"object","additionalProperties":false,
      "properties":{"model":{"type":"string"},"domain":{"type":"array","description":"AND conditions. For exact records use [id, in, [positive IDs]] within the configured row limit.","items":{"type":"array","items":operand}},
      "fields":{"type":"array","items":{"type":"string"}},"limit":{"type":"integer"},"order":{"type":"string"}},
      "required":["model","domain","fields","limit","order"]})
    else {
        return Err(Error::programming("Invalid search_records schema."));
    };
    ToolSpec::new(
        name()?,
        "Read records using the current user's permissions and selected company.",
        schema,
    )
    .map_err(|e| Error::programming(e.to_string()))
}

/// Approved model, query and projection. The host validates the plan again in the actual
/// RPC attempt's session before its business method executes.
#[derive(Clone, PartialEq)]
pub struct SearchPlan {
    model: String,
    fields: BTreeSet<String>,
    domain: Json,
    order: String,
    limit: usize,
    company: i64,
    text_limit: usize,
    byte_limit: usize,
}
impl std::fmt::Debug for SearchPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchPlan")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}
impl SearchPlan {
    /// The installed model for the ordinary RPC call.
    pub fn model(&self) -> &str {
        &self.model
    }
    /// The arguments for the standard `web_search_read` binder. No caller-supplied context is forwarded.
    pub fn rpc_arguments(&self) -> Json {
        let fields: Map<_, _> = self.fields.iter().map(|f| (f.clone(), json!({}))).collect();
        json!({"domain":self.domain,"specification":fields,"order":self.order,"limit":self.limit,
          "count_limit":self.limit+1,"context":{"allowed_company_ids":[self.company]}})
    }
    /// Reduce an audited successful response to the admitted scalar projection and return its provenance.
    ///
    /// # Errors
    /// Refuses malformed RPC responses and enforces the labelled result budget.
    pub fn project(&self, call: &ToolCall, reply: &Json) -> Result<ToolResult> {
        let malformed =
            || Error::internal("Unexpected web_search_read result for assistant search.");
        let records = reply["records"].as_array().ok_or_else(malformed)?;
        let length = reply["length"].as_u64().ok_or_else(malformed)?;
        if records.len() > self.limit
            || length < u64::try_from(records.len()).map_err(|_| malformed())?
        {
            return Err(malformed());
        }
        let mut shortened = length > records.len() as u64;
        let mut ids = Vec::new();
        let mut rows = Vec::new();
        let labelled = |ids, rows, shortened| -> Result<LabelledData> {
            let source = DataSource::new(name()?, Some(self.model.clone()), ids)
                .map_err(|e| Error::programming(e.to_string()))?;
            Ok(LabelledData::new(
                source,
                json!({"records":rows,"truncated":shortened}),
            ))
        };
        let mut bytes = serde_json::to_vec(&labelled(Vec::new(), Vec::<Json>::new(), false)?)
            .map_err(|_| malformed())?
            .len();
        for record in records {
            let id = record["id"]
                .as_i64()
                .filter(|id| *id > 0)
                .ok_or_else(malformed)?;
            let mut row = Map::new();
            row.insert("id".into(), json!(id));
            for field in &self.fields {
                let value = record.get(field).ok_or_else(malformed)?;
                let cell = match value {
                    Json::Bool(_) | Json::Null | Json::Number(_) => value.clone(),
                    Json::String(value) => {
                        let mut chars = value.chars();
                        let clipped: String = chars.by_ref().take(self.text_limit).collect();
                        shortened |= chars.next().is_some();
                        Json::String(clipped)
                    }
                    _ => return Err(malformed()),
                };
                row.insert(field.clone(), cell);
            }
            let mut row = Json::Object(row);
            let overhead = id.to_string().len() + 2 * usize::from(!rows.is_empty());
            let mut size = serde_json::to_vec(&row).map_err(|_| malformed())?.len() + overhead;
            if size > self.byte_limit.saturating_sub(bytes) {
                shortened = true;
                if !rows.is_empty() {
                    break;
                }
                row = json!({"id":id});
                size = serde_json::to_vec(&row).map_err(|_| malformed())?.len() + overhead;
                if size > self.byte_limit.saturating_sub(bytes) {
                    return Err(Error::validation(
                        "Assistant result budget cannot fit the first record ID.",
                    ));
                }
            }
            bytes += size;
            ids.push(id);
            rows.push(row);
        }
        Ok(ToolResult::new(
            call.id().clone(),
            labelled(ids, rows, shortened)?,
            false,
        ))
    }
}
/// Validate a typed tool call against the actual transaction's ordinary user, current company,
/// model ACL, field visibility and adjustable database admission limits. Returned filters are
/// direct scalar comparisons combined with AND. Field paths and relation expansion are refused.
///
/// # Errors
/// Access refusals conceal unknown or unreadable models and predicate fields. Malformed shapes,
/// unsupported domains and bounds are reported as validation errors.
pub async fn prepare(tx: &mut Transaction, call: &ToolCall) -> Result<SearchPlan> {
    let env = tx.env();
    if !crate::ordinary_user(&env) {
        return Err(denied());
    }
    let company = env.company()?.ok_or_else(denied)?.get();
    let args = call.arguments().as_object().ok_or_else(invalid)?;
    if call.name() != TOOL || args.len() != 5 {
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
    let limit = args
        .get("limit")
        .and_then(Json::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(invalid)?;
    let max_rows = crate::catalog::limit(tx, ROWS).await?;
    if limit == 0 || limit > max_rows {
        return Err(Error::validation(
            "search_records limit exceeds ai_base.search_rows or is zero.",
        ));
    }
    let fields = args
        .get("fields")
        .and_then(Json::as_array)
        .ok_or_else(invalid)?;
    if fields.is_empty() || fields.len() > crate::catalog::limit(tx, FIELDS).await? {
        return Err(invalid());
    }
    let domain = args
        .get("domain")
        .and_then(Json::as_array)
        .ok_or_else(invalid)?;
    if domain.is_empty() || domain.len() > crate::catalog::limit(tx, TERMS).await? {
        return Err(invalid());
    }
    let order = args
        .get("order")
        .and_then(Json::as_str)
        .ok_or_else(invalid)?;
    if order.len() > 70 {
        return Err(invalid());
    }
    let mut sort = order.split_ascii_whitespace();
    let sort_field = sort.next().filter(|f| ident(f)).ok_or_else(invalid)?;
    let direction = sort.next().unwrap_or("asc");
    if !matches!(direction, "asc" | "desc") || sort.next().is_some() {
        return Err(invalid());
    }
    // Every returned record carries its id in both its row and its provenance.
    let mut predicates = BTreeSet::from([sort_field, "id"]);
    let operand_limit = crate::catalog::limit(tx, OPERAND).await?;
    for item in domain {
        let triple = item.as_array().ok_or_else(invalid)?;
        let [Json::String(field), Json::String(op), rhs] = triple.as_slice() else {
            return Err(invalid());
        };
        if field == "id" && op == "in" {
            if !valid_id_list(rhs, max_rows) {
                return Err(invalid());
            }
            continue;
        }
        if !ident(field) || !matches!(op.as_str(), "=" | "!=" | ">" | "<" | ">=" | "<=" | "ilike") {
            return Err(invalid());
        }
        match rhs {
            Json::Bool(_) | Json::Number(_) if op != "ilike" => {}
            Json::String(v) if v.len() <= operand_limit => {}
            _ => return Err(invalid()),
        }
        predicates.insert(field.as_str());
    }
    let projection = fields
        .iter()
        .map(|v| v.as_str().filter(|f| ident(f)).ok_or_else(invalid))
        .collect::<Result<BTreeSet<_>>>()?;
    if projection.len() != fields.len() {
        return Err(invalid());
    }
    let all: Vec<_> = predicates.union(&projection).copied().collect();
    let readable = recs.fields_get(tx, Some(&all), Some(&["readonly"])).await?;
    let safe = |field: &str| {
        readable.contains_key(field)
            && recs.spec().field(field).is_some_and(|s| {
                s.store
                    && !s.secret
                    && s.related.is_none()
                    && s.inherited.is_none()
                    && !s.is_generated()
                    && (s.compute.is_none() || (model == "product.product" && field == "lst_price"))
                    && matches!(
                        s.ty,
                        FieldType::Id
                            | FieldType::Boolean
                            | FieldType::Integer
                            | FieldType::BigInt
                            | FieldType::Float { .. }
                            | FieldType::Monetary { .. }
                            | FieldType::Char { .. }
                            | FieldType::Text
                            | FieldType::Selection { .. }
                            | FieldType::Date
                            | FieldType::Datetime
                            | FieldType::Uuid
                            | FieldType::TimestampTz
                    )
            })
    };
    if !predicates.iter().all(|f| safe(f)) {
        return Err(denied());
    }
    let fields: BTreeSet<String> = projection
        .into_iter()
        .filter(|f| safe(f))
        .map(str::to_owned)
        .collect();
    if fields.is_empty() {
        return Err(denied());
    }
    let domain = Json::Array(domain.clone());
    Domain::from_json(&domain).map_err(|_| invalid())?;
    Ok(SearchPlan {
        model: model.to_owned(),
        fields,
        domain,
        order: format!("{sort_field} {direction}"),
        limit,
        company,
        text_limit: crate::catalog::limit(tx, TEXT).await?,
        byte_limit: crate::catalog::limit(tx, BYTES).await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_sdk::ai::ToolCallId;

    #[test]
    fn id_filters_accept_only_bounded_positive_ids() {
        assert!(valid_id_list(&json!([83, 5, 120]), 3));
        for invalid in [
            json!([]),
            json!([1, 2, 3, 4]),
            json!([0]),
            json!(["1"]),
            json!([[1]]),
            json!([1.5]),
        ] {
            assert!(!valid_id_list(&invalid, 3));
        }
    }

    #[test]
    fn byte_limit_includes_escaped_text_and_provenance() {
        let plan = SearchPlan {
            model: "purchase.order".into(),
            fields: BTreeSet::from(["name".into()]),
            domain: json!([["id", ">", 0]]),
            order: "id asc".into(),
            limit: 3,
            company: 1,
            text_limit: 2000,
            byte_limit: 4096,
        };
        let call = ToolCall::new(
            ToolCallId::new("test-read").unwrap(),
            TOOL.into(),
            json!({}),
        )
        .unwrap();
        // The escaped payload exceeds the cap even though its character count fits.
        let name = "\n".repeat(2000);
        let full = json!({"id": 17, "name":name});
        let reply = json!({"length":2, "records":[{"id":2,"name":"ready"},full]});
        let result = plan.project(&call, &reply).unwrap();
        assert_eq!(result.data().source().ids(), &[2]);
        assert_eq!(
            result.data().value(),
            &json!({"records":[{"id":2,"name":"ready"}],"truncated":true})
        );
        let result = plan
            .project(&call, &json!({"length":1,"records":[full]}))
            .unwrap();
        assert_eq!(result.data().source().ids(), &[17]);
        assert_eq!(
            result.data().value(),
            &json!({"records":[{"id":17}],"truncated":true})
        );
        assert!(serde_json::to_vec(result.data()).unwrap().len() <= plan.byte_limit);
        let invalid = plan
            .project(&call, &json!({"length":1,"records":[{"id":2,"name":[1]}]}))
            .unwrap_err();
        assert_eq!(
            invalid.message(),
            "Unexpected web_search_read result for assistant search."
        );
    }
}
