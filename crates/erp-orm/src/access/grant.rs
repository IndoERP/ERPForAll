//! Grant sources (DN §5): domains ORed with the permissions of a
//! (model, operation), so share grants and link tokens widen access inside the SQL predicate
//! instead of bypassing it. Restrictions still AND over them and the model-level check still
//! applies. implements the sources; the policy snapshot ORs them in at its combine
//! stage.

use std::fmt;

use erp_domain::{Domain, DomainKind, PrincipalParam, Value};
use erp_fields::{Error, Result};
use erp_registry::ModelSpec;

use super::Operation;
use super::ctx::AccessCtx;

/// A grant source: contributes, per (model, operation, group set, shape), a principal-independent
/// grant domain whose principal values are parameters (R-M1).
///
/// Register one in [`GRANT_SOURCES`] (through `erp-sdk`) so every policy snapshot sees it.
pub trait GrantSource: Send + Sync + 'static {
    /// A stable name (`share`, `link`), unique among the linked sources.
    fn name(&self) -> &'static str;

    /// Whether the source contributes to `op` on `model` for principals like `ctx` (a function of
    /// the memo key: it sees no principal values).
    ///
    /// # Errors
    ///
    /// Any error denies the operation with it (nothing narrows or widens silently, R-S15).
    fn applies(&self, ctx: &AccessCtx<'_>, model: &ModelSpec, op: Operation) -> Result<bool>;

    /// The grant domain of `op` on `model`.
    ///
    /// # Errors
    ///
    /// Any error denies the operation with it.
    fn grant_domain(
        &self,
        ctx: &AccessCtx<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<GrantDomain>;
}

impl fmt::Debug for dyn GrantSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GrantSource({})", self.name())
    }
}

/// Grant sources linked into the binary.
#[linkme::distributed_slice]
pub static GRANT_SOURCES: [&'static dyn GrantSource];

/// Where a grant domain came from (explain).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantRef {
    source: &'static str,
}

impl GrantRef {
    /// The grant of the source named `source`.
    pub const fn new(source: &'static str) -> Self {
        Self { source }
    }

    /// The source's name.
    pub fn source(&self) -> &'static str {
        self.source
    }
}

/// A grant domain: a [`Domain`] whose only principal parameters are `Uid`, `Now` and `GrantIds`
/// (a grant is bound per statement, never compiled for one principal).
///
/// ```
/// use erp_domain::{Domain, Operator, PrincipalParam, Value};
/// use erp_orm::{GrantDomain, GrantRef};
/// let mine = Domain::condition("grantee_id", Operator::Eq, Value::Param(PrincipalParam::Uid))?;
/// assert!(GrantDomain::new(mine, GrantRef::new("share")).is_ok());
/// let company = Domain::condition(
///     "company_id",
///     Operator::In,
///     Value::Param(PrincipalParam::CompanyIds),
/// )?;
/// assert_eq!(
///     GrantDomain::new(company, GrantRef::new("share")).unwrap_err().message(),
///     "grant source 'share' uses the principal parameter company_ids: grant domains may use \
///      user.id, the current time and grant_ids only"
/// );
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct GrantDomain {
    domain: Domain,
    source: GrantRef,
}

impl GrantDomain {
    /// The grant domain `domain` of `source`.
    ///
    /// # Errors
    ///
    /// `Programming` when the domain uses another principal parameter than `Uid`, `Now` or
    /// `GrantIds` (companies and user paths would make a grant depend on more than the
    /// grantee).
    pub fn new(domain: Domain, source: GrantRef) -> Result<Self> {
        if let Some(param) = first_disallowed(&domain) {
            return Err(Error::programming(format!(
                "grant source '{}' uses the principal parameter {}: grant domains may use \
                 user.id, the current time and grant_ids only",
                source.source(),
                param.symbol()
            )));
        }
        Ok(Self { domain, source })
    }

    /// The domain.
    pub fn domain(&self) -> &Domain {
        &self.domain
    }

    /// Its source.
    pub fn source(&self) -> &GrantRef {
        &self.source
    }
}

fn first_disallowed(domain: &Domain) -> Option<PrincipalParam> {
    match domain.kind() {
        DomainKind::True | DomainKind::False | DomainKind::Custom(_) => None,
        DomainKind::Not(child) => first_disallowed(child),
        DomainKind::And(children) | DomainKind::Or(children) => {
            children.iter().find_map(first_disallowed)
        }
        DomainKind::Condition(condition) => disallowed_in(condition.value()),
    }
}

fn disallowed_in(value: &Value) -> Option<PrincipalParam> {
    match value {
        Value::Param(
            PrincipalParam::Uid | PrincipalParam::Now(_) | PrincipalParam::GrantIds { .. },
        ) => None,
        Value::Param(other) => Some(other.clone()),
        Value::List(items) => items.iter().find_map(disallowed_in),
        Value::Set(set) => set.iter().find_map(disallowed_in),
        Value::Domain(sub) => first_disallowed(sub),
        _ => None,
    }
}
