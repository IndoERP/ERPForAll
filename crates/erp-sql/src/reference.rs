//! The reference-query carrier (Amendment 1): a condition value that lets a
//! `many2one_reference` column be tested against the records of one named model.
//!
//! A `many2one_reference` field (`res_id` next to a `model` name column) has no fixed comodel, so
//! an `any!` sub-domain has nothing to be written against. [`ReferenceQuery`] supplies it: the
//! target model and the predicate to apply on that model. It is built by `erp-security` when it
//! expands a declared rule operator (`thread_access`), travels in a domain as the value of an
//! `any!` condition on the column, and is compiled with the sub-query shape of a many2one `any`:
//!
//! ```sql
//! "t"."res_id" IN (SELECT "crm_lead"."id" FROM "crm_lead" WHERE <predicate>)
//! ```
//!
//! The carrier does **not** pin the column's model: the caller ANDs `model = '<target>'`, as the
//! expansion does. It holds no principal value, only a [`Domain`] whose principal leaves are
//! parameters, so one value is safe to share between transactions and principals.
//!
//! # Passes must descend
//!
//! Every pass over an access predicate that rewrites relational sub-domains (search methods,
//! hierarchy resolution, FULL optimization, completion) has to descend into [`ReferenceQuery::sub`]
//! with [`ReferenceQuery::target`] as the model, and the FULL optimizer marks the carrier with
//! [`ReferenceQuery::optimized`]. A carrier that reaches the planner unoptimized, on another field
//! kind, or negated is a programming error naming Amendment 1: a pass that forgot to
//! descend fails loudly and never lets rows through.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use erp_domain::{Domain, ExternalValue, Opaque, OpaqueKind, Value};
use erp_registry::ModelId;

/// "Records of `target` that satisfy `sub`", as the value of an `any!` condition on a
/// `many2one_reference` column. See the [module documentation](self).
#[non_exhaustive]
pub struct ReferenceQuery {
    target: ModelId,
    sub: Domain,
    optimized: bool,
}

impl ReferenceQuery {
    /// A query for the records of `target` that satisfy `sub`, not yet optimized.
    pub fn new(target: ModelId, sub: Domain) -> Self {
        Self {
            target,
            sub,
            optimized: false,
        }
    }

    /// The model whose records the column is tested against.
    pub fn target(&self) -> ModelId {
        self.target
    }

    /// The predicate on [`ReferenceQuery::target`].
    pub fn sub(&self) -> &Domain {
        &self.sub
    }

    /// Whether the FULL optimizer has completed the predicate. The planner refuses a carrier
    /// that is not.
    pub fn is_optimized(&self) -> bool {
        self.optimized
    }

    /// The same query over a rewritten predicate, keeping the optimization mark: for a pass that
    /// rewrites the predicate in place (search methods, hierarchy resolution, completion).
    #[must_use]
    pub fn with_sub(&self, sub: Domain) -> Self {
        Self {
            target: self.target,
            sub,
            optimized: self.optimized,
        }
    }

    /// The same query over `sub`, the FULL-optimized predicate, marked optimized.
    #[must_use]
    pub fn optimized(&self, sub: Domain) -> Self {
        Self {
            target: self.target,
            sub,
            optimized: true,
        }
    }

    /// The query as a condition value.
    pub fn into_value(self) -> Value {
        Value::Opaque(Opaque::new(Arc::new(self)))
    }

    /// The reference query held by a condition value, if it is one.
    pub fn of(value: &Value) -> Option<&Self> {
        match value {
            Value::Opaque(o) => o.get().as_any().downcast_ref::<Self>(),
            _ => None,
        }
    }
}

impl fmt::Debug for ReferenceQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "reference_query[model #{}{}: {:?}]",
            self.target.index(),
            if self.optimized { ", optimized" } else { "" },
            self.sub
        )
    }
}

impl ExternalValue for ReferenceQuery {
    fn kind(&self) -> OpaqueKind {
        OpaqueKind::Query
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
