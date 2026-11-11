//! Value types bridging Rust and Luau execution.

use indexmap::IndexMap;

use crate::error::{ScriptError, ScriptResult};

/// Largest integer magnitude a Luau number (an IEEE double) holds exactly: 2^53.
pub const LUAU_EXACT_INT_MAX: i64 = 1 << 53;

/// Dynamically typed value exchanged between Rust and the Luau VM.
///
/// Conversions in both directions are exact or fail with [`ScriptError::Type`]:
/// - a Luau table becomes `List` when its keys are exactly `1..=n`, `Dict` when every key is a
///   UTF-8 string (sorted by key, Luau tables have no order), an empty `Dict` when it is empty;
///   anything else (holes, mixed or non-string keys, cycles) is an error;
/// - a Luau number becomes `Int` when it is integral and fits `i64` exactly, `Float` otherwise; NaN
///   and infinities are errors;
/// - a Luau string becomes `String` when it is UTF-8, `Bytes` otherwise;
/// - functions, userdata, threads, buffers and vectors are errors.
///
/// No serde derive on purpose: an untagged encoding cannot round-trip `Bytes` or tell `Int` from an
/// integral `Float`. Use the explicit `serde_json::Value` conversions, which fail on such values.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ScriptValue {
    /// Nil / null value.
    #[default]
    Nil,
    /// Boolean value.
    Bool(bool),
    /// 64-bit integer.
    Int(i64),
    /// 64-bit floating point.
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Byte sequence.
    Bytes(Vec<u8>),
    /// Heterogeneous list / array.
    List(Vec<ScriptValue>),
    /// String-keyed dictionary / table.
    Dict(IndexMap<String, ScriptValue>),
}

impl ScriptValue {
    /// Returns true if this value is Nil.
    #[must_use]
    pub fn is_nil(&self) -> bool {
        matches!(self, ScriptValue::Nil)
    }

    /// Evaluates truthiness according to Luau semantics:
    /// only `nil` and `false` are false; all numbers (including 0), strings (including ""), tables are true.
    #[must_use]
    pub fn is_truthy(&self) -> bool {
        match self {
            ScriptValue::Nil => false,
            ScriptValue::Bool(b) => *b,
            ScriptValue::Int(_)
            | ScriptValue::Float(_)
            | ScriptValue::String(_)
            | ScriptValue::Bytes(_)
            | ScriptValue::List(_)
            | ScriptValue::Dict(_) => true,
        }
    }

    /// Attempts to extract a boolean value.
    pub fn as_bool(&self) -> ScriptResult<bool> {
        match self {
            ScriptValue::Bool(b) => Ok(*b),
            _ => Err(self.expected("boolean")),
        }
    }

    /// Attempts to extract an integer value.
    pub fn as_int(&self) -> ScriptResult<i64> {
        match self {
            ScriptValue::Int(i) => Ok(*i),
            // Exact only: integral and inside the range where f64 and i64 agree (|f| <= 2^53).
            ScriptValue::Float(f)
                if f.is_finite() && f.fract() == 0.0 && f.abs() <= LUAU_EXACT_INT_MAX as f64 =>
            {
                Ok(*f as i64)
            }
            _ => Err(self.expected("integer")),
        }
    }

    /// Attempts to extract a float value.
    pub fn as_float(&self) -> ScriptResult<f64> {
        match self {
            ScriptValue::Float(f) => Ok(*f),
            ScriptValue::Int(i) if i.unsigned_abs() <= LUAU_EXACT_INT_MAX.unsigned_abs() => {
                Ok(*i as f64)
            }
            _ => Err(self.expected("float")),
        }
    }

    /// Attempts to extract a string reference.
    pub fn as_str(&self) -> ScriptResult<&str> {
        match self {
            ScriptValue::String(s) => Ok(s.as_str()),
            _ => Err(self.expected("string")),
        }
    }

    /// Attempts to extract a dictionary reference.
    pub fn as_dict(&self) -> ScriptResult<&IndexMap<String, ScriptValue>> {
        match self {
            ScriptValue::Dict(d) => Ok(d),
            _ => Err(self.expected("dictionary")),
        }
    }

    /// Attempts to extract a mutable dictionary reference.
    pub fn as_dict_mut(&mut self) -> ScriptResult<&mut IndexMap<String, ScriptValue>> {
        match self {
            ScriptValue::Dict(d) => Ok(d),
            _ => Err(self.expected("dictionary")),
        }
    }

    /// Attempts to extract a list reference.
    pub fn as_list(&self) -> ScriptResult<&[ScriptValue]> {
        match self {
            ScriptValue::List(l) => Ok(l.as_slice()),
            _ => Err(self.expected("list")),
        }
    }

    /// Look up a key if this value is a dictionary.
    pub fn get(&self, key: &str) -> Option<&ScriptValue> {
        match self {
            ScriptValue::Dict(d) => d.get(key),
            _ => None,
        }
    }

    fn expected(&self, what: &str) -> ScriptError {
        let shown = match self {
            ScriptValue::Int(i) => format!("integer {i}"),
            ScriptValue::Float(f) => format!("float {f}"),
            other => other.type_name().to_string(),
        };
        ScriptError::type_error("$", format!("expected {what}, got {shown}"))
    }

    /// Human-readable type name for diagnostics.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            ScriptValue::Nil => "nil",
            ScriptValue::Bool(_) => "boolean",
            ScriptValue::Int(_) => "integer",
            ScriptValue::Float(_) => "float",
            ScriptValue::String(_) => "string",
            ScriptValue::Bytes(_) => "bytes",
            ScriptValue::List(_) => "list",
            ScriptValue::Dict(_) => "table",
        }
    }
}

// Convenient From implementations
impl From<bool> for ScriptValue {
    fn from(v: bool) -> Self {
        ScriptValue::Bool(v)
    }
}

impl From<i64> for ScriptValue {
    fn from(v: i64) -> Self {
        ScriptValue::Int(v)
    }
}

impl From<i32> for ScriptValue {
    fn from(v: i32) -> Self {
        ScriptValue::Int(v as i64)
    }
}

impl From<f64> for ScriptValue {
    fn from(v: f64) -> Self {
        ScriptValue::Float(v)
    }
}

impl From<String> for ScriptValue {
    fn from(v: String) -> Self {
        ScriptValue::String(v)
    }
}

impl From<&str> for ScriptValue {
    fn from(v: &str) -> Self {
        ScriptValue::String(v.to_string())
    }
}

impl From<Vec<ScriptValue>> for ScriptValue {
    fn from(v: Vec<ScriptValue>) -> Self {
        ScriptValue::List(v)
    }
}

impl From<IndexMap<String, ScriptValue>> for ScriptValue {
    fn from(v: IndexMap<String, ScriptValue>) -> Self {
        ScriptValue::Dict(v)
    }
}

impl TryFrom<serde_json::Value> for ScriptValue {
    type Error = ScriptError;

    fn try_from(val: serde_json::Value) -> Result<Self, Self::Error> {
        Ok(match val {
            serde_json::Value::Null => ScriptValue::Nil,
            serde_json::Value::Bool(b) => ScriptValue::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    ScriptValue::Int(i)
                } else if n.is_f64() {
                    match n.as_f64() {
                        Some(f) => ScriptValue::Float(f),
                        None => {
                            return Err(ScriptError::type_error(
                                "$",
                                format!("JSON number {n} is not a finite float"),
                            ));
                        }
                    }
                } else {
                    // A u64 above i64::MAX: as a float it would lose its low bits.
                    return Err(ScriptError::type_error(
                        "$",
                        format!("JSON integer {n} does not fit a 64-bit signed integer"),
                    ));
                }
            }
            serde_json::Value::String(s) => ScriptValue::String(s),
            serde_json::Value::Array(arr) => {
                let mut list = Vec::with_capacity(arr.len());
                for item in arr {
                    list.push(ScriptValue::try_from(item)?);
                }
                ScriptValue::List(list)
            }
            serde_json::Value::Object(obj) => {
                let mut dict = IndexMap::with_capacity(obj.len());
                for (k, v) in obj {
                    dict.insert(k, ScriptValue::try_from(v)?);
                }
                ScriptValue::Dict(dict)
            }
        })
    }
}

impl TryFrom<ScriptValue> for serde_json::Value {
    type Error = ScriptError;

    fn try_from(val: ScriptValue) -> Result<Self, Self::Error> {
        Ok(match val {
            ScriptValue::Nil => serde_json::Value::Null,
            ScriptValue::Bool(b) => serde_json::Value::Bool(b),
            ScriptValue::Int(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            ScriptValue::Float(f) => match serde_json::Number::from_f64(f) {
                Some(n) => serde_json::Value::Number(n),
                None => {
                    return Err(ScriptError::type_error(
                        "$",
                        format!("float {f} has no JSON representation"),
                    ));
                }
            },
            ScriptValue::String(s) => serde_json::Value::String(s),
            ScriptValue::Bytes(b) => {
                return Err(ScriptError::type_error(
                    "$",
                    format!(
                        "{} bytes that are not UTF-8 have no JSON representation",
                        b.len()
                    ),
                ));
            }
            ScriptValue::List(list) => {
                let mut arr = Vec::with_capacity(list.len());
                for item in list {
                    arr.push(serde_json::Value::try_from(item)?);
                }
                serde_json::Value::Array(arr)
            }
            ScriptValue::Dict(dict) => {
                let mut map = serde_json::Map::with_capacity(dict.len());
                for (k, v) in dict {
                    map.insert(k, serde_json::Value::try_from(v)?);
                }
                serde_json::Value::Object(map)
            }
        })
    }
}
