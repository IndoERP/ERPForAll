//! Test policies (design note §8): access policies for engine tests that do not need
//! `erp-security`.
//!
//! - [`DomainPolicy`]: a table of record predicates per (model, operation), with principal
//!   parameters (`user.id`, `company_ids`, …); every other pair is allowed or denied as a whole.
//!   It runs the `#[access_domain]` override chains when given them.
//! - [`RulesPolicy`]: a policy written as two functions ([`TestRules`]), the shape of the v1
//!   policy trait that the tests of earlier slices implement.
//!
//! Both are snapshots of their own (their own [`SnapshotId`], origin `Shared`) and decide without
//! a group graph: `groups(direct)` is the sorted direct ids, no group set is of a share type, every
//! capability follows the policy's default. A [`DomainPolicy`] knows the external ids it is given
//! ([`DomainPolicyBuilder::group_xmlid`]) for `AccessCtx::has_group` in overrides.

use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{Error, Result};
use erp_orm::FieldMask;
use erp_orm::{
    AccessCtx, AccessCtxParts, AccessDecision, AccessDomain, AccessDomainChains, AccessPolicy,
    Capability, ElevationKind, ExplainRequest, Explanation, GroupId, GroupLookup, GroupSet,
    GroupSetInterner, InternedGroups, Operation, PolicyGeneration, PrincipalBinding,
    QualifiedXmlId, SnapshotId, SnapshotOrigin, SnapshotRef,
};
use erp_registry::{ModelSpec, Registry};
use rustc_hash::FxHashMap;

/// Group permissions per (model, operation).
type GroupRules = FxHashMap<(Box<str>, Operation), Vec<(GroupId, Domain)>>;

/// Most group sets a test policy interns before it starts over.
const INTERNER_CAPACITY: usize = 1_024;

/// Record rules written as two functions (the v1 policy shape). The engine never asks for a
/// superuser (`Full`) binding.
pub trait TestRules: Send + Sync + 'static {
    /// `Ok(())` when allowed, else an `Access` error.
    ///
    /// # Errors
    ///
    /// The refusal.
    fn check_model(&self, b: &PrincipalBinding<'_>, model: &ModelSpec, op: Operation)
    -> Result<()>;

    /// The record predicate (`FALSE` hides everything, `TRUE` restricts nothing).
    ///
    /// # Errors
    ///
    /// A predicate that cannot be built.
    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain>;
}

/// A policy snapshot over [`TestRules`].
#[derive(Debug)]
pub struct RulesPolicy<R> {
    rules: R,
    id: SnapshotId,
    interner: GroupSetInterner,
}

impl<R: TestRules> RulesPolicy<R> {
    /// The snapshot of `rules`.
    ///
    /// # Errors
    ///
    /// `ResourceExhausted` when the process ran out of snapshot ids.
    pub fn new(rules: R) -> Result<Self> {
        Ok(Self {
            rules,
            id: SnapshotId::next()?,
            interner: GroupSetInterner::new(INTERNER_CAPACITY)?,
        })
    }
}

impl<R: TestRules> AccessPolicy for RulesPolicy<R> {
    /// An immutable snapshot: what it resolves never changes.
    fn group_refs_are_stable(&self) -> bool {
        true
    }

    fn generation(&self) -> PolicyGeneration {
        PolicyGeneration::default()
    }

    fn snapshot(&self) -> SnapshotRef {
        SnapshotRef::new(self.id, SnapshotOrigin::Shared)
    }

    fn needs_resolved_principals(&self) -> bool {
        false
    }

    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups> {
        Ok(InternedGroups::new(self.interner.intern(groups)?, false))
    }

    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        self.rules.check_model(b, model, op)
    }

    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<AccessDomain> {
        Ok(AccessDomain::builder(self.rules.access_domain(b, model, op)?).build())
    }

    /// Test rules know no field groups.
    fn field_access(
        &self,
        _b: &PrincipalBinding<'_>,
        _model: &ModelSpec,
    ) -> Result<Arc<FieldMask>> {
        Ok(FieldMask::shared_all())
    }

    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet> {
        Ok(GroupSet::new(direct.iter().copied()))
    }

    fn allowing_groups(&self, _model: &ModelSpec, _op: Operation) -> Result<Vec<GroupId>> {
        Ok(Vec::new())
    }

    /// Test rules know no capability.
    fn has_capability(&self, _b: &PrincipalBinding<'_>, cap: &Capability) -> Result<bool> {
        Err(Error::programming(format!(
            "the test rules policy maps no capability ('{}')",
            cap.name()
        )))
    }

    fn explain(&self, req: &ExplainRequest<'_>) -> Result<Explanation> {
        if !req.record_ids().is_empty() {
            return explain_records(req, self.generation());
        }
        let decision = match self.rules.check_model(req.binding(), req.model(), req.op()) {
            Ok(()) => AccessDecision::Allow,
            Err(e) if e.kind() == erp_fields::ErrorKind::Access => AccessDecision::Deny,
            Err(e) => return Err(e),
        };
        Ok(Explanation::new(decision, self.generation(), Vec::new()))
    }
}

/// The record-level half of a test policy's explanation (the engine asks about the records a
/// check denied, then again with the evidence it gathered): a test policy has no rules to
/// evaluate or blame, so it asks for no column, and its text names the first denied record that
/// exists: `You are not allowed to <verb> '<description>' (<model>) records.\n\nRecords:
/// <model>(<id>,), User: <uid>`.
fn explain_records(req: &ExplainRequest<'_>, generation: PolicyGeneration) -> Result<Explanation> {
    let deny = Explanation::new(AccessDecision::Deny, generation, Vec::new());
    let Some(evidence) = req.record_evidence() else {
        return Ok(deny);
    };
    let row = evidence
        .rows()
        .first()
        .ok_or_else(|| Error::programming("access evidence without a record"))?;
    let verb = match req.op() {
        Operation::Read => "access",
        Operation::Write => "modify",
        Operation::Create => "create",
        Operation::Unlink => "delete",
    };
    let model = req.model();
    Ok(deny.with_message(format!(
        "You are not allowed to {verb} '{}' ({}) records.\n\nRecords: {}({},), User: {}",
        model.description,
        model.name,
        model.name,
        row.id(),
        req.binding().principal().subject()
    )))
}

/// What a [`DomainPolicy`] decides for a (model, operation) it has no predicate for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Otherwise {
    /// Every record (`TRUE`).
    Allow,
    /// No record, and the model-level check refuses (`FALSE`).
    Deny,
}

/// A policy of record predicates per (model, operation), with principal parameters.
///
/// ```
/// use erp_domain::{Domain, Operator, PrincipalParam, Value};
/// use erp_orm::Operation;
/// use erp_testkit::{DomainPolicy, Otherwise};
/// let mine = Domain::condition("user_id", Operator::Eq, Value::Param(PrincipalParam::Uid))?;
/// let policy = DomainPolicy::builder(Otherwise::Allow)
///     .rule("test.note", Operation::Read, mine)
///     .deny("test.secret", Operation::Read)
///     .build()?;
/// # let _ = policy;
/// # Ok::<(), erp_fields::Error>(())
/// ```
pub struct DomainPolicy {
    rules: FxHashMap<(Box<str>, Operation), Domain>,
    group_rules: GroupRules,
    xmlids: XmlIds,
    otherwise: Otherwise,
    resolved: bool,
    origin: SnapshotOrigin,
    chains: Option<(Arc<Registry>, AccessDomainChains)>,
    id: SnapshotId,
    interner: GroupSetInterner,
}

impl std::fmt::Debug for DomainPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainPolicy")
            .field("rules", &self.rules)
            .field("otherwise", &self.otherwise)
            .field("snapshot", &self.id)
            .finish_non_exhaustive()
    }
}

/// Builds a [`DomainPolicy`].
#[must_use = "call `.build()`"]
pub struct DomainPolicyBuilder {
    rules: FxHashMap<(Box<str>, Operation), Domain>,
    group_rules: GroupRules,
    xmlids: Vec<(Box<str>, GroupId)>,
    otherwise: Otherwise,
    resolved: bool,
    origin: SnapshotOrigin,
    chains: Option<(Arc<Registry>, AccessDomainChains)>,
}

impl DomainPolicy {
    /// A policy deciding `otherwise` for every (model, operation) without a predicate.
    pub fn builder(otherwise: Otherwise) -> DomainPolicyBuilder {
        DomainPolicyBuilder {
            rules: FxHashMap::default(),
            group_rules: FxHashMap::default(),
            xmlids: Vec::new(),
            otherwise,
            resolved: false,
            origin: SnapshotOrigin::Shared,
            chains: None,
        }
    }
}

impl DomainPolicyBuilder {
    /// The predicate of `op` on `model` (a later call for the same pair replaces it).
    pub fn rule(mut self, model: &str, op: Operation, domain: Domain) -> Self {
        self.rules.insert((Box::from(model), op), domain);
        self
    }

    /// The same predicate for every operation of `model`.
    pub fn rule_all(mut self, model: &str, domain: &Domain) -> Self {
        for op in Operation::ALL {
            self.rules.insert((Box::from(model), op), domain.clone());
        }
        self
    }

    /// Refuse `op` on `model` (the predicate `FALSE`).
    pub fn deny(self, model: &str, op: Operation) -> Self {
        self.rule(model, op, Domain::FALSE)
    }

    pub fn group_rule(
        mut self,
        model: &str,
        op: Operation,
        group: GroupId,
        domain: Domain,
    ) -> Self {
        self.group_rules
            .entry((Box::from(model), op))
            .or_default()
            .push((group, domain));
        self
    }

    /// The external id `xmlid` (`module.name`) of `group`, for `AccessCtx::has_group` in the
    /// overrides ([`DomainPolicyBuilder::overrides`]); an override asking for an id the policy
    /// was not given fails with `Programming`.
    pub fn group_xmlid(mut self, xmlid: &str, group: GroupId) -> Self {
        self.xmlids.push((Box::from(xmlid), group));
        self
    }

    /// A snapshot built privately for one transaction (origin `Private`, design note §2.3).
    pub fn private(mut self) -> Self {
        self.origin = SnapshotOrigin::Private;
        self
    }

    /// Decide like a compiled policy: refuse principals no resolver read.
    pub fn needs_resolved_principals(mut self) -> Self {
        self.resolved = true;
        self
    }

    /// Run the `#[access_domain]` chains of `registry` on every predicate.
    pub fn overrides(mut self, registry: Arc<Registry>, chains: AccessDomainChains) -> Self {
        self.chains = Some((registry, chains));
        self
    }

    /// The policy.
    ///
    /// # Errors
    ///
    /// `ResourceExhausted` when the process ran out of snapshot ids; `Programming` when an
    /// external id does not parse or names two groups.
    pub fn build(self) -> Result<DomainPolicy> {
        let mut xmlids = FxHashMap::default();
        for (xmlid, group) in self.xmlids {
            let parsed = QualifiedXmlId::parse(&xmlid)?;
            if let Some(previous) = xmlids.insert(parsed, group)
                && previous != group
            {
                return Err(Error::programming(format!(
                    "the external id '{xmlid}' names two groups: {previous} and {group}"
                )));
            }
        }
        Ok(DomainPolicy {
            rules: self.rules,
            group_rules: self.group_rules,
            xmlids: XmlIds(xmlids),
            otherwise: self.otherwise,
            resolved: self.resolved,
            origin: self.origin,
            chains: self.chains,
            id: SnapshotId::next()?,
            interner: GroupSetInterner::new(INTERNER_CAPACITY)?,
        })
    }
}

/// The external ids a test policy was given.
#[derive(Debug, Default)]
struct XmlIds(FxHashMap<QualifiedXmlId, GroupId>);

impl GroupLookup for XmlIds {
    fn group_id(&self, xmlid: &QualifiedXmlId) -> Result<Option<GroupId>> {
        Ok(self.0.get(xmlid).copied())
    }
}

impl DomainPolicy {
    fn base(&self, groups: &GroupSet, model: &ModelSpec, op: Operation) -> Domain {
        let key = (Box::from(model.name), op);
        let permission = self.group_rules.get(&key).map(|rules| {
            Domain::or(
                rules
                    .iter()
                    .filter(|(g, _)| groups.contains(*g))
                    .map(|(_, d)| d.clone()),
            )
        });
        match (permission, self.rules.get(&key)) {
            (Some(permission), Some(rule)) => permission & rule.clone(),
            (Some(permission), None) => permission,
            (None, Some(rule)) => rule.clone(),
            (None, None) => Domain::constant(self.otherwise == Otherwise::Allow),
        }
    }

    fn domain(&self, b: &PrincipalBinding<'_>, model: &ModelSpec, op: Operation) -> Result<Domain> {
        let base = self.base(b.resolution().groups(), model, op);
        let Some((registry, chains)) = &self.chains else {
            return Ok(base);
        };
        let ctx = AccessCtx::new(AccessCtxParts::new(
            b.resolution().groups(),
            b.shape().class(),
            ElevationKind::of(b.shape().elevation()),
            model,
            registry,
            &self.xmlids,
        ));
        chains.apply(&ctx, op, &base)
    }
}

impl AccessPolicy for DomainPolicy {
    /// An immutable snapshot: what it resolves never changes.
    fn group_refs_are_stable(&self) -> bool {
        true
    }

    fn generation(&self) -> PolicyGeneration {
        PolicyGeneration::default()
    }

    fn snapshot(&self) -> SnapshotRef {
        SnapshotRef::new(self.id, self.origin)
    }

    fn needs_resolved_principals(&self) -> bool {
        self.resolved
    }

    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups> {
        Ok(InternedGroups::new(self.interner.intern(groups)?, false))
    }

    /// `access_domain` is not `FALSE`.
    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if self.domain(b, model, op)?.is_false() {
            return Err(Error::access(format!(
                "You are not allowed to {} '{}' ({}) records.",
                op.as_str(),
                model.description,
                model.name
            )));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<AccessDomain> {
        Ok(AccessDomain::builder(self.domain(b, model, op)?).build())
    }

    /// Test rules know no field groups.
    fn field_access(
        &self,
        _b: &PrincipalBinding<'_>,
        _model: &ModelSpec,
    ) -> Result<Arc<FieldMask>> {
        Ok(FieldMask::shared_all())
    }

    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet> {
        Ok(GroupSet::new(direct.iter().copied()))
    }

    fn allowing_groups(&self, _model: &ModelSpec, _op: Operation) -> Result<Vec<GroupId>> {
        Ok(Vec::new())
    }

    /// Capabilities follow the default: all under `Allow`, none under `Deny`.
    fn has_capability(&self, _b: &PrincipalBinding<'_>, _cap: &Capability) -> Result<bool> {
        Ok(self.otherwise == Otherwise::Allow)
    }

    fn explain(&self, req: &ExplainRequest<'_>) -> Result<Explanation> {
        if !req.record_ids().is_empty() {
            return explain_records(req, self.generation());
        }
        let decision = if self
            .domain(req.binding(), req.model(), req.op())?
            .is_false()
        {
            AccessDecision::Deny
        } else {
            AccessDecision::Allow
        };
        Ok(Explanation::new(decision, self.generation(), Vec::new()))
    }
}
