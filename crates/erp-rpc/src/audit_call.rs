//! The call summary of the audit trail (stage B, design `m3-audit` §5.5): the redacted
//! canonical projection of a call and its SHA-256 digest.
//!
//! The digest is over `{"model":…,"method":…,"ids":[…],"args":<redacted>}` written as RFC 8785
//! canonical JSON (`erp_fields::json::canonical`), with [`REDACTION_VERSION`] 2. It is **not** the
//! idempotency request digest and never reuses it: the redacted projection is built and hashed
//! here, and the raw arguments are not copied past the call.
//!
//! Redaction rules (version 2; version 1 kept every value of a plain field, which put the cleartext
//! of a password under an unsalted hash):
//!
//! - a method that takes record values (the keyword arguments `vals` and `vals_list`, as `create`,
//!   `write` and `web_save` do) keeps each value unless its field is unknown to the registry
//!   (`unknown`), marked `secret` on its declaration (`secret`: passwords, whatever their value
//!   or type), a binary or image field (`binary`) or a field restricted to a group with `.` in
//!   its `groups` attribute (`restricted`); x2many create and update commands recurse into the
//!   comodel's values (so a `secret` field of a nested `[0, 0, {..}]` is redacted by the
//!   comodel's own declaration), to at most [`MAX_DEPTH`] levels (`depth` beyond);
//! - every other keyword argument keeps only its **name**; its value becomes
//!   `{"$redacted":"undeclared"}` (decision L6, until method schemas declare safe arguments).

use erp_fields::json::{Json, canonical};
use erp_orm::audit::ArgsDigest;
use erp_registry::{ModelSpec, Registry};
use sha2::{Digest, Sha256};

/// The version of the redaction and canonical rules (`redaction_version` column).
pub(crate) const REDACTION_VERSION: u16 = 2;
/// Deepest nesting of x2many values followed.
const MAX_DEPTH: usize = 16;

fn redacted(reason: &str) -> Json {
    Json::Object(vec![("$redacted".into(), Json::Str(reason.into()))])
}

/// The redacted projection of a call: the digest input before it is hashed.
pub(crate) fn projection(
    registry: &Registry,
    model: &str,
    method: &str,
    ids: &[i64],
    kwargs: &Json,
) -> Json {
    let spec = registry.model(model);
    let args = match kwargs {
        Json::Object(members) => Json::Object(
            members
                .iter()
                .map(|(name, value)| {
                    let kept = match name.as_ref() {
                        "vals" => values(registry, spec, value, 0),
                        "vals_list" => match value {
                            Json::Array(items) => Json::Array(
                                items
                                    .iter()
                                    .map(|item| values(registry, spec, item, 0))
                                    .collect(),
                            ),
                            _ => redacted("undeclared"),
                        },
                        _ => redacted("undeclared"),
                    };
                    (name.clone(), kept)
                })
                .collect(),
        ),
        // the binder has refused anything but an object; a defect is still hashed, never skipped
        _ => redacted("undeclared"),
    };
    Json::Object(vec![
        ("model".into(), Json::Str(model.into())),
        ("method".into(), Json::Str(method.into())),
        (
            "ids".into(),
            Json::Array(ids.iter().map(|id| Json::Int(*id)).collect()),
        ),
        ("args".into(), args),
    ])
}

/// The digest of the redacted projection of a call.
pub(crate) fn args_digest(
    registry: &Registry,
    model: &str,
    method: &str,
    ids: &[i64],
    kwargs: &Json,
) -> ArgsDigest {
    let input = projection(registry, model, method, ids, kwargs);
    let digest: [u8; 32] = Sha256::digest(canonical(&input)).into();
    ArgsDigest::new(digest, REDACTION_VERSION)
}

/// One record's values on `spec`.
fn values(registry: &Registry, spec: Option<&ModelSpec>, value: &Json, depth: usize) -> Json {
    let Json::Object(members) = value else {
        return redacted("undeclared");
    };
    if depth > MAX_DEPTH {
        return redacted("depth");
    }
    Json::Object(
        members
            .iter()
            .map(|(field, item)| {
                let kept = match spec.and_then(|s| s.field(field)) {
                    None => redacted("unknown"),
                    Some(f) if f.secret => redacted("secret"),
                    Some(f) if matches!(f.ty.type_name(), "binary" | "image") => redacted("binary"),
                    Some(f) if f.groups.is_some_and(|g| g.contains('.')) => redacted("restricted"),
                    Some(f) => match (f.ty.type_name(), f.ty.comodel()) {
                        ("one2many" | "many2many", Some(comodel)) => {
                            x2many_value(registry, registry.model(comodel), item, depth)
                        }
                        // a many2one is an id: any other shape is not parsed
                        ("many2one", _) if matches!(item, Json::Array(_) | Json::Object(_)) => {
                            redacted("unparsed")
                        }
                        _ => item.clone(),
                    },
                };
                (field.clone(), kept)
            })
            .collect(),
    )
}

/// The value of an x2many field, in every shape the ORM accepts (`Records::commands_from_value`):
/// `False` and `null` (clear), a list of ids, a list of value maps (each a create) and a list of
/// command triples. Values of a create or update are redacted with the comodel's declarations.
/// Anything else is replaced as a whole by `{"$redacted":"unparsed"}`: a shape this function does
/// not know is never passed through.
fn x2many_value(
    registry: &Registry,
    comodel: Option<&ModelSpec>,
    value: &Json,
    depth: usize,
) -> Json {
    match value {
        Json::Null | Json::Bool(false) => value.clone(),
        Json::Array(items) => Json::Array(
            items
                .iter()
                .map(|item| x2many_element(registry, comodel, item, depth))
                .collect(),
        ),
        _ => redacted("unparsed"),
    }
}

/// One element of an x2many list: an id, a map of values to create, or a command triple.
fn x2many_element(
    registry: &Registry,
    comodel: Option<&ModelSpec>,
    element: &Json,
    depth: usize,
) -> Json {
    match element {
        Json::Int(_) => element.clone(),
        Json::Object(_) => values(registry, comodel, element, depth + 1),
        Json::Array(parts) => match parts.as_slice() {
            // create and update carry values; a missing, false or zero value map carries none
            [Json::Int(op @ (0 | 1)), id @ Json::Int(_), vals] => match vals {
                Json::Object(_) => Json::Array(vec![
                    Json::Int(*op),
                    id.clone(),
                    values(registry, comodel, vals, depth + 1),
                ]),
                Json::Null | Json::Bool(false) | Json::Int(0) => element.clone(),
                _ => redacted("unparsed"),
            },
            // delete, unlink, link and clear name an id; the third item is ignored by the ORM, so
            // only a scalar is kept
            [
                Json::Int(2..=5),
                Json::Int(_) | Json::Null | Json::Bool(false),
                Json::Int(_) | Json::Null | Json::Bool(false),
            ] => element.clone(),
            [
                Json::Int(2..=5),
                Json::Int(_) | Json::Null | Json::Bool(false),
            ] => element.clone(),
            // set: a list of ids
            [
                Json::Int(6),
                Json::Int(_) | Json::Null | Json::Bool(false),
                Json::Array(ids),
            ] if ids.iter().all(|id| matches!(id, Json::Int(_))) => element.clone(),
            _ => redacted("unparsed"),
        },
        _ => redacted("unparsed"),
    }
}

#[cfg(test)]
mod tests;
