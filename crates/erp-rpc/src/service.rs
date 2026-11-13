//! [`RpcService`]: one call, one unit of work (design note §4): route, exposure, gates, binding,
//! a transaction of the right kind, the call, the result, the commit, retries and the error body.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::audit::{
    AttemptEvidence, AuditAttempt, AuditCall, AuditError, AuditOrigin, AuditOutcome,
    AuditStoreState, ErrorClass, TargetIds,
};
use erp_orm::rpc::{Effect, Exposed, Exposure, Receiver};
use erp_orm::{
    ApiKeyId, BeginOptions, Capability, CommitError, CompanyId, FinalStatement, KeyKind,
    OperationIdentity, Orm, Principal, PrincipalError, PrincipalKind, ReadAuditOwner, SignalKey,
    Transaction,
};
use tokio_postgres::types::{ToSql, Type};

use crate::binding::{self, Bound, CarriedContext};
use crate::config::RpcConfig;
use crate::error::{DebugInfo, IdempotencyRefusal, InfraError, ProtocolError, RpcFailure};
use crate::hook::{AttemptHook, HookCall, HookRefusal, RefusalKind};
use crate::idempotency::{self, Claim, CorrelationId, Existing, IdempotencyKey};
use crate::json::{self, Json};
use crate::output::{self, WriteError};
use crate::pending::PendingMethods;
use crate::route::{Route, py_repr};
use erp_orm::trace::RequestTrace;

/// Attempts of one call.
pub const MAX_ATTEMPTS: u32 = 5;
/// Base of the full-jitter backoff between attempts.
pub const RETRY_BASE: Duration = Duration::from_millis(50);
/// Cap of the backoff.
pub const RETRY_CAP: Duration = Duration::from_secs(2);
/// `Retry-After` of a 409 for a key whose first request is still running.
pub const IN_PROGRESS_RETRY_AFTER: u32 = 1;

const UNSAFE_NAMES: [&str; 20] = [
    "f_builtins",
    "f_code",
    "f_globals",
    "f_locals",
    "f_generator",
    "func_code",
    "func_globals",
    "co_code",
    "_co_code_adaptive",
    "mro",
    "tb_frame",
    "gi_code",
    "gi_frame",
    "gi_yieldfrom",
    "cr_await",
    "cr_code",
    "cr_frame",
    "ag_await",
    "ag_code",
    "ag_frame",
];

pub(crate) const CORE_INTERNAL: [&str; 30] = [
    "browse",
    "check_access",
    "concat",
    "ensure_one",
    "exists",
    "fetch",
    "filtered",
    "filtered_domain",
    "flush_model",
    "flush_recordset",
    "grouped",
    "init",
    "invalidate_model",
    "invalidate_recordset",
    "is_transient",
    "lock_for_update",
    "mapped",
    "modified",
    "new",
    "search_fetch",
    "sorted",
    "sudo",
    "try_lock_for_update",
    "union",
    "update",
    "with_company",
    "with_context",
    "with_env",
    "with_prefetch",
    "with_user",
];

erp_orm::capability!(
    /// The capability that lets a subject see the `debug` field of error bodies when the
    /// operator switch `rpc.error_debug` is on (`addons/base` maps it to
    /// `base.group_system`).
    pub RPC_DEBUG_ERRORS = "rpc.debug_errors"
);

/// Who the transport authenticated (AUTH).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Caller {
    /// A session, or a `user`-kind API key (its scope is in the principal).
    Principal(Principal),
    /// An `agent` or `service` API key: refused until.
    ApiKey {
        /// Its kind.
        kind: KeyKind,
        /// The key.
        key: ApiKeyId,
    },
    /// The public user of a website or company.
    Public,
    /// A link token.
    Link,
}

/// What the server knows besides the database: active languages and the subject's capabilities.
/// Both answer from memory (named caches, the resolved principal); neither takes a transaction.
pub trait RpcHost: Send + Sync + 'static {
    /// Whether `code` is an active language (`en_US` always is; the service checks that first).
    ///
    /// # Errors
    ///
    /// Any error of the language source; the call fails with it.
    fn language_active(&self, code: &str) -> Result<bool>;

    /// Whether `principal` holds `capability` (provides the real check).
    ///
    /// # Errors
    ///
    /// Any error of the capability source; the `debug` field is then concealed.
    fn has_capability(&self, principal: &Principal, capability: &Capability) -> Result<bool>;
}

/// A transport credential checked in the same primary transaction that executes each attempt.
/// Retries must recheck the credential. Implementations must use the supplied ORM generation
/// or refuse it; they must never substitute a system principal or reuse an open transaction.
pub trait RpcCredential: Send + Sync + std::fmt::Debug {
    /// Open with the effect/context selected by RPC and return the credential's transaction.
    fn begin<'a>(
        &'a self,
        orm: &'a Orm,
        options: erp_orm::CredentialOptions,
    ) -> erp_orm::BoxFuture<'a, Result<Transaction>>;

    /// Render a non-retryable failure from this credential's `begin`, preserving the
    /// transport's authentication protocol (for example bearer 401 or address cooldown 429).
    /// Method/ORM failures after admission never pass through this hook. Retryable opening
    /// errors retain the service's normal retry policy. The default keeps domain errors.
    fn failure(&self, error: Error) -> RpcFailure {
        error.into()
    }

    /// Execute a fixed browser password operation without an ordinary outer RPC transaction.
    /// The default supplies no HTTP authority. Errors retain the normal RPC error policy.
    fn password<'a>(
        &'a self,
        _orm: &'a Orm,
        _call: crate::PasswordCall<'a>,
    ) -> erp_orm::BoxFuture<'a, std::result::Result<crate::PasswordReply, RpcFailure>> {
        Box::pin(async { Err(Error::user("This method can only be accessed over HTTP").into()) })
    }
}

/// Where the service gets the current [`Orm`] of the database: after a registry change in
/// another process the old one is superseded and a retry runs on the new one.
pub trait OrmSource: Send + Sync + 'static {
    /// The current ORM.
    ///
    /// # Errors
    ///
    /// Any error of the source (a database being rebuilt).
    fn current(&self) -> Result<Orm>;
}

impl OrmSource for Orm {
    fn current(&self) -> Result<Orm> {
        Ok(self.clone())
    }
}

/// What a [`Gate`] sees.
#[derive(Debug)]
#[non_exhaustive]
pub struct GateCall<'a> {
    /// The principal.
    pub principal: &'a Principal,
    /// The model.
    pub model: &'a str,
    /// The method.
    pub method: &'a str,
    /// The chain's effect.
    pub effect: Effect,
    /// The call's idempotency key.
    pub idempotency_key: Option<&'a IdempotencyKey>,
}

/// A gate run before dispatch, after the built-in ones (principal kind, scope allowlist,
/// `read_only` versus effect):'s approval slot (design note §12).
pub trait Gate: Send + Sync + 'static {
    /// Refuse or pass the call.
    ///
    /// # Errors
    ///
    /// The refusal, rendered as the call's error body.
    fn check(&self, call: &GateCall<'_>) -> std::result::Result<(), RpcFailure>;
}

/// One call, as the transport hands it over.
pub struct RpcCall {
    route: std::result::Result<Route, RpcFailure>,
    body: Vec<u8>,
    caller: Caller,
    carried: CarriedContext,
    idempotency_key: Option<IdempotencyKey>,
    deadline: Option<Duration>,
    correlation: Option<CorrelationId>,
    capture: bool,
    credential: Option<Arc<dyn RpcCredential>>,
    attempt_hook: Option<Arc<dyn AttemptHook>>,
}

impl std::fmt::Debug for RpcCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcCall")
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

impl RpcCall {
    /// A JSON-2 call: `POST <path>` with `body` (already size-limited by the transport).
    pub fn json2(path: &str, body: impl Into<Vec<u8>>, caller: Caller) -> Self {
        Self {
            route: Route::parse_json2(path),
            body: body.into(),
            caller,
            carried: CarriedContext::none(),
            idempotency_key: None,
            deadline: None,
            correlation: None,
            capture: false,
            credential: None,
            attempt_hook: None,
        }
    }

    /// A call on an already validated route (`call_kw`, MCP).
    pub fn new(route: Route, body: impl Into<Vec<u8>>, caller: Caller) -> Self {
        Self {
            route: Ok(route),
            ..Self::json2("", body, caller)
        }
    }

    /// Context keys the transport carries (`call_kw`).
    #[must_use]
    pub fn carried(mut self, carried: CarriedContext) -> Self {
        self.carried = carried;
        self
    }

    /// Require a fresh primary credential check inside every execution attempt.
    /// The transaction must retain this call's authenticated subject and principal kind.
    #[must_use]
    pub fn credential(mut self, credential: Arc<dyn RpcCredential>) -> Self {
        self.credential = Some(credential);
        self
    }

    /// Run `hook` inside every execution attempt of this call ([`AttemptHook`]).
    /// Only server-side Rust code sets it; no body field, context key or header reaches it. The
    /// call must have a write effect and an [`idempotency_key`](Self::idempotency_key), or it is
    /// refused before any transaction.
    #[must_use]
    pub fn attempt_hook(mut self, hook: Arc<dyn AttemptHook>) -> Self {
        self.attempt_hook = Some(hook);
        self
    }

    /// The `Idempotency-Key` header ([`IdempotencyKey::from_headers`]).
    #[must_use]
    pub fn idempotency_key(mut self, key: IdempotencyKey) -> Self {
        self.idempotency_key = Some(key);
        self
    }

    /// The time left for the call (default: [`RpcConfig::deadline`]).
    #[must_use]
    pub fn deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// The correlation id (shown in `debug`, stored with idempotency keys, logged).
    #[must_use]
    pub fn correlation(mut self, correlation: CorrelationId) -> Self {
        self.correlation = Some(correlation);
        self
    }

    /// Record the texts of the statements the answering attempt counts
    /// ([`RpcResponse::captured`]); used by the floor service's statement capture
    /// (`erp-bench capture-serve`). Off by default: recording costs an allocation
    /// per statement.
    #[must_use]
    pub fn capture_statements(mut self) -> Self {
        self.capture = true;
        self
    }
}

/// The answer to a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponse {
    status: u16,
    body: Vec<u8>,
    replayed: bool,
    retry_after: Option<u32>,
    attempts: u32,
    statements: u64,
    captured: Option<Vec<String>>,
    hook_refusal: Option<HookRefusal>,
}

impl RpcResponse {
    /// The HTTP status.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The JSON body (`Content-Type: application/json; charset=utf-8`).
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The body, by value.
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }

    /// The body is a stored response replayed for an idempotency key (header
    /// `Idempotent-Replayed: true`).
    pub fn replayed(&self) -> bool {
        self.replayed
    }

    /// `Retry-After` seconds.
    pub fn retry_after(&self) -> Option<u32> {
        self.retry_after
    }

    /// Execution attempts (1 without retry; 0 when refused before execution).
    /// Managed password operations may own several internal transactions in one attempt.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Statements the answering attempt sent, transaction control and the signal read
    /// excluded ([`erp_orm::Transaction::query_count`]): the method's own, plus the idempotency
    /// claim and store (design note §14 budgets). 0 for a failure or an auth-owned managed
    /// operation whose statements are not counted here; that sentinel is not a measurement.
    pub fn statements(&self) -> u64 {
        self.statements
    }

    /// The texts of the [`RpcResponse::statements`], in order, when the call asked for them
    /// ([`RpcCall::capture_statements`]) and an attempt answered; `None` otherwise (a failure
    /// answers without one). Managed password operations refuse capture explicitly.
    pub fn captured(&self) -> Option<&[String]> {
        self.captured.as_deref()
    }

    /// The refusal of the call's [`AttemptHook`] when that refusal is the answer; `None` for
    /// every other outcome (success, replay, a method-body or ORM error, a transport refusal).
    /// Server-side only: the body bytes are unchanged, so a [`RefusalKind::Concealed`] refusal
    /// still renders as the access error. This is how a confirm route tells its own hook's
    /// refusal apart from an access error raised by the method body.
    pub fn hook_refusal(&self) -> Option<&HookRefusal> {
        self.hook_refusal.as_ref()
    }
}

/// The JSON-2 core: turns an authenticated call into one unit of work.
pub struct RpcService {
    orms: Arc<dyn OrmSource>,
    host: Arc<dyn RpcHost>,
    config: RpcConfig,
    pending: PendingMethods,
    gates: Vec<Box<dyn Gate>>,
    #[cfg(feature = "test-faults")]
    commit_fault: Mutex<Option<erp_orm::CommitFault>>,
    /// Replaces the backoff pause while set (RPC-018). Tests only.
    #[cfg(feature = "test-faults")]
    retry_pause: Mutex<Option<Duration>>,
    jitter: Mutex<u64>,
}

impl std::fmt::Debug for RpcService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcService")
            .field("config", &self.config)
            .field("gates", &self.gates.len())
            .finish_non_exhaustive()
    }
}

/// How a (model, method) answers before any transaction.
enum Lookup {
    Exposed(Exposed),
    Private,
    Pending(String),
    UnknownModel,
    UnknownMethod,
}

/// Everything decided before the first transaction.
#[derive(Clone)]
struct Plan<'a> {
    route: &'a Route,
    entry: String,
    principal: Principal,
    bound: Bound,
    key: Option<&'a IdempotencyKey>,
    request_digest: Option<[u8; 32]>,
    /// Only for keyed calls (the binding encoding costs an allocation).
    binding_digest: Option<[u8; 32]>,
    correlation: Option<&'a CorrelationId>,
    deadline: Instant,
    deadline_secs: u64,
    /// [`RpcCall::capture_statements`].
    capture: bool,
    credential: Option<&'a dyn RpcCredential>,
    /// [`RpcCall::attempt_hook`]: only on a keyed call with a write effect.
    hook: Option<&'a dyn AttemptHook>,
    /// The execution identity shared by every attempt of the call (correlation and operation
    /// ids); retried attempts keep it, a new call gets a fresh one.
    identity: OperationIdentity,
}

/// What a failed, replayed or selected-read attempt leaves for its independent audit row
/// (design `m3-audit` §7).
#[derive(Default)]
struct Trail {
    /// The evidence of the attempt that ended the call without a committed change.
    evidence: Option<AttemptEvidence>,
    /// The idempotency row a replay was answered from.
    replay_row: Option<i64>,
    /// A read that policy selected for audit (an agent or service principal, or an elevation).
    read_ok: Option<AttemptEvidence>,
}

/// What an attempt ended with.
enum Attempt {
    /// Committed (or replayed): the response.
    Done(RpcResponse),
    /// A serialization failure, deadlock or superseded ORM: run again.
    Retry(Error),
    /// The call's answer.
    Fail(RpcFailure, DebugInfo),
}

impl RpcService {
    /// A service over `orms` with `host` and `config`, and core's pending methods
    /// ([`PendingMethods::core`]).
    pub fn new(orms: impl OrmSource, host: impl RpcHost, config: RpcConfig) -> Self {
        Self {
            orms: Arc::new(orms),
            host: Arc::new(host),
            config,
            pending: PendingMethods::core(),
            gates: Vec::new(),
            #[cfg(feature = "test-faults")]
            commit_fault: Mutex::new(None),
            #[cfg(feature = "test-faults")]
            retry_pause: Mutex::new(None),
            jitter: Mutex::new(0x9E37_79B9_7F4A_7C15),
        }
    }

    /// Replace the pending-method table.
    #[must_use]
    pub fn pending(mut self, pending: PendingMethods) -> Self {
        self.pending = pending;
        self
    }

    /// Add a gate after the built-in ones (TRUST); gates run in the order added.
    #[must_use]
    pub fn gate(mut self, gate: impl Gate) -> Self {
        self.gates.push(Box::new(gate));
        self
    }

    /// The configuration.
    pub fn config(&self) -> &RpcConfig {
        &self.config
    }

    /// Make the next commit simulate a lost connection (RPC-012). Tests only.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn inject_commit_fault(&self, fault: erp_orm::CommitFault) {
        *self
            .commit_fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(fault);
    }

    /// Make every pause between attempts exactly `pause` (`None` restores the jittered
    /// backoff), so a test can stage "the backoff would end past the deadline" without
    /// depending on how long an attempt takes. Tests only.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn inject_retry_pause(&self, pause: Option<Duration>) {
        *self
            .retry_pause
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = pause;
    }

    /// Render a failure the transport detected itself (401, 413, 429, 503 …) with this
    /// service's error policy. `debug` is always empty: no principal is known.
    pub fn reject(&self, failure: RpcFailure) -> RpcResponse {
        log_failure(&failure, &FailureLog::default());
        RpcResponse {
            status: failure.status(),
            body: failure.body("", body_timestamp(), None),
            replayed: false,
            retry_after: failure.retry_after(),
            attempts: 0,
            statements: 0,
            captured: None,
            hook_refusal: None,
        }
    }

    /// Serve one call. Never fails: every failure is an error body with its status.
    pub async fn call(&self, call: RpcCall) -> RpcResponse {
        let started = Instant::now();
        // absent, not wrong: the transport gave no deadline, so the configured one applies
        let deadline = call.deadline.unwrap_or(self.config.deadline());
        let mut attempts = 0;
        // one unit of work per call: a write the call began is read back from the primary, and
        // the routing counters can tell a call from a read outside any request
        let trace = RequestTrace::new(call.correlation.as_ref().map(CorrelationId::as_str));
        let outcome = erp_orm::trace::with_request_trace(
            Arc::clone(&trace),
            // boxed: the call future is large, and one more inline layer around it overflowed the
            // 2 MiB stack of a debug-build test thread
            Box::pin(erp_orm::unit_of_work(self.run(
                &call,
                started,
                deadline,
                &mut attempts,
            ))),
        )
        .await;
        match outcome {
            Ok(mut response) => {
                response.attempts = attempts;
                response
            }
            Err(failed) => {
                let (failure, debug) = *failed;
                let principal = match &call.caller {
                    Caller::Principal(p) => Some(p),
                    _ => None,
                };
                // Credential attempts resolve a newer principal than the preflight caller.
                // Conceal debug rather than authorizing disclosure with that older resolution.
                let shown = call.credential.is_none() && self.debug_allowed(principal);
                let debug_text = if shown {
                    debug.render(&failure)
                } else {
                    String::new()
                };
                let (model, method) = match &call.route {
                    Ok(route) => (Some(route.model()), Some(route.method())),
                    Err(_) => (None, None),
                };
                let addon = trace.addon();
                log_failure(
                    &failure,
                    &FailureLog {
                        request_id: trace.id(),
                        model,
                        method,
                        uid: principal.map(|p| p.subject().get()),
                        addon: addon.as_deref(),
                    },
                );
                RpcResponse {
                    status: failure.status(),
                    body: failure.body(&debug_text, body_timestamp(), trace.id()),
                    replayed: false,
                    retry_after: failure.retry_after(),
                    attempts,
                    statements: 0,
                    captured: None,
                    // filled only from the hook's own refusal, never from a domain error
                    hook_refusal: match failure {
                        RpcFailure::Hook(refusal) => Some(refusal),
                        _ => None,
                    },
                }
            }
        }
    }

    /// `debug` is shown only with the operator switch on and the capability held.
    fn debug_allowed(&self, principal: Option<&Principal>) -> bool {
        if !self.config.error_debug() {
            return false;
        }
        let Some(principal) = principal else {
            return false;
        };
        match self.host.has_capability(principal, &RPC_DEBUG_ERRORS) {
            Ok(held) => held,
            Err(error) => {
                tracing::warn!(
                    target: "erp::rpc",
                    error = %error,
                    "capability check for rpc.debug_errors failed; debug concealed"
                );
                false
            }
        }
    }

    async fn run(
        &self,
        call: &RpcCall,
        started: Instant,
        deadline: Duration,
        attempts: &mut u32,
    ) -> std::result::Result<RpcResponse, Box<(RpcFailure, DebugInfo)>> {
        let plain = |failure: RpcFailure| Box::new((failure, DebugInfo::default()));
        let route = match &call.route {
            Ok(route) => route,
            Err(failure) => return Err(plain(clone_failure(failure))),
        };
        let mut debug = DebugInfo {
            correlation: call.correlation.as_ref().map(ToString::to_string),
            entry: Some(route.entry()),
            statement: None,
        };
        // 1. the caller (gate 1)
        let principal = match &call.caller {
            Caller::Principal(p) if p.kind() == PrincipalKind::User => p.clone(),
            // only user principals call over RPC until (agent, service, public,
            // link) — and `System` never does
            Caller::Principal(p) => {
                return Err(plain(
                    Error::new(
                        ErrorKind::AccessDenied,
                        format!(
                            "principals of kind '{}' are not accepted until a later release",
                            p.kind().as_str()
                        ),
                    )
                    .into(),
                ));
            }
            Caller::ApiKey { kind, .. } => {
                let kind = match kind {
                    KeyKind::Agent => "agent",
                    KeyKind::Service => "service",
                    other => {
                        return Err(plain(
                            Error::programming(format!(
                                "API key kind {other:?} is not handled by the RPC layer"
                            ))
                            .into(),
                        ));
                    }
                };
                return Err(plain(
                    Error::new(
                        ErrorKind::AccessDenied,
                        format!("API keys of kind '{kind}' are not accepted until a later release"),
                    )
                    .into(),
                ));
            }
            Caller::Public | Caller::Link => {
                let kind = if matches!(call.caller, Caller::Public) {
                    "public"
                } else {
                    "link"
                };
                if call.idempotency_key.is_some() {
                    return Err(plain(
                        InfraError::IdempotencyNotAllowed(IdempotencyRefusal::CallerKind { kind })
                            .into(),
                    ));
                }
                return Err(plain(
                    Error::new(
                        ErrorKind::AccessDenied,
                        format!("{kind} callers are not accepted until a later release"),
                    )
                    .into(),
                ));
            }
        };
        // the key's model and method allowlist, before any lookup: a scoped key learns nothing
        // about names outside its scope and opens no transaction for them
        let mut orm = self.orms.current().map_err(|e| plain(e.into()))?;
        scope_allowlist(&orm, route, &principal).map_err(plain)?;
        // 2. the body (design note §6.2)
        if call.body.len() as u64 > self.config.body_bytes() {
            return Err(plain(
                ProtocolError::RequestEntityTooLarge {
                    limit: self.config.body_bytes(),
                }
                .into(),
            ));
        }
        let body = if call.body.is_empty() {
            Json::Object(Vec::new())
        } else {
            match json::parse(&call.body, self.config.json_depth()) {
                Ok(Json::Null) => Json::Object(Vec::new()),
                Ok(object @ Json::Object(_)) => object,
                Ok(other) => {
                    return Err(plain(
                        ProtocolError::BadRequest(format!(
                            "the JSON-2 body must be an object, got {}",
                            other.type_name()
                        ))
                        .into(),
                    ));
                }
                Err(e) => {
                    return Err(plain(
                        ProtocolError::BadRequest(format!(
                            "could not parse the body as json: {}",
                            e.detail()
                        ))
                        .into(),
                    ));
                }
            }
        };
        // 3. exposure (no statement), then the registry check for unknown names
        let mut refreshes: u32 = 0;
        let exposed = loop {
            match self.lookup(&orm, route) {
                Lookup::Exposed(exposed) => break exposed,
                Lookup::Private => return Err(plain(private_error(route))),
                Lookup::Pending(task) => {
                    return Err(plain(
                        InfraError::NotImplemented {
                            entry: route.entry(),
                            task,
                        }
                        .into(),
                    ));
                }
                unknown @ (Lookup::UnknownModel | Lookup::UnknownMethod) => {
                    // decided only on a current registry (SIGNAL): open a transaction
                    match registry_current(&orm, &principal, route).await {
                        Ok(true) => {
                            return Err(plain(not_found(route, &unknown)));
                        }
                        Ok(false) => {
                            refreshes += 1;
                            if refreshes >= MAX_ATTEMPTS {
                                return Err(plain(
                                    Error::new(
                                        ErrorKind::Concurrency,
                                        format!(
                                            "the registry was replaced {refreshes} times while \
                                             looking up {}",
                                            route.entry()
                                        ),
                                    )
                                    .into(),
                                ));
                            }
                            orm = self.orms.current().map_err(|e| plain(e.into()))?;
                        }
                        Err(e) => return Err(plain(e.into())),
                    }
                }
            }
        };
        let effect = exposed.effect();
        // This method returns a one-time credential. Never write its plaintext
        // response to the generic idempotency replay store, on any transport.
        if call.idempotency_key.is_some()
            && route.model() == "res.users.apikeys"
            && route.method() == "generate"
        {
            return Err(plain(ProtocolError::UnprocessableEntity(
                "Idempotency-Key is not accepted for API key generation: credentials are returned only once.".to_owned()
            ).into()));
        }
        // 4. an attempt hook needs a keyed write, then no idempotency key on a read
        if call.attempt_hook.is_some() && (effect == Effect::Read || call.idempotency_key.is_none())
        {
            return Err(plain(
                Error::programming(format!(
                    "An attempt hook needs a call with a write effect and an Idempotency-Key; \
                     {} has effect '{effect}' and the call {}.",
                    route.entry(),
                    if call.idempotency_key.is_some() {
                        "has a key"
                    } else {
                        "has no key"
                    }
                ))
                .into(),
            ));
        }
        if call.idempotency_key.is_some() && effect == Effect::Read {
            return Err(plain(
                InfraError::IdempotencyNotAllowed(IdempotencyRefusal::ReadEffect {
                    entry: route.entry(),
                })
                .into(),
            ));
        }
        // 5. gates (design note §12): scope allowlist, read_only versus effect, then TRUST's
        self.gates_check(route, &principal, effect, call.idempotency_key.as_ref())
            .map_err(plain)?;
        // 6. binding (no statement)
        let bound = binding::bind_body(
            route,
            body,
            exposed.receiver(),
            exposed.params(),
            &call.carried,
        )
        .map_err(plain)?;
        let principal = match &bound.context.allowed_company_ids {
            None => principal,
            Some(ids) => with_companies(&principal, ids).map_err(plain)?,
        };
        if let Some(lang) = &bound.context.lang
            && lang != "en_US"
        {
            match self.host.language_active(lang) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(plain(
                        Error::user(format!("Invalid language code: {lang}")).into(),
                    ));
                }
                Err(e) => return Err(plain(e.into())),
            }
        }
        if let Some(operation) = crate::PasswordOperation::lookup(route.model(), route.method()) {
            if bound.html_validation.reports() {
                return Err(plain(
                    Error::validation("Password operations do not accept html_validation.").into(),
                ));
            }
            if call.capture {
                return Err(plain(
                    Error::validation("Password operations do not accept statement capture.")
                        .into(),
                ));
            }
            if call.attempt_hook.is_some() {
                return Err(plain(
                    Error::programming("Password operations do not accept an attempt hook.").into(),
                ));
            }
            if call.idempotency_key.is_some() {
                return Err(plain(
                    Error::validation(
                        "Idempotency keys are not supported for password operations.",
                    )
                    .into(),
                ));
            }
            let model = orm.registry().model_id(route.model()).ok_or_else(|| {
                plain(Error::programming("Managed password model disappeared.").into())
            })?;
            if !orm.dispatch().is_base_method(model, route.method()) {
                return Err(plain(Error::programming("Password operations require the native base implementation without overrides.").into()));
            }
            let limit = usize::try_from(self.config.result_bytes()).map_err(|_| {
                plain(Error::programming("Password result limit is not representable.").into())
            })?;
            for possible in crate::PasswordReply::possible() {
                output::serialize(&possible.value(), limit).map_err(|_| {
                    plain(
                        InfraError::ResultTooLarge {
                            entry: route.entry(),
                            limit: self.config.result_bytes(),
                        }
                        .into(),
                    )
                })?;
            }
            let credential = call.credential.as_deref().ok_or_else(|| {
                plain(Error::user("This method can only be accessed over HTTP").into())
            })?;
            let failure = Mutex::new(None);
            let companies = bound.context.allowed_company_ids.clone();
            let gate = |fresh: &Principal| {
                let restricted = match &companies {
                    Some(ids) => with_companies(fresh, ids)?,
                    None => fresh.clone(),
                };
                scope_allowlist(&orm, route, &restricted)?;
                self.gates_check(route, &restricted, effect, None)
            };
            let managed = crate::PasswordCall {
                operation,
                route,
                ids: bound.ids,
                args: bound.args,
                context: bound.context.context,
                gate: &gate,
                failure: &failure,
            };
            *attempts = 1;
            let reply = tokio::time::timeout_at(
                (started + deadline).into(),
                credential.password(&orm, managed),
            )
            .await;
            if let Some(error) = failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return Err(plain(error));
            }
            let reply = match reply {
                Ok(value) => value.map_err(plain)?,
                Err(_) if operation.changes_password() => {
                    return Err(plain(
                        InfraError::CommitUnknown {
                            entry: route.entry(),
                            keyed: false,
                        }
                        .into(),
                    ));
                }
                Err(_) => {
                    return Err(plain(
                        InfraError::DeadlineExceeded {
                            secs: deadline.as_secs(),
                        }
                        .into(),
                    ));
                }
            };
            let body = output::serialize(&reply.value(), limit).map_err(|_| {
                plain(
                    Error::programming("Fixed password result exceeded its admitted shape.").into(),
                )
            })?;
            return Ok(RpcResponse {
                status: 200,
                body,
                replayed: false,
                retry_after: None,
                attempts: 1,
                statements: 0,
                captured: None,
                hook_refusal: None,
            });
        }
        let request_digest = call.idempotency_key.as_ref().map(|_| {
            idempotency::request_digest(
                route.model(),
                route.method(),
                &bound.ids_json,
                &bound.context_json,
                &bound.kwargs_json,
            )
        });
        let plan = Plan {
            route,
            entry: route.entry(),
            binding_digest: call
                .idempotency_key
                .as_ref()
                .map(|_| idempotency::binding_digest(&principal.binding_bytes())),
            principal,
            bound,
            key: call.idempotency_key.as_ref(),
            request_digest,
            correlation: call.correlation.as_ref(),
            deadline: started + deadline,
            deadline_secs: deadline.as_secs(),
            capture: call.capture,
            credential: call.credential.as_deref(),
            hook: call.attempt_hook.as_deref(),
            identity: match call.correlation.as_ref() {
                Some(correlation) => OperationIdentity::new(correlation.clone()),
                None => OperationIdentity::generate(),
            }
            .map_err(|e| plain(e.into()))?,
        };
        // 7. the unit of work, retried as a whole
        let mut exposed = exposed;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            *attempts = attempt;
            let mut trail = Trail::default();
            let outcome = self
                .attempt(&orm, &exposed, &plan, attempt, &mut trail)
                .await;
            match outcome {
                Attempt::Done(response) => {
                    // a replay and a selected read are audited before the answer is released:
                    // when the row cannot be written the result is withheld
                    if let Err(failure) =
                        self.audit_answered(&orm, &plan, &mut trail, attempt).await
                    {
                        return Err(Box::new((failure, debug)));
                    }
                    return Ok(response);
                }
                Attempt::Fail(failure, info) => {
                    self.audit_failed(&orm, &plan, &mut trail, &failure, attempt)
                        .await;
                    return Err(Box::new((failure, info)));
                }
                Attempt::Retry(error) => {
                    if attempt >= MAX_ATTEMPTS {
                        let failure: RpcFailure = error.into();
                        self.audit_failed(&orm, &plan, &mut trail, &failure, attempt)
                            .await;
                        return Err(Box::new((failure, debug)));
                    }
                    let pause = self.backoff(attempt);
                    if Instant::now() + pause >= plan.deadline {
                        // the next attempt could not finish in time: the last error stands
                        let failure: RpcFailure = error.into();
                        self.audit_failed(&orm, &plan, &mut trail, &failure, attempt)
                            .await;
                        return Err(Box::new((failure, debug)));
                    }
                    tokio::time::sleep(pause).await;
                    if error.is_superseded() {
                        orm = self.orms.current().map_err(|e| plain(e.into()))?;
                    }
                    // the ORM may have been replaced: the gates, the key check and the binding
                    // were decided on the first registry's declaration, so a retry runs only when
                    // the new one declares the same effect, receiver and parameters
                    match self.lookup(&orm, route) {
                        Lookup::Exposed(e) => {
                            if let Some(change) = declaration_change(&exposed, &e) {
                                return Err(plain(
                                    Error::new(
                                        ErrorKind::Concurrency,
                                        format!(
                                            "the declaration of {} changed while the call was \
                                             retried ({change}); send the call again",
                                            route.entry()
                                        ),
                                    )
                                    .into(),
                                ));
                            }
                            exposed = e;
                        }
                        other => {
                            let failure = match other {
                                Lookup::Private => private_error(route),
                                Lookup::Pending(task) => InfraError::NotImplemented {
                                    entry: route.entry(),
                                    task,
                                }
                                .into(),
                                unknown => not_found(route, &unknown),
                            };
                            return Err(plain(failure));
                        }
                    }
                    debug.statement = None;
                }
            }
        }
    }

    fn lookup(&self, orm: &Orm, route: &Route) -> Lookup {
        let registry = orm.registry();
        let Some(model) = registry.model_id(route.model()) else {
            return Lookup::UnknownModel;
        };
        let method = route.method();
        if method.starts_with('_') || UNSAFE_NAMES.contains(&method) {
            return Lookup::Private;
        }
        match orm.exposure().get(model, method) {
            Some(Exposure::Exposed(e)) => Lookup::Exposed(e.clone()),
            Some(Exposure::Hidden { .. }) => Lookup::Private,
            Some(_) | None => match self.pending.lookup(route.model(), method) {
                _ if CORE_INTERNAL.contains(&method) => Lookup::Private,
                Some(task) => Lookup::Pending(task.to_owned()),
                None if orm.exposure().get(model, method).is_some() => Lookup::Private,
                None => Lookup::UnknownMethod,
            },
        }
    }

    fn gates_check(
        &self,
        route: &Route,
        principal: &Principal,
        effect: Effect,
        key: Option<&IdempotencyKey>,
    ) -> std::result::Result<(), RpcFailure> {
        // the allowlist was checked before the lookup (`scope_allowlist`); a retry on a replaced
        // ORM keeps the route, so it still holds
        if let Some(scope) = principal.scope()
            && scope.read_only()
            && effect != Effect::Read
        {
            return Err(Error::access(format!(
                "The API key '{}' is read-only: it may not call '{}/{}' (effect '{effect}').",
                scope.name(),
                route.model(),
                route.method()
            ))
            .into());
        }
        let gate_call = GateCall {
            principal,
            model: route.model(),
            method: route.method(),
            effect,
            idempotency_key: key,
        };
        for gate in &self.gates {
            gate.check(&gate_call)?;
        }
        Ok(())
    }

    /// One transaction: claim, call, serialize, commit. What an audit row of an attempt that did
    /// not commit needs is left in `trail`.
    async fn attempt(
        &self,
        orm: &Orm,
        exposed: &Exposed,
        plan: &Plan<'_>,
        attempt_no: u32,
        trail: &mut Trail,
    ) -> Attempt {
        let mut debug = DebugInfo {
            correlation: plan.correlation.map(ToString::to_string),
            entry: Some(plan.entry.clone()),
            statement: None,
        };
        // a call that changes data is refused before any statement when the audit store cannot
        // take its row; reads need it only when policy selects them
        let writes = exposed.effect() != Effect::Read;
        if writes && let Err(failure) = self.audit_ready(orm).await {
            return Attempt::Fail(failure, debug);
        }
        let mut options = BeginOptions::for_principal(plan.principal.clone())
            .context(plan.bound.context.context.clone())
            .operation_identity(plan.identity.clone());
        if exposed.effect() == Effect::Read {
            // the service is the read-audit owner of every read-effect call: it writes the attempt
            // evidence as one `read_ok` row before the body is released (A2)
            let owner = match ReadAuditOwner::new(READ_AUDIT_OWNER) {
                Ok(owner) => owner,
                Err(error) => return classify(error, debug),
            };
            options = options.read_only_audited(plan.entry.as_str(), owner);
            if exposed.primary_read() {
                options = options.on_primary();
            }
        }
        let mut credential_failed = false;
        let opening = async {
            match plan.credential {
                Some(credential) => {
                    let mut options = erp_orm::CredentialOptions::new()
                        .context(plan.bound.context.context.clone())
                        .operation_identity(plan.identity.clone());
                    if exposed.effect() == Effect::Read {
                        options = options.read_only_audited(
                            plan.entry.as_str(),
                            ReadAuditOwner::new(READ_AUDIT_OWNER)?,
                        );
                    }
                    let tx = credential.begin(orm, options).await.inspect_err(|_| {
                        credential_failed = true;
                    })?;
                    if tx.env().context() != &plan.bound.context.context {
                        return Err(Error::programming(
                            "RPC credential ignored the requested context",
                        ));
                    }
                    let intent_ok = match (tx.intent(), exposed.effect()) {
                        (
                            erp_orm::TxIntent::Write,
                            Effect::Write | Effect::Destructive | Effect::External,
                        ) => true,
                        (erp_orm::TxIntent::Read { entry }, Effect::Read) => {
                            entry.as_ref() == plan.entry.as_str()
                        }
                        _ => false,
                    };
                    // only a read-effect call carries a read-audit owner
                    let owner_matches =
                        tx.read_audit_owner().is_some() == (exposed.effect() == Effect::Read);
                    let intent_matches = intent_ok && owner_matches;
                    if !intent_matches {
                        return Err(Error::programming(
                            "RPC credential ignored the requested transaction intent",
                        ));
                    }
                    if tx.env().principal().subject() != plan.principal.subject()
                        || tx.env().principal().kind() != plan.principal.kind()
                        || !matches!(tx.env().principal().elevation(), erp_orm::Elevation::None)
                        || !Arc::ptr_eq(tx.registry(), orm.registry())
                        || tx.route() != erp_orm::Route::Primary
                    {
                        return Err(Error::programming(
                            "RPC credential returned a different or elevated principal, a different registry, or a nonprimary transaction",
                        ));
                    }
                    Ok(tx)
                }
                None => orm.begin_opts(options).await,
            }
        };
        let begun = tokio::time::timeout_at(plan.deadline.into(), opening).await;
        let mut tx = match begun {
            Ok(Ok(tx)) => tx,
            Ok(Err(error)) => {
                return match (credential_failed, plan.credential, classify(error, debug)) {
                    (true, Some(credential), Attempt::Fail(RpcFailure::Domain(error), info)) => {
                        Attempt::Fail(credential.failure(error), info)
                    }
                    (_, _, outcome) => outcome,
                };
            }
            Err(_) => return deadline_exceeded(plan, debug),
        };
        let mut fresh_plan = plan.clone();
        if plan.credential.is_some() {
            let env = tx.env();
            if let Err(error) = env.companies() {
                return classify(error, debug);
            }
            fresh_plan.principal = env.principal().clone();
            fresh_plan.binding_digest = if plan.key.is_some() {
                match credential_binding_digest(&fresh_plan.principal) {
                    Ok(digest) => Some(digest),
                    Err(error) => return classify(error, debug),
                }
            } else {
                None
            };
            if let Err(error) = scope_allowlist(orm, plan.route, &fresh_plan.principal) {
                return Attempt::Fail(error, debug);
            }
            if let Err(error) = self.gates_check(
                plan.route,
                &fresh_plan.principal,
                exposed.effect(),
                plan.key,
            ) {
                return Attempt::Fail(error, debug);
            }
        }
        let plan = &fresh_plan;
        if plan.capture {
            tx.capture_statements(true);
        }
        if plan.bound.html_validation.strict() {
            tx.require_strict_html();
        }
        let build_call = || self.audit_call(orm, plan, exposed.effect(), attempt_no);
        if writes {
            // the summary of this attempt: written with the change by one INSERT at commit
            let summary = match build_call() {
                Ok(summary) => summary,
                Err(error) => {
                    tx.rollback().await.ok();
                    return classify(error, debug);
                }
            };
            if let Err(error) = tx
                .audit_call(summary)
                .and_then(|()| tx.audit_origin(attempt_origin(plan)))
            {
                tx.rollback().await.ok();
                return classify(error, debug);
            }
        }
        let work = async {
            // resolve user widening against the refreshed transaction binding,
            // within the deadline, before business work. Do not short-circuit group validation.
            let read_selected = if writes {
                false
            } else {
                let widening = self
                    .config
                    .read_audit_policy()
                    .selects_user(&mut tx)
                    .await?;
                let model = orm.registry().model(plan.route.model()).ok_or_else(|| {
                    Error::programming("read audit selection lost the invoked model metadata")
                })?;
                widening
                    || model.audit_reads
                    || matches!(
                        plan.principal.kind(),
                        PrincipalKind::Agent | PrincipalKind::Service
                    )
            };
            self.work(&mut tx, orm, exposed, plan, attempt_no)
                .await
                .map(|work| (read_selected, work))
        };
        let worked = tokio::time::timeout_at(plan.deadline.into(), work).await;
        let (read_selected, done) = match worked {
            Err(_) => {
                // the work future is dropped, its statements ran inside `tx` and are discarded
                // with it; the failure still leaves its attempt row like every other failure
                reapply_hook_origin(&mut tx, plan);
                trail.evidence = Some(evidence_of(&mut tx, build_call().ok()));
                return deadline_exceeded(plan, debug);
            }
            Ok(Err(failure)) => {
                debug.statement = tx.failed_statement().map(str::to_owned);
                // the method body may have changed the origin before it or the hook failed
                reapply_hook_origin(&mut tx, plan);
                trail.evidence = Some(evidence_of(&mut tx, build_call().ok()));
                // `Transaction::rollback` never returns `Err`: a failed ROLLBACK detaches the
                // connection, which rolls back on the server (erp-orm transaction.rs, `discard`)
                tx.rollback().await.ok();
                // a hook refusal (`RpcFailure::Hook`) is the call's answer, never retried
                return match failure {
                    RpcFailure::Domain(error) => classify(error, debug),
                    other => Attempt::Fail(other, debug),
                };
            }
            Ok(Ok((_, Work::Replay(row, policy_now)))) => {
                let statements = tx.query_count();
                let captured = plan.capture.then(|| tx.captured().to_vec());
                trail.evidence = Some(evidence_of(&mut tx, build_call().ok()));
                trail.replay_row = Some(row.id);
                // the row was locked by the claim's DO UPDATE … WHERE: release it at once.
                // `Transaction::rollback` never returns `Err` (see above)
                tx.rollback().await.ok();
                return match self.replay(plan, row, policy_now, debug) {
                    Attempt::Done(mut response) => {
                        response.statements = statements;
                        response.captured = captured;
                        Attempt::Done(response)
                    }
                    other => other,
                };
            }
            Ok(Ok((selected, Work::Done(done)))) => (selected, done),
        };
        #[cfg(feature = "test-faults")]
        if let Some(fault) = self
            .commit_fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            tx.inject_commit_fault(fault);
        }
        let status: i16 = 200;
        let committed = match done.claim {
            Some(id) => {
                let [t_id, t_status, t_body] = idempotency::STORE_TYPES;
                // a response over the cap is not kept: the key still deduplicates, and a replay
                // answers "result no longer available" (409)
                let cap = usize::try_from(self.config.idempotency_response_bytes());
                let stored: Option<&[u8]> = match cap {
                    Ok(cap) if done.body.len() <= cap => Some(done.body.as_slice()),
                    Ok(_) | Err(_) => None,
                };
                let params: [(&(dyn ToSql + Sync), Type); 3] =
                    [(&id, t_id), (&status, t_status), (&stored, t_body)];
                let last = FinalStatement::new(idempotency::STORE_SQL, &params);
                tokio::time::timeout_at(plan.deadline.into(), tx.commit_pipelined(Some(last))).await
            }
            None => tokio::time::timeout_at(plan.deadline.into(), tx.commit_pipelined(None)).await,
        };
        match committed {
            Ok(Ok(())) => {
                if !writes {
                    // Mandatory selection and operator widening are additive; elevation can
                    // also be observed during the work. Persist before releasing the body.
                    let evidence = evidence_of(&mut tx, build_call().ok());
                    if read_selected || evidence.has_elevation() {
                        trail.read_ok = Some(evidence);
                    }
                }
                Attempt::Done(RpcResponse {
                    status: 200,
                    body: done.body,
                    replayed: false,
                    retry_after: None,
                    attempts: 0,
                    statements: tx.query_count(),
                    captured: plan.capture.then(|| tx.captured().to_vec()),
                    hook_refusal: None,
                })
            }
            Ok(Err(CommitError::RolledBack(error))) => {
                trail.evidence = Some(evidence_of(&mut tx, build_call().ok()));
                classify(error, debug)
            }
            Ok(Err(CommitError::Unknown(error))) => {
                trail.evidence = Some(evidence_of(&mut tx, build_call().ok()));
                tracing::error!(
                    target: "erp::rpc",
                    entry = %plan.entry,
                    correlation = ?plan.correlation.map(ToString::to_string),
                    error = %error,
                    "commit outcome unknown"
                );
                Attempt::Fail(
                    InfraError::CommitUnknown {
                        entry: plan.entry.clone(),
                        keyed: plan.key.is_some(),
                    }
                    .into(),
                    debug,
                )
            }
            Ok(Err(other)) => classify(other.into_error(), debug),
            // the deadline cancelled the wait for COMMIT's answer: unknown, never retried
            Err(_) => {
                trail.evidence = Some(evidence_of(&mut tx, build_call().ok()));
                Attempt::Fail(
                    InfraError::CommitUnknown {
                        entry: plan.entry.clone(),
                        keyed: plan.key.is_some(),
                    }
                    .into(),
                    debug,
                )
            }
        }
    }

    /// The audit store must be able to take a row before a call that changes data starts. The
    /// first call on an ORM that was never checked checks it (a server checks at startup).
    async fn audit_ready(&self, orm: &Orm) -> std::result::Result<(), RpcFailure> {
        let mut state = orm.audit_store_state();
        if state == AuditStoreState::Unchecked {
            state = orm.check_audit_store().await.map_err(RpcFailure::from)?;
        }
        if state.allows_writes() {
            return Ok(());
        }
        tracing::error!(
            target: "erp::rpc",
            state = %state.describe(),
            "the audit store cannot take rows: calls that change data are refused"
        );
        Err(InfraError::AuditUnavailable.into())
    }

    /// The call summary of one attempt (model, method, effect, ordered targets, redacted digest).
    fn audit_call(
        &self,
        orm: &Orm,
        plan: &Plan<'_>,
        effect: Effect,
        attempt_no: u32,
    ) -> Result<AuditCall> {
        let digest = crate::audit_call::args_digest(
            orm.registry(),
            plan.route.model(),
            plan.route.method(),
            &plan.bound.ids,
            &plan.bound.kwargs_json,
        );
        Ok(AuditCall::new(
            Some(Arc::from(plan.route.model())),
            Arc::from(plan.route.method()),
            effect,
            TargetIds::from_ids(&plan.bound.ids)?,
            Some(digest),
        )?
        .attempt(u16::try_from(attempt_no).unwrap_or(u16::MAX)))
    }

    /// A replay and a policy-selected read write their attempt row before the answer is released:
    /// when the row cannot be written, the result is withheld (503, nothing else changes).
    async fn audit_answered(
        &self,
        orm: &Orm,
        plan: &Plan<'_>,
        trail: &mut Trail,
        attempt_no: u32,
    ) -> std::result::Result<(), RpcFailure> {
        let number = u16::try_from(attempt_no).unwrap_or(u16::MAX);
        let mut rows = Vec::new();
        if let (Some(row), Some(evidence)) = (trail.replay_row.take(), trail.evidence.take()) {
            rows.push(AuditAttempt::new(evidence, AuditOutcome::Replayed, number).replay_of(row));
        }
        if let Some(evidence) = trail.read_ok.take() {
            rows.push(AuditAttempt::new(evidence, AuditOutcome::ReadOk, number));
        }
        for row in rows {
            if let Err(error) = self.write_attempt(orm, row).await {
                tracing::error!(
                    target: "erp::rpc",
                    entry = %plan.entry,
                    error = %error,
                    "audit attempt row failed: the result is withheld"
                );
                return Err(InfraError::AuditUnavailable.into());
            }
        }
        Ok(())
    }

    /// The independent attempt row of a call that ended without a committed change (denial,
    /// failure, unknown commit). A row that cannot be written is logged: the call already failed
    /// and the caller learns that, never less.
    async fn audit_failed(
        &self,
        orm: &Orm,
        plan: &Plan<'_>,
        trail: &mut Trail,
        failure: &RpcFailure,
        attempt_no: u32,
    ) {
        let Some(evidence) = trail.evidence.take() else {
            return;
        };
        let (outcome, class) = match failure {
            RpcFailure::Infra(InfraError::CommitUnknown { .. }) => {
                (AuditOutcome::CommitUnknown, None)
            }
            RpcFailure::Domain(error) => (
                if matches!(
                    error.kind(),
                    ErrorKind::Access | ErrorKind::AccessDenied | ErrorKind::SessionExpired
                ) {
                    AuditOutcome::Denied
                } else {
                    AuditOutcome::Failed
                },
                Some(ErrorClass::of(error.kind())),
            ),
            RpcFailure::Infra(InfraError::IdempotencyAccessChanged) => {
                (AuditOutcome::Denied, Some(ErrorClass::Access))
            }
            RpcFailure::Infra(
                InfraError::IdempotencyInProgress { .. } | InfraError::IdempotencyResultExpired,
            ) => (AuditOutcome::Failed, Some(ErrorClass::Concurrency)),
            RpcFailure::Infra(InfraError::ResultTooLarge { .. }) => {
                (AuditOutcome::Failed, Some(ErrorClass::Limit))
            }
            RpcFailure::Infra(
                InfraError::AuditUnavailable | InfraError::DeadlineExceeded { .. },
            ) => (AuditOutcome::Failed, Some(ErrorClass::Unavailable)),
            RpcFailure::Infra(_) | RpcFailure::Protocol(_) => {
                (AuditOutcome::Failed, Some(ErrorClass::Validation))
            }
            RpcFailure::Hook(refusal) => match refusal.kind() {
                RefusalKind::Concealed => (AuditOutcome::Denied, Some(ErrorClass::Access)),
                RefusalKind::Refused => (AuditOutcome::Failed, Some(ErrorClass::Validation)),
            },
        };
        let number = u16::try_from(attempt_no).unwrap_or(u16::MAX);
        let mut row = AuditAttempt::new(evidence, outcome, number);
        if let Some(class) = class {
            row = row.error_class(class);
        }
        if outcome == AuditOutcome::CommitUnknown {
            row = row.original_operation(plan.identity.operation().clone());
        }
        if let Err(error) = self.write_attempt(orm, row).await {
            tracing::error!(
                target: "erp::rpc",
                entry = %plan.entry,
                error = %error,
                "audit attempt row failed; the call's own failure is unchanged"
            );
        }
    }

    /// One attempt row, through the ORM's independent connection.
    async fn write_attempt(&self, orm: &Orm, row: AuditAttempt) -> Result<()> {
        orm.record_audit_attempt(row).await
    }

    /// The statements of an attempt before `COMMIT`.
    async fn work(
        &self,
        tx: &mut Transaction,
        orm: &Orm,
        exposed: &Exposed,
        plan: &Plan<'_>,
        attempt_no: u32,
    ) -> std::result::Result<Work, RpcFailure> {
        let claim = match (plan.key, plan.request_digest) {
            (Some(key), Some(digest)) => match self.claim(tx, orm, plan, key, &digest).await? {
                (Claim::Claimed { id }, _) => Some(id),
                // a replay answers from the store: the hook does not run
                (Claim::Existing(row), policy_now) => return Ok(Work::Replay(row, policy_now)),
            },
            _ => None,
        };
        // refused before any transaction otherwise: a hooked call is keyed and claimed here
        let hook = match plan.hook {
            Some(hook) => {
                let key = match (claim, plan.key) {
                    (Some(_), Some(key)) => key,
                    _ => {
                        return Err(Error::programming(
                            "an attempt hook reached execution without a new idempotency claim",
                        )
                        .into());
                    }
                };
                Some((
                    hook,
                    HookCall {
                        model: plan.route.model(),
                        method: plan.route.method(),
                        effect: exposed.effect(),
                        idempotency_key: key,
                        attempt: attempt_no,
                        correlation: plan.correlation,
                    },
                ))
            }
            None => None,
        };
        if let Some((hook, call)) = &hook {
            hook.before_body(tx, call).await?;
        }
        let model = tx.env().records(plan.route.model())?;
        let records = match exposed.receiver() {
            Receiver::Records => model.browse(plan.bound.ids.iter().copied())?,
            Receiver::Model => model,
        };
        let value = exposed
            .call(tx, records, plan.bound.args.clone())
            .await
            .map_err(|e| binding::bind_failure(plan.route, e))?;
        // Stored computes can add sanitation reports while flushing. Finish admission
        // before constructing the response whose bytes are stored with COMMIT.
        tx.flush().await?;
        let reports = plan
            .bound
            .html_validation
            .reports()
            .then(|| tx.take_html_reports())
            .transpose()?;
        let limit = self.config.result_bytes();
        // the limit is at most 1 GiB (RpcConfig); on a 32-bit target a larger one saturates
        let body = match output::serialize_html(
            &value,
            reports.as_deref(),
            usize::try_from(limit).unwrap_or(usize::MAX),
        ) {
            Ok(body) => body,
            Err(WriteError::TooLarge) => {
                return Err(InfraError::ResultTooLarge {
                    entry: plan.entry.clone(),
                    limit,
                }
                .into());
            }
            Err(WriteError::NonFinite) => {
                return Err(Error::programming(format!(
                    "the result of {} holds a float JSON cannot represent (NaN or infinity)",
                    plan.entry
                ))
                .into());
            }
            Err(WriteError::TooDeep) => {
                return Err(Error::programming(format!(
                    "the result of {} is nested deeper than {} levels",
                    plan.entry,
                    output::MAX_RESULT_DEPTH
                ))
                .into());
            }
            Err(WriteError::Unsupported(what)) => {
                return Err(Error::programming(format!(
                    "the result of {} holds a value the JSON writer does not support: {what}",
                    plan.entry
                ))
                .into());
            }
            Err(WriteError::Inconsistent(what)) => {
                return Err(Error::programming(format!(
                    "the result of {} could not be written as JSON: {what}",
                    plan.entry
                ))
                .into());
            }
        };
        if let Some((hook, call)) = &hook {
            hook.after_body(tx, call, &body).await?;
            // a write of the hook fails inside the attempt, not in the commit preamble
            tx.flush().await?;
            // the method body may have set another origin: core's is the one recorded
            tx.audit_origin(hook.origin().audit_origin())?;
        }
        Ok(Work::Done(Done { body, claim }))
    }

    /// The claim statement (design note §10.3).
    async fn claim(
        &self,
        tx: &mut Transaction,
        orm: &Orm,
        plan: &Plan<'_>,
        key: &IdempotencyKey,
        digest: &[u8; 32],
    ) -> std::result::Result<(Claim, i64), RpcFailure> {
        let principal = &plan.principal;
        let kind = match principal.kind() {
            PrincipalKind::User => "user",
            PrincipalKind::Agent => "agent",
            PrincipalKind::Service => "service",
            other => {
                return Err(Error::programming(format!(
                    "{} principals have no idempotency key space",
                    other.as_str()
                ))
                .into());
            }
        };
        let api_key: Option<i64> = principal.scope().map(|s| s.key().get());
        let subject = principal.subject().get();
        // the transaction must be open to know the generations it runs on
        tx.open().await?;
        let policy_generation: i64 = policy_generation(orm, tx)?;
        let epoch = i64::try_from(principal.resolution().epoch().get()).map_err(|_| {
            Error::programming("the resolution epoch does not fit a signed 64-bit integer")
        })?;
        let ttl = self.config.idempotency_ttl_secs() as f64;
        let replay = self.config.idempotency_replay_secs() as f64;
        let key_text = key.as_str();
        let model = plan.route.model();
        let method = plan.route.method();
        let digest = digest.as_slice();
        let binding_digest = plan
            .binding_digest
            .ok_or_else(|| Error::programming("a keyed call without a binding digest"))?;
        let binding = binding_digest.as_slice();
        let correlation: Option<&str> = plan.correlation.map(CorrelationId::as_str);
        let batch: i64 = idempotency::MAINTENANCE_BATCH;
        let params: [(&(dyn ToSql + Sync), Type); 14] = [
            (&key_text, idempotency::CLAIM_TYPES[0].clone()),
            (&kind, idempotency::CLAIM_TYPES[1].clone()),
            (&subject, idempotency::CLAIM_TYPES[2].clone()),
            (&api_key, idempotency::CLAIM_TYPES[3].clone()),
            (&model, idempotency::CLAIM_TYPES[4].clone()),
            (&method, idempotency::CLAIM_TYPES[5].clone()),
            (&digest, idempotency::CLAIM_TYPES[6].clone()),
            (&binding, idempotency::CLAIM_TYPES[7].clone()),
            (&policy_generation, idempotency::CLAIM_TYPES[8].clone()),
            (&epoch, idempotency::CLAIM_TYPES[9].clone()),
            (&correlation, idempotency::CLAIM_TYPES[10].clone()),
            (&ttl, idempotency::CLAIM_TYPES[11].clone()),
            (&replay, idempotency::CLAIM_TYPES[12].clone()),
            (&batch, idempotency::CLAIM_TYPES[13].clone()),
        ];
        match tx.query_typed(idempotency::CLAIM_SQL, &params).await {
            Ok(rows) => Ok((idempotency::decode_claim(&rows)?, policy_generation)),
            // recognised by statement: the claim waited past lock_timeout on a running duplicate
            Err(error) if error.sqlstate() == Some("55P03") => {
                Err(InfraError::IdempotencyInProgress {
                    key: key.clone(),
                    retry_after: IN_PROGRESS_RETRY_AFTER,
                }
                .into())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Answer from a committed key row (design note §10.1).
    fn replay(&self, plan: &Plan<'_>, row: Existing, policy_now: i64, debug: DebugInfo) -> Attempt {
        let Some(digest) = plan.request_digest else {
            return Attempt::Fail(
                Error::programming("a replay without a request digest").into(),
                debug,
            );
        };
        let Some(key) = plan.key else {
            return Attempt::Fail(Error::programming("a replay without a key").into(), debug);
        };
        if row.digest != digest {
            return Attempt::Fail(
                InfraError::IdempotencyMismatch { key: key.clone() }.into(),
                debug,
            );
        }
        let Some(status) = row.status else {
            // claim and store share one transaction: a committed row without a status is
            // impossible unless written by hand
            tracing::error!(target: "erp::rpc", row = row.id, "idempotency key row without a status");
            return Attempt::Fail(
                Error::programming(
                    "an idempotency key row is committed without a status (row id in the server \
                     log)",
                )
                .into(),
                debug,
            );
        };
        let Ok(epoch) = i64::try_from(plan.principal.resolution().epoch().get()) else {
            return Attempt::Fail(
                Error::programming("the resolution epoch does not fit a signed 64-bit integer")
                    .into(),
                debug,
            );
        };
        let same_access = plan.binding_digest.is_some_and(|d| row.binding_digest == d)
            && row.resolution_epoch == epoch
            && row.policy_generation == policy_now;
        if !same_access {
            return Attempt::Fail(InfraError::IdempotencyAccessChanged.into(), debug);
        }
        let Some(body) = row.response.filter(|_| !row.expired) else {
            return Attempt::Fail(InfraError::IdempotencyResultExpired.into(), debug);
        };
        let Ok(status) = u16::try_from(status) else {
            tracing::error!(target: "erp::rpc", row = row.id, status, "idempotency key row status");
            return Attempt::Fail(
                Error::programming(format!(
                    "an idempotency key row holds the status {status} (row id in the server \
                     log)"
                ))
                .into(),
                debug,
            );
        };
        Attempt::Done(RpcResponse {
            status,
            body,
            replayed: true,
            retry_after: None,
            attempts: 0,
            statements: 0,
            captured: None,
            hook_refusal: None,
        })
    }

    /// Full-jitter backoff: `uniform(0, min(cap, base × 2^(attempt-1)))`.
    fn backoff(&self, attempt: u32) -> Duration {
        #[cfg(feature = "test-faults")]
        if let Some(pause) = *self
            .retry_pause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return pause;
        }
        let exp = RETRY_BASE.saturating_mul(1u32 << (attempt.saturating_sub(1)).min(16));
        // at most RETRY_CAP (2 s) in milliseconds: always fits
        let bound = u64::try_from(exp.min(RETRY_CAP).as_millis()).unwrap_or(u64::MAX);
        let mut state = self.jitter.lock().unwrap_or_else(PoisonError::into_inner);
        // xorshift: jitter only, no cryptographic use
        let mut x = *state ^ u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        Duration::from_millis(if bound == 0 { 0 } else { x % (bound + 1) })
    }
}

struct Done {
    body: Vec<u8>,
    claim: Option<i64>,
}

enum Work {
    Done(Done),
    /// A committed row, with the policy generation this attempt runs on.
    Replay(Existing, i64),
}

// A new session resolves with an epoch local to that check. Include the actual effective
// authority in credential replay identity so removed groups/companies cannot replay an old
// response merely because the new resolution also starts at epoch one.
fn credential_binding_digest(principal: &Principal) -> Result<[u8; 32]> {
    let mut bytes = principal.binding_bytes();
    bytes.extend_from_slice(b"credential-resolution:v1");
    let groups = principal.resolution().groups().ids();
    bytes.extend_from_slice(
        &u64::try_from(groups.len())
            .map_err(|_| Error::programming("credential group count overflow"))?
            .to_be_bytes(),
    );
    for group in groups {
        bytes.extend_from_slice(&group.get().to_be_bytes());
    }
    let companies = principal.resolution().all_companies();
    bytes.extend_from_slice(
        &u64::try_from(companies.len())
            .map_err(|_| Error::programming("credential company count overflow"))?
            .to_be_bytes(),
    );
    for company in companies {
        bytes.extend_from_slice(&company.get().to_be_bytes());
    }
    Ok(idempotency::binding_digest(&bytes))
}

/// The `policy` generation the transaction runs on. Without a [`erp_orm::Signal`]
/// the process is the only one and its policy cannot change under it: generation 0.
fn policy_generation(orm: &Orm, tx: &Transaction) -> Result<i64> {
    if orm.signal().is_none() {
        return Ok(0);
    }
    let generations = tx.began_generations().ok_or_else(|| {
        Error::programming("the transaction began without reading the signal generations")
    })?;
    i64::try_from(generations.get(&SignalKey::Policy)).map_err(|_| {
        Error::programming("the policy generation does not fit a signed 64-bit integer")
    })
}

/// The `timestamp` of an error body. A host clock outside the Unix range is logged as an error;
/// the body is still sent (with 0), because an answer the client cannot read would hide the
/// original failure. Idempotency expiry never uses it (the database's `now()` does).
fn body_timestamp() -> i64 {
    match crate::error::unix_seconds() {
        Ok(seconds) => seconds,
        Err(error) => {
            tracing::error!(target: "erp::rpc", %error, "error body timestamp unavailable");
            0
        }
    }
}

/// Gate 2 (design note §12): the API key's model and method allowlist.
fn scope_allowlist(
    orm: &Orm,
    route: &Route,
    principal: &Principal,
) -> std::result::Result<(), RpcFailure> {
    let Some(scope) = principal.scope() else {
        return Ok(());
    };
    if scope.models().allows(route.model()) && scope.methods().allows(route.method()) {
        return Ok(());
    }
    // a model the registry does not know is named by its technical name only
    let description = orm
        .registry()
        .model(route.model())
        .map_or(route.model(), |m| m.description.as_str());
    Err(Error::access(format!(
        "The API key '{}' may not call '{}' on '{description}' ({}): outside its scope.",
        scope.name(),
        route.method(),
        route.model()
    ))
    .into())
}

/// What differs between the declaration a call was planned on and the one of a replaced registry.
fn declaration_change(planned: &Exposed, now: &Exposed) -> Option<String> {
    if planned.effect() != now.effect() {
        return Some(format!("effect {} -> {}", planned.effect(), now.effect()));
    }
    if planned.receiver() != now.receiver() {
        return Some("records or model method changed".to_owned());
    }
    if planned.params() != now.params() {
        return Some("parameters changed".to_owned());
    }
    None
}

/// Whether the registry is current: a transaction begins (the SIGNAL read applies what changed)
/// and is rolled back. `Ok(false)` when the ORM was superseded meanwhile.
async fn registry_current(orm: &Orm, principal: &Principal, route: &Route) -> Result<bool> {
    if orm.signal().is_none() {
        return Ok(true);
    }
    // on the primary: the answer ("is this model or method known?") must not be decided on a
    // replica's lagging state
    let options = BeginOptions::for_principal(principal.clone())
        .read_only(route.entry())
        .on_primary();
    let mut tx = orm.begin_opts(options).await?;
    let opened = tx.open().await;
    tx.rollback().await?;
    match opened {
        Ok(()) => Ok(true),
        Err(error) if error.is_superseded() => Ok(false),
        Err(error) => Err(error),
    }
}

fn with_companies(
    principal: &Principal,
    ids: &[i64],
) -> std::result::Result<Principal, RpcFailure> {
    let companies: std::result::Result<Vec<CompanyId>, PrincipalError> =
        ids.iter().map(|id| CompanyId::new(*id)).collect();
    let companies = companies.map_err(|e| RpcFailure::Domain(e.into()))?;
    if companies.is_empty() {
        // `allowed_company_ids = []` is the subject's default order
        return Ok(principal.with_default_companies());
    }
    principal.with_companies(companies).map_err(|e| match e {
        duplicate @ PrincipalError::DuplicateContextCompany { .. } => {
            RpcFailure::Protocol(ProtocolError::UnprocessableEntity(duplicate.to_string()))
        }
        other => RpcFailure::Domain(other.into()),
    })
}

fn private_error(route: &Route) -> RpcFailure {
    Error::access(format!(
        "Private methods (such as '{}') cannot be called remotely.",
        route.entry()
    ))
    .into()
}

fn not_found(route: &Route, lookup: &Lookup) -> RpcFailure {
    let message = match lookup {
        Lookup::UnknownModel => format!("the model {} does not exist", py_repr(route.model())),
        _ => format!("The method '{}' does not exist", route.entry()),
    };
    ProtocolError::NotFound(message).into()
}

/// Retry serialization failures, deadlocks and a superseded ORM; anything else is the answer.
fn classify(error: Error, debug: DebugInfo) -> Attempt {
    // an audit store that cannot take a row refuses the call (503): the detail is logged
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(current) = cause {
        if let Some(AuditError::Unavailable(_) | AuditError::Insertion { .. }) =
            current.downcast_ref::<AuditError>()
        {
            tracing::error!(target: "erp::rpc", error = %error, "the audit trail could not record the call");
            return Attempt::Fail(InfraError::AuditUnavailable.into(), debug);
        }
        cause = current.source();
    }
    let retryable = error.is_superseded()
        || (error.kind() == ErrorKind::Concurrency
            && matches!(error.sqlstate(), Some("40001" | "40P01")));
    if retryable {
        Attempt::Retry(error)
    } else {
        Attempt::Fail(error.into(), debug)
    }
}

/// The evidence of an attempt that did not commit: what the buffer held, with the caller's
/// environment and the call summary filled in when the buffer never got them.
fn evidence_of(tx: &mut Transaction, call: Option<AuditCall>) -> AttemptEvidence {
    let env = tx.env();
    let mut evidence = tx.take_audit_attempt().with_env(env);
    if let Some(call) = call {
        evidence = evidence.with_call(call);
    }
    evidence
}

/// The audit origin of a write attempt: the hook's mapped origin when one is attached.
fn attempt_origin(plan: &Plan<'_>) -> AuditOrigin {
    match plan.hook {
        Some(hook) => hook.origin().audit_origin(),
        None => AuditOrigin::Rpc,
    }
}

/// Before a failed hooked attempt's evidence is taken: the method body may have set another
/// origin, and the failed-attempt row must carry the hook's. `audit_origin` refuses only a frozen
/// buffer (after the commit preamble), which a failure before `COMMIT` never has; if it ever does,
/// the call's own failure stands and the row keeps the buffer's origin, logged.
fn reapply_hook_origin(tx: &mut Transaction, plan: &Plan<'_>) {
    if let Some(hook) = plan.hook
        && let Err(error) = tx.audit_origin(hook.origin().audit_origin())
    {
        tracing::error!(
            target: "erp::rpc",
            entry = %plan.entry,
            error = %error,
            "the hook's audit origin could not be applied to a failed attempt"
        );
    }
}

fn deadline_exceeded(plan: &Plan<'_>, debug: DebugInfo) -> Attempt {
    Attempt::Fail(
        InfraError::DeadlineExceeded {
            secs: plan.deadline_secs,
        }
        .into(),
        debug,
    )
}

/// Failures are values without `Clone` (domain errors carry sources): a route failure is a
/// protocol error, rebuilt.
fn clone_failure(failure: &RpcFailure) -> RpcFailure {
    match failure {
        RpcFailure::Protocol(p) => RpcFailure::Protocol(p.clone()),
        RpcFailure::Infra(i) => RpcFailure::Infra(i.clone()),
        RpcFailure::Domain(e) => RpcFailure::Domain(Error::new(e.kind(), e.message())),
        RpcFailure::Hook(r) => RpcFailure::Hook(r.clone()),
    }
}

/// What a refusal log line says about the call, besides the failure. Names
/// and ids only: never argument values, bodies, headers, cookies or credentials.
#[derive(Default)]
struct FailureLog<'a> {
    request_id: Option<&'a str>,
    model: Option<&'a str>,
    method: Option<&'a str>,
    uid: Option<i64>,
    /// The WebAssembly addon the request entered last, when it entered one.
    addon: Option<&'a str>,
}

/// The name of the read-audit owner the service declares on its read-effect transactions.
const READ_AUDIT_OWNER: &str = "erp-rpc";

/// Longest failure message written to a log line, in characters.
const LOG_MESSAGE_CHARS: usize = 300;

/// Log a failure: server errors at error, every refusal at warn.
/// One line per
/// failed call; a successful call is not logged. The message is the one the client already
/// received (database errors concealed), cut and written escaped so it cannot split the line.
fn log_failure(failure: &RpcFailure, call: &FailureLog<'_>) {
    // the hook's code and kind are for the operator; its message may name proposal details and
    // a concealed refusal's body says nothing of it, so the message is never logged
    if let RpcFailure::Hook(refusal) = failure {
        let kind = match refusal.kind() {
            RefusalKind::Concealed => "concealed",
            RefusalKind::Refused => "refused",
        };
        tracing::warn!(
            target: "erp::rpc",
            status = failure.status(),
            hook_code = refusal.code(),
            hook_kind = kind,
            model = call.model,
            method = call.method,
            uid = call.uid,
            addon = call.addon,
            request_id = call.request_id,
            "rpc call refused by its attempt hook"
        );
        return;
    }
    let message: String = failure.message().chars().take(LOG_MESSAGE_CHARS).collect();
    if failure.status() >= 500 {
        let detail = failure
            .domain_error()
            .map(|e| format!("{e:?}"))
            .unwrap_or_default();
        tracing::error!(
            target: "erp::rpc",
            status = failure.status(),
            name = failure.name(),
            model = call.model,
            method = call.method,
            uid = call.uid,
            addon = call.addon,
            request_id = call.request_id,
            reason = ?message,
            detail,
            "rpc call failed"
        );
    } else {
        tracing::warn!(
            target: "erp::rpc",
            status = failure.status(),
            name = failure.name(),
            model = call.model,
            method = call.method,
            uid = call.uid,
            addon = call.addon,
            request_id = call.request_id,
            reason = ?message,
            "rpc call refused"
        );
    }
}
