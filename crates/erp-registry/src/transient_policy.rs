//! The cleanup policy of a transient model (package A).
//!
//! A transient (wizard) model's rows are meant to be short-lived.
//! It is metadata only: declaring a policy removes nothing, the
//! cleanup engine is a later package and is not active.

use crate::decl::{ModelDecl, ModelKind};
use erp_fields::{Error, Result};

/// Age threshold a transient model gets when it declares none: one hour.
pub const DEFAULT_TRANSIENT_MAX_AGE_SECONDS: u64 = 3_600;
/// Largest declarable age threshold: ten 365-day years, in seconds. A policy bound, not a
/// measured capacity limit.
pub const MAX_TRANSIENT_MAX_AGE_SECONDS: u64 = 315_360_000;
/// Largest declarable count threshold: `i64::MAX`, the largest count PostgreSQL can return.
pub const MAX_TRANSIENT_MAX_COUNT: u64 = 9_223_372_036_854_775_807;

/// Name of the age attribute in declarations (native, model files and WebAssembly descriptions).
pub const ATTR_MAX_AGE_SECONDS: &str = "transient_max_age_seconds";
/// Name of the count attribute in declarations.
pub const ATTR_MAX_COUNT: &str = "transient_max_count";

/// The validated cleanup thresholds of a transient model.
///
/// `None` means that threshold is disabled. A declaration spells "disabled" as `0`. The defaults
/// are an age of [`DEFAULT_TRANSIENT_MAX_AGE_SECONDS`] and no count limit. Both thresholds
/// disabled is allowed: the model is then never cleaned by policy, which
/// [`TransientPolicy::is_cleanup_disabled`] reports explicitly. The thresholds never disable the
/// batch, time and resource caps of the cleanup itself, and rows written in the last 300 seconds
/// are always protected.
///
/// ```
/// use erp_registry::TransientPolicy;
/// let policy = TransientPolicy::default();
/// assert_eq!(policy.max_age_seconds(), Some(3_600));
/// assert_eq!(policy.max_count(), None);
/// assert!(!policy.is_cleanup_disabled());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransientPolicy {
    max_age_seconds: Option<u64>,
    max_count: Option<u64>,
}

impl Default for TransientPolicy {
    fn default() -> Self {
        Self {
            max_age_seconds: Some(DEFAULT_TRANSIENT_MAX_AGE_SECONDS),
            max_count: None,
        }
    }
}

impl TransientPolicy {
    /// Rows older than this many seconds may be removed; `None` when the age threshold is
    /// disabled.
    pub const fn max_age_seconds(&self) -> Option<u64> {
        self.max_age_seconds
    }

    /// When the table holds more rows than this, the oldest surplus may be removed; `None` when
    /// the count threshold is disabled. Never above `i64::MAX`.
    pub const fn max_count(&self) -> Option<u64> {
        self.max_count
    }

    /// True when both thresholds are disabled: policy-driven cleanup never removes a row.
    pub const fn is_cleanup_disabled(&self) -> bool {
        self.max_age_seconds.is_none() && self.max_count.is_none()
    }
}

/// Compose the policy of the model `name` of kind `kind` from its declarations in method
/// resolution order (newest first). Each threshold comes from the newest declaration that states
/// it, so an in-place extension inherits what it does not state. Every stated value is validated,
/// including those a newer declaration overrides.
///
/// # Errors
///
/// `Programming` naming the declaration, the model and the attribute when a model that is not
/// transient states a threshold, or a value is above its bound.
pub(crate) fn resolve(
    name: &str,
    kind: ModelKind,
    mro: &[&'static ModelDecl],
) -> Result<Option<TransientPolicy>> {
    let stated = |attr: &str, pick: fn(&ModelDecl) -> Option<u64>, bound: u64| {
        let mut newest = None;
        for decl in mro {
            let Some(value) = pick(decl) else { continue };
            if kind != ModelKind::Transient {
                return Err(Error::programming(format!(
                    "{} sets {attr} on model '{name}', which is not a transient model: only \
                     transient models declare a cleanup policy",
                    decl.describe()
                )));
            }
            if value > bound {
                return Err(Error::programming(format!(
                    "{} sets {attr} = {value} on model '{name}': at most {bound} is accepted \
                     (0 disables the threshold)",
                    decl.describe()
                )));
            }
            newest.get_or_insert(value);
        }
        Ok(newest)
    };
    let age = stated(
        ATTR_MAX_AGE_SECONDS,
        |d| d.transient_max_age_seconds,
        MAX_TRANSIENT_MAX_AGE_SECONDS,
    )?;
    let count = stated(
        ATTR_MAX_COUNT,
        |d| d.transient_max_count,
        MAX_TRANSIENT_MAX_COUNT,
    )?;
    if kind != ModelKind::Transient {
        return Ok(None);
    }
    let nonzero = |v: u64| (v != 0).then_some(v);
    Ok(Some(TransientPolicy {
        max_age_seconds: match age {
            Some(v) => nonzero(v),
            None => Some(DEFAULT_TRANSIENT_MAX_AGE_SECONDS),
        },
        max_count: count.and_then(nonzero),
    }))
}
