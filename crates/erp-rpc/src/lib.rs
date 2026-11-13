//! ERPForAll JSON-2 core (layer L7).
//!
//! Transport-agnostic: `erp-server` parses HTTP and authenticates, then hands an
//! [`RpcCall`] to [`RpcService::call`], which returns an [`RpcResponse`] (status, body bytes,
//! replay flag). Tests drive the same service in-process.
//!
//! One call is one unit of work (design note §4):
//!
//! 1. **Route** ([`Route::parse_json2`]): `/json/2/<model>/<method>`, path over body.
//! 2. **Caller gate**: `agent`/`service` API keys are refused until; `public`/`link`
//!    callers have no idempotency key space.
//! 5. **Gates**: the API key's scope allowlist, `read_only` versus the method's effect, then the
//!    registered [`Gate`]s.
//! 7. **Transaction** of the right kind: a `read` method runs `READ ONLY` ([`erp_orm::TxIntent`]);
//!    a keyed write claims its [`IdempotencyKey`] first.
//! 8. **Result** serialized and size-checked before `COMMIT`; the keyed response is stored by an
//!    `UPDATE` pipelined with `COMMIT`.
//! 9. **Retry** of the whole call on `40001`/`40P01` or a superseded ORM (at most 5 attempts);
//!    a missing `COMMIT` answer is `erp.commit_unknown`, never retried.
//!
//! Server-side code may attach an [`AttemptHook`] to a keyed write call ([`RpcCall::attempt_hook`],
//! ): it runs inside every attempt after a new idempotency claim, before and after the
//! method body, can only refuse ([`HookRefusal`]) and sets the attempt's audit origin.
//!
//! Every failure is rendered by one error table ([`RpcFailure`]); `debug` stays empty unless the
//! operator switch `rpc.error_debug` is on and the subject holds [`RPC_DEBUG_ERRORS`].

mod audit_call;
mod binding;
mod config;
mod read_audit_policy;
pub use read_audit_policy::ReadAuditPolicy;
mod error;
mod hook;
mod idempotency;
mod output;
mod password_change;
mod pending;
pub use password_change::{PasswordCall, PasswordOperation, PasswordReply};
mod route;
mod service;

// The strict parser and canonical writer live in `erp_fields::json`; the modules
// of this crate keep using them through `crate::json`.
use erp_fields::json;

pub use binding::{
    CarriedContext, MAX_CONTEXT_KEY_BYTES, MAX_CONTEXT_KEYS, SERVER_ONLY_CONTEXT_KEYS,
    SERVER_ONLY_CONTEXT_PREFIX,
};
pub use config::{RpcConfig, RpcConfigBuilder, RpcLimits};
pub use error::{IdempotencyRefusal, InfraError, ProtocolError, RpcFailure};
pub use hook::{
    AttemptHook, CONCEALED_ACCESS_MESSAGE, HookCall, HookError, HookOrigin, HookRefusal,
    RefusalKind,
};
pub use idempotency::{CorrelationId, IDEMPOTENCY_DDL, IdempotencyKey, MAINTENANCE_BATCH};
pub use pending::PendingMethods;
pub use route::{MAX_NAME_LEN, Route, RouteKind};
pub use service::{
    Caller, Gate, GateCall, IN_PROGRESS_RETRY_AFTER, MAX_ATTEMPTS, OrmSource, RETRY_BASE,
    RETRY_CAP, RPC_DEBUG_ERRORS, RpcCall, RpcCredential, RpcHost, RpcResponse, RpcService,
};

/// The result writer for the allocation-budget test (`tests/alloc_json_rows.rs`).
#[doc(hidden)]
pub mod __output {
    /// The JSON-2 result body of `value` within `limit` bytes; the error as its debug text.
    ///
    /// # Errors
    /// The value is over the budget, nested too deep, or holds a non-finite float.
    pub fn serialize(value: &erp_fields::Value, limit: usize) -> Result<Vec<u8>, String> {
        crate::output::serialize(value, limit).map_err(|e| format!("{e:?}"))
    }
}

#[doc(hidden)]
pub mod __parity {
    /// Core's `BaseModel` names refused with 403.
    pub fn core_internal() -> &'static [&'static str] {
        &crate::service::CORE_INTERNAL
    }

    /// Core's pending table.
    pub fn core_pending() -> &'static [(&'static str, &'static str, &'static str)] {
        crate::pending::CORE_PENDING
    }
}
