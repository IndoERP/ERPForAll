// SPDX-License-Identifier: MIT OR Apache-2.0
//! Exact JSONB numeric tokens, bounded before recursive canonical construction.
use super::{CanonicalValue as Value, SealerProblem, problem::codec};
use erp_fields::{Error, Result};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use std::{collections::HashSet, fmt};

const MAX_NODES: usize = 65_536;
const MAX_DEPTH: u8 = 16;
pub(super) const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

pub(super) struct Members<'a>(pub Vec<(String, &'a RawValue)>);
impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Members<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded object")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut result = Vec::new();
                let mut seen = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if result.len() >= MAX_NODES || !seen.insert(key.clone()) {
                        return Err(de::Error::custom("object bound or duplicate"));
                    }
                    result.push((key, map.next_value::<&RawValue>()?));
                }
                Ok(Members(result))
            }
        }
        d.deserialize_map(V)
    }
}
struct Items<'a>(Vec<&'a RawValue>);
impl<'de> Deserialize<'de> for Items<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Items<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded array")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut result = Vec::new();
                while let Some(value) = seq.next_element::<&RawValue>()? {
                    if result.len() >= MAX_NODES {
                        return Err(de::Error::custom("array bound"));
                    }
                    result.push(value);
                }
                Ok(Items(result))
            }
        }
        d.deserialize_seq(V)
    }
}
// Parser errors can quote input tokens. Return a typed payload-free refusal instead.
fn invalid(_: serde_json::Error) -> Error {
    SealerProblem::RowValue.into()
}

/// Scan nesting before serde scans raw compound values; quoted braces are data.
pub(super) fn bounded(text: &str, max_depth: u8) -> Result<()> {
    if text.len() > MAX_PAYLOAD {
        return Err(SealerProblem::RowPayload.into());
    }
    let (mut depth, mut quoted, mut escape) = (0u8, false, false);
    for b in text.bytes() {
        if quoted {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                quoted = false;
            }
        } else if b == b'"' {
            quoted = true;
        } else if b == b'[' || b == b'{' {
            depth = depth.checked_add(1).ok_or(SealerProblem::DecodeLimit)?;
            if depth > max_depth {
                return Err(SealerProblem::DecodeLimit.into());
            }
        } else if b == b']' || b == b'}' {
            depth = depth.checked_sub(1).ok_or(SealerProblem::RowValue)?;
        }
    }
    if depth != 0 || quoted {
        return Err(SealerProblem::RowValue.into());
    }
    Ok(())
}
pub(super) fn members(text: &str) -> Result<Members<'_>> {
    bounded(text, MAX_DEPTH + 1)?;
    serde_json::from_str(text).map_err(invalid)
}
pub(super) fn string(raw: &RawValue) -> Result<String> {
    serde_json::from_str(raw.get()).map_err(invalid)
}
pub(super) fn integer(raw: &RawValue) -> Result<i64> {
    raw.get()
        .parse()
        .map_err(|_| SealerProblem::RowValue.into())
}
pub(super) fn array(raw: &RawValue) -> Result<Vec<&RawValue>> {
    serde_json::from_str::<Items<'_>>(raw.get())
        .map(|v| v.0)
        .map_err(invalid)
}
pub(super) fn canonical(raw: &RawValue) -> Result<Value> {
    bounded(raw.get(), MAX_DEPTH)?;
    let mut left = MAX_NODES;
    build(raw, 0, &mut left)
}
fn build(raw: &RawValue, depth: u8, left: &mut usize) -> Result<Value> {
    *left = left.checked_sub(1).ok_or(SealerProblem::DecodeLimit)?;
    let text = raw.get();
    match text.as_bytes().first() {
        Some(b'[') => {
            if depth >= MAX_DEPTH {
                return Err(SealerProblem::DecodeLimit.into());
            }
            let values = array(raw)?
                .into_iter()
                .map(|v| build(v, depth + 1, left))
                .collect::<Result<Vec<_>>>()?;
            Value::array(&values).map_err(codec)
        }
        Some(b'{') => {
            if depth >= MAX_DEPTH {
                return Err(SealerProblem::DecodeLimit.into());
            }
            let Members(values) = serde_json::from_str(text).map_err(invalid)?;
            let values = values
                .into_iter()
                .map(|(k, v)| build(v, depth + 1, left).map(|v| (k, v)))
                .collect::<Result<Vec<_>>>()?;
            let refs = values
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect::<Vec<_>>();
            Value::object(&refs).map_err(codec)
        }
        Some(b'"') => Value::text(&string(raw)?).map_err(codec),
        _ => match text {
            "null" => Ok(Value::null()),
            "true" => Ok(Value::boolean(true)),
            "false" => Ok(Value::boolean(false)),
            number => Value::numeric(number).map_err(codec),
        },
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;
    fn parse(s: &str) -> Result<Value> {
        bounded(s, 16)?;
        canonical(serde_json::from_str::<&RawValue>(s).map_err(invalid)?)
    }
    #[test]
    fn exact_decimal_and_sorted_object() {
        assert_eq!(
            parse("9007199254740993.100").unwrap(),
            Value::numeric("9007199254740993.1").unwrap()
        );
        assert_eq!(parse("-0e9").unwrap(), Value::numeric("0").unwrap());
        assert_eq!(
            parse(r#"{"z":null,"a":[true,"é"]}"#).unwrap(),
            Value::object(&[
                (
                    "a",
                    Value::array(&[Value::boolean(true), Value::text("é").unwrap()]).unwrap()
                ),
                ("z", Value::null())
            ])
            .unwrap()
        );
        assert_ne!(parse(r#"{"a":null}"#).unwrap(), parse("{}").unwrap());
    }
    #[test]
    fn refusal_messages_do_not_contain_payloads() {
        for input in [
            r#"{"canary":1,"canary":2}"#.to_owned(),
            "[".repeat(17) + &"]".repeat(17),
            "\"canary".to_owned(),
        ] {
            let e = parse(&input).unwrap_err();
            assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
            assert!(!e.to_string().contains("canary"));
            assert!(
                e.message()
                    == "a later release: persisted audit value has an unsupported shape or type"
                    || e.message()
                        == "a later release: persisted value decoding exceeds the safety limit"
            );
        }
    }
}
