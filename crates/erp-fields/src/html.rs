//! Reporting HTML conversion for host field writers.
//!
//! Callers must deliver the removal report through their operation result. A
//! returned cell is a storage value, not a browser Markup capability. Guest
//! results must be re-admitted by the host before cache/storage mutation.
use std::sync::Arc;

use erp_html::{BuiltinSanitizer, HtmlSanitizer};
pub use erp_html::{
    DefaultPolicy, FieldPolicy, HtmlSanitizeError, Limits, RemovalReport, SanitizeMode,
    SanitizePolicy,
};

use crate::convert::FieldRef;
use crate::{Cell, Error, Result, Value};

/// One admitted value and the complete report produced for it.
#[derive(Debug, Clone)]
#[must_use = "deliver the HTML removal report to the caller"]
pub struct HtmlConversion {
    cell: Cell,
    report: RemovalReport,
}
impl HtmlConversion {
    /// Consume both parts; neither is a browser rendering capability.
    pub fn into_parts(self) -> (Cell, RemovalReport) {
        (self.cell, self.report)
    }
    /// Inspect the complete aggregated report before publishing a value.
    pub fn report(&self) -> &RemovalReport {
        &self.report
    }
    /// Inspect the admitted storage value.
    pub fn cell(&self) -> &Cell {
        &self.cell
    }
}

/// Preserve the engine error as a source, without including the rejected body.
/// Internal sanitizer faults are programming errors, not user input failures.
pub fn field_error(field: FieldRef<'_>, cause: HtmlSanitizeError) -> Error {
    let message = format!("{}.{}: {cause}", field.model, field.field);
    let error = match cause {
        HtmlSanitizeError::Internal { .. } => Error::programming(message).with_source(cause),
        _ => Error::validation(message).with_source(cause),
    };
    error.with_context(Value::Map(Arc::new(crate::ValueMap::from([
        ("html_model".to_owned(), Value::str(field.model)),
        ("html_field".to_owned(), Value::str(field.field)),
    ]))))
}

/// Admit one typed Html cell under the host-selected policy.
///
/// # Errors
/// Refuses non-text cells, invalid content and resource failures. Raw text still
/// obeys the input byte and NUL limits and grants no trusted markup status.
pub fn convert_cell(
    field: FieldRef<'_>,
    cell: Cell,
    policy: &FieldPolicy,
    limits: &Limits,
    mode: SanitizeMode,
) -> Result<HtmlConversion> {
    let input = match cell {
        Cell::Null => {
            return Ok(HtmlConversion {
                cell: Cell::Null,
                report: RemovalReport::default(),
            });
        }
        Cell::Str(input) => input,
        _ => {
            return Err(Error::programming(format!(
                "{}.{}: expected an HTML text cell",
                field.model, field.field
            )));
        }
    };
    match policy {
        FieldPolicy::Sanitize(policy) => {
            let result = BuiltinSanitizer
                .sanitize(&input, policy, limits, mode)
                .map_err(|cause| field_error(field, cause))?;
            let (output, report) = result.into_parts();
            Ok(HtmlConversion {
                cell: Cell::Str(Arc::from(output.as_str())),
                report,
            })
        }
        FieldPolicy::UnsanitisedText => {
            // Raw text is a deliberate non-markup policy, not a bypass around admission.
            let len = u64::try_from(input.len()).map_err(|_| {
                field_error(
                    field,
                    HtmlSanitizeError::InputTooLarge {
                        len: u64::MAX,
                        limit: limits.input_bytes(),
                    },
                )
            })?;
            if len > limits.input_bytes() {
                return Err(field_error(
                    field,
                    HtmlSanitizeError::InputTooLarge {
                        len,
                        limit: limits.input_bytes(),
                    },
                ));
            }
            if let Some(offset) = input.bytes().position(|byte| byte == 0) {
                return Err(field_error(
                    field,
                    HtmlSanitizeError::NulByte {
                        offset: offset as u64,
                    },
                ));
            }
            Ok(HtmlConversion {
                cell: Cell::Str(input),
                report: RemovalReport::default(),
            })
        }
    }
}

/// Convert a dynamic Html value and retain its diagnostics.
///
/// String/numeric/boolean coercions match the ordinary textual converter. UTF-8
/// decoding is strict and returns the sanitizer's typed byte-ingress failure.
///
/// # Errors
/// Returns a typed validation error without echoing a rejected body or byte buffer.
pub fn convert_value(
    field: FieldRef<'_>,
    value: &Value,
    policy: &FieldPolicy,
    limits: &Limits,
    mode: SanitizeMode,
) -> Result<HtmlConversion> {
    let cell = match value {
        Value::False | Value::Bool(false) => Cell::Null,
        Value::Str(input) => Cell::Str(Arc::clone(input)),
        Value::Bytes(input) => {
            // Bound before decoding/copying, as at the engine's byte ingress.
            if input.len() as u64 > limits.input_bytes() {
                return Err(field_error(
                    field,
                    HtmlSanitizeError::InputTooLarge {
                        len: input.len() as u64,
                        limit: limits.input_bytes(),
                    },
                ));
            }
            let text = std::str::from_utf8(input)
                .map_err(|source| field_error(field, HtmlSanitizeError::InvalidUtf8 { source }))?;
            Cell::Str(Arc::from(text))
        }
        Value::Int(value) => Cell::Str(Arc::from(value.to_string())),
        Value::Float(value) => Cell::Str(Arc::from(format!("{value:?}"))),
        Value::Decimal(value) => Cell::Str(Arc::from(value.to_string())),
        Value::Bool(true) => Cell::Str(Arc::from("True")),
        _ => {
            return Err(Error::validation(format!(
                "{}.{}: expected an HTML text value",
                field.model, field.field
            )));
        }
    };
    convert_cell(field, cell, policy, limits, mode)
}
