//! Errors across the boundary: the contract's `boundary-error`, built in one place.
//!
//! `wit-bindgen` already gives `BoundaryError` its `Display` and `std::error::Error` impls (it is the
//! error type of the contract's results), so this module only constructs values.

use crate::{BoundaryError, BoundaryErrorKind};

/// Result of a guest-side operation.
pub type Result<T> = std::result::Result<T, BoundaryError>;

/// A boundary error of `kind` with `message`.
pub fn error(kind: BoundaryErrorKind, message: impl Into<String>) -> BoundaryError {
    BoundaryError {
        kind,
        message: message.into(),
    }
}
