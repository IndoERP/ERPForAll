//! Principals: the only
//! identity the ORM knows. Every [`crate::Env`] carries one.
//!
//! A principal names its **subject** (the `res.users` row whose groups and record rules apply), the
//! **actor** when someone else acts (an API key, an impersonating principal, a link token), the
//! subject's **resolution** (effective groups and companies), the **company context**, an API-key
//! **scope**, the **grant binding** of a link, the **elevation** and the debug flag.
//!
//! lands the values and the environments that carry them. A uid becomes a principal
//! without database I/O, exactly as today's `(uid, su)` pair: [`crate::SUPERUSER_ID`] is the
//! `System` principal (superuser mode), any other uid an *unresolved* user principal (no groups,
//! no companies; [`Resolution::is_resolved`] is false). Resolution against the database comes with,
//! agent and service principals with, public and link principals and scoped
//! elevation blocks with.

mod binding;
pub(crate) mod block;
mod capability;
mod elevation;
mod error;
mod ids;
mod scope;
pub(crate) mod switch;

use std::num::NonZeroU64;
use std::sync::Arc;

pub use binding::{
    ActiveCompanySet, BindingKey, ElevationKey, GroupSetId, PrincipalBinding, PrincipalShape,
    SnapshotId,
};
pub(crate) use capability::validate as validate_capabilities;
pub use capability::{CAPABILITIES, Capability, MAX_CAPABILITY_LEN, registered_capability};
pub use elevation::{
    BlockId, Elevation, ElevationScope, FieldAllow, MAX_ELEVATION_ITEMS, MAX_ITEM_ENTRIES,
    OperationSet, ScopeItem, TxSerial,
};
pub use error::{IdKind, PrincipalError};
pub use ids::{
    ApiKeyId, CompanyId, GroupId, LinkId, MAX_XML_ID_LEN, QualifiedXmlId, SUPERUSER_ID, UserId,
};
pub use scope::{
    MAX_KEY_NAME_LEN, MAX_SCOPE_ENTRIES, MAX_SCOPE_NAME_LEN, MethodAllow, ModelAllow, Scope,
    ScopeBuilder,
};

/// The answer for an unknown, and equally for a refused, password reset target (
/// Amendment 1: a refused target is not told apart from a missing one). Every site that refuses a
/// reset target uses this one constructor (Auth's checks and the reset line model of `base`), so
/// the answers cannot drift.
///
/// ```
/// let error = erp_orm::missing_reset_target();
/// assert_eq!(error.kind(), erp_fields::ErrorKind::Missing);
/// ```
#[must_use]
pub fn missing_reset_target() -> erp_fields::Error {
    erp_fields::Error::new(
        erp_fields::ErrorKind::Missing,
        "A selected password reset user no longer exists.",
    )
}

/// What kind of principal acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrincipalKind {
    /// An interactive or session user.
    User,
    /// An API key acting for its delegating user.
    Agent,
    /// An API key of a dedicated service user.
    Service,
    /// The public user of the request's website or company.
    Public,
    /// A link token over a record set.
    Link,
    /// The core: install, data loading, migrations, schema sync, tests.
    System,
}

impl PrincipalKind {
    /// Lowercase name, used in error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Service => "service",
            Self::Public => "public",
            Self::Link => "link",
            Self::System => "system",
        }
    }
}

/// What `#[access_domain]` and `#[field_access]` overrides may see of a principal (DN §2.2):
/// derived from the kind and the subject's user type, never set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KindClass {
    /// An internal user (or service/system principal of one).
    Internal,
    /// A portal user (`share`).
    Portal,
    /// The public principal.
    Public,
    /// A link principal.
    Link,
    /// An agent principal.
    Agent,
}

impl KindClass {
    /// The class of a `kind` principal whose subject has the user type `share` (portal or public
    /// user type; reads it from the subject's groups).
    pub fn derive(kind: PrincipalKind, share: bool) -> Self {
        match kind {
            PrincipalKind::User | PrincipalKind::Service | PrincipalKind::System => {
                if share {
                    Self::Portal
                } else {
                    Self::Internal
                }
            }
            PrincipalKind::Agent => Self::Agent,
            PrincipalKind::Public => Self::Public,
            PrincipalKind::Link => Self::Link,
        }
    }
}

/// Kind of an API key (maps key kinds to principal kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KeyKind {
    /// A user key: the subject keeps its user kind and is audited as this key.
    User,
    /// Acts for its delegating user.
    Agent,
    /// Acts as a dedicated service user.
    Service,
}

/// Who acts when it is not the subject. Audit records subject and actor separately.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Actor {
    /// An API key, including a user principal authenticated with a key.
    ApiKey {
        /// The key.
        id: ApiKeyId,
        /// Its kind.
        kind: KeyKind,
        /// Its name.
        name: Box<str>,
    },
    /// The principal that switched user with `with_user`; always the original one, so the chain
    /// never grows.
    Impersonator(Box<Principal>),
    /// A link token.
    Link {
        /// The link.
        id: LinkId,
    },
}

/// A set of group ids: sorted, without duplicates, compared and hashed by value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct GroupSet(Box<[GroupId]>);

impl GroupSet {
    /// The set of `ids` (set semantics: order and repeats do not matter). The effective groups of
    /// a subject are the closure the policy computes (`AccessPolicy::groups`).
    pub fn new(ids: impl IntoIterator<Item = GroupId>) -> Self {
        let mut ids: Vec<GroupId> = ids.into_iter().collect();
        ids.sort_unstable();
        ids.dedup();
        Self(ids.into_boxed_slice())
    }

    /// Whether `group` is in the set.
    pub fn contains(&self, group: GroupId) -> bool {
        self.0.binary_search(&group).is_ok()
    }

    /// The ids, sorted.
    pub fn ids(&self) -> &[GroupId] {
        &self.0
    }

    /// Number of groups.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Which resolution of a subject a [`Resolution`] is (the transaction's resolution epoch, DN §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolutionEpoch(u64);

impl ResolutionEpoch {
    /// Not read from the database (the harness and static-policy principals).
    pub const UNRESOLVED: Self = Self(0);

    /// The `n`-th resolution.
    pub fn resolved(n: NonZeroU64) -> Self {
        Self(n.get())
    }

    /// The raw epoch (0 for [`ResolutionEpoch::UNRESOLVED`]).
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The subject's effective groups and all its companies, as read at one resolution epoch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Resolution {
    groups: GroupSet,
    all_companies: Box<[CompanyId]>,
    /// The same companies in the order of the default company context: the main company first,
    /// then the others in `res.company` order.
    default_companies: Box<[CompanyId]>,
    epoch: ResolutionEpoch,
}

impl Resolution {
    /// A resolution whose default company context is the subject's companies in **ascending id
    /// order**, which is not the order (main company first, then
    /// `res.company` order): the principal resolver replaces it with
    /// [`Resolution::with_default_order`]; a hand-built resolution that does not need an order
    /// (tests with one company) keeps this one.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::DuplicateCompany`] when a company repeats.
    pub fn in_id_order(
        groups: GroupSet,
        all_companies: impl IntoIterator<Item = CompanyId>,
        epoch: NonZeroU64,
    ) -> Result<Self, PrincipalError> {
        let mut companies: Vec<CompanyId> = all_companies.into_iter().collect();
        companies.sort_unstable();
        if let Some(pair) = companies.windows(2).find(|w| w[0] == w[1]) {
            return Err(PrincipalError::DuplicateCompany {
                company: pair[0].get(),
            });
        }
        Ok(Self {
            groups,
            default_companies: companies.clone().into_boxed_slice(),
            all_companies: companies.into_boxed_slice(),
            epoch: ResolutionEpoch::resolved(epoch),
        })
    }

    /// This resolution with the default company context in `ordered` (main company first, then
    /// the others in `res.company` order; the resolver sets it). Without it the default order is
    /// [`Resolution::all_companies`] (ascending ids, see [`Resolution::in_id_order`]).
    ///
    /// ```
    /// use erp_orm::{CompanyId, GroupSet, Resolution};
    /// use std::num::NonZeroU64;
    /// let (c1, c2) = (CompanyId::new(1)?, CompanyId::new(2)?);
    /// let resolution = Resolution::in_id_order(GroupSet::default(), [c1, c2], NonZeroU64::MIN)?
    ///     .with_default_order(vec![c2, c1])?;
    /// assert_eq!(resolution.default_companies(), [c2, c1]);
    /// assert_eq!(resolution.all_companies(), [c1, c2]);
    /// # Ok::<(), erp_orm::PrincipalError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`PrincipalError::DuplicateCompany`] when a company repeats;
    /// [`PrincipalError::UnauthorizedCompanies`] when `ordered` is not a permutation of the
    /// resolution's companies (a company the subject is not in, or one left out).
    pub fn with_default_order(mut self, ordered: Vec<CompanyId>) -> Result<Self, PrincipalError> {
        for (i, company) in ordered.iter().enumerate() {
            if ordered[..i].contains(company) {
                return Err(PrincipalError::DuplicateCompany {
                    company: company.get(),
                });
            }
            if self.all_companies.binary_search(company).is_err() {
                return Err(PrincipalError::UnauthorizedCompanies);
            }
        }
        if ordered.len() != self.all_companies.len() {
            return Err(PrincipalError::UnauthorizedCompanies);
        }
        self.default_companies = ordered.into_boxed_slice();
        Ok(self)
    }

    /// Nothing read yet: no groups, no companies, [`ResolutionEpoch::UNRESOLVED`].'s
    /// engine refuses such a principal under a compiled policy.
    pub fn unresolved() -> Self {
        Self {
            groups: GroupSet::default(),
            all_companies: Box::default(),
            default_companies: Box::default(),
            epoch: ResolutionEpoch::UNRESOLVED,
        }
    }

    /// Effective groups.
    pub fn groups(&self) -> &GroupSet {
        &self.groups
    }

    /// All companies of the subject, sorted.
    pub fn all_companies(&self) -> &[CompanyId] {
        &self.all_companies
    }

    /// The default company context: the main company first, then the others in `res.company`
    /// order. The companies of [`Resolution::all_companies`], in that order.
    pub fn default_companies(&self) -> &[CompanyId] {
        &self.default_companies
    }

    /// The epoch.
    pub fn epoch(&self) -> ResolutionEpoch {
        self.epoch
    }

    /// Whether this was read from the database.
    pub fn is_resolved(&self) -> bool {
        self.epoch != ResolutionEpoch::UNRESOLVED
    }

    /// The same resolution as the transaction's `epoch`-th (the transaction numbers the
    /// resolutions it reads, design note §3.4).
    pub(crate) fn with_epoch(&self, epoch: NonZeroU64) -> Self {
        Self {
            epoch: ResolutionEpoch::resolved(epoch),
            ..self.clone()
        }
    }
}

/// The active companies: `allowed` (ordered; the first is the current company), always a subset
/// of the subject's companies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CompanyContext {
    allowed: Box<[CompanyId]>,
    all_of_subject: Box<[CompanyId]>,
    /// What the principal was built with: the companies an environment falls back to when its
    /// context names none (the context's absent `allowed_company_ids`).
    baseline: Box<[CompanyId]>,
}

impl CompanyContext {
    fn new(allowed: Vec<CompanyId>, resolution: &Resolution) -> Result<Self, PrincipalError> {
        for (i, company) in allowed.iter().enumerate() {
            if allowed[..i].contains(company) {
                return Err(PrincipalError::DuplicateCompany {
                    company: company.get(),
                });
            }
            if resolution.all_companies.binary_search(company).is_err() {
                return Err(PrincipalError::UnauthorizedCompanies);
            }
        }
        Ok(Self {
            baseline: allowed.clone().into_boxed_slice(),
            allowed: allowed.into_boxed_slice(),
            all_of_subject: resolution.all_companies.clone(),
        })
    }

    /// The allowed companies, current first.
    pub fn allowed(&self) -> &[CompanyId] {
        &self.allowed
    }

    /// The current company.
    pub fn current(&self) -> Option<CompanyId> {
        self.allowed.first().copied()
    }

    /// All companies of the subject, sorted.
    pub fn all_of_subject(&self) -> &[CompanyId] {
        &self.all_of_subject
    }
}

/// Records a link principal may reach beyond the public user's rights. adds
/// `Link(LinkGrant)` with its ADR ("Principals and scoped elevation"); share grants are read by
/// sub-select and never carried here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GrantBinding {
    /// No grant binding.
    None,
}

/// Who an environment acts. Built by [`PrincipalBuilder`] (validated)
/// or by the core; compared and hashed by value.
///
/// ```
/// use erp_orm::{CompanyId, GroupSet, PrincipalBuilder, PrincipalKind, Resolution, UserId};
/// use std::num::NonZeroU64;
///
/// let (c1, c2) = (CompanyId::new(1)?, CompanyId::new(2)?);
/// let resolution = Resolution::in_id_order(GroupSet::default(), [c1, c2], NonZeroU64::MIN)?;
/// let principal = PrincipalBuilder::user(UserId::new(7)?)
///     .resolution(resolution)
///     .companies(vec![c2, c1])
///     .build()?;
/// assert_eq!(principal.kind(), PrincipalKind::User);
/// assert_eq!(principal.companies().current(), Some(c2));
/// # Ok::<(), erp_orm::PrincipalError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Principal {
    kind: PrincipalKind,
    subject: UserId,
    actor: Option<Actor>,
    resolution: Arc<Resolution>,
    companies: CompanyContext,
    scope: Option<Scope>,
    grants: GrantBinding,
    elevation: Elevation,
    debug: bool,
}

impl Principal {
    /// The `System` principal: subject [`SUPERUSER_ID`], `Full` elevation. Core only (not exported
    /// by `erp-sdk`).
    pub(crate) fn system() -> Self {
        Self {
            kind: PrincipalKind::System,
            subject: SUPERUSER_ID,
            actor: None,
            resolution: Arc::new(Resolution::unresolved()),
            companies: CompanyContext::default(),
            scope: None,
            grants: GrantBinding::None,
            elevation: Elevation::Full,
            debug: false,
        }
    }

    /// The principal a bare uid stands for: `System` for the superuser, otherwise
    /// an unresolved user principal.
    pub(crate) fn for_uid(subject: UserId) -> Self {
        if subject == SUPERUSER_ID {
            Self::system()
        } else {
            Self {
                kind: PrincipalKind::User,
                elevation: Elevation::None,
                subject,
                ..Self::system()
            }
        }
    }

    /// The same principal with other allowed companies (current first), validated against the
    /// subject's companies (an environment applies its context's
    /// `allowed_company_ids` this way). An empty `allowed` is refused: the
    /// default order is [`Principal::with_default_companies`]. The `System` principal the core created and unresolved
    /// principals have no companies to validate against and accept any duplicate-free list; a
    /// resolver-backed transaction still checks it against the newest resolution before every
    /// decision ([`crate::Transaction::companies`]). A `System` principal made by
    /// `with_user(SUPERUSER_ID)` is bounded by the companies of the principal that switched.
    ///
    /// ```
    /// use erp_orm::{CompanyId, GroupSet, PrincipalBuilder, Resolution, UserId};
    /// use std::num::NonZeroU64;
    /// let (c1, c2, c3) = (CompanyId::new(1)?, CompanyId::new(2)?, CompanyId::new(3)?);
    /// let principal = PrincipalBuilder::user(UserId::new(7)?)
    ///     .resolution(Resolution::in_id_order(GroupSet::default(), [c1, c2], NonZeroU64::MIN)?)
    ///     .companies(vec![c1])
    ///     .build()?;
    /// assert_eq!(principal.with_companies(vec![c2, c1])?.companies().current(), Some(c2));
    /// assert_eq!(principal.with_default_companies().companies().allowed(), [c1, c2]);
    /// assert_eq!(
    ///     principal.with_companies(vec![c3]).unwrap_err().to_string(),
    ///     "One of the selected companies does not exist or is not one of yours."
    /// );
    /// # Ok::<(), erp_orm::PrincipalError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`PrincipalError::NoCompanies`] for an empty list; [`PrincipalError::UnauthorizedCompanies`]
    /// (kind `Access`) when a company is not one of the
    /// subject's; [`PrincipalError::DuplicateContextCompany`] (kind `Validation`) when one repeats.
    pub fn with_companies(&self, allowed: Vec<CompanyId>) -> Result<Self, PrincipalError> {
        if allowed.is_empty() {
            return Err(PrincipalError::NoCompanies);
        }
        // whose companies bound the list: this principal's, or for a `System` principal reached
        // by `with_user(SUPERUSER_ID)` those of the principal that switched (review S2); only a
        // `System` principal the core created takes any list
        let authority = self
            .company_authority()
            .map(|a| &a.resolution)
            .filter(|r| r.is_resolved());
        let companies = if let Some(resolution) = authority {
            CompanyContext {
                baseline: self.companies.baseline.clone(),
                ..CompanyContext::new(allowed, resolution).map_err(|error| match error {
                    PrincipalError::DuplicateCompany { company } => {
                        PrincipalError::DuplicateContextCompany { company }
                    }
                    other => other,
                })?
            }
        } else {
            // nothing to validate against: the transaction's resolution decides later
            if let Some(i) = (1..allowed.len()).find(|&i| allowed[..i].contains(&allowed[i])) {
                return Err(PrincipalError::DuplicateContextCompany {
                    company: allowed[i].get(),
                });
            }
            CompanyContext {
                allowed: allowed.into_boxed_slice(),
                all_of_subject: self.resolution.all_companies.clone(),
                baseline: self.companies.baseline.clone(),
            }
        };
        Ok(Self {
            companies,
            ..self.clone()
        })
    }

    /// The same principal with the subject's default companies: the main company first, then the
    /// others in `res.company` order (the resolution's default order). What an empty
    /// `allowed_company_ids` means; [`Principal::with_companies`] refuses an
    /// empty list instead of guessing.
    ///
    /// ```
    /// use erp_orm::{CompanyId, GroupSet, PrincipalBuilder, Resolution, UserId};
    /// use std::num::NonZeroU64;
    /// let (c1, c2) = (CompanyId::new(1)?, CompanyId::new(2)?);
    /// let principal = PrincipalBuilder::user(UserId::new(7)?)
    ///     .resolution(
    ///         Resolution::in_id_order(GroupSet::default(), [c1, c2], NonZeroU64::MIN)?
    ///             .with_default_order(vec![c2, c1])?,
    ///     )
    ///     .companies(vec![c1])
    ///     .build()?;
    /// assert_eq!(principal.with_default_companies().companies().allowed(), [c2, c1]);
    /// # Ok::<(), erp_orm::PrincipalError>(())
    /// ```
    #[must_use]
    pub fn with_default_companies(&self) -> Self {
        Self {
            companies: CompanyContext {
                allowed: self.resolution.default_companies.clone(),
                all_of_subject: self.resolution.all_companies.clone(),
                baseline: self.companies.baseline.clone(),
            },
            ..self.clone()
        }
    }

    /// The principal whose companies bound this one's: this principal, or for a `System`
    /// principal that `with_user(SUPERUSER_ID)` made from another principal, the one that
    /// switched (the chain never grows). `None` for a `System` principal the core created, the
    /// only one that takes any well-formed company list.
    pub(crate) fn company_authority(&self) -> Option<&Principal> {
        match (self.kind, &self.actor) {
            (PrincipalKind::System, None) => None,
            (PrincipalKind::System, Some(Actor::Impersonator(original))) => {
                original.company_authority()
            }
            _ => Some(self),
        }
    }

    /// The same principal with the companies it was built with: what an environment uses when
    /// its context names no `allowed_company_ids`.
    pub(crate) fn with_baseline_companies(&self) -> Self {
        Self {
            companies: CompanyContext {
                allowed: self.companies.baseline.clone(),
                ..self.companies.clone()
            },
            ..self.clone()
        }
    }

    /// A stable byte encoding of what decides this principal's access besides its resolution
    /// (the binding key without the snapshot and group
    /// set ids): kind, subject, actor, scope, allowed companies, grants, elevation and debug.
    /// Two principals with equal encodings are bound identically for the same resolution epoch
    /// and policy generation. Stored digests of it survive restarts and upgrades (the RPC
    /// idempotency store compares them), so the format is versioned and
    /// written field by field, never derived from `Debug` or `Hash`.
    pub fn binding_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        let mut put = |tag: &str, value: &str| {
            out.extend_from_slice(tag.as_bytes());
            out.push(b'=');
            out.extend_from_slice(&(value.len() as u64).to_be_bytes());
            out.extend_from_slice(value.as_bytes());
        };
        put("v", "1");
        put("kind", self.kind.as_str());
        put("subject", &self.subject.get().to_string());
        match &self.actor {
            None => put("actor", "none"),
            Some(Actor::ApiKey { id, kind, name }) => {
                put("actor", "api_key");
                put("actor.id", &id.get().to_string());
                put(
                    "actor.kind",
                    match kind {
                        KeyKind::User => "user",
                        KeyKind::Agent => "agent",
                        KeyKind::Service => "service",
                    },
                );
                put("actor.name", name);
            }
            Some(Actor::Impersonator(original)) => {
                put("actor", "impersonator");
                let inner: String = original
                    .binding_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                put("actor.principal", &inner);
            }
            Some(Actor::Link { id }) => {
                put("actor", "link");
                put("actor.link", &id.get().to_string());
            }
        }
        match &self.scope {
            None => put("scope", "none"),
            Some(scope) => {
                put("scope", "key");
                put("scope.key", &scope.key().get().to_string());
                put("scope.name", scope.name());
                put("scope.read_only", if scope.read_only() { "1" } else { "0" });
                put("scope.models", &scope.models().encode());
                put("scope.methods", &scope.methods().encode());
            }
        }
        let companies: Vec<String> = self
            .companies
            .allowed()
            .iter()
            .map(|c| c.get().to_string())
            .collect();
        put("companies", &companies.join(","));
        match &self.grants {
            GrantBinding::None => put("grants", "none"),
        }
        match &self.elevation {
            Elevation::None => put("elevation", "none"),
            Elevation::Full => put("elevation", "full"),
            Elevation::Scoped { scope, .. } => {
                // never at a request's entry; encoded by value so two blocks opening the same
                // scope bind alike
                put("elevation", "scoped");
                put("elevation.scope", &format!("{scope:?}"));
            }
        }
        put("debug", if self.debug { "1" } else { "0" });
        out
    }

    /// The same principal with debug mode on or off.
    pub(crate) fn with_debug(&self, debug: bool) -> Self {
        Self {
            debug,
            ..self.clone()
        }
    }

    /// The same principal with `elevation`.
    pub(crate) fn with_elevation(&self, elevation: Elevation) -> Self {
        Self {
            elevation,
            ..self.clone()
        }
    }

    /// `with_user(subject)`: the same subject keeps the principal and drops elevation
    /// (except the superuser, always `Full`); the original impersonator's subject gives back that
    /// original principal under the same elevation rule; another subject gives
    /// [`Principal::for_uid`] with this principal (or its original impersonator) as actor.
    pub(crate) fn switched_to(&self, subject: UserId) -> Self {
        let elevation = if subject == SUPERUSER_ID {
            Elevation::Full
        } else {
            Elevation::None
        };
        if subject == self.subject {
            return self.with_elevation(elevation);
        }
        let actor = match &self.actor {
            Some(Actor::Impersonator(original)) if original.subject == subject => {
                return original.with_elevation(elevation);
            }
            Some(Actor::Impersonator(original)) => Actor::Impersonator(original.clone()),
            Some(Actor::ApiKey { .. } | Actor::Link { .. }) | None => {
                Actor::Impersonator(Box::new(self.clone()))
            }
        };
        Self {
            actor: Some(actor),
            debug: self.debug,
            ..Self::for_uid(subject)
        }
    }

    /// The kind.
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }

    /// The subject (`env.uid()`).
    pub fn subject(&self) -> UserId {
        self.subject
    }

    /// Who acts, when it is not the subject.
    pub fn actor(&self) -> Option<&Actor> {
        self.actor.as_ref()
    }

    /// The subject's resolution.
    pub fn resolution(&self) -> &Arc<Resolution> {
        &self.resolution
    }

    /// The company context the principal was built with. It is **not** what an environment acts
    /// with: the context's `allowed_company_ids` replaces it and may be refused, and the
    /// transaction validates it against the newest resolution. Read the active companies from
    /// [`crate::Env::companies`] (or [`crate::Transaction::companies`] for a decision), never from
    /// here (review T3).
    pub fn companies(&self) -> &CompanyContext {
        &self.companies
    }

    /// The API-key scope (`None`: unscoped).
    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }

    /// The grant binding.
    pub fn grants(&self) -> &GrantBinding {
        &self.grants
    }

    /// The elevation.
    pub fn elevation(&self) -> &Elevation {
        &self.elevation
    }

    /// Whether `base.group_no_one` counts (SEC-025).
    pub fn debug(&self) -> bool {
        self.debug
    }
}

/// Builds a [`Principal`] with validation (DN §2.1).
#[derive(Debug)]
#[must_use = "call `.build()`"]
pub struct PrincipalBuilder {
    subject: UserId,
    resolution: Option<Resolution>,
    companies: Vec<CompanyId>,
    scope: Option<Scope>,
    debug: bool,
}

impl PrincipalBuilder {
    /// A user principal for `subject`. Without [`PrincipalBuilder::resolution`] it is unresolved.
    pub fn user(subject: UserId) -> Self {
        Self {
            subject,
            resolution: None,
            companies: Vec::new(),
            scope: None,
            debug: false,
        }
    }

    /// An agent principal (API key acting for `subject`).
    ///
    /// # Errors
    ///
    /// Always [`PrincipalError::Unsupported`] until.
    pub fn agent(_subject: UserId, _key: ApiKeyId) -> Result<Self, PrincipalError> {
        Err(PrincipalError::Unsupported {
            feature: "agent principals",
            task: "a later release",
        })
    }

    /// A service principal (API key of the service user `subject`).
    ///
    /// # Errors
    ///
    /// Always [`PrincipalError::Unsupported`] until.
    pub fn service(_subject: UserId, _key: ApiKeyId) -> Result<Self, PrincipalError> {
        Err(PrincipalError::Unsupported {
            feature: "service principals",
            task: "a later release",
        })
    }

    /// The public principal of the request's public user.
    ///
    /// # Errors
    ///
    /// Always [`PrincipalError::Unsupported`] until.
    pub fn public(_public_user: UserId) -> Result<Self, PrincipalError> {
        Err(PrincipalError::Unsupported {
            feature: "public principals",
            task: "a later release",
        })
    }

    /// A link principal.
    ///
    /// # Errors
    ///
    /// Always [`PrincipalError::Unsupported`] until.
    pub fn link(_link: LinkId, _public_user: UserId) -> Result<Self, PrincipalError> {
        Err(PrincipalError::Unsupported {
            feature: "link principals",
            task: "a later release",
        })
    }

    /// The subject's resolution ('s resolver reads it).
    pub fn resolution(mut self, resolution: Resolution) -> Self {
        self.resolution = Some(resolution);
        self
    }

    /// The allowed companies, current first (default: none).
    pub fn companies(mut self, allowed: Vec<CompanyId>) -> Self {
        self.companies = allowed;
        self
    }

    /// Debug mode (`base.group_no_one` counts; default off).
    pub fn debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// The scope of the `user`-kind API key the request authenticated with (
    /// sets it, the RPC gates read it). Its validated key identity also determines
    /// [`Actor::ApiKey`]; callers cannot supply a separate actor. Default: unscoped (a session).
    pub fn scope(mut self, scope: Scope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// The principal.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::SuperuserAsUser`] for [`SUPERUSER_ID`];
    /// [`PrincipalError::UnauthorizedCompanies`] (kind `Access`) when an allowed company is not
    /// one of the resolution's companies (an unresolved principal has none);
    /// [`PrincipalError::DuplicateCompany`] when an allowed company repeats.
    pub fn build(self) -> Result<Principal, PrincipalError> {
        if self.subject == SUPERUSER_ID {
            return Err(PrincipalError::SuperuserAsUser);
        }
        // absent, not wrong: a principal nobody resolved is the unresolved one (DN §2.1, the
        // harness's `as_user`), and refuses it under a compiled policy
        let resolution = self.resolution.unwrap_or_else(Resolution::unresolved);
        let companies = CompanyContext::new(self.companies, &resolution)?;
        let actor = self.scope.as_ref().map(|scope| Actor::ApiKey {
            id: scope.key(),
            kind: KeyKind::User,
            name: scope.name().into(),
        });
        Ok(Principal {
            kind: PrincipalKind::User,
            subject: self.subject,
            actor,
            resolution: Arc::new(resolution),
            companies,
            scope: self.scope,
            grants: GrantBinding::None,
            elevation: Elevation::None,
            debug: self.debug,
        })
    }
}

/// What a transaction is opened for ([`crate::Orm::begin`]); resolves it against the
/// database, later slices add agent, service, public and link hints.
///
/// ```
/// use erp_orm::{PrincipalHint, PrincipalKind, SUPERUSER_ID, UserId};
/// assert_eq!(PrincipalHint::from(UserId::new(5)?).kind(), PrincipalKind::User);
/// assert_eq!(PrincipalHint::from(SUPERUSER_ID).kind(), PrincipalKind::System);
/// # Ok::<(), erp_orm::PrincipalError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PrincipalHint {
    subject: UserId,
}

impl PrincipalHint {
    /// The `System` principal.
    pub fn system() -> Self {
        Self {
            subject: SUPERUSER_ID,
        }
    }

    /// The user `subject` (the superuser is `System`).
    pub fn user(subject: UserId) -> Self {
        Self { subject }
    }

    /// The hint addon code opens a transaction or an environment with for a caller (
    /// A4): always a `User` principal, never `System`. Uid 0 cannot reach it
    /// ([`UserId::new`] refuses it).
    ///
    /// ```
    /// use erp_orm::{PrincipalHint, PrincipalKind, SUPERUSER_ID, UserId};
    /// assert_eq!(PrincipalHint::caller(UserId::new(5)?)?.kind(), PrincipalKind::User);
    /// let refused = PrincipalHint::caller(SUPERUSER_ID).unwrap_err();
    /// assert_eq!(refused.kind(), erp_fields::ErrorKind::Access);
    /// # Ok::<(), erp_orm::PrincipalError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`PrincipalError::SuperuserAsCaller`] (kind `Access`) for [`SUPERUSER_ID`].
    pub fn caller(subject: UserId) -> Result<Self, PrincipalError> {
        if subject == SUPERUSER_ID {
            return Err(PrincipalError::SuperuserAsCaller);
        }
        Ok(Self { subject })
    }

    /// The kind of principal this hint gives.
    pub fn kind(self) -> PrincipalKind {
        if self.subject == SUPERUSER_ID {
            PrincipalKind::System
        } else {
            PrincipalKind::User
        }
    }

    /// The subject.
    pub fn subject(self) -> UserId {
        self.subject
    }

    /// The principal, without database I/O (resolves user principals).
    pub(crate) fn principal(self) -> Principal {
        Principal::for_uid(self.subject)
    }
}

impl From<UserId> for PrincipalHint {
    fn from(subject: UserId) -> Self {
        Self::user(subject)
    }
}

#[cfg(test)]
mod tests;
