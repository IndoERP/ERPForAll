//! The authentication service of one database (§14.4): password
//! login, the session check in the transaction's opening flight, rotation, touch, logout and
//! collection.
//!
//! [`Auth`] holds the database's ORM, the process settings ([`AuthConfig`]), the hashing pool,
//! the address pre-filter, the login admission and a cache of the database settings and session
//! keys. The cache is labelled with the `config_parameter` ormcache generation read **before**
//! the statement that filled it, so a write committed while it loaded leaves it stale (reloaded
//! next time), never current with old values.
//!
//! A login is two short transactions with no connection held between them (lead decision 9,
//! database review H1/H2): **phase A** reads the throttle rows, the trusted pair, the user and the
//! installed languages in one READ COMMITTED READ ONLY flight ([`Orm::one_flight`]); the hash runs
//! on the pool with no connection; **phase B** writes the verdict (the failure count, or the
//! success with the rehash and the session row, both conditioned on the hash phase A read) in one
//! READ COMMITTED flight. Two round trips with the settings cached. When an addon overrides
//! `_check_credentials`, the override needs an ORM transaction, so phase A is a REPEATABLE READ
//! transaction kept open through the check (and committed with a success).
//!
//! Phase B (either statement), rotation and touch are one raw READ COMMITTED statement each and
//! retry a deadlock (40P01) up to three tries after 5 – 25 ms ([`super::retry`]; database review
//! round 3 DD-1); logout, the deletion after a refused check and the sessions a login ends run the
//! session-end hooks in a READ COMMITTED transaction and retry a deadlock the same way (database
//! review round 2 N9). Every absorbed deadlock is logged at `warn` and counted
//! ([`AuthStats::deadlock_retries`]). Hooks use raw statements only, on every path: READ
//! COMMITTED refuses the record API ([`super::hooks`]).

use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use erp_fields::{Condition, Error, ErrorKind, Result};
use erp_orm::node::{FeatureBinding, FeatureKey, FeatureSnapshot, FeatureVersion, NodeAdmission};
use erp_orm::{
    ApiKeyId, BeginOptions, CommitError, CredentialBegin, CredentialOptions, FinalStatement,
    Isolation, Orm, SUPERUSER_ID, Timeouts, Transaction, UserId,
};
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, oneshot};
use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};
use zeroize::Zeroizing;

use super::context::{DEFAULT_LANG, UserContext, context_lang};
use super::cooldown_policy::{LoginCooldownContext, LoginCooldownPolicy};
use super::credential::{self, AuthInfo, CheckEnv, Credential, StoredPassword};
use super::dummy;
use super::hooks::EndReason;
use super::password::{self, HashParams};
use super::pool::{self, HashPool, HashPoolError};
use super::retry::{DEADLOCK, DeadlockRetries, RetryCounters, absorb, is_retried, retry};
use super::schema;
use super::session::{
    self, Cached, CheckedSession, Refusal, SessionCheck, SessionId, SessionSecret, TokenFields,
    Touch,
};
use super::settings::{self, AuthConfig, AuthSettings, CACHE_NAME, Cooldown};
use super::store;
use super::throttle::{self, AddressKey, Prefilter, ThrottleHasher, ThrottleKey};
use super::warn::WarnLimit;

mod identity;
mod selection;
use super::token_plan::{
    COLUMNS_MARKER, GUARD_MARKER, SessionTokenFields, TokenPlan, TokenValues, replace_once,
};
use crate::resolver::Resolver;
pub use identity::{IdentityCheck, PasswordAuthorization, PasswordReset};
use selection::{CandidateRecord, LoginSelectionPlan, select_candidate};
pub use selection::{
    LoginDomain, LoginLookup, LoginOrder, LoginOrderTerm, LoginSelectionContext,
    LoginSelectionLimits, OrderDirection, SelectionWitness,
};

pub const COOLDOWN_MESSAGE: &str =
    "Too many login failures, please wait a bit before trying again.";

/// Sessions one user keeps at most (lead decision 7): a login past it ends the user's oldest
/// sessions, with the session-end hooks.
pub const SESSIONS_PER_USER: u32 = 100;

/// The entry name of the login's read-only phase A (named by the read-only guard's errors).
const PHASE_A_ENTRY: &str = "res.users.authenticate";

/// The entry name of the cooldown hook's read-only transaction (named by the read-only guard's
/// errors).
const COOLDOWN_ENTRY: &str = "res.users._on_login_cooldown";

/// What a failed cooldown hook reports to the caller: the hook's own text
/// goes to the `erp::auth` log, never to the response.
const COOLDOWN_HOOK_FAILED: &str = "the login cooldown policy could not be evaluated";

/// The entry name of the settings reload (named by the read-only guard's errors).
const SETTINGS_ENTRY: &str = "ir.config_parameter.auth_settings";

/// Where a login attempt comes from: chosen by the caller every time, never a default (security
/// review round 2 S7: a route that forgot the address must not turn the address throttle off by
/// accident).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAddr {
    /// The client address after the trusted-proxy rule (maps it): stored on the
    /// session row as is; its key (IPv6 collapsed to the /64) is the throttle's, the pre-filter's
    /// and the trusted pair's.
    Known(IpAddr),
    /// No client address: an in-process caller (a command-line tool, a job, a test), named by
    /// the reason, which the logs show. Only the per-login rule applies: no address cooldown, no
    /// pre-filter, no trusted pair. An HTTP route never uses it.
    InProcess(&'static str),
}

/// A password login attempt.
pub struct LoginRequest {
    login: String,
    password: Zeroizing<String>,
    client: ClientAddr,
    user_agent: Option<String>,
    interactive: bool,
}

impl LoginRequest {
    /// A login with `login` and `password` from `client`, interactive.
    pub fn new(login: impl Into<String>, password: impl Into<String>, client: ClientAddr) -> Self {
        Self {
            login: login.into(),
            password: Zeroizing::new(password.into()),
            client,
            user_agent: None,
            interactive: true,
        }
    }

    /// The client's `User-Agent`, cut to its first 1,024 characters here (as
    /// [`Touch::agent`] does), and stored on the session row.
    #[must_use]
    pub fn user_agent(mut self, agent: &str) -> Self {
        self.user_agent = Some(session::user_agent(agent));
        self
    }

    #[must_use]
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }
}

/// Never the login text, nor its length (users type passwords into the login field; security
/// review round 2 N1 and delta N-c).
impl fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginRequest")
            .field("login", &"<redacted>")
            .field("password", &"<redacted>")
            .field("client", &self.client)
            .field("interactive", &self.interactive)
            .finish_non_exhaustive()
    }
}

/// A session made by a login: the caller sets the cookie from [`NewSession::secret`].
#[derive(Debug)]
pub struct NewSession {
    secret: SessionSecret,
    id: SessionId,
    lifetime: Duration,
}

impl NewSession {
    /// The cookie secret (shown once, never stored).
    pub fn secret(&self) -> &SessionSecret {
        &self.secret
    }

    /// The row id.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The absolute lifetime of the row (`sessions.max_lifetime_seconds`).
    pub fn lifetime(&self) -> Duration {
        self.lifetime
    }
}

/// A successful login.
#[derive(Debug)]
pub struct Login {
    session: NewSession,
    auth: AuthInfo,
    context: UserContext,
}

impl Login {
    /// The user.
    pub fn uid(&self) -> UserId {
        self.auth.uid()
    }

    /// The session.
    pub fn session(&self) -> &NewSession {
        &self.session
    }

    /// What `_check_credentials` returned.
    pub fn auth(&self) -> &AuthInfo {
        &self.auth
    }

    /// The user's context (`res.users.context_get`), stored on the session row.
    pub fn context(&self) -> &UserContext {
        &self.context
    }
}

/// Why a login did not succeed.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoginError {
    /// Wrong credential, unknown or inactive login, empty password, an over-long login or
    /// password, a credential changed while it was verified, a stored hash that needs more
    /// memory than a permit of this node reserves (counted like a wrong password, security
    /// review round 2 delta SD-1), or a verify the pool could not start in time (its wait
    /// expired; not counted): one text for all (security review Low 6).
    Denied,
    /// The cooldown of the address or the login, or the pre-filter (answers 429).
    Cooldown,
    /// A wait of the login expired before anything about the credential was read (
    /// answers 503 with `Retry-After`): the login admission's (too many logins in progress;
    /// counted in [`AuthStats::admission_overloaded`]), or the settings read's, waiting for
    /// another caller's reload or running its own (counted in
    /// [`AuthStats::settings_wait_expired`] or [`AuthStats::settings_load_expired`]; security
    /// review round 2 delta N-b, round 3 R3-c). Nothing is counted against the login.
    Overloaded {
        /// The wait that expired (the hashing pool's queue wait, which bounds both), handed to the
        /// client as its `Retry-After` hint: how long this attempt waited, not an estimate of
        /// when a slot frees.
        retry_after: Duration,
    },
    /// Anything else (database, a malformed setting, an overridden `_check_credentials` that
    /// failed): nothing counted. Transparent: its `Display` is the cause's and its `source()` is
    /// the cause's source (type design T8).
    Failed(Error),
}

impl LoginError {
    fn retry_after_secs(retry_after: Duration) -> u64 {
        retry_after.as_secs().max(1)
    }
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str(credential::ACCESS_DENIED),
            Self::Cooldown => f.write_str(COOLDOWN_MESSAGE),
            Self::Overloaded { retry_after } => match Self::retry_after_secs(*retry_after) {
                1 => f.write_str("Server busy; retry after 1 second."),
                secs => write!(f, "Server busy; retry after {secs} seconds."),
            },
            Self::Failed(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for LoginError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failed(e) => std::error::Error::source(e),
            Self::Denied | Self::Cooldown | Self::Overloaded { .. } => None,
        }
    }
}

impl From<LoginError> for Error {
    fn from(e: LoginError) -> Self {
        match e {
            LoginError::Denied => credential::access_denied(),
            LoginError::Cooldown => Error::new(ErrorKind::AccessDenied, COOLDOWN_MESSAGE),
            LoginError::Overloaded { retry_after } => Error::new(
                ErrorKind::ResourceExhausted,
                LoginError::Overloaded { retry_after }.to_string(),
            )
            .with_condition(Condition::Overloaded),
            LoginError::Failed(e) => e,
        }
    }
}

/// A transaction opened by a session check, with the session it accepted.
pub struct SessionBegin {
    /// The request's transaction, acting as the session's user.
    pub tx: Transaction,
    /// The accepted session (rotation and touch due flags).
    pub session: CheckedSession,
}

/// A primary lifecycle session checked against policy rows from the same transaction.
/// This is authentication evidence, never an elevation or an ORM environment.
pub struct SessionAuthorization {
    principal: erp_orm::Principal,
    session: CheckedSession,
    policy: Arc<dyn erp_orm::AccessPolicy>,
}
impl SessionAuthorization {
    /// The current resolved human principal; no impersonator or system principal is substituted.
    pub fn principal(&self) -> &erp_orm::Principal {
        &self.principal
    }
    /// Identity of the checked session for an immutable lifecycle audit.
    pub fn session_id(&self) -> SessionId {
        self.session.id()
    }
    /// Test a group specification against the policy read with this session.
    /// # Errors
    /// Invalid group specifications or incomplete/corrupt policy data.
    pub fn has_groups(&self, groups: &str) -> Result<bool> {
        let binding = erp_orm::PrincipalBinding::bind(
            self.policy.as_ref(),
            &self.principal,
            Arc::clone(self.principal.resolution()),
        )?;
        self.policy.has_groups(&binding, groups)
    }

    /// The users among `users` this session's human may see under `res.users`' generated
    /// company restriction, with all of the human's companies active (
    /// Amendment 1): the resolution's default companies, whatever the company switcher shows.
    /// Missing and hidden users are both absent; input order, each once.
    /// # Errors
    /// As [`erp_orm::Transaction::visible_under_company_restriction`]: `Access` for a drifted
    /// generated restriction of `res.users`; `Programming` when `res.users` lost its declaration;
    /// statement errors.
    pub async fn visible_users(
        &self,
        tx: &mut Transaction,
        users: &[UserId],
    ) -> Result<Vec<UserId>> {
        let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
        let companies = self.principal.resolution().default_companies().to_vec();
        tx.visible_under_company_restriction(
            "res.users",
            &ids,
            self.policy.as_ref(),
            &self.principal,
            &companies,
        )
        .await?
        .into_iter()
        .map(|id| UserId::new(id).map_err(Error::from))
        .collect()
    }

    /// The users among `users` whose groups, closed with the implied groups of the policy read
    /// with this session, hold `base.group_system` (inactive users included); input order.
    /// # Errors
    /// `Programming` when the policy does not know `base.group_system`; a group row the policy
    /// does not know; statement errors.
    pub async fn settings_administrators(
        &self,
        tx: &mut Transaction,
        users: &[UserId],
    ) -> Result<Vec<UserId>> {
        let xmlid = erp_orm::QualifiedXmlId::parse("base.group_system")?;
        let system = match self.policy.resolve_group_ref(&xmlid)? {
            erp_orm::GroupRefResolution::Known { id, .. } => id,
            _ => {
                return Err(Error::programming(
                    "the access policy read with this session does not know base.group_system: \
                     a Settings administrator target cannot be decided",
                ));
            }
        };
        let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
        let rows = tx
            .query(
                "SELECT uid::int8, array_agg(gid::int8 ORDER BY gid) FROM res_groups_users_rel \
                  WHERE uid = ANY($1::int8[]) GROUP BY uid",
                &[&ids],
            )
            .await?;
        let mut administrators = std::collections::BTreeSet::new();
        for row in &rows {
            let uid: i64 = col(row, 0)?;
            let gids: Vec<i64> = col(row, 1)?;
            let direct = gids
                .into_iter()
                .map(|g| erp_orm::GroupId::new(g).map_err(Error::from))
                .collect::<Result<Vec<_>>>()?;
            if self.policy.groups(&direct)?.contains(system) {
                administrators.insert(uid);
            }
        }
        Ok(users
            .iter()
            .copied()
            .filter(|u| administrators.contains(&u.get()))
            .collect())
    }

    /// The company scope and Settings administrator rule of an administrator password reset (ADR
    /// 0138 Amendment 1). Each refusal of a target is the unknown-user answer
    /// ([`missing_reset_target`], Missing "A selected password reset user no longer exists.", so
    /// existence is not revealed):
    /// - the superuser row (`SUPERUSER_ID`) is never a target, for any actor;
    /// - every target must be visible to this session's human
    ///   ([`SessionAuthorization::visible_users`]; the `share` escape scopes portal users only);
    /// - when the actor does not hold `base.group_system`, every target, internal users included,
    ///   must share a company (`res_company_users_rel` or `res_users.company_id`) with the
    ///   resolution's default companies.
    ///
    /// Then a target holding `base.group_system` needs an actor holding it, else Access "Only a
    /// Settings administrator can reset the password of a Settings administrator.". The callers
    /// check existence and the manager authority themselves.
    /// # Errors
    /// The refusals above; the errors of the lookups.
    pub async fn check_reset_targets(
        &self,
        tx: &mut Transaction,
        targets: &[UserId],
    ) -> Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        if targets.contains(&SUPERUSER_ID) {
            return Err(missing_reset_target());
        }
        let visible = self.visible_users(tx, targets).await?;
        if targets.iter().any(|u| !visible.contains(u)) {
            return Err(missing_reset_target());
        }
        let settings_actor = self.has_groups("base.group_system")?;
        if !settings_actor {
            let ids: Vec<i64> = targets.iter().map(|u| u.get()).collect();
            let companies: Vec<i64> = self
                .principal
                .resolution()
                .default_companies()
                .iter()
                .map(|c| c.get())
                .collect();
            let rows = tx
                .query(
                    "SELECT count(*)::int8 FROM res_users u WHERE u.id = ANY($1::int8[]) \
                       AND (u.company_id = ANY($2::int8[]) OR EXISTS (SELECT FROM \
                            res_company_users_rel r WHERE r.user_id = u.id \
                              AND r.cid = ANY($2::int8[])))",
                    &[&ids, &companies],
                )
                .await?;
            let row = rows
                .first()
                .ok_or_else(|| Error::db("the reset company check returned no row"))?;
            let shared: i64 = col(row, 0)?;
            if usize::try_from(shared).ok() != Some(ids.len()) {
                return Err(missing_reset_target());
            }
        }
        if !settings_actor && !self.settings_administrators(tx, targets).await?.is_empty() {
            return Err(Error::access(
                "Only a Settings administrator can reset the password of a Settings administrator.",
            ));
        }
        Ok(())
    }
}

/// What the imported rows of one index say about a 40-hex key: the verified row, none, or a loud
/// refusal of the stored rows that still counts as a failed attempt.
type LegacyOutcome = std::result::Result<
    Option<super::apikey_legacy::LegacyMatch>,
    super::apikey_legacy::LoudRefusal,
>;

/// The answer for an unknown or refused password reset target, defined in `erp-orm` so the reset
/// line model of `base` gives the same one (Amendment 1).
pub use erp_orm::missing_reset_target;
impl fmt::Debug for SessionAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionAuthorization")
            .field("subject", &self.principal.subject())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SessionBegin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionBegin")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// Counters of an [`Auth`] (monotonic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AuthStats {
    /// Loads of the settings and keys.
    pub settings_loads: u64,
    /// Deletions after a refused check that failed (logged at `warn`; the row keeps failing).
    pub refusal_delete_failures: u64,
    /// Failed attempts whose writes could not complete: the failure count, or the rollback of
    /// the transaction an overridden `_check_credentials` ran in (logged at `error`; the attempt
    /// was still answered `Denied`, the database review's matrix).
    pub failure_write_errors: u64,
    /// Sessions a successful login should have ended (over [`SESSIONS_PER_USER`], or the other
    /// sessions after a rehash) whose deletion failed (logged at `error` after a rehash, at
    /// `warn` over the cap; the next login applies the cap again).
    pub session_end_failures: u64,
    /// Logins whose verdict task ended in [`LoginError::Failed`] or panicked (logged at
    /// `error` in the task, so the outcome is kept when the caller is gone).
    pub verdict_failures: u64,
    /// Attempts refused because the `_on_login_cooldown` hook failed or panicked.
    pub cooldown_hook_failures: u64,
    /// Logins answered [`LoginError::Overloaded`] because the admission's wait expired (admission
    /// pressure; silent-failure delta N-6). A login answered `Overloaded` by the settings read is
    /// counted in [`AuthStats::settings_wait_expired`] or [`AuthStats::settings_load_expired`]
    /// instead (rust review round 3 RD-2).
    pub admission_overloaded: u64,
    /// Correct passwords logged in without the rehash they asked for, because the hashing pool
    /// could not run it (its wait expired: `warn`; the new parameters need more memory than a
    /// permit reserves: `error`; security review round 2 M1). The next login tries again.
    pub rehash_skipped: u64,
    /// Correct logins read again from phase A once because their credential changed while it
    /// was verified (a concurrent login rehashed it; database review round 2 N7).
    pub login_restarts: u64,
    /// Settings generations loaded with hashing parameters that need more memory than a permit
    /// of this node's hashing pool reserves (logged at `error` once per generation; security
    /// review round 2 delta SD-1): this node verifies dummies within its reservation, refuses
    /// stored hashes over it (counted failures) and skips rehashes until the settings or
    /// `admission.hash_memory_kib` match.
    pub settings_over_reservation: u64,
    /// Callers waiting for the settings reload in progress right now (a gauge, not monotonic).
    pub settings_waiting: u64,
    /// Waits for another caller's settings reload that expired after the hashing pool's queue
    /// wait (logged at `warn`; answered `Overloaded`; security review round 2 delta N-b).
    pub settings_wait_expired: u64,
    /// Settings reloads of this caller that did not finish within the hashing pool's queue wait
    /// (logged at `warn`; answered `Overloaded`; security review round 3 R3-c).
    pub settings_load_expired: u64,
    /// Deadlocks the short transactions absorbed by running again, by step (each logged at
    /// `warn`; rust review round 3 RD-5).
    pub deadlock_retries: DeadlockRetries,
    /// API key `last_used_at` writes that changed the row.
    pub last_use_writes: u64,
    /// Due last-use tickets dropped while another write of this ORM lineage held the one-permit
    /// gate; nothing was written, the key's next use is due again.
    pub last_use_skipped: u64,
    /// Last-use writes that changed no row: the key was no longer due (another node wrote it
    /// first), locked by another transaction (skipped without waiting) or already deleted.
    pub last_use_noops: u64,
    /// Last-use writes that failed (logged at `warn` with the key id only; the request that held
    /// the ticket was never affected).
    pub last_use_errors: u64,
}

#[derive(Debug, Default)]
struct Counters {
    last_use_writes: AtomicU64,
    last_use_skipped: AtomicU64,
    last_use_noops: AtomicU64,
    last_use_errors: AtomicU64,
    settings_loads: AtomicU64,
    refusal_delete_failures: AtomicU64,
    failure_write_errors: AtomicU64,
    session_end_failures: AtomicU64,
    verdict_failures: AtomicU64,
    cooldown_hook_failures: AtomicU64,
    admission_overloaded: AtomicU64,
    rehash_skipped: AtomicU64,
    login_restarts: AtomicU64,
    settings_over_reservation: AtomicU64,
    settings_waiting: AtomicU64,
    settings_expiries: SettingsExpiries,
    retries: RetryCounters,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The authentication service of one database.
pub struct Auth {
    orm: Orm,
    config: AuthConfig,
    pool: &'static HashPool,
    prefilter: Prefilter,
    check_sql: String,
    api_key_sql: String,
    legacy_memo: super::apikey_legacy::LegacyMemo,
    #[cfg(feature = "test-support")]
    api_key_verdict_gate: Mutex<Option<super::apikey_verify::ApiKeyVerdictGate>>,
    /// The statements that depend on the token plan.
    sql: Arc<PlanSql>,
    /// The session token field plan of the registry (H58 TOKEN); resolved when this `Auth` was
    /// built, never per request.
    token_plan: Arc<TokenPlan>,
    /// The user row lock of the identity operations, with the plan's added fields.
    identity_lock_sql: String,
    has_languages: bool,
    checks_overridden: bool,
    /// An addon overrides `_on_login_cooldown`.
    cooldown_overridden: bool,
    /// An addon overrides `_get_login_domain` or `_get_login_order` (H58).
    selection_overridden: bool,
    selection_limits: LoginSelectionLimits,
    feature: Option<Arc<MemberRollout>>,
    /// Longest one `_on_login_cooldown` call may take (`AuthConfig::cooldown_hook_deadline`).
    cooldown_deadline: Duration,
    admission: Arc<Semaphore>,
    registration: pool::PoolBinding,
    cache: Mutex<Option<Arc<Cached>>>,
    /// One settings reload at a time (database review round 2 N8): a waiter finds the cache
    /// filled. A waiter waits at most the hashing pool's queue wait (security review round 2
    /// delta N-b).
    reload: tokio::sync::Mutex<()>,
    /// The settings generation + 1 whose hashing parameters over the reservation were last
    /// logged (0: none), so the `error` line comes once per generation.
    over_reservation_logged: AtomicU64,
    counters: Arc<Counters>,
}

/// The statements whose text depends on the token plan: phase A's login read, which also reads the
/// plan's added fields, and phase B's success statements, which write nothing when those fields
/// changed since. For the base plan they are the constants themselves.
struct PlanSql {
    login: std::borrow::Cow<'static, str>,
    success: std::borrow::Cow<'static, str>,
    success_rehash: std::borrow::Cow<'static, str>,
}

impl PlanSql {
    /// # Errors
    ///
    /// `Programming` when a statement lost or repeated its marker.
    fn new(plan: &TokenPlan, has_languages: bool) -> Result<Self> {
        let login = if has_languages {
            LOGIN_SQL_LANGUAGES
        } else {
            LOGIN_SQL_NO_LANGUAGES
        };
        if plan.is_base() {
            return Ok(Self {
                login: login.into(),
                success: SUCCESS_SQL.into(),
                success_rehash: SUCCESS_REHASH_SQL.into(),
            });
        }
        let guard = plan.guard("u", SUCCESS_TOKEN_PARAM);
        Ok(Self {
            login: replace_once(login, COLUMNS_MARKER, &plan.select_list("u"))?.into(),
            success: replace_once(SUCCESS_SQL, GUARD_MARKER, &guard)?.into(),
            success_rehash: replace_once(SUCCESS_REHASH_SQL, GUARD_MARKER, &guard)?.into(),
        })
    }
}

/// The position of the added token fields in the success statements' parameters.
const SUCCESS_TOKEN_PARAM: usize = 22;

/// The first column of phase A's login read that holds an added token field.
const LOGIN_TOKEN_COLUMN: usize = 13;

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("config", &self.config)
            .field("pool", &self.pool.config())
            .field("admission", &self.admission.available_permits())
            .finish_non_exhaustive()
    }
}

/// Phase A of a login (one row): the address and login throttle rows, whether the (address,
/// login) pair is trusted, the active user of the login and the installed languages (database
/// review M5: folded in, so no statement follows the hash but the verdict). `$1` address key hash
/// or NULL, `$2` login key hash, `$3` login, `$4` trusted-pair key hash or NULL, `$5` the trust
/// window, seconds.
const LOGIN_SQL_LANGUAGES: &str = "\
SELECT a.failures, extract(epoch FROM now() - a.last_failure_at)::float8,
       l.failures, extract(epoch FROM now() - l.last_failure_at)::float8,
       COALESCE(t.last_failure_at > now() - make_interval(secs => $5::float8), false),
       u.id::int8, u.password, u.lang, u.tz,
       (SELECT array_agg(g.code ORDER BY g.name, g.code) FROM res_lang g WHERE g.active),
       f.required_version, f.enabled, f.generation /*token_cols*/
  FROM (SELECT 1) x
  CROSS JOIN public.erp_node_feature_snapshot('auth.login_principal') f
  LEFT JOIN erp_login_throttle a ON a.kind = 'address' AND a.key_hash = $1::bytea
  LEFT JOIN erp_login_throttle l ON l.kind = 'login' AND l.key_hash = $2::bytea
  LEFT JOIN erp_login_throttle t ON t.kind = 'trusted' AND t.key_hash = $4::bytea
  LEFT JOIN res_users u ON u.login = $3::text AND u.active";

/// [`LOGIN_SQL_LANGUAGES`] while the registry has no `res.lang` (`en_US` alone is installed,
/// adds the model): the guard `installed_languages` has.
const LOGIN_SQL_NO_LANGUAGES: &str = "\
SELECT a.failures, extract(epoch FROM now() - a.last_failure_at)::float8,
       l.failures, extract(epoch FROM now() - l.last_failure_at)::float8,
       COALESCE(t.last_failure_at > now() - make_interval(secs => $5::float8), false),
       u.id::int8, u.password, u.lang, u.tz,
       NULL::text[], f.required_version, f.enabled, f.generation /*token_cols*/
  FROM (SELECT 1) x
  CROSS JOIN public.erp_node_feature_snapshot('auth.login_principal') f
  LEFT JOIN erp_login_throttle a ON a.kind = 'address' AND a.key_hash = $1::bytea
  LEFT JOIN erp_login_throttle l ON l.kind = 'login' AND l.key_hash = $2::bytea
  LEFT JOIN erp_login_throttle t ON t.kind = 'trusted' AND t.key_hash = $4::bytea
  LEFT JOIN res_users u ON u.login = $3::text AND u.active";

/// Throttle read when candidate selection runs separately (custom domain/order hooks).
/// `$1` address key hash or NULL, `$2` login key hash, `$3` trusted-pair key hash or NULL,
/// `$4` trust window in seconds.
const THROTTLE_SQL_LANGUAGES: &str = "\
SELECT a.failures, extract(epoch FROM now() - a.last_failure_at)::float8,
       l.failures, extract(epoch FROM now() - l.last_failure_at)::float8,
       COALESCE(t.last_failure_at > now() - make_interval(secs => $4::float8), false),
       (SELECT array_agg(g.code ORDER BY g.name, g.code) FROM res_lang g WHERE g.active),
       p.failures, extract(epoch FROM now() - p.last_failure_at)::float8,
       COALESCE(pt.last_failure_at > now() - make_interval(secs => $4::float8), false)
  FROM (SELECT 1) x
  LEFT JOIN erp_login_throttle a ON a.kind = 'address' AND a.key_hash = $1::bytea
  LEFT JOIN erp_login_throttle l ON l.kind = 'login' AND l.key_hash = $2::bytea
  LEFT JOIN erp_login_throttle t ON t.kind = 'trusted' AND t.key_hash = $3::bytea
  LEFT JOIN erp_login_throttle p ON p.kind = 'principal' AND p.key_hash = $5::bytea
  LEFT JOIN erp_login_throttle pt ON pt.kind = 'principal_trusted' AND pt.key_hash = $6::bytea";

const THROTTLE_SQL_NO_LANGUAGES: &str = "\
SELECT a.failures, extract(epoch FROM now() - a.last_failure_at)::float8,
       l.failures, extract(epoch FROM now() - l.last_failure_at)::float8,
       COALESCE(t.last_failure_at > now() - make_interval(secs => $4::float8), false),
       NULL::text[],
       p.failures, extract(epoch FROM now() - p.last_failure_at)::float8,
       COALESCE(pt.last_failure_at > now() - make_interval(secs => $4::float8), false)
  FROM (SELECT 1) x
  LEFT JOIN erp_login_throttle a ON a.kind = 'address' AND a.key_hash = $1::bytea
  LEFT JOIN erp_login_throttle l ON l.kind = 'login' AND l.key_hash = $2::bytea
  LEFT JOIN erp_login_throttle t ON t.kind = 'trusted' AND t.key_hash = $3::bytea
  LEFT JOIN erp_login_throttle p ON p.kind = 'principal' AND p.key_hash = $5::bytea
  LEFT JOIN erp_login_throttle pt ON pt.kind = 'principal_trusted' AND pt.key_hash = $6::bytea";

/// The part of phase B's success statement after the user row (`current_user_row`, the user as
/// phase A read it, or nothing): the pair trusted and the login's other pairs past
/// [`throttle::TRUSTED_PAIRS_PER_LOGIN`] dropped, oldest success first (security review round 2
/// S2; the pairs of a login share a key prefix, `$2` ..= `$15`), the login logged, the session
/// inserted; `RETURNING` the user's session count before the insert (the cap). No row: the
/// credential changed concurrently. No failure counter is cleared (round 2 N4: they decay).
/// `$1` the pair's key or NULL (no client address), `$22` (only when the token plan adds fields) the
/// added fields as phase A read them, `$16` the other pairs kept, `$17` principal_trusted
/// key or NULL, `$18`..=`$19` principal trusted key range. Concurrent successes of one login from new
/// addresses each see the others' pairs as uncommitted, so the pairs may overshoot 16 by the logins
/// in flight until the next success (delta N-h; accepted).
macro_rules! success_tail {
    () => {
        "
), trusted AS (
    INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at)
    SELECT 'trusted', $1::bytea, 1, now() FROM current_user_row WHERE $1::bytea IS NOT NULL
    ON CONFLICT (kind, key_hash) DO UPDATE SET last_failure_at = now()
), untrusted AS (
    DELETE FROM erp_login_throttle
     WHERE kind = 'trusted' AND EXISTS (SELECT 1 FROM current_user_row)
       AND key_hash IN (SELECT o.key_hash FROM erp_login_throttle o
                         WHERE o.kind = 'trusted' AND o.key_hash BETWEEN $2::bytea AND $15::bytea
                           AND o.key_hash <> $1::bytea
                         ORDER BY o.last_failure_at DESC, o.key_hash
                        OFFSET $16::int8)
), trusted_principal AS (
    INSERT INTO erp_login_throttle (kind, key_hash, failures, last_failure_at)
    SELECT 'principal_trusted', $17::bytea, 1, now() FROM current_user_row WHERE $17::bytea IS NOT NULL
    ON CONFLICT (kind, key_hash) DO UPDATE SET last_failure_at = now()
), untrusted_principal AS (
    DELETE FROM erp_login_throttle
     WHERE kind = 'principal_trusted' AND EXISTS (SELECT 1 FROM current_user_row)
       AND key_hash IN (SELECT o.key_hash FROM erp_login_throttle o
                         WHERE o.kind = 'principal_trusted' AND o.key_hash BETWEEN $18::bytea AND $19::bytea
                           AND o.key_hash <> $17::bytea
                         ORDER BY o.last_failure_at DESC, o.key_hash
                        OFFSET $16::int8)
), logged AS (
    INSERT INTO res_users_log (create_uid, create_date, write_uid, write_date)
    SELECT id, now() AT TIME ZONE 'UTC', id, now() AT TIME ZONE 'UTC' FROM current_user_row
)
INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at,
                         first_addr, first_agent, last_addr, last_agent, context)
SELECT $5::text::uuid, c.id, decode($6::text, 'hex'), decode($7::text, 'hex'), $8::int2,
       'active', now() + make_interval(secs => $9::float8), $10::text::inet, $11::text,
       $10::text::inet, $11::text,
       jsonb_build_object('lang', $12::text, 'tz', $13::text, 'uid', c.id)
  FROM current_user_row c
RETURNING (SELECT count(*) FROM erp_session WHERE user_id = $3::int8)::int8"
    };
}

/// Phase B of a successful login without a rehash (database review H3), one statement: the user
/// row as phase A read it (active, the stored hash `$14`) is locked `FOR SHARE`, so a password
/// change committed since phase A either waits for this statement or makes it see the new hash
/// and write nothing; the canonical login `$20` phase A saw must still be the row's (a rename since
/// then writes nothing); then `success_tail!`. `$4` (the new hash) is NULL and unused.
const SUCCESS_SQL: &str = concat!(
    "\
WITH feature_guard AS MATERIALIZED (
    SELECT CASE WHEN $21::int8 IS NOT NULL THEN (
        SELECT generation FROM public.erp_node_feature_protect(
            'auth.login_principal', 1, NULL::uuid, NULL::bytea, $21::int8, false)
    ) ELSE NULL::int8 END AS generation
), current_user_row AS (
    SELECT u.id FROM feature_guard f CROSS JOIN res_users u
     WHERE u.id = $3::int8 AND u.active AND u.password IS NOT DISTINCT FROM $14::text
       AND u.login = $20::text AND $4::text IS NULL
       AND ($21::int8 IS NULL OR f.generation = $21::int8) /*token_guard*/
       FOR SHARE OF u",
    success_tail!()
);

/// Phase B of a successful login that rehashes: the compare-and-set of the new hash `$4` over the
/// stored hash phase A read (`$14`) is the user row (`RETURNING id`; no row when the password or
/// the active flag changed since), then `success_tail!`. Not `FOR SHARE` plus an `UPDATE` in one
/// statement: a row the statement updated is skipped by its own lock.
const SUCCESS_REHASH_SQL: &str = concat!(
    "\
WITH feature_guard AS MATERIALIZED (
    SELECT CASE WHEN $21::int8 IS NOT NULL THEN (
        SELECT generation FROM public.erp_node_feature_protect(
            'auth.login_principal', 1, NULL::uuid, NULL::bytea, $21::int8, false)
    ) ELSE NULL::int8 END AS generation
), current_user_row AS (
    UPDATE res_users u SET password = $4::text FROM feature_guard f
     WHERE u.id = $3::int8 AND u.active AND u.password IS NOT DISTINCT FROM $14::text
       AND u.login = $20::text AND ($21::int8 IS NULL OR f.generation = $21::int8)
       /*token_guard*/
    RETURNING u.id",
    success_tail!()
);

/// The sessions of user `$1` other than `$2`, newest first, past the first `$3` (the cap, and
/// every other session after a rehash).
const OTHER_SESSIONS_SQL: &str = "\
SELECT id::text FROM erp_session WHERE user_id = $1::int8 AND id <> $2::text::uuid
 ORDER BY id DESC OFFSET $3::int8";

/// The failures of one throttle key, as phase A read them.
#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    since_last: Duration,
}

impl Failures {
    fn in_force(self, rule: Cooldown) -> bool {
        rule.in_force(self.count, self.since_last)
    }
}

/// The installed languages, as phase A read them.
enum Languages {
    /// `res.lang` is in the registry: its active codes.
    Installed(Vec<String>),
    /// `res.lang` is in the registry and no language is active.
    NoneActive,
    /// No `res.lang` in the registry yet: `en_US` alone is installed.
    NoModel,
}

impl Languages {
    fn context_lang(&self, user_lang: Option<&str>) -> String {
        match self {
            Self::Installed(codes) => context_lang(user_lang, codes),
            Self::NoneActive => context_lang(user_lang, &[]),
            Self::NoModel => context_lang(user_lang, &[DEFAULT_LANG.to_owned()]),
        }
    }
}

/// The rows of phase A.
struct Attempt {
    /// Disabled state observed by unbound base phase A; never refreshed across a restart.
    base_generation: Option<i64>,
    address: Failures,
    login: Failures,
    trusted: bool,
    principal: Option<Failures>,
    principal_trusted: bool,
    user: Option<LoginUser>,
    languages: Languages,
}

/// The active user of the presented login, as phase A read it.
struct LoginUser {
    uid: UserId,
    /// The stored hash. Wiped when the attempt ends (LD3): every selection-owned copy of a
    /// credential-adjacent string is a `Zeroizing` value or the witness (which wipes itself).
    stored: Option<Zeroizing<String>>,
    lang: Option<String>,
    tz: Option<String>,
    canonical_login: Option<Zeroizing<String>>,
    witness: Option<SelectionWitness>,
    /// The current values of the token plan's added fields, as phase A read them (wiped on drop).
    token_extra: TokenValues,
}

/// A session about to be written: its cookie secret, id, token, key id and the context's language.
struct Prepared {
    secret: SessionSecret,
    id: SessionId,
    token: [u8; 32],
    kid: i16,
    lang: String,
}

/// The keys of a client address: its own and the (address, login) pair's.
struct AddressKeys {
    key: AddressKey,
    address: [u8; 32],
    trusted: [u8; 32],
}

/// The throttle keys of an attempt (type design T6: the address and the pair come together), made
/// with the database's keyed hash (security review round 2 S4).
struct Keys {
    /// `None`: no client address (an in-process caller).
    address: Option<AddressKeys>,
    /// Why there is no address, for the logs.
    in_process: &'static str,
    login: [u8; 32],
    /// The keys a trusted pair of this login can have (round 2 S2).
    trusted_range: ([u8; 32], [u8; 32]),
}

impl Keys {
    fn new(hasher: &ThrottleHasher, client: ClientAddr, login: &str) -> Self {
        let (address, in_process) = match client {
            ClientAddr::Known(addr) => (Some(throttle::address_key(addr)), ""),
            ClientAddr::InProcess(reason) => (None, reason),
        };
        Self {
            address: address.map(|key| AddressKeys {
                address: hasher.key_hash(ThrottleKey::Address(&key)),
                trusted: hasher.key_hash(ThrottleKey::Trusted(&key, login)),
                key,
            }),
            in_process,
            login: hasher.key_hash(ThrottleKey::Login(login)),
            trusted_range: hasher.trusted_range(login),
        }
    }

    fn address(&self) -> Option<&[u8]> {
        self.address.as_ref().map(|a| a.address.as_slice())
    }

    fn trusted(&self) -> Option<&[u8]> {
        self.address.as_ref().map(|a| a.trusted.as_slice())
    }

    /// The address key's text for logs, or the in-process reason.
    fn address_text(&self) -> &str {
        self.address
            .as_ref()
            .map_or(self.in_process, |a| a.key.as_str())
    }

    /// The login's tag for logs.
    fn tag(&self) -> String {
        login_tag(&self.login)
    }
}

/// Phase A's transaction: committed already (one flight), or open for an overridden
/// `_check_credentials` (type design T6).
enum PhaseA {
    /// The one flight: nothing is held.
    Flight,
    /// An open REPEATABLE READ transaction, ended by the verdict (boxed: a transaction is large
    /// and the flight variant is the common one).
    Open(Box<Transaction>),
}

fn col<'r, T: tokio_postgres::types::FromSql<'r>>(row: &'r Row, idx: usize) -> Result<T> {
    row.try_get(idx).map_err(|e| {
        Error::db(format!(
            "could not decode column {idx} of the login row: {e}"
        ))
        .with_source(e)
    })
}

fn failures(row: &Row, idx: usize) -> Result<Failures> {
    let count: Option<i32> = col(row, idx)?;
    let since: Option<f64> = col(row, idx + 1)?;
    // no row: no failure (the LEFT JOIN found none)
    let count = u32::try_from(count.unwrap_or(0))
        .map_err(|_| Error::db("erp_login_throttle.failures is negative"))?;
    let since_last = match since {
        Some(s) if s.is_finite() && s > 0.0 => Duration::from_secs_f64(s),
        // no row, or a failure stamped in the future by another clock: just now
        _ => Duration::ZERO,
    };
    Ok(Failures { count, since_last })
}

impl Attempt {
    fn read(
        rows: &[Row],
        has_languages: bool,
        expected_generation: Option<i64>,
        plan: &TokenPlan,
    ) -> Result<Self> {
        let row = match rows {
            [row] => row,
            other => {
                return Err(Error::programming(format!(
                    "the login statement returned {} rows; res_users.login must be unique",
                    other.len()
                )));
            }
        };
        let required: i32 = col(row, 10)?;
        let enabled: bool = col(row, 11)?;
        let generation: i64 = col(row, 12)?;
        if required != 1 || generation < 0 {
            return Err(Error::programming(
                "login selection rollout state is incompatible",
            ));
        }
        if expected_generation.is_some_and(|expected| expected != generation) {
            return Err(Error::programming(
                "login selection rollout generation changed",
            ));
        }
        if enabled && expected_generation.is_some() {
            return Err(Error::programming(
                "login selection rollout generation changed",
            ));
        }
        if enabled {
            return Err(Error::programming(
                "base login requires membership-bound principal accounting while login selection rollout is active",
            ));
        }
        let id: Option<i64> = col(row, 5)?;
        let user = match id {
            Some(id) => Some(LoginUser {
                uid: UserId::new(id)?,
                stored: col::<Option<String>>(row, 6)?.map(Zeroizing::new),
                lang: col(row, 7)?,
                tz: col(row, 8)?,
                canonical_login: None,
                witness: None,
                token_extra: plan.read_values(row, LOGIN_TOKEN_COLUMN)?,
            }),
            None => None,
        };
        let codes: Option<Vec<String>> = col(row, 9)?;
        let languages = match (has_languages, codes) {
            (false, _) => Languages::NoModel,
            (true, Some(codes)) => Languages::Installed(codes),
            (true, None) => Languages::NoneActive,
        };
        Ok(Self {
            base_generation: Some(generation),
            address: failures(row, 0)?,
            login: failures(row, 2)?,
            trusted: col(row, 4)?,
            principal: None,
            principal_trusted: false,
            user,
            languages,
        })
    }

    fn read_throttles(
        rows: &[Row],
        candidate: Option<CandidateRecord>,
        has_languages: bool,
    ) -> Result<Self> {
        let row = match rows {
            [row] => row,
            other => {
                return Err(Error::programming(format!(
                    "the throttle statement returned {} rows",
                    other.len()
                )));
            }
        };
        let user = candidate.map(
            |(uid, stored, lang, tz, canonical_login, witness)| LoginUser {
                uid,
                stored,
                lang,
                tz,
                canonical_login: Some(canonical_login),
                witness: Some(witness),
                // custom selection with added token fields is refused at construction
                token_extra: TokenValues::empty(),
            },
        );
        let codes: Option<Vec<String>> = col(row, 5)?;
        let languages = match (has_languages, codes) {
            (false, _) => Languages::NoModel,
            (true, Some(codes)) => Languages::Installed(codes),
            (true, None) => Languages::NoneActive,
        };
        Ok(Self {
            base_generation: None,
            address: failures(row, 0)?,
            login: failures(row, 2)?,
            trusted: col(row, 4)?,
            principal: if user.is_some() {
                Some(failures(row, 6)?)
            } else {
                None
            },
            principal_trusted: col(row, 8)?,
            user,
            languages,
        })
    }
}

/// A short tag of a login for logs: the first 8 hexadecimal digits of its throttle key (keyed by
/// the database's secret, round 2 S4), never the text (users type passwords into the login
/// field; security review Low 2). The same tag names the login's throttle row.
fn login_tag(login_key: &[u8; 32]) -> String {
    session::hex(&login_key[..4])
}

/// Whether `e` is the hashing pool refusing this attempt's hash (its wait expired, or the stored
/// hash needs more memory than a permit reserves): the login answers `Denied`, counting only the
/// over-reservation (only before a real verify ran: after it, the verdict is decided,
/// silent-failure review M1).
fn pool_refusal(e: &Error) -> Option<&HashPoolError> {
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        if let Some(pool) = s.downcast_ref::<HashPoolError>()
            && matches!(
                pool,
                HashPoolError::Overloaded { .. } | HashPoolError::MemoryOverReservation { .. }
            )
        {
            return Some(pool);
        }
        source = s.source();
    }
    None
}

/// Log a pool refusal of the verify of the login tagged `tag`: an expired wait is load (`warn`,
/// not counted); a hash needing more memory than a permit reserves is a configuration fault that
/// refuses that user on every attempt, each counted (`error`, naming what lifts it; silent-failure
/// review S3, security review round 2 delta SD-1).
fn log_pool_refusal(refusal: &HashPoolError, tag: &str) {
    match refusal {
        HashPoolError::MemoryOverReservation {
            needed_kib,
            reserved_kib,
        } => tracing::error!(
            target: "erp::auth",
            "Login for login key {tag} answered Denied and counted: the verify needs \
             {needed_kib} KiB, more than the {reserved_kib} KiB a hashing permit of this node \
             reserves; every attempt of this user is refused until admission.hash_memory_kib is \
             the same on every node or this user's password is reset"
        ),
        other => tracing::warn!(
            target: "erp::auth",
            "Login for login key {tag} answered Denied, not counted: the verify could not run: \
             {other}"
        ),
    }
}

/// Log a rehash the pool could not run for the login tagged `tag` (the login goes on without it,
/// security review round 2 M1): an expired wait is load (`warn`); new parameters needing more
/// memory than a permit reserves is a configuration fault (`error`, naming both settings).
fn log_rehash_skipped(refusal: &HashPoolError, tag: &str) {
    match refusal {
        HashPoolError::MemoryOverReservation {
            needed_kib,
            reserved_kib,
        } => tracing::error!(
            target: "erp::auth",
            "Login for login key {tag} logged in without its rehash: the new hash needs \
             {needed_kib} KiB, more than the {reserved_kib} KiB a hashing permit reserves; no \
             stored hash is upgraded until admission.hash_memory_kib is raised or \
             password.hashing.argon2_memory_kib lowered"
        ),
        other => tracing::warn!(
            target: "erp::auth",
            "Login for login key {tag} logged in without its rehash (the next login tries \
             again): {other}"
        ),
    }
}

/// Roll `tx` back and return `error` (a rollback failure is added to it).
async fn abandon(tx: &mut Transaction, error: Error) -> Error {
    match tx.rollback().await {
        Ok(()) => error,
        Err(r) => error.and_also(&r),
    }
}

/// A REPEATABLE READ transaction on the primary acting as the system, for an overridden
/// `_check_credentials` (it runs ORM record paths).
fn repeatable_read() -> BeginOptions {
    BeginOptions::new(SUPERUSER_ID).on_primary()
}

/// A READ COMMITTED transaction on the primary acting as the system, for [`Orm::one_flight`]: a raw
/// statement alone, no record path.
fn read_committed() -> BeginOptions {
    BeginOptions::new(SUPERUSER_ID)
        .on_primary()
        .isolation(Isolation::ReadCommitted)
}

/// [`Orm::one_flight`] of `sql` with `params`; its rows.
async fn one_flight(
    orm: &Orm,
    options: BeginOptions,
    sql: &str,
    params: &[(&(dyn ToSql + Sync), Type)],
) -> Result<Vec<Row>> {
    orm.one_flight(options, FinalStatement::new(sql, params))
        .await
        .map_err(CommitError::into_error)
}

/// A login admission permit from `admission`, waiting at most `wait` (rust review H1: logins in
/// progress stay below the primary pool's size). An expired wait is counted in `counters`.
async fn admit(
    admission: &Arc<Semaphore>,
    wait: Duration,
    counters: &Counters,
) -> Result<OwnedSemaphorePermit, LoginError> {
    match tokio::time::timeout(wait, Arc::clone(admission).acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(LoginError::Failed(Error::programming(
            "the login admission is closed",
        ))),
        Err(_) => {
            bump(&counters.admission_overloaded);
            Err(LoginError::Overloaded { retry_after: wait })
        }
    }
}

/// The login admission's size: `2 × permits` logins in progress, below the primary pool so a
/// session check always finds a connection. Half the primary pool is reserved even before a
/// credential override is installed: rebuilt registries share the same lineage admission.
fn admission_size(permits: u32, connections: usize, _overridden: bool) -> usize {
    (2 * permits as usize).min(connections / 2).max(1)
}

/// The limits of the settings reload's flight: the lock and statement limits within the queue
/// wait `wait`, so an abandoned flight also ends on the server (security review round 3 R3-c).
fn reload_limits(wait: Duration) -> Result<Timeouts> {
    let statement = wait.max(Duration::from_millis(2));
    Timeouts::new(
        statement - Duration::from_millis(1),
        statement,
        statement + Duration::from_secs(1),
    )
}

/// What a settings read was doing when its queue wait ran out.
#[derive(Debug)]
enum SettingsExpiry {
    /// Waiting for another caller's reload.
    Waiting,
    /// Running its own reload, ended by `ReloadCause`.
    Reload(ReloadCause),
}

/// What ended a settings reload at its queue wait (round 4 C4-6: kept in the error's source and
/// in the `warn` line).
#[derive(Debug)]
enum ReloadCause {
    /// One of the flight's limits fired on the server: `what` names it.
    Limit { what: &'static str, error: Error },
    /// The caller's deadline passed first; the flight was dropped.
    Deadline(tokio::time::error::Elapsed),
}

impl ReloadCause {
    /// The cause when `e` is one of the flight's limits (lock, statement, backstop); any other
    /// error is given back, to be returned as it is.
    fn of_limit(e: Error) -> std::result::Result<Self, Error> {
        let what = match e.condition() {
            Some(Condition::LockTimeout) => "its lock_timeout fired",
            Some(Condition::StatementTimeout) => "its statement_timeout fired",
            Some(Condition::TransactionTimeout) => "its transaction_timeout backstop fired",
            // not a limit (`Condition` is non-exhaustive): the error goes back to the caller
            _ => return Err(e),
        };
        Ok(Self::Limit { what, error: e })
    }
}

/// The two settings expiries: their counters ([`AuthStats::settings_wait_expired`],
/// [`AuthStats::settings_load_expired`]) and a rate limit of the `warn` line for each, so a
/// line of one kind never hides the other's (round 4 C4-5).
#[derive(Debug, Default)]
struct SettingsExpiries {
    wait_expired: AtomicU64,
    load_expired: AtomicU64,
    wait_warn: WarnLimit,
    load_warn: WarnLimit,
}

impl SettingsExpiries {
    /// The error of a settings read past its queue wait `wait`, counted by its kind and logged
    /// at `warn` at most once a second per kind. A reload's cause is the error's source and is
    /// named in the line (round 4 C4-6).
    fn expired(&self, wait: Duration, expiry: SettingsExpiry) -> Error {
        let secs = wait.as_secs();
        let e = Error::new(
            ErrorKind::ResourceExhausted,
            format!(
                "Server busy; the authentication settings could not be read within the {secs} s \
                 queue wait (admission.hash_queue_wait_secs); retry."
            ),
        )
        .with_condition(Condition::Overloaded);
        let (expired, limit, what, e) = match expiry {
            SettingsExpiry::Waiting => (
                &self.wait_expired,
                &self.wait_warn,
                "waiting for another's reload".to_owned(),
                e,
            ),
            SettingsExpiry::Reload(ReloadCause::Limit { what, error }) => (
                &self.load_expired,
                &self.load_warn,
                format!("running the reload: {what} ({error})"),
                e.with_source(error),
            ),
            SettingsExpiry::Reload(ReloadCause::Deadline(elapsed)) => (
                &self.load_expired,
                &self.load_warn,
                "running the reload: the queue-wait deadline passed first and the flight was \
                 dropped"
                    .to_owned(),
                e.with_source(elapsed),
            ),
        };
        let total = expired.fetch_add(1, Ordering::Relaxed) + 1;
        if limit.admit() {
            tracing::warn!(
                target: "erp::auth",
                "a caller gave up on the authentication settings after the {secs} s queue wait \
                 (admission.hash_queue_wait_secs), {what}; {total} so far of this kind (this line \
                 comes at most once a second per kind)"
            );
        }
        e
    }
}

/// The cooldown refusal of `attempt`, if any (logged). The per-login cooldown does not refuse a
/// trusted (address, login) pair (lead decision 3): its password is verified, and a wrong one
/// counts. Applied after phase A, and again after the restart's phase A (security review round 2
/// delta N-a).
fn cooldown(
    s: &AuthSettings,
    policy: &LoginCooldownPolicy,
    attempt: &Attempt,
    keys: &Keys,
) -> Option<LoginError> {
    // the core rule OR the hook's additional rule: a policy only tightens
    let by_address = attempt.address.in_force(s.address_cooldown())
        || (keys.address.is_some()
            && policy
                .address()
                .is_some_and(|rule| attempt.address.in_force(rule)));
    let by_login = (attempt.login.in_force(s.login_cooldown())
        || policy
            .login()
            .is_some_and(|rule| attempt.login.in_force(rule)))
        && !attempt.trusted;
    let by_principal = attempt.principal.is_some_and(|failures| {
        failures.in_force(s.login_cooldown())
            || policy.login().is_some_and(|rule| failures.in_force(rule))
    }) && !attempt.principal_trusted;
    if !(by_address || by_login || by_principal) {
        return None;
    }
    tracing::warn!(
        target: "erp::auth",
        "Login attempt ignored for login key {} from {}: on cooldown after repeated failures \
         (base.login_cooldown_after, base.login_cooldown_duration)",
        keys.tag(),
        keys.address_text()
    );
    Some(LoginError::Cooldown)
}

/// Aborts its task when dropped (a caller that hangs up while the cooldown hook runs).
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Whether an addon overrides the hook, and how long one call may take.
#[derive(Clone, Copy)]
struct CooldownHook {
    overridden: bool,
    deadline: Duration,
}

/// Dispatch `res.users._on_login_cooldown` on an empty recordset in `tx`.
async fn dispatch_cooldown(
    tx: &mut Transaction,
    context: LoginCooldownContext,
) -> Result<LoginCooldownPolicy> {
    let users = tx.env().records("res.users")?;
    users
        .call::<(LoginCooldownContext,), LoginCooldownPolicy>(tx, "_on_login_cooldown", (context,))
        .await
}

/// The additional cooldown rules of an attempt: [`LoginCooldownPolicy::base`] without
/// opening a transaction while the base method is the only one; else the hook, in a read-only
/// transaction that is always rolled back, run in a task of its own so that a panic is caught,
/// a caller that drops the future aborts it, and `deadline` bounds it (an async wait; a hook that
/// blocks its thread without yielding cannot be interrupted). Any failure refuses the attempt as
/// [`LoginError::Failed`]: nothing is verified or counted; the cause is logged and the caller gets
/// [`COOLDOWN_HOOK_FAILED`] only.
async fn cooldown_policy(
    orm: &Orm,
    hook: CooldownHook,
    counters: &Counters,
    settings: &AuthSettings,
    client: ClientAddr,
    login: &str,
    keys: &Keys,
) -> Result<LoginCooldownPolicy, LoginError> {
    let CooldownHook {
        overridden,
        deadline,
    } = hook;
    if !overridden {
        return Ok(LoginCooldownPolicy::base());
    }
    let context = LoginCooldownContext::new(
        login,
        client,
        settings.address_cooldown(),
        settings.login_cooldown(),
    );
    let orm = orm.clone();
    let mut task = AbortOnDrop(tokio::spawn(async move {
        let options = BeginOptions::new(SUPERUSER_ID)
            .on_primary()
            .read_only(COOLDOWN_ENTRY);
        let mut tx = orm.begin_opts(options).await?;
        match dispatch_cooldown(&mut tx, context).await {
            Ok(policy) => {
                tx.rollback().await?;
                Ok(policy)
            }
            Err(e) => Err(abandon(&mut tx, e).await),
        }
    }));
    let cause = match tokio::time::timeout(deadline, &mut task.0).await {
        Ok(Ok(Ok(policy))) => return Ok(policy),
        Ok(Ok(Err(e))) => e,
        // the guard aborts the task when it drops at the return below
        Err(_) => Error::programming(format!(
            "_on_login_cooldown exceeded its {} ms deadline",
            deadline.as_millis()
        )),
        Ok(Err(join)) if join.is_panic() => Error::programming(format!(
            "_on_login_cooldown panicked: {}",
            panic_text(join.into_panic().as_ref())
        )),
        Ok(Err(join)) => Error::programming(format!("_on_login_cooldown task ended: {join}")),
    };
    bump(&counters.cooldown_hook_failures);
    tracing::error!(
        target: "erp::auth",
        "_on_login_cooldown failed for login key {}: {cause}",
        keys.tag()
    );
    Err(LoginError::Failed(Error::programming(COOLDOWN_HOOK_FAILED)))
}

/// The entry label of the read-only transaction that resolves the token plan.
const TOKEN_PLAN_ENTRY: &str = "res.users._get_session_token_fields";

/// Dispatch `res.users._get_session_token_fields` on an empty recordset in `tx`, where only that
/// chain may run: any query or write the hook attempts is refused.
async fn dispatch_token_fields(tx: &mut Transaction) -> Result<SessionTokenFields> {
    let mut guard = tx.pure_methods("res.users", &[super::token_plan::METHOD]);
    let users = guard.env().records("res.users")?;
    let fields: SessionTokenFields = users
        .call::<(), SessionTokenFields>(&mut guard, super::token_plan::METHOD, ())
        .await?;
    guard.finish()?;
    Ok(fields)
}

/// The session token plan of `orm`'s registry (H58 TOKEN): the base plan, without opening a
/// transaction, while the base method is the only one; else the hook, once, in a read-only
/// transaction that is always rolled back, in a task of its own so that a panic is caught and a
/// hung hook is bounded by `AuthConfig::token_plan_hook_deadline`, and the fields it names checked against
/// `res.users`. Any failure refuses the service: no field is skipped and no base plan stands in.
async fn resolve_token_plan(orm: &Orm, deadline: Duration) -> Result<TokenPlan> {
    let method = super::token_plan::METHOD;
    let overridden = orm
        .registry()
        .model_id("res.users")
        .is_some_and(|id| !orm.dispatch().is_base_method(id, method));
    if !overridden {
        return Ok(TokenPlan::base());
    }
    let task_orm = orm.clone();
    let mut task = AbortOnDrop(tokio::spawn(async move {
        let options = BeginOptions::new(SUPERUSER_ID)
            .on_primary()
            .read_only(TOKEN_PLAN_ENTRY);
        let mut tx = task_orm.begin_opts(options).await?;
        match dispatch_token_fields(&mut tx).await {
            Ok(fields) => {
                tx.rollback().await?;
                Ok(fields)
            }
            Err(e) => Err(abandon(&mut tx, e).await),
        }
    }));
    let failed = |cause: &dyn fmt::Display| Error::programming(format!("{method} failed: {cause}"));
    let fields = match tokio::time::timeout(deadline, &mut task.0).await {
        Ok(Ok(Ok(fields))) => fields,
        Ok(Ok(Err(e))) => return Err(failed(&e.message())),
        Err(_) => {
            return Err(Error::programming(format!(
                "{method} exceeded its {} ms deadline",
                deadline.as_millis()
            )));
        }
        Ok(Err(join)) if join.is_panic() => {
            return Err(Error::programming(format!(
                "{method} panicked: {}",
                panic_text(join.into_panic().as_ref())
            )));
        }
        Ok(Err(join)) => return Err(Error::programming(format!("{method} task ended: {join}"))),
    };
    TokenPlan::compile(&fields, orm.registry())
}

/// Counts one waiter while alive (also when the waiting future is dropped).
struct Gauge<'a>(&'a AtomicU64);

impl<'a> Gauge<'a> {
    fn new(gauge: &'a AtomicU64) -> Self {
        gauge.fetch_add(1, Ordering::Relaxed);
        Self(gauge)
    }
}

impl Drop for Gauge<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Phase A: one READ COMMITTED READ ONLY flight, committed before the hash; or, when an addon
/// overrides `_check_credentials`, a REPEATABLE READ transaction left open for it (the dispatch
/// runs the ORM's record paths). Shared by the first read and the one restart (database review
/// round 2 N7).
struct PhaseARead<'a> {
    orm: &'a Orm,
    sql: &'a str,
    plan: &'a TokenPlan,
    has_languages: bool,
    overridden: bool,
    principal_accounting: bool,
    expected_generation: Option<i64>,
    limits: LoginSelectionLimits,
}

async fn dispatch_selection(
    tx: &mut Transaction,
    login: &str,
    limits: &LoginSelectionLimits,
    registry: &erp_registry::Registry,
) -> Result<LoginSelectionPlan> {
    let mut guard = tx.pure_methods("res.users", &["_get_login_domain", "_get_login_order"]);
    let users = guard.env().records("res.users")?;
    let lookup = LoginLookup::new(login, limits.encoded_bytes())?;
    let domain: LoginDomain = users
        .call::<(LoginLookup,), LoginDomain>(&mut guard, "_get_login_domain", (lookup,))
        .await?;
    let order: LoginOrder = users
        .call::<(), LoginOrder>(&mut guard, "_get_login_order", ())
        .await?;
    guard.finish()?;

    let validated_domain = domain.admit(limits, registry)?;
    let validated_order = order.admit(limits, registry)?;
    LoginSelectionPlan::admit(validated_domain, validated_order, limits, registry)
}

impl PhaseARead<'_> {
    async fn run(
        &self,
        keys: &Keys,
        login: &str,
        hasher: &ThrottleHasher,
    ) -> Result<(Attempt, PhaseA, Option<LoginSelectionPlan>), LoginError> {
        let window = throttle::TRUSTED_FOR.as_secs_f64();
        let (address, trusted) = (keys.address(), keys.trusted());

        if self.principal_accounting {
            if self.overridden {
                return Err(LoginError::Failed(Error::programming(
                    "active login principal accounting combined with credential override is unsupported until a later release",
                )));
            }
            let mut tx = self
                .orm
                .begin_opts(repeatable_read().read_only(PHASE_A_ENTRY))
                .await
                .map_err(LoginError::Failed)?;

            let plan =
                match dispatch_selection(&mut tx, login, &self.limits, self.orm.registry()).await {
                    Ok(p) => p,
                    Err(e) => return Err(LoginError::Failed(abandon(&mut tx, e).await)),
                };

            let candidate = match select_candidate(&mut tx, &plan, &self.limits).await {
                Ok(c) => c,
                Err(e) => return Err(LoginError::Failed(abandon(&mut tx, e).await)),
            };

            let throttle_sql = if self.has_languages {
                THROTTLE_SQL_LANGUAGES
            } else {
                THROTTLE_SQL_NO_LANGUAGES
            };
            let principal = candidate
                .as_ref()
                .map(|c| hasher.key_hash(ThrottleKey::Principal(c.0)));
            let principal_trusted = candidate.as_ref().and_then(|c| {
                keys.address
                    .as_ref()
                    .map(|a| hasher.key_hash(ThrottleKey::PrincipalTrusted(&a.key, c.0)))
            });
            let throttle_params: [(&(dyn ToSql + Sync), Type); 6] = [
                (&address, Type::BYTEA),
                (&keys.login.as_slice(), Type::BYTEA),
                (&trusted, Type::BYTEA),
                (&window, Type::FLOAT8),
                (&principal.as_ref().map(|p| p.as_slice()), Type::BYTEA),
                (
                    &principal_trusted.as_ref().map(|p| p.as_slice()),
                    Type::BYTEA,
                ),
            ];
            let rows = match tx.query_typed(throttle_sql, &throttle_params).await {
                Ok(r) => r,
                Err(e) => return Err(LoginError::Failed(abandon(&mut tx, e).await)),
            };

            let attempt = match Attempt::read_throttles(&rows, candidate, self.has_languages) {
                Ok(a) => a,
                Err(e) => return Err(LoginError::Failed(abandon(&mut tx, e).await)),
            };

            tx.rollback().await.map_err(LoginError::Failed)?;
            return Ok((attempt, PhaseA::Flight, Some(plan)));
        }

        let params: [(&(dyn ToSql + Sync), Type); 5] = [
            (&address, Type::BYTEA),
            (&keys.login.as_slice(), Type::BYTEA),
            (&login, Type::TEXT),
            (&trusted, Type::BYTEA),
            (&window, Type::FLOAT8),
        ];
        if !self.overridden {
            let options = read_committed().read_only(PHASE_A_ENTRY);
            let rows = one_flight(self.orm, options, self.sql, &params)
                .await
                .map_err(|error| LoginError::Failed(rollout_error(error)))?;
            let attempt = Attempt::read(
                &rows,
                self.has_languages,
                self.expected_generation,
                self.plan,
            )
            .map_err(LoginError::Failed)?;
            return Ok((attempt, PhaseA::Flight, None));
        }
        // the override runs through the ORM's record paths, which need REPEATABLE READ
        let mut tx = self
            .orm
            .begin_opts(repeatable_read())
            .await
            .map_err(LoginError::Failed)?;
        let read = match tx.query_typed(self.sql, &params).await {
            Ok(rows) => Attempt::read(
                &rows,
                self.has_languages,
                self.expected_generation,
                self.plan,
            ),
            Err(e) => Err(rollout_error(e)),
        };
        match read {
            Ok(attempt) => Ok((attempt, PhaseA::Open(Box::new(tx)), None)),
            Err(e) => Err(LoginError::Failed(abandon(&mut tx, e).await)),
        }
    }
}

/// How a constructor supplies rollout authority; the regression seam never enters native serving.
enum SelectionAdmission {
    Serving,
    Membership(NodeAdmission),
    #[cfg(feature = "test-support")]
    RegressionFixture,
}

struct MemberRollout {
    binding: FeatureBinding,
    admission: NodeAdmission,
}

impl MemberRollout {
    async fn snapshot(&self, orm: &Orm) -> Result<FeatureSnapshot, LoginError> {
        if !self.admission.is_serving() {
            return Err(LoginError::Failed(Error::programming(
                "login selection membership is not serving",
            )));
        }
        let mut tx = orm
            .begin_opts(read_committed())
            .await
            .map_err(LoginError::Failed)?;
        let snapshot = self
            .binding
            .snapshot_in(&mut tx)
            .await
            .map_err(|error| LoginError::Failed(rollout_error(error)))?;
        tx.rollback().await.map_err(LoginError::Failed)?;
        if !snapshot.is_serving() {
            return Err(LoginError::Failed(Error::programming(
                "login selection membership is not serving",
            )));
        }
        Ok(snapshot)
    }
}

struct AttemptFeature {
    member: Arc<MemberRollout>,
    snapshot: FeatureSnapshot,
}

/// Convert only the known feature policy code/local refusal; retain every original cause.
fn rollout_error(error: Error) -> Error {
    if error.message() == "feature membership is not serving" {
        Error::programming("login selection membership is not serving")
            .with_sqlstate(error.sqlstate().map(str::to_owned))
            .with_source(error)
    } else if error.sqlstate() == Some("ERF01") {
        Error::programming(error.message().to_owned())
            .with_sqlstate(Some("ERF01".to_owned()))
            .with_source(error)
    } else {
        error
    }
}

impl Auth {
    /// The service of `orm`'s database, hashing on `pool` (the process pool in a server:
    /// [`HashPool::global`]). `pool` becomes the hashing pool of every transaction of this ORM
    /// lineage ([`pool::for_env`]: setting a password hashes there too) while any service,
    /// detached verdict or queued/running helper hash retains its lease.
    /// All services of this lineage share an admission of
    /// `min(2 × permits, primary pool size / 2)`, including before an addon installs an
    /// override, so logins leave connection capacity for session checks.
    ///
    /// Startup checks, in order, before anything is registered: the `base` models, the core
    /// tables the ORM was built to require ([`schema::check_orm`]), the primary pool size,
    /// then the stored settings and `database.secret` read once ([`settings::check_stored`]) and
    /// the hashing parameters against `pool`'s reservation ([`settings::check_reservation`]), so
    /// a misconfiguration is reported here, naming the key, instead of refusing every login. What
    /// that read loaded is kept as the first cached generation, labelled with the generation read
    /// before it, so the first login does not read it again (counted in
    /// [`AuthStats::settings_loads`]; rust review round 3 RD-7).
    ///
    /// # Errors
    ///
    /// `Programming` when the registry lacks `res.users`, `res.users.log` or
    /// `ir.config_parameter` (addon `base`), when the ORM was built without
    /// `OrmBuilder::require_core_tables(erp_security::auth::core_tables())`, when the primary pool
    /// has fewer than 2 connections, or when another `Auth` of this ORM lineage hashes on another
    /// pool; `Validation` naming the key for a stored setting that does not parse, or for hashing
    /// parameters that need more memory than a permit of `pool` reserves; a missing or
    /// malformed `database.secret`; the read transaction's errors. Custom domain/order overrides
    /// return `Programming` requiring an active membership-bound rollout; use [`Self::with_membership`]
    /// to supply real rollout admission.
    /// A combined selection/credential override instead names first.
    pub async fn new(orm: Orm, config: AuthConfig, pool: &'static HashPool) -> Result<Self> {
        Self::construct(orm, config, pool, SelectionAdmission::Serving).await
    }

    /// Compose authentication against an actual registered membership, including while starting.
    ///
    /// The binary must declare `auth.login_principal` version 1. Every login revalidates live
    /// membership and feature state; custom selection requires enabled serving admission. This
    /// constructor does not request readiness or activate the feature.
    ///
    /// ```no_run
    /// # async fn compose(orm: erp_orm::Orm, member: &erp_orm::node::NodeMembership) -> erp_fields::Result<()> {
    /// use erp_security::auth::{Auth, AuthConfig, HashPool};
    /// let auth = Auth::with_membership(orm, AuthConfig::DEFAULT,
    ///     HashPool::global(), member.admission()).await?;
    /// # let _ = auth;
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    /// The errors of [`Self::new`], except its custom-selection rollout refusal; invalid membership,
    /// helper/schema identity or binary capability refuses. SESSION composition retains precedence.
    /// Membership-bound credential overrides refuse verification pending SESSION support, including
    /// while disabled: their open transaction cannot precede the later feature protection lock.
    /// Construction and ordinary validation of existing sessions remain available.
    pub async fn with_membership(
        orm: Orm,
        config: AuthConfig,
        pool: &'static HashPool,
        admission: NodeAdmission,
    ) -> Result<Self> {
        Self::construct(orm, config, pool, SelectionAdmission::Membership(admission)).await
    }

    /// Explicit default-off fixture seam; every check except rollout admission remains active.
    #[cfg(feature = "test-support")]
    pub(super) async fn for_selection_tests(
        orm: Orm,
        config: AuthConfig,
        pool: &'static HashPool,
    ) -> Result<Self> {
        Self::construct(orm, config, pool, SelectionAdmission::RegressionFixture).await
    }

    async fn construct(
        orm: Orm,
        config: AuthConfig,
        pool: &'static HashPool,
        selection_admission: SelectionAdmission,
    ) -> Result<Self> {
        for model in ["res.users", "res.users.log", "ir.config_parameter"] {
            if orm.registry().model(model).is_none() {
                return Err(Error::programming(format!(
                    "authentication needs the model {model} (addon base) in the registry"
                )));
            }
        }
        schema::check_orm(&orm)?;
        let connections = orm.primary_pool_max_size();
        if connections < 2 {
            return Err(Error::programming(format!(
                "authentication needs a primary pool of at least 2 connections (it has \
                 {connections}): logins leave one for session checks"
            )));
        }
        // the label is read before the statement (the cache rule): a write committed meanwhile
        // leaves the first generation stale, reloaded at first use (rust review round 3 RD-7)
        let label = orm.ormcache_generation(CACHE_NAME);
        let loaded = settings::check_stored(&orm).await?;
        settings::check_reservation(loaded.settings.hash_params(), pool.config().memory_kib())?;
        let token_plan =
            Arc::new(resolve_token_plan(&orm, config.token_plan_hook_deadline()).await?);
        let first = Arc::new(Cached {
            generation: label,
            settings: loaded.settings,
            keys: loaded.keys,
            throttle: loaded.throttle,
            apikey: loaded.apikey,
            browser: loaded.browser,
            token_plan: Arc::clone(&token_plan),
        });
        let counters = Counters::default();
        bump(&counters.settings_loads);
        let checks_overridden = orm
            .registry()
            .model_id("res.users")
            .is_some_and(|id| !orm.dispatch().is_base_method(id, "_check_credentials"));
        let cooldown_overridden = orm
            .registry()
            .model_id("res.users")
            .is_some_and(|id| !orm.dispatch().is_base_method(id, "_on_login_cooldown"));
        let domain_overridden = orm
            .registry()
            .model_id("res.users")
            .is_some_and(|id| !orm.dispatch().is_base_method(id, "_get_login_domain"));
        let order_overridden = orm
            .registry()
            .model_id("res.users")
            .is_some_and(|id| !orm.dispatch().is_base_method(id, "_get_login_order"));
        let selection_overridden = domain_overridden || order_overridden;
        if selection_overridden && checks_overridden {
            return Err(Error::programming(
                "custom login selection combined with credential override is unsupported until a later release",
            ));
        }
        if selection_overridden && matches!(&selection_admission, SelectionAdmission::Serving) {
            return Err(Error::programming(
                "custom login selection requires an active membership-bound rollout",
            ));
        }
        if selection_overridden && !token_plan.is_base() {
            return Err(Error::programming(
                "custom login selection combined with a session token plan that adds fields is unsupported until a later release",
            ));
        }
        let feature = match selection_admission {
            SelectionAdmission::Serving => None,
            #[cfg(feature = "test-support")]
            SelectionAdmission::RegressionFixture => None,
            SelectionAdmission::Membership(admission) => {
                let binding = FeatureBinding::new(
                    admission.clone(),
                    &orm,
                    FeatureKey::new("auth.login_principal")?,
                    FeatureVersion::new(1)?,
                )
                .await?;
                let mut tx = orm.begin(SUPERUSER_ID).await?;
                schema::check_login_rollout(&mut tx).await?;
                tx.rollback().await?;
                Some(Arc::new(MemberRollout { binding, admission }))
            }
        };
        let permits = admission_size(pool.config().permits(), connections, checks_overridden);
        let has_languages = orm.registry().model("res.lang").is_some();
        let resolver = Resolver::for_registry(orm.registry())?;
        let registration = pool::register_for(&orm, pool)?;
        Ok(Self {
            check_sql: session::check_sql(&resolver, &token_plan),
            api_key_sql: super::apikey_verify::check_sql(&resolver, &token_plan),
            legacy_memo: super::apikey_legacy::LegacyMemo::default(),
            #[cfg(feature = "test-support")]
            api_key_verdict_gate: Mutex::new(None),
            sql: Arc::new(PlanSql::new(&token_plan, has_languages)?),
            identity_lock_sql: format!(
                "SELECT login, password, active{} FROM res_users WHERE id = $1 FOR UPDATE",
                token_plan.select_list("")
            ),
            token_plan,
            has_languages,
            checks_overridden,
            cooldown_overridden,
            selection_overridden,
            selection_limits: LoginSelectionLimits::default(),
            feature,
            cooldown_deadline: config.cooldown_hook_deadline(),
            admission: registration.admission(permits)?,
            registration,
            prefilter: Prefilter::new(
                config.throttle_entries(),
                config.login_burst(),
                config.login_rate_per_sec(),
            ),
            orm,
            config,
            pool,
            cache: Mutex::new(Some(first)),
            reload: tokio::sync::Mutex::new(()),
            over_reservation_logged: AtomicU64::new(0),
            counters: Arc::new(counters),
        })
    }

    /// Purpose-limited browser tokens, refreshed through the authentication settings generation.
    /// A signal-enabled ORM checks the primary generation even when settings are cached, so a
    /// login page can rebuild a superseded registry before accepting a login POST.
    /// # Errors
    /// Signal refresh, settings reload, key derivation or admission failures; no raw database
    /// secret is returned.
    pub async fn browser_tokens(&self) -> Result<Arc<super::browser::PrefsSigner>> {
        if self.orm.signal().is_some() {
            // Sealed auth-owned read: no business data or caller-supplied SQL. Use a normal
            // transaction: one_flight deliberately skips cross-process signals.
            let mut tx = self
                .orm
                .begin_opts(read_committed().read_only("res.users.browser_tokens"))
                .await?;
            if let Err(error) = tx.query("SELECT 1", &[]).await {
                return Err(abandon(&mut tx, error).await);
            }
            tx.rollback().await?;
        }
        Ok(Arc::clone(&self.settings_current().await?.browser))
    }

    /// The ORM.
    pub fn orm(&self) -> &Orm {
        &self.orm
    }

    /// The process settings.
    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

    /// The deadlock retry counters (the collection's batches count there too).
    pub(super) fn retry_counters(&self) -> &RetryCounters {
        &self.counters.retries
    }

    /// The pre-filter (its refusal counters).
    pub fn prefilter(&self) -> &Prefilter {
        &self.prefilter
    }

    /// The counters.
    pub fn stats(&self) -> AuthStats {
        let c = &self.counters;
        AuthStats {
            settings_loads: c.settings_loads.load(Ordering::Relaxed),
            refusal_delete_failures: c.refusal_delete_failures.load(Ordering::Relaxed),
            failure_write_errors: c.failure_write_errors.load(Ordering::Relaxed),
            session_end_failures: c.session_end_failures.load(Ordering::Relaxed),
            verdict_failures: c.verdict_failures.load(Ordering::Relaxed),
            cooldown_hook_failures: c.cooldown_hook_failures.load(Ordering::Relaxed),
            admission_overloaded: c.admission_overloaded.load(Ordering::Relaxed),
            rehash_skipped: c.rehash_skipped.load(Ordering::Relaxed),
            login_restarts: c.login_restarts.load(Ordering::Relaxed),
            settings_over_reservation: c.settings_over_reservation.load(Ordering::Relaxed),
            settings_waiting: c.settings_waiting.load(Ordering::Relaxed),
            settings_wait_expired: c.settings_expiries.wait_expired.load(Ordering::Relaxed),
            settings_load_expired: c.settings_expiries.load_expired.load(Ordering::Relaxed),
            deadlock_retries: c.retries.read(),
            last_use_writes: c.last_use_writes.load(Ordering::Relaxed),
            last_use_skipped: c.last_use_skipped.load(Ordering::Relaxed),
            last_use_noops: c.last_use_noops.load(Ordering::Relaxed),
            last_use_errors: c.last_use_errors.load(Ordering::Relaxed),
        }
    }

    /// The throttle row key of `key` in this database (keyed by its `database.secret`, security
    /// review round 2 S4): for operators and tests that look a row up.
    ///
    /// # Errors
    ///
    /// As [`Auth::settings`], the expired settings wait included: `ResourceExhausted` with
    /// [`Condition::Overloaded`] when the settings could not be read within the hashing pool's
    /// queue wait.
    pub async fn throttle_key(&self, key: ThrottleKey<'_>) -> Result<[u8; 32]> {
        Ok(self.settings_current().await?.throttle.key_hash(key))
    }

    fn cache_get(&self) -> Option<Arc<Cached>> {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn cache_put(&self, cached: &Arc<Cached>) {
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(cached));
    }

    fn cache_drop(&self) {
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn cache_fresh(&self) -> Option<Arc<Cached>> {
        let label = self.orm.ormcache_generation(CACHE_NAME);
        self.cache_get().filter(|c| c.generation == label)
    }

    /// The cached settings; when missing or stale, read again in one READ COMMITTED read-only
    /// flight on the primary, labelled with the generation read before it. One reload at a time
    /// per service: a caller that waited finds the cache filled (database review round 2 N8).
    ///
    /// Every call spends at most the hashing pool's queue wait here, from its first look at the
    /// cache: waiting for another caller's reload (security review round 2 delta N-b) and running
    /// its own (round 3 R3-c) share one deadline. The bound is per call: a session check calls it
    /// again after its keys went stale ([`Auth::begin_session`]), so it can wait up to two queue
    /// waits (round 4 C4-7); a login calls it once (its restart keeps the settings it read). The
    /// reload's flight also carries a lock and statement limit of the queue wait, so a flight
    /// abandoned at the deadline ends on the server too. Past the deadline the caller fails with
    /// `ResourceExhausted` and [`Condition::Overloaded`], counted
    /// ([`AuthStats::settings_wait_expired`] or [`AuthStats::settings_load_expired`]) and logged
    /// at `warn` at most once a second for each of the two (R3-d, round 4 C4-5), the reload's
    /// line and the error's source naming what ended it (C4-6): a stalled database must not
    /// queue every login and session check behind one statement without bound, nor flood the
    /// log with one line per request.
    async fn settings_current(&self) -> Result<Arc<Cached>> {
        if let Some(cached) = self.cache_fresh() {
            return Ok(cached);
        }
        let wait = self.pool.config().queue_wait();
        let deadline = tokio::time::Instant::now() + wait;
        let waiting = Gauge::new(&self.counters.settings_waiting);
        let locked = tokio::time::timeout_at(deadline, self.reload.lock()).await;
        drop(waiting);
        let Ok(_reloading) = locked else {
            let expiries = &self.counters.settings_expiries;
            return Err(expiries.expired(wait, SettingsExpiry::Waiting));
        };
        if let Some(cached) = self.cache_fresh() {
            return Ok(cached);
        }
        let label = self.orm.ormcache_generation(CACHE_NAME);
        let names = settings::keys::ALL.to_vec();
        let params: [(&(dyn ToSql + Sync), Type); 1] = [(&names, Type::TEXT_ARRAY)];
        let options = read_committed()
            .read_only(SETTINGS_ENTRY)
            .timeouts(reload_limits(wait)?);
        let flight = one_flight(&self.orm, options, settings::SETTINGS_SQL, &params);
        let expiries = &self.counters.settings_expiries;
        let rows = match tokio::time::timeout_at(deadline, flight).await {
            Ok(Ok(rows)) => rows,
            // the flight's own limit ends it at the queue wait; any other error is returned as is
            Ok(Err(e)) => {
                let cause = ReloadCause::of_limit(e)?;
                return Err(expiries.expired(wait, SettingsExpiry::Reload(cause)));
            }
            // the deadline: the flight is dropped, its connection discarded, which rolls the
            // read-only transaction back
            Err(elapsed) => {
                let cause = ReloadCause::Deadline(elapsed);
                return Err(expiries.expired(wait, SettingsExpiry::Reload(cause)));
            }
        };
        let loaded = settings::loaded_from(settings::rows_to_map(&rows)?)?;
        bump(&self.counters.settings_loads);
        self.note_over_reservation(label, loaded.settings.hash_params());
        let cached = Arc::new(Cached {
            generation: label,
            settings: loaded.settings,
            keys: loaded.keys,
            throttle: loaded.throttle,
            apikey: loaded.apikey,
            browser: loaded.browser,
            token_plan: Arc::clone(&self.token_plan),
        });
        self.cache_put(&cached);
        Ok(cached)
    }

    /// Log and count, once per settings generation, hashing parameters that need more memory
    /// than a permit of this node's pool reserves (security review round 2 delta SD-1 (a)): the
    /// write check ran on the writing node's pool, and `Auth::new` checks only at start, so a
    /// node with a smaller `admission.hash_memory_kib` (or a scheme switched over a larger stored
    /// memory) gets here. The node keeps working: dummies run within its reservation, stored
    /// hashes over it are refused and counted, rehashes are skipped.
    fn note_over_reservation(&self, generation: u64, params: &HashParams) {
        let Err(e) = settings::check_reservation(params, self.pool.config().memory_kib()) else {
            return;
        };
        let mark = generation.wrapping_add(1);
        if self.over_reservation_logged.swap(mark, Ordering::Relaxed) == mark {
            return;
        }
        bump(&self.counters.settings_over_reservation);
        tracing::error!(
            target: "erp::auth",
            "authentication settings over this node's hashing reservation: {e}. Until fixed, \
             unknown logins verify a dummy within the reservation, users whose stored hash needs \
             more are refused (counted as failures), and no rehash is written; every node must \
             have the same admission.hash_memory_kib"
        );
    }

    /// The database settings, current (read when the cache is missing or stale).
    ///
    /// # Errors
    ///
    /// `ResourceExhausted` with [`Condition::Overloaded`] when the settings could not be read
    /// within the hashing pool's queue wait (waiting for another caller's reload, or running this
    /// caller's own: the statement's lock and statement limits are that wait too; counted and
    /// logged); the statement's other errors; `Validation` naming the key for a malformed value;
    /// `Programming` when `database.secret` is missing.
    pub async fn settings(&self) -> Result<SettingsView> {
        Ok(SettingsView(self.settings_current().await?))
    }

    fn phase_a(
        &self,
        principal_accounting: bool,
        expected_generation: Option<i64>,
    ) -> PhaseARead<'_> {
        PhaseARead {
            orm: &self.orm,
            sql: &self.sql.login,
            plan: &self.token_plan,
            has_languages: self.has_languages,
            overridden: self.checks_overridden,
            principal_accounting,
            expected_generation,
            limits: self.selection_limits,
        }
    }

    /// Shared by every password-verification entry, including identity confirmation. A bound
    /// credential override cannot retain user locks before later feature protection, even while
    /// disabled; SESSION must integrate that transaction ownership before it can be supported.
    async fn feature_attempt(&self) -> Result<Option<AttemptFeature>, LoginError> {
        match &self.feature {
            Some(member) => {
                let snapshot = member.snapshot(&self.orm).await?;
                if snapshot.enabled() && self.checks_overridden {
                    return Err(LoginError::Failed(Error::programming(
                        "active login principal accounting combined with credential override is unsupported until a later release",
                    )));
                }
                if self.selection_overridden && !snapshot.enabled() {
                    return Err(LoginError::Failed(Error::programming(
                        "login selection rollout is disabled",
                    )));
                }
                if self.checks_overridden {
                    return Err(LoginError::Failed(Error::programming(
                        "membership-bound credential overrides are unsupported until a later release",
                    )));
                }
                Ok(Some(AttemptFeature {
                    member: Arc::clone(member),
                    snapshot,
                }))
            }
            None => Ok(None),
        }
    }

    /// Log in with a password (§14.4).
    ///
    /// # Cancellation
    ///
    /// Once the attempt passed the bounds, the pre-filter, the admission and phase A, the rest
    /// (the hash, the verdict and its write: the failure count, or the session) runs in a task
    /// of its own (`tokio::spawn`), so dropping this future (a client that hangs up) never loses
    /// a failure count: the guess is verified and counted all the same (rust review M2). A
    /// [`LoginError::Failed`] outcome or a panic is counted ([`AuthStats::verdict_failures`]) and
    /// returned; the task logs it at `error` itself only when nobody awaits it any more, so the
    /// caller that receives `Failed` reports it and nothing is logged twice. A success
    /// whose caller is gone leaves a session nobody holds (idle collection ends it).
    /// may therefore drop the future on disconnect or deadline; it must not run
    /// logins outside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// [`LoginError::Denied`], [`LoginError::Cooldown`], [`LoginError::Overloaded`] (the login
    /// admission's wait expired, or the settings read's: see [`Auth::settings`]), or
    /// [`LoginError::Failed`] for anything that is not a decision about the credential (nothing
    /// is counted then).
    pub async fn authenticate(&self, request: LoginRequest) -> Result<Login, LoginError> {
        if self.selection_overridden && self.checks_overridden {
            return Err(LoginError::Failed(Error::programming(
                "custom login selection combined with credential override is unsupported until a later release",
            )));
        }
        let LoginRequest {
            login,
            password,
            client,
            user_agent,
            interactive,
        } = request;
        self.screen(&login, &password, client)?;
        let admitted = admit(
            &self.admission,
            self.pool.config().queue_wait(),
            &self.counters,
        )
        .await?;
        let cached = self.settings_current().await.map_err(|e| {
            if e.condition() == Some(Condition::Overloaded) {
                LoginError::Overloaded {
                    retry_after: self.pool.config().queue_wait(),
                }
            } else {
                LoginError::Failed(e)
            }
        })?;
        let feature = self.feature_attempt().await?;
        let principal_accounting = self.selection_overridden
            || feature
                .as_ref()
                .is_some_and(|feature| feature.snapshot.enabled());
        let keys = Keys::new(&cached.throttle, client, &login);
        let policy = self
            .cooldown_policy(&cached.settings, client, &login, &keys)
            .await?;
        let expected_generation = feature
            .as_ref()
            .filter(|feature| !feature.snapshot.enabled())
            .map(|feature| feature.snapshot.generation());
        let (attempt, phase_a, plan) = self
            .phase_a(principal_accounting, expected_generation)
            .run(&keys, &login, &cached.throttle)
            .await?;
        if let Some(refusal) = cooldown(&cached.settings, &policy, &attempt, &keys) {
            return Err(self.refuse_cooldown(phase_a, &keys, refusal).await);
        }
        let verdict = Verdict {
            orm: self.orm.clone(),
            pool: self.registration.clone(),
            counters: Arc::clone(&self.counters),
            cached,
            credential: Arc::new(Credential::Password { login, password }),
            interactive,
            attempt,
            keys,
            client,
            user_agent,
            sql: Arc::clone(&self.sql),
            has_languages: self.has_languages,
            checks_overridden: self.checks_overridden,
            cooldown_overridden: self.cooldown_overridden,
            principal_accounting,
            feature,
            limits: self.selection_limits,
            plan,
            cooldown_deadline: self.cooldown_deadline,
            policy,
            _admitted: admitted,
        };
        hand_off(verdict, phase_a, Arc::clone(&self.counters)).await
    }

    /// The additional cooldown rules of an attempt ([`cooldown_policy`]).
    async fn cooldown_policy(
        &self,
        settings: &AuthSettings,
        client: ClientAddr,
        login: &str,
        keys: &Keys,
    ) -> Result<LoginCooldownPolicy, LoginError> {
        cooldown_policy(
            &self.orm,
            CooldownHook {
                overridden: self.cooldown_overridden,
                deadline: self.cooldown_deadline,
            },
            &self.counters,
            settings,
            client,
            login,
            keys,
        )
        .await
    }

    /// The checks before any database work: the credential's size and the pre-filter.
    fn screen(&self, login: &str, password: &str, client: ClientAddr) -> Result<(), LoginError> {
        let limit = self.config.credential_bytes();
        if login.len() > limit || password.len() > limit {
            return Err(LoginError::Denied);
        }
        if let ClientAddr::Known(addr) = client {
            let address = throttle::address_key(addr);
            if !self.prefilter.admit(&address) {
                tracing::info!(target: "erp::auth", "Login refused by the pre-filter for {address}");
                return Err(LoginError::Cooldown);
            }
        }
        Ok(())
    }

    /// End phase A of an attempt refused by a cooldown and return the refusal: the answer is
    /// decided, so a failed rollback of an override's phase A is logged and counted, and the
    /// client still gets the cooldown (silent-delta N-2).
    async fn refuse_cooldown(
        &self,
        phase_a: PhaseA,
        keys: &Keys,
        refusal: LoginError,
    ) -> LoginError {
        if let Err(r) = end_phase_a(phase_a, false).await {
            bump(&self.counters.failure_write_errors);
            tracing::error!(
                target: "erp::auth",
                "could not roll back the login transaction of login key {}: {r}",
                keys.tag()
            );
        }
        refusal
    }

    /// Open the request's transaction for the session of the presented cookie `secret` (§14.3):
    /// the check rides in the opening flight; `options` carry the request's context, limits and
    /// read intent.
    ///
    /// # Errors
    ///
    /// `SessionExpired` ("user is not connected", "session is too old") for a refused session;
    /// the row of a session that can never pass again is deleted first, with the session-end
    /// hooks, on the primary in its own transaction (a failure of that deletion is logged and
    /// counted, [`AuthStats::refusal_delete_failures`]). `ResourceExhausted` with
    /// [`Condition::Overloaded`] when the settings could not be read within the hashing pool's
    /// queue wait (as [`Auth::settings`]; nothing is deleted then). Each read has its own queue
    /// wait, and a check whose keys went stale reads them again, so a check waits up to two queue
    /// waits for the settings (round 4 C4-7). `Concurrency` when the keys changed twice during one
    /// check. The flight's errors.
    pub async fn begin_session(
        &self,
        secret: &SessionSecret,
        options: CredentialOptions,
    ) -> Result<SessionBegin> {
        let digest = secret.digest();
        for _ in 0..2 {
            let cached = self.settings_current().await?;
            let check = SessionCheck {
                orm: &self.orm,
                sql: &self.check_sql,
                digest,
                cached,
                config: &self.config,
            };
            match self.orm.begin_credential(options.clone(), &check).await? {
                CredentialBegin::Open { tx, value } => {
                    return Ok(SessionBegin { tx, session: value });
                }
                CredentialBegin::Refused(refused) if refused.reason() == Refusal::KeysStale => {
                    self.cache_drop();
                }
                CredentialBegin::Refused(refused) => {
                    if let Some((row, version)) = refused.observed_row() {
                        // Keep the cold deletion state out of every request-check future.
                        Box::pin(self.end_after_refusal(row, version)).await;
                    }
                    return Err(session::session_expired(refused.reason()));
                }
            }
        }
        Err(Error::new(
            ErrorKind::Concurrency,
            "the session keys changed twice during one session check; retry the request",
        ))
    }

    /// Recheck a session and resolve its current identity in an already-open lifecycle
    /// transaction on the primary. Settings and keys are read from that same snapshot.
    /// This does not change the transaction's authority; only the lifecycle engine may
    /// bind the returned human after validating its administrative group.
    /// # Errors
    /// SessionExpired for revoked/inactive/expired credentials; Programming for a read-pool
    /// transaction; database/settings errors propagate without exposing credential data.
    pub async fn check_session_in(
        &self,
        tx: &mut Transaction,
        secret: &SessionSecret,
    ) -> Result<SessionAuthorization> {
        use erp_orm::{CredentialCheck, CredentialVerdict};
        if tx.route() != erp_orm::Route::Primary {
            return Err(Error::programming(
                "Lifecycle session checks require the primary transaction",
            ));
        }
        let loaded = settings::load(tx).await?;
        let cached = Arc::new(Cached {
            generation: self.orm.ormcache_generation(CACHE_NAME),
            settings: loaded.settings,
            keys: loaded.keys,
            throttle: loaded.throttle,
            apikey: loaded.apikey,
            browser: loaded.browser,
            token_plan: Arc::clone(&self.token_plan),
        });
        let check = SessionCheck {
            orm: &self.orm,
            sql: &self.check_sql,
            digest: secret.digest(),
            cached,
            config: &self.config,
        };
        let digest = session::hex(&check.digest);
        let rows = tx.query(&self.check_sql, &[&digest]).await?;
        // Lifecycle transactions intentionally use the engine's System-only policy. Build
        // current human policy from the same primary snapshot rather than treating it as
        // a human authorization source or importing another transaction's cached groups.
        let policy_rows = crate::loader::load_rows(tx).await?;
        let policy: Arc<dyn erp_orm::AccessPolicy> = Arc::new(
            crate::snapshot::Compiler::new(Arc::clone(tx.registry())).compile(&policy_rows)?,
        );
        match check.finish(&rows, policy.as_ref())? {
            CredentialVerdict::Accepted { principal, value } => Ok(SessionAuthorization {
                principal,
                session: value,
                policy,
            }),
            CredentialVerdict::Refused(refused) => Err(session::session_expired(refused.reason())),
            _ => Err(Error::programming(
                "Unsupported lifecycle credential verdict",
            )),
        }
    }

    async fn end_after_refusal(&self, row: SessionId, version: i64) {
        let absorbed = &self.counters.retries.refusal_delete;
        let outcome = retry(
            DEADLOCK,
            absorbed,
            "the deletion of a refused session",
            || end_observed_session(&self.orm, row, version),
        )
        .await;
        if let Err(e) = outcome {
            bump(&self.counters.refusal_delete_failures);
            tracing::warn!(
                target: "erp::auth",
                "could not delete refused session {row}: {e}; it keeps failing its checks"
            );
        }
    }

    /// Rotate the secret of an accepted session: `Some(new secret)` for the
    /// caller's cookie, `None` when another request rotated it first (or the session ended): the
    /// caller keeps the presented secret, valid for the grace. One READ COMMITTED flight: a
    /// rotation that waited for a concurrent one re-reads the row, finds its digest changed and
    /// sets nothing (the two converge on one successor). The new cookie reaches the client only
    /// in this request's response: a response that is lost leaves the client on the previous
    /// secret, refused once the grace is over (design §3.3;'s follow-up).
    ///
    /// # Errors
    ///
    /// The statement's and the commit's (a deadlock after three tries).
    pub async fn rotate(
        &self,
        session: &CheckedSession,
        presented: &SessionSecret,
    ) -> Result<Option<SessionSecret>> {
        let next = SessionSecret::generate()?;
        let id = session.id().to_text();
        let new_hex = session::hex(&next.digest());
        let old_hex = session::hex(&presented.digest());
        let params: [(&(dyn ToSql + Sync), Type); 3] = [
            (&id, Type::TEXT),
            (&new_hex, Type::TEXT),
            (&old_hex, Type::TEXT),
        ];
        let absorbed = &self.counters.retries.rotate;
        let rows = retry(DEADLOCK, absorbed, "rotate", || {
            one_flight(&self.orm, read_committed(), session::ROTATE_SQL, &params)
        })
        .await?;
        Ok((!rows.is_empty()).then_some(next))
    }

    /// Write `last_seen_at`, the address and the user agent of `touches` not seen within
    /// `session.touch_interval_secs`, and drop predecessors past their grace; returns the rows
    /// written. Rows another transaction holds are skipped (the next request touches them). One
    /// READ COMMITTED flight. When `touches` names one session twice, the first entry's address
    /// and agent are written (type design N7).
    ///
    /// # Errors
    ///
    /// The statement's and the commit's (a deadlock after three tries).
    pub async fn touch(&self, touches: &[Touch]) -> Result<u64> {
        if touches.is_empty() {
            return Ok(0);
        }
        // a stable sort keeps the first entry of each id first; dedup keeps it
        let mut sorted: Vec<&Touch> = touches.iter().collect();
        sorted.sort_by_key(|t| t.id());
        sorted.dedup_by_key(|t| t.id());
        let ids: Vec<String> = sorted.iter().map(|t| t.id().to_text()).collect();
        let addrs: Vec<Option<String>> = sorted
            .iter()
            .map(|t| t.address().map(|a| a.to_string()))
            .collect();
        let agents: Vec<Option<&str>> = sorted.iter().map(|t| t.user_agent()).collect();
        let interval = self.config.touch_interval().as_secs_f64();
        let grace = self.config.rotation_grace().as_secs_f64();
        let params: [(&(dyn ToSql + Sync), Type); 5] = [
            (&ids, Type::TEXT_ARRAY),
            (&addrs, Type::TEXT_ARRAY),
            (&agents, Type::TEXT_ARRAY),
            (&interval, Type::FLOAT8),
            (&grace, Type::FLOAT8),
        ];
        let absorbed = &self.counters.retries.touch;
        let rows = retry(DEADLOCK, absorbed, "touch", || {
            one_flight(&self.orm, read_committed(), session::TOUCH_SQL, &params)
        })
        .await?;
        Ok(rows.len() as u64)
    }

    /// End the session `id` (logout): the row, through the session-end hooks,
    /// the subscribers' rows go in one READ COMMITTED transaction (hooks use raw statements only,
    /// as on every path; a deadlock is retried up to three tries, database review round 2 N9).
    /// `false` when no such row exists.
    ///
    /// The id is not a secret (UUIDv7, it appears in CSRF tokens and audit rows): the caller
    /// passes the id of the session it has just checked, or, on a devices page, an id it has
    /// checked belongs to the requesting user. Ending sessions by an id a client merely presents
    /// would let anyone end other users' sessions (security review round 2 N5).
    ///
    /// # Errors
    ///
    /// The statement's, the hooks' and the commit's (a deadlock after three tries).
    pub async fn logout(&self, id: SessionId) -> Result<bool> {
        let absorbed = &self.counters.retries.logout;
        let ended = retry(DEADLOCK, absorbed, "logout", || {
            end_sessions_now(&self.orm, vec![id], EndReason::Logout)
        })
        .await?;
        Ok(ended > 0)
    }
}

/// Run `verdict` in a task of its own and answer with its outcome (cancellation, see
/// [`Auth::authenticate`]); a watch task keeps the outcome when the caller is gone.
async fn hand_off(
    verdict: Verdict,
    phase_a: PhaseA,
    counters: Arc<Counters>,
) -> Result<Login, LoginError> {
    let login_key = verdict.keys.login;
    let task = tokio::spawn(verdict.run(phase_a));
    let (reply, answer) = oneshot::channel();
    tokio::spawn(watch(task, counters, login_key, reply));
    match answer.await {
        Ok(outcome) => outcome,
        Err(_) => Err(LoginError::Failed(Error::programming(
            "the login's watch task ended without an answer",
        ))),
    }
}

/// Await the verdict task and keep its outcome when nobody else will (silent-failure review S1):
/// a `Failed` outcome or a panic is counted, and handed to the caller through `reply`; it is
/// logged here, at `error` with the login's tag, only when the caller is gone (the reply cannot
/// be delivered), so a failure is logged once (types-delta N-e). A panic keeps its payload text
/// (silent-delta N-1); a task cancelled by the runtime shutting down is reported as such.
async fn watch<T: Send + 'static>(
    task: tokio::task::JoinHandle<Result<T, LoginError>>,
    counters: Arc<Counters>,
    login_key: [u8; 32],
    reply: oneshot::Sender<Result<T, LoginError>>,
) {
    let outcome = match task.await {
        Ok(outcome) => outcome,
        Err(e) if e.is_panic() => Err(LoginError::Failed(Error::programming(format!(
            "the login's verdict task panicked: {}",
            panic_text(e.into_panic().as_ref())
        )))),
        Err(_) => Err(LoginError::Failed(Error::programming(
            "the login's verdict task was cancelled: the runtime is shutting down",
        ))),
    };
    if let Err(LoginError::Failed(_)) = &outcome {
        bump(&counters.verdict_failures);
    }
    if let Err(Err(LoginError::Failed(e))) = reply.send(outcome) {
        tracing::error!(
            target: "erp::auth",
            "Login for login key {} failed after its caller was gone: {e}",
            login_tag(&login_key)
        );
    }
}

/// The text of a panic payload (`panic!` with a literal or a format string), or a note that it
/// carries none.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        text
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text
    } else {
        "(a payload that is not text)"
    }
}

/// End the sessions `ids` with the hooks in one READ COMMITTED transaction (hooks use raw
/// statements only, [`super::hooks`]; database review round 2 N9); how many ended.
async fn end_sessions_now(orm: &Orm, ids: Vec<SessionId>, reason: EndReason) -> Result<usize> {
    let mut tx = orm.begin_opts(read_committed()).await?;
    match store::end_sessions(&mut tx, &ids, reason).await {
        Ok(ended) => {
            tx.commit().await?;
            Ok(ended.len())
        }
        Err(e) => Err(abandon(&mut tx, e).await),
    }
}

async fn end_observed_session(orm: &Orm, id: SessionId, version: i64) -> Result<usize> {
    let mut tx = orm.begin_opts(read_committed()).await?;
    match store::end_with(
        &mut tx,
        store::END_OBSERVED_SESSION,
        &[&id.to_text(), &version],
        EndReason::Expired,
    )
    .await
    {
        Ok(ended) => {
            tx.commit().await?;
            Ok(ended.len())
        }
        Err(e) => Err(abandon(&mut tx, e).await),
    }
}

/// Which other sessions a successful login ends (type design N3).
#[derive(Debug, Clone, Copy)]
enum EndOthers {
    /// After a rehash: sessions whose tokens are obsolete under current locked credentials.
    AfterRehash,
    /// Past [`SESSIONS_PER_USER`]: the oldest beyond it.
    OverCap,
}

impl EndOthers {
    /// Other sessions kept, newest first.
    fn keep(self) -> i64 {
        match self {
            Self::AfterRehash => 0,
            Self::OverCap => i64::from(SESSIONS_PER_USER) - 1,
        }
    }

    fn reason(self) -> EndReason {
        match self {
            Self::AfterRehash => EndReason::CredentialChanged,
            Self::OverCap => EndReason::OverCap,
        }
    }
}

/// The part of a login after phase A, owned so it runs in a task of its own (cancellation,
/// [`Auth::authenticate`]): the verify, the verdict and its write.
struct Verdict {
    orm: Orm,
    pool: pool::PoolBinding,
    counters: Arc<Counters>,
    cached: Arc<Cached>,
    credential: Arc<Credential>,
    interactive: bool,
    attempt: Attempt,
    keys: Keys,
    client: ClientAddr,
    user_agent: Option<String>,
    /// What phase A needs to run again (one restart, database review round 2 N7).
    sql: Arc<PlanSql>,
    has_languages: bool,
    checks_overridden: bool,
    cooldown_overridden: bool,
    principal_accounting: bool,
    feature: Option<AttemptFeature>,
    limits: LoginSelectionLimits,
    plan: Option<LoginSelectionPlan>,
    cooldown_deadline: Duration,
    /// The hook's rules for this attempt; asked again before the restart.
    policy: LoginCooldownPolicy,
    /// Held until the verdict is written.
    _admitted: OwnedSemaphorePermit,
}

impl Verdict {
    fn login(&self) -> &str {
        let Credential::Password { login, .. } = &*self.credential;
        login
    }

    /// Log and count a failed rollback of phase A that must not change the answer.
    fn rollback_failed(&self, r: &Error) {
        bump(&self.counters.failure_write_errors);
        tracing::error!(
            target: "erp::auth",
            "could not roll back the login transaction of login key {}: {r}",
            self.keys.tag()
        );
    }

    /// The verdict; `phase_a` is phase A's transaction when an addon overrides
    /// `_check_credentials` (kept out of `self`: a transaction is `Send`, not `Sync`). A correct
    /// password whose phase B finds the credential changed since phase A (typically a concurrent
    /// login that rehashed it) reads phase A again, applies the cooldown again (security review
    /// round 2 delta N-a) and verifies once more, once (database review round 2 N7); a second
    /// change answers `Denied`.
    async fn run(mut self, phase_a: PhaseA) -> Result<Login, LoginError> {
        if let Some(login) = self.attempt_once(phase_a).await? {
            return Ok(login);
        }
        bump(&self.counters.login_restarts);
        tracing::info!(
            target: "erp::auth",
            "Login for login key {}: the credential changed while it was verified; reading it \
             again",
            self.keys.tag()
        );
        if let Some(feature) = &self.feature {
            let current = feature.member.snapshot(&self.orm).await?;
            if current.generation() != feature.snapshot.generation()
                || current.enabled() != feature.snapshot.enabled()
            {
                return Err(LoginError::Failed(Error::programming(
                    "login selection rollout generation changed",
                )));
            }
        }
        self.policy = cooldown_policy(
            &self.orm,
            CooldownHook {
                overridden: self.cooldown_overridden,
                deadline: self.cooldown_deadline,
            },
            &self.counters,
            &self.cached.settings,
            self.client,
            self.login(),
            &self.keys,
        )
        .await?;
        let phase_a = self.read_again().await?;
        // the counters may have moved since the first phase A (security review round 2 delta N-a)
        if let Some(refusal) = cooldown(
            &self.cached.settings,
            &self.policy,
            &self.attempt,
            &self.keys,
        ) {
            if let Err(r) = end_phase_a(phase_a, false).await {
                self.rollback_failed(&r);
            }
            return Err(refusal);
        }
        if let Some(login) = self.attempt_once(phase_a).await? {
            return Ok(login);
        }
        tracing::info!(
            target: "erp::auth",
            "Login for login key {} answered Denied: the user's password or active flag changed \
             while it was verified, twice",
            self.keys.tag()
        );
        Err(LoginError::Denied)
    }

    /// One verify and its verdict; `None` when the credential changed since phase A (nothing
    /// written).
    async fn attempt_once(&mut self, mut phase_a: PhaseA) -> Result<Option<Login>, LoginError> {
        let auth = match self.verify(&mut phase_a).await {
            Ok(auth) => auth,
            Err(e) => return self.refuse(e, phase_a).await.map(Some),
        };
        self.succeed(auth, phase_a).await
    }

    /// Phase A again, for the restart.
    async fn read_again(&mut self) -> Result<PhaseA, LoginError> {
        let read = PhaseARead {
            orm: &self.orm,
            sql: &self.sql.login,
            plan: &self.cached.token_plan,
            has_languages: self.has_languages,
            overridden: self.checks_overridden,
            principal_accounting: self.principal_accounting,
            expected_generation: self
                .feature
                .as_ref()
                .filter(|feature| !feature.snapshot.enabled())
                .map(|feature| feature.snapshot.generation()),
            limits: self.limits,
        };
        let login = self.login().to_owned();
        let (attempt, phase_a, plan) = read.run(&self.keys, &login, &self.cached.throttle).await?;
        if self.attempt.base_generation != attempt.base_generation {
            let error = Error::programming("login selection rollout generation changed");
            return Err(self
                .abandon_phase_a(phase_a, LoginError::Failed(error))
                .await);
        }
        self.attempt = attempt;
        self.plan = plan;
        Ok(phase_a)
    }

    /// The answer to a verify that did not accept: a refusal is answered `Denied` and counted
    /// whatever happens to phase A's rollback (the connection is discarded then, which rolls
    /// back on the server); when the count itself cannot be written the answer is `Failed` with
    /// the store's error instead (LD2); the pool's expired wait is `Denied`, not counted (load); a hash
    /// needing more memory than a permit reserves is `Denied` and counted (a configuration
    /// fault, never a free answer: security review round 2 delta SD-1; the base check already
    /// turns its own into a refusal, this catches an override's); anything else `Failed`.
    async fn refuse<T>(&self, e: Error, phase_a: PhaseA) -> Result<T, LoginError> {
        if e.kind() == ErrorKind::AccessDenied {
            if let Err(r) = end_phase_a(phase_a, false).await {
                self.rollback_failed(&r);
            }
            self.write_failure().await.map_err(LoginError::Failed)?;
            return Err(LoginError::Denied);
        }
        let e = match end_phase_a(phase_a, false).await {
            Ok(()) => e,
            Err(r) => e.and_also(&r),
        };
        match pool_refusal(&e) {
            Some(refusal) => {
                log_pool_refusal(refusal, &self.keys.tag());
                if matches!(refusal, HashPoolError::MemoryOverReservation { .. }) {
                    self.write_failure().await.map_err(LoginError::Failed)?;
                }
                Err(LoginError::Denied)
            }
            None => Err(LoginError::Failed(e)),
        }
    }

    /// The hash: the dummy verify for an unknown login (phase A's open transaction, if any, is
    /// ended first: nothing is held through the dummy, type design T6), the dispatched
    /// `_check_credentials` when an addon overrides it (in phase A's transaction), else the base
    /// password check on the stored hash phase A read (no transaction).
    async fn verify(&mut self, phase_a: &mut PhaseA) -> Result<AuthInfo> {
        let params = *self.cached.settings.hash_params();
        let Some(user) = &self.attempt.user else {
            let open = std::mem::replace(phase_a, PhaseA::Flight);
            if let Err(r) = end_phase_a(open, false).await {
                self.rollback_failed(&r);
            }
            dummy::verify(&self.pool, &params).await?;
            return Err(credential::access_denied());
        };
        let PhaseA::Open(tx) = phase_a else {
            let stored = user.stored.as_deref().map(String::as_str);
            return credential::verify_stored(
                user.uid,
                &self.credential,
                stored,
                &params,
                &self.pool,
            )
            .await;
        };
        let env = CheckEnv::new(
            self.interactive,
            StoredPassword::Preloaded(user.stored.as_deref().cloned()),
            params,
            self.pool.raw(),
        );
        check_credentials(tx, user.uid, Arc::clone(&self.credential), env).await
    }

    /// Phase B of a failure: both keys counted in one READ COMMITTED flight, run again after a
    /// deadlock ([`super::retry`]). A failure of the write is logged, counted and returned: the
    /// caller answers `Failed` with the store's error (kind and message kept), never `Denied`, so
    /// a counted wrong-password verdict stays distinct from one whose authoritative write failed.
    async fn write_failure(&self) -> Result<()> {
        let cap = throttle::UNKNOWN_LOGIN_ROWS_PER_ADDRESS;
        let address = self.keys.address();
        let principal = self
            .attempt
            .user
            .as_ref()
            .filter(|_| self.principal_accounting)
            .map(|user| {
                self.cached
                    .throttle
                    .key_hash(ThrottleKey::Principal(user.uid))
            });
        let params: [(&(dyn ToSql + Sync), Type); 4] = [
            (&address, Type::BYTEA),
            (&self.keys.login.as_slice(), Type::BYTEA),
            (&cap, Type::INT4),
            (&principal.as_ref().map(|p| p.as_slice()), Type::BYTEA),
        ];
        let (sql, params) = if self.principal_accounting {
            (throttle::FAILURE_SELECTION_SQL, &params[..])
        } else {
            (throttle::FAILURE_SQL, &params[..3])
        };
        let from = self.keys.address_text();
        // one statement: a deadlock rolls all of it back, so a rerun counts once (database
        // review round 3 DD-1)
        let absorbed = &self.counters.retries.failure_write;
        let written = retry(DEADLOCK, absorbed, "the failure count of a login", || {
            one_flight(&self.orm, read_committed(), sql, params)
        })
        .await;
        if let Err(e) = written {
            bump(&self.counters.failure_write_errors);
            tracing::error!(
                target: "erp::auth",
                "could not count the login failure of login key {} from {from}: {e}",
                self.keys.tag()
            );
            return Err(e);
        }
        tracing::info!(
            target: "erp::auth",
            "Login failed for login key {} from {from}",
            self.keys.tag()
        );
        Ok(())
    }

    /// End phase A without a success and return `error`: a rollback failure is added to a
    /// `Failed` error, and logged and counted otherwise (a `Denied` stays `Denied`;
    /// silent-failure review S2).
    async fn abandon_phase_a(&self, phase_a: PhaseA, error: LoginError) -> LoginError {
        match (end_phase_a(phase_a, false).await, error) {
            (Ok(()), error) => error,
            (Err(r), LoginError::Failed(e)) => LoginError::Failed(e.and_also(&r)),
            (Err(r), other) => {
                self.rollback_failed(&r);
                other
            }
        }
    }

    /// The success: the rehash (on the pool), phase A ended, phase B, then the sessions the
    /// login ends (the cap; every other session after a rehash, lead decision 6). `None`: the
    /// credential changed since phase A, nothing was written.
    ///
    /// Under custom selection or a bound rollout the finalization is one guarded transaction
    /// ([`Self::finalize_once`]) that is run again after a deadlock, counted in
    /// [`AuthStats::deadlock_retries`] `.success`.
    async fn succeed(
        &mut self,
        auth: AuthInfo,
        phase_a: PhaseA,
    ) -> Result<Option<Login>, LoginError> {
        let Some(user) = self.attempt.user.take() else {
            let error =
                Error::programming("_check_credentials accepted a login that names no active user");
            return Err(self
                .abandon_phase_a(phase_a, LoginError::Failed(error))
                .await);
        };
        if user.uid != auth.uid() {
            let error = Error::programming(format!(
                "_check_credentials of user {} returned user {}",
                user.uid.get(),
                auth.uid().get()
            ));
            return Err(self
                .abandon_phase_a(phase_a, LoginError::Failed(error))
                .await);
        }
        // Hashing remains outside the finalization transaction. No DB guard is held
        // while waiting for the hashing pool (accepted contract §4).
        let rehash = match self.rehash(&auth).await {
            Ok(rehash) => rehash,
            Err(e) => return Err(self.abandon_phase_a(phase_a, e).await),
        };
        if user.witness.is_some() && self.plan.is_none() {
            return Err(self
                .abandon_phase_a(
                    phase_a,
                    LoginError::Failed(Error::programming(
                        "missing selection plan for witness verification",
                    )),
                )
                .await);
        }
        let rehash_hash = rehash.as_deref().map(String::as_str);
        let mut phase_a = Some(phase_a);
        // A guarded finalization (a rollout feature or a selection witness) runs in one
        // transaction that a deadlock (40P01) aborts as a whole: the guard, the re-selection and
        // the success statement run again from the start, at most `DEADLOCK_TRIES` times in all.
        // The unguarded base path keeps the retry of its one-statement flight.
        let guarded = self.feature.is_some() || user.witness.is_some();
        let mut tries = 1;
        let mut absorbed: Option<Error> = None;
        let outcome = loop {
            let mut phase_a_failed = false;
            let error = match self
                .finalize_once(&user, rehash_hash, &mut phase_a, &mut phase_a_failed)
                .await
            {
                Ok(outcome) => break outcome,
                Err(error) => error,
            };
            if guarded && !phase_a_failed && is_retried(DEADLOCK, &error, tries) {
                absorb(
                    &self.counters.retries.success,
                    "the guarded success of a login",
                    tries,
                    &error,
                )
                .await;
                tries += 1;
                absorbed = Some(error);
                continue;
            }
            // the absorbed deadlock stays the cause when the rerun fails (lead review r1 S-1)
            return Err(LoginError::Failed(match absorbed {
                Some(cause) => error.and_also(&cause),
                None => error,
            }));
        };
        let Some((session, context, sessions)) = outcome else {
            return Ok(None);
        };
        let others = if rehash.is_some() {
            Some(EndOthers::AfterRehash)
        } else if sessions > i64::from(SESSIONS_PER_USER) {
            Some(EndOthers::OverCap)
        } else {
            None
        };
        if let Some(others) = others {
            self.end_other_sessions(user.uid, session.id, others).await;
        }
        Ok(Some(Login {
            session,
            auth,
            context,
        }))
    }

    /// One try of the guarded finalization: a READ COMMITTED transaction takes the protection of
    /// the rollout feature (when bound) and the table guard, selects the candidate again and
    /// compares it with the witness, ends phase A, writes the session and commits. `None`: the
    /// selection or the credential changed since phase A, nothing was written (phase A ended
    /// without commit when it was still open).
    ///
    /// A deadlock aborts the whole transaction and its error keeps the SQLSTATE, so a caller may
    /// run this again; phase A is taken out of `phase_a` once, by the first try that gets that
    /// far, and later tries find it ended (`None` is an ended phase A, not a default). When
    /// ending phase A with its commit fails, `phase_a_failed` is set: its writes may be lost, so
    /// no rerun may log in without them (the order of the bound path before LOGIN-RACES, where
    /// phase A ended outside the retry). Without a guard (the base path) the success statement
    /// is its own flight and retries itself ([`Self::success_flight`]).
    async fn finalize_once(
        &self,
        user: &LoginUser,
        rehash: Option<&str>,
        phase_a: &mut Option<PhaseA>,
        phase_a_failed: &mut bool,
    ) -> Result<Option<(NewSession, UserContext, i64)>> {
        let mut selection_tx = if let Some(feature) = &self.feature {
            let mut tx = self.orm.begin_opts(read_committed()).await?;
            if let Err(error) = feature
                .member
                .binding
                .protect_in(&mut tx, &feature.snapshot)
                .await
            {
                return Err(abandon(&mut tx, rollout_error(error)).await);
            }
            Some(tx)
        } else {
            None
        };
        if let Some(witness) = &user.witness {
            let Some(plan) = &self.plan else {
                return Err(Error::programming(
                    "missing selection plan for witness verification",
                ));
            };
            let mut tx = match selection_tx.take() {
                Some(tx) => tx,
                None => self.orm.begin_opts(read_committed()).await?,
            };
            if let Err(error) = tx
                .execute("LOCK TABLE res_users IN SHARE ROW EXCLUSIVE MODE", &[])
                .await
            {
                return Err(abandon(&mut tx, error).await);
            }
            let fresh =
                match dispatch_selection(&mut tx, self.login(), &self.limits, self.orm.registry())
                    .await
                {
                    Ok(plan) => plan,
                    Err(error) => return Err(abandon(&mut tx, error).await),
                };
            let candidate = match select_candidate(&mut tx, &fresh, &self.limits).await {
                Ok(candidate) => candidate,
                Err(error) => return Err(abandon(&mut tx, error).await),
            };
            let matches = fresh.domain.domain() == plan.domain.domain()
                && fresh.order == plan.order
                && candidate.is_some_and(|(uid, _, _, _, _, new_witness)| {
                    uid == user.uid && new_witness == *witness
                });
            if !matches {
                tx.rollback().await?;
                end_phase_a(phase_a.take().unwrap_or(PhaseA::Flight), false).await?;
                return Ok(None);
            }
            // This transaction owns the table guard through session/rehash persistence.
            selection_tx = Some(tx);
        }
        if let Err(error) = end_phase_a(phase_a.take().unwrap_or(PhaseA::Flight), true).await {
            *phase_a_failed = true;
            return Err(match selection_tx.as_mut() {
                Some(tx) => abandon(tx, error).await,
                None => error,
            });
        }
        let written = self
            .write_success(user, rehash, selection_tx.as_mut())
            .await;
        let outcome = match written {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(match selection_tx.as_mut() {
                    Some(tx) => abandon(tx, error).await,
                    None => error,
                });
            }
        };
        if let Some(tx) = selection_tx.as_mut() {
            tx.commit().await?;
        }
        Ok(outcome)
    }

    /// The new hash of the presented password when `_check_credentials` asked for a rehash.
    /// A rehash the pool cannot run (its wait expired, or the current parameters need more memory
    /// than a permit reserves) is skipped: the password was verified, refusing it would protect
    /// nothing and lock every user out of a server whose settings outgrew its pool (security
    /// review round 2 M1). Logged and counted ([`AuthStats::rehash_skipped`]; the pool counts the
    /// refusal too); the next login tries again.
    async fn rehash(&self, auth: &AuthInfo) -> Result<Option<Zeroizing<String>>, LoginError> {
        if !auth.rehash() {
            return Ok(None);
        }
        let params = *self.cached.settings.hash_params();
        let Credential::Password { password, .. } = &*self.credential;
        let secret = Zeroizing::new(password.as_bytes().to_vec());
        let made = self
            .pool
            .run(params.memory_kib(), move || {
                password::hash(&secret, &params)
            })
            .await;
        match made {
            Ok(Ok(hash)) => Ok(Some(Zeroizing::new(hash))),
            Ok(Err(e)) => Err(LoginError::Failed(e.into())),
            Err(e) => {
                let e = Error::from(e);
                match pool_refusal(&e) {
                    Some(refusal) => {
                        bump(&self.counters.rehash_skipped);
                        log_rehash_skipped(refusal, &self.keys.tag());
                        Ok(None)
                    }
                    None => Err(LoginError::Failed(e)),
                }
            }
        }
    }

    /// The new session of `user` (its secret, id and token; the password the token covers is the
    /// rehash when there is one) and the context's language.
    fn new_session(&self, user: &LoginUser, rehash: Option<&str>) -> Result<Prepared> {
        let lang = self.attempt.languages.context_lang(user.lang.as_deref());
        let secret = SessionSecret::generate()?;
        let id = SessionId::generate()?;
        let (kid, key) = self.cached.keys.issuing();
        let login = user
            .canonical_login
            .as_deref()
            .map(String::as_str)
            .unwrap_or_else(|| self.login());
        let extra = self.cached.token_plan.bind(&user.token_extra)?;
        let token = session::token(
            key.bytes(),
            &id,
            &TokenFields {
                user: user.uid,
                login,
                password: rehash
                    .or(user.stored.as_deref().map(String::as_str))
                    .unwrap_or(""),
                active: true,
                extra: &extra,
            },
        )?;
        Ok(Prepared {
            secret,
            id,
            token,
            kid,
            lang,
        })
    }

    /// Phase B of a success: the session row and the rest in one READ COMMITTED flight,
    /// conditioned on the stored hash phase A read. The session, the context and the user's
    /// session count with this one; `None` when the credential changed since phase A (nothing
    /// written).
    async fn write_success(
        &self,
        user: &LoginUser,
        rehash: Option<&str>,
        selection_tx: Option<&mut Transaction>,
    ) -> Result<Option<(NewSession, UserContext, i64)>> {
        let prepared = self.new_session(user, rehash)?;
        let rows = self
            .success_flight(user, rehash, &prepared, selection_tx)
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let before: i64 = col(row, 0)?;
        let Prepared {
            secret, id, lang, ..
        } = prepared;
        let context = UserContext::new(user.uid, lang, user.tz.clone());
        let session = NewSession {
            secret,
            id,
            lifetime: self.cached.settings.max_lifetime(),
        };
        Ok(Some((session, context, before + 1)))
    }

    /// The success statement's flight (parameters as `success_tail!` names them). Without a guard
    /// transaction it runs again after a deadlock ([`super::retry`]) here. With `selection_tx` a
    /// deadlock (40P01) aborts that whole transaction, the table guard included, so it is not
    /// retried here: the error keeps its SQLSTATE and `succeed` runs the entire guarded
    /// finalization again ([`Self::finalize_once`]).
    async fn success_flight(
        &self,
        user: &LoginUser,
        rehash: Option<&str>,
        prepared: &Prepared,
        selection_tx: Option<&mut Transaction>,
    ) -> Result<Vec<Row>> {
        let (uid, id_text) = (user.uid.get(), prepared.id.to_text());
        let digest_hex = session::hex(&prepared.secret.digest());
        let token_hex = session::hex(&prepared.token);
        let lifetime_secs = self.cached.settings.max_lifetime().as_secs_f64();
        let address = match self.client {
            ClientAddr::Known(addr) => Some(addr.to_string()),
            ClientAddr::InProcess(_) => None,
        };
        let (low, high) = (&self.keys.trusted_range.0, &self.keys.trusted_range.1);
        let keep = i64::from(throttle::TRUSTED_PAIRS_PER_LOGIN) - 1;
        let trusted = self.keys.trusted();
        let principal_trusted = if self.principal_accounting {
            self.keys.address.as_ref().map(|a| {
                self.cached
                    .throttle
                    .key_hash(ThrottleKey::PrincipalTrusted(&a.key, user.uid))
            })
        } else {
            None
        };
        let (p_low, p_high) = self.cached.throttle.trusted_principal_range(user.uid);
        let stored_param: Option<&str> = user.stored.as_deref().map(String::as_str);
        let token_login: &str = user
            .canonical_login
            .as_deref()
            .map(String::as_str)
            .unwrap_or_else(|| self.login());
        // A bound verdict already holds protection in selection_tx. The fixture seam has no
        // base generation; only an unbound ordinary base attempt uses the same-flight guard.
        let base_generation = if self.feature.is_none() {
            self.attempt.base_generation
        } else {
            None
        };
        let token_values = user.token_extra.as_array();
        let (low_key, high_key) = (low.as_slice(), high.as_slice());
        let (p_low_key, p_high_key) = (p_low.as_slice(), p_high.as_slice());
        let principal_trusted_key = principal_trusted.as_ref().map(|p| p.as_slice());
        let mut params: Vec<(&(dyn ToSql + Sync), Type)> = vec![
            (&trusted, Type::BYTEA),
            (&low_key, Type::BYTEA),
            (&uid, Type::INT8),
            (&rehash, Type::TEXT),
            (&id_text, Type::TEXT),
            (&digest_hex, Type::TEXT),
            (&token_hex, Type::TEXT),
            (&prepared.kid, Type::INT2),
            (&lifetime_secs, Type::FLOAT8),
            (&address, Type::TEXT),
            (&self.user_agent, Type::TEXT),
            (&prepared.lang, Type::TEXT),
            (&user.tz, Type::TEXT),
            (&stored_param, Type::TEXT),
            (&high_key, Type::BYTEA),
            (&keep, Type::INT8),
            (&principal_trusted_key, Type::BYTEA),
            (&p_low_key, Type::BYTEA),
            (&p_high_key, Type::BYTEA),
            (&token_login, Type::TEXT),
            (&base_generation, Type::INT8),
        ];
        // The added token fields as phase A read them: the statement writes nothing when they
        // changed since, like the stored hash (the base plan has no such parameter).
        if !self.cached.token_plan.is_base() {
            params.push((&token_values, Type::TEXT_ARRAY));
        }
        let sql: &str = if rehash.is_some() {
            &self.sql.success_rehash
        } else {
            &self.sql.success
        };
        // one statement: a deadlock rolls all of it back (no session row, no pair, no log row),
        // so a rerun writes the same session once (database review round 3 DD-1)
        if let Some(tx) = selection_tx {
            return tx.query_typed(sql, &params).await.map_err(rollout_error);
        }
        let absorbed = &self.counters.retries.success;
        retry(
            DEADLOCK,
            absorbed,
            "the success statement of a login",
            || one_flight(&self.orm, read_committed(), sql, &params),
        )
        .await
        .map_err(rollout_error)
    }

    /// End the sessions that `others` names, with hooks in READ COMMITTED transactions
    /// (database review round 2 N9). Rehash cleanup uses bounded, independently committed
    /// batches under current user/key locks. A failure is logged and counted:
    /// at `error` after a rehash (lead decision 6 wants their hooks to run now), at `warn` over
    /// the cap (the next login applies it again).
    async fn end_other_sessions(&self, uid: UserId, new: SessionId, others: EndOthers) {
        let uid = uid.get();
        // Current-token cleanup does not use an excluded session id. Only cap cleanup
        // passes it to OTHER_SESSIONS_SQL; avoid allocating an unused UUID string on rehash.
        let new_text = match others {
            EndOthers::AfterRehash => String::new(),
            EndOthers::OverCap => new.to_text(),
        };
        let (keep, reason) = (others.keep(), others.reason());
        let absorbed = &self.counters.retries.end_others;
        let outcome = retry(DEADLOCK, absorbed, "the sessions a login ends", || {
            end_others_once(
                &self.orm,
                uid,
                &new_text,
                keep,
                reason,
                &self.cached.token_plan,
            )
        })
        .await;
        if let Err(e) = outcome {
            bump(&self.counters.session_end_failures);
            match others {
                EndOthers::AfterRehash => tracing::error!(
                    target: "erp::auth",
                    "session cleanup after rehash is incomplete for user {uid}; earlier \
                     batches may have committed; cleanup stopped: {e}"
                ),
                EndOthers::OverCap => tracing::warn!(
                    target: "erp::auth",
                    "could not end the sessions of user {uid} over the cap: {e}"
                ),
            }
        }
    }
}

/// One try of [`Verdict::end_other_sessions`]. Credential changes use bounded current-token
/// batches; OverCap ends sessions other than `new` past `keep` in one transaction.
pub(super) async fn end_others_once(
    orm: &Orm,
    uid: i64,
    new: &str,
    keep: i64,
    reason: EndReason,
    plan: &TokenPlan,
) -> Result<usize> {
    if reason == EndReason::CredentialChanged {
        end_obsolete_sessions(orm, uid, plan).await
    } else {
        end_over_cap_once(orm, uid, new, keep, reason).await
    }
}

/// Keep cap-only transaction state inside its own future. The dispatcher contains only
/// mutually exclusive child futures; its layout is verified by the attribution probe.
/// Semantics and commit allocation are unchanged.
async fn end_over_cap_once(
    orm: &Orm,
    uid: i64,
    new: &str,
    keep: i64,
    reason: EndReason,
) -> Result<usize> {
    let mut tx = orm.begin_opts(read_committed()).await?;
    let ended = async {
        let rows = tx.query(OTHER_SESSIONS_SQL, &[&uid, &new, &keep]).await?;
        let ids = rows
            .iter()
            .map(|r| {
                r.try_get::<_, String>(0)
                    .map_err(|e| Error::db(format!("erp_session.id: {e}")).with_source(e))
                    .and_then(|text| SessionId::parse(&text))
            })
            .collect::<Result<Vec<_>>>()?;
        store::end_sessions(&mut tx, &ids, reason).await
    }
    .await;
    match ended {
        Ok(ended) => {
            // Commit carries the largest cleanup child future. Box only that cold
            // await, so other login paths do not retain its frame.
            Box::pin(tx.commit()).await?;
            Ok(ended.len())
        }
        Err(e) => Err(abandon(&mut tx, e).await),
    }
}

/// Rehash cleanup is a sweep, not an authorization decision. Bound work even while callers
/// keep inserting rows; a full final batch leaves completeness unknown and fails explicitly.
const REHASH_CLEANUP_BATCHES: u32 = 100;

/// One batch read: materialized dependencies require user → key → session locks. Metadata
/// appears once (kind 0); session rows (kind 1) contain no repeated PHC/key source. Result
/// order is unspecified; the next cursor is the maximum parsed UUID, not the last wire row.
const CLEANUP_BATCH: &str = "\
WITH locked_user AS MATERIALIZED (
    SELECT id, login, password, active /*token_cols_cte*/ FROM res_users
     WHERE id = $1::int8 FOR SHARE
), locked_key AS MATERIALIZED (
    SELECT p.value FROM ir_config_parameter p JOIN locked_user u ON true
     WHERE p.key = 'database.secret' LIMIT 2 FOR SHARE OF p
), key_meta AS MATERIALIZED (
    SELECT count(*)::int8 AS sources, min(value) AS value FROM locked_key
), locked_sessions AS MATERIALIZED (
    SELECT s.id, s.token, s.token_kid
      FROM erp_session s JOIN locked_user u ON s.user_id = u.id
      CROSS JOIN key_meta k
     WHERE k.sources = 1 AND ($2::text IS NULL OR s.id > $2::text::uuid)
     ORDER BY s.id LIMIT $3::int8 FOR UPDATE OF s
)
SELECT 0::int2 AS kind, u.login, u.password, u.active, k.sources, k.value,
       NULL::text AS session_id, NULL::bytea AS token, NULL::int2 AS token_kid
       /*token_cols_meta*/
  FROM locked_user u CROSS JOIN key_meta k
UNION ALL
SELECT 1::int2, NULL::text, NULL::text, NULL::bool, NULL::int8, NULL::text,
       s.id::text, s.token, s.token_kid /*token_cols_pad*/ FROM locked_sessions s";

/// The columns of [`CLEANUP_BATCH`] before the added token fields.
const CLEANUP_WIDTH: usize = 9;

/// [`CLEANUP_BATCH`] for `plan`: the constant itself for the base plan, else with the added
/// fields read under the same user lock (metadata row) and padded in the session rows.
fn cleanup_sql(plan: &TokenPlan) -> Result<std::borrow::Cow<'static, str>> {
    if plan.is_base() {
        return Ok(CLEANUP_BATCH.into());
    }
    let sql = replace_once(CLEANUP_BATCH, "/*token_cols_cte*/", &plan.select_list(""))?;
    let sql = replace_once(&sql, "/*token_cols_meta*/", &plan.select_list("u"))?;
    Ok(replace_once(&sql, "/*token_cols_pad*/", &plan.null_list())?.into())
}

struct CleanupBatch {
    last: Option<SessionId>,
    scanned: usize,
    ended: usize,
}

async fn end_obsolete_sessions(orm: &Orm, uid: i64, plan: &TokenPlan) -> Result<usize> {
    let uid = UserId::new(uid)?;
    let mut last = None;
    let mut total = 0_usize;
    for _ in 0..REHASH_CLEANUP_BATCHES {
        let mut tx = orm.begin_opts(read_committed()).await?;
        let batch = match obsolete_batch(&mut tx, uid, last, plan).await {
            Ok(batch) => {
                // Match cap cleanup: allocate the commit future only for this batch.
                Box::pin(tx.commit()).await?;
                batch
            }
            Err(error) => return Err(abandon(&mut tx, error).await),
        };
        total = total
            .checked_add(batch.ended)
            .ok_or_else(|| Error::programming("session cleanup deletion count overflowed"))?;
        if batch.scanned < store::GC_BATCH as usize {
            return Ok(total);
        }
        last = batch.last;
    }
    Err(Error::new(
        ErrorKind::ResourceExhausted,
        "session cleanup reached its 100000-row scan bound; cleanup is incomplete",
    ))
}

/// Lock order: user → trusted key source → sessions in UUID order. Password writers acquire
/// the user before ending sessions; parameter writers acquire no user/session locks. An absent
/// key source locks nothing and aborts. Both fields and keys are fresh in every batch.
async fn obsolete_batch(
    tx: &mut Transaction,
    uid: UserId,
    last: Option<SessionId>,
    plan: &TokenPlan,
) -> Result<CleanupBatch> {
    let after = last.map(|id| id.to_text());
    let sql = cleanup_sql(plan)?;
    let rows = tx
        .query_typed(
            &sql,
            &[
                (&uid.get(), Type::INT8),
                (&after, Type::TEXT),
                (&i64::from(store::GC_BATCH), Type::INT8),
            ],
        )
        .await?;
    let (metadata, scanned) = cleanup_metadata(&rows, CLEANUP_WIDTH + plan.len())?;
    let Some(metadata) = metadata else {
        // A deleted user has no sessions under the core table's ON DELETE CASCADE.
        return Ok(CleanupBatch {
            last,
            scanned: 0,
            ended: 0,
        });
    };
    let login: &str = col(metadata, 1)?;
    // TokenFields defines NULL password as the empty field; this is not a parse fallback.
    let password = Zeroizing::new(col::<Option<String>>(metadata, 2)?.unwrap_or_default());
    let active: bool = col(metadata, 3)?;
    let extra_values = plan.read_values(metadata, CLEANUP_WIDTH)?;
    let extra = plan.bind(&extra_values)?;
    match col::<i64>(metadata, 4)? {
        0 => return Err(super::secret::SecretError::Missing.into()),
        1 => {}
        2 => {
            return Err(Error::programming(
                "database.secret has more than one source row",
            ));
        }
        _ => {
            return Err(Error::db(
                "session cleanup returned an invalid key-source count",
            ));
        }
    }
    let secret_text = Zeroizing::new(
        col::<Option<String>>(metadata, 5)?.ok_or(super::secret::SecretError::Missing)?,
    );
    let secret = super::secret::DatabaseSecret::from_stored(&secret_text)?;
    let keys = super::secret::SessionKeys::derive(&secret)?;
    let fields = TokenFields {
        user: uid,
        login,
        password: &password,
        active,
        extra: &extra,
    };
    let mut obsolete = Vec::new();
    let mut last = last;
    // Validate the complete batch before invoking any DELETE or subscriber.
    for row in &rows {
        if col::<i16>(row, 0)? == 0 {
            continue;
        }
        let id = SessionId::parse(col::<&str>(row, 6)?)?;
        let stored: &[u8] = col(row, 7)?;
        if stored.len() != 32 {
            return Err(Error::db("erp_session.token must contain exactly 32 bytes"));
        }
        let kid: i16 = col(row, 8)?;
        let key = keys.key(kid).ok_or_else(|| {
            Error::validation(format!(
                "session cleanup has no trusted key for token_kid {kid}",
            ))
        })?;
        let expected = session::token(key.bytes(), &id, &fields)?;
        if !bool::from(expected.as_slice().ct_eq(stored)) {
            if obsolete.is_empty() {
                obsolete.reserve_exact(scanned);
            }
            obsolete.push(id);
        }
        last = Some(last.map_or(id, |previous| previous.max(id)));
    }
    let ended = store::end_sessions(tx, &obsolete, EndReason::CredentialChanged).await?;
    Ok(CleanupBatch {
        last,
        scanned,
        ended: ended.len(),
    })
}

/// Validate the tagged result shape before interpreting credentials or deleting anything.
/// The SQL bounds sessions independently of the single metadata row; no ordering is assumed.
fn cleanup_metadata(rows: &[Row], width: usize) -> Result<(Option<&Row>, usize)> {
    if rows.len() > store::GC_BATCH as usize + 1 {
        return Err(Error::db(
            "session cleanup returned more than one bounded batch",
        ));
    }
    let mut metadata = None;
    let mut scanned = 0;
    for row in rows {
        if row.len() != width {
            return Err(Error::db("session cleanup returned an invalid row width"));
        }
        match col::<i16>(row, 0)? {
            0 => {
                if metadata.replace(row).is_some() {
                    return Err(Error::db(
                        "session cleanup returned duplicate metadata rows",
                    ));
                }
                if col::<Option<&str>>(row, 6)?.is_some()
                    || col::<Option<&[u8]>>(row, 7)?.is_some()
                    || col::<Option<i16>>(row, 8)?.is_some()
                {
                    return Err(Error::db(
                        "session cleanup metadata contains session fields",
                    ));
                }
            }
            1 => {
                if col::<Option<&str>>(row, 1)?.is_some()
                    || col::<Option<&str>>(row, 2)?.is_some()
                    || col::<Option<bool>>(row, 3)?.is_some()
                    || col::<Option<i64>>(row, 4)?.is_some()
                    || col::<Option<&str>>(row, 5)?.is_some()
                {
                    return Err(Error::db(
                        "session cleanup session contains metadata fields",
                    ));
                }
                scanned += 1;
                if scanned > store::GC_BATCH as usize {
                    return Err(Error::db("session cleanup exceeded its session-row bound"));
                }
            }
            _ => return Err(Error::db("session cleanup returned an unknown row kind")),
        }
    }
    if metadata.is_none() && !rows.is_empty() {
        return Err(Error::db(
            "session cleanup returned sessions without metadata",
        ));
    }
    Ok((metadata, scanned))
}

/// Commit (a success: the override's writes stand) or roll back phase A's open transaction.
async fn end_phase_a(phase_a: PhaseA, commit: bool) -> Result<()> {
    let PhaseA::Open(mut tx) = phase_a else {
        return Ok(());
    };
    if commit {
        tx.commit().await
    } else {
        tx.rollback().await
    }
}

/// Dispatch `res.users._check_credentials` for `uid` as that user in superuser mode.
async fn check_credentials(
    tx: &mut Transaction,
    uid: UserId,
    credential: Arc<Credential>,
    env: CheckEnv,
) -> Result<AuthInfo> {
    let users = tx
        .env()
        .records("res.users")?
        .browse([uid.get()])?
        .with_user(uid.get())
        .sudo(true);
    users
        .call::<(Arc<Credential>, CheckEnv), AuthInfo>(tx, "_check_credentials", (credential, env))
        .await
}

/// A read-only view of the current database settings.
#[derive(Debug, Clone)]
pub struct SettingsView(Arc<Cached>);

impl SettingsView {
    /// The settings.
    pub fn settings(&self) -> &AuthSettings {
        &self.0.settings
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, reason = "tests")]
mod tests {
    use super::super::warn::capture;
    use super::*;

    /// The log tag of a login is a prefix of its keyed throttle key, never the text (round 2 S4:
    /// the key is an HMAC under the database's secret).
    #[test]
    fn login_tags_hide_the_login() {
        let secret =
            crate::auth::secret::DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef")
                .unwrap();
        let hasher = ThrottleHasher::derive(&secret).unwrap();
        let keys = Keys::new(
            &hasher,
            ClientAddr::InProcess("test"),
            "hunter2-typed-as-login",
        );
        let tag = keys.tag();
        assert_eq!(tag.len(), 8);
        assert!(!tag.contains("hunter"));
        assert_eq!(
            tag,
            session::hex(&hasher.key_hash(ThrottleKey::Login("hunter2-typed-as-login"))[..4])
        );
        assert_eq!(keys.address_text(), "test");
    }

    /// The service's futures are `Send`, so a server can `tokio::spawn` them (a compile-time
    /// check; an async closure in the retry helper once made them `Send` for one lifetime only).
    #[allow(dead_code, reason = "compile-time check")]
    fn the_service_futures_are_send(
        auth: &Auth,
        session: &CheckedSession,
        secret: &SessionSecret,
        touches: &[Touch],
        id: SessionId,
    ) {
        fn send<T: Send>(_: T) {}
        send(auth.authenticate(LoginRequest::new("a", "b", ClientAddr::InProcess("t"))));
        send(auth.begin_session(secret, CredentialOptions::new()));
        send(auth.rotate(session, secret));
        send(auth.touch(touches));
        send(auth.logout(id));
        send(auth.settings());
    }

    /// Security review round 2 N1 and delta N-c: `Debug` of a login request prints neither the
    /// login nor its length.
    #[test]
    fn a_login_request_debug_hides_the_login() {
        let request = LoginRequest::new("hunter2", "pw", ClientAddr::InProcess("test"));
        let text = format!("{request:?}");
        assert_eq!(
            text,
            r#"LoginRequest { login: "<redacted>", password: "<redacted>", client: InProcess("test"), interactive: true, .. }"#
        );
    }

    /// Security review round 2 N6: the admission leaves the primary pool a connection, and half
    /// of it when an override holds phase A through the hash.
    #[test]
    fn the_admission_leaves_connections_for_checks() {
        assert_eq!(admission_size(2, 10, false), 4);
        assert_eq!(admission_size(8, 10, false), 5);
        assert_eq!(admission_size(8, 10, true), 5);
        assert_eq!(admission_size(1, 2, true), 1);
    }

    /// Type design T8: `LoginError::Failed` is transparent: its text is the cause's and its
    /// source is the cause's source, so a chain reporter prints the cause once.
    #[test]
    fn a_failed_login_is_transparent() {
        let cause = Error::db("boom").with_source(std::fmt::Error);
        let e = LoginError::Failed(cause);
        assert_eq!(e.to_string(), "boom");
        let source = std::error::Error::source(&e).unwrap();
        assert_eq!(source.to_string(), std::fmt::Error.to_string());
        assert!(std::error::Error::source(&LoginError::Denied).is_none());
    }

    /// Silent-delta N-1 and types-delta N-e: a panic of the verdict task reaches the caller with
    /// its payload text and is counted once; the watch hands it over instead of logging it while
    /// the caller is there.
    #[tokio::test]
    async fn a_panicking_verdict_keeps_its_payload() {
        let counters = Arc::new(Counters::default());
        let task = tokio::spawn(async {
            let fail = true;
            if fail {
                panic!("boom {}", 7);
            }
            Err::<Login, LoginError>(LoginError::Denied)
        });
        let (reply, answer) = oneshot::channel();
        watch(task, Arc::clone(&counters), [0u8; 32], reply).await;
        let Err(LoginError::Failed(e)) = answer.await.unwrap() else {
            panic!("expected Failed");
        };
        assert_eq!(
            (e.kind(), e.message()),
            (
                ErrorKind::Programming,
                "the login's verdict task panicked: boom 7"
            )
        );
        assert_eq!(counters.verdict_failures.load(Ordering::Relaxed), 1);
        // a caller that is gone: still counted (and logged by the watch)
        let task =
            tokio::spawn(async { Err::<Login, LoginError>(LoginError::Failed(Error::db("x"))) });
        let (reply, answer) = oneshot::channel();
        drop(answer);
        watch(task, Arc::clone(&counters), [0u8; 32], reply).await;
        assert_eq!(counters.verdict_failures.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn panic_payloads_as_text() {
        let literal: Box<dyn std::any::Any + Send> = Box::new("lit");
        let formatted: Box<dyn std::any::Any + Send> = Box::new(String::from("fmt"));
        let other: Box<dyn std::any::Any + Send> = Box::new(3_u8);
        assert_eq!(panic_text(literal.as_ref()), "lit");
        assert_eq!(panic_text(formatted.as_ref()), "fmt");
        assert_eq!(panic_text(other.as_ref()), "(a payload that is not text)");
    }

    /// Type design T8: the login admission's expired wait is `Overloaded` with the wait as its
    /// `Retry-After`, not a hashing-pool error inside `Failed`.
    #[tokio::test]
    async fn an_expired_admission_wait_is_overloaded() {
        let none = Arc::new(Semaphore::new(0));
        let counters = Counters::default();
        let refused = admit(&none, Duration::from_millis(20), &counters)
            .await
            .unwrap_err();
        // silent-failure delta N-6: the expired wait is counted
        assert_eq!(counters.admission_overloaded.load(Ordering::Relaxed), 1);
        let LoginError::Overloaded { retry_after } = refused else {
            panic!("expected Overloaded, got {refused:?}");
        };
        assert_eq!(retry_after, Duration::from_millis(20));
        let e = LoginError::Overloaded { retry_after };
        assert_eq!(e.to_string(), "Server busy; retry after 1 second.");
        // types-delta N-d: the plural from 2 on
        let five = LoginError::Overloaded {
            retry_after: Duration::from_secs(5),
        };
        assert_eq!(five.to_string(), "Server busy; retry after 5 seconds.");
        let e = Error::from(e);
        assert_eq!(
            (e.kind(), e.message(), e.condition()),
            (
                ErrorKind::ResourceExhausted,
                "Server busy; retry after 1 second.",
                Some(Condition::Overloaded)
            )
        );
    }

    /// The `warn` line of a settings expiry with a 1 s queue wait, `what` the caller was doing.
    fn expiry_line(what: &str) -> String {
        format!(
            "a caller gave up on the authentication settings after the 1 s queue wait \
             (admission.hash_queue_wait_secs), {what}; 1 so far of this kind (this line comes at \
             most once a second per kind)"
        )
    }

    /// The `Overloaded` error of a settings expiry with a 1 s queue wait.
    fn assert_settings_overloaded(e: &Error) {
        assert_eq!(
            (e.kind(), e.message(), e.condition()),
            (
                ErrorKind::ResourceExhausted,
                "Server busy; the authentication settings could not be read within the 1 s queue \
                 wait (admission.hash_queue_wait_secs); retry.",
                Some(Condition::Overloaded)
            )
        );
    }

    /// A limit of `condition` as the server reports it (the text is ours: only the condition
    /// decides).
    fn limit_error(condition: Condition) -> Error {
        Error::db(format!("the server ended the statement ({condition:?})"))
            .with_condition(condition)
    }

    /// Round 4 C4-5: the two settings expiries have a `warn` limit each, so in one second a line
    /// of one kind does not hide the other's; each line carries its own kind's count. Two of each
    /// in one second: one line of each, counts 2 and 2.
    #[test]
    fn the_two_settings_expiries_have_their_own_warning() {
        let started = std::time::Instant::now();
        let (lines, _guard) = capture::start();
        let expiries = SettingsExpiries::default();
        let wait = Duration::from_secs(1);
        let shown = limit_error(Condition::LockTimeout).to_string();
        for _ in 0..2 {
            assert_settings_overloaded(&expiries.expired(wait, SettingsExpiry::Waiting));
            let cause = ReloadCause::of_limit(limit_error(Condition::LockTimeout)).unwrap();
            assert_settings_overloaded(&expiries.expired(wait, SettingsExpiry::Reload(cause)));
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the host stalled for a second; the case needs the four expiries in one second"
        );
        assert_eq!(
            lines.take(),
            [
                expiry_line("waiting for another's reload"),
                expiry_line(&format!(
                    "running the reload: its lock_timeout fired ({shown})"
                )),
            ]
        );
        assert_eq!(
            (
                expiries.wait_expired.load(Ordering::Relaxed),
                expiries.load_expired.load(Ordering::Relaxed)
            ),
            (2, 2)
        );
    }

    /// Round 4 C4-6: a reload ended at its queue wait keeps what ended it, as the error's source
    /// and in the `warn` line: the lock, statement or backstop limit (the server's error), or the
    /// caller's deadline (the flight dropped). Any other error of the flight is not a limit and
    /// goes back to the caller as it is.
    #[tokio::test]
    async fn a_reload_expiry_keeps_its_cause() {
        let wait = Duration::from_secs(1);
        for (condition, what) in [
            (Condition::LockTimeout, "its lock_timeout fired"),
            (Condition::StatementTimeout, "its statement_timeout fired"),
            (
                Condition::TransactionTimeout,
                "its transaction_timeout backstop fired",
            ),
        ] {
            let (lines, _guard) = capture::start();
            let expiries = SettingsExpiries::default();
            let limit = limit_error(condition);
            let shown = limit.to_string();
            let cause = ReloadCause::of_limit(limit).unwrap();
            let e = expiries.expired(wait, SettingsExpiry::Reload(cause));
            assert_settings_overloaded(&e);
            let source = std::error::Error::source(&e)
                .and_then(|s| s.downcast_ref::<Error>())
                .unwrap_or_else(|| panic!("no limit error as the source: {e:?}"));
            assert_eq!(
                (source.condition(), source.to_string()),
                (Some(condition), shown.clone())
            );
            assert_eq!(
                lines.take(),
                [expiry_line(&format!(
                    "running the reload: {what} ({shown})"
                ))]
            );
        }
        let (lines, _guard) = capture::start();
        let expiries = SettingsExpiries::default();
        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        let e = expiries.expired(wait, SettingsExpiry::Reload(ReloadCause::Deadline(elapsed)));
        assert_settings_overloaded(&e);
        assert!(
            std::error::Error::source(&e).is_some_and(|s| s.is::<tokio::time::error::Elapsed>()),
            "the deadline as the source: {e:?}"
        );
        assert_eq!(
            lines.take(),
            [expiry_line(
                "running the reload: the queue-wait deadline passed first and the flight was \
                 dropped"
            )]
        );
        let other = ReloadCause::of_limit(Error::db("connection reset")).unwrap_err();
        assert_eq!(
            (other.kind(), other.message(), other.condition()),
            (ErrorKind::Database, "connection reset", None)
        );
    }

    /// Security review round 3 R3-c: the reload's limits stay within the queue wait.
    #[test]
    fn the_reload_limits_are_the_queue_wait() {
        let limits = reload_limits(Duration::from_secs(5)).unwrap();
        assert_eq!(
            (limits.lock(), limits.statement(), limits.transaction()),
            (
                Duration::from_millis(4_999),
                Duration::from_secs(5),
                Duration::from_secs(6)
            )
        );
    }

    /// Security review Low 6: the pool refusing a hash is recognised through the error chain.
    #[test]
    fn pool_refusals_are_recognised() {
        let overloaded = Error::from(HashPoolError::Overloaded {
            retry_after_secs: 1,
        });
        assert!(pool_refusal(&overloaded).is_some());
        let memory = Error::from(HashPoolError::MemoryOverReservation {
            needed_kib: 2,
            reserved_kib: 1,
        });
        assert!(pool_refusal(&memory).is_some());
        assert!(pool_refusal(&Error::from(HashPoolError::Panicked)).is_none());
        assert!(pool_refusal(&Error::db("x")).is_none());
    }
}

#[cfg(test)]
mod cleanup_layout_tests {
    use std::future::Future;
    use std::mem::size_of;

    fn one<A, F: Future>(_: impl FnOnce(A) -> F) -> usize {
        size_of::<F>()
    }

    fn four<A, B, C, D, F: Future>(_: impl FnOnce(A, B, C, D) -> F) -> usize {
        size_of::<F>()
    }

    fn two<A, B, F: Future>(_: impl FnOnce(A, B) -> F) -> usize {
        size_of::<F>()
    }

    fn three<A, B, C, F: Future>(_: impl FnOnce(A, B, C) -> F) -> usize {
        size_of::<F>()
    }

    fn five<A, B, C, D, E, F: Future>(_: impl FnOnce(A, B, C, D, E) -> F) -> usize {
        size_of::<F>()
    }

    fn six<A, B, C, D, E, G, F: Future>(_: impl FnOnce(A, B, C, D, E, G) -> F) -> usize {
        size_of::<F>()
    }

    /// Compile-time future sizes: constructors are never called, no fake references or ORM
    /// fixture is needed, and no production path is changed to take this measurement.
    #[test]
    #[allow(
        clippy::print_stdout,
        reason = "explicit future-layout attribution probe"
    )]
    #[ignore = "DEFERRED_PERFORMANCE: milestone-end future layout measurement"]
    fn cleanup_future_layout() {
        println!("end_others_once={}", six(super::end_others_once));
        println!("end_over_cap_once={}", five(super::end_over_cap_once));
        println!(
            "end_obsolete_sessions={}",
            three(super::end_obsolete_sessions)
        );
        println!("obsolete_batch={}", four(super::obsolete_batch));
        println!("end_sessions={}", three(super::store::end_sessions));
        println!("transaction={}", size_of::<super::Transaction>());
        println!("begin_opts={}", two(super::Orm::begin_opts));
        println!("commit={}", one(super::Transaction::commit));
        println!("rollback={}", one(super::Transaction::rollback));
        println!(
            "end_other_sessions={}",
            four(super::Verdict::end_other_sessions)
        );
        println!("verdict_run={}", two(super::Verdict::run));
        println!("succeed={}", three(super::Verdict::succeed));
    }
}

impl Auth {
    /// Authenticate a native API key and optional cookie in the primary opening flight.
    ///
    /// Current subject/groups/companies, expiry and exact route scope are read anew on every
    /// invocation. Refusals roll back before the invalid-attempt counter is committed. Invalid
    /// cookies are only reported for clearing: this path never deletes or touches sessions.
    /// Once a counted refusal is decided, its private ticket persists the counter even when
    /// cancellation occurs during rollback. A lineage admission lease bounds that detached work.
    ///
    /// # Errors
    /// `AccessDenied` for invalid native or imported legacy keys, unsupported kinds,
    /// and address cooldown (typed [`super::ApiKeyCooldown`] source); `Access` for a live cookie
    /// of another subject. Database, settings and decoding errors propagate. Expected refusals
    /// carry [`super::ApiKeyRefusal`] cookie-clearing evidence. Admission expiry is
    /// `ResourceExhausted` with `Condition::Overloaded`. No submitted text is reflected.
    pub async fn begin_api_key(
        &self,
        request: super::ApiKeyRequest<'_>,
        options: CredentialOptions,
    ) -> Result<super::ApiKeyBegin> {
        use super::apikey_verify::{ApiKeyCooldown, ApiKeyRefusal, KeyCheck, KeyRefused};
        // Reuse the lineage's bounded login admission. On a counted verdict ownership moves
        // into its failure ticket and stays there until the separate write has ended.
        let admission = Mutex::new(Some(
            admit(
                &self.admission,
                self.pool.config().queue_wait(),
                &self.counters,
            )
            .await
            .map_err(Error::from)?,
        ));
        let address = throttle::address_key(request.address);
        // Reused by a retry under the same settings generation, so it does not repeat the hashing
        // work; a new generation may change the cooldown screen, so it resolves again. The
        // opening statement re-reads the row on every attempt.
        let mut resolved: Option<(u64, LegacyOutcome)> = None;
        for _ in 0..2 {
            let cached = self.settings_current().await?;
            let address_hash = cached.throttle.key_hash(ThrottleKey::Address(&address));
            let parsed = super::apikey::NativeApiKey::parse(request.raw);
            let outcome =
                if request.raw.len() == 40 && request.raw.bytes().all(|c| c.is_ascii_hexdigit()) {
                    match &resolved {
                        Some((generation, outcome)) if *generation == cached.generation => {
                            outcome.clone()
                        }
                        _ => {
                            let outcome = self
                                .resolve_legacy_key(&request, &cached, &address_hash)
                                .await?;
                            resolved = Some((cached.generation, outcome.clone()));
                            outcome
                        }
                    }
                } else {
                    Ok(None)
                };
            // a loud refusal of the stored rows is counted as a miss first (review S3)
            let (legacy, loud) = match outcome {
                Ok(found) => (found, None),
                Err(loud) => (None, Some(loud)),
            };
            let (digest, kid, malformed) = match parsed {
                Ok(token) => {
                    let digest = cached.apikey.digest(&token);
                    (session::hex(digest.bytes()), digest.kid(), false)
                }
                Err(error)
                    if error.kind() == ErrorKind::AccessDenied
                        && error.message() == "Invalid apikey" =>
                {
                    // an imported key verified above is not a malformed native key
                    (String::new(), 1, legacy.is_none())
                }
                Err(error) => return Err(error),
            };
            // This guard precedes and is borrowed by the opening-flight future. Cancellation
            // drops that future (and its Transaction) before readiness can be sent by Drop.
            let settlement = ApiKeySettlementRelease::new();
            let check = KeyCheck {
                orm: &self.orm,
                auth: self,
                admission: &admission,
                settlement: &settlement,
                sql: &self.api_key_sql,
                session_sql: &self.check_sql,
                digest,
                kid,
                malformed,
                legacy,
                request: &request,
                cached,
                config: &self.config,
                address_hash,
                #[cfg(feature = "test-support")]
                verdict_gate: &self.api_key_verdict_gate,
            };
            let begun = self.orm.begin_credential(options.clone(), &check).await;
            // On refusal/error the ORM-owned Transaction is gone at this return boundary.
            // The detached writer may now acquire a connection. Success installed no sender.
            settlement.release()?;
            match begun? {
                CredentialBegin::Open { tx, value } => {
                    // A failure here drops `tx` (rolled back) before any ticket exists.
                    let last_use = if value.last_use_due {
                        Some(self.api_key_last_use(value.key)?)
                    } else {
                        None
                    };
                    return Ok(super::ApiKeyBegin {
                        tx,
                        drop_cookie: value.drop_cookie,
                        last_use,
                    });
                }
                CredentialBegin::Refused(KeyRefused::Stale) => self.cache_drop(),
                CredentialBegin::Refused(KeyRefused::Cooldown {
                    retry_after,
                    drop_cookie,
                }) => return Err(ApiKeyCooldown::error(retry_after, drop_cookie)),
                CredentialBegin::Refused(KeyRefused::Denied {
                    error,
                    drop_cookie,
                    failure,
                }) => {
                    if let Some(failure) = failure {
                        failure.settle().await?;
                    }
                    if let Some(loud) = loud {
                        return Err(loud.error());
                    }
                    return Err(ApiKeyRefusal::attach(error, drop_cookie));
                }
            }
        }
        Err(Error::new(
            ErrorKind::Concurrency,
            "the authentication keys changed twice during one API key check; retry the request",
        ))
    }

    /// The imported row whose stored PBKDF2 hash accepts this 40-hex key, if any. The answer
    /// is evidence only: the opening statement re-reads the row, user, audience and expiry.
    async fn resolve_legacy_key(
        &self,
        request: &super::ApiKeyRequest<'_>,
        cached: &session::Cached,
        address_hash: &[u8; 32],
    ) -> Result<LegacyOutcome> {
        use super::apikey_legacy::{CANDIDATES_SQL, verify_candidates};
        let generation = cached.generation;
        let index = request.raw[..8].to_owned();
        let audience = match request.scope {
            super::apikey_store::KeyScope::Rpc => "rpc",
            super::apikey_store::KeyScope::Mcp => "mcp",
        }
        .to_owned();
        let address_hex = session::hex(address_hash);
        let cooldown = cached.settings.address_cooldown();
        let after = i64::from(cooldown.after());
        let window = cooldown.duration().as_secs_f64();
        let params: [(&(dyn ToSql + Sync), Type); 5] = [
            (&index, Type::TEXT),
            (&audience, Type::TEXT),
            (&address_hex, Type::TEXT),
            (&after, Type::INT8),
            (&window, Type::FLOAT8),
        ];
        let rows = one_flight(&self.orm, read_committed(), CANDIDATES_SQL, &params).await?;
        let mut candidates = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.try_get(0).map_err(|e| {
                Error::db(format!("could not decode legacy API key id: {e}")).with_source(e)
            })?;
            let hash: String = row.try_get(1).map_err(|e| {
                Error::db(format!("could not decode legacy API key hash: {e}")).with_source(e)
            })?;
            candidates.push((id, Arc::<str>::from(hash)));
        }
        if let Some(found) = self.legacy_memo.get(request.raw, generation, &candidates)? {
            return Ok(Ok(Some(found)));
        }
        let outcome = verify_candidates(&self.registration, request.raw, candidates).await?;
        if let Ok(Some(found)) = &outcome {
            self.legacy_memo
                .put(request.raw, generation, found.clone())?;
        }
        Ok(outcome)
    }

    fn api_key_last_use(&self, key: ApiKeyId) -> Result<ApiKeyLastUse> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            Error::programming("API key verification requires an active Tokio runtime")
                .with_source(error)
        })?;
        Ok(ApiKeyLastUse {
            key,
            work: Some(LastUseWork {
                orm: self.orm.clone(),
                counters: Arc::clone(&self.counters),
                registration: self.registration.clone(),
                runtime,
            }),
        })
    }

    pub(super) fn api_key_failure_ticket(
        &self,
        address: [u8; 32],
        admission: &ApiKeyAdmission,
        settlement: &ApiKeySettlementRelease,
    ) -> Result<ApiKeyFailureTicket> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            Error::programming("API key verification requires an active Tokio runtime")
                .with_source(error)
        })?;
        let admission = admission
            .lock()
            .map_err(|_| Error::programming("API key admission ownership was poisoned"))?
            .take()
            .ok_or_else(|| {
                Error::programming("API key counted verdict already transferred its admission")
            })?;
        Ok(ApiKeyFailureTicket {
            work: Some(ApiKeyFailureWork {
                orm: self.orm.clone(),
                counters: Arc::clone(&self.counters),
                registration: self.registration.clone(),
                address,
                admission,
                ready: settlement.receiver()?,
                runtime,
            }),
        })
    }
}

#[cfg(feature = "test-support")]
impl Auth {
    pub(super) fn freeze_prefilter(&self, now: std::time::Instant) -> Result<()> {
        self.prefilter.freeze_at(now)
    }

    pub(super) fn api_key_verdict_gate(
        &self,
        reached: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Result<()> {
        let mut gate = self
            .api_key_verdict_gate
            .lock()
            .map_err(|_| Error::programming("API key verdict test gate was poisoned"))?;
        if gate.is_some() {
            return Err(Error::programming(
                "API key verdict test gate was already installed",
            ));
        }
        *gate = Some(super::apikey_verify::ApiKeyVerdictGate { reached, release });
        Ok(())
    }
}

/// The right to record one use of an API key, handed out by [`Auth::begin_api_key`] only when
/// the key's `last_used_at` is due (never set, or older than one hour by the database clock).
///
/// **Dropping the ticket launches the write**; there is nothing to await and no other way to fire
/// it. Hold it until the request transaction has ended, then drop it. The write is bookkeeping:
/// one READ COMMITTED statement on the primary, outside the request transaction, repeating the
/// due test and skipping a locked row, so the key is written at most about once an hour across
/// the whole cluster. It runs on the runtime that verified the key, so dropping the ticket in a
/// cancelled request still records the use; a ticket dropped while that runtime is shutting
/// down may record nothing, and that loss is neither counted nor logged. Only one such write
/// runs at a time per process and ORM lineage: a ticket dropped while another write is running
/// is skipped and counted ([`AuthStats::last_use_skipped`]), and the key's next use is due
/// again. A failed write is logged at `warn` (key id only) and counted
/// ([`AuthStats::last_use_errors`]); the request is never affected.
///
/// Dropping the ticket before the request transaction has ended is safe, with one exception: the
/// request transaction is REPEATABLE READ, so any such transaction that takes its snapshot before
/// a last-use write of this key commits, and then updates, deletes or locks (`FOR UPDATE`) that
/// key row, gets a serialization failure (`40001`). That is the same request when it dropped the
/// ticket early (a key revoking itself), and also another request that revokes or changes this
/// key while a write from a different request commits in between. Holding the ticket until after
/// commit protects only the request that owns it; the RPC layer retries a `40001` as a whole
/// attempt (`crates/erp-rpc/src/service.rs`, `classify`), so the cost is one more attempt.
pub struct ApiKeyLastUse {
    key: ApiKeyId,
    work: Option<LastUseWork>,
}

impl fmt::Debug for ApiKeyLastUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyLastUse")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

struct LastUseWork {
    orm: Orm,
    counters: Arc<Counters>,
    registration: pool::PoolBinding,
    runtime: tokio::runtime::Handle,
}

impl ApiKeyLastUse {
    /// Throw this ticket away without writing: nothing is launched and no counter moves.
    ///
    /// For a caller that already holds a ticket for the same key in the same request (for
    /// example a preflight check followed by the request's own opening statement): keep the first
    /// ticket and discard the others, so the write runs once, when the kept ticket is dropped.
    pub fn discard(mut self) {
        self.work = None;
    }
}

impl Drop for ApiKeyLastUse {
    fn drop(&mut self) {
        let Some(work) = self.work.take() else {
            return;
        };
        let key = self.key;
        // No queueing: a busy gate means another write is on its way, and this key is due again
        // on its next use.
        let permit = match work.registration.last_use_gate().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                bump(&work.counters.last_use_skipped);
                return;
            }
            Err(TryAcquireError::Closed) => {
                tracing::warn!(target: "erp::auth", "could not record the last use of API key {}: the last-use gate is closed", key.get());
                bump(&work.counters.last_use_errors);
                return;
            }
        };
        // A dropped JoinHandle detaches rather than cancels; the captured runtime lets a
        // cancelled request drop this ticket outside any runtime context.
        drop(work.runtime.clone().spawn(async move {
            let id = key.get();
            let secs = super::apikey_verify::LAST_USED_INTERVAL_SECS;
            let params: [(&(dyn ToSql + Sync), Type); 2] =
                [(&id, Type::INT8), (&secs, Type::INT8)];
            match one_flight(
                &work.orm,
                read_committed(),
                super::apikey_verify::LAST_USE_SQL,
                &params,
            )
            .await
            {
                Ok(rows) if rows.is_empty() => bump(&work.counters.last_use_noops),
                Ok(_) => bump(&work.counters.last_use_writes),
                Err(error) => {
                    tracing::warn!(target: "erp::auth", "could not record the last use of API key {id}: {error}");
                    bump(&work.counters.last_use_errors);
                }
            }
            // keep the lineage binding alive until the write has ended
            drop(permit);
            drop(work.registration);
        }));
    }
}

/// Request-local permit ownership; only a counted verdict transfers it into a ticket.
pub(super) type ApiKeyAdmission = Mutex<Option<OwnedSemaphorePermit>>;

/// A known failed attempt which must survive cancellation of the request's rollback.
/// Neither the ticket nor its task owns submitted credentials or a request transaction.
pub(super) struct ApiKeyFailureTicket {
    work: Option<ApiKeyFailureWork>,
}

impl ApiKeyFailureTicket {
    async fn settle(mut self) -> Result<()> {
        let work = self
            .work
            .take()
            .ok_or_else(|| Error::programming("API key failure ticket was already settled"))?;
        work.spawn().await.map_err(|error| {
            Error::programming(format!(
                "the API key failure counter task ended early: {error}"
            ))
        })?
    }
}

impl Drop for ApiKeyFailureTicket {
    fn drop(&mut self) {
        if let Some(work) = self.work.take() {
            // A dropped JoinHandle detaches rather than cancels the task. The captured runtime
            // lets cancellation drop this ticket without requiring a runtime-local lookup.
            drop(work.spawn());
        }
    }
}

struct ApiKeyFailureWork {
    orm: Orm,
    counters: Arc<Counters>,
    registration: pool::PoolBinding,
    address: [u8; 32],
    admission: OwnedSemaphorePermit,
    ready: oneshot::Receiver<()>,
    runtime: tokio::runtime::Handle,
}

impl ApiKeyFailureWork {
    fn spawn(self) -> tokio::task::JoinHandle<Result<()>> {
        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            // Keep the weak registry's owning binding in the actual worker, not merely the
            // waiting Auth/ticket. Reconstructed Auth must reuse this same admission gate.
            let registration = self.registration;
            let admission = self.admission;
            let params: [(&(dyn ToSql + Sync), Type); 1] = [(&self.address.as_slice(), Type::BYTEA)];
            // Even a ticket dropped before ORM rollback finishes cannot acquire a pool
            // connection until the outer request guard has observed/dropped that flight.
            let written = match self.ready.await {
                Ok(()) => retry(
                    DEADLOCK,
                    &self.counters.retries.failure_write,
                    "the failure count of an API key",
                    || one_flight(&self.orm, read_committed(), super::apikey_verify::FAILURE_SQL, &params),
                ).await,
                Err(error) => Err(Error::programming("API key request did not release failure settlement").with_source(error)),
            };
            if let Err(error) = &written {
                bump(&self.counters.failure_write_errors);
                tracing::error!(target:"erp::auth", "could not persist an API key address failure: {error}");
            }
            // The same lineage admission bounds both live credential checks and detached writes.
            drop(admission);
            drop(registration);
            written.map(|_| ())
        })
    }
}

/// A request-local release signal with no channel allocation on accepted/noncounted paths.
/// The sender is installed only when a counted verdict transfers its admission into a ticket.
pub(super) struct ApiKeySettlementRelease {
    ready: Mutex<Option<oneshot::Sender<()>>>,
}

impl ApiKeySettlementRelease {
    fn new() -> Self {
        Self {
            ready: Mutex::new(None),
        }
    }

    fn receiver(&self) -> Result<oneshot::Receiver<()>> {
        let mut ready = self
            .ready
            .lock()
            .map_err(|_| Error::programming("API key settlement release was poisoned"))?;
        if ready.is_some() {
            return Err(Error::programming(
                "API key settlement release was already installed",
            ));
        }
        let (send, receive) = oneshot::channel();
        *ready = Some(send);
        Ok(receive)
    }

    fn release(&self) -> Result<()> {
        let sender = self
            .ready
            .lock()
            .map_err(|_| Error::programming("API key settlement release was poisoned"))?
            .take();
        if let Some(sender) = sender {
            sender.send(()).map_err(|_| {
                Error::programming("API key failure settlement receiver disconnected")
            })?;
        }
        Ok(())
    }
}

impl Drop for ApiKeySettlementRelease {
    fn drop(&mut self) {
        // Cancellation has no caller to receive an error. The writer also reports a closed
        // readiness channel and releases its admission, rather than proceeding without proof.
        if let Err(error) = self.release() {
            tracing::error!(target:"erp::auth", "could not release API key failure settlement: {error}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod api_key_settlement_release_tests {
    use super::ApiKeySettlementRelease;
    use tokio::sync::oneshot::error::TryRecvError;

    #[tokio::test]
    async fn counted_work_cannot_be_ready_before_request_release() {
        let release = ApiKeySettlementRelease::new();
        let mut ready = release.receiver().unwrap();
        assert_eq!(ready.try_recv(), Err(TryRecvError::Empty));
        release.release().unwrap();
        ready.await.unwrap();
        // The normal release and later request Drop must be idempotent.
        release.release().unwrap();
    }

    #[tokio::test]
    async fn cancelled_request_drop_releases_its_counted_work() {
        let release = ApiKeySettlementRelease::new();
        let mut ready = release.receiver().unwrap();
        assert_eq!(ready.try_recv(), Err(TryRecvError::Empty));
        drop(release);
        ready.await.unwrap();
    }
}

/// LD3: every selection-owned copy of a stored hash or canonical login is wiped on drop. The
/// assertions are on ownership types (a `Zeroizing` value wipes on drop); the Postgres driver's
/// own receive buffers are outside this guarantee and stay documented as such.
#[cfg(test)]
mod sensitive_copy_ownership_tests {
    use super::{CandidateRecord, LoginUser};
    use zeroize::{ZeroizeOnDrop, Zeroizing};

    fn wiped_on_drop<T: ZeroizeOnDrop>(_: &T) {}

    #[test]
    fn candidate_record_and_login_user_copies_are_wiped_on_drop() {
        fn record(candidate: CandidateRecord) {
            let (_uid, stored, _lang, _tz, canonical, _witness) = candidate;
            let stored: Option<Zeroizing<String>> = stored;
            let canonical: Zeroizing<String> = canonical;
            wiped_on_drop(&canonical);
            if let Some(stored) = &stored {
                wiped_on_drop(stored);
            }
        }
        fn user(user: &LoginUser) {
            let stored: &Option<Zeroizing<String>> = &user.stored;
            let canonical: &Option<Zeroizing<String>> = &user.canonical_login;
            if let (Some(stored), Some(canonical)) = (stored, canonical) {
                wiped_on_drop(stored);
                wiped_on_drop(canonical);
            }
        }
        let _ = (record, user);
    }

    /// The real `Drop` of a preloaded hash wipes it. Observed through the `wipe_probe` seam
    /// (`credential.rs`): the drop records the bytes it wiped and that the string was empty right
    /// after, without any read of freed memory. An empty or unread value wipes nothing, and a
    /// clone is a second copy with its own wipe.
    #[test]
    fn dropping_a_preloaded_hash_wipes_it_directly() {
        use crate::auth::credential::{StoredPassword, wipe_probe};
        const HASH: &str = "$argon2id$synthetic-test-value";
        let _ = wipe_probe::take();

        drop(StoredPassword::Unread);
        drop(StoredPassword::Preloaded(None));
        assert_eq!(wipe_probe::take(), Vec::<(usize, bool)>::new());

        let stored = StoredPassword::Preloaded(Some(HASH.to_owned()));
        assert!(
            wipe_probe::take().is_empty(),
            "constructing must not wipe anything"
        );
        let copy = stored.clone();
        drop(stored);
        assert_eq!(wipe_probe::take(), vec![(HASH.len(), true)]);
        drop(copy);
        assert_eq!(wipe_probe::take(), vec![(HASH.len(), true)]);

        let holder = vec![StoredPassword::Preloaded(Some(HASH.to_owned()))];
        assert!(wipe_probe::take().is_empty(), "moving must not wipe");
        drop(holder);
        assert_eq!(wipe_probe::take(), vec![(HASH.len(), true)]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod token_plan_statement_tests {
    use super::{CLEANUP_BATCH, LOGIN_SQL_LANGUAGES, PlanSql, SUCCESS_REHASH_SQL, SUCCESS_SQL};
    use super::{TokenPlan, cleanup_sql};

    /// Every statement the plan rewrites carries each of its markers exactly once, so a real
    /// plan can always be applied (the build fails loudly otherwise).
    #[test]
    fn a_non_base_plan_rewrites_every_statement() {
        let plan = TokenPlan::text_fields_for_tests(&["tok_a", "tok_n"]);
        for has_languages in [true, false] {
            let sql = PlanSql::new(&plan, has_languages).unwrap();
            assert!(sql.login.contains(", u.\"tok_a\"::text, u.\"tok_n\"::text"));
            assert!(!sql.login.contains("/*token_cols*/"));
            for text in [&sql.success, &sql.success_rehash] {
                assert!(text.contains("IS NOT DISTINCT FROM $22::text[]"));
                assert!(!text.contains("/*token_guard*/"));
            }
        }
        let cleanup = cleanup_sql(&plan).unwrap();
        assert!(!cleanup.contains("/*token_cols"));
        assert_eq!(cleanup.matches("tok_a").count(), 2);
        assert!(cleanup.contains("s.token_kid , NULL::text, NULL::text FROM locked_sessions"));
        // the base plan is the constants themselves
        let base = PlanSql::new(&TokenPlan::base(), true).unwrap();
        assert_eq!(&*base.login, LOGIN_SQL_LANGUAGES);
        assert_eq!(&*base.success, SUCCESS_SQL);
        assert_eq!(&*base.success_rehash, SUCCESS_REHASH_SQL);
        assert_eq!(&*cleanup_sql(&TokenPlan::base()).unwrap(), CLEANUP_BATCH);
    }
}
