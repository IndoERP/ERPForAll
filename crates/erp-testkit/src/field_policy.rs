//! A test policy with explicit field masks (engine note §2.2): model operations and
//! records are unrestricted, fields are denied by name or by their `groups` text against group
//! memberships the test states. It stands in for `erp-security`'s compiled field masks, which
//! come with, so the engine's field checks are tested without the group tables.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use erp_domain::Domain;
use erp_fields::{Error, Result};
use erp_orm::{
    AccessCtx, AccessCtxParts, AccessDecision, AccessDomain, AccessPolicy, Capability,
    ElevationKind, ExplainRequest, Explanation, FieldAccessTable, FieldDenialDetail, FieldMask,
    FieldOp, GroupId, GroupLookup, GroupRefResolution, GroupSet, GroupSetInterner, InternedGroups,
    Operation, PolicyGeneration, PrincipalBinding, QualifiedXmlId, SnapshotId, SnapshotOrigin,
    SnapshotRef,
};
use erp_registry::{FieldSpec, ModelSpec, Registry};

/// Most group sets one policy interns before it starts over.
const INTERNER_CAPACITY: usize = 1_024;

#[derive(Debug, Clone)]
enum Denial {
    Op(FieldOp),
    Forbid,
}

/// The state clones of a [`FieldPolicy`] share.
struct State {
    /// External ids of the groups the test mentions; the position + 1 is the group id.
    groups: Mutex<Vec<String>>,
    /// uid → external ids of its groups.
    members: Mutex<HashMap<i64, Vec<String>>>,
    group_names: Mutex<HashMap<String, String>>,
    mask_calls: AtomicUsize,
    /// The `#[field_access]` chains and the registry they were built for ([`FieldPolicy::attach`]).
    overrides: OnceLock<(Arc<Registry>, FieldAccessTable)>,
    snapshot: SnapshotId,
    interner: GroupSetInterner,
    denied_models: Mutex<HashSet<String>>,
}

impl State {
    fn group_id(&self, xmlid: &str) -> Result<GroupId> {
        let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
        let position = match groups.iter().position(|g| g == xmlid) {
            Some(p) => p,
            None => {
                groups.push(xmlid.to_owned());
                groups.len() - 1
            }
        };
        let id = i64::try_from(position + 1)
            .map_err(|_| Error::programming("FieldPolicy: too many groups"))?;
        Ok(GroupId::new(id)?)
    }

    fn is_member(&self, uid: i64, group: &str) -> bool {
        self.members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&uid)
            .is_some_and(|groups| groups.iter().any(|g| g == group))
    }

    /// Whether the test named `group` (through `member`, `join` or `group_name`).
    fn is_known(&self, group: &str) -> bool {
        self.members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|groups| groups.iter().any(|g| g == group))
            || self
                .group_names
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(group)
    }

    fn group_set(&self, uid: i64) -> Result<GroupSet> {
        let names: Vec<String> = self
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&uid)
            .cloned()
            .unwrap_or_default();
        let ids = names
            .iter()
            .map(|g| self.group_id(g))
            .collect::<Result<Vec<_>>>()?;
        Ok(GroupSet::new(ids))
    }
}

impl GroupLookup for State {
    fn group_id(&self, xmlid: &QualifiedXmlId) -> Result<Option<GroupId>> {
        State::group_id(self, &xmlid.to_string()).map(Some)
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").finish_non_exhaustive()
    }
}

/// Explicit per-(model, field) masks for tests.
///
/// ```
/// use erp_orm::FieldOp;
/// use erp_testkit::FieldPolicy;
/// let policy = FieldPolicy::new()
///     .deny("test_fa.item", "email", FieldOp::Read)
///     .forbid("test_fa.item", "forbidden3")
///     .enforce_groups()
///     .member(7, ["base.group_system"]);
/// policy.join(7, "base.group_user");
/// ```
///
/// Cloning shares the memberships, so a test keeps a handle after installing the policy and can
/// [`FieldPolicy::join`] a group mid-test. To run the `#[field_access]` overrides of the
/// registry, ask for them with [`FieldPolicy::with_overrides`] and call [`FieldPolicy::attach`]
/// once the harness has built its registry.
#[derive(Debug, Clone)]
pub struct FieldPolicy {
    denials: Vec<(String, String, Denial)>,
    enforce_groups: bool,
    wants_overrides: bool,
    state: Arc<State>,
}

impl Default for FieldPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl FieldPolicy {
    /// No restriction yet.
    #[allow(
        clippy::expect_used,
        reason = "test support: the process would need 2^64 policy snapshots to run out of ids"
    )]
    pub fn new() -> Self {
        Self {
            denials: Vec::new(),
            enforce_groups: false,
            wants_overrides: false,
            state: Arc::new(State {
                groups: Mutex::new(Vec::new()),
                members: Mutex::new(HashMap::new()),
                group_names: Mutex::new(HashMap::new()),
                mask_calls: AtomicUsize::new(0),
                overrides: OnceLock::new(),
                snapshot: SnapshotId::next().expect("snapshot ids"),
                interner: GroupSetInterner::new(INTERNER_CAPACITY).expect("interner capacity"),
                denied_models: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// `op` on `model.field` is denied to every non-superuser.
    #[must_use]
    pub fn deny(mut self, model: &str, field: &str, op: FieldOp) -> Self {
        self.denials
            .push((model.to_owned(), field.to_owned(), Denial::Op(op)));
        self
    }

    #[must_use]
    pub fn forbid(mut self, model: &str, field: &str) -> Self {
        self.denials
            .push((model.to_owned(), field.to_owned(), Denial::Forbid));
        self
    }

    /// Also decide by the fields' `groups` text: `.` forbids, a comma-separated list allows the
    /// members of any of its groups. Group expressions beyond that (`!group`) are an error.
    #[must_use]
    pub fn enforce_groups(mut self) -> Self {
        self.enforce_groups = true;
        self
    }

    /// Run the registry's `#[field_access]` overrides on every mask; [`FieldPolicy::attach`] must
    /// have been called, else every mask is an error (an override silently skipped would test
    /// nothing).
    #[must_use]
    pub fn with_overrides(mut self) -> Self {
        self.wants_overrides = true;
        self
    }

    /// Build the override chains of `registry` (the harness's, once built).
    ///
    /// # Errors
    ///
    /// The chain table's build errors; `Programming` when called twice.
    pub fn attach(&self, registry: &Arc<Registry>) -> Result<()> {
        let table = FieldAccessTable::build(registry, &[])?;
        self.state
            .overrides
            .set((Arc::clone(registry), table))
            .map_err(|_| Error::programming("FieldPolicy::attach was called twice"))
    }

    /// The context keys the overrides of `model` declared, newest addon first (empty before
    /// [`FieldPolicy::attach`]).
    pub fn context_keys(&self, model: erp_registry::ModelId) -> Vec<&'static str> {
        self.state
            .overrides
            .get()
            .map(|(_, table)| table.context_keys(model).to_vec())
            .unwrap_or_default()
    }

    /// `uid` is a member of `groups` (external ids), replacing its memberships.
    #[must_use]
    pub fn member<'g>(self, uid: i64, groups: impl IntoIterator<Item = &'g str>) -> Self {
        self.state
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(uid, groups.into_iter().map(str::to_owned).collect());
        self
    }

    /// `uid` joins `group` (an external id) from now on, also through clones of this policy.
    pub fn join(&self, uid: i64, group: &str) {
        self.state
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(uid)
            .or_default()
            .push(group.to_owned());
    }

    /// The display name `group` has in debug-mode error details (default: its external id).
    #[must_use]
    pub fn group_name(self, group: &str, name: &str) -> Self {
        self.state
            .group_names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(group.to_owned(), name.to_owned());
        self
    }

    /// Deny all operations on `model`.
    #[must_use]
    pub fn deny_model(self, model: &str) -> Self {
        self.state
            .denied_models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(model.to_owned());
        self
    }

    /// How many times the engine asked this policy for a field mask (superuser mode never does).
    pub fn mask_calls(&self) -> usize {
        self.state.mask_calls.load(Ordering::SeqCst)
    }

    fn by_groups(&self, uid: i64, spec: &str) -> Result<Option<bool>> {
        if spec == "." {
            return Ok(None);
        }
        let mut allowed = false;
        for group in spec.split(',').map(str::trim) {
            if group.is_empty() || group.starts_with('!') {
                return Err(Error::programming(format!(
                    "FieldPolicy cannot evaluate the groups expression '{spec}'"
                )));
            }
            allowed |= self.state.is_member(uid, group);
        }
        Ok(Some(allowed))
    }

    fn base_mask(&self, uid: i64, model: &ModelSpec) -> Result<FieldMask> {
        let mut mask = FieldMask::full(model.fields.len());
        if self.enforce_groups {
            for field in &model.fields {
                let Some(spec) = field.groups else { continue };
                match self.by_groups(uid, spec)? {
                    None => {
                        mask.forbid(field.id)?;
                    }
                    Some(true) => {}
                    Some(false) => {
                        mask.deny(field.id, FieldOp::Read)?
                            .deny(field.id, FieldOp::Write)?;
                    }
                }
            }
        }
        for (m, f, denial) in &self.denials {
            if m != model.name {
                continue;
            }
            let field = model.field(f).ok_or_else(|| {
                Error::programming(format!(
                    "FieldPolicy rule names the field '{f}', which {m} does not have"
                ))
            })?;
            match denial {
                Denial::Op(op) => {
                    mask.deny(field.id, *op)?;
                }
                Denial::Forbid => {
                    mask.forbid(field.id)?;
                }
            }
        }
        Ok(mask)
    }
}

impl AccessPolicy for FieldPolicy {
    fn generation(&self) -> PolicyGeneration {
        PolicyGeneration::default()
    }

    fn snapshot(&self) -> SnapshotRef {
        SnapshotRef::new(self.state.snapshot, SnapshotOrigin::Shared)
    }

    fn needs_resolved_principals(&self) -> bool {
        false
    }

    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups> {
        Ok(InternedGroups::new(
            self.state.interner.intern(groups)?,
            false,
        ))
    }

    fn check_model(
        &self,
        _b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if self
            .state
            .denied_models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(model.name)
        {
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
        _b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        _op: Operation,
    ) -> Result<AccessDomain> {
        if self
            .state
            .denied_models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(model.name)
        {
            return Ok(AccessDomain::deny_all());
        }
        Ok(AccessDomain::builder(Domain::TRUE).build())
    }

    fn field_context_keys(&self) -> &[&'static str] {
        self.state
            .overrides
            .get()
            .map_or(&[][..], |(_, table)| table.all_context_keys())
    }

    fn field_access(&self, b: &PrincipalBinding<'_>, model: &ModelSpec) -> Result<Arc<FieldMask>> {
        self.state.mask_calls.fetch_add(1, Ordering::SeqCst);
        let uid = b.principal().subject().get();
        let base = Arc::new(self.base_mask(uid, model)?);
        match (self.state.overrides.get(), self.wants_overrides) {
            (Some((registry, table)), _) => {
                let groups = self.state.group_set(uid)?;
                let ctx = AccessCtx::new(AccessCtxParts::new(
                    &groups,
                    b.shape().class(),
                    ElevationKind::of(b.shape().elevation()),
                    model,
                    registry,
                    &*self.state,
                ));
                table.narrow(ctx, b.field_ctx(), base)
            }
            (None, true) => Err(Error::programming(
                "FieldPolicy::with_overrides needs FieldPolicy::attach(registry) before the first \
                 use",
            )),
            (None, false) => Ok(base),
        }
    }

    /// Every debug principal counts as a member of `base.group_no_one`.
    fn field_denial_detail(
        &self,
        _b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        field: &FieldSpec,
        _op: FieldOp,
    ) -> Result<Option<FieldDenialDetail>> {
        let forbidden = self
            .denials
            .iter()
            .any(|(m, f, d)| m == model.name && f == field.name && matches!(d, Denial::Forbid));
        if forbidden || field.groups == Some(".") {
            return Ok(Some(FieldDenialDetail::AlwaysForbidden));
        }
        let Some(spec) = field.groups else {
            return Ok(Some(FieldDenialDetail::CustomRules));
        };
        let names = self
            .state
            .group_names
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(Some(FieldDenialDetail::Groups(
            spec.split(',')
                .map(str::trim)
                .map(|g| names.get(g).cloned().unwrap_or_else(|| g.to_owned()))
                .collect(),
        )))
    }

    /// The members the test stated: a comma-separated list of external ids, each optionally
    /// negated with `!`; satisfied when the subject is in one of the plain groups (or the spec
    /// has only negated ones) and in none of the negated ones. `.` is never satisfied. Debug
    /// mode and implied groups are not modelled.
    ///
    /// # Errors
    ///
    /// `Programming` for a group no `member`, `join` or `group_name` call named (as the real
    /// policy, an unknown group is never `false`), an empty token and a `!` without a name.
    fn has_groups(&self, b: &PrincipalBinding<'_>, spec: &str) -> Result<bool> {
        let uid = b.principal().subject().get();
        if spec.trim() == "." {
            return Ok(false);
        }
        let (mut plain, mut any_plain, mut negated_hit) = (false, false, false);
        for token in spec.split(',').map(str::trim) {
            let (negated, name) = match token.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, token),
            };
            if name.is_empty() {
                return Err(Error::programming(format!(
                    "FieldPolicy cannot evaluate the group spec '{spec}': empty group name"
                )));
            }
            if !self.state.is_known(name) {
                return Err(Error::programming(format!(
                    "FieldPolicy does not know the group '{name}' in '{spec}'"
                )));
            }
            if negated {
                negated_hit |= self.state.is_member(uid, name);
            } else {
                any_plain = true;
                plain |= self.state.is_member(uid, name);
            }
        }
        Ok((plain || !any_plain) && !negated_hit)
    }

    /// A group the test named (`member`, `join`, `group_name`) is known; `base.group_no_one` is
    /// known and counts only in debug mode; any other name is unknown.
    fn resolve_group_ref(&self, xmlid: &QualifiedXmlId) -> Result<GroupRefResolution> {
        let name = xmlid.as_str();
        if name == "base.group_no_one" {
            return Ok(GroupRefResolution::Known {
                id: self.state.group_id(name)?,
                debug_only: true,
            });
        }
        if self.state.is_known(name) {
            return Ok(GroupRefResolution::Known {
                id: self.state.group_id(name)?,
                debug_only: false,
            });
        }
        Ok(GroupRefResolution::Unknown)
    }

    /// The memberships the test stated, not the binding's resolution (this policy leaves
    /// principals unresolved). As in the compiled policy, a debug principal does not become a
    /// member of `base.group_no_one`: the group counts in debug mode only for a subject the test
    /// made a member of it (`member`, `join`).
    fn subject_groups<'b>(&self, b: &'b PrincipalBinding<'_>) -> Result<Cow<'b, GroupSet>> {
        let uid = b.principal().subject().get();
        Ok(Cow::Owned(self.state.group_set(uid)?))
    }

    /// Memberships and the set of known groups change in place: resolve per evaluation.
    fn group_refs_are_stable(&self) -> bool {
        false
    }

    fn has_any_group(&self, b: &PrincipalBinding<'_>, groups: &[GroupId]) -> Result<bool> {
        let held = self.subject_groups(b)?;
        // as the compiled policy: `base.group_no_one` counts only in debug mode
        let no_one = self.state.group_id("base.group_no_one")?;
        let debug = b.principal().debug();
        Ok(groups
            .iter()
            .any(|g| held.contains(*g) && (debug || *g != no_one)))
    }

    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet> {
        Ok(GroupSet::new(direct.iter().copied()))
    }

    fn allowing_groups(&self, _model: &ModelSpec, _op: Operation) -> Result<Vec<GroupId>> {
        Ok(Vec::new())
    }

    /// The field policy maps no capability.
    fn has_capability(&self, _b: &PrincipalBinding<'_>, cap: &Capability) -> Result<bool> {
        Err(Error::programming(format!(
            "the field policy maps no capability ('{}')",
            cap.name()
        )))
    }

    fn explain(&self, _req: &ExplainRequest<'_>) -> Result<Explanation> {
        Ok(Explanation::new(
            AccessDecision::Allow,
            self.generation(),
            Vec::new(),
        ))
    }
}
