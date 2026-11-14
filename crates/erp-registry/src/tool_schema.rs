// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tool schemas for the assistant: JSON Schema of an exposed method's keyword arguments and of a
//! model's `values`, in a portable subset, plus a validator for that subset (D6;
//! R5, R6).
//!
//! The subset is closed by type ([`ToolSchema`]): objects always carry
//! `"additionalProperties": false`, there is no `$ref` (so no cycle can be expressed), and
//! `anyOf` appears only for "T or null" and the domain leaf. [`ToolSchema::to_json`] gives the
//! schema as a [`Json`] value; `erp_fields::json::canonical` turns it into deterministic bytes
//! (members sorted, properties kept in a `BTreeMap`, enum values in a `BTreeSet`).
//!
//! ```
//! use erp_fields::json::{canonical, parse};
//! use erp_registry::tool_schema::{params_schema, validate};
//! use erp_registry::{ArgShape, ParamSpec};
//!
//! const PARAMS: &[ParamSpec] = &[
//!     ParamSpec::with_shape("name", true, "a string", ArgShape::String),
//!     ParamSpec::with_shape("limit", false, "an integer", ArgShape::OrNull(&ArgShape::Integer { min: Some(0) })),
//! ];
//! let schema = params_schema("res.partner", "rename", PARAMS).unwrap();
//! assert!(validate(&schema, &parse(br#"{"name": "Ada", "limit": null}"#, 8).unwrap()).is_ok());
//! let err = validate(&schema, &parse(br#"{"name": "Ada", "limit": -1}"#, 8).unwrap()).unwrap_err();
//! assert_eq!(err.to_string(), "$.limit: expected an integer >= 0, got -1");
//! assert!(String::from_utf8(canonical(&schema.to_json())).unwrap().contains(r#""additionalProperties":false"#));
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use erp_fields::FieldType;
use erp_fields::json::Json;

use crate::rpc::{ArgShape, ParamSpec};
use crate::spec::{FieldSpec, ModelSpec};

/// A schema of the portable subset. Build objects with [`ObjectSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSchema {
    /// An object with known properties only (`"additionalProperties": false`).
    Object(ObjectSchema),
    /// An array.
    Array {
        /// The schema of every item.
        items: Box<ToolSchema>,
        /// Fewest items, when bounded.
        min_items: Option<u32>,
        /// Most items, when bounded.
        max_items: Option<u32>,
    },
    /// A string.
    String {
        /// The only accepted values, when closed (sorted, so the bytes do not depend on
        /// declaration order).
        values: Option<BTreeSet<Box<str>>>,
        /// Most characters (Unicode scalar values), when bounded.
        max_length: Option<u32>,
        /// A fixed textual form, when the value has one.
        pattern: Option<StringPattern>,
    },
    /// A JSON integer that fits `i64`.
    Integer {
        /// Smallest accepted value.
        minimum: Option<i64>,
        /// Largest accepted value.
        maximum: Option<i64>,
    },
    /// A JSON number. Integers are accepted up to 2^53 in magnitude (larger ones are not exact
    /// floats and the binder refuses them; JSON Schema cannot say so).
    Number,
    /// `true` or `false`.
    Boolean,
    /// `null`.
    Null,
    /// Any of these schemas ("T or null", and the domain leaf's value).
    AnyOf(Vec<ToolSchema>),
}

/// The fixed textual forms a [`ToolSchema::String`] can require; each is emitted as a `pattern`
/// and checked by [`validate`] without a regular-expression engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringPattern {
    /// Canonical signed i64 decimal text.
    BigInt,
    /// `YYYY-MM-DD`.
    Date,
    /// `YYYY-MM-DD HH:MM:SS` (naive UTC, the server's datetime form).
    Datetime,
    /// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` in hexadecimal digits.
    Uuid,
}

impl StringPattern {
    /// The JSON Schema `pattern` text.
    pub const fn regex(self) -> &'static str {
        match self {
            Self::BigInt => "^(0|-?[1-9][0-9]{0,18})$",
            Self::Date => "^[0-9]{4}-[0-9]{2}-[0-9]{2}$",
            Self::Datetime => "^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}$",
            Self::Uuid => {
                "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
            }
        }
    }

    /// Position by position: `d` a decimal digit, `h` a hexadecimal digit, anything else itself.
    const fn template(self) -> &'static str {
        match self {
            Self::BigInt => "",
            Self::Date => "dddd-dd-dd",
            Self::Datetime => "dddd-dd-dd dd:dd:dd",
            Self::Uuid => "hhhhhhhh-hhhh-hhhh-hhhh-hhhhhhhhhhhh",
        }
    }

    const fn describe(self) -> &'static str {
        match self {
            Self::BigInt => "canonical signed 64-bit decimal text",
            Self::Date => "a date (YYYY-MM-DD)",
            Self::Datetime => "a datetime (YYYY-MM-DD HH:MM:SS)",
            Self::Uuid => "a UUID",
        }
    }

    /// Whether `text` has the form (the same strings the `regex` accepts).
    pub fn matches(self, text: &str) -> bool {
        if self == Self::BigInt {
            return erp_fields::BigInt::parse(text).is_ok();
        }
        let template = self.template().as_bytes();
        text.len() == template.len()
            && text.bytes().zip(template).all(|(c, t)| match t {
                b'd' => c.is_ascii_digit(),
                b'h' => c.is_ascii_hexdigit(),
                literal => c == *literal,
            })
    }
}

/// An object schema: properties by name (sorted), and the names a value must carry, always a
/// subset of the properties.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectSchema {
    properties: BTreeMap<Box<str>, Property>,
    required: BTreeSet<Box<str>>,
}

/// One property of an [`ObjectSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    schema: ToolSchema,
    description: Option<Box<str>>,
}

impl Property {
    /// The property's schema.
    pub fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    /// The description, when one is emitted.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

impl ObjectSchema {
    /// An object without properties.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a property; `required` puts its name in `required`. Returns `false` (and changes
    /// nothing) when the name is already a property.
    pub fn insert(
        &mut self,
        name: &str,
        schema: ToolSchema,
        description: Option<&str>,
        required: bool,
    ) -> bool {
        if self.properties.contains_key(name) {
            return false;
        }
        self.properties.insert(
            name.into(),
            Property {
                schema,
                description: description.map(Into::into),
            },
        );
        if required {
            self.required.insert(name.into());
        }
        true
    }

    /// The properties, sorted by name.
    pub fn properties(&self) -> impl Iterator<Item = (&str, &Property)> {
        self.properties.iter().map(|(k, v)| (&**k, v))
    }

    /// The required names, sorted.
    pub fn required(&self) -> impl Iterator<Item = &str> {
        self.required.iter().map(|k| &**k)
    }
}

fn member(key: &str, value: Json) -> (Box<str>, Json) {
    (key.into(), value)
}

fn type_member(name: &str) -> (Box<str>, Json) {
    member("type", Json::Str(name.into()))
}

fn int_member(key: &str, value: impl Into<i64>) -> (Box<str>, Json) {
    member(key, Json::Int(value.into()))
}

impl ToolSchema {
    /// "This or null": `anyOf [this, null]`, unchanged when it already accepts `null`.
    pub fn or_null(self) -> Self {
        match self {
            Self::Null => Self::Null,
            Self::AnyOf(branches) if branches.contains(&Self::Null) => Self::AnyOf(branches),
            other => Self::AnyOf(vec![other, Self::Null]),
        }
    }

    /// The schema as JSON (pass it to `erp_fields::json::canonical` for stable bytes).
    pub fn to_json(&self) -> Json {
        Json::Object(self.members())
    }

    fn members(&self) -> Vec<(Box<str>, Json)> {
        match self {
            Self::Object(object) => {
                let properties = object
                    .properties
                    .iter()
                    .map(|(name, property)| {
                        let mut members = property.schema.members();
                        if let Some(text) = &property.description {
                            members.push(member("description", Json::Str(text.clone())));
                        }
                        (name.clone(), Json::Object(members))
                    })
                    .collect();
                let required = object
                    .required
                    .iter()
                    .map(|name| Json::Str(name.clone()))
                    .collect();
                vec![
                    type_member("object"),
                    member("properties", Json::Object(properties)),
                    member("required", Json::Array(required)),
                    member("additionalProperties", Json::Bool(false)),
                ]
            }
            Self::Array {
                items,
                min_items,
                max_items,
            } => {
                let mut out = vec![type_member("array"), member("items", items.to_json())];
                if let Some(n) = min_items {
                    out.push(int_member("minItems", *n));
                }
                if let Some(n) = max_items {
                    out.push(int_member("maxItems", *n));
                }
                out
            }
            Self::String {
                values,
                max_length,
                pattern,
            } => {
                let mut out = vec![type_member("string")];
                if let Some(values) = values {
                    let list = values.iter().map(|v| Json::Str(v.clone())).collect();
                    out.push(member("enum", Json::Array(list)));
                }
                if let Some(n) = max_length {
                    out.push(int_member("maxLength", *n));
                }
                if let Some(p) = pattern {
                    out.push(member("pattern", Json::Str(p.regex().into())));
                }
                out
            }
            Self::Integer { minimum, maximum } => {
                let mut out = vec![type_member("integer")];
                if let Some(n) = minimum {
                    out.push(int_member("minimum", *n));
                }
                if let Some(n) = maximum {
                    out.push(int_member("maximum", *n));
                }
                out
            }
            Self::Number => vec![type_member("number")],
            Self::Boolean => vec![type_member("boolean")],
            Self::Null => vec![type_member("null")],
            Self::AnyOf(branches) => vec![member(
                "anyOf",
                Json::Array(branches.iter().map(Self::to_json).collect()),
            )],
        }
    }

    /// What the schema accepts, for violation messages.
    fn expected(&self) -> String {
        match self {
            Self::Object(_) => "an object".into(),
            Self::Array { .. } => "an array".into(),
            Self::String {
                pattern: Some(p), ..
            } => p.describe().into(),
            Self::String {
                values: Some(v), ..
            } => one_of(v),
            Self::String { .. } => "a string".into(),
            Self::Integer { .. } => "an integer".into(),
            Self::Number => "a number".into(),
            Self::Boolean => "a boolean".into(),
            Self::Null => "null".into(),
            Self::AnyOf(branches) => branches
                .iter()
                .map(Self::expected)
                .collect::<Vec<_>>()
                .join(" or "),
        }
    }

    /// Whether `value`'s JSON type is the one this schema takes (for picking the branch of an
    /// `anyOf` whose message to report).
    fn takes_type_of(&self, value: &Json) -> bool {
        matches!(
            (self, value),
            (Self::Object(_), Json::Object(_))
                | (Self::Array { .. }, Json::Array(_))
                | (Self::String { .. }, Json::Str(_))
                | (Self::Integer { .. }, Json::Int(_) | Json::BigInt(_))
                | (
                    Self::Number,
                    Json::Int(_) | Json::Float(_) | Json::BigInt(_)
                )
                | (Self::Boolean, Json::Bool(_))
                | (Self::Null, Json::Null)
        )
    }
}

/// Most characters of a received string or key shown in a violation.
const SHOWN_CHARS: usize = 64;

fn shown(text: &str) -> String {
    let mut out: String = text.chars().take(SHOWN_CHARS).collect();
    if text.chars().nth(SHOWN_CHARS).is_some() {
        out.push('…');
    }
    format!("{out:?}")
}

fn one_of(values: &BTreeSet<Box<str>>) -> String {
    let list: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
    format!("one of {}", list.join(", "))
}

/// Why a value does not match a schema: where (`$`, `$.name`, `$.items[2]`), what the schema
/// expects there and what was received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaViolation {
    path: String,
    expected: String,
    got: String,
}

impl SchemaViolation {
    /// The location in the value, `$` for the value itself.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// What the schema expects there.
    pub fn expected(&self) -> &str {
        &self.expected
    }

    /// What the value has there.
    pub fn got(&self) -> &str {
        &self.got
    }
}

impl fmt::Display for SchemaViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: expected {}, got {}",
            self.path, self.expected, self.got
        )
    }
}

impl std::error::Error for SchemaViolation {}

/// Largest integer magnitude a `number` takes (every integer up to it is an exact `f64`).
const MAX_EXACT_FLOAT_INT: u64 = 1 << 53;

/// Check `value` against `schema`. The walk follows the schema, so its depth is the schema's,
/// whatever the value's nesting; work is linear in the size of the value.
///
/// # Errors
///
/// The first [`SchemaViolation`] found, in document order.
pub fn validate(schema: &ToolSchema, value: &Json) -> Result<(), SchemaViolation> {
    let mut path = String::from("$");
    check(schema, value, &mut path)
}

fn violation(path: &str, expected: impl Into<String>, got: impl Into<String>) -> SchemaViolation {
    SchemaViolation {
        path: path.to_owned(),
        expected: expected.into(),
        got: got.into(),
    }
}

fn type_mismatch(schema: &ToolSchema, value: &Json, path: &str) -> SchemaViolation {
    let got = match value {
        Json::BigInt(_) => "an integer outside the 64-bit range",
        other => other.type_name(),
    };
    violation(path, schema.expected(), got)
}

fn push_key(path: &mut String, key: &str) {
    if !key.is_empty()
        && key.len() <= SHOWN_CHARS
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        path.push('.');
        path.push_str(key);
    } else {
        path.push('[');
        path.push_str(&shown(key));
        path.push(']');
    }
}

fn check(schema: &ToolSchema, value: &Json, path: &mut String) -> Result<(), SchemaViolation> {
    match (schema, value) {
        (ToolSchema::Object(object), Json::Object(members)) => {
            let len = path.len();
            for (key, member) in members {
                push_key(path, key);
                match object.properties.get(key) {
                    Some(property) => check(&property.schema, member, path)?,
                    None => return Err(violation(path, "no such property", member.type_name())),
                }
                path.truncate(len);
            }
            for name in &object.required {
                if !members.iter().any(|(key, _)| key == name) {
                    push_key(path, name);
                    return Err(violation(path, "a value (required)", "nothing"));
                }
            }
            Ok(())
        }
        (
            ToolSchema::Array {
                items,
                min_items,
                max_items,
            },
            Json::Array(values),
        ) => {
            let count = values.len();
            if let Some(min) = min_items
                && count < *min as usize
            {
                return Err(violation(
                    path,
                    format!("at least {min} items"),
                    format!("{count} items"),
                ));
            }
            if let Some(max) = max_items
                && count > *max as usize
            {
                return Err(violation(
                    path,
                    format!("at most {max} items"),
                    format!("{count} items"),
                ));
            }
            let len = path.len();
            for (index, item) in values.iter().enumerate() {
                path.push_str(&format!("[{index}]"));
                check(items, item, path)?;
                path.truncate(len);
            }
            Ok(())
        }
        (
            ToolSchema::String {
                values,
                max_length,
                pattern,
            },
            Json::Str(text),
        ) => {
            if let Some(values) = values
                && !values.contains(&**text)
            {
                return Err(violation(path, one_of(values), shown(text)));
            }
            if let Some(max) = max_length {
                let chars = text.chars().count();
                if chars > *max as usize {
                    return Err(violation(
                        path,
                        format!("at most {max} characters"),
                        format!("{chars} characters"),
                    ));
                }
            }
            if let Some(p) = pattern
                && !p.matches(text)
            {
                return Err(violation(path, p.describe(), shown(text)));
            }
            Ok(())
        }
        (ToolSchema::Integer { minimum, maximum }, Json::Int(n)) => {
            if let Some(min) = minimum
                && n < min
            {
                return Err(violation(
                    path,
                    format!("an integer >= {min}"),
                    n.to_string(),
                ));
            }
            if let Some(max) = maximum
                && n > max
            {
                return Err(violation(
                    path,
                    format!("an integer <= {max}"),
                    n.to_string(),
                ));
            }
            Ok(())
        }
        (ToolSchema::Number, Json::Float(_)) => Ok(()),
        (ToolSchema::Number, Json::Int(n)) if n.unsigned_abs() <= MAX_EXACT_FLOAT_INT => Ok(()),
        (ToolSchema::Number, Json::Int(n)) => Err(violation(
            path,
            "a number (integers up to 2^53 in magnitude)",
            n.to_string(),
        )),
        (ToolSchema::Boolean, Json::Bool(_)) | (ToolSchema::Null, Json::Null) => Ok(()),
        (ToolSchema::AnyOf(branches), _) => {
            let mut typed = None;
            for branch in branches {
                match check(branch, value, path) {
                    Ok(()) => return Ok(()),
                    Err(e) if branch.takes_type_of(value) && typed.is_none() => typed = Some(e),
                    Err(_) => {}
                }
            }
            Err(typed.unwrap_or_else(|| type_mismatch(schema, value, path)))
        }
        (
            ToolSchema::Object(_)
            | ToolSchema::Array { .. }
            | ToolSchema::String { .. }
            | ToolSchema::Integer { .. }
            | ToolSchema::Number
            | ToolSchema::Boolean
            | ToolSchema::Null,
            _,
        ) => Err(type_mismatch(schema, value, path)),
    }
}

// ---- method parameters ------------------------------------------------------------------------

/// Why a method's parameters have no tool schema.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaExportError {
    /// A parameter's shape is [`ArgShape::Open`].
    Unsupported {
        /// The model.
        model: Box<str>,
        /// The method.
        method: Box<str>,
        /// The parameter.
        param: &'static str,
        /// Why its shape is not closed.
        reason: &'static str,
    },
    /// Two parameters have the same name.
    DuplicateParameter {
        /// The model.
        model: Box<str>,
        /// The method.
        method: Box<str>,
        /// The parameter.
        param: &'static str,
    },
}

impl fmt::Display for SchemaExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported {
                model,
                method,
                param,
                reason,
            } => write!(
                f,
                "{model}.{method}: parameter '{param}' has no closed schema ({reason})"
            ),
            Self::DuplicateParameter {
                model,
                method,
                param,
            } => write!(f, "{model}.{method}: parameter '{param}' is declared twice"),
        }
    }
}

impl std::error::Error for SchemaExportError {}

/// The schema of `[field, operator, value]` and of the connectors of a domain (R2). The binder
/// checks operator names and the leaf's arity details the subset cannot state positionally.
fn domain_schema() -> ToolSchema {
    let scalar = || {
        vec![
            unbounded_string(None, None),
            ToolSchema::Integer {
                minimum: None,
                maximum: None,
            },
            ToolSchema::Number,
            ToolSchema::Boolean,
            ToolSchema::Null,
        ]
    };
    let mut leaf_value = scalar();
    leaf_value.push(ToolSchema::Array {
        items: Box::new(ToolSchema::AnyOf(scalar())),
        min_items: None,
        max_items: None,
    });
    let leaf = ToolSchema::Array {
        items: Box::new(ToolSchema::AnyOf(leaf_value)),
        min_items: Some(3),
        max_items: Some(3),
    };
    let connector = ToolSchema::String {
        values: Some(["!", "&", "|"].into_iter().map(Box::from).collect()),
        max_length: None,
        pattern: None,
    };
    ToolSchema::Array {
        items: Box::new(ToolSchema::AnyOf(vec![connector, leaf])),
        min_items: None,
        max_items: None,
    }
}

/// The schema of one closed shape; the reason of the first [`ArgShape::Open`] otherwise.
///
/// # Errors
///
/// The text of the [`ArgShape::Open`] found.
pub fn shape_schema(shape: &ArgShape) -> Result<ToolSchema, &'static str> {
    Ok(match shape {
        ArgShape::Boolean => ToolSchema::Boolean,
        ArgShape::Integer { min } => ToolSchema::Integer {
            minimum: *min,
            maximum: None,
        },
        ArgShape::Number => ToolSchema::Number,
        ArgShape::String => unbounded_string(None, None),
        ArgShape::Enum(names) => ToolSchema::String {
            values: Some(names.iter().map(|n| Box::from(*n)).collect()),
            max_length: None,
            pattern: None,
        },
        ArgShape::Array(inner) => ToolSchema::Array {
            items: Box::new(shape_schema(inner)?),
            min_items: None,
            max_items: None,
        },
        ArgShape::OrNull(inner) => shape_schema(inner)?.or_null(),
        ArgShape::Domain => domain_schema(),
        ArgShape::Open(reason) => return Err(reason),
    })
}

/// The schema of a method's keyword arguments: one property per parameter, `required` listing
/// the required ones. `ids` and `context` are the RPC envelope and never part of it.
///
/// # Errors
///
/// [`SchemaExportError::Unsupported`] naming model, method and parameter when a parameter's
/// shape is not closed; [`SchemaExportError::DuplicateParameter`] when a name repeats.
pub fn params_schema(
    model: &str,
    method: &str,
    params: &[ParamSpec],
) -> Result<ToolSchema, SchemaExportError> {
    let mut object = ObjectSchema::new();
    for param in params {
        let schema =
            shape_schema(&param.shape()).map_err(|reason| SchemaExportError::Unsupported {
                model: model.into(),
                method: method.into(),
                param: param.name(),
                reason,
            })?;
        if !object.insert(param.name(), schema, None, param.required()) {
            return Err(SchemaExportError::DuplicateParameter {
                model: model.into(),
                method: method.into(),
                param: param.name(),
            });
        }
    }
    Ok(ToolSchema::Object(object))
}

// ---- record values ----------------------------------------------------------------------------

/// Which call the `values` schema is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValuesMode {
    /// `create`: required fields without a default are required.
    Create,
    /// `write`: nothing is required.
    Update,
}

/// Why a field is not in a `values` schema (R6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Excluded {
    /// The field holds a credential (`secret`).
    Secret,
    /// Callers cannot write it: the id, the row version, the audit fields, server-set fields,
    /// computed fields without an inverse, read-only related fields.
    NotWritable,
    /// One2many or many2many: their commands reach other models (planned with `AI1.TOOLS-WRITE`).
    X2Many,
    /// The type has no schema in this slice (the field type's name).
    Unsupported(&'static str),
    /// The caller's predicate left it out (the user's field mask).
    Predicate,
}

fn unbounded_string(max_length: Option<u32>, pattern: Option<StringPattern>) -> ToolSchema {
    ToolSchema::String {
        values: None,
        max_length,
        pattern,
    }
}

/// The schema of a writable field's value, or why the field is left out (before the predicate).
fn field_schema(field: &FieldSpec) -> Result<ToolSchema, Excluded> {
    if field.secret {
        return Err(Excluded::Secret);
    }
    let schema = match &field.ty {
        FieldType::Id | FieldType::RowVersion => return Err(Excluded::NotWritable),
        FieldType::One2many { .. } | FieldType::Many2many { .. } => return Err(Excluded::X2Many),
        ty @ (FieldType::Xid8
        | FieldType::TimestampTz
        | FieldType::Json
        | FieldType::Reference { .. }
        | FieldType::Many2oneReference { .. }
        | FieldType::DateRange { .. }
        | FieldType::DatetimeRange { .. }) => return Err(Excluded::Unsupported(ty.type_name())),
        FieldType::BigInt => unbounded_string(Some(20), Some(StringPattern::BigInt)),
        FieldType::Boolean => ToolSchema::Boolean,
        FieldType::Integer => ToolSchema::Integer {
            minimum: Some(i64::from(i32::MIN)),
            maximum: Some(i64::from(i32::MAX)),
        },
        FieldType::Float { .. } | FieldType::Monetary { .. } => ToolSchema::Number,
        FieldType::Char { size } => unbounded_string(*size, None),
        FieldType::Text | FieldType::Html => unbounded_string(None, None),
        FieldType::Selection {
            values: Some(values),
            ..
        } => ToolSchema::String {
            values: Some(values.iter().map(|(v, _)| Box::from(v.as_str())).collect()),
            max_length: None,
            pattern: None,
        },
        FieldType::Selection { values: None, .. } => unbounded_string(None, None),
        FieldType::Date => unbounded_string(None, Some(StringPattern::Date)),
        FieldType::Datetime => unbounded_string(None, Some(StringPattern::Datetime)),
        FieldType::Uuid => unbounded_string(None, Some(StringPattern::Uuid)),
        FieldType::Many2one { .. } => ToolSchema::Integer {
            minimum: Some(1),
            maximum: None,
        },
    };
    let computed_only = field.compute.is_some() && field.inverse.is_none();
    let readonly_related = field.related.is_some() && field.readonly;
    if field.magic || field.server_set || computed_only || readonly_related {
        return Err(Excluded::NotWritable);
    }
    Ok(if field.required {
        schema
    } else {
        schema.or_null()
    })
}

/// The description of a field: its code-declared label and help; none for a manual field, whose
/// texts users edit.
fn field_description(field: &FieldSpec) -> Option<String> {
    if field.manual {
        return None;
    }
    match (field.string.is_empty(), field.help) {
        (true, None) => None,
        (true, Some(help)) => Some(help.to_owned()),
        (false, None) => Some(field.string.clone()),
        (false, Some(help)) => Some(format!("{}: {help}", field.string)),
    }
}

/// The schema of `values` for `create` or `write` on `model`, and the fields left out with the
/// reason, sorted by name. `keep` is the caller's field mask (S6 applies the user's access there;
/// this function does no access work).
///
/// ```
/// # fn demo(model: &erp_registry::ModelSpec) {
/// use erp_registry::tool_schema::{ValuesMode, values_schema};
/// let (schema, excluded) = values_schema(model, ValuesMode::Update, |_| true);
/// # let _ = (schema, excluded);
/// # }
/// ```
pub fn values_schema(
    model: &ModelSpec,
    mode: ValuesMode,
    keep: impl Fn(&FieldSpec) -> bool,
) -> (ToolSchema, Vec<(&'static str, Excluded)>) {
    let mut object = ObjectSchema::new();
    let mut excluded = Vec::new();
    for field in &model.fields {
        let schema = match field_schema(field) {
            Ok(_) if !keep(field) => Err(Excluded::Predicate),
            other => other,
        };
        match schema {
            Ok(schema) => {
                let required =
                    mode == ValuesMode::Create && field.required && field.default.is_none();
                let description = field_description(field);
                // field names are unique in a model; a repeat would be a registry bug
                if !object.insert(field.name, schema, description.as_deref(), required) {
                    excluded.push((field.name, Excluded::NotWritable));
                }
            }
            Err(reason) => excluded.push((field.name, reason)),
        }
    }
    excluded.sort_by_key(|(name, _)| *name);
    (ToolSchema::Object(object), excluded)
}
