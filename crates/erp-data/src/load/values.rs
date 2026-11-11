//! The forms that read the database or the addon's files
//! (`search=`, `file=`, lists, `type="xml"`) are `values_ext`.
//!
//! A text or an expression result is converted to what the field type takes; a value that cannot
//! be converted is a [`LoadErrorKind::TypeMismatch`] naming the model, field and what was given.

use erp_fields::{FieldType, Value};
use erp_registry::FieldSpec;

use super::LoadErrorKind;

fn str2bool(text: &str) -> bool {
    !matches!(text.to_lowercase().as_str(), "0" | "false" | "off")
}

/// The value to write for `field` from `value`, or `None` when the field is not written (a text
/// value of a one2many field: the sub-records follow the parent).
///
/// # Errors
/// [`LoadErrorKind::TypeMismatch`] when `value` cannot be converted to the field's type.
pub(crate) fn coerce(
    model: &str,
    field: &FieldSpec,
    value: Value,
) -> Result<Option<Value>, LoadErrorKind> {
    let mismatch = |given: &Value| LoadErrorKind::TypeMismatch {
        model: model.to_owned(),
        field: field.name.to_owned(),
        ty: field.ty.type_name().to_owned(),
        given: describe(given),
    };
    Ok(Some(match &field.ty {
        FieldType::Many2one { .. } => match &value {
            // `int(f_val) if f_val else False`
            Value::False | Value::Bool(false) | Value::Int(0) => Value::False,
            Value::Str(s) if s.is_empty() => Value::False,
            other => Value::Int(as_i64(other).ok_or_else(|| mismatch(other))?),
        },
        FieldType::Integer => Value::Int(as_i64(&value).ok_or_else(|| mismatch(&value))?),
        FieldType::Float { .. } | FieldType::Monetary { .. } => {
            Value::Float(as_f64(&value).ok_or_else(|| mismatch(&value))?)
        }
        FieldType::Boolean => match &value {
            Value::Str(s) => Value::Bool(str2bool(s)),
            _ => value,
        },
        FieldType::One2many { .. } => match &value {
            Value::Str(_) => return Ok(None),
            _ => value,
        },
        _ => value,
    }))
}

fn describe(value: &Value) -> String {
    match value {
        Value::Str(s) => format!("the text {s:?}"),
        other => format!("the value {other}"),
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Int(i) => Some(*i),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Str(s) => s.trim().parse().ok(),
        // a whole float inside the i64 range (2^63 itself is outside: it would saturate)
        Value::Float(f)
            if f.fract() == 0.0
                && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(f) =>
        {
            #[allow(clippy::cast_possible_truncation, reason = "checked to be in range")]
            Some(*f as i64)
        }
        _ => None,
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Float(f) => Some(*f),
        #[allow(clippy::cast_precision_loss, reason = "Python's float(int)")]
        Value::Int(i) => Some(*i as f64),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Str(s) => s.trim().parse().ok().filter(|f: &f64| f.is_finite()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole float converts inside the i64 range only: 2^63 would saturate to `i64::MAX`.
    #[test]
    fn a_whole_float_converts_only_inside_the_i64_range() {
        assert_eq!(as_i64(&Value::Float(5.0)), Some(5));
        assert_eq!(
            as_i64(&Value::Float(-9_223_372_036_854_775_808.0)),
            Some(i64::MIN)
        );
        assert_eq!(as_i64(&Value::Float(9_223_372_036_854_775_808.0)), None);
        assert_eq!(as_i64(&Value::Float(1.5)), None);
        assert_eq!(as_i64(&Value::Float(f64::NAN)), None);
    }
}
