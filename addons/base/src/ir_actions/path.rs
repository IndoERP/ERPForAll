//! The `path` of an action: the URL segment the web client shows for it (
//! ).
//!
//! Uniqueness across every action type is the
//! `_path_unique` index of `ir_actions` (design m2-s3 §6.1): two transactions saving the same
//! path meet on it, the second waits for the first and fails, with no count query to race.

use std::fmt;

/// Longest path accepted, in bytes (design m2-s3 §12).
pub const MAX_PATH_BYTES: usize = 128;

/// Why a path is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActionPathError {
    /// Not `[a-z][a-z0-9_-]*` (an empty path included).
    Format,
    /// Starts with `m-`.
    ReservedM,
    /// Starts with `action-`.
    ReservedAction,
    /// Is `new`.
    ReservedNew,
    /// Longer than [`MAX_PATH_BYTES`].
    TooLong {
        /// The path's length in bytes.
        len: usize,
        /// The bound.
        max: usize,
    },
    /// Another action of any type has it. Never returned by [`check_path`], which checks the
    /// format only: the key table's unique index `_path_unique` raises it when the row is written
    /// (`create`, `write`), so two transactions cannot both pass.
    Duplicate,
}

impl fmt::Display for ActionPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format => f.write_str(
                "The path should contain only lowercase alphanumeric characters, underscore, and \
                 dash, and it should start with a letter.",
            ),
            Self::ReservedM => f.write_str("'m-' is a reserved prefix."),
            Self::ReservedAction => f.write_str("'action-' is a reserved prefix."),
            Self::ReservedNew => f.write_str("'new' is reserved, and can not be used as path."),
            Self::TooLong { len, max } => {
                write!(
                    f,
                    "The path is {len} bytes long; at most {max} are allowed."
                )
            }
            Self::Duplicate => {
                f.write_str("Path to show in the URL must be unique! Please choose another one.")
            }
        }
    }
}

impl std::error::Error for ActionPathError {}

impl From<ActionPathError> for erp_sdk::Error {
    fn from(error: ActionPathError) -> Self {
        erp_sdk::Error::validation(error.to_string()).with_source(error)
    }
}

/// Check the format of `path`.
///
/// ```
/// use erp_addon_base::ir_actions::{ActionPathError, check_path};
/// assert_eq!(check_path("invoices"), Ok(()));
/// assert_eq!(check_path("Invoices"), Err(ActionPathError::Format));
/// assert_eq!(check_path("m-x"), Err(ActionPathError::ReservedM));
/// ```
///
/// # Errors
///
/// The [`ActionPathError`] that applies first: length, pattern, `m-`, `action-`, `new`.
pub fn check_path(path: &str) -> Result<(), ActionPathError> {
    if path.len() > MAX_PATH_BYTES {
        return Err(ActionPathError::TooLong {
            len: path.len(),
            max: MAX_PATH_BYTES,
        });
    }
    let mut bytes = path.bytes();
    let starts_with_letter = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
    let rest_ok =
        bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !starts_with_letter || !rest_ok {
        return Err(ActionPathError::Format);
    }
    if path.starts_with("m-") {
        return Err(ActionPathError::ReservedM);
    }
    if path.starts_with("action-") {
        return Err(ActionPathError::ReservedAction);
    }
    if path == "new" {
        return Err(ActionPathError::ReservedNew);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_in_order() {
        for ok in [
            "a",
            "invoices",
            "a-b_c9",
            "new-thing",
            "mx",
            "actions",
            "news",
        ] {
            assert_eq!(check_path(ok), Ok(()), "{ok}");
        }
        for bad in [
            "", "Invoices", "1a", "-a", "_a", "a b", "a.b", "é", "aé", "a\n", "a/b",
        ] {
            assert_eq!(check_path(bad), Err(ActionPathError::Format), "{bad:?}");
        }
        assert_eq!(check_path("m-"), Err(ActionPathError::ReservedM));
        assert_eq!(check_path("m-x"), Err(ActionPathError::ReservedM));
        assert_eq!(check_path("action-1"), Err(ActionPathError::ReservedAction));
        assert_eq!(check_path("new"), Err(ActionPathError::ReservedNew));
        let max = "a".repeat(MAX_PATH_BYTES);
        assert_eq!(check_path(&max), Ok(()));
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        assert_eq!(
            check_path(&long),
            Err(ActionPathError::TooLong { len: 129, max: 128 })
        );
        // the bound applies before the pattern
        assert!(matches!(
            check_path(&"A".repeat(200)),
            Err(ActionPathError::TooLong { .. })
        ));
    }

    #[test]
    fn messages_are_references() {
        assert_eq!(
            ActionPathError::Format.to_string(),
            "The path should contain only lowercase alphanumeric characters, underscore, and dash, \
             and it should start with a letter."
        );
        assert_eq!(
            ActionPathError::TooLong { len: 129, max: 128 }.to_string(),
            "The path is 129 bytes long; at most 128 are allowed."
        );
        let error = erp_sdk::Error::from(ActionPathError::Duplicate);
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.to_string(),
            "Path to show in the URL must be unique! Please choose another one."
        );
    }
}
