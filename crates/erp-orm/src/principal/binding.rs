//! Principal binding (DN §2.1, R-M1): the view of a principal every access
//! decision is taken against, its shape (what changes a compiled predicate's structure) and the
//! by-value key of the per-transaction record memo.
//!
//! Only `Transaction::binding` builds a binding; BASE-0 lands the types and their pure
//! derivations. `PrincipalShape::grant_sources` comes with's `GrantSource`,
//! `PrincipalBinding::field_ctx` with's `#[field_access]` context keys.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use erp_fields::Result;

use super::elevation::{Elevation, ElevationScope};
use super::error::PrincipalError;
use super::ids::UserId;
use super::scope::Scope;
use super::{CompanyContext, GrantBinding, KindClass, Principal, Resolution};
use crate::access::AccessPolicy;
use crate::field_chain::FieldContextValues;

/// Identity of one policy snapshot build, shared or private (DN §2.3, Revision 3 M1): unique in
/// the process, never reused. Every cache that outlives a transaction is keyed by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotId(NonZeroU64);

static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

impl SnapshotId {
    /// A new id, larger than every id handed out before in this process.
    ///
    /// ```
    /// use erp_orm::SnapshotId;
    /// let (a, b) = (SnapshotId::next()?, SnapshotId::next()?);
    /// assert!(b > a);
    /// # Ok::<(), erp_orm::PrincipalError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`PrincipalError::SnapshotIdsExhausted`] after 2^64 − 1 ids (ids are never reused).
    pub fn next() -> Result<Self, PrincipalError> {
        match NEXT_SNAPSHOT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        {
            // the counter starts at 1 and only grows, so `n` is non-zero
            Ok(n) => NonZeroU64::new(n)
                .map(Self)
                .ok_or(PrincipalError::SnapshotIdsExhausted),
            Err(_) => Err(PrincipalError::SnapshotIdsExhausted),
        }
    }

    /// The raw id.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// A group set interned by one policy snapshot ('s interner). Meaningful only together with
/// that snapshot's [`SnapshotId`], so every key holding one holds both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupSetId(NonZeroU64);

impl GroupSetId {
    /// The id the interner handed out.
    pub fn new(id: NonZeroU64) -> Self {
        Self(id)
    }

    /// The raw id.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// The elevation as a memo key: the scope by value, no block (DN §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ElevationKey {
    /// No elevation.
    None,
    /// A scoped block, keyed by what it opens.
    Scoped(Arc<ElevationScope>),
    /// Superuser mode.
    Full,
}

impl ElevationKey {
    /// The key of `elevation`.
    pub fn of(elevation: &Elevation) -> Self {
        match elevation {
            Elevation::None => Self::None,
            Elevation::Scoped { scope, .. } => Self::Scoped(Arc::clone(scope)),
            Elevation::Full => Self::Full,
        }
    }
}

/// Whether the validated active-company set of a decision is empty (design `m3-company-a1` §4.3).
/// Part of the shape: an empty set compiles every generated company restriction from its deny
/// variant, so the two variants never share a memo entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ActiveCompanySet {
    /// At least one active company.
    NonEmpty,
    /// No active company.
    Empty,
}

impl ActiveCompanySet {
    /// The kind of a validated company list.
    pub fn of(companies: &[super::CompanyId]) -> Self {
        if companies.is_empty() {
            Self::Empty
        } else {
            Self::NonEmpty
        }
    }
}

/// What changes the structure of a compiled predicate; holds no principal values (DN §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PrincipalShape {
    class: KindClass,
    elevation: ElevationKey,
    companies: ActiveCompanySet,
}

impl PrincipalShape {
    /// The kind class.
    pub fn class(&self) -> KindClass {
        self.class
    }

    /// The elevation key.
    pub fn elevation(&self) -> &ElevationKey {
        &self.elevation
    }

    /// Whether the validated active-company set is empty.
    pub fn companies(&self) -> ActiveCompanySet {
        self.companies
    }
}

/// A principal bound for decisions in one transaction (DN §2.1): the principal, the newest
/// resolution of its subject, its group set interned in the pinned snapshot, and its shape.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PrincipalBinding<'e> {
    principal: &'e Principal,
    resolution: Arc<Resolution>,
    groups: GroupSetId,
    shape: PrincipalShape,
    field_ctx: FieldContextValues,
}

impl<'e> PrincipalBinding<'e> {
    /// Bind `principal` with the newest `resolution` of its subject under `policy`: the group
    /// set of the resolution is interned in the policy's snapshot and the class derived here, so
    /// the interned id and the resolution a policy decides on cannot differ (the shared
    /// combined-policy cache is keyed by one and computed from the other).
    ///
    /// The engine builds the binding of every decision (`Transaction::binding`: newest
    /// resolution, open elevation block, validated companies); policy crates build bindings in
    /// their own tests with this constructor.
    ///
    /// # Errors
    ///
    /// The policy's interning error (an unknown group).
    pub fn bind(
        policy: &dyn AccessPolicy,
        principal: &'e Principal,
        resolution: Arc<Resolution>,
    ) -> Result<Self> {
        let interned = policy.intern_groups(resolution.groups())?;
        let class = KindClass::derive(principal.kind(), interned.share());
        Ok(Self::new(principal, resolution, interned.id(), class))
    }

    /// The parts of a binding; `groups` must be the interned group set of `resolution` (only
    /// [`PrincipalBinding::bind`] and this module's tests, which fabricate ids, call it).
    pub(crate) fn new(
        principal: &'e Principal,
        resolution: Arc<Resolution>,
        groups: GroupSetId,
        class: KindClass,
    ) -> Self {
        Self {
            principal,
            resolution,
            groups,
            shape: PrincipalShape {
                class,
                elevation: ElevationKey::of(principal.elevation()),
                // the companies the principal names; the engine replaces it with the validated
                // set of the transaction (`with_companies`)
                companies: ActiveCompanySet::of(principal.companies().allowed()),
            },
            field_ctx: FieldContextValues::default(),
        }
    }

    /// The same binding with the validated active-company set of the decision: the engine's
    /// `Transaction::binding` states it from the companies it validated (an environment that
    /// names none acts with the subject's defaults).
    #[must_use]
    pub fn with_companies(mut self, companies: ActiveCompanySet) -> Self {
        self.shape.companies = companies;
        self
    }

    /// The same binding carrying the values of the context keys `#[field_access]` overrides
    /// declared (the engine reads them once per environment).
    #[must_use]
    pub fn with_field_ctx(mut self, field_ctx: FieldContextValues) -> Self {
        self.field_ctx = field_ctx;
        self
    }

    /// The full value key of the record memo under `snapshot` (the snapshot that interned
    /// [`PrincipalBinding::groups`]).
    #[allow(
        dead_code,
        reason = "the record memo of the access engine (a later release) keys its entries with it"
    )]
    pub(crate) fn key(&self, snapshot: SnapshotId) -> BindingKey {
        BindingKey {
            snapshot,
            subject: self.principal.subject(),
            groups: self.groups,
            companies: self.principal.companies().clone(),
            grants: self.principal.grants().clone(),
            scope: self.principal.scope().cloned(),
            elevation: self.shape.elevation.clone(),
            active: self.shape.companies,
            debug: self.principal.debug(),
        }
    }
}

impl<'e> PrincipalBinding<'e> {
    /// The principal.
    pub fn principal(&self) -> &'e Principal {
        self.principal
    }

    /// The newest resolution of the subject in this transaction.
    pub fn resolution(&self) -> &Arc<Resolution> {
        &self.resolution
    }

    /// The interned group set.
    pub fn groups(&self) -> GroupSetId {
        self.groups
    }

    /// The values of the context keys declared by `#[field_access]` overrides anywhere (empty when
    /// no override declares one).
    pub fn field_ctx(&self) -> &FieldContextValues {
        &self.field_ctx
    }

    /// The shape.
    pub fn shape(&self) -> &PrincipalShape {
        &self.shape
    }
}

/// Key of the per-transaction record memo (DN §3.3), by value: no memo is keyed by a bare hash,
/// and two principals that differ in anything a decision depends on never share an entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct BindingKey {
    snapshot: SnapshotId,
    subject: UserId,
    groups: GroupSetId,
    companies: CompanyContext,
    grants: GrantBinding,
    scope: Option<Scope>,
    elevation: ElevationKey,
    active: ActiveCompanySet,
    debug: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::{CompanyId, GroupSet, PrincipalBuilder, SUPERUSER_ID};

    fn principal(uid: i64, companies: &[i64]) -> Principal {
        let all: Vec<CompanyId> = [1, 2]
            .into_iter()
            .map(|c| CompanyId::new(c).unwrap())
            .collect();
        PrincipalBuilder::user(UserId::new(uid).unwrap())
            .resolution(Resolution::in_id_order(GroupSet::default(), all, NonZeroU64::MIN).unwrap())
            .companies(
                companies
                    .iter()
                    .map(|c| CompanyId::new(*c).unwrap())
                    .collect(),
            )
            .build()
            .unwrap()
    }

    fn id(n: u64) -> GroupSetId {
        GroupSetId::new(NonZeroU64::new(n).unwrap())
    }

    #[test]
    fn snapshot_ids_grow_and_are_unique_across_threads() {
        let ids: Vec<SnapshotId> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        (0..100)
                            .map(|_| SnapshotId::next().unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "never reused");
    }

    #[test]
    fn same_groups_different_principal_values_never_share_a_key() {
        let snapshot = SnapshotId::next().unwrap();
        let base = principal(5, &[1]);
        let key = |p: &Principal, g: GroupSetId, s: SnapshotId| {
            let res = Arc::clone(p.resolution());
            PrincipalBinding::new(p, res, g, KindClass::Internal).key(s)
        };
        let k = key(&base, id(1), snapshot);
        assert_eq!(k, key(&base.clone(), id(1), snapshot), "by value");
        let others = [
            key(&principal(6, &[1]), id(1), snapshot),
            key(&principal(5, &[2]), id(1), snapshot),
            key(&principal(5, &[1, 2]), id(1), snapshot),
            key(&principal(5, &[2, 1]), id(1), snapshot),
            key(&base.with_elevation(Elevation::Full), id(1), snapshot),
            key(&base, id(2), snapshot),
            key(&base, id(1), SnapshotId::next().unwrap()),
        ];
        for (i, other) in others.iter().enumerate() {
            assert_ne!(&k, other, "variant {i}");
        }
        let debug = PrincipalBuilder::user(UserId::new(5).unwrap())
            .resolution((**base.resolution()).clone())
            .companies(vec![CompanyId::new(1).unwrap()])
            .debug(true)
            .build()
            .unwrap();
        assert_ne!(k, key(&debug, id(1), snapshot));
    }

    #[test]
    fn shape_holds_class_and_elevation_only() {
        let a = principal(5, &[1]);
        let b = principal(6, &[2]);
        let bind = |p: &Principal| {
            PrincipalBinding::new(p, Arc::clone(p.resolution()), id(3), KindClass::Portal)
                .shape()
                .clone()
        };
        assert_eq!(
            bind(&a),
            bind(&b),
            "principal values are not part of the shape"
        );
        let full = a.with_elevation(Elevation::Full);
        assert_eq!(bind(&full).elevation(), &ElevationKey::Full);
        assert_eq!(bind(&a).class(), KindClass::Portal);
        let sys = Principal::system();
        let binding = PrincipalBinding::new(
            &sys,
            Arc::clone(sys.resolution()),
            id(9),
            KindClass::Internal,
        );
        assert_eq!(binding.principal().subject(), SUPERUSER_ID);
        assert_eq!(binding.groups(), id(9));
        assert!(!binding.resolution().is_resolved());
    }
}
