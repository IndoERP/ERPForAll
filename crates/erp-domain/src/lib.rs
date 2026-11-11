//! Everything here is pure: no database, no registry access. Models are described through
//! the [`Schema`] trait; the environment-dependent levels (DYNAMIC_VALUES, FULL), SQL generation
//! and in-memory evaluation build on this crate in `erp-sql` and `erp-orm`.
//!
//! Errors carry an [`ErrorKind`] decided by provenance: domains parsed from external input
//! (`from_json`, `from_list`, `from_value`) give `Validation` errors (HTTP 422), domains built in
//! Rust give `Programming` errors (see [`DomainError`]). Untrusted input is bounded by
//! [`MAX_ITEMS`] and [`MAX_DEPTH`], and parsing is linear in its length. BASIC results can be
//! memoized under [`ModelContext::cache_key`].
//!
//! ```
//! use erp_domain::Domain;
//!
//! let domain = Domain::from_json(&serde_json::json!([
//!     "|", ["name", "ilike", "acme"], ["email", "=", false],
//! ]))?;
//! assert_eq!(domain.to_list().len(), 3);
//! let negated = !&domain;
//! assert_eq!(
//!     negated.to_json()?,
//!     serde_json::json!(["&", ["name", "not ilike", "acme"], ["email", "!=", false]]),
//! );
//! # Ok::<(), erp_domain::DomainError>(())
//! ```

#![allow(missing_docs)]

mod dates;
mod domain;
mod error;
mod operator;
mod optimize;
mod param;
pub mod query;
mod schema;
mod value;

pub use dates::{DateContext, parse_date};
pub use domain::{
    Condition, CustomDomain, Domain, DomainKind, FieldExpr, MAX_DEPTH, MAX_ITEMS, OptimizationLevel,
};
pub use error::{ConditionError, DomainError, ErrorKind, ErrorReason};
pub use operator::{Operator, UnknownOperator};
pub use param::{
    MAX_PATH_FIELD_LEN, MAX_USER_PATH_LEN, NowKind, Operation, PrincipalParam, UserPath,
};
pub use query::Granularity;
pub use schema::{ContextKey, FieldInfo, FieldType, ModelContext, Schema, TimeZone, Utc};
pub use value::{ExternalValue, Opaque, OpaqueKind, Value, ValueKind, ValueSet};
