//! The one input converter: dynamic [`Value`]s to typed [`Cell`]s.
//!
//! Lossy ones (`'False'` → True, `1.9` → 1 for integers) and values that cannot
//! be converted at all raise `Validation` naming the model, field and value (
//! ).

use std::sync::Arc;

use rust_decimal::{Decimal, RoundingStrategy};

use crate::cell::Cell;
use crate::error::{Error, Result};
use crate::range::{Range, RangeBounds, RangeValue};
use crate::temporal::{Date, DateTime};
use crate::types::{Digits, FieldType};
use crate::value::Value;

/// Names used in conversion error messages.
#[derive(Debug, Clone, Copy)]
pub struct FieldRef<'a> {
    /// Model name, e.g. `test_orm.mixed`.
    pub model: &'a str,
    /// Field name.
    pub field: &'a str,
    /// Field label (`string`), for messages addressed to users.
    pub label: &'a str,
}

impl FieldRef<'_> {
    fn invalid(&self, value: &Value) -> Error {
        Error::validation(format!(
            "Invalid value for {}.{}: {value}",
            self.model, self.field
        ))
    }

    fn native_error(&self, error: Error) -> Error {
        Error::new(
            error.kind(),
            format!("{}.{}: {}", self.model, self.field, error.message()),
        )
        .with_source(error)
    }

    fn expected_native(&self, expected: &str) -> Error {
        Error::validation(format!(
            "{}.{}: expected {expected}",
            self.model, self.field
        ))
    }

    fn invalid_decimal(&self, value: &Value) -> Error {
        Error::validation(format!(
            "Invalid value for {}.{}: {}",
            self.model,
            self.field,
            decimal_input_display(value)
        ))
    }
}

pub fn round_half_up(value: Decimal, scale: u32) -> Decimal {
    value.round_dp_with_strategy(scale, RoundingStrategy::MidpointAwayFromZero)
}

pub fn decimal_from_f64(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }
    // This public Option API reports non-representable float text as None; it
    // never substitutes a rounded decimal (the dynamic converter retains why).
    exact_decimal_text(&value.to_string()).ok()
}

/// Apply a field's digits to a decimal value.
pub fn apply_digits(value: Decimal, digits: Option<Digits>) -> Decimal {
    match digits {
        Some(Digits::Fixed { scale, .. }) => round_half_up(value, scale),
        _ => value,
    }
}

/// A named precision write refused fractional digits that cannot be stored under its policy.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NamedScaleExceeded {
    field: String,
    usage: String,
    digits: u32,
}

impl NamedScaleExceeded {
    /// Qualified field whose value was refused.
    pub fn field(&self) -> &str {
        &self.field
    }
    /// Configured precision usage.
    pub fn usage(&self) -> &str {
        &self.usage
    }
    /// Maximum significant fractional digits permitted by the usage.
    pub fn digits(&self) -> u32 {
        self.digits
    }
}

impl std::fmt::Display for NamedScaleExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Excess decimal scale for {} (precision '{}'): maximum {} fractional digits.",
            self.field, self.usage, self.digits
        )
    }
}
impl std::error::Error for NamedScaleExceeded {}

/// Check significant fractional scale without rounding or changing the decimal.
///
/// Trailing zeroes are lossless: `1.2300` is admissible at two places.
/// The caller supplies an authoritative, resolved named precision in `0..=28`.
///
/// # Errors
///
/// Validation with [`NamedScaleExceeded`] as its source when the value has excess scale;
/// Validation when `digits` is outside the Decimal scale range.
///
/// ```
/// use erp_fields::convert::{FieldRef, NamedScaleExceeded, validate_named_scale};
/// use erp_fields::ErrorKind;
/// let field = FieldRef { model: "demo", field: "quantity", label: "Quantity" };
/// validate_named_scale(field, "Product Unit", "1.2300".parse().unwrap(), 2).unwrap();
/// let error = validate_named_scale(field, "Product Unit", "1.234".parse().unwrap(), 2).unwrap_err();
/// assert_eq!(error.kind(), ErrorKind::Validation);
/// let source = std::error::Error::source(&error).unwrap()
///     .downcast_ref::<NamedScaleExceeded>().unwrap();
/// assert_eq!(source.field(), "demo.quantity");
/// assert_eq!(source.usage(), "Product Unit");
/// assert_eq!(source.digits(), 2);
/// ```
pub fn validate_named_scale(
    field: FieldRef<'_>,
    usage: &str,
    value: Decimal,
    digits: u32,
) -> Result<()> {
    if digits > Decimal::MAX_SCALE {
        return Err(Error::validation(format!(
            "Decimal precision '{usage}' must be between 0 and 28, got {digits}."
        )));
    }
    if value.normalize().scale() > digits {
        let source = NamedScaleExceeded {
            field: format!("{}.{}", field.model, field.field),
            usage: usage.to_owned(),
            digits,
        };
        return Err(Error::validation(source.to_string()).with_source(source));
    }
    Ok(())
}

pub fn json_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

/// Cell for a JSON document, NULL when falsy.
pub fn json_cell(value: Option<serde_json::Value>) -> Cell {
    match value {
        Some(v) if json_truthy(&v) => Cell::Json(Arc::new(v)),
        _ => Cell::Null,
    }
}

/// # Errors
/// `Programming` when a result batch in `value` contradicts itself (never for a batch built by
/// [`crate::rows`], which checks it when it is built).
pub fn value_to_json(value: &Value) -> Result<serde_json::Value> {
    use serde_json::Value as J;
    Ok(match value {
        Value::False => J::Bool(false),
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        Value::Decimal(d) => J::String(d.to_string()),
        Value::Str(s) => J::String(s.to_string()),
        Value::Bytes(b) => J::String(String::from_utf8_lossy(b).into_owned()),
        Value::Date(d) => J::String(d.to_string()),
        Value::DateTime(d) => J::String(d.to_string()),
        Value::Uuid(value) => J::String(value.to_string()),
        Value::Xid8(value) => J::String(value.to_string()),
        Value::TimestampTz(value) => J::String(value.to_string()),
        Value::Json(j) => serde_json::Value::clone(j),
        Value::List(items) => J::Array(items.iter().map(value_to_json).collect::<Result<_>>()?),
        Value::Map(map) => J::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), value_to_json(v)?)))
                .collect::<Result<_>>()?,
        ),
        Value::Range(range) => J::Array(
            range
                .ends_text()
                .into_iter()
                .map(|end| end.map_or(J::Bool(false), J::String))
                .collect(),
        ),
        Value::Row(row) => J::Object(
            row.iter()
                .map(|entry| {
                    let (k, v) = entry?;
                    Ok((k.to_owned(), value_to_json(&v)?))
                })
                .collect::<Result<_>>()?,
        ),
        Value::Rows(rows) => J::Array(
            rows.iter()?
                .map(|row| value_to_json(&Value::Row(row)))
                .collect::<Result<_>>()?,
        ),
    })
}

/// Largest integer magnitude every `f64` represents exactly (2^53).
const F64_EXACT_INT: u64 = 1 << 53;

/// Convert a dynamic value to the cell stored for a field of type `ty`, then apply the same
/// validation as typed writes ([`validate_cell`]).
/// HTML uses the default policy in strict mode because this Cell-only result cannot
/// carry removal reports. Host writers use [`crate::html::convert_value`] with final metadata.
pub fn to_cell(field: FieldRef<'_>, ty: &FieldType, value: &Value) -> Result<Cell> {
    if matches!(ty, FieldType::Html) {
        return crate::html::convert_value(
            field,
            value,
            &crate::html::FieldPolicy::Sanitize(crate::html::SanitizePolicy::Default(
                crate::html::DefaultPolicy::new(),
            )),
            &crate::html::Limits::recommended(),
            crate::html::SanitizeMode::Strict,
        )
        .map(|result| result.into_parts().0);
    }
    let cell = coerce(field, ty, value)?;
    validate_cell(field, ty, cell)
}

/// The `Programming` error for a caller value of a field only the server sets (a row version,
/// ): `'<model>.<field>' is set by the server and cannot be written`.
///
/// ```
/// use erp_fields::convert::{FieldRef, server_set_error};
/// let e = server_set_error(FieldRef {
///     model: "scale.quant",
///     field: "row_version",
///     label: "Row version",
/// });
/// assert_eq!(e.message(), "'scale.quant.row_version' is set by the server and cannot be written");
/// ```
pub fn server_set_error(field: FieldRef<'_>) -> Error {
    Error::programming(format!(
        "'{}.{}' is set by the server and cannot be written",
        field.model, field.field
    ))
}

/// Check (and normalize) a cell for a field, whichever surface produced it: storage fit, finite
/// floats, digits rounding, static selection keys, `Char(size)`
/// limits, falsy JSON stored as NULL, positive record ids. A row
/// version takes no caller value at all, NULL included ([`server_set_error`]).
/// HTML uses strict default sanitation; use [`crate::html::convert_cell`] to retain
/// cleaning diagnostics or select the final declared field policy.
pub fn validate_cell(field: FieldRef<'_>, ty: &FieldType, cell: Cell) -> Result<Cell> {
    if matches!(ty, FieldType::Html) {
        // A Cell-only API cannot deliver removals. Strict mode guarantees the
        // successful report is empty; host writers use html::convert_cell.
        return crate::html::convert_cell(
            field,
            cell,
            &crate::html::FieldPolicy::Sanitize(crate::html::SanitizePolicy::Default(
                crate::html::DefaultPolicy::new(),
            )),
            &crate::html::Limits::recommended(),
            crate::html::SanitizeMode::Strict,
        )
        .map(|result| result.into_parts().0);
    }
    if matches!(ty, FieldType::RowVersion) {
        return Err(server_set_error(field));
    }
    if !cell.fits(ty.storage()) {
        return Err(Error::programming(format!(
            "invalid value {cell:?} for field {}.{} of type {}",
            field.model,
            field.field,
            ty.type_name()
        )));
    }
    match (ty, cell) {
        (_, Cell::Null) => Ok(Cell::Null),
        (FieldType::Float { .. }, Cell::Float(f)) if !f.is_finite() => Err(Error::validation(
            format!("Invalid value for {}.{}: {f:?}", field.model, field.field),
        )),
        (FieldType::Float { digits }, Cell::Decimal(d)) => {
            Ok(Cell::Decimal(apply_digits(d, *digits)))
        }
        (FieldType::Char { size: Some(limit) }, Cell::Str(s)) => {
            let len = s.chars().count();
            if len > *limit as usize {
                Err(Error::validation(format!(
                    "Value too long for {}.{} (at most {limit} characters, got {len}): '{s}'",
                    field.model, field.field
                )))
            } else {
                Ok(Cell::Str(s))
            }
        }
        (
            FieldType::Selection {
                values: Some(list), ..
            },
            Cell::Str(key),
        ) => {
            if list.iter().any(|(k, _)| **k == *key) {
                Ok(Cell::Str(key))
            } else {
                Err(Error::validation(format!(
                    "Wrong value for {}.{}: {}",
                    field.model,
                    field.field,
                    Value::Str(key)
                )))
            }
        }
        (FieldType::Json, Cell::Json(j)) if !json_truthy(&j) => Ok(Cell::Null),
        (FieldType::Many2one { .. }, Cell::Id(id)) if id <= 0 => Err(Error::validation(format!(
            "Invalid value for {}.{}: {id}",
            field.model, field.field
        ))),
        (FieldType::Many2oneReference { .. }, Cell::Id(id)) if id < 0 => Err(Error::validation(
            format!("Invalid value for {}.{}: {id}", field.model, field.field),
        )),
        (FieldType::Many2oneReference { .. }, Cell::Id(0)) => Ok(Cell::Null),
        (FieldType::DateRange { bounds }, Cell::DateRange(r)) if r.is_empty(*bounds) => {
            Err(empty_range(field, *bounds))
        }
        (FieldType::DatetimeRange { bounds }, Cell::DatetimeRange(r)) if r.is_empty(*bounds) => {
            Err(empty_range(field, *bounds))
        }
        (FieldType::Reference { models, .. }, Cell::Str(text)) => {
            let wrong = || {
                Error::validation(format!(
                    "Wrong value for {}.{}: {}",
                    field.model,
                    field.field,
                    Value::Str(Arc::clone(&text))
                ))
            };
            let (model, _) = parse_reference(&text).ok_or_else(wrong)?;
            if models
                .as_ref()
                .is_some_and(|list| !list.iter().any(|(m, _)| m == model))
            {
                return Err(wrong());
            }
            Ok(Cell::Str(text))
        }
        (_, cell) => Ok(cell),
    }
}

/// The error for an empty (or inverted) range written to a range field (design §5.2):
/// range fields never store empty ranges, which would overlap nothing and escape every rule.
fn empty_range(field: FieldRef<'_>, bounds: RangeBounds) -> Error {
    Error::validation(format!(
        "The operation cannot be completed: '{}' ({}.{}) is an empty range: its lower end must \
         {} its upper end",
        field.label,
        field.model,
        field.field,
        bounds.order_words()
    ))
}

/// The error for a dynamic value that is not a range: the expected shape is named.
fn not_a_range(field: FieldRef<'_>, value: &Value, of: &str) -> Error {
    Error::validation(format!(
        "Invalid value for {}.{}: {value} (expected a range [lower, upper] of {of}, with false \
         for an unbounded end)",
        field.model, field.field
    ))
}

/// `[lower, upper]` (each end `False` for unbounded) or a range value, as a range of `T`.
fn range_of<T: Copy + Ord>(
    field: FieldRef<'_>,
    value: &Value,
    of: &str,
    end: impl Fn(&Value) -> Option<T>,
    from_range: impl Fn(&RangeValue) -> Option<Range<T>>,
) -> Result<Option<Range<T>>> {
    let bad = || not_a_range(field, value, of);
    match value {
        Value::False => Ok(None),
        Value::Range(r) => from_range(r).map(Some).ok_or_else(bad),
        Value::List(items) => match &items[..] {
            [lower, upper] => {
                // `false` ends are unbounded, whichever surface spelled them (`False`, JSON or
                // Luau `false`): a boolean is never a date
                let side = |v: &Value| match v {
                    Value::False | Value::Bool(false) => Ok(None),
                    other => end(other).map(Some).ok_or_else(bad),
                };
                Ok(Some(Range::new(side(lower)?, side(upper)?)))
            }
            _ => Err(bad()),
        },
        _ => Err(bad()),
    }
}

/// `"model,id"` split into its model and positive id (REL §4.11).
pub fn parse_reference(text: &str) -> Option<(&str, i64)> {
    let (model, id) = text.split_once(',')?;
    let id: i64 = id.trim().parse().ok()?;
    (id > 0 && !model.is_empty()).then_some((model, id))
}

fn coerce(field: FieldRef<'_>, ty: &FieldType, value: &Value) -> Result<Cell> {
    let invalid = || field.invalid(value);
    let finite = |f: f64| {
        if f.is_finite() {
            Ok(Cell::Float(f))
        } else {
            Err(invalid())
        }
    };
    match ty {
        FieldType::Id => Err(Error::programming("field 'id' cannot be assigned")),
        FieldType::Boolean => match value {
            Value::False => Ok(Cell::Bool(false)),
            Value::Bool(b) => Ok(Cell::Bool(*b)),
            Value::Int(0) => Ok(Cell::Bool(false)),
            Value::Int(1) => Ok(Cell::Bool(true)),
            _ => Err(invalid()),
        },
        FieldType::Integer => match value {
            Value::False => Ok(Cell::Int(0)),
            Value::Bool(b) => Ok(Cell::Int(i32::from(*b))),
            Value::Int(i) => i32::try_from(*i).map(Cell::Int).map_err(|_| invalid()),
            Value::Float(f) if f.fract() == 0.0 && f.abs() <= f64::from(i32::MAX) => {
                Ok(Cell::Int(*f as i32))
            }
            Value::Decimal(d) if d.fract().is_zero() => d
                .trunc()
                .normalize()
                .to_string()
                .parse::<i32>()
                .map(Cell::Int)
                .map_err(|_| invalid()),
            Value::Str(s) => s
                .trim()
                .parse::<i32>()
                .map(Cell::Int)
                .map_err(|_| invalid()),
            _ => Err(invalid()),
        },
        FieldType::Float { digits: None } => match value {
            Value::False => Ok(Cell::Float(0.0)),
            // Above 2^53 not every integer has an f64: only exact conversions are accepted.
            Value::Int(i) if i.unsigned_abs() <= F64_EXACT_INT || (*i as f64) as i64 == *i => {
                Ok(Cell::Float(*i as f64))
            }
            Value::Float(f) => finite(*f),
            Value::Decimal(d) => {
                let f: f64 = d.to_string().parse().map_err(|_| invalid())?;
                // lossless only: the float must denote the same decimal
                if decimal_from_f64(f).is_some_and(|back| back == *d) {
                    finite(f)
                } else {
                    Err(invalid())
                }
            }
            Value::Str(s) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| invalid())
                .and_then(finite),
            _ => Err(invalid()),
        },
        FieldType::Float { digits } => {
            let raw = decimal_of(value).map_err(|cause| match (digits, cause) {
                (Some(Digits::Unrounded), DecimalInputError::Precision) => {
                    let source = DecimalPrecisionExceeded::new(field, value);
                    Error::validation(source.to_string()).with_source(source)
                }
                // Other decimal fields still reject an out-of-capacity input;
                // only declared fixed/currency boundaries intentionally round.
                _ => field.invalid_decimal(value),
            })?;
            Ok(Cell::Decimal(apply_digits(raw, *digits)))
        }
        // Rounding by currency needs `res.currency` (M3).
        FieldType::Monetary { .. } => decimal_of(value)
            .map(Cell::Decimal)
            .map_err(|_| field.invalid_decimal(value)),
        FieldType::Char { .. } | FieldType::Text | FieldType::Html => match value {
            Value::False => Ok(Cell::Null),
            Value::Str(s) => Ok(Cell::Str(Arc::clone(s))),
            Value::Bytes(b) => std::str::from_utf8(b)
                .map(|s| Cell::Str(Arc::from(s)))
                .map_err(|_| invalid()),
            Value::Int(i) => Ok(Cell::Str(Arc::from(i.to_string()))),
            Value::Float(f) => Ok(Cell::Str(Arc::from(format!("{f:?}")))),
            Value::Decimal(d) => Ok(Cell::Str(Arc::from(d.to_string()))),
            Value::Bool(true) => Ok(Cell::Str(Arc::from("True"))),
            Value::Bool(false) => Ok(Cell::Null),
            _ => Err(invalid()),
        },
        FieldType::Selection { values, .. } => {
            let key: Arc<str> = match value {
                Value::False | Value::Bool(false) => return Ok(Cell::Null),
                Value::Str(s) => Arc::clone(s),
                Value::Int(i) => Arc::from(i.to_string()),
                _ => return Err(invalid()),
            };
            // key membership is checked by validate_cell
            let _ = values;
            Ok(Cell::Str(key))
        }
        FieldType::Date => match value {
            Value::False => Ok(Cell::Null),
            Value::Date(d) => Ok(Cell::Date(*d)),
            Value::DateTime(d) => Ok(Cell::Date(d.date())),
            Value::Str(s) => Date::parse_odoo(s).map(Cell::Date).ok_or_else(invalid),
            _ => Err(invalid()),
        },
        FieldType::Datetime => match value {
            Value::False => Ok(Cell::Null),
            Value::DateTime(d) => Ok(Cell::DateTime(*d)),
            Value::Date(d) => d
                .checked_at_midnight()
                .map(Cell::DateTime)
                .ok_or_else(invalid),
            Value::Str(s) => DateTime::parse_odoo(s)
                .map(Cell::DateTime)
                .ok_or_else(invalid),
            _ => Err(invalid()),
        },
        FieldType::Uuid => match value {
            Value::False => Ok(Cell::Null),
            Value::Uuid(value) => Ok(Cell::Uuid(*value)),
            Value::Str(text) => crate::Uuid::parse(text)
                .map(Cell::Uuid)
                .map_err(|error| field.native_error(error)),
            _ => Err(field.expected_native("native uuid or canonical lowercase UUID text")),
        },
        FieldType::Xid8 => match value {
            Value::False => Ok(Cell::Null),
            Value::Xid8(value) => Ok(Cell::Xid8(*value)),
            Value::Str(text) => crate::Xid8::parse(text)
                .map(Cell::Xid8)
                .map_err(|error| field.native_error(error)),
            _ => Err(field.expected_native("native xid8 or canonical unsigned decimal text")),
        },
        FieldType::TimestampTz => match value {
            Value::False => Ok(Cell::Null),
            Value::TimestampTz(value) => Ok(Cell::TimestampTz(*value)),
            Value::Str(text) => crate::TimestampTz::parse(text)
                .map(Cell::TimestampTz)
                .map_err(|error| field.native_error(error)),
            _ => Err(field.expected_native("native timestamptz or explicit-offset ISO text")),
        },
        FieldType::Json => Ok(if value.is_truthy() {
            json_cell(Some(value_to_json(value)?))
        } else {
            Cell::Null
        }),
        FieldType::Many2one { .. } => match value {
            Value::False => Ok(Cell::Null),
            Value::Int(i) if *i > 0 => Ok(Cell::Id(*i)),
            // an `(id, display_name)` pair as `read()` returns it
            // the read format `[id, display_name]` (the name may be False), nothing else (
            // F5)
            Value::List(items) => match &items[..] {
                [Value::Int(i), Value::Str(_) | Value::False] if *i > 0 => Ok(Cell::Id(*i)),
                _ => Err(invalid()),
            },
            // '5', True, 5.0 are rejected: lossy for a record reference.
            _ => Err(invalid()),
        },
        FieldType::Many2oneReference { .. } => match value {
            Value::False | Value::Int(0) => Ok(Cell::Null),
            Value::Int(i) if *i > 0 => Ok(Cell::Id(*i)),
            _ => Err(invalid()),
        },
        FieldType::Reference { .. } => match value {
            Value::False => Ok(Cell::Null),
            Value::Str(s) if s.is_empty() => Ok(Cell::Null),
            Value::Str(s) => Ok(Cell::Str(Arc::clone(s))),
            _ => Err(Error::validation(format!(
                "Wrong value for {}.{}: {value}",
                field.model, field.field
            ))),
        },
        FieldType::DateRange { .. } => range_of(
            field,
            value,
            "dates",
            |v| match v {
                Value::Date(d) => Some(*d),
                Value::DateTime(d) => Some(d.date()),
                Value::Str(s) => Date::parse_odoo(s),
                _ => None,
            },
            |r| match r {
                RangeValue::Date(r) => Some(*r),
                // a datetime range would lose its times
                RangeValue::Datetime(_) => None,
            },
        )
        .map(|r| r.map_or(Cell::Null, Cell::DateRange)),
        FieldType::DatetimeRange { .. } => range_of(
            field,
            value,
            "datetimes",
            |v| match v {
                Value::DateTime(d) => Some(*d),
                Value::Date(d) => d.checked_at_midnight(),
                Value::Str(s) => DateTime::parse_odoo(s),
                _ => None,
            },
            |r| match r {
                RangeValue::Datetime(r) => Some(*r),
                RangeValue::Date(r) => {
                    let end = |d: Option<Date>| match d {
                        None => Some(None),
                        Some(d) => d.checked_at_midnight().map(Some),
                    };
                    Some(Range::new(end(r.lower())?, end(r.upper())?))
                }
            },
        )
        .map(|r| r.map_or(Cell::Null, Cell::DatetimeRange)),
        // x2many values are commands, converted by the ORM with the comodel's fields
        FieldType::One2many { .. } | FieldType::Many2many { .. } => {
            Err(Error::programming(format!(
                "{}.{}: x2many values are converted to commands by the ORM",
                field.model, field.field
            )))
        }
        FieldType::RowVersion => Err(server_set_error(field)),
    }
}

/// Precision loss refused at an unrounded decimal field's input boundary.
///
/// The converter attaches this typed source to a [`crate::ErrorKind::Validation`]
/// error. `field()` is qualified as `model.field`; `input()` uses [`Value`]'s
/// display representation, except oversized strings use a bounded diagnostic
/// marker instead of retaining their text. The limit describes decimal/scientific admission
/// boundary; ordinary integers up to [`Decimal::MAX`] remain exactly accepted.
///
/// ```
/// use erp_fields::{Digits, FieldType, Value};
/// use erp_fields::convert::{DecimalPrecisionExceeded, FieldRef, to_cell};
/// let field = FieldRef { model: "demo", field: "ratio", label: "Ratio" };
/// let ty = FieldType::Float { digits: Some(Digits::Unrounded) };
/// let error = to_cell(field, &ty, &Value::str("1e-29")).unwrap_err();
/// let source = std::error::Error::source(&error).unwrap()
///     .downcast_ref::<DecimalPrecisionExceeded>().unwrap();
/// assert_eq!(source.field(), "demo.ratio");
/// assert_eq!(source.input(), "'1e-29'");
/// assert_eq!(source.max_digits(), 28);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecimalPrecisionExceeded {
    field: String,
    input: String,
    max_digits: u32,
}

impl DecimalPrecisionExceeded {
    fn new(field: FieldRef<'_>, input: &Value) -> Self {
        Self {
            field: format!("{}.{}", field.model, field.field),
            input: decimal_input_display(input),
            max_digits: Decimal::MAX_SCALE,
        }
    }

    /// The qualified field whose unrounded admission refused the value.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The dynamic input's display representation, or
    /// `'<decimal input exceeds 4096 bytes>'` for an oversized string.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Maximum decimal scale and scientific mantissa significant-digit count.
    pub fn max_digits(&self) -> u32 {
        self.max_digits
    }
}

impl std::fmt::Display for DecimalPrecisionExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Decimal precision exceeded for {}: {} (maximum {} digits)",
            self.field, self.input, self.max_digits
        )
    }
}

impl std::error::Error for DecimalPrecisionExceeded {}

#[derive(Debug, Clone, Copy)]
enum DecimalInputError {
    Invalid,
    Precision,
}

// This bounds direct native callers too, without building a coefficient or an
// exponent-sized string from untrusted text. Ordinary RPC has its own body cap.
const MAX_DECIMAL_INPUT_BYTES: usize = 4096;

fn decimal_input_display(value: &Value) -> String {
    match value {
        Value::Str(text) if text.len() > MAX_DECIMAL_INPUT_BYTES => {
            "'<decimal input exceeds 4096 bytes>'".to_owned()
        }
        // Strings admitted to formatting are bounded above. Other kinds retain
        // FIELD-019's existing type diagnostics; they are not numeric tokens.
        _ => value.to_string(),
    }
}

// Keep the decimal parser's sign, decimal-point and underscore compatibility,
// while validating the whole token before classifying any representability loss.
fn decimal_syntax(text: &str) -> bool {
    let unsigned = match text.strip_prefix(['+', '-']) {
        Some(unsigned) => unsigned,
        None => text,
    };
    let mut digit = false;
    let mut point = false;
    for byte in unsigned.bytes() {
        match byte {
            b'0'..=b'9' => digit = true,
            b'.' if !point => point = true,
            b'_' if digit => {}
            _ => return false,
        }
    }
    digit
}

fn exact_decimal_text(text: &str) -> std::result::Result<Decimal, DecimalInputError> {
    if text.len() > MAX_DECIMAL_INPUT_BYTES {
        return Err(DecimalInputError::Precision);
    }
    let text = text.trim();
    let Some((base, exponent)) = text.split_once(['e', 'E']) else {
        if !decimal_syntax(text) {
            return Err(DecimalInputError::Invalid);
        }
        return Decimal::from_str_exact(text).map_err(|_| DecimalInputError::Precision);
    };
    if !decimal_syntax(base) {
        return Err(DecimalInputError::Invalid);
    }
    let exponent_digits = match exponent.strip_prefix(['+', '-']) {
        Some(unsigned) => unsigned,
        None => exponent,
    };
    if exponent_digits.is_empty() || !exponent_digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DecimalInputError::Invalid);
    }
    // Parse the lexical coefficient, rather than making a Decimal from the
    // mantissa first: an exponent can rescue its otherwise excessive scale.
    let mut significant = 0_u32;
    let mut nonzero = false;
    let mut point = false;
    let mut fractional_digits = 0_i64;
    let mut coefficient = 0_i128;
    for byte in base.bytes() {
        if byte == b'.' {
            point = true;
        } else if byte.is_ascii_digit() {
            if point {
                fractional_digits = fractional_digits
                    .checked_add(1)
                    .ok_or(DecimalInputError::Precision)?;
            }
            nonzero |= byte != b'0';
            if nonzero {
                significant = significant
                    .checked_add(1)
                    .ok_or(DecimalInputError::Precision)?;
                if significant > Decimal::MAX_SCALE {
                    return Err(DecimalInputError::Precision);
                }
            }
            coefficient = coefficient
                .checked_mul(10)
                .and_then(|value| value.checked_add(i128::from(byte - b'0')))
                .ok_or(DecimalInputError::Precision)?;
        }
        // Signs and underscores were validated by decimal_syntax above.
    }
    if base.starts_with('-') {
        coefficient = coefficient
            .checked_neg()
            .ok_or(DecimalInputError::Precision)?;
    }
    let exponent = exponent
        .parse::<i64>()
        .map_err(|_| DecimalInputError::Precision)?;
    let mut scale = fractional_digits
        .checked_sub(exponent)
        .ok_or(DecimalInputError::Precision)?;
    // Removing coefficient zeros and equal scale is exact. In particular,
    // 1.0e-28 is coefficient 10/scale29 before this lossless normalization.
    while scale > 0 && coefficient % 10 == 0 {
        coefficient /= 10;
        scale = scale.checked_sub(1).ok_or(DecimalInputError::Precision)?;
        if coefficient == 0 {
            scale = 0;
        }
    }
    let scale = if scale < 0 {
        let shift = scale.checked_neg().ok_or(DecimalInputError::Precision)?;
        if shift > i64::from(Decimal::MAX_SCALE) {
            return Err(DecimalInputError::Precision);
        }
        for _ in 0..shift {
            coefficient = coefficient
                .checked_mul(10)
                .ok_or(DecimalInputError::Precision)?;
        }
        0
    } else {
        u32::try_from(scale).map_err(|_| DecimalInputError::Precision)?
    };
    Decimal::try_from_i128_with_scale(coefficient, scale).map_err(|_| DecimalInputError::Precision)
}

fn decimal_of(value: &Value) -> std::result::Result<Decimal, DecimalInputError> {
    match value {
        Value::False => Ok(Decimal::ZERO),
        Value::Int(i) => Ok(Decimal::from(*i)),
        Value::Float(f) if f.is_finite() => exact_decimal_text(&f.to_string()),
        Value::Decimal(d) => Ok(*d),
        Value::Str(s) => exact_decimal_text(s),
        // Other Value kinds are not numeric coercions, as before (FIELD-019).
        _ => Err(DecimalInputError::Invalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: FieldRef<'static> = FieldRef {
        model: "test_orm.mixed",
        field: "f",
        label: "F",
    };

    fn cell(ty: &FieldType, v: Value) -> Result<Cell> {
        to_cell(F, ty, &v)
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str_exact(s).unwrap()
    }

    #[test]
    fn spec_field_019_input_coercion() {
        assert_eq!(
            cell(&FieldType::Integer, Value::str("12")).unwrap(),
            Cell::Int(12)
        );
        // divergent: truncation is lossy
        let err = cell(&FieldType::Integer, Value::Float(1.9)).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Validation);
        assert_eq!(err.message(), "Invalid value for test_orm.mixed.f: 1.9");
        // unconvertible value is Validation
        let err = cell(&FieldType::Integer, Value::str("abc")).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Validation);
        assert_eq!(
            cell(&FieldType::Float { digits: None }, Value::str("1.5")).unwrap(),
            Cell::Float(1.5)
        );
        // divergent: 'False' would change truthiness
        let err = cell(&FieldType::Boolean, Value::str("False")).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Validation);
        assert_eq!(err.message(), "Invalid value for test_orm.mixed.f: 'False'");
        assert_eq!(
            cell(&FieldType::Boolean, Value::Int(0)).unwrap(),
            Cell::Bool(false)
        );
        let char = FieldType::Char { size: None };
        assert_eq!(cell(&char, Value::Int(42)).unwrap(), Cell::Str("42".into()));
        assert_eq!(
            cell(&char, Value::Bytes(Arc::from(&b"abc"[..]))).unwrap(),
            Cell::Str("abc".into())
        );
        assert_eq!(
            cell(&FieldType::Integer, Value::Float(5.0)).unwrap(),
            Cell::Int(5)
        );
    }

    #[test]
    fn spec_field_021_half_up_rounding() {
        let ty = FieldType::Float {
            digits: Some(Digits::Fixed {
                precision: 0,
                scale: 2,
            }),
        };
        for (input, expected) in [
            (2.49609375, "2.50"),
            (1.1, "1.10"),
            (2.675, "2.68"),
            (-2.675, "-2.68"),
        ] {
            assert_eq!(
                cell(&ty, Value::Float(input)).unwrap(),
                Cell::Decimal(dec(expected)),
                "{input}"
            );
        }
        assert_eq!(round_half_up(dec("2.675"), 2), dec("2.68"));
        assert_eq!(round_half_up(dec("-2.675"), 2), dec("-2.68"));
        let unrounded = FieldType::Float {
            digits: Some(Digits::Unrounded),
        };
        assert_eq!(
            cell(&unrounded, Value::Float(1.23456789)).unwrap(),
            Cell::Decimal(dec("1.23456789"))
        );
    }

    #[test]
    fn spec_field_023_float_falsy_digits_unrounded() {
        let unrounded = FieldType::Float {
            digits: Some(Digits::Unrounded),
        };
        assert_eq!(unrounded.column_type(), Some(crate::ColumnType::Numeric));
        let c = cell(&unrounded, Value::Float(1.23456789)).unwrap();
        assert_eq!(c, Cell::Decimal(dec("1.23456789")));
    }

    #[test]
    fn spec_field_027_date_inputs() {
        let d = Date::from_ymd(2012, 5, 1).unwrap();
        assert_eq!(cell(&FieldType::Date, Value::False).unwrap(), Cell::Null);
        for v in [
            Value::Date(d),
            Value::str("2012-05-01"),
            Value::str("2012-05-01 10:45:00"),
            Value::DateTime(DateTime::from_parts(d, 10, 45, 0, 0).unwrap()),
        ] {
            assert_eq!(cell(&FieldType::Date, v).unwrap(), Cell::Date(d));
        }
        let err = cell(&FieldType::Date, Value::str("12-5-1")).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Validation);
    }

    #[test]
    fn spec_field_028_datetime_inputs() {
        let d = Date::from_ymd(2012, 5, 1).unwrap();
        let ty = FieldType::Datetime;
        assert_eq!(cell(&ty, Value::False).unwrap(), Cell::Null);
        assert_eq!(
            cell(&ty, Value::str("2012-05-01")).unwrap(),
            Cell::DateTime(d.at_midnight())
        );
        assert_eq!(
            cell(&ty, Value::str("2012-05-01 06:00:00")).unwrap(),
            Cell::DateTime(DateTime::from_parts(d, 6, 0, 0, 0).unwrap())
        );
        assert!(cell(&ty, Value::str("12-5-1")).is_err());
        assert_eq!(
            cell(&ty, Value::Date(d)).unwrap(),
            Cell::DateTime(d.at_midnight())
        );
    }

    #[test]
    fn spec_field_031_partial_and_malformed_temporal_strings() {
        let ty_dt = FieldType::Datetime;
        assert_eq!(
            cell(&ty_dt, Value::str("2020")).unwrap(),
            Cell::DateTime(Date::from_ymd(2020, 1, 1).unwrap().at_midnight())
        );
        assert_eq!(
            cell(&ty_dt, Value::str("2020-04")).unwrap(),
            Cell::DateTime(Date::from_ymd(2020, 4, 1).unwrap().at_midnight())
        );
        assert_eq!(
            cell(&ty_dt, Value::str("2020-04-05 10")).unwrap(),
            Cell::DateTime(
                DateTime::from_parts(Date::from_ymd(2020, 4, 5).unwrap(), 10, 0, 0, 0).unwrap()
            )
        );
        assert_eq!(
            cell(&ty_dt, Value::str("2020-04-05 10:11")).unwrap(),
            Cell::DateTime(
                DateTime::from_parts(Date::from_ymd(2020, 4, 5).unwrap(), 10, 11, 0, 0).unwrap()
            )
        );

        // 'T' separator fails DateTime parsing
        let err_t = cell(&ty_dt, Value::str("2020-04-05T10:11:12")).unwrap_err();
        assert_eq!(err_t.kind(), crate::ErrorKind::Validation);
        assert_eq!(
            err_t.message(),
            "Invalid value for test_orm.mixed.f: '2020-04-05T10:11:12'"
        );

        // Fractional seconds fail DateTime parsing
        let err_frac = cell(&ty_dt, Value::str("2020-04-05 10:11:12.5")).unwrap_err();
        assert_eq!(err_frac.kind(), crate::ErrorKind::Validation);
        assert_eq!(
            err_frac.message(),
            "Invalid value for test_orm.mixed.f: '2020-04-05 10:11:12.5'"
        );

        // Unpadded month fails Datetime parsing (only Date accepts unpadded components)
        let err_unpadded = cell(&ty_dt, Value::str("2020-4-5")).unwrap_err();
        assert_eq!(err_unpadded.kind(), crate::ErrorKind::Validation);
        assert_eq!(
            err_unpadded.message(),
            "Invalid value for test_orm.mixed.f: '2020-4-5'"
        );

        let ty_date = FieldType::Date;
        assert_eq!(
            cell(&ty_date, Value::str("2020-4-5")).unwrap(),
            Cell::Date(Date::from_ymd(2020, 4, 5).unwrap())
        );
    }

    #[test]
    fn spec_field_032_selection_validation() {
        let stat = FieldType::Selection {
            values: Some(vec![
                ("foo".into(), "Foo".into()),
                ("bar".into(), "Bar".into()),
            ]),
            method: None,
        };
        let f = FieldRef {
            model: "test_orm.selection",
            field: "state",
            label: "State",
        };
        assert_eq!(
            to_cell(f, &stat, &Value::str("foo")).unwrap(),
            Cell::Str("foo".into())
        );
        assert_eq!(to_cell(f, &stat, &Value::False).unwrap(), Cell::Null);
        let err = to_cell(f, &stat, &Value::str("zz_ZZ")).unwrap_err();
        assert_eq!(
            err.message(),
            "Wrong value for test_orm.selection.state: 'zz_ZZ'"
        );
        let dynamic = FieldType::Selection {
            values: None,
            method: Some("_get_selection".into()),
        };
        assert_eq!(
            to_cell(f, &dynamic, &Value::str("zz_ZZ")).unwrap(),
            Cell::Str("zz_ZZ".into())
        );
    }

    #[test]
    fn spec_field_037_json_round_trip() {
        let mut map = crate::ValueMap::new();
        map.insert("delete_messages".to_owned(), Value::list([Value::str("x")]));
        let v = cell(&FieldType::Json, Value::Map(Arc::new(map))).unwrap();
        assert_eq!(
            v,
            Cell::Json(Arc::new(serde_json::json!({"delete_messages": ["x"]})))
        );
        let tuple = cell(
            &FieldType::Json,
            Value::list([Value::str("x"), Value::str("y")]),
        )
        .unwrap();
        assert_eq!(tuple, Cell::Json(Arc::new(serde_json::json!(["x", "y"]))));
        let empty = Value::Map(Arc::new(crate::ValueMap::new()));
        assert_eq!(cell(&FieldType::Json, empty).unwrap(), Cell::Null);
    }

    #[test]
    fn many2one_rejects_lossy_inputs() {
        let ty = FieldType::Many2one {
            comodel: "res.users".into(),
            ondelete: crate::OnDelete::SetNull,
        };
        assert_eq!(cell(&ty, Value::Int(5)).unwrap(), Cell::Id(5));
        for v in [Value::str("5"), Value::Bool(true), Value::Float(5.0)] {
            assert!(cell(&ty, v).is_err());
        }
    }

    #[test]
    fn floats_are_finite_and_exact() {
        let ty = FieldType::Float { digits: None };
        assert!(cell(&ty, Value::Float(f64::NAN)).is_err());
        assert!(cell(&ty, Value::Float(f64::INFINITY)).is_err());
        assert!(cell(&ty, Value::str("inf")).is_err());
        assert_eq!(
            cell(&ty, Value::Int(1 << 53)).unwrap(),
            Cell::Float(9_007_199_254_740_992.0)
        );
        assert!(
            cell(&ty, Value::Int((1 << 53) + 1)).is_err(),
            "not representable"
        );
        assert_eq!(
            cell(&ty, Value::Decimal(dec("1.10"))).unwrap(),
            Cell::Float(1.1)
        );
        assert!(cell(&ty, Value::Decimal(dec("0.12345678901234567891"))).is_err());
    }

    /// `Char(size)` is checked by the converter.
    #[test]
    fn char_size_is_validated() {
        let ty = FieldType::Char { size: Some(3) };
        assert_eq!(
            cell(&ty, Value::str("abc")).unwrap(),
            Cell::Str("abc".into())
        );
        let err = cell(&ty, Value::str("abcd")).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Validation);
        assert!(err.message().contains("test_orm.mixed.f") && err.message().contains("at most 3"));
        // characters, not bytes
        assert!(cell(&ty, Value::str("äöü")).is_ok());
    }

    #[test]
    fn typed_cells_are_validated_like_dynamic_values() {
        let sel = FieldType::Selection {
            values: Some(vec![("a".into(), "A".into())]),
            method: None,
        };
        assert!(validate_cell(F, &sel, Cell::Str("zz".into())).is_err());
        let digits = FieldType::Float {
            digits: Some(Digits::Fixed {
                precision: 0,
                scale: 2,
            }),
        };
        assert_eq!(
            validate_cell(F, &digits, Cell::Decimal(dec("2.675"))).unwrap(),
            Cell::Decimal(dec("2.68"))
        );
        assert_eq!(
            validate_cell(
                F,
                &FieldType::Json,
                Cell::Json(Arc::new(serde_json::json!({})))
            )
            .unwrap(),
            Cell::Null
        );
        let err = validate_cell(F, &FieldType::Integer, Cell::Str("1".into())).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Programming);
    }

    /// for an unbounded end, the shape error, empty and inverted ranges refused with the label)
    #[test]
    fn range_values_convert_and_empty_ranges_are_refused() {
        let ty = FieldType::DateRange {
            bounds: RangeBounds::HalfOpen,
        };
        let d1 = Date::from_ymd(2026, 1, 1).unwrap();
        let d2 = Date::from_ymd(2026, 2, 1).unwrap();
        let open = Value::list([Value::Date(d1), Value::False]);
        let cell = cell(&ty, open.clone()).unwrap();
        assert_eq!(cell, Cell::DateRange(Range::new(Some(d1), None)));
        // the read value is the range itself, rendered like the input list
        let read = Value::from_cell(&cell);
        assert_eq!(read.to_string(), open.to_string());
        assert_eq!(value_to_json(&read).unwrap(), value_to_json(&open).unwrap());
        // strings are parsed like a date field's
        assert_eq!(
            to_cell(
                F,
                &ty,
                &Value::list([Value::str("2026-01-01"), Value::str("2026-02-01")])
            )
            .unwrap(),
            Cell::DateRange(Range::new(Some(d1), Some(d2)))
        );
        assert_eq!(to_cell(F, &ty, &Value::False).unwrap(), Cell::Null);
        assert_eq!(
            to_cell(F, &ty, &Value::list([Value::Bool(false), Value::Date(d2)])).unwrap(),
            Cell::DateRange(Range::new(None, Some(d2)))
        );
        // wrong shapes name the field and the expected shape
        for bad in [
            Value::list([Value::Date(d1), Value::Date(d2), Value::False]),
            Value::str("[2026-01-01,2026-02-01)"),
            Value::list([Value::Int(1), Value::False]),
        ] {
            let err = to_cell(F, &ty, &bad).unwrap_err();
            assert_eq!(err.kind(), crate::ErrorKind::Validation);
            assert_eq!(
                err.message(),
                format!(
                    "Invalid value for test_orm.mixed.f: {bad} (expected a range [lower, upper] of \
                     dates, with false for an unbounded end)"
                )
            );
        }
        // inverted and empty ranges are refused with the field's label
        for (bounds, words) in [
            (RangeBounds::HalfOpen, "be before"),
            (RangeBounds::Closed, "not be after"),
        ] {
            let ty = FieldType::DateRange { bounds };
            let err =
                to_cell(F, &ty, &Value::list([Value::Date(d2), Value::Date(d1)])).unwrap_err();
            assert_eq!(err.kind(), crate::ErrorKind::Validation);
            assert_eq!(
                err.message(),
                format!(
                    "The operation cannot be completed: 'F' (test_orm.mixed.f) is an empty range: \
                     its lower end must {words} its upper end"
                )
            );
        }
        let same = Value::list([Value::Date(d1), Value::Date(d1)]);
        assert!(to_cell(F, &ty, &same).is_err(), "[d, d) is empty");
        let closed = FieldType::DateRange {
            bounds: RangeBounds::Closed,
        };
        assert!(to_cell(F, &closed, &same).is_ok(), "[d, d] holds d");
        // a typed range is checked the same way
        let err =
            validate_cell(F, &ty, Cell::DateRange(Range::new(Some(d2), Some(d1)))).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Validation);
        // datetime ranges take dates at midnight, never the other way round
        let dt = FieldType::DatetimeRange {
            bounds: RangeBounds::HalfOpen,
        };
        assert_eq!(
            to_cell(F, &dt, &Value::from(Range::new(Some(d1), Some(d2)))).unwrap(),
            Cell::DatetimeRange(Range::new(Some(d1.at_midnight()), Some(d2.at_midnight())))
        );
        let err = to_cell(
            F,
            &ty,
            &Value::from(Range::new(Some(d1.at_midnight()), None)),
        )
        .unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Validation);
    }

    /// Review S3: dates whose midnight does not fit in 64 bits of microseconds are refused.
    #[test]
    fn datetime_coercion_refuses_extreme_dates() {
        let dt = FieldType::DatetimeRange {
            bounds: RangeBounds::HalfOpen,
        };
        for days in [i32::MAX, i32::MIN] {
            let far = Date::from_days(days);
            assert_eq!(far.checked_at_midnight(), None);
            let list = Value::list([Value::Date(far), Value::False]);
            let err = to_cell(F, &dt, &list).unwrap_err();
            assert_eq!(err.kind(), crate::ErrorKind::Validation);
            assert_eq!(
                err.message(),
                format!(
                    "Invalid value for test_orm.mixed.f: {list} (expected a range [lower, upper] \
                     of datetimes, with false for an unbounded end)"
                )
            );
            let typed = Value::from(Range::new(Some(far), None));
            let err = to_cell(F, &dt, &typed).unwrap_err();
            assert_eq!(err.kind(), crate::ErrorKind::Validation);
            assert_eq!(
                err.message(),
                format!(
                    "Invalid value for test_orm.mixed.f: {typed} (expected a range [lower, upper] \
                     of datetimes, with false for an unbounded end)"
                )
            );
            let plain = Value::Date(far);
            let err = cell(&FieldType::Datetime, plain.clone()).unwrap_err();
            assert_eq!(err.kind(), crate::ErrorKind::Validation);
            assert_eq!(
                err.message(),
                format!("Invalid value for test_orm.mixed.f: {plain}")
            );
        }
    }

    #[test]
    fn id_cannot_be_assigned() {
        let err = cell(&FieldType::Id, Value::Int(5)).unwrap_err();
        assert_eq!(err.message(), "field 'id' cannot be assigned");
    }
}
