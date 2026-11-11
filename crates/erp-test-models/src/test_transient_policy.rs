//! Fixture models for the transient cleanup policy (package A):
//! transient models with the default policy, a declared one, a disabling zero, an in-place
//! extension that overrides one threshold, and a regular model without any policy. The
//! equivalence test compares the native registry with the models read back from the addon's
//! `describe()` description (the WebAssembly route).

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_transient_policy");

/// Transient model with no policy attribute: the defaults apply (1 hour, no count limit).
#[model(
    name = "test_tp.defaults",
    description = "Policy defaults",
    addon = "test_transient_policy",
    kind = "transient"
)]
pub struct Defaults {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Transient model with both thresholds declared.
#[model(
    name = "test_tp.tuned",
    description = "Policy declared",
    addon = "test_transient_policy",
    kind = "transient",
    transient_max_age_seconds = 7_200,
    transient_max_count = 50
)]
pub struct Tuned {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Transient model whose age threshold is disabled by an explicit zero.
#[model(
    name = "test_tp.count_only",
    description = "Count only",
    addon = "test_transient_policy",
    kind = "transient",
    transient_max_age_seconds = 0,
    transient_max_count = 25
)]
pub struct CountOnly {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// In-place extension of [`Tuned`]: overrides the count only, inherits the age.
#[extend(Tuned, addon = "test_transient_policy", transient_max_count = 10)]
pub struct TunedExt {
    #[field(char)]
    pub note: Option<Arc<str>>,
}

/// Regular model: no policy.
#[model(
    name = "test_tp.plain",
    description = "Plain",
    addon = "test_transient_policy"
)]
pub struct Plain {
    #[field(char)]
    pub name: Option<Arc<str>>,
}
