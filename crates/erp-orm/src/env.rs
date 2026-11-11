//! Environments (ENV §4.1–4.3): immutable values of (registry, principal, context).
//!
//! An `Env` borrows nothing: data lives in the [`crate::Transaction`] that every operation takes
//! as `&mut`, and all environments of a transaction share its cache. Equal
//! parameters give equal environments. The [`Principal`] says who
//! acts; `uid` is its subject and `su` its `Full` elevation.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use erp_fields::{Error, Result, Value};
use erp_registry::Registry;

use crate::context::Context;
use crate::principal::{CompanyId, Elevation, Principal, PrincipalError, SUPERUSER_ID, UserId};
use crate::records::{Model, RecordSet, Records};

/// An environment: who acts (the principal) with which context, on which registry.
#[derive(Clone)]
pub struct Env {
    registry: Arc<Registry>,
    /// Trace metadata, excluded from environment equality/cache identity: never authorization.
    operation_identity: Option<Arc<crate::OperationIdentity>>,
    generation_attachments: Arc<crate::generation::GenerationAttachments>,
    principal: Arc<Principal>,
    context: Arc<Context>,
    /// The company context derived from `context` was refused: reported by every use of the
    /// companies, never by the call that built the environment.
    company_error: Option<CompanyRefusal>,
    /// How a company-bound refusal read in this environment may name the records it reached. Not
    /// part of the identity of the environment (it changes the wording of a refusal, nothing a
    /// cache holds).
    disclosure: Disclosure,
    /// The database lineage of the ORM that made this environment (`None` for an environment
    /// made without one, in unit tests); ignored by equality and hashing.
    lineage: Option<crate::transaction::Lineage>,
    /// The consumer of a privileged custom label. Only internal label resolution sets this
    /// immutable snapshot; input context cannot supply authority. Nested labels reuse the
    /// same snapshot rather than retaining a chain of environments.
    label_reader: Option<Arc<Env>>,
}

/// Whether a read in an [`Env`] was asked for by the caller or reached through a link (review
/// TD3, replacing the former `label_reads` counter of the engine): a company-bound refusal names
/// the hop record only for a direct read; the label of a link's target was reached through the
/// link, and the reader may not see the record behind its hop by id (review SD4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Disclosure {
    /// The caller asked for these records.
    #[default]
    Direct,
    /// The records are the targets of a link whose label is being read.
    ThroughLink,
}

/// Why the companies named by a context were refused (kept in the environment, reported lazily).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CompanyRefusal {
    /// A company the subject is not in.
    Unauthorized,
    /// A value that is not a list of company ids: the `Validation` error, kept as
    /// the context reported it (review F2).
    Malformed(String),
    /// A company listed twice.
    Duplicate(i64),
    /// Any other principal error (the message is kept as `Programming`).
    Other(String),
}

impl CompanyRefusal {
    fn of(error: &PrincipalError) -> Self {
        match error {
            PrincipalError::UnauthorizedCompanies => Self::Unauthorized,
            PrincipalError::DuplicateContextCompany { company } => Self::Duplicate(*company),
            other => Self::Other(other.to_string()),
        }
    }

    fn to_error(&self) -> Error {
        match self {
            Self::Unauthorized => PrincipalError::UnauthorizedCompanies.into(),
            Self::Malformed(message) => Error::validation(message.clone()),
            Self::Duplicate(company) => {
                PrincipalError::DuplicateContextCompany { company: *company }.into()
            }
            Self::Other(message) => Error::programming(message.clone()),
        }
    }
}

/// The principal that carries the companies `context` names, and the refusal when they are not
/// the subject's. The one place a context list becomes a company context:
/// absent (or `False`) keeps the companies the principal was built with, `[]` is the default
/// order (main company first), a list is validated against the subject's companies. On a
/// refusal the principal is kept as it was.
fn derive(
    principal: &Arc<Principal>,
    context: &Context,
) -> (Arc<Principal>, Option<CompanyRefusal>) {
    let derived = match context.allowed_company_ids() {
        Err(error) => {
            return (
                Arc::clone(principal),
                Some(CompanyRefusal::Malformed(error.message().to_owned())),
            );
        }
        Ok(None) => return (Arc::new(principal.with_baseline_companies()), None),
        Ok(Some([])) => return (Arc::new(principal.with_default_companies()), None),
        Ok(Some(ids)) => ids
            .iter()
            .map(|id| CompanyId::new(*id))
            .collect::<Result<Vec<_>, _>>()
            .and_then(|ids| principal.with_companies(ids)),
    };
    match derived {
        Ok(derived) => (Arc::new(derived), None),
        Err(error) => (Arc::clone(principal), Some(CompanyRefusal::of(&error))),
    }
}

impl Env {
    /// A new environment. Environments come from an [`crate::Orm`] or a [`crate::Transaction`],
    /// so their registry is the one the data uses.
    pub(crate) fn new(registry: Arc<Registry>, principal: Principal, context: Context) -> Self {
        let (principal, company_error) = derive(&Arc::new(principal), &context);
        Self {
            generation_attachments: Arc::default(),
            registry,
            operation_identity: None,
            principal,
            context: Arc::new(context),
            company_error,
            disclosure: Disclosure::Direct,
            lineage: None,
            label_reader: None,
        }
    }

    /// Read a required immutable product of this exact registry generation.
    ///
    /// Missing products fail explicitly; this lookup grants no caller authority.
    pub fn generation_attachment<T: std::any::Any + Send + Sync>(&self) -> Result<Arc<T>> {
        self.generation_attachments.get::<T>()
    }

    pub(crate) fn with_generation_attachments(
        mut self,
        attachments: Arc<crate::generation::GenerationAttachments>,
    ) -> Self {
        self.generation_attachments = attachments;
        self
    }

    /// This environment tagged with the lineage of the ORM that makes it.
    #[must_use]
    pub(crate) fn with_lineage(mut self, lineage: crate::transaction::Lineage) -> Self {
        self.lineage = Some(lineage);
        self
    }

    /// Root correlation and execution id, when the entry point supplied them.
    /// Tracing metadata conveys no authority and is never read from context.
    pub fn operation_identity(&self) -> Option<&crate::OperationIdentity> {
        self.operation_identity.as_deref()
    }

    pub(crate) fn with_operation_identity(
        mut self,
        identity: Option<Arc<crate::OperationIdentity>>,
    ) -> Self {
        self.operation_identity = identity;
        self
    }

    /// Whether `orm` is of the lineage this environment was made in: both come, directly or
    /// through [`crate::Orm::rebuild`], from one [`crate::Orm::builder`]. `false` for an
    /// environment of another ORM lineage, whatever its registry.
    pub fn same_lineage(&self, orm: &crate::Orm) -> bool {
        self.lineage
            .as_ref()
            .is_some_and(|lineage| orm.has_lineage(lineage))
    }

    /// Whether both environments originate from the same ORM lineage, including clones and
    /// rebuilt handles. This is the same identity relation as [`Self::same_lineage`], for
    /// resource leases created before a service has an ORM handle (authentication hashes).
    /// It grants no authority and is not an equality/cache key for environments. Environments
    /// without an ORM lineage do not match, even each other.
    pub fn shares_lineage(&self, other: &Self) -> bool {
        match (&self.lineage, &other.lineage) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// The registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// The same principal and context on another registry (`Transaction::switch_orm`).
    pub(crate) fn rebind(
        &self,
        registry: Arc<Registry>,
        attachments: Arc<crate::generation::GenerationAttachments>,
    ) -> Self {
        Self {
            registry,
            principal: Arc::clone(&self.principal),
            context: Arc::clone(&self.context),
            company_error: self.company_error.clone(),
            disclosure: self.disclosure,
            lineage: self.lineage.clone(),
            generation_attachments: attachments,
            label_reader: self.label_reader.as_ref().map(Arc::clone),
            operation_identity: self.operation_identity.clone(),
        }
    }

    /// The user id: the principal's subject.
    pub fn uid(&self) -> i64 {
        self.principal.subject().get()
    }

    /// This environment with another principal, unchecked: tests of memos keyed by the principal
    /// need two principals of one subject in one transaction. Never in production builds.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_principal_for_tests(&self, principal: Principal) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            principal: Arc::new(principal),
            context: Arc::clone(&self.context),
            company_error: self.company_error.clone(),
            disclosure: self.disclosure,
            lineage: self.lineage.clone(),
            generation_attachments: Arc::clone(&self.generation_attachments),
            label_reader: self.label_reader.as_ref().map(Arc::clone),
            operation_identity: self.operation_identity.clone(),
        }
    }

    /// The user id as a [`UserId`].
    pub fn user_id(&self) -> UserId {
        self.principal.subject()
    }

    /// Who acts.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Superuser mode: the principal's elevation is `Full`.
    pub fn su(&self) -> bool {
        self.principal.elevation().is_full()
    }

    /// Alias of [`Env::su`] (`is_superuser()`).
    pub fn is_superuser(&self) -> bool {
        self.su()
    }

    /// The context.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// `env.lang`: context `lang` (validation against active languages comes with `res.lang`).
    pub fn lang(&self) -> Option<&str> {
        self.context.lang()
    }

    /// `prefetch_langs` from the context (default false).
    pub fn prefetch_langs(&self) -> bool {
        self.context.prefetch_langs()
    }

    /// `env.tz`: context `tz` when it names a known zone, else UTC (the user's tz arrives with
    /// `res.users` in M3).
    pub fn tz(&self) -> &str {
        match self.context.tz() {
            Some(tz) if erp_fields::DateTime::now().offset_seconds(tz).is_some() => tz,
            Some(tz) => {
                tracing::warn!(target: "erp::orm", tz, "unknown timezone in the context; using UTC");
                "UTC"
            }
            None => "UTC",
        }
    }

    /// `fields.Date.context_today(record)` for this environment: today's date in [`Env::tz`]
    /// .
    ///
    /// # Errors
    ///
    /// `Internal` when `Env::tz` returned a zone the clock database does not know.
    pub fn context_today(&self) -> Result<erp_fields::Date> {
        erp_fields::Date::context_today(self.tz())
            .map_err(|e| Error::internal(e.message().to_owned()))
    }

    /// `env.companies`: the active companies, the current one first.
    /// Without the key (or with `False`) the
    /// companies stay those the principal was built with: the principal resolver builds them in
    /// the subject's default order (main company first), a principal built by hand keeps what its
    /// builder was given (none for an unresolved one); `[]` asks for the default order. Only the
    /// `System` principal the core created takes a list as given. This is the one source of the
    /// active companies: `Principal::companies` is what the principal was built with and may
    /// disagree. An environment from `with_user` has no companies of its own here (the subject is
    /// not resolved yet): [`crate::Transaction::companies`] resolves it and returns the subject's
    /// default companies when the context names none (review R2). No statement; code that decides something from the companies reads them with
    /// [`crate::Transaction::companies`], which also checks them against the newest resolution.
    ///
    /// ```no_run
    /// # fn demo(env: &erp_orm::Env) -> erp_fields::Result<()> {
    /// let env = env.with_company(2)?; // never fails here; a foreign company is refused below
    /// let companies = env.companies()?;
    /// assert_eq!(env.company()?, companies.first().copied());
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// `Access` ("Access to unauthorized or invalid companies.") when the context names a company
    /// the subject is not in; `Validation` when `allowed_company_ids` is not a list of company ids
    /// or a company is listed twice.
    pub fn companies(&self) -> Result<&[CompanyId]> {
        match &self.company_error {
            Some(refusal) => Err(refusal.to_error()),
            None => Ok(self.principal.companies().allowed()),
        }
    }

    /// `env.company`: the current company, the first of [`Env::companies`]; `None` for a `System`
    /// or unresolved principal whose context names none.
    ///
    /// ```no_run
    /// # fn demo(env: &erp_orm::Env) -> erp_fields::Result<()> {
    /// let current: Option<erp_orm::CompanyId> = env.company()?;
    /// # let _ = current; Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Env::companies`].
    pub fn company(&self) -> Result<Option<CompanyId>> {
        Ok(self.companies()?.first().copied())
    }

    /// `env.company_id()`: [`Env::company`] as a raw id.
    ///
    /// # Errors
    ///
    /// As [`Env::companies`].
    pub fn company_id(&self) -> Result<Option<i64>> {
        Ok(self.company()?.map(CompanyId::get))
    }

    /// `sudo(flag)`: same principal with elevation `Full` (on) or none (off). Turning it on
    /// strips `default_*` keys.
    #[must_use]
    pub fn sudo(&self, flag: bool) -> Self {
        let flag = flag || self.principal.subject() == SUPERUSER_ID;
        if flag == self.su() {
            return self.clone();
        }
        let (elevation, context) = if flag {
            (Elevation::Full, Arc::new(self.context.without_defaults()))
        } else {
            (Elevation::None, Arc::clone(&self.context))
        };
        Self {
            registry: Arc::clone(&self.registry),
            principal: Arc::new(self.principal.with_elevation(elevation)),
            context,
            company_error: self.company_error.clone(),
            disclosure: self.disclosure,
            lineage: self.lineage.clone(),
            generation_attachments: Arc::clone(&self.generation_attachments),
            label_reader: self.label_reader.as_ref().map(Arc::clone),
            operation_identity: self.operation_identity.clone(),
        }
    }

    /// This environment's context with the principal of `origin` without elevation (a
    /// superuser keeps its own): the transaction's original principal, for x2many commands
    /// on a model that does not allow sudo commands.
    #[must_use]
    pub(crate) fn acting_as(&self, origin: &Env) -> Self {
        let origin = origin.sudo(false);
        // the debug bit belongs to this environment (it gates `base.group_no_one`),
        // not to the transaction's stored principal
        let wanted = origin.principal.with_debug(self.principal.debug());
        // the origin's companies are not this environment's: the context decides again
        let (wanted, company_error) = derive(&Arc::new(wanted), &self.context);
        if *self.principal == *wanted && self.company_error == company_error {
            return self.clone();
        }
        Self {
            registry: Arc::clone(&self.registry),
            principal: wanted,
            context: Arc::clone(&self.context),
            company_error,
            disclosure: self.disclosure,
            lineage: self.lineage.clone(),
            generation_attachments: Arc::clone(&self.generation_attachments),
            label_reader: self.label_reader.as_ref().map(Arc::clone),
            operation_identity: self.operation_identity.clone(),
        }
    }

    /// `with_user(uid)`: `su` is false unless uid is the superuser; uid 0 keeps this environment
    /// . The current principal becomes the new one's actor (its original actor
    /// when it already impersonates someone). Takes an `i64` or a [`UserId`].
    ///
    /// The permission rule (only `System`, a holder of `principal.impersonate` or the subject
    /// itself may switch) and resolution of the new subject come with; until then the
    /// new principal is unresolved, as under today's static policies.
    #[must_use]
    pub fn with_user(&self, uid: impl Into<i64>) -> Self {
        let Some(subject) = UserId::from_raw(uid.into()) else {
            return self.clone();
        };
        // another subject has other companies: the context is applied to them
        let (principal, company_error) = derive(
            &Arc::new(self.principal.switched_to(subject)),
            &self.context,
        );
        Self {
            registry: Arc::clone(&self.registry),
            principal,
            context: Arc::clone(&self.context),
            company_error,
            disclosure: Disclosure::Direct,
            lineage: self.lineage.clone(),
            generation_attachments: Arc::clone(&self.generation_attachments),
            label_reader: self.label_reader.as_ref().map(Arc::clone),
            operation_identity: self.operation_identity.clone(),
        }
    }

    /// The same environment in debug mode (`debug` true: `base.group_no_one` counts,;
    /// the request layer sets it from the session) or out of it. Debug mode adds the `User:` and
    /// `Groups:` lines to field access errors for the policy's debug users.
    #[must_use]
    pub fn with_debug(&self, debug: bool) -> Self {
        if self.principal.debug() == debug {
            return self.clone();
        }
        Self {
            registry: Arc::clone(&self.registry),
            principal: Arc::new(self.principal.with_debug(debug)),
            context: Arc::clone(&self.context),
            company_error: self.company_error.clone(),
            disclosure: self.disclosure,
            lineage: self.lineage.clone(),
            generation_attachments: Arc::clone(&self.generation_attachments),
            label_reader: self.label_reader.as_ref().map(Arc::clone),
            operation_identity: self.operation_identity.clone(),
        }
    }

    /// `with_context(**kw)`: the current context updated with `updates`.
    #[must_use]
    pub fn with_context<K: Into<String>>(
        &self,
        updates: impl IntoIterator<Item = (K, Value)>,
    ) -> Self {
        let mut context = Context::clone(&self.context);
        for (k, v) in updates {
            context.set(k, v);
        }
        self.with_context_value(context)
    }

    /// `with_context(ctx)`: replace the context; `allowed_company_ids` is kept from the current one
    /// when `ctx` lacks it.
    #[must_use]
    pub fn with_context_replaced(&self, mut context: Context) -> Self {
        if !context.contains("allowed_company_ids")
            && let Some(ids) = self.context.get("allowed_company_ids")
        {
            context.set("allowed_company_ids", ids);
        }
        self.with_context_value(context)
    }

    /// `with_company(company)`: move `company` to the front of the active companies; 0 keeps this
    /// environment.
    /// Works on an
    /// elevated environment too, which stays validated.
    ///
    /// # Errors
    ///
    /// `Programming` for a negative id; the refusal of this environment's own companies when it
    /// already has one ([`Env::companies`]).
    pub fn with_company(&self, company: i64) -> Result<Self> {
        if company == 0 {
            return Ok(self.clone());
        }
        if company < 0 {
            return Err(Error::programming(format!(
                "with_company: invalid company id {company}"
            )));
        }
        let current = self.companies()?;
        if current.first().map(|c| c.get()) == Some(company) {
            return Ok(self.clone());
        }
        let mut ids = vec![company];
        ids.extend(current.iter().map(|c| c.get()).filter(|c| *c != company));
        Ok(self.with_context([(
            "allowed_company_ids",
            Value::list(ids.into_iter().map(Value::Int)),
        )]))
    }

    fn with_context_value(&self, context: Context) -> Self {
        let (principal, company_error) = if self.context.same_company_ids(&context) {
            (Arc::clone(&self.principal), self.company_error.clone())
        } else {
            derive(&self.principal, &context)
        };
        Self {
            registry: Arc::clone(&self.registry),
            principal,
            context: Arc::new(context),
            company_error,
            disclosure: self.disclosure,
            lineage: self.lineage.clone(),
            generation_attachments: Arc::clone(&self.generation_attachments),
            label_reader: self.label_reader.as_ref().map(Arc::clone),
            operation_identity: self.operation_identity.clone(),
        }
    }

    /// This environment for reading the label of a link's target: a company-bound refusal inside
    /// names no hop id (review SD4, TD3).
    #[must_use]
    pub(crate) fn through_link(&self) -> Self {
        Self {
            disclosure: Disclosure::ThroughLink,
            ..self.clone()
        }
    }

    /// Preserve the consumer's company boundary while an internal label read bypasses
    /// ordinary target permissions. Direct caller `sudo()` has no such consumer snapshot.
    pub(crate) fn with_label_reader(&self, reader: &Env) -> Self {
        if !self.su() || reader.company_bound_reader().su() {
            return self.clone();
        }
        let reader = match &reader.label_reader {
            Some(reader) => Arc::clone(reader),
            None => Arc::new(reader.through_link()),
        };
        Self {
            label_reader: Some(reader),
            disclosure: Disclosure::ThroughLink,
            ..self.clone()
        }
    }

    /// The original consumer for company-bound related reads and their cache identity.
    /// Ordinary environments are their own reader; the snapshot never contains another one.
    pub(crate) fn company_bound_reader(&self) -> &Env {
        match &self.label_reader {
            Some(reader) => reader,
            None => self,
        }
    }

    /// Whether the records read here are the ones the caller asked for (not a link's target read
    /// for its label).
    pub(crate) fn is_direct(&self) -> bool {
        self.disclosure == Disclosure::Direct
    }

    /// `env[model]`: an empty recordset of a model by name.
    pub fn records(&self, model: &str) -> Result<Records> {
        let id = self.registry.model_id(model).ok_or_else(|| {
            Error::programming(format!("Model '{model}' does not exist in registry."))
        })?;
        Ok(Records::empty(self.clone(), id))
    }

    /// `env[M]`: an empty typed recordset.
    pub fn model<M: Model>(&self) -> Result<RecordSet<M>> {
        // looked up by M's own name: no inheritance check needed
        self.records(M::MODEL_NAME).map(RecordSet::unchecked)
    }
}

impl PartialEq for Env {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.registry, &other.registry)
            && (Arc::ptr_eq(&self.principal, &other.principal) || self.principal == other.principal)
            && self.context == other.context
            && self.company_error == other.company_error
            && self.label_reader == other.label_reader
    }
}

impl Eq for Env {}

impl Hash for Env {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.principal.hash(state);
        self.context.hash(state);
        self.company_error.hash(state);
        self.label_reader.hash(state);
    }
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("uid", &self.uid())
            .field("su", &self.su())
            .field("principal", &self.principal)
            .field("context", &self.context.to_map())
            .finish_non_exhaustive()
    }
}
