//! [`StaticPolicy`]: the bootstrap and test policy (no record rules, no groups).

use std::sync::Arc;

use erp_fields::{Error, Result};
use erp_registry::ModelSpec;

use super::domain::AccessDomain;
use super::explain::{AccessDecision, ExplainRequest, Explanation};
use super::interner::{GroupSetInterner, InternedGroups};
use super::{AccessPolicy, Operation, PolicyGeneration, SnapshotOrigin, SnapshotRef};
use crate::field_access::FieldMask;
use crate::principal::{Capability, GroupId, GroupSet, PrincipalBinding, SnapshotId};

/// Static policies for tests and bootstrap: no record rules, no groups. Installed with
/// [`crate::OrmBuilder::policy`], each installation is its own snapshot (its own
/// [`SnapshotId`]). `erp-server` refuses them: a server decides with `erp-security`.
///
/// The engine allows superuser (`Full`) principals itself, so a static policy only decides for the
/// others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticPolicy {
    /// Everything is allowed.
    AllowAll,
    /// Only superuser mode is allowed: every non-superuser principal is refused.
    SuperuserOnly,
    /// Nothing is allowed (what `erp-orm` uses when no policy is installed).
    DenyAll,
}

impl StaticPolicy {
    fn allows(self) -> bool {
        match self {
            Self::AllowAll => true,
            Self::SuperuserOnly | Self::DenyAll => false,
        }
    }
}

/// Most group sets one static snapshot interns before it starts over.
const STATIC_INTERNER_CAPACITY: usize = 4_096;

/// A [`StaticPolicy`] as a snapshot.
#[derive(Debug)]
pub(crate) struct StaticSnapshot {
    kind: StaticPolicy,
    id: SnapshotId,
    interner: GroupSetInterner,
}

impl StaticSnapshot {
    pub(crate) fn new(kind: StaticPolicy) -> Result<Self> {
        Ok(Self {
            kind,
            id: SnapshotId::next()?,
            interner: GroupSetInterner::new(STATIC_INTERNER_CAPACITY)?,
        })
    }
}

impl AccessPolicy for StaticSnapshot {
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

    /// Static decisions read neither groups nor companies.
    fn needs_resolved_principals(&self) -> bool {
        false
    }

    /// Every set interns (no share user types without a group graph).
    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups> {
        Ok(InternedGroups::new(self.interner.intern(groups)?, false))
    }

    fn check_model(
        &self,
        _b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if self.kind.allows() {
            Ok(())
        } else {
            Err(Error::access(format!(
                "Your access rights do not let you {} '{}' ({}) records.",
                op.as_str(),
                model.description,
                model.name
            )))
        }
    }

    /// No record rules: every record when the model is allowed, none otherwise.
    fn access_domain(
        &self,
        _b: &PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<AccessDomain> {
        Ok(if self.kind.allows() {
            AccessDomain::allow_all()
        } else {
            AccessDomain::deny_all()
        })
    }

    /// The static policies know no field groups: every field is accessible whenever the model is
    /// (the model check comes first).
    fn field_access(
        &self,
        _b: &PrincipalBinding<'_>,
        _model: &ModelSpec,
    ) -> Result<Arc<FieldMask>> {
        Ok(FieldMask::shared_all())
    }

    /// No implied groups: the sorted direct ids.
    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet> {
        Ok(GroupSet::new(direct.iter().copied()))
    }

    /// No group grants anything: the decision does not depend on groups.
    fn allowing_groups(&self, _model: &ModelSpec, _op: Operation) -> Result<Vec<GroupId>> {
        Ok(Vec::new())
    }

    /// `AllowAll` grants every capability, the others none (superuser principals never reach
    /// the policy).
    fn has_capability(&self, _b: &PrincipalBinding<'_>, _cap: &Capability) -> Result<bool> {
        Ok(self.kind.allows())
    }

    /// The static decision, blaming no rule (there are none).
    fn explain(&self, _req: &ExplainRequest<'_>) -> Result<Explanation> {
        let decision = if self.kind.allows() {
            AccessDecision::Allow
        } else {
            AccessDecision::Deny
        };
        Ok(Explanation::new(decision, self.generation(), Vec::new()))
    }
}
