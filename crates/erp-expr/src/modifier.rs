//! View modifier Python expression manipulation.

use crate::error::{ExprError, ExprResult};

/// Implements the exact algorithm.
pub fn combine_python_attribute(
    current: Option<&str>,
    add: Option<&str>,
    remove: Option<&str>,
    separator: Option<&str>,
    attribute_name: &str,
) -> ExprResult<Option<String>> {
    let sep = separator
        .map(|s| s.trim())
        .ok_or_else(|| ExprError::modifier_error(format!(
            "Missing separator for python expression '{attribute_name}'; valid values are 'and' and 'or'"
        )))?;

    if sep != "and" && sep != "or" {
        return Err(ExprError::modifier_error(format!(
            "Invalid separator '{sep}' for python expression '{attribute_name}'; valid values are 'and' and 'or'"
        )));
    }

    let mut value = current.unwrap_or("").to_string();

    if let Some(rem) = remove {
        let rem_trimmed = rem.trim();
        if !rem_trimmed.is_empty() {
            // Check if value equals `rem_trimmed` optionally enclosed in parentheses
            if is_exact_match_enclosed(&value, rem_trimmed) {
                value.clear();
            } else {
                let patterns = [
                    format!("({rem_trimmed}) {sep} "),
                    format!(" {sep} ({rem_trimmed})"),
                    format!("{rem_trimmed} {sep} "),
                    format!(" {sep} {rem_trimmed}"),
                ];
                for pat in &patterns {
                    if let Some(idx) = value.find(pat) {
                        value = format!("{}{}", &value[..idx], &value[idx + pat.len()..]);
                        break;
                    }
                }
            }
        }
    }

    if let Some(to_add) = add {
        let to_add_trimmed = to_add.trim();
        if !to_add_trimmed.is_empty() {
            if value.is_empty() {
                value = to_add_trimmed.to_string();
            } else {
                value = format!("({value}) {sep} ({to_add_trimmed})");
            }
        }
    }

    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

/// Checks if `value` equals `rem` surrounded by any number of matching parentheses.
fn is_exact_match_enclosed(value: &str, rem: &str) -> bool {
    let mut trimmed = value.trim();
    while trimmed.starts_with('(') && trimmed.ends_with(')') && trimmed.len() >= 2 {
        trimmed = trimmed[1..trimmed.len() - 1].trim();
    }
    trimmed == rem
}
