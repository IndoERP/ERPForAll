//! A strict JSON reader for untrusted payloads: bounded size and depth, duplicate object keys
//! refused (never last-one-wins), objects kept in document order.

use std::collections::HashSet;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::Number;
use serde_json::value::RawValue;

use crate::error::WireError;
use crate::limits::{Limit, WireLimits};

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// The members of one object, each value still unparsed text, with duplicate keys refused.
struct Members<'de>(Vec<(String, &'de RawValue)>);

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Members<'de>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members<'de>, A::Error> {
                let mut members = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(de::Error::custom(format!("duplicate key {key:?}")));
                    }
                    members.push((key, map.next_value::<&'de RawValue>()?));
                }
                Ok(Members(members))
            }
        }
        deserializer.deserialize_map(V)
    }
}

/// A number from its text. Integers keep their exact form; every other number (a fraction, an
/// exponent, `-0`, an integer beyond 64 bits) is parsed with `str::parse::<f64>`, which rounds
/// correctly. This is the only place the crate turns decimal text into an `f64`, so the workspace
/// does not need serde_json's additive `float_roundtrip` feature. The text has already passed
/// the JSON number grammar when it is read as a raw value.
fn number(text: &str) -> Result<Number, serde_json::Error> {
    if text != "-0" {
        if let Ok(v) = text.parse::<i64>() {
            return Ok(Number::from(v));
        }
        if let Ok(v) = text.parse::<u64>() {
            return Ok(Number::from(v));
        }
    }
    let value: f64 = text
        .parse()
        .map_err(|_| de::Error::custom("number is not valid"))?;
    Number::from_f64(value).ok_or_else(|| de::Error::custom("number is not finite"))
}

/// Build the value for one raw JSON text. Each nesting level reads its own text again, so the work
/// is bounded by the nesting depth, which `parse` limits before any value is built.
fn build(raw: &RawValue) -> Result<Json, serde_json::Error> {
    let text = raw.get().trim_start();
    match text.as_bytes().first() {
        Some(b'{') => {
            let Members(members) = serde_json::from_str(text)?;
            members
                .into_iter()
                .map(|(key, value)| build(value).map(|value| (key, value)))
                .collect::<Result<Vec<_>, _>>()
                .map(Json::Object)
        }
        Some(b'[') => {
            let items: Vec<&RawValue> = serde_json::from_str(text)?;
            items
                .into_iter()
                .map(build)
                .collect::<Result<Vec<_>, _>>()
                .map(Json::Array)
        }
        Some(b'"') => serde_json::from_str::<String>(text).map(Json::String),
        _ => match text.trim_end() {
            "null" => Ok(Json::Null),
            "true" => Ok(Json::Bool(true)),
            "false" => Ok(Json::Bool(false)),
            other => number(other).map(Json::Number),
        },
    }
}

/// The deepest nesting of arrays and objects in `bytes`, counted outside strings; stops as soon
/// as `max` is exceeded.
fn depth_exceeds(bytes: &[u8], max: u64) -> bool {
    let (mut depth, mut in_string, mut escaped) = (0_u64, false, false);
    for &b in bytes {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// Parse one document under the size and depth limits.
pub(crate) fn parse(bytes: &[u8], limits: &WireLimits) -> Result<Json, WireError> {
    limits.check(Limit::Bytes, bytes.len(), "")?;
    if depth_exceeds(bytes, limits.get(Limit::Depth)) {
        return Err(WireError::LimitExceeded {
            limit: Limit::Depth.name(),
            path: String::new(),
            maximum: limits.get(Limit::Depth),
        });
    }
    let malformed = |e: serde_json::Error| WireError::Malformed {
        path: String::new(),
        reason: e.to_string(),
    };
    let raw = serde_json::from_slice::<&RawValue>(bytes).map_err(malformed)?;
    build(raw).map_err(malformed)
}
