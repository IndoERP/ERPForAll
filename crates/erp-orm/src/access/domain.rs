//! [`AccessDomain`]: the compiled record predicate of (model, operation) for one memo key.

use std::fmt;
use std::sync::{Arc, LazyLock};

use erp_domain::Domain;
use erp_fields::{Error, Result};
use erp_registry::{FieldId, ModelId};

use super::grant::GrantRef;

/// An `ir.access` rule a predicate came from (explain, errors, the build report).
///
/// ```
/// use erp_orm::RuleRef;
/// let rule = RuleRef::new(7, "Restrict to humans")?.with_xmlid("test_access.rule_humans");
/// assert_eq!(rule.to_string(), "Restrict to humans (7)");
/// assert!(RuleRef::new(0, "x").is_err());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuleRef {
    id: i64,
    name: Arc<str>,
    xmlid: Option<Arc<str>>,
    origin: RuleOrigin,
}

/// Where a rule comes from (design `m3-company-a1` §6.7): data (a file, an import, the web client)
/// or a company restriction generated from a model's `company_access` declaration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuleOrigin {
    /// An ordinary row.
    Data,
    /// The restriction generated for `model` by its `company_access` declaration, made by addon
    /// `addon` at `file:line`.
    Generated {
        /// The restricted model.
        model: Arc<str>,
        /// The declaring addon.
        addon: Arc<str>,
        /// The declaring source file.
        file: Arc<str>,
        /// The declaring source line.
        line: u32,
    },
}

impl RuleRef {
    /// The rule `id` (an `ir.access` row id, positive) named `name`.
    ///
    /// # Errors
    ///
    /// `Programming` when `id` is not positive.
    pub fn new(id: i64, name: impl Into<Arc<str>>) -> Result<Self> {
        if id <= 0 {
            return Err(Error::programming(format!(
                "an access rule id is a positive row id, not {id}"
            )));
        }
        Ok(Self {
            id,
            name: name.into(),
            xmlid: None,
            origin: RuleOrigin::Data,
        })
    }

    /// The same rule with its origin.
    #[must_use]
    pub fn with_origin(mut self, origin: RuleOrigin) -> Self {
        self.origin = origin;
        self
    }

    /// Where the rule comes from.
    pub fn origin(&self) -> &RuleOrigin {
        &self.origin
    }

    /// The same rule with its external id.
    #[must_use]
    pub fn with_xmlid(mut self, xmlid: impl Into<Arc<str>>) -> Self {
        self.xmlid = Some(xmlid.into());
        self
    }

    /// The row id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The rule's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The external id, when it has one.
    pub fn xmlid(&self) -> Option<&str> {
        self.xmlid.as_deref()
    }
}

impl fmt::Display for RuleRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.id)
    }
}

/// The record predicate of one (model, operation, group set, shape) (DN §2.2): a [`Domain`] whose
/// leaves may be principal parameters ([`erp_domain::Value::Param`]), with
///
/// - `reads`: every (model, field) the predicate reads, including the fields of a
///   [`erp_domain::PrincipalParam::UserPath`] and of sub-selected models (R-S5): their pending
///   writes are flushed before a record check, and writing one drops the record memo;
/// - `rules` / `grants`: where it came from (explain);
/// - `constant`: `Some(b)` when the domain is the constant `b` (no statement for `FALSE`);
/// - `reads_own_table`: the predicate sub-selects the model's own table, so a create cannot fuse
///   the check into `INSERT … RETURNING` (the sub-select would not see the new rows).
///
/// Cheap to clone (shared).
///
/// ```
/// use erp_domain::{Domain, Operator, PrincipalParam, Value};
/// use erp_orm::AccessDomain;
/// let mine = Domain::condition("user_id", Operator::Eq, Value::Param(PrincipalParam::Uid))?;
/// let access = AccessDomain::builder(mine).build();
/// assert_eq!(access.constant(), None);
/// assert!(AccessDomain::deny_all().is_false());
/// assert!(AccessDomain::allow_all().is_true());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Clone)]
pub struct AccessDomain(Arc<Inner>);

struct Inner {
    domain: Domain,
    reads: Vec<(ModelId, FieldId)>,
    rules: Vec<RuleRef>,
    grants: Vec<GrantRef>,
    constant: Option<bool>,
    reads_own_table: bool,
}

static ALL: LazyLock<AccessDomain> = LazyLock::new(|| {
    AccessDomain(Arc::new(Inner {
        domain: Domain::TRUE,
        reads: Vec::new(),
        rules: Vec::new(),
        grants: Vec::new(),
        constant: Some(true),
        reads_own_table: false,
    }))
});

static DENY: LazyLock<AccessDomain> = LazyLock::new(|| {
    AccessDomain(Arc::new(Inner {
        domain: Domain::FALSE,
        reads: Vec::new(),
        rules: Vec::new(),
        grants: Vec::new(),
        constant: Some(false),
        reads_own_table: false,
    }))
});

impl AccessDomain {
    /// Every record (`TRUE`), from no rule. Shared instance.
    pub fn allow_all() -> Self {
        ALL.clone()
    }

    /// No record (`FALSE`), from no rule. Shared instance.
    pub fn deny_all() -> Self {
        DENY.clone()
    }

    /// Start from the predicate `domain`.
    pub fn builder(domain: Domain) -> AccessDomainBuilder {
        AccessDomainBuilder {
            domain,
            reads: Vec::new(),
            rules: Vec::new(),
            grants: Vec::new(),
            reads_own_table: false,
        }
    }

    /// The predicate.
    pub fn domain(&self) -> &Domain {
        &self.0.domain
    }

    /// The (model, field) pairs it reads, each once.
    pub fn reads(&self) -> &[(ModelId, FieldId)] {
        &self.0.reads
    }

    /// The rules it came from.
    pub fn rules(&self) -> &[RuleRef] {
        &self.0.rules
    }

    /// The grant domains ORed in.
    pub fn grants(&self) -> &[GrantRef] {
        &self.0.grants
    }

    /// `Some(b)` when the predicate is the constant `b`.
    pub fn constant(&self) -> Option<bool> {
        self.0.constant
    }

    /// `TRUE`: restricts nothing.
    pub fn is_true(&self) -> bool {
        self.0.constant == Some(true)
    }

    /// `FALSE`: hides every record.
    pub fn is_false(&self) -> bool {
        self.0.constant == Some(false)
    }

    /// Whether the predicate sub-selects its own model's table (no `INSERT … RETURNING` fusion).
    pub fn reads_own_table(&self) -> bool {
        self.0.reads_own_table
    }

    /// Whether the predicate reads `field` of `model`.
    pub fn reads_field(&self, model: ModelId, field: FieldId) -> bool {
        self.0.reads.contains(&(model, field))
    }
}

impl fmt::Debug for AccessDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessDomain")
            .field("domain", &self.0.domain)
            .field("reads", &self.0.reads.len())
            .field("rules", &self.0.rules)
            .field("grants", &self.0.grants)
            .field("reads_own_table", &self.0.reads_own_table)
            .finish()
    }
}

/// Builds an [`AccessDomain`] ([`AccessDomain::builder`]).
#[derive(Debug)]
#[must_use = "call `.build()`"]
pub struct AccessDomainBuilder {
    domain: Domain,
    reads: Vec<(ModelId, FieldId)>,
    rules: Vec<RuleRef>,
    grants: Vec<GrantRef>,
    reads_own_table: bool,
}

impl AccessDomainBuilder {
    /// Add fields the predicate reads (duplicates are kept once).
    pub fn reads(mut self, reads: impl IntoIterator<Item = (ModelId, FieldId)>) -> Self {
        for read in reads {
            if !self.reads.contains(&read) {
                self.reads.push(read);
            }
        }
        self
    }

    /// Add a rule the predicate came from.
    pub fn rule(mut self, rule: RuleRef) -> Self {
        self.rules.push(rule);
        self
    }

    /// Add rules the predicate came from.
    pub fn rules(mut self, rules: impl IntoIterator<Item = RuleRef>) -> Self {
        self.rules.extend(rules);
        self
    }

    /// Add a grant ORed in.
    pub fn grant(mut self, grant: GrantRef) -> Self {
        self.grants.push(grant);
        self
    }

    /// Mark that the predicate sub-selects its own model's table.
    pub fn reads_own_table(mut self, yes: bool) -> Self {
        self.reads_own_table = yes;
        self
    }

    /// The access domain; `constant` follows the domain.
    pub fn build(self) -> AccessDomain {
        let constant = if self.domain.is_true() {
            Some(true)
        } else if self.domain.is_false() {
            Some(false)
        } else {
            None
        };
        // A constant predicate without provenance reuses its lazy shared instance.
        // Allocation cost, including first initialization, is measured only at milestone end.
        if self.reads.is_empty()
            && self.rules.is_empty()
            && self.grants.is_empty()
            && !self.reads_own_table
        {
            match constant {
                Some(true) => return AccessDomain::allow_all(),
                Some(false) => return AccessDomain::deny_all(),
                None => {}
            }
        }
        AccessDomain(Arc::new(Inner {
            domain: self.domain,
            reads: self.reads,
            rules: self.rules,
            grants: self.grants,
            constant,
            reads_own_table: self.reads_own_table,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_and_deny_domains_preserve_constant_semantics() {
        for (access, expected) in [
            (AccessDomain::allow_all(), true),
            (AccessDomain::deny_all(), false),
            (AccessDomain::builder(Domain::TRUE).build(), true),
            (AccessDomain::builder(Domain::FALSE).build(), false),
        ] {
            assert_eq!(access.constant(), Some(expected));
            assert_eq!(access.is_true(), expected);
            assert_eq!(access.is_false(), !expected);
        }
    }

    #[test]
    #[ignore = "DEFERRED_PERFORMANCE: shared allocation identity at milestone end; boolean and security provenance run ordinarily"]
    fn shared_allow_and_deny_domains_are_shared() {
        let a = AccessDomain::allow_all();
        let b = AccessDomain::allow_all();
        assert!(Arc::ptr_eq(&a.0, &b.0));
        assert!(a.is_true());
        assert_eq!(a.constant(), Some(true));

        let c = AccessDomain::builder(Domain::TRUE).build();
        assert!(Arc::ptr_eq(&a.0, &c.0));

        let d1 = AccessDomain::deny_all();
        let d2 = AccessDomain::deny_all();
        assert!(Arc::ptr_eq(&d1.0, &d2.0));
        assert!(d1.is_false());
        assert_eq!(d1.constant(), Some(false));

        let d3 = AccessDomain::builder(Domain::FALSE).build();
        assert!(Arc::ptr_eq(&d1.0, &d3.0));
    }

    #[test]
    fn a_constant_domain_with_provenance_is_not_shared() -> Result<()> {
        let all = AccessDomain::allow_all();
        let deny = AccessDomain::deny_all();
        let rule = RuleRef::new(1, "rule")?;
        for constant in [Domain::TRUE, Domain::FALSE] {
            let shared = if constant.is_true() { &all } else { &deny };
            let built = [
                AccessDomain::builder(constant.clone())
                    .rule(rule.clone())
                    .build(),
                AccessDomain::builder(constant.clone())
                    .grant(GrantRef::new("share"))
                    .build(),
                AccessDomain::builder(constant.clone())
                    .reads_own_table(true)
                    .build(),
            ];
            for access in &built {
                assert!(!Arc::ptr_eq(&shared.0, &access.0), "{access:?}");
                assert_eq!(access.constant(), shared.constant());
            }
            assert_eq!(built[0].rules(), std::slice::from_ref(&rule));
            assert_eq!(built[1].grants(), &[GrantRef::new("share")]);
            assert!(built[2].reads_own_table());
        }
        // the shared instances carry no provenance
        assert!(all.rules().is_empty() && all.grants().is_empty() && !all.reads_own_table());
        assert!(deny.rules().is_empty() && deny.grants().is_empty() && !deny.reads_own_table());
        Ok(())
    }
}
