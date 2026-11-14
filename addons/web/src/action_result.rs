// SPDX-License-Identifier: MIT OR Apache-2.0
//! Normalization of an object-method result into a typed [`wire::ActionResult`].
//!
//! The value a method returned is data: this module validates it against a closed vocabulary
//! before the browser ever sees it. It re-executes nothing, writes nothing and consults no
//! authority itself; the caller of [`normalize`] checks read access on the returned model.

use erp_sdk::prelude::*;
use erp_sdk::serde_json::{self, Map, Value as Json};
use erp_sdk::views::wire;
use std::collections::BTreeMap;

/// Largest serialized method result admitted.
pub(crate) const MAX_RESULT_BYTES: usize = 16 * 1024;
/// Deepest JSON nesting admitted in a result.
pub(crate) const MAX_RESULT_DEPTH: usize = 6;
const MAX_TEXT_BYTES: usize = 4096;
const MAX_TITLE_BYTES: usize = 256;
const MAX_MODEL_BYTES: usize = 256;
const MAX_CONTEXT_KEYS: usize = 64;
const ACT_WINDOW: &str = "ir.actions.act_window";
const ACT_CLOSE: &str = "ir.actions.act_window_close";
const ACT_CLIENT: &str = "ir.actions.client";
const NOTIFY_TAG: &str = "display_notification";

/// Registry questions the pure validator needs answered.
pub(crate) trait ModelLookup {
    /// Whether the model is registered.
    fn has_model(&self, model: &str) -> bool;
    /// Whether the registered model declares the field.
    fn has_field(&self, model: &str, field: &str) -> bool;
}

fn fail(message: impl Into<String>) -> Error {
    Error::validation(message)
}

fn kind_name(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

/// Clip attacker-controlled text before it enters an error message.
fn shown(text: &str) -> String {
    text.chars().take(64).collect()
}

/// Convert the RPC value to JSON with a hard depth bound; kinds JSON cannot carry are refused.
pub(crate) fn to_json(value: &Value, depth: usize) -> Result<Json> {
    if depth > MAX_RESULT_DEPTH {
        return Err(fail(format!(
            "Method result nests deeper than {MAX_RESULT_DEPTH} levels."
        )));
    }
    Ok(match value {
        Value::False => Json::Null,
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(number) => Json::from(*number),
        Value::Float(number) => serde_json::Number::from_f64(*number)
            .map(Json::Number)
            .ok_or_else(|| fail("Method result contains a non-finite number."))?,
        Value::Str(text) => Json::String(text.to_string()),
        Value::List(items) => Json::Array(
            items
                .iter()
                .map(|item| to_json(item, depth + 1))
                .collect::<Result<_>>()?,
        ),
        Value::Map(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, item)| Ok((key.clone(), to_json(item, depth + 1)?)))
                .collect::<Result<Map<_, _>>>()?,
        ),
        Value::Json(document) => (**document).clone(),
        _ => {
            return Err(fail(
                "Method result contains a value kind that an action result cannot carry.",
            ));
        }
    })
}

fn depth_of(value: &Json, depth: usize) -> Result<()> {
    if depth > MAX_RESULT_DEPTH {
        return Err(fail(format!(
            "Method result nests deeper than {MAX_RESULT_DEPTH} levels."
        )));
    }
    match value {
        Json::Array(items) => items.iter().try_for_each(|item| depth_of(item, depth + 1)),
        Json::Object(entries) => entries
            .values()
            .try_for_each(|item| depth_of(item, depth + 1)),
        _ => Ok(()),
    }
}

/// Validate a returned value into a typed result. Unknown or malformed input is a named error.
pub(crate) fn normalize(input: &Json, lookup: &dyn ModelLookup) -> Result<wire::ActionResult> {
    depth_of(input, 1)?;
    let size = serde_json::to_vec(input)
        .map_err(|error| fail(format!("Method result is not serializable: {error}")))?
        .len();
    if size > MAX_RESULT_BYTES {
        return Err(fail(format!(
            "Method result is {size} bytes; the limit is {MAX_RESULT_BYTES}."
        )));
    }
    match input {
        Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => {
            Ok(wire::ActionResult::None {})
        }
        Json::Object(members) => dictionary(members, lookup),
        other => Err(fail(format!(
            "A method result of kind {} is not an action result; only a scalar or an action dictionary is admitted.",
            kind_name(other)
        ))),
    }
}

fn reject_members(members: &Map<String, Json>, allowed: &[&str], what: &str) -> Result<()> {
    match members.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(fail(format!(
            "Member '{}' is not supported in a {what} result.",
            shown(key)
        ))),
        None => Ok(()),
    }
}

fn dictionary(members: &Map<String, Json>, lookup: &dyn ModelLookup) -> Result<wire::ActionResult> {
    let kind = match members.get("type") {
        Some(Json::String(kind)) => kind.as_str(),
        _ => return Err(fail("An action result requires a text 'type'.")),
    };
    match kind {
        ACT_CLOSE => {
            reject_members(members, &["type"], "close")?;
            Ok(wire::ActionResult::Close {})
        }
        ACT_WINDOW => window(members, lookup),
        ACT_CLIENT => notification(members),
        other => Err(fail(format!(
            "Action type '{}' is not supported as a method result.",
            shown(other)
        ))),
    }
}

fn text<'a>(
    members: &'a Map<String, Json>,
    key: &str,
    limit: usize,
    what: &str,
) -> Result<&'a str> {
    match members.get(key) {
        Some(Json::String(value)) if !value.trim().is_empty() && value.len() <= limit => Ok(value),
        Some(Json::String(value)) if value.len() > limit => Err(fail(format!(
            "{what} '{key}' exceeds the {limit}-byte limit."
        ))),
        _ => Err(fail(format!("{what} requires a nonempty text '{key}'."))),
    }
}

fn positive_id(value: &Json, what: &str) -> Result<wire::WireId> {
    value
        .as_u64()
        .and_then(|id| wire::WireId::try_from(id).ok())
        .ok_or_else(|| fail(format!("{what} must be a positive JSON-safe integer.")))
}

fn optional_id(members: &Map<String, Json>, key: &str, what: &str) -> Result<Option<wire::WireId>> {
    match members.get(key) {
        None | Some(Json::Null | Json::Bool(false)) => Ok(None),
        Some(value) => positive_id(value, what).map(Some),
    }
}

fn window(members: &Map<String, Json>, lookup: &dyn ModelLookup) -> Result<wire::ActionResult> {
    if members.get("target").and_then(Json::as_str) == Some("current") {
        return current_window(members, lookup);
    }
    reject_members(
        members,
        &[
            "type",
            "name",
            "res_model",
            "res_id",
            "target",
            "view_mode",
            "view_id",
            "views",
            "context",
        ],
        "window",
    )?;
    match members.get("target") {
        Some(Json::String(target)) if target == "new" => {}
        Some(Json::String(target)) => {
            return Err(fail(format!(
                "Window target '{}' is unsupported: only 'new' or 'current' is admitted from a method result.",
                shown(target)
            )));
        }
        _ => return Err(fail("A window result requires target 'new'.")),
    }
    let name = text(members, "name", MAX_TEXT_BYTES, "A window result")?;
    let model = text(members, "res_model", MAX_MODEL_BYTES, "A window result")?;
    if !model
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
    {
        return Err(fail(format!("Model name '{}' is invalid.", shown(model))));
    }
    if !lookup.has_model(model) {
        return Err(fail(format!("Unknown action model '{model}'.")));
    }
    let res_id = optional_id(members, "res_id", "Window res_id")?;
    let view = match members.get("views") {
        Some(Json::Array(list)) => {
            if members.contains_key("view_id") || members.contains_key("view_mode") {
                return Err(fail(
                    "A window result gives either 'views' or 'view_id'/'view_mode', not both.",
                ));
            }
            let [Json::Array(pair)] = list.as_slice() else {
                return Err(fail(
                    "A window result needs exactly one [id|false, 'form'] view entry.",
                ));
            };
            let [id, Json::String(view_type)] = pair.as_slice() else {
                return Err(fail("A window view entry must be [id|false, 'form']."));
            };
            if view_type != "form" {
                return Err(fail(format!(
                    "View type '{}' is unsupported in a method-result window; only 'form' opens.",
                    shown(view_type)
                )));
            }
            match id {
                Json::Null | Json::Bool(false) => None,
                id => Some(positive_id(id, "Window view identity")?),
            }
        }
        Some(_) => return Err(fail("A window result 'views' must be an array.")),
        None => {
            match members.get("view_mode") {
                None => {}
                Some(Json::String(mode)) if mode == "form" => {}
                Some(Json::String(mode)) => {
                    return Err(fail(format!(
                        "View mode '{}' is unsupported in a method-result window; only 'form' opens.",
                        shown(mode)
                    )));
                }
                Some(_) => return Err(fail("A window result 'view_mode' must be text.")),
            }
            optional_id(members, "view_id", "Window view_id")?
        }
    };
    let context = context(members.get("context"), model, lookup, false)?;
    Ok(wire::ActionResult::Window {
        name: name.to_owned(),
        model: model.to_owned(),
        res_id,
        views: vec![wire::ActionView::new(view, wire::ViewType::Form)],
        context,
    })
}

fn current_window(
    members: &Map<String, Json>,
    lookup: &dyn ModelLookup,
) -> Result<wire::ActionResult> {
    fn browser_numbers(value: &Json) -> Result<()> {
        match value {
            Json::Number(number)
                if number.as_f64().is_some_and(|number| {
                    number.fract() == 0.0 && number.abs() > 9_007_199_254_740_991.0
                }) =>
            {
                Err(fail(
                    "Current-window integer literal exceeds the browser's safe range.",
                ))
            }
            Json::Array(values) => values.iter().try_for_each(browser_numbers),
            Json::Object(values) => values.values().try_for_each(browser_numbers),
            _ => Ok(()),
        }
    }
    reject_members(
        members,
        &[
            "type",
            "name",
            "res_model",
            "target",
            "views",
            "domain",
            "context",
        ],
        "current window",
    )?;
    // The whole result's depth/bytes are already bounded by normalize. Refuse precision loss
    // before a filter/default crosses the JSON-to-JavaScript numeric boundary.
    for value in members.values() {
        browser_numbers(value)?;
    }
    let name = text(members, "name", MAX_TITLE_BYTES, "A current window")?;
    let model = text(members, "res_model", MAX_MODEL_BYTES, "A current window")?;
    if !lookup.has_model(model) {
        return Err(fail(format!("Unknown action model '{model}'.")));
    }
    let Some(Json::Array(entries)) = members.get("views") else {
        return Err(fail(
            "A current window requires list, optionally followed by form.",
        ));
    };
    if entries.is_empty() || entries.len() > 2 {
        return Err(fail(
            "A current window requires list, optionally followed by form.",
        ));
    }
    let mut views = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Json::Array(pair) = entry else {
            return Err(fail("A current-window view must be [id|false, type]."));
        };
        let [id, Json::String(kind)] = pair.as_slice() else {
            return Err(fail("A current-window view must be [id|false, type]."));
        };
        let view_type = match (index, kind.as_str()) {
            (0, "list") => wire::ViewType::List,
            (1, "form") => wire::ViewType::Form,
            _ => {
                return Err(fail(
                    "A current window requires list, optionally followed by form.",
                ));
            }
        };
        let id = match id {
            Json::Null | Json::Bool(false) => None,
            other => Some(positive_id(other, "Current-window view identity")?),
        };
        views.push(wire::ActionView::new(id, view_type));
    }
    let Some(domain @ Json::Array(_)) = members.get("domain") else {
        return Err(fail("A current-window domain must be a literal array."));
    };
    // Parse external syntax with the existing bounded domain grammar, then emit its canonical
    // literal form. This is admission only: ORM searches apply current caller authority later.
    let domain = Domain::from_json(domain)
        .map_err(Error::from)?
        .to_json()
        .map_err(Error::from)?;
    let Json::Array(domain) = domain else {
        return Err(Error::invariant("a canonical domain is not an array"));
    };
    Ok(wire::ActionResult::CurrentWindow {
        name: name.to_owned(),
        model: model.to_owned(),
        views,
        domain,
        context: context(members.get("context"), model, lookup, true)?,
    })
}

fn literal(value: &Json) -> Result<()> {
    match value {
        Json::String(text) if text.len() > MAX_TEXT_BYTES => Err(fail(format!(
            "A context literal exceeds the {MAX_TEXT_BYTES}-byte limit."
        ))),
        Json::Array(items) => items.iter().try_for_each(literal),
        Json::Object(entries) => entries.values().try_for_each(literal),
        _ => Ok(()),
    }
}

fn context(
    raw: Option<&Json>,
    model: &str,
    lookup: &dyn ModelLookup,
    current_window: bool,
) -> Result<BTreeMap<String, Json>> {
    let entries = match raw {
        None | Some(Json::Null) => return Ok(BTreeMap::new()),
        Some(Json::Object(entries)) => entries,
        Some(other) => {
            return Err(fail(format!(
                "A window context must be an object, not {}.",
                kind_name(other)
            )));
        }
    };
    if entries.len() > MAX_CONTEXT_KEYS {
        return Err(fail(format!(
            "A window context has more than {MAX_CONTEXT_KEYS} keys."
        )));
    }
    let mut result = BTreeMap::new();
    for (key, value) in entries {
        if current_window && key == "active_test" {
            if !value.is_boolean() {
                return Err(fail("Current-window active_test must be a boolean."));
            }
            result.insert(key.clone(), value.clone());
            continue;
        }
        if current_window && key == "create" {
            if !value.is_boolean() {
                return Err(fail("Current-window create must be a boolean."));
            }
            result.insert(key.clone(), value.clone());
            continue;
        }
        if current_window && model == "product.uom" {
            // Browser integer identities must remain exactly representable.
            let positive_id = |value: &Json| {
                value
                    .as_i64()
                    .is_some_and(|id| id > 0 && id <= 9_007_199_254_740_991)
            };
            let valid = match key.as_str() {
                "product_id" => {
                    if !positive_id(value) {
                        return Err(fail(
                            "Packaging product_id must be a positive safe integer.",
                        ));
                    }
                    true
                }
                "product_ids" => {
                    if !value
                        .as_array()
                        .is_some_and(|ids| ids.iter().all(positive_id))
                    {
                        return Err(fail(
                            "Packaging product_ids must be a list of positive safe integers.",
                        ));
                    }
                    true
                }
                "active_model" => {
                    if !matches!(
                        value.as_str(),
                        Some("product.template" | "product.product" | "uom.uom")
                    ) {
                        return Err(fail(
                            "Packaging active_model must name a product or unit model.",
                        ));
                    }
                    true
                }
                // These keys describe the originating environment and caller selection.
                // Native actions retain them; the browser keeps its own invocation metadata.
                "active_ids" | "allowed_company_ids" => {
                    if !value
                        .as_array()
                        .is_some_and(|ids| ids.iter().all(positive_id))
                    {
                        return Err(fail(format!(
                            "Packaging environment {key} must contain positive safe integers."
                        )));
                    }
                    continue;
                }
                "active_id" | "company_id" => {
                    if !positive_id(value) {
                        return Err(fail(format!(
                            "Packaging environment {key} must be a positive safe integer."
                        )));
                    }
                    continue;
                }
                "lang" | "tz" => {
                    if !value.is_string() {
                        return Err(fail(
                            "Packaging environment language and timezone must be strings.",
                        ));
                    }
                    literal(value)?;
                    continue;
                }
                "prefetch_fields" | "prefetch_langs" => {
                    if !value.is_boolean() {
                        return Err(fail(
                            "Packaging environment prefetch flags must be booleans.",
                        ));
                    }
                    continue;
                }
                // Other keys still pass through the ordinary closed-context validator below.
                _ => false,
            };
            if valid {
                result.insert(key.clone(), value.clone());
                continue;
            }
        }
        let Some(field) = key.strip_prefix("default_").filter(|name| !name.is_empty()) else {
            return Err(fail(format!(
                "Context key '{}' is not admitted: only default_<field> literals pass.",
                shown(key)
            )));
        };
        if !lookup.has_field(model, field) {
            return Err(fail(format!(
                "Context key '{}' names no field of '{model}'.",
                shown(key)
            )));
        }
        literal(value)?;
        result.insert(key.clone(), value.clone());
    }
    Ok(result)
}

fn notification(members: &Map<String, Json>) -> Result<wire::ActionResult> {
    reject_members(members, &["type", "tag", "params"], "notification")?;
    match members.get("tag") {
        Some(Json::String(tag)) if tag == NOTIFY_TAG => {}
        Some(Json::String(tag)) => {
            return Err(fail(format!(
                "Client action tag '{}' is not supported; only '{NOTIFY_TAG}' is.",
                shown(tag)
            )));
        }
        _ => return Err(fail("A client action result requires a text 'tag'.")),
    }
    let Some(Json::Object(params)) = members.get("params") else {
        return Err(fail("A notification requires an object 'params'."));
    };
    reject_members(
        params,
        &["title", "message", "type", "sticky", "next"],
        "notification params",
    )?;
    if params
        .get("sticky")
        .is_some_and(|sticky| sticky != &Json::Bool(false))
    {
        return Err(fail("A sticky notification is not supported."));
    }
    let message = text(params, "message", MAX_TEXT_BYTES, "A notification")?;
    let title = match params.get("title") {
        None | Some(Json::Null) => None,
        Some(Json::String(_)) => Some(text(params, "title", MAX_TITLE_BYTES, "A notification")?),
        Some(_) => return Err(fail("A notification 'title' must be text.")),
    };
    // Upstream defaults an absent level to info; that default is kept explicitly.
    let level = match params.get("type") {
        None => wire::NotificationLevel::Info,
        Some(Json::String(level)) => match level.as_str() {
            "info" => wire::NotificationLevel::Info,
            "success" => wire::NotificationLevel::Success,
            "warning" => wire::NotificationLevel::Warning,
            "danger" => wire::NotificationLevel::Danger,
            other => {
                return Err(fail(format!(
                    "Notification level '{}' is unsupported.",
                    shown(other)
                )));
            }
        },
        Some(_) => return Err(fail("A notification 'type' must be text.")),
    };
    let then_close = match params.get("next") {
        None | Some(Json::Null) => false,
        Some(Json::Object(next))
            if next.get("type") == Some(&Json::String(ACT_CLOSE.to_owned())) =>
        {
            reject_members(next, &["type"], "notification next")?;
            true
        }
        Some(_) => {
            return Err(fail(
                "A notification 'next' may only be an act_window_close action.",
            ));
        }
    };
    Ok(wire::ActionResult::Notification {
        title: title.map(str::to_owned),
        message: message.to_owned(),
        level,
        then_close,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_sdk::serde_json::json;

    struct Fixture;
    impl ModelLookup for Fixture {
        fn has_model(&self, model: &str) -> bool {
            matches!(model, "library.loan" | "product.uom")
        }
        fn has_field(&self, model: &str, field: &str) -> bool {
            model == "library.loan" && matches!(field, "book_id" | "borrower_id" | "due_date")
        }
    }

    fn run(value: Json) -> Result<wire::ActionResult> {
        normalize(&value, &Fixture)
    }

    fn message(value: Json) -> String {
        run(value).expect_err("must be refused").to_string()
    }

    fn window_json() -> Json {
        json!({"type": ACT_WINDOW, "name": "Lend", "res_model": "library.loan",
               "target": "new", "views": [[false, "form"]],
               "context": {"default_book_id": 7, "default_due_date": "2026-11-01"}})
    }

    #[test]
    fn branch_active_test_is_boolean_and_current_window_only() {
        for active in [true, false] {
            let value = json!({"type": ACT_WINDOW, "name":"Branches", "res_model":"library.loan", "target":"current", "views":[[false,"list"],[false,"form"]], "domain":[], "context":{"active_test":active}});
            let result = serde_json::to_value(run(value).unwrap()).unwrap();
            assert_eq!(result["context"]["active_test"], json!(active));
        }
        for invalid in [json!(null), json!(0), json!("false"), json!([])] {
            let value = json!({"type": ACT_WINDOW, "name":"Branches", "res_model":"library.loan", "target":"current", "views":[[false,"list"]], "domain":[], "context":{"active_test":invalid}});
            let error = run(value).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Current-window active_test must be a boolean."
            );
        }
        let mut modal = window_json();
        modal["context"] = json!({"active_test":false});
        let error = run(modal).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Context key 'active_test' is not admitted: only default_<field> literals pass."
        );
    }

    #[test]
    fn create_is_boolean_and_current_window_only() {
        for create in [true, false] {
            let value = json!({"type": ACT_WINDOW, "name":"Branches", "res_model":"library.loan", "target":"current", "views":[[false,"list"],[false,"form"]], "domain":[], "context":{"create":create, "default_book_id": 3}});
            let result = serde_json::to_value(run(value).unwrap()).unwrap();
            assert_eq!(result["context"]["create"], json!(create));
            assert_eq!(result["context"]["default_book_id"], json!(3));
        }
        for invalid in [json!(null), json!(0), json!("false"), json!([])] {
            let value = json!({"type": ACT_WINDOW, "name":"Branches", "res_model":"library.loan", "target":"current", "views":[[false,"list"]], "domain":[], "context":{"create":invalid}});
            let error = run(value).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(error.message(), "Current-window create must be a boolean.");
        }
        let mut modal = window_json();
        modal["context"] = json!({"create":false});
        let error = run(modal).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Context key 'create' is not admitted: only default_<field> literals pass."
        );
    }

    #[test]
    fn current_window_retains_literal_domain_and_optional_form() {
        let result = run(
            json!({"type": ACT_WINDOW, "name": "Related", "res_model": "library.loan",
            "target": "current", "views": [[false, "list"], [false, "form"]],
            "domain": [["book_id", "=", 7]], "context": {"default_book_id": 7}}),
        )
        .unwrap();
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["kind"], json!("current_window"));
        assert_eq!(value["domain"], json!([["book_id", "=", 7]]));
        assert_eq!(value["views"].as_array().unwrap().len(), 2);
        let empty = run(
            json!({"type":ACT_WINDOW,"name":"None","res_model":"library.loan",
            "target":"current","views":[[false,"list"]],"domain":[[0,"=",1]],"context":{}}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(empty).unwrap()["domain"],
            json!([[0, "=", 1]])
        );
    }

    #[test]
    fn current_window_refuses_unsupported_views_context_and_domain_shapes() {
        let base = json!({"type": ACT_WINDOW, "name":"Related", "res_model":"library.loan",
            "target":"current", "views":[[false,"list"]], "domain":[], "context":{}});
        for (key, value, expected) in [
            (
                "views",
                json!([[false, "form"]]),
                "A current window requires list, optionally followed by form.",
            ),
            (
                "views",
                json!([[false, "list"], [false, "list"]]),
                "A current window requires list, optionally followed by form.",
            ),
            (
                "domain",
                json!("[]"),
                "A current-window domain must be a literal array.",
            ),
            (
                "domain",
                json!([["book_id", "=", 9_007_199_254_740_992_u64]]),
                "Current-window integer literal exceeds the browser's safe range.",
            ),
            (
                "context",
                json!({"allowed_company_ids":[9]}),
                "Context key 'allowed_company_ids' is not admitted: only default_<field> literals pass.",
            ),
            (
                "context",
                json!({"search_view_ref":"addon.view"}),
                "Context key 'search_view_ref' is not admitted: only default_<field> literals pass.",
            ),
            (
                "context",
                json!({"searchpanel_default_priority":"high"}),
                "Context key 'searchpanel_default_priority' is not admitted: only default_<field> literals pass.",
            ),
            (
                "context",
                json!({"search_default_name":"filter"}),
                "Context key 'search_default_name' is not admitted: only default_<field> literals pass.",
            ),
            (
                "context",
                json!({"default_missing":1}),
                "Context key 'default_missing' names no field of 'library.loan'.",
            ),
        ] {
            let mut value_under_test = base.clone();
            value_under_test[key] = value;
            let error = run(value_under_test).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(error.to_string(), expected);
        }
        let mut malformed = base;
        malformed["domain"] = json!(["|", ["book_id", "=", 7]]);
        let error = run(malformed).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        // Compare the complete parser diagnostic; its grammar remains owned by erp-domain.
        let expected = Domain::from_json(&json!(["|", ["book_id", "=", 7]])).unwrap_err();
        assert_eq!(error.to_string(), Error::from(expected).to_string());
    }

    #[test]
    fn window_result_becomes_a_typed_form_request() {
        let wire::ActionResult::Window {
            name,
            model,
            res_id,
            views,
            context,
        } = run(window_json()).unwrap()
        else {
            panic!("window expected");
        };
        assert_eq!((name.as_str(), model.as_str()), ("Lend", "library.loan"));
        assert!(res_id.is_none());
        assert_eq!(views.len(), 1);
        assert_eq!(context["default_book_id"], json!(7));
        // Alternative spelling: view_id/view_mode plus an existing record.
        let mut other = window_json();
        let map = other.as_object_mut().unwrap();
        map.remove("views");
        map.insert("view_mode".into(), json!("form"));
        map.insert("view_id".into(), json!(12));
        map.insert("res_id".into(), json!(5));
        let wire::ActionResult::Window { res_id, .. } = run(other).unwrap() else {
            panic!("window expected");
        };
        assert_eq!(res_id.map(u64::from), Some(5));
        // Unknown model, wrong target, non-form view and mixed spellings are refused by name.
        let mut bad = window_json();
        bad["res_model"] = json!("nope.model");
        assert_eq!(message(bad), "Unknown action model 'nope.model'.");
        let mut bad = window_json();
        bad["target"] = json!("fullscreen");
        assert!(message(bad).starts_with("Window target 'fullscreen' is unsupported"));
        let mut bad = window_json();
        bad["views"] = json!([[false, "list"]]);
        assert!(message(bad).starts_with("View type 'list' is unsupported"));
        let mut bad = window_json();
        bad["view_id"] = json!(3);
        assert_eq!(
            message(bad),
            "A window result gives either 'views' or 'view_id'/'view_mode', not both."
        );
    }

    #[test]
    fn context_admits_only_bounded_default_literals_of_real_fields() {
        for (key, expected) in [
            (
                "active_id",
                "Context key 'active_id' is not admitted: only default_<field> literals pass.",
            ),
            (
                "lang",
                "Context key 'lang' is not admitted: only default_<field> literals pass.",
            ),
            (
                "allowed_company_ids",
                "Context key 'allowed_company_ids' is not admitted: only default_<field> literals pass.",
            ),
            (
                "default_",
                "Context key 'default_' is not admitted: only default_<field> literals pass.",
            ),
            (
                "default_ghost",
                "Context key 'default_ghost' names no field of 'library.loan'.",
            ),
        ] {
            let mut bad = window_json();
            bad["context"] = json!({ key: 1 });
            assert_eq!(message(bad), expected);
        }
        let mut bad = window_json();
        bad["context"] = json!([1]);
        assert_eq!(
            message(bad),
            "A window context must be an object, not array."
        );
        let mut bad = window_json();
        bad["context"] = json!({"default_book_id": "x".repeat(MAX_TEXT_BYTES + 1)});
        assert_eq!(
            message(bad),
            "A context literal exceeds the 4096-byte limit."
        );
        let mut many = Map::new();
        for index in 0..=MAX_CONTEXT_KEYS {
            many.insert(format!("default_book_id{index}"), json!(1));
        }
        let mut bad = window_json();
        bad["context"] = Json::Object(many);
        assert_eq!(message(bad), "A window context has more than 64 keys.");
    }

    #[test]
    fn close_result_is_exact() {
        assert!(matches!(
            run(json!({"type": ACT_CLOSE})).unwrap(),
            wire::ActionResult::Close {}
        ));
        assert_eq!(
            message(json!({"type": ACT_CLOSE, "infos": {}})),
            "Member 'infos' is not supported in a close result."
        );
    }

    #[test]
    fn notification_is_bounded_text_with_an_optional_close() {
        let ok = json!({"type": ACT_CLIENT, "tag": NOTIFY_TAG,
            "params": {"title": "Saved", "message": "Loan created", "type": "success",
                       "next": {"type": ACT_CLOSE}}});
        assert!(matches!(
            run(ok).unwrap(),
            wire::ActionResult::Notification {
                level: wire::NotificationLevel::Success,
                then_close: true,
                ..
            }
        ));
        let plain = json!({"type": ACT_CLIENT, "tag": NOTIFY_TAG, "params": {"message": "Hi"}});
        assert!(matches!(
            run(plain).unwrap(),
            wire::ActionResult::Notification {
                title: None,
                level: wire::NotificationLevel::Info,
                then_close: false,
                ..
            }
        ));
        for (params, expected) in [
            (
                json!({"message": "m", "sticky": true}),
                "A sticky notification is not supported.",
            ),
            (
                json!({"message": "m", "links": []}),
                "Member 'links' is not supported in a notification params result.",
            ),
            (
                json!({"message": "m", "className": "x"}),
                "Member 'className' is not supported in a notification params result.",
            ),
            (
                json!({"message": "m", "next": {"type": ACT_WINDOW}}),
                "A notification 'next' may only be an act_window_close action.",
            ),
            (
                json!({"message": "m", "type": "loud"}),
                "Notification level 'loud' is unsupported.",
            ),
            (
                json!({"title": "t"}),
                "A notification requires a nonempty text 'message'.",
            ),
            (
                json!({"message": "m".repeat(MAX_TEXT_BYTES + 1)}),
                "A notification 'message' exceeds the 4096-byte limit.",
            ),
        ] {
            assert_eq!(
                message(json!({"type": ACT_CLIENT, "tag": NOTIFY_TAG, "params": params})),
                expected
            );
        }
        assert_eq!(
            message(json!({"type": ACT_CLIENT, "tag": "reload", "params": {}})),
            "Client action tag 'reload' is not supported; only 'display_notification' is."
        );
    }

    #[test]
    fn scalars_mean_no_action_and_arrays_are_refused() {
        for ok in [
            json!(null),
            json!(true),
            json!(false),
            json!(5),
            json!(-1.5),
            json!("done"),
            json!(""),
        ] {
            assert!(matches!(run(ok).unwrap(), wire::ActionResult::None {}));
        }
        assert_eq!(
            message(json!([])),
            "A method result of kind array is not an action result; only a scalar or an action dictionary is admitted."
        );
    }

    #[test]
    fn unknown_malformed_oversized_and_deep_results_are_refused() {
        assert_eq!(
            message(json!({})),
            "An action result requires a text 'type'."
        );
        assert_eq!(
            message(json!({"type": "ir.actions.server"})),
            "Action type 'ir.actions.server' is not supported as a method result."
        );
        assert_eq!(
            message(
                json!({"type": ACT_WINDOW, "name": "x", "res_model": "library.loan",
                           "target": "new", "domain": []})
            ),
            "Member 'domain' is not supported in a window result."
        );
        let mut bad = window_json();
        bad["res_id"] = json!(0);
        assert_eq!(
            message(bad),
            "Window res_id must be a positive JSON-safe integer."
        );
        let mut bad = window_json();
        bad["res_id"] = json!(9_007_199_254_740_992_u64);
        assert_eq!(
            message(bad),
            "Window res_id must be a positive JSON-safe integer."
        );
        let mut bad = window_json();
        bad["name"] = json!("");
        assert_eq!(
            message(bad),
            "A window result requires a nonempty text 'name'."
        );
        let mut bad = window_json();
        bad["res_model"] = json!("Bad Model!");
        assert_eq!(message(bad), "Model name 'Bad Model!' is invalid.");
        let huge = json!({"type": ACT_CLIENT, "tag": NOTIFY_TAG,
                          "params": {"message": "a", "pad": "b".repeat(MAX_RESULT_BYTES)}});
        assert!(message(huge).starts_with("Method result is "));
        let mut deep = json!(1);
        for _ in 0..=MAX_RESULT_DEPTH {
            deep = json!([deep]);
        }
        assert_eq!(message(deep), "Method result nests deeper than 6 levels.");
    }

    #[test]
    fn rpc_values_convert_with_a_depth_bound_and_refuse_unjsonable_kinds() {
        assert_eq!(to_json(&Value::False, 1).unwrap(), Json::Null);
        assert_eq!(to_json(&Value::Int(3), 1).unwrap(), json!(3));
        assert!(to_json(&Value::Float(f64::NAN), 1).is_err());
        let mut nested = Value::Int(1);
        for _ in 0..=MAX_RESULT_DEPTH {
            nested = Value::List(vec![nested].into());
        }
        assert_eq!(
            to_json(&nested, 1).unwrap_err().to_string(),
            "Method result nests deeper than 6 levels."
        );
    }
    #[test]
    fn packaging_product_context_is_typed_and_scoped() {
        let base = json!({"type": ACT_WINDOW, "name":"Packaging", "res_model":"product.uom", "target":"current", "views":[[false,"list"]], "domain":[], "context":{"product_id":7,"product_ids":[7,8],"active_model":"uom.uom"}});
        let result = serde_json::to_value(run(base.clone()).unwrap()).unwrap();
        assert_eq!(result["context"]["product_ids"], json!([7, 8]));
        for (key, value, message) in [
            (
                "product_id",
                json!(0),
                "Packaging product_id must be a positive safe integer.",
            ),
            (
                "product_ids",
                json!([7, "8"]),
                "Packaging product_ids must be a list of positive safe integers.",
            ),
            (
                "active_model",
                json!("res.users"),
                "Packaging active_model must name a product or unit model.",
            ),
        ] {
            let mut bad = base.clone();
            bad["context"][key] = value;
            let err = run(bad).unwrap_err();
            assert_eq!(err.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(err.message(), message);
        }
        let mut other = base;
        other["res_model"] = json!("library.loan");
        other["context"] = json!({"product_id":7});
        let err = run(other).unwrap_err();
        assert_eq!(err.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "Context key 'product_id' is not admitted: only default_<field> literals pass."
        );
    }

    #[test]
    fn packaging_environment_context_does_not_travel_to_the_browser() {
        let value = json!({"type": ACT_WINDOW, "name":"Packaging", "res_model":"product.uom", "target":"current", "views":[[false,"list"]], "domain":[],
            "context":{"product_ids":[7],"active_id":7,"active_ids":[7],"allowed_company_ids":[1],"company_id":1,"lang":"en_US","tz":"UTC","prefetch_fields":false}});
        let result = serde_json::to_value(run(value.clone()).unwrap()).unwrap();
        assert_eq!(result["context"], json!({"product_ids":[7]}));
        for (key, invalid, message) in [
            (
                "active_id",
                json!(0),
                "Packaging environment active_id must be a positive safe integer.",
            ),
            (
                "active_ids",
                json!(["7"]),
                "Packaging environment active_ids must contain positive safe integers.",
            ),
        ] {
            let mut bad = value.clone();
            bad["context"][key] = invalid;
            let err = run(bad).unwrap_err();
            assert_eq!(err.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(err.message(), message);
        }
    }
}
