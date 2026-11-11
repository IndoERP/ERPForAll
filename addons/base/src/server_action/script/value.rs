//! Bounded, exact copies between ORM values and the VM; no lossy JSON intermediary.
use erp_sdk::script::ScriptValue;
use erp_sdk::{Error, Result, Value, ValueMap, serde_json};
use std::sync::Arc;

pub(super) const MAX_BYTES: usize = 1_048_576;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 32;

#[derive(Default)]
pub(super) struct Budget {
    bytes: usize,
    nodes: usize,
}
impl Budget {
    pub(super) fn charge(&mut self, bytes: usize, depth: usize) -> Result<()> {
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(bound)?;
        self.nodes = self.nodes.checked_add(1).ok_or_else(bound)?;
        if self.bytes > MAX_BYTES || self.nodes > MAX_NODES || depth > MAX_DEPTH {
            return Err(bound());
        }
        Ok(())
    }
}
fn bound() -> Error {
    Error::validation("Server action script value exceeds the byte, node or depth limit")
}

pub(super) fn from_value(v: &Value, b: &mut Budget, depth: usize) -> Result<ScriptValue> {
    b.charge(0, depth)?;
    Ok(match v {
        Value::False => ScriptValue::Bool(false),
        Value::Bool(x) => ScriptValue::Bool(*x),
        Value::Int(x) => ScriptValue::Int(*x),
        Value::Float(x) if x.is_finite() => ScriptValue::Float(*x),
        Value::Str(x) => {
            b.charge(x.len(), depth)?;
            ScriptValue::String(x.to_string())
        }
        Value::Bytes(x) => {
            b.charge(x.len(), depth)?;
            ScriptValue::Bytes(x.to_vec())
        }
        Value::Date(x) => text(x.to_string(), b, depth)?,
        Value::DateTime(x) => text(x.to_string(), b, depth)?,
        Value::Decimal(x) => text(x.to_string(), b, depth)?,
        Value::List(xs) => {
            if xs.len() > MAX_NODES {
                return Err(bound());
            }
            ScriptValue::List(
                xs.iter()
                    .map(|x| from_value(x, b, depth + 1))
                    .collect::<Result<_>>()?,
            )
        }
        Value::Map(xs) => {
            if xs.len() > MAX_NODES {
                return Err(bound());
            }
            ScriptValue::Dict(
                xs.iter()
                    .map(|(k, v)| {
                        b.charge(k.len(), depth)?;
                        Ok((k.clone(), from_value(v, b, depth + 1)?))
                    })
                    .collect::<Result<_>>()?,
            )
        }
        Value::Json(x) => from_json(x, b, depth + 1)?,
        _ => {
            return Err(Error::validation(
                "Server action cannot expose this value kind",
            ));
        }
    })
}
fn text(value: String, b: &mut Budget, depth: usize) -> Result<ScriptValue> {
    b.charge(value.len(), depth)?;
    Ok(ScriptValue::String(value))
}
fn from_json(v: &serde_json::Value, b: &mut Budget, depth: usize) -> Result<ScriptValue> {
    b.charge(0, depth)?;
    Ok(match v {
        serde_json::Value::Null => ScriptValue::Nil,
        serde_json::Value::Bool(x) => ScriptValue::Bool(*x),
        serde_json::Value::Number(x) => {
            if let Some(i) = x.as_i64() {
                ScriptValue::Int(i)
            } else if x.is_f64() {
                ScriptValue::Float(x.as_f64().ok_or_else(bound)?)
            } else {
                return Err(Error::validation("Script JSON integer does not fit int64"));
            }
        }
        serde_json::Value::String(x) => {
            b.charge(x.len(), depth)?;
            ScriptValue::String(x.clone())
        }
        serde_json::Value::Array(xs) => {
            if xs.len() > MAX_NODES {
                return Err(bound());
            }
            ScriptValue::List(
                xs.iter()
                    .map(|x| from_json(x, b, depth + 1))
                    .collect::<Result<_>>()?,
            )
        }
        serde_json::Value::Object(xs) => {
            if xs.len() > MAX_NODES {
                return Err(bound());
            }
            ScriptValue::Dict(
                xs.iter()
                    .map(|(k, v)| {
                        b.charge(k.len(), depth)?;
                        Ok((k.clone(), from_json(v, b, depth + 1)?))
                    })
                    .collect::<Result<_>>()?,
            )
        }
    })
}

/// Validate before a callback clones arguments into an owned request.
pub(super) fn check(v: &ScriptValue, b: &mut Budget, depth: usize) -> Result<()> {
    b.charge(0, depth)?;
    match v {
        ScriptValue::String(x) => b.charge(x.len(), depth),
        ScriptValue::Bytes(x) => b.charge(x.len(), depth),
        ScriptValue::List(xs) => {
            for x in xs {
                check(x, b, depth + 1)?;
            }
            Ok(())
        }
        ScriptValue::Dict(xs) => {
            for (k, v) in xs {
                b.charge(k.len(), depth)?;
                check(v, b, depth + 1)?;
            }
            Ok(())
        }
        ScriptValue::Float(x) if !x.is_finite() => {
            Err(Error::validation("Script number must be finite"))
        }
        ScriptValue::Nil | ScriptValue::Bool(_) | ScriptValue::Int(_) | ScriptValue::Float(_) => {
            Ok(())
        }
    }
}
pub(super) fn into_value(v: ScriptValue) -> Result<Value> {
    Ok(match v {
        ScriptValue::Nil => Value::False,
        ScriptValue::Bool(x) => Value::Bool(x),
        ScriptValue::Int(x) => Value::Int(x),
        ScriptValue::Float(x) => Value::Float(x),
        ScriptValue::String(x) => Value::Str(Arc::from(x)),
        ScriptValue::Bytes(x) => Value::Bytes(Arc::from(x)),
        ScriptValue::List(xs) => {
            Value::List(xs.into_iter().map(into_value).collect::<Result<_>>()?)
        }
        ScriptValue::Dict(xs) => Value::Map(Arc::new(
            xs.into_iter()
                .map(|(k, v)| Ok((k, into_value(v)?)))
                .collect::<Result<ValueMap>>()?,
        )),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    #[test]
    fn conversion_checks_bytes_nodes_and_depth_before_copying() {
        let exact = Value::str("x".repeat(MAX_BYTES));
        assert!(from_value(&exact, &mut Budget::default(), 0).is_ok());
        let over = Value::str("x".repeat(MAX_BYTES + 1));
        let error = from_value(&over, &mut Budget::default(), 0).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Server action script value exceeds the byte, node or depth limit"
        );
        let mut budget = Budget::default();
        from_value(&exact, &mut budget, 0).unwrap();
        assert!(from_value(&Value::str("x"), &mut budget, 0).is_err());
        assert!(from_value(&Value::Int(1), &mut Budget::default(), MAX_DEPTH + 1).is_err());
        assert!(
            check(
                &ScriptValue::List(vec![ScriptValue::Nil; MAX_NODES]),
                &mut Budget::default(),
                0
            )
            .is_err()
        );
    }
}
