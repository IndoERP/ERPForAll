// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{integrity, invalid};
use erp_fields::{Error, Result, Uuid};
use serde_json::{Map, Value, json};

pub(super) const FIELDS: [&str; 8] = [
    "entry_id",
    "create_date",
    "entry_kind",
    "outcome",
    "model",
    "method",
    "effect",
    "target_ids",
];
pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_CANDIDATES: usize = 4096;

pub(super) fn object<'a>(
    v: &'a Value,
    allowed: &[&str],
    message: &str,
) -> Result<&'a Map<String, Value>> {
    let map = v.as_object().ok_or_else(|| invalid(message))?;
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(message));
    }
    Ok(map)
}
fn text<'a>(m: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    m.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing or invalid query value"))
}
pub(super) fn timestamp(s: &str) -> Result<jiff::Timestamp> {
    if s.len() > 32 || !s.ends_with('Z') {
        return Err(invalid("invalid UTC timestamp"));
    }
    let t = s
        .parse::<jiff::Timestamp>()
        .map_err(|_| invalid("invalid UTC timestamp"))?;
    if t.subsec_nanosecond() % 1000 != 0 {
        return Err(invalid("invalid UTC timestamp"));
    }
    Ok(t)
}
pub(super) fn fields(v: Option<&Value>) -> Result<Vec<String>> {
    let a = v
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("fields must be a nonempty array of at most 8 names"))?;
    if a.is_empty() || a.len() > 8 {
        return Err(invalid(
            "fields must be a nonempty array of at most 8 names",
        ));
    }
    let mut out = Vec::new();
    for v in a {
        let f = v
            .as_str()
            .ok_or_else(|| invalid("unsupported projection field"))?;
        if !FIELDS.contains(&f) {
            return Err(invalid("unsupported projection field"));
        }
        if !out.iter().any(|s| s == f) {
            out.push(f.to_owned());
        }
    }
    // Canonical field order gives semantically identical queries one cursor binding.
    out.sort();
    Ok(out)
}

/// Composite audit identity; neither component grants access.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryKey {
    pub(super) id: Uuid,
    pub(super) time: jiff::Timestamp,
}
impl EntryKey {
    /// Parse exactly entry_id and a microsecond-precision UTC create_date.
    /// # Errors
    /// Validation for a malformed key; rejected input is never echoed.
    pub fn parse(v: &Value) -> Result<Self> {
        let parse = || {
            let m = object(
                v,
                &["entry_id", "create_date"],
                "invalid composite entry key",
            )?;
            Ok(Self {
                id: Uuid::parse(text(m, "entry_id")?)?,
                time: timestamp(text(m, "create_date")?)?,
            })
        };
        parse().map_err(|_: Error| invalid("invalid composite entry key"))
    }
    pub(super) fn stored(id: &str, time: &str) -> Result<Self> {
        Self::parse(&json!({"entry_id":id,"create_date":time})).map_err(|_| integrity())
    }
    /// Public components only, suitable for a later dedicated RPC adapter.
    pub fn to_json(&self) -> Value {
        json!({"entry_id":self.id.to_string(),"create_date":self.time.to_string()})
    }
}

/// Validated closed audit query. Parsing allocates only bounded strings/field names.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct AuditQuery {
    pub(super) model: String,
    pub(super) from: jiff::Timestamp,
    pub(super) until: jiff::Timestamp,
    pub(super) fields: Vec<String>,
    pub(super) limit: usize,
    pub(super) kind: Option<String>,
    pub(super) outcome: Option<String>,
    pub(super) continuation: Option<String>,
}
impl AuditQuery {
    /// Parse the query; unknown keys and explicit wrong types refuse.
    /// # Errors
    /// Validation for an unsupported field/filter, malformed interval or safety-bound input.
    pub fn parse(v: &Value) -> Result<Self> {
        let m = object(
            v,
            &[
                "model",
                "from",
                "until",
                "fields",
                "limit",
                "entry_kind",
                "outcome",
                "continuation",
            ],
            "unknown query key",
        )?;
        let model = text(m, "model")?;
        if model.is_empty()
            || model.len() > 256
            || !model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
        {
            return Err(invalid("invalid model name"));
        }
        let from = timestamp(text(m, "from")?)?;
        let until = timestamp(text(m, "until")?)?;
        let span = until
            .as_microsecond()
            .checked_sub(from.as_microsecond())
            .ok_or_else(|| invalid("interval must be positive and at most 31 days"))?;
        if !(1..=31 * 86400 * 1_000_000_i64).contains(&span) {
            return Err(invalid("interval must be positive and at most 31 days"));
        }
        let limit = match m.get("limit") {
            None => 100,
            Some(v) => {
                let n = v
                    .as_u64()
                    .filter(|n| (1..=1000).contains(n))
                    .ok_or_else(|| invalid("limit must be an integer in 1..=1000"))?;
                usize::try_from(n).map_err(|_| invalid("limit must be an integer in 1..=1000"))?
            }
        };
        let select = |key: &str, allowed: &[&str]| -> Result<Option<String>> {
            match m.get(key) {
                None => Ok(None),
                Some(v) => {
                    let s = v
                        .as_str()
                        .filter(|s| allowed.contains(s))
                        .ok_or_else(|| invalid("unsupported query filter"))?;
                    Ok(Some(s.to_owned()))
                }
            }
        };
        let continuation = match m.get("continuation") {
            None => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 1024)
                    .ok_or_else(|| invalid("invalid continuation"))?;
                Some(s.to_owned())
            }
        };
        Ok(Self {
            model: model.to_owned(),
            from,
            until,
            fields: fields(m.get("fields"))?,
            limit,
            kind: select("entry_kind", &["change", "attempt"])?,
            outcome: select(
                "outcome",
                &[
                    "committed",
                    "denied",
                    "failed",
                    "replayed",
                    "commit_unknown",
                    "read_ok",
                ],
            )?,
            continuation,
        })
    }
    /// Maximum eligible rows requested, after authorization.
    pub fn page_size(&self) -> usize {
        self.limit
    }
    pub(super) fn canonical(&self) -> Value {
        json!({"model":self.model,"from":self.from.to_string(),"until":self.until.to_string(),"fields":self.fields,"limit":self.limit,"entry_kind":self.kind,"outcome":self.outcome})
    }
}

/// Bounded direct read; duplicate composite keys retain their first request position.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ReadRequest {
    pub(super) keys: Vec<EntryKey>,
    pub(super) fields: Vec<String>,
}
impl ReadRequest {
    /// Parse bounded keys and a nonempty projection.
    /// # Errors
    /// Validation on unknown keys, malformed identities/fields or more than 1,000 keys.
    pub fn parse(v: &Value) -> Result<Self> {
        let m = object(v, &["keys", "fields"], "unknown direct-read key")?;
        let a = m
            .get("keys")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= 1000)
            .ok_or_else(|| invalid("keys must be an array of at most 1000 composite keys"))?;
        let mut keys = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for v in a {
            let key = EntryKey::parse(v)?;
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
        Ok(Self {
            keys,
            fields: fields(m.get("fields"))?,
        })
    }
    /// Distinct requested keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }
    /// Whether there are no requested keys.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Fully buffered authorized output. No raw candidates or hidden counts are accessible.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct AuditPage {
    pub(super) rows: Vec<Value>,
    pub(super) continuation: Option<String>,
}
impl AuditPage {
    /// Encoded public projection. Target integers remain exact i64 JSON tokens for the existing RPC bigint codec.
    pub fn to_json(&self) -> Value {
        json!({"rows":self.rows,"continuation":self.continuation})
    }
}
