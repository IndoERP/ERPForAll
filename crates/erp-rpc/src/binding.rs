//! Body binding (design note §6, §7): the JSON-2 body's `ids`, `context` and keyword arguments,
//! checked against the method's parameter table before any statement runs.

use std::collections::BTreeSet;
use std::sync::Arc;

use erp_fields::{Value, ValueMap};
use erp_orm::Context;
use erp_orm::rpc::{ArgError, BindError, ParamSpec, Receiver, RpcArgs};

use crate::error::{ProtocolError, RpcFailure};
use crate::json::Json;
use crate::route::Route;

/// Most context keys a call may send (design note §7.2).
pub const MAX_CONTEXT_KEYS: usize = 256;
/// Longest context key, in bytes.
pub const MAX_CONTEXT_KEY_BYTES: usize = 256;

/// The prefix of the data loader's context keys (`install_mode`, `install_module`,
/// `install_filename`, `install_demo` and the load state, `erp_data::load::load_state`): every
/// key under it is server-only, so a key the loader adds later is refused without a list to keep
/// in step (review A4-R1).
pub const SERVER_ONLY_CONTEXT_PREFIX: &str = "install_";

/// Context keys only the server sets (design note §7.2): the loader's and `force_delete`. A key whose presence
/// relaxes a check, an audit, tracking or a constraint belongs here: the load-state keys
/// (`erp_data::load::load_state`) make `ir.ui.view` skip sibling views in its write check
/// (review A3-S1).
pub const SERVER_ONLY_CONTEXT_KEYS: [&str; 15] = server_only_keys();

/// The loader's keys, then the context keys that switch tracking off
/// ([`erp_orm::TRACKING_DISABLE_KEYS`], the one list the ORM reads: a caller that could send them
/// would silence the change log of its own writes).
const fn server_only_keys() -> [&'static str; 15] {
    const LOADER: [&str; 12] = [
        "install_mode",
        "module",
        "install_filename",
        "noupdate",
        "max_number_opened_groups",
        "install_module",
        "install_pending_modules",
        "install_loaded_names",
        "install_track_batch",
        "install_batch_names",
        "install_batch_positions",
        "install_batch_written",
    ];
    let mut out = [""; 15];
    let mut i = 0;
    while i < LOADER.len() {
        out[i] = LOADER[i];
        i += 1;
    }
    let mut j = 0;
    while j < erp_orm::TRACKING_DISABLE_KEYS.len() {
        out[LOADER.len() + j] = erp_orm::TRACKING_DISABLE_KEYS[j];
        j += 1;
    }
    // `force_delete` skips the deletion refusals of the ORM (restrict and fallback references of
    // company maps, the view cascade): only server code sets it
    out[LOADER.len() + j] = "force_delete";
    out
}

/// Validated sanitation/report contract, independent from field policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HtmlValidation {
    #[default]
    LegacyStrict,
    Clean,
    Strict,
}
impl HtmlValidation {
    pub(crate) fn reports(self) -> bool {
        self != Self::LegacyStrict
    }
    pub(crate) fn strict(self) -> bool {
        self != Self::Clean
    }
}

/// The body split into its parts, as sent (for the idempotency digest) and as bound.
#[derive(Debug, Clone)]
pub(crate) struct Bound {
    /// `ids` as sent (`[]` when absent).
    pub(crate) ids_json: Json,
    /// `context` as sent (`{}` when absent).
    pub(crate) context_json: Json,
    /// The keyword arguments as sent.
    pub(crate) kwargs_json: Json,
    pub(crate) ids: Vec<i64>,
    pub(crate) context: CallContext,
    pub(crate) args: RpcArgs,
    pub(crate) html_validation: HtmlValidation,
}

/// The validated call context and the typed keys the service checks against the database
/// and the principal.
#[derive(Debug, Clone, Default)]
pub(crate) struct CallContext {
    pub(crate) context: Context,
    pub(crate) lang: Option<String>,
    pub(crate) allowed_company_ids: Option<Vec<i64>>,
}

/// Keys the transport carries into an absent or partial context (`call_kw`: `website_id`,
/// `host_id`; JSON-2: none; design note §7.1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CarriedContext {
    keys: Vec<(String, Value)>,
}

impl CarriedContext {
    /// Nothing carried (JSON-2).
    pub fn none() -> Self {
        Self::default()
    }

    /// `call_kw`'s carried keys.
    ///
    /// # Errors
    ///
    /// `Programming` for a key other than `website_id` and `host_id`, or a value that is not an
    /// integer, a string, a boolean or `False`.
    pub fn call_kw(keys: impl IntoIterator<Item = (String, Value)>) -> erp_fields::Result<Self> {
        let keys: Vec<(String, Value)> = keys.into_iter().collect();
        for (k, v) in &keys {
            if k != "website_id" && k != "host_id" {
                return Err(erp_fields::Error::programming(format!(
                    "call_kw carries only website_id and host_id, not '{k}'"
                )));
            }
            if carried_json(v).is_none() {
                return Err(erp_fields::Error::programming(format!(
                    "call_kw's carried '{k}' must be an integer, a string, a boolean or False"
                )));
            }
        }
        Ok(Self { keys })
    }
}

/// A carried value as JSON (the request digest covers the context the call runs with).
fn carried_json(value: &Value) -> Option<Json> {
    match value {
        Value::False => Some(Json::Null),
        Value::Bool(b) => Some(Json::Bool(*b)),
        Value::Int(i) => Some(Json::Int(*i)),
        Value::Str(s) => Some(Json::Str((**s).into())),
        _ => None,
    }
}

fn unprocessable(message: impl Into<String>) -> RpcFailure {
    RpcFailure::Protocol(ProtocolError::UnprocessableEntity(message.into()))
}

/// Split and check the body of a call to an exposed method.
pub(crate) fn bind_body(
    route: &Route,
    body: Json,
    receiver: Receiver,
    params: &'static [ParamSpec],
    carried: &CarriedContext,
) -> Result<Bound, RpcFailure> {
    let Json::Object(members) = body else {
        // the caller turned `null` and an empty body into `{}` and refused other non-objects
        return Err(RpcFailure::Domain(erp_fields::Error::programming(
            "bind_body: the body is not an object",
        )));
    };
    let mut html_validation = HtmlValidation::LegacyStrict;
    let mut control_json = None;
    let mut ids_json: Option<Json> = None;
    let mut context_json: Option<Json> = None;
    let mut kwargs: Vec<(Box<str>, Json)> = Vec::new();
    for (key, value) in members {
        match &*key {
            // the route's own parameters: the path wins
            "__model__" | "__method__" => {}
            "ids" => ids_json = Some(value),
            "context" => context_json = Some(value),
            "html_validation" => {
                html_validation = match &value {
                    Json::Str(mode) if &**mode == "clean" => HtmlValidation::Clean,
                    Json::Str(mode) if &**mode == "strict" => HtmlValidation::Strict,
                    _ => return Err(unprocessable("html_validation must be 'clean' or 'strict'")),
                };
                control_json = Some(value);
            }
            _ => kwargs.push((key, value)),
        }
    }
    let ids_json = ids_json.unwrap_or(Json::Array(Vec::new()));
    if receiver == Receiver::Model && truthy(&ids_json) {
        return Err(unprocessable(format!(
            "cannot call {}.{} with ids",
            route.model(),
            route.method()
        )));
    }
    let ids = bind_ids(&ids_json)?;
    // Python's `signature.bind` reports a missing argument before an unexpected one
    if let Some(missing) = params
        .iter()
        .find(|p| p.required() && !kwargs.iter().any(|(k, _)| **k == *p.name()))
    {
        return Err(unprocessable(format!(
            "missing a required argument: '{}'",
            missing.name()
        )));
    }
    if let Some((unexpected, _)) = kwargs
        .iter()
        .find(|(k, _)| !params.iter().any(|p| p.name() == &**k))
    {
        return Err(unprocessable(format!(
            "got an unexpected keyword argument '{unexpected}'"
        )));
    }
    let mut values = ValueMap::with_capacity(kwargs.len());
    for (key, value) in &kwargs {
        let converted = to_value(value).map_err(|text| {
            unprocessable(format!(
                "argument '{key}' of {}: integer {text} is outside the 64-bit range",
                route.entry()
            ))
        })?;
        values.insert(key.to_string(), converted);
    }
    let context_json = match context_json {
        None | Some(Json::Null) => Json::Object(Vec::new()),
        Some(object @ Json::Object(_)) => object,
        Some(other) => {
            return Err(unprocessable(format!(
                "context must be an object, got {}",
                other.type_name()
            )));
        }
    };
    let context = bind_context(&context_json, carried)?;
    // the digest of an idempotent call covers the carried keys the call runs with (they change
    // what it does), not only what the body sent
    let context_json = match context_json {
        Json::Object(mut members) => {
            for (key, value) in &carried.keys {
                if !members.iter().any(|(k, _)| **k == **key) {
                    let json = carried_json(value).ok_or_else(|| {
                        RpcFailure::Domain(erp_fields::Error::programming(format!(
                            "carried context key '{key}' has a value JSON cannot hold"
                        )))
                    })?;
                    members.push((key.as_str().into(), json));
                }
            }
            Json::Object(members)
        }
        other => other,
    };
    // Include the explicit control in request identity, but never in model arguments.
    // Absence keeps the legacy digest so an existing committed request remains recoverable.
    if let Some(value) = control_json {
        kwargs.push(("html_validation".into(), value));
    }
    Ok(Bound {
        html_validation,
        ids_json,
        context_json,
        kwargs_json: Json::Object(kwargs),
        ids,
        context,
        args: RpcArgs::new(values),
    })
}

fn truthy(json: &Json) -> bool {
    match json {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Int(i) => *i != 0,
        Json::BigInt(_) => true,
        Json::Float(f) => *f != 0.0,
        Json::Str(s) => !s.is_empty(),
        Json::Array(a) => !a.is_empty(),
        Json::Object(o) => !o.is_empty(),
    }
}

/// `ids`: a positive integer (one record) or a list of them, order and duplicates kept, like
/// `browse`.
fn bind_ids(json: &Json) -> Result<Vec<i64>, RpcFailure> {
    let refuse = |got: &str| {
        unprocessable(format!(
            "ids must be a positive integer or a list of positive integers, got {got}"
        ))
    };
    match json {
        Json::Int(i) if *i > 0 => Ok(vec![*i]),
        Json::Array(items) => items
            .iter()
            .map(|item| match item {
                Json::Int(i) if *i > 0 => Ok(*i),
                Json::Int(i) => Err(refuse(&format!("the integer {i} in the list"))),
                other => Err(refuse(&format!("{} in the list", other.type_name()))),
            })
            .collect(),
        Json::Int(i) => Err(refuse(&format!("the integer {i}"))),
        other => Err(refuse(other.type_name())),
    }
}

/// A JSON value as a dynamic value: `null` is `False`, `false` is `Bool(false)`. An integer
/// outside `i64` is refused with its text.
pub(crate) fn to_value(json: &Json) -> Result<Value, Box<str>> {
    Ok(match json {
        Json::Null => Value::False,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => Value::Int(*i),
        Json::BigInt(text) => return Err(text.clone()),
        Json::Float(f) => Value::Float(*f),
        Json::Str(s) => Value::str(s),
        Json::Array(items) => {
            let values: Result<Vec<Value>, Box<str>> = items.iter().map(to_value).collect();
            Value::List(values?.into())
        }
        Json::Object(members) => {
            let mut map = ValueMap::with_capacity(members.len());
            for (k, v) in members {
                map.insert(k.to_string(), to_value(v)?);
            }
            Value::Map(Arc::new(map))
        }
    })
}

/// The call context (design note §7): the body's object replaces the context; carried keys fill
/// in only what it lacks. Typed keys are checked here for shape; `lang` and
/// `allowed_company_ids` are checked against the database and the principal by the service.
fn bind_context(json: &Json, carried: &CarriedContext) -> Result<CallContext, RpcFailure> {
    let Json::Object(members) = json else {
        return Err(unprocessable(format!(
            "context must be an object, got {}",
            json.type_name()
        )));
    };
    if members.len() > MAX_CONTEXT_KEYS {
        return Err(unprocessable(format!(
            "the context has {} keys; at most {MAX_CONTEXT_KEYS} are accepted",
            members.len()
        )));
    }
    let mut out = CallContext::default();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (key, value) in members {
        if key.len() > MAX_CONTEXT_KEY_BYTES {
            let shown: String = key.chars().take(32).collect();
            return Err(unprocessable(format!(
                "context key '{shown}…' is longer than {MAX_CONTEXT_KEY_BYTES} bytes"
            )));
        }
        if SERVER_ONLY_CONTEXT_KEYS.contains(&&**key)
            || key.starts_with(SERVER_ONLY_CONTEXT_PREFIX)
            || key.starts_with("__")
            || &**key == "html_validation"
        {
            return Err(unprocessable(format!(
                "context key '{key}' cannot be set by the caller"
            )));
        }
        seen.insert(key);
        let shape = |expected: &str| {
            unprocessable(format!(
                "context key '{key}': expected {expected}, got {}",
                value.type_name()
            ))
        };
        let dynamic = match &**key {
            "lang" => match value {
                Json::Str(s) => {
                    out.lang = Some(s.to_string());
                    Value::str(s)
                }
                Json::Null | Json::Bool(false) => Value::False,
                _ => return Err(shape("a language code")),
            },
            "tz" => match value {
                Json::Str(s) => {
                    if erp_fields::DateTime::now().offset_seconds(s).is_none() {
                        return Err(RpcFailure::Domain(erp_fields::Error::validation(format!(
                            "Invalid timezone: {s}"
                        ))));
                    }
                    Value::str(s)
                }
                Json::Null | Json::Bool(false) => Value::False,
                _ => return Err(shape("a timezone name")),
            },
            "active_test" | "bin_size" | "prefetch_fields" => match value {
                Json::Bool(b) => Value::Bool(*b),
                // lossless integers
                Json::Int(0) => Value::Bool(false),
                Json::Int(1) => Value::Bool(true),
                _ => return Err(shape("a boolean")),
            },
            "allowed_company_ids" => match value {
                // `None`/`False` behave like an absent key
                Json::Null | Json::Bool(false) => Value::False,
                Json::Array(items) => {
                    let ids: Option<Vec<i64>> = items
                        .iter()
                        .map(|i| match i {
                            Json::Int(n) if *n > 0 => Some(*n),
                            _ => None,
                        })
                        .collect();
                    let Some(ids) = ids else {
                        return Err(shape("a list of positive integers"));
                    };
                    let value = Value::list(ids.iter().map(|i| Value::Int(*i)));
                    out.allowed_company_ids = Some(ids);
                    value
                }
                _ => return Err(shape("a list of positive integers")),
            },
            _ => to_value(value).map_err(|text| {
                unprocessable(format!(
                    "context key '{key}': integer {text} is outside the 64-bit range"
                ))
            })?,
        };
        out.context.set(key.to_string(), dynamic);
    }
    for (key, value) in &carried.keys {
        if !seen.contains(key.as_str()) {
            out.context.set(key.clone(), value.clone());
        }
    }
    Ok(out)
}

/// A binder error as an RPC failure (design note §6.3).
pub(crate) fn bind_failure(route: &Route, error: BindError) -> RpcFailure {
    match error {
        BindError::Argument {
            name,
            expected,
            error,
        } => match error {
            ArgError::Missing => unprocessable(format!("missing a required argument: '{name}'")),
            ArgError::Mismatch { got } => unprocessable(format!(
                "argument '{name}' of {}: expected {expected}, got {got}",
                route.entry()
            )),
            ArgError::OutOfRange { got } => unprocessable(format!(
                "argument '{name}' of {}: expected {expected}, got {got}",
                route.entry()
            )),
            ArgError::Invalid(e) => RpcFailure::Domain(
                erp_fields::Error::new(
                    e.kind(),
                    format!("argument '{name}' of {}: {}", route.entry(), e.message()),
                )
                .with_source(e),
            ),
            other => RpcFailure::Domain(erp_fields::Error::programming(format!(
                "argument '{name}' of {}: {other:?}",
                route.entry()
            ))),
        },
        BindError::Call(e) => RpcFailure::Domain(e),
        other => RpcFailure::Domain(erp_fields::Error::programming(other.to_string())),
    }
}

#[cfg(test)]
mod tests;
