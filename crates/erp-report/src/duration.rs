// SPDX-License-Identifier: MIT OR Apache-2.0
//! Duration conversion uses finite, exactly representable whole seconds, then integer decomposition.
use crate::{ReportError, format::LangFormat, locale::UNITS, program::At};
use erp_expr::{ExprValue, Map};

fn string<'a>(
    options: Option<&'a Map>,
    key: &str,
    default: &'a str,
    at: &At,
) -> Result<&'a str, ReportError> {
    match options.and_then(|o| o.get_str(key)) {
        None => Ok(default),
        Some(ExprValue::Str(s)) => Ok(s),
        Some(_) => Err(at.evaluate(format!("duration option `{key}` must be a string"))),
    }
}
fn flag(options: Option<&Map>, key: &str, at: &At) -> Result<bool, ReportError> {
    match options.and_then(|o| o.get_str(key)) {
        None => Ok(false),
        Some(ExprValue::Bool(b)) => Ok(*b),
        Some(_) => Err(at.evaluate(format!("duration option `{key}` must be a boolean"))),
    }
}
fn unit(options: Option<&Map>, key: &str, at: &At) -> Result<u64, ReportError> {
    let name = string(options, key, "second", at)?;
    UNITS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, seconds)| *seconds)
        .ok_or_else(|| at.evaluate(format!("unknown duration {key} `{name}`")))
}
pub(crate) fn format(
    value: &ExprValue,
    options: Option<&Map>,
    formats: &LangFormat,
    at: &At,
) -> Result<String, ReportError> {
    if let Some(options) = options {
        for (key, _) in options.iter() {
            let ExprValue::Str(key) = key else {
                return Err(at.evaluate("duration option keys must be strings"));
            };
            if ![
                "widget",
                "unit",
                "round",
                "digital",
                "format",
                "add_direction",
            ]
            .contains(&key.as_str())
            {
                return Err(at.evaluate(format!("unknown duration option `{key}`")));
            }
        }
    }
    let value = match value {
        ExprValue::Int(n) if n.unsigned_abs() <= 9_007_199_254_740_991 => *n as f64,
        ExprValue::Float(n) if n.is_finite() => *n,
        _ => {
            return Err(at.evaluate(
                "duration value must be a finite number within the exact whole-second range",
            ));
        }
    };
    let factor = unit(options, "unit", at)?;
    let mut round = unit(options, "round", at)?;
    let digital = flag(options, "digital", at)?;
    let direction = flag(options, "add_direction", at)?;
    let width = string(options, "format", "long", at)?;
    if !["long", "short", "narrow"].contains(&width) {
        return Err(at.evaluate(format!(
            "duration format `{width}` must be long, short or narrow"
        )));
    }
    if direction {
        return Err(ReportError::Unsupported {
            feature: "directional duration labels",
            task: "a later release",
        });
    }
    if digital {
        round = round.min(3600);
    }
    let seconds = (value * factor as f64 / round as f64).round_ties_even() * round as f64;
    // Binary64's exact integer range: arithmetic admission, not a configurable business limit.
    if !seconds.is_finite() || seconds.abs() > 9_007_199_254_740_991.0 {
        return Err(at.evaluate("duration exceeds the exact whole-second range"));
    }
    let mut remaining = seconds.abs() as u64;
    let mut parts = Vec::new();
    if digital {
        for (_, unit) in UNITS.into_iter().filter(|(_, seconds)| *seconds <= 3600) {
            let count = remaining / unit;
            remaining %= unit;
            if count == 0 && (unit > factor || unit < round) {
                continue;
            }
            parts.push(format!("{count:02}"));
        }
        Ok(format!(
            "{}{}",
            if value < 0.0 { "-" } else { "" },
            parts.join(":")
        ))
    } else {
        let locale = formats.locale().map_err(|reason| at.evaluate(reason))?;
        if value < 0.0 {
            parts.push("-".into());
        }
        for (name, unit) in UNITS {
            let count = remaining / unit;
            remaining %= unit;
            if count != 0 {
                parts.push(
                    locale
                        .duration(width, name, count)
                        .map_err(|reason| at.evaluate(reason))?,
                );
            }
        }
        Ok(parts.join(" "))
    }
}
