// SPDX-License-Identifier: MIT OR Apache-2.0
//! The blind index hasher of encrypted fields (`blind_index = true`).
//!
//! The SQL compiler turns `in` / `not in` on a blind-indexed field into a comparison of its
//! companion column with the blind indexes of the values. The keys never reach this crate: the
//! ORM installs, once per process with its field keys, the function that computes a blind index
//! (`erp_orm::field_encryption`). Without it a condition on such a field is refused.

use std::sync::OnceLock;

use erp_fields::{Error, Result};

/// Computes the blind index of the clear text of `model.field`.
pub type BlindHasher = fn(model: &str, field: &str, clear: &str) -> Result<String>;

static HASHER: OnceLock<BlindHasher> = OnceLock::new();

/// Install the hasher of this process. A second call keeps the first hasher.
pub fn install(hasher: BlindHasher) {
    let _ = HASHER.set(hasher);
}

/// The blind index of `clear` for `model.field`.
///
/// # Errors
///
/// `Programming` when no hasher is installed; the hasher's error.
pub fn hash(model: &str, field: &str, clear: &str) -> Result<String> {
    let hasher = HASHER.get().ok_or_else(|| {
        Error::programming(format!(
            "the field {field} of {model} has a blind index, but this process has no field keys \
             to compute it"
        ))
    })?;
    hasher(model, field, clear)
}
