//! Guard plans and their evaluation (§5.2,
//! §6).
//!
//! A view bundle needs a set of questions decided for one principal: which node guards (group
//! expressions) hold, which legal records' `group_ids` are satisfied, which fields are readable.
//! The questions are a [`GuardPlan`], built and canonicalized by a [`GuardPlanBuilder`]. The answers
//! are a [`GuardEvaluation`], which only [`crate::Transaction::evaluate_guards`] can mint: its
//! fields are private, it has no constructor, no `Default`, no `Clone` and no `Deserialize`, and
//! it borrows the transaction it was decided in. A caller cannot supply verdicts of its own.
//!
//! ```compile_fail,E0451
//! # use erp_orm::GuardEvaluation;
//! // every field is set, so the only error left is that the fields are private (E0451): verdicts
//! // cannot be fabricated outside the module that mints them
//! let _forged = GuardEvaluation {
//!     tx: todo!(),
//!     plan: todo!(),
//!     node: todo!(),
//!     record: todo!(),
//!     fields: todo!(),
//!     registry_generation: todo!(),
//!     policy: todo!(),
//!     snapshot: todo!(),
//!     subject: todo!(),
//!     key: todo!(),
//!     field_ctx: todo!(),
//! };
//! ```

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use erp_domain::Operation;
use erp_fields::{Error, ErrorKind, Result};
use erp_registry::ModelId;
use rustc_hash::{FxHashMap, FxHashSet};

use super::guard_cache::GuardCacheKey;
use super::{GroupClause, GroupSpec, PolicyGeneration, SnapshotRef};
use crate::env::Env;
use crate::field_access::FieldMask;
use crate::field_chain::FieldContextValues;
use crate::principal::{BindingKey, GroupId, GroupSet, QualifiedXmlId, UserId};
use crate::transaction::Transaction;

/// How one symbolic group reference resolved in one policy snapshot
/// ([`crate::AccessPolicy::resolve_group_ref`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupRefResolution {
    /// The snapshot has the group. `debug_only` is `base.group_no_one`, which counts only in
    /// debug mode.
    Known {
        /// The group's id in the snapshot.
        id: GroupId,
        /// Held only by a principal in debug mode.
        debug_only: bool,
    },
    /// The snapshot has no such group: a positive reference never matches, a negated one never
    /// excludes.
    Unknown,
}

macro_rules! plan_index {
    ($(#[$meta:meta])* $name:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(u32);

        impl $name {
            #[doc = concat!("The ", $what, " at position `index` of a plan. Whether the position")]
            #[doc = "exists is checked when it is used, against the plan of the evaluation."]
            ///
            /// # Errors
            ///
            /// `Programming` for a position beyond `u32::MAX`.
            pub fn new(index: usize) -> Result<Self> {
                u32::try_from(index).map(Self).map_err(|_| {
                    Error::programming(format!(
                        concat!($what, " index {} is out of range"),
                        index
                    ))
                })
            }

            /// The position.
            #[must_use]
            pub const fn get(self) -> usize {
                self.0 as usize
            }
        }
    };
}

plan_index!(
    /// Position of a node guard in a [`GuardPlan`].
    GuardIdx,
    "node guard"
);
plan_index!(
    /// Position of a record guard in a [`GuardPlan`].
    RecordGuardIdx,
    "record guard"
);
plan_index!(
    /// Position of a field reference in a [`GuardPlan`].
    FieldIdx,
    "field"
);

/// Bounds of a [`GuardPlan`] (style: chosen bounds that can be lowered, never raised;
/// not measurements).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardPlanLimits {
    node_guards: usize,
    refs_per_guard: usize,
    record_guards: usize,
    fields: usize,
    path_hops: usize,
    plan_bytes: usize,
}

impl GuardPlanLimits {
    /// Most distinct node guards in one plan.
    pub const MAX_NODE_GUARDS: usize = 4_096;
    /// Most group references (positive and negated) in one guard.
    pub const MAX_REFS_PER_GUARD: usize = 64;
    /// Most record guards in one plan.
    pub const MAX_RECORD_GUARDS: usize = 128;
    /// Most field references in one plan.
    pub const MAX_FIELDS: usize = 50_000;
    /// Most hops of one field path.
    pub const MAX_PATH_HOPS: usize = 8;
    /// Most groups in one record guard.
    pub const MAX_RECORD_GROUPS: usize = 1_024;
    /// Longest model name or field name (one path hop), in bytes.
    pub const MAX_NAME_BYTES: usize = 128;
    /// Most bytes one plan may retain (the sum [`GuardPlan::retained_bytes`] reports).
    pub const MAX_PLAN_BYTES: usize = 8 << 20;

    /// Limits, each from 1 up to the maximum above.
    ///
    /// # Errors
    ///
    /// `Validation` naming the limit that is zero or above its maximum.
    pub fn new(
        node_guards: usize,
        refs_per_guard: usize,
        record_guards: usize,
        fields: usize,
        path_hops: usize,
        plan_bytes: usize,
    ) -> Result<Self> {
        for (name, value, max) in [
            ("node_guards", node_guards, Self::MAX_NODE_GUARDS),
            ("refs_per_guard", refs_per_guard, Self::MAX_REFS_PER_GUARD),
            ("record_guards", record_guards, Self::MAX_RECORD_GUARDS),
            ("fields", fields, Self::MAX_FIELDS),
            ("path_hops", path_hops, Self::MAX_PATH_HOPS),
            ("plan_bytes", plan_bytes, Self::MAX_PLAN_BYTES),
        ] {
            if value == 0 || value > max {
                return Err(Error::validation(format!(
                    "guard plan limit {name} is {value}: it must be from 1 to {max}"
                )));
            }
        }
        Ok(Self {
            node_guards,
            refs_per_guard,
            record_guards,
            fields,
            path_hops,
            plan_bytes,
        })
    }

    /// Most distinct node guards.
    #[must_use]
    pub const fn node_guards(&self) -> usize {
        self.node_guards
    }

    /// Most group references in one guard.
    #[must_use]
    pub const fn refs_per_guard(&self) -> usize {
        self.refs_per_guard
    }

    /// Most record guards.
    #[must_use]
    pub const fn record_guards(&self) -> usize {
        self.record_guards
    }

    /// Most field references.
    #[must_use]
    pub const fn fields(&self) -> usize {
        self.fields
    }

    /// Most hops of one field path.
    #[must_use]
    pub const fn path_hops(&self) -> usize {
        self.path_hops
    }

    /// Most bytes one plan may retain.
    #[must_use]
    pub const fn plan_bytes(&self) -> usize {
        self.plan_bytes
    }
}

impl Default for GuardPlanLimits {
    fn default() -> Self {
        Self {
            node_guards: Self::MAX_NODE_GUARDS,
            refs_per_guard: Self::MAX_REFS_PER_GUARD,
            record_guards: Self::MAX_RECORD_GUARDS,
            fields: Self::MAX_FIELDS,
            path_hops: Self::MAX_PATH_HOPS,
            plan_bytes: Self::MAX_PLAN_BYTES,
        }
    }
}

/// A field a bundle reads: the model and a path of at most [`GuardPlanLimits::MAX_PATH_HOPS`]
/// hops (`a.b.c`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldPath {
    model: Box<str>,
    path: Box<str>,
}

impl FieldPath {
    /// The model the path starts at (the model of the enclosing sub-view).
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The dotted field path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// A legal contributing record's `group_ids`: the view is for members of at least one of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordGuard {
    view_id: i64,
    groups: Box<[GroupId]>,
}

impl RecordGuard {
    /// The record (view) id.
    #[must_use]
    pub fn view_id(&self) -> i64 {
        self.view_id
    }

    /// The groups, sorted.
    #[must_use]
    pub fn groups(&self) -> &[GroupId] {
        &self.groups
    }
}

/// What a bundle needs decided: node guards, record guards and field references in ORM-owned
/// validated types. Built only by [`GuardPlanBuilder::finish`].
///
/// A plan is **positional**: the bundle that built it refers to guards by their index, so the
/// builder keeps the order entries were first added in (equal entries share one index) and two
/// plans are equal only when every position holds equal contents. [`Eq`] and [`Hash`] cover the
/// complete contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GuardPlan {
    root_model: Box<str>,
    node_guards: Box<[GroupSpec]>,
    record_guards: Box<[RecordGuard]>,
    fields: Box<[FieldPath]>,
    bytes: usize,
}

impl GuardPlan {
    /// The model the bundle is for.
    #[must_use]
    pub fn root_model(&self) -> &str {
        &self.root_model
    }

    /// The node guards, in index order.
    #[must_use]
    pub fn node_guards(&self) -> &[GroupSpec] {
        &self.node_guards
    }

    /// The record guards, in index order.
    #[must_use]
    pub fn record_guards(&self) -> &[RecordGuard] {
        &self.record_guards
    }

    /// The field references, in index order.
    #[must_use]
    pub fn fields(&self) -> &[FieldPath] {
        &self.fields
    }

    /// Bytes this plan retains, charged to the resolved-plan cache.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    /// A 64-bit digest of the contents, used only to pick a cache bucket: a cache hit still
    /// compares the whole plan ("compare canonical contents, not just a digest").
    #[must_use]
    pub fn content_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

/// Builds a [`GuardPlan`], validating every term and every bound.
#[derive(Debug)]
pub struct GuardPlanBuilder {
    root_model: Box<str>,
    limits: GuardPlanLimits,
    node_guards: Vec<GroupSpec>,
    node_index: FxHashMap<GroupSpec, usize>,
    record_guards: Vec<RecordGuard>,
    fields: Vec<FieldPath>,
    field_index: FxHashMap<(Box<str>, Box<str>), usize>,
    /// Bytes retained so far, as [`GuardPlan::retained_bytes`] will report them.
    retained: usize,
}

fn record_bytes(groups: usize) -> usize {
    std::mem::size_of::<RecordGuard>()
        .saturating_add(groups.saturating_mul(std::mem::size_of::<GroupId>()))
}

fn field_bytes(model: &str, path: &str) -> usize {
    std::mem::size_of::<FieldPath>()
        .saturating_add(model.len())
        .saturating_add(path.len())
}

fn valid_name(part: &str) -> bool {
    let mut chars = part.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn valid_model(name: &str) -> bool {
    name.len() <= GuardPlanLimits::MAX_NAME_BYTES
        && !name.is_empty()
        && name.split('.').all(valid_name)
}

impl GuardPlanBuilder {
    /// A builder for a bundle of `root_model`.
    ///
    /// # Errors
    ///
    /// `Validation` when `root_model` is not a dotted model name, or when `limits.plan_bytes` is
    /// below what the empty plan of `root_model` already retains (the exact minimum is accepted).
    pub fn new(root_model: &str, limits: GuardPlanLimits) -> Result<Self> {
        if !valid_model(root_model) {
            return Err(Error::validation(format!(
                "guard plan root model '{root_model}' is not a model name"
            )));
        }
        let retained = std::mem::size_of::<GuardPlan>().saturating_add(root_model.len());
        if retained > limits.plan_bytes {
            return Err(Error::validation(format!(
                "guard plan limit plan_bytes is {}: the empty plan of '{root_model}' already \
                 retains {retained} bytes",
                limits.plan_bytes
            )));
        }
        Ok(Self {
            root_model: root_model.into(),
            limits,
            node_guards: Vec::new(),
            node_index: FxHashMap::default(),
            record_guards: Vec::new(),
            fields: Vec::new(),
            field_index: FxHashMap::default(),
            retained,
        })
    }

    /// Account `bytes` more; refuse the entry that would take the plan over its bound.
    fn charge(&mut self, bytes: usize) -> Result<()> {
        let total = self.retained.saturating_add(bytes);
        if total > self.limits.plan_bytes {
            return Err(Error::validation(format!(
                "the guard plan of '{}' would retain more than {} bytes",
                self.root_model, self.limits.plan_bytes
            )));
        }
        self.retained = total;
        Ok(())
    }

    /// A node guard (already the intersection of the enclosing guards). Returns its index; a
    /// guard equal to one added before returns that one's index.
    ///
    /// # Errors
    ///
    /// `Validation` for an open guard (no restriction needs no entry), for more group references
    /// than [`GuardPlanLimits::refs_per_guard`], and for more distinct guards than
    /// [`GuardPlanLimits::node_guards`].
    pub fn node_guard(&mut self, spec: GroupSpec) -> Result<GuardIdx> {
        if spec.is_open() {
            return Err(Error::validation(
                "an open group spec is not a guard: a node without a restriction has no entry in \
                 the guard plan",
            ));
        }
        if spec.ref_count() > self.limits.refs_per_guard {
            return Err(Error::validation(format!(
                "a guard names {} group references: at most {} are allowed",
                spec.ref_count(),
                self.limits.refs_per_guard
            )));
        }
        if let Some(&at) = self.node_index.get(&spec) {
            return GuardIdx::new(at);
        }
        if self.node_guards.len() >= self.limits.node_guards {
            return Err(Error::validation(format!(
                "the guard plan of '{}' has more than {} distinct guards",
                self.root_model, self.limits.node_guards
            )));
        }
        self.charge(spec.retained_bytes())?;
        let at = self.node_guards.len();
        self.node_index.insert(spec.clone(), at);
        self.node_guards.push(spec);
        GuardIdx::new(at)
    }

    /// A legal contributing record's `group_ids` (primary views only; the caller enforces
    /// legality). Returns its index; a view id added before returns that index when the groups
    /// are the same.
    ///
    /// # Errors
    ///
    /// `Validation` for a non-positive view id, no groups, more than
    /// [`GuardPlanLimits::MAX_RECORD_GROUPS`] groups, more than
    /// [`GuardPlanLimits::record_guards`] records, and a view id given twice with different groups.
    pub fn record_guard(&mut self, view_id: i64, groups: &[GroupId]) -> Result<RecordGuardIdx> {
        if view_id <= 0 {
            return Err(Error::validation(format!(
                "record guard view id {view_id} is not a record id"
            )));
        }
        if groups.is_empty() {
            return Err(Error::validation(format!(
                "record guard of view {view_id} has no groups: a record without groups is \
                 unrestricted and has no entry"
            )));
        }
        if groups.len() > GuardPlanLimits::MAX_RECORD_GROUPS {
            return Err(Error::validation(format!(
                "record guard of view {view_id} names {} groups: at most {} are allowed",
                groups.len(),
                GuardPlanLimits::MAX_RECORD_GROUPS
            )));
        }
        let mut sorted = groups.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let guard = RecordGuard {
            view_id,
            groups: sorted.into_boxed_slice(),
        };
        if let Some(at) = self.record_guards.iter().position(|g| g.view_id == view_id) {
            if self.record_guards[at] != guard {
                return Err(Error::validation(format!(
                    "view {view_id} was added to the guard plan twice with different groups"
                )));
            }
            return RecordGuardIdx::new(at);
        }
        if self.record_guards.len() >= self.limits.record_guards {
            return Err(Error::validation(format!(
                "the guard plan of '{}' has more than {} record guards",
                self.root_model, self.limits.record_guards
            )));
        }
        self.charge(record_bytes(guard.groups.len()))?;
        self.record_guards.push(guard);
        RecordGuardIdx::new(self.record_guards.len() - 1)
    }

    /// A field the bundle reads: `model` and a dotted `path` of at most
    /// [`GuardPlanLimits::path_hops`] validated names. Returns its index; the same reference
    /// added again returns the same index.
    ///
    /// # Errors
    ///
    /// `Validation` for a bad model or path, too many hops, or more than
    /// [`GuardPlanLimits::fields`] references.
    pub fn field(&mut self, model: &str, path: &str) -> Result<FieldIdx> {
        if !valid_model(model) {
            return Err(Error::validation(format!(
                "field reference model '{model}' is not a model name"
            )));
        }
        if path.len() > GuardPlanLimits::MAX_NAME_BYTES * GuardPlanLimits::MAX_PATH_HOPS + 7 {
            return Err(Error::validation(format!(
                "field reference path of model '{model}' is {} bytes: it is too long",
                path.len()
            )));
        }
        let mut hops = 0usize;
        for hop in path.split('.') {
            hops += 1;
            if hop.len() > GuardPlanLimits::MAX_NAME_BYTES {
                return Err(Error::validation(format!(
                    "field reference path of model '{model}' has a field name of {} bytes: at \
                     most {} are allowed",
                    hop.len(),
                    GuardPlanLimits::MAX_NAME_BYTES
                )));
            }
            if !valid_name(hop) {
                return Err(Error::validation(format!(
                    "field reference path '{path}' of model '{model}' is not a dotted list of \
                     field names"
                )));
            }
        }
        if hops > self.limits.path_hops {
            return Err(Error::validation(format!(
                "field reference path '{path}' of model '{model}' has {hops} hops: at most {} \
                 are allowed",
                self.limits.path_hops
            )));
        }
        let key: (Box<str>, Box<str>) = (model.into(), path.into());
        if let Some(&at) = self.field_index.get(&key) {
            return FieldIdx::new(at);
        }
        if self.fields.len() >= self.limits.fields {
            return Err(Error::validation(format!(
                "the guard plan of '{}' has more than {} field references",
                self.root_model, self.limits.fields
            )));
        }
        self.charge(field_bytes(model, path))?;
        let at = self.fields.len();
        self.fields.push(FieldPath {
            model: key.0.clone(),
            path: key.1.clone(),
        });
        self.field_index.insert(key, at);
        FieldIdx::new(at)
    }

    /// The plan.
    ///
    /// # Errors
    ///
    /// Never today (every bound is checked as entries are added); `Result` keeps room for checks
    /// that need the whole plan.
    pub fn finish(self) -> Result<GuardPlan> {
        let bytes = self.retained;
        Ok(GuardPlan {
            root_model: self.root_model,
            node_guards: self.node_guards.into_boxed_slice(),
            record_guards: self.record_guards.into_boxed_slice(),
            fields: self.fields.into_boxed_slice(),
            bytes,
        })
    }
}

// ---- resolved form (cached per snapshot) ----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedTerm {
    pub(crate) id: GroupId,
    pub(crate) debug_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedClause {
    pub(crate) positives: Box<[ResolvedTerm]>,
    /// Positive references the snapshot has no group for: they never match, and a clause whose
    /// positives are all unknown still requires one (so it fails), never "no positives".
    pub(crate) unknown_positives: u32,
    pub(crate) negatives: Box<[ResolvedTerm]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedGuard {
    pub(crate) never: bool,
    pub(crate) clauses: Box<[ResolvedClause]>,
}

/// A plan's node guards resolved to group ids in one snapshot. The symbolic references stay in
/// the [`GuardPlan`]; ids live only here and die with the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedPlan {
    pub(crate) node: Box<[ResolvedGuard]>,
    pub(crate) bytes: usize,
}

impl ResolvedPlan {
    /// Resolve every reference of `plan` with `resolve`.
    pub(crate) fn resolve(
        plan: &GuardPlan,
        mut resolve: impl FnMut(&QualifiedXmlId) -> Result<GroupRefResolution>,
    ) -> Result<Self> {
        let mut bytes = std::mem::size_of::<Self>();
        let mut node = Vec::with_capacity(plan.node_guards.len());
        for spec in plan.node_guards.iter() {
            let mut clauses = Vec::with_capacity(spec.clauses().len());
            for clause in spec.clauses() {
                clauses.push(resolve_clause(clause, &mut resolve)?);
            }
            bytes = bytes
                .checked_add(std::mem::size_of::<ResolvedGuard>())
                .ok_or_else(|| Error::programming("resolved guard plan size overflows"))?;
            for c in &clauses {
                let terms = c.positives.len() + c.negatives.len();
                bytes = bytes
                    .checked_add(std::mem::size_of::<ResolvedClause>())
                    .and_then(|b| b.checked_add(terms * std::mem::size_of::<ResolvedTerm>()))
                    .ok_or_else(|| Error::programming("resolved guard plan size overflows"))?;
            }
            node.push(ResolvedGuard {
                never: spec.is_never(),
                clauses: clauses.into_boxed_slice(),
            });
        }
        Ok(Self {
            node: node.into_boxed_slice(),
            bytes,
        })
    }
}

fn resolve_clause(
    clause: &GroupClause,
    resolve: &mut impl FnMut(&QualifiedXmlId) -> Result<GroupRefResolution>,
) -> Result<ResolvedClause> {
    let mut positives = Vec::new();
    let mut unknown = 0u32;
    for xmlid in clause.positives() {
        match resolve(xmlid)? {
            GroupRefResolution::Known { id, debug_only } => {
                positives.push(ResolvedTerm { id, debug_only });
            }
            GroupRefResolution::Unknown => unknown += 1,
        }
    }
    let mut negatives = Vec::new();
    for xmlid in clause.negatives() {
        // an unknown negated reference never excludes anybody
        if let GroupRefResolution::Known { id, debug_only } = resolve(xmlid)? {
            negatives.push(ResolvedTerm { id, debug_only });
        }
    }
    Ok(ResolvedClause {
        positives: positives.into_boxed_slice(),
        unknown_positives: unknown,
        negatives: negatives.into_boxed_slice(),
    })
}

fn held(term: &ResolvedTerm, groups: &GroupSet, debug: bool) -> bool {
    groups.contains(term.id) && (debug || !term.debug_only)
}

/// 4: `.` refuses; an empty group set refuses every guard (negative-only ones
/// included); a held negated term refuses; a clause with positive terms needs one held known
/// positive (unknown positives never match); every clause must pass.
pub(crate) fn guard_allows(guard: &ResolvedGuard, groups: &GroupSet, debug: bool) -> bool {
    if guard.never || groups.is_empty() {
        return false;
    }
    guard.clauses.iter().all(|clause| {
        if clause.negatives.iter().any(|t| held(t, groups, debug)) {
            return false;
        }
        if clause.positives.is_empty() && clause.unknown_positives == 0 {
            return true;
        }
        clause.positives.iter().any(|t| held(t, groups, debug))
    })
}

// ---- the evaluation -------------------------------------------------------------------------

/// The decided guard plan for one principal in one transaction.
///
/// No `Clone`, `Serialize`, `Deserialize` or `Default`, no constructor and private fields:
/// [`Transaction::evaluate_guards`] is the only producer. It borrows the transaction it was
/// decided in, so it cannot outlive it and the transaction cannot be used (or committed) while it
/// lives; a policy or membership change is therefore never observable through a held
/// evaluation.
///
/// borrow cases are the trybuild suite of `erp-views`, `tests/compile_fail.rs`)
///
/// Verdicts are read by position of the plan; a position outside the plan is a
/// `Programming` error, never `false`.
#[must_use = "an evaluation decides nothing until it is read"]
pub struct GuardEvaluation<'t> {
    tx: &'t Transaction,
    plan: Arc<GuardPlan>,
    node: Box<[bool]>,
    record: Box<[bool]>,
    fields: Box<[bool]>,
    registry_generation: u64,
    policy: PolicyGeneration,
    snapshot: SnapshotRef,
    subject: UserId,
    key: BindingKey,
    field_ctx: FieldContextValues,
}

impl fmt::Debug for GuardEvaluation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardEvaluation")
            .field("root_model", &self.plan.root_model())
            .field("node_guards", &self.node.len())
            .field("record_guards", &self.record.len())
            .field("fields", &self.fields.len())
            .field("registry_generation", &self.registry_generation)
            .field("snapshot", &self.snapshot)
            .field("subject", &self.subject)
            .finish_non_exhaustive()
    }
}

fn verdict(verdicts: &[bool], index: usize, what: &str) -> Result<bool> {
    verdicts.get(index).copied().ok_or_else(|| {
        Error::programming(format!(
            "{what} index {index} outside plan of {} {what}s",
            verdicts.len()
        ))
    })
}

impl GuardEvaluation<'_> {
    /// The plan that was decided.
    #[must_use]
    pub fn plan(&self) -> &Arc<GuardPlan> {
        &self.plan
    }

    /// Whether the principal passes node guard `guard`.
    ///
    /// # Errors
    ///
    /// `Programming` when `guard` is outside the plan.
    pub fn node_allowed(&self, guard: GuardIdx) -> Result<bool> {
        verdict(&self.node, guard.get(), "guard")
    }

    /// Whether the principal is in one of the groups of record guard `guard`.
    ///
    /// # Errors
    ///
    /// `Programming` when `guard` is outside the plan.
    pub fn record_allowed(&self, guard: RecordGuardIdx) -> Result<bool> {
        verdict(&self.record, guard.get(), "record guard")
    }

    /// Whether the principal may read every hop of field reference `field`.
    ///
    /// # Errors
    ///
    /// `Programming` when `field` is outside the plan.
    pub fn field_readable(&self, field: FieldIdx) -> Result<bool> {
        verdict(&self.fields, field.get(), "field")
    }

    /// The registry generation the evaluation was decided under.
    #[must_use]
    pub fn registry_generation(&self) -> u64 {
        self.registry_generation
    }

    /// The policy generations of the snapshot (diagnostics; the identity is [`Self::snapshot`]).
    #[must_use]
    pub fn policy(&self) -> PolicyGeneration {
        self.policy
    }

    /// The snapshot the evaluation was decided with.
    #[must_use]
    pub fn snapshot(&self) -> SnapshotRef {
        self.snapshot
    }

    /// The user the evaluation was decided for.
    #[must_use]
    pub fn subject(&self) -> UserId {
        self.subject
    }

    /// Whether `env` is the principal this evaluation was decided for: the same binding key
    /// (subject, kind, interned group set, debug mode, elevation, companies, scope) in the same
    /// snapshot, with the same values of the context keys that `#[field_access]` overrides
    /// declared (the field verdicts of the evaluation were computed with them; a context key no
    /// override declared is not part of the identity). An environment whose binding cannot be
    /// built in this transaction is not it.
    #[must_use]
    pub fn is_for(&self, env: &Env) -> bool {
        self.tx.binding(env).is_ok_and(|binding| {
            binding.key(self.snapshot.id()) == self.key && binding.field_ctx() == &self.field_ctx
        })
    }
}

impl Transaction {
    /// Decide `plan` for the principal of `env` (4): the node guards against the
    /// subject's effective groups, the record guards against `group_ids`, and every field
    /// reference against the field masks, all in the transaction's pinned policy snapshot.
    /// This is the only way to obtain a [`GuardEvaluation`].
    ///
    /// Order: the principal is prepared (a stale policy or membership is refused), the model
    /// read check of the root model and of every model the plan reads runs through the access
    /// engine (superuser mode is decided there), then the guards are resolved (cached per
    /// snapshot for shared snapshots) and evaluated. Superuser mode does not change guard
    /// answers (it does not make the user a member of a group), only field masks.
    ///
    /// ```
    /// # async fn demo(tx: &mut erp_orm::Transaction, plan: &std::sync::Arc<erp_orm::GuardPlan>)
    /// #     -> erp_fields::Result<()> {
    /// let env = tx.env();
    /// let evaluation = tx.evaluate_guards(&env, plan).await?;
    /// let _ = evaluation.subject();
    /// // `tx` is borrowed until `evaluation` is dropped
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// `Access` when the principal may not read the root model (another model of the plan that
    /// it may not read makes the field references through it unreadable instead);
    /// `Programming` for a stale policy, a closed elevation block, an unresolved principal, a
    /// policy that does not resolve group references, and a model or field of the plan the
    /// registry does not have.
    pub async fn evaluate_guards<'t>(
        &'t mut self,
        env: &Env,
        plan: &Arc<GuardPlan>,
    ) -> Result<GuardEvaluation<'t>> {
        self.prepare_access(env).await?;
        let tx: &'t Transaction = self;
        tx.decide_guards(env, plan)
    }

    fn decide_guards<'t>(
        &'t self,
        env: &Env,
        plan: &Arc<GuardPlan>,
    ) -> Result<GuardEvaluation<'t>> {
        let registry = &self.shared.registry;
        let mut masks: FxHashMap<ModelId, Arc<FieldMask>> = FxHashMap::default();
        let mut denied: FxHashSet<ModelId> = FxHashSet::default();
        // The model of `name` when the principal may read it; `None` for a model other than the
        // root that it may not: every field reference through it is then unreadable (the node and
        // its subtree leave with the filter), which is no error (review M1). The root model's
        // refusal, and any error that is not a read refusal, is returned.
        let mut readable_model = |tx: &Transaction, name: &str| -> Result<Option<ModelId>> {
            let id = registry.model_id(name).ok_or_else(|| {
                Error::programming(format!(
                    "the guard plan of '{}' names the model '{name}', which the registry does \
                     not have",
                    plan.root_model()
                ))
            })?;
            if denied.contains(&id) {
                return Ok(None);
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = masks.entry(id) {
                match tx.check_model(env, id, Operation::Read) {
                    Ok(()) => {
                        slot.insert(tx.field_mask(env, id)?);
                    }
                    Err(error)
                        if error.kind() == ErrorKind::Access && name != plan.root_model() =>
                    {
                        denied.insert(id);
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(Some(id))
        };
        if readable_model(self, plan.root_model())?.is_none() {
            return Err(Error::programming(
                "the root model of a guard plan was reported unreadable without an error",
            ));
        }
        // the model checks of every hop run before anything is decided; `None` is a path through
        // a model the principal may not read
        let mut paths: Vec<Option<Vec<(ModelId, erp_registry::FieldId)>>> =
            Vec::with_capacity(plan.fields().len());
        for field in plan.fields() {
            let Some(mut model) = readable_model(self, field.model())? else {
                paths.push(None);
                continue;
            };
            let hops: Vec<&str> = field.path().split('.').collect();
            let mut resolved = Vec::with_capacity(hops.len());
            let mut blocked = false;
            for (i, hop) in hops.iter().enumerate() {
                let spec = registry.model_by_id(model);
                let f = spec.field(hop).ok_or_else(|| {
                    Error::programming(format!(
                        "the guard plan of '{}' reads '{}.{}', but '{}' has no field '{hop}'",
                        plan.root_model(),
                        field.model(),
                        field.path(),
                        spec.name
                    ))
                })?;
                resolved.push((model, f.id));
                if i + 1 < hops.len() {
                    let comodel = f.ty.comodel().ok_or_else(|| {
                        Error::programming(format!(
                            "the guard plan of '{}' reads '{}.{}', but '{hop}' of '{}' is not \
                             a relational field",
                            plan.root_model(),
                            field.model(),
                            field.path(),
                            spec.name
                        ))
                    })?;
                    match readable_model(self, comodel)? {
                        Some(next) => model = next,
                        None => {
                            blocked = true;
                            break;
                        }
                    }
                }
            }
            paths.push((!blocked).then_some(resolved));
        }

        let binding = self.binding(env)?;
        let policy = self.policy();
        let snapshot = policy.snapshot();
        let groups = policy.subject_groups(&binding)?;
        let debug = binding.principal().debug();

        let resolve =
            |p: &GuardPlan| ResolvedPlan::resolve(p, |xmlid| policy.resolve_group_ref(xmlid));
        let resolved: Arc<ResolvedPlan> = if snapshot.is_shared() && policy.group_refs_are_stable()
        {
            let key = GuardCacheKey::new(
                self.database_id(),
                registry.generation(),
                snapshot.id(),
                plan.content_hash(),
            );
            self.shared
                .resolved_guards
                .get_or_resolve(key, plan, resolve)?
        } else {
            Arc::new(resolve(plan)?)
        };

        let node: Box<[bool]> = resolved
            .node
            .iter()
            .map(|g| guard_allows(g, &groups, debug))
            .collect();
        let mut record = Vec::with_capacity(plan.record_guards().len());
        for guard in plan.record_guards() {
            record.push(policy.has_any_group(&binding, guard.groups())?);
        }
        let mut fields = Vec::with_capacity(paths.len());
        for path in &paths {
            let Some(path) = path else {
                fields.push(false);
                continue;
            };
            let mut readable = true;
            for (model, field) in path {
                let mask = masks.get(model).ok_or_else(|| {
                    Error::programming("a field mask of the guard plan was not prepared")
                })?;
                if !mask.readable(*field) {
                    readable = false;
                    break;
                }
            }
            fields.push(readable);
        }

        Ok(GuardEvaluation {
            tx: self,
            plan: Arc::clone(plan),
            node,
            record: record.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
            registry_generation: registry.generation(),
            policy: policy.generation(),
            snapshot,
            subject: binding.principal().subject(),
            key: binding.key(snapshot.id()),
            field_ctx: binding.field_ctx().clone(),
        })
    }
}
