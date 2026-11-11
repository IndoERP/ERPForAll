//! Database-authoritative feature admission and bounded binary declarations.

mod types;
pub use types::{FeatureKey, FeatureSet, FeatureVersion};

mod operator;
pub use operator::{FEATURE_REGISTRY_LOCK, FeatureRollout, FeatureStatus, LegacyEnrollment};

mod grant;

mod binding;
pub use binding::{FeatureBinding, FeatureSnapshot};
