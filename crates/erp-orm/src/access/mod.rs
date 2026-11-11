//! The access engine's policy interface (
//! §2.2, §2.3).
//!
//! `erp-orm` asks an [`AccessPolicy`] before every model operation and fails closed when none is
//! installed. The policy is an immutable **snapshot** pinned per transaction: every decision is a
//! pure function of (snapshot, [`PrincipalBinding`], model, operation), with no I/O. `erp-security`
//! builds snapshots from `ir.access` and publishes them through a
//! [`PolicyProvider`]; tests install a [`StaticPolicy`] or a test policy of the
//! testkit.
//!
//! The ORM never calls the policy directly: the engine entry points on
//! [`crate::Transaction`] (`binding`, `check_model`, `access_domain`, record checks) build the
//! binding, apply elevation and hold the per-transaction memo.
//!
//! - [`AccessDomain`]: the record predicate of (model, operation) with what it reads and where
//!   it came from.
//! - [`PolicyProvider`] / [`PrincipalResolver`] (`provider`): where snapshots and resolved
//!   principals come from.
//! - [`GrantSource`] (`grant`): grant domains ORed with permissions (decision 6).
//! - [`AccessCtx`] and the `#[access_domain]` override chain (`chain`).
//! - [`PolicySourceWrite`] (`source_write`): the models whose writes change the policy or a
//!   membership (§3.4).
//! - [`ExplainRequest`] / [`Explanation`] (`explain`): what the error builder asks (decision 8).

mod chain;
mod credential;
mod ctx;
mod domain;
mod engine;
mod explain;
mod grant;
mod group_spec;
mod guard;
mod guard_cache;
#[cfg(test)]
mod guard_tests;
mod interner;
mod provider;
mod records;
mod refresh;
mod rule_op;
mod source_write;
mod static_policy;

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use erp_fields::{Error, Result};
use erp_registry::{FieldSpec, ModelSpec};

pub use chain::{
    ACCESS_DOMAIN_IMPLS, AccessDomainChains, AccessDomainFn, AccessDomainImpl, AccessSuper,
};
pub use credential::{CredentialBegin, CredentialCheck, CredentialVerdict};
pub use ctx::{AccessCtx, AccessCtxParts, ElevationKind, GroupLookup};
pub use domain::{AccessDomain, AccessDomainBuilder, RuleOrigin, RuleRef};
pub(crate) use engine::TxAccess;
pub use erp_domain::Operation;
pub use explain::{
    AccessDecision, CompanyHint, EvidenceColumn, EvidenceCompany, EvidenceRow, ExplainDetail,
    ExplainRequest, ExplainTarget, Explanation, MAX_EXPLAIN_IDS, RecordEvidence, RuleKind,
};
pub use grant::{GRANT_SOURCES, GrantDomain, GrantRef, GrantSource};
pub use group_spec::{GroupClause, GroupSpec};
pub use guard::{
    FieldIdx, FieldPath, GroupRefResolution, GuardEvaluation, GuardIdx, GuardPlan,
    GuardPlanBuilder, GuardPlanLimits, RecordGuard, RecordGuardIdx,
};
pub use guard_cache::{ResolvedGuardCache, ResolvedGuardCacheConfig, ResolvedGuardCacheStats};
pub use interner::{GroupSetInterner, InternedGroups};
pub use provider::{
    FixedPolicy, IntoPolicyProvider, PolicyProvider, PrincipalResolver, ResolverStatement,
};
pub(crate) use records::{AccessState, MemoSnapshot, bypass_traversals};
pub(crate) use refresh::RefreshState;
pub use rule_op::{RULE_OPERATORS, RuleOperatorDecl};
pub use source_write::{POLICY_SOURCE_WRITES, PolicySourceWrite, SourceKind};
pub use static_policy::StaticPolicy;

use crate::field_access::{FieldDenialDetail, FieldMask, FieldOp};
use crate::principal::{
    Capability, GroupId, GroupSet, PrincipalBinding, QualifiedXmlId, SnapshotId,
};

/// The generations a policy snapshot was built for: the registry generation
/// and the `policy` signal generation read inside the build's own snapshot. Diagnostics only: the
/// identity of a snapshot is its [`SnapshotId`] (Revision 3 M1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PolicyGeneration {
    registry: u64,
    policy: u64,
}

impl PolicyGeneration {
    /// The generations `(registry, policy)`.
    pub const fn new(registry: u64, policy: u64) -> Self {
        Self { registry, policy }
    }

    /// The registry generation.
    pub const fn registry(self) -> u64 {
        self.registry
    }

    /// The `policy` signal generation (sum of weights) the build read.
    pub const fn policy(self) -> u64 {
        self.policy
    }
}

impl fmt::Display for PolicyGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "registry {} / policy {}", self.registry, self.policy)
    }
}

/// Whether a snapshot was published to every transaction or built for one (DN §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SnapshotOrigin {
    /// Published by the provider; pinned by transactions at `BEGIN`.
    Shared,
    /// Built by [`PolicyProvider::rebuild_private`] from one transaction's uncommitted view
    /// (SEC-018); never published. Its decisions never populate a cache that outlives the
    /// transaction (the shared search memo, prepared statements).
    Private,
}

/// The identity and origin of a policy snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapshotRef {
    id: SnapshotId,
    origin: SnapshotOrigin,
}

impl SnapshotRef {
    /// The snapshot `id` of `origin`.
    pub const fn new(id: SnapshotId, origin: SnapshotOrigin) -> Self {
        Self { id, origin }
    }

    /// The snapshot's id (unique in the process, never reused).
    pub const fn id(self) -> SnapshotId {
        self.id
    }

    /// Shared or private.
    pub const fn origin(self) -> SnapshotOrigin {
        self.origin
    }

    /// Whether its decisions may populate caches that outlive a transaction.
    pub fn is_shared(self) -> bool {
        self.origin == SnapshotOrigin::Shared
    }
}

/// Decides whether a principal may perform an operation on a model, and on which records
/// (design note §2.2). Implemented by `erp-security`'s policy snapshot; tests
/// install [`StaticPolicy`] or a testkit policy.
///
/// Rules for implementations:
///
/// - **Pure.** No I/O and no interior mutability other than bounded memo maps; every answer is a
///   function of the snapshot, the binding's key parts and the arguments.
/// - **Fail closed.** There is no default [`AccessPolicy::access_domain`]: a policy states its
///   record rules. An error (an uncompilable rule, an unknown group) denies with that error.
/// - [`AccessPolicy::check_model`] is exactly "[`AccessPolicy::access_domain`] is not `FALSE`",
///   plus a `read_only` API-key scope refusing every operation other than `Read`. The scope's
///   model and method allowlist is a dispatch check (RPC), not part of it.
/// - The engine never asks for a `Full` (superuser) binding: it allows those itself, after the
///   `read_only` scope check.
pub trait AccessPolicy: Send + Sync + 'static {
    /// The generations this snapshot was built for.
    fn generation(&self) -> PolicyGeneration;

    /// The snapshot's identity and origin.
    fn snapshot(&self) -> SnapshotRef;

    /// Whether decisions depend on the subject's resolved groups and companies (every compiled
    /// policy). The engine then refuses a principal that no resolver read
    /// ([`crate::Resolution::is_resolved`]) instead of deciding with an empty group set.
    fn needs_resolved_principals(&self) -> bool;

    /// The snapshot's interned id of `groups` (with whether the set is of a share user type,
    /// which makes a user principal's class `Portal`). An id is meaningful only with this
    /// snapshot's [`SnapshotId`] and never names two sets.
    ///
    /// # Errors
    ///
    /// A snapshot that cannot intern (resource bound reached and not evictable).
    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups>;

    /// # Errors
    ///
    /// `Access` with the SEC-008 message when the operation is not allowed; the error of a rule
    /// that does not compile.
    fn check_model(&self, b: &PrincipalBinding<'_>, model: &ModelSpec, op: Operation)
    -> Result<()>;

    /// Principal values are [`erp_domain::Value::Param`]
    /// leaves, never literals, so the result is a function of the memo key
    /// (model, op, `GroupSetId`, shape).
    ///
    /// # Errors
    ///
    /// The error of a rule that does not compile or sits on an `access` cycle; an override's or a
    /// grant source's error.
    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<AccessDomain>;

    /// The fields of `model` the binding's subject may read and write (
    /// design note §2.2): the fields' `groups` against the subject's groups, narrowed by the
    /// model's `#[field_access]` overrides ([`crate::FieldAccessTable::narrow`], which the policy
    /// runs because it owns the memo; [`PrincipalBinding::field_ctx`] holds the values of the
    /// context keys the overrides declared). The engine never asks for a superuser (`Full`)
    /// binding: it allows those itself ([`crate::Transaction::field_mask`]). There is no default,
    /// on purpose (fail closed): a policy without field groups returns
    /// [`FieldMask::shared_all`].
    ///
    /// # Errors
    ///
    /// A groups expression that does not parse or names an unknown group; an override's error,
    /// which denies with it (a widening override is a `Programming` error).
    fn field_access(&self, b: &PrincipalBinding<'_>, model: &ModelSpec) -> Result<Arc<FieldMask>>;

    /// The context keys the `#[field_access]` overrides this policy runs declared
    /// ([`crate::FieldAccessTable::all_context_keys`]): the engine reads their values from the
    /// environment into [`PrincipalBinding::field_ctx`]. The policy that runs the chains is the
    /// only owner of the override table, so the keys cannot name overrides that never run. The
    /// default is no keys, right for a policy that runs no overrides.
    fn field_context_keys(&self) -> &[&'static str] {
        &[]
    }

    /// The engine asks only for principals in debug mode. The default shows nothing.
    ///
    /// # Errors
    ///
    /// A groups expression that does not parse or names an unknown group.
    fn field_denial_detail(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        field: &FieldSpec,
        op: FieldOp,
    ) -> Result<Option<FieldDenialDetail>> {
        let _ = (b, model, field, op);
        Ok(None)
    }

    /// The subject satisfies it when it is in at least one of the plain groups and in none of
    /// the negated ones; a spec of only negated groups needs just the second half; `.` is never
    /// satisfied. `base.group_no_one` counts only in debug mode
    /// ([`crate::Principal::debug`]). No statements.
    ///
    /// There is no default that answers: a policy without groups cannot say, and a wrong `true`
    /// or `false` here is an access decision.
    ///
    /// # Errors
    ///
    /// A spec that does not parse or names a group the policy does not have; a policy that does
    /// not know groups (the default) is a `Programming` error.
    fn has_groups(&self, b: &PrincipalBinding<'_>, spec: &str) -> Result<bool> {
        let _ = (b, spec);
        Err(Error::programming(
            "this access policy does not evaluate group specs (has_groups): use a compiled \
             policy snapshot",
        ))
    }

    /// As in
    /// [`AccessPolicy::has_groups`], `base.group_no_one` counts only in debug mode. No statements.
    /// An empty list is never satisfied.
    ///
    /// There is no default that answers, for the reason given at [`AccessPolicy::has_groups`].
    ///
    /// # Errors
    ///
    /// A policy that does not know groups (the default) is a `Programming` error.
    fn has_any_group(&self, b: &PrincipalBinding<'_>, groups: &[GroupId]) -> Result<bool> {
        let _ = (b, groups);
        Err(Error::programming(
            "this access policy does not evaluate group membership (has_any_group): use a \
             compiled policy snapshot",
        ))
    }

    /// Resolve one symbolic group reference of a guard plan in this snapshot (4): the
    /// group id and whether it is `base.group_no_one` (counted only in debug mode), or
    /// [`GroupRefResolution::Unknown`] for a reference the snapshot has no group for. Pure, no I/O;
    /// the answer is valid for this snapshot's [`SnapshotId`] only.
    ///
    /// There is no default that answers: a policy without groups cannot say (the same rule as
    /// [`AccessPolicy::has_groups`]).
    ///
    /// # Errors
    ///
    /// A policy that does not know groups (the default) is a `Programming` error.
    fn resolve_group_ref(&self, xmlid: &QualifiedXmlId) -> Result<GroupRefResolution> {
        let _ = xmlid;
        Err(Error::programming(
            "this access policy does not resolve group references (resolve_group_ref): use a \
             compiled policy snapshot",
        ))
    }

    /// The effective groups guard evaluation holds the subject to (4). The default is
    /// the binding's resolution ([`crate::Resolution::groups`]), right for every compiled policy;
    /// a test policy that keeps memberships itself returns those. A debug principal is not given
    /// `base.group_no_one` here: like the compiled policy, the group counts in debug mode only for
    /// a subject who is a member of it ([`GroupRefResolution::Known::debug_only`]).
    ///
    /// Contract: [`AccessPolicy::has_any_group`], which decides record guards, must answer from
    /// the same set, so a node guard and a record guard naming the same groups agree.
    ///
    /// # Errors
    ///
    /// A policy that cannot state the memberships.
    fn subject_groups<'b>(&self, b: &'b PrincipalBinding<'_>) -> Result<Cow<'b, GroupSet>> {
        Ok(Cow::Borrowed(b.resolution().groups()))
    }

    /// Whether the answers of [`AccessPolicy::resolve_group_ref`] stay the same for this
    /// snapshot's whole life, so a resolved guard plan may be cached under its
    /// [`SnapshotId`]. True for a compiled (immutable) snapshot; false for a policy whose groups
    /// change in place, whose plans are then resolved per evaluation and never cached.
    ///
    /// There is no default, on purpose: a cache that is wrongly trusted answers wrongly, so every
    /// implementation states it (a policy that does not resolve group references says `true`,
    /// since nothing it could cache is wrong).
    fn group_refs_are_stable(&self) -> bool;

    /// The effective group set of `direct` groups (reflexive transitive closure of the implied
    /// groups, SEC-027).
    ///
    /// # Errors
    ///
    /// A group id that is not in the snapshot (a deleted group) is an error, never dropped.
    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet>;

    /// # Errors
    ///
    /// The error of a rule that does not compile.
    fn allowing_groups(&self, model: &ModelSpec, op: Operation) -> Result<Vec<GroupId>>;

    /// Whether the binding's subject holds `cap` (decision 7).
    ///
    /// # Errors
    ///
    /// A capability without a mapping in the snapshot is an error, never `false`.
    fn has_capability(&self, b: &PrincipalBinding<'_>, cap: &Capability) -> Result<bool>;

    /// Explain a decision (decision 8): the error builder asks for the SEC-009 record message
    /// with this. Bounded: at most [`MAX_EXPLAIN_IDS`] ids.
    ///
    /// # Errors
    ///
    /// More ids than the bound; a policy that cannot explain the request.
    fn explain(&self, req: &ExplainRequest<'_>) -> Result<Explanation>;

    /// The generated company restriction of `model` compiled for the binding (its deny variant
    /// under an empty active-company set, [`crate::ActiveCompanySet`]), `None` for a model without
    /// one. The engine checks the new state of a record that a write moves against it (design
    /// `m3-company-a1` §6.5). The default is a policy without generated restrictions (a fixed or
    /// test policy): it answers `None`.
    ///
    /// # Errors
    ///
    /// The error of a drifted generated restriction (its model is refused); a binding the policy
    /// cannot decide for.
    fn generated_restriction(
        &self,
        _b: &PrincipalBinding<'_>,
        _model: &ModelSpec,
    ) -> Result<Option<AccessDomain>> {
        Ok(None)
    }
}

impl<P: AccessPolicy + ?Sized> AccessPolicy for Arc<P> {
    fn generation(&self) -> PolicyGeneration {
        (**self).generation()
    }

    fn snapshot(&self) -> SnapshotRef {
        (**self).snapshot()
    }

    fn needs_resolved_principals(&self) -> bool {
        (**self).needs_resolved_principals()
    }

    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups> {
        (**self).intern_groups(groups)
    }

    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        (**self).check_model(b, model, op)
    }

    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<AccessDomain> {
        (**self).access_domain(b, model, op)
    }

    fn field_access(&self, b: &PrincipalBinding<'_>, model: &ModelSpec) -> Result<Arc<FieldMask>> {
        (**self).field_access(b, model)
    }

    fn field_context_keys(&self) -> &[&'static str] {
        (**self).field_context_keys()
    }

    fn field_denial_detail(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        field: &FieldSpec,
        op: FieldOp,
    ) -> Result<Option<FieldDenialDetail>> {
        (**self).field_denial_detail(b, model, field, op)
    }

    fn has_groups(&self, b: &PrincipalBinding<'_>, spec: &str) -> Result<bool> {
        (**self).has_groups(b, spec)
    }

    fn has_any_group(&self, b: &PrincipalBinding<'_>, groups: &[GroupId]) -> Result<bool> {
        (**self).has_any_group(b, groups)
    }

    fn resolve_group_ref(&self, xmlid: &QualifiedXmlId) -> Result<GroupRefResolution> {
        (**self).resolve_group_ref(xmlid)
    }

    fn subject_groups<'b>(&self, b: &'b PrincipalBinding<'_>) -> Result<Cow<'b, GroupSet>> {
        (**self).subject_groups(b)
    }

    fn group_refs_are_stable(&self) -> bool {
        (**self).group_refs_are_stable()
    }

    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet> {
        (**self).groups(direct)
    }

    fn allowing_groups(&self, model: &ModelSpec, op: Operation) -> Result<Vec<GroupId>> {
        (**self).allowing_groups(model, op)
    }

    fn has_capability(&self, b: &PrincipalBinding<'_>, cap: &Capability) -> Result<bool> {
        (**self).has_capability(b, cap)
    }

    fn explain(&self, req: &ExplainRequest<'_>) -> Result<Explanation> {
        (**self).explain(req)
    }

    fn generated_restriction(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
    ) -> Result<Option<AccessDomain>> {
        (**self).generated_restriction(b, model)
    }
}
