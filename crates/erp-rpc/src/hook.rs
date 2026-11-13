// SPDX-License-Identifier: MIT OR Apache-2.0
//! The attempt hook (AI1.RPC-PRECONDITION, "Seam"): server-side Rust code attaches
//! an [`AttemptHook`] to one [`crate::RpcCall`]; the service runs its two phases inside every
//! execution attempt of that call, on the attempt's own transaction. No request body, context key
//! or header can attach, select or skip it.

use std::fmt;

use erp_fields::Error;
use erp_orm::audit::AuditOrigin;
use erp_orm::rpc::Effect;
use erp_orm::{BoxFuture, Transaction};

use crate::idempotency::{CorrelationId, IdempotencyKey};

/// The message of a [`RefusalKind::Concealed`] refusal: the response is an access error with this
/// one fixed text, the same answer the confirm endpoint gives when the user, session or digest of
/// a proposal does not match (endpoint check). The caller cannot tell those two
/// apart; the text is not meant to match every ORM access denial, whose texts name models.
pub const CONCEALED_ACCESS_MESSAGE: &str =
    "You are not allowed to access this document or to perform this operation.";

/// A per-call hook run inside each execution attempt of a keyed write call.
///
/// The service refuses a hook on a method with a `read` effect or on a call without an
/// `Idempotency-Key` before any transaction. On every attempt that makes a new idempotency claim
/// it runs [`before_body`](Self::before_body) after the claim, the gates and the credential
/// re-check and before the method body, then [`after_body`](Self::after_body) after the final
/// flush and after the response is serialized. A replay of a committed key runs neither phase.
///
/// Contract for implementations:
/// - **No state across attempts.** A retried attempt starts from a rolled-back transaction; every
///   decision is made again from the database and the [`HookCall`] of that attempt.
/// - **Only refuse.** A phase returns `Ok(())` or an error; it cannot skip the claim, the gates,
///   access checks or the commit. Its writes go through the attempt's transaction and commit or
///   roll back with the body. A bookkeeping write needs a named, audited elevation on `tx`.
/// - The service, not the hook, sets the attempt's audit origin from [`origin`](Self::origin),
///   before the first phase and again after `after_body`.
/// - **Bookkeeping reads** go through the audited elevation, or an access failure is mapped to a
///   [`RefusalKind::Concealed`] refusal: a [`HookError::Database`] carrying an access error renders
///   the ORM's real access text, which names the model.
/// - The service reports a refusal to its server-side caller through
///   [`crate::RpcResponse::hook_refusal`]; the body bytes alone cannot tell a concealed refusal
///   from the confirm endpoint's own ownership-mismatch error.
pub trait AttemptHook: Send + Sync + fmt::Debug {
    /// The origin recorded on every audit row of the hooked attempt.
    fn origin(&self) -> HookOrigin;

    /// Runs after a new idempotency claim and before the method body.
    ///
    /// # Errors
    ///
    /// [`HookError::Refused`] ends the call with that refusal (never retried);
    /// [`HookError::Database`] goes through the ordinary retry classification.
    fn before_body<'a>(
        &'a self,
        tx: &'a mut Transaction,
        call: &'a HookCall<'a>,
    ) -> BoxFuture<'a, Result<(), HookError>>;

    /// Runs after the method body, the final flush and the serialization of the response, with
    /// the exact response bytes; the service flushes once more after it, before the commit.
    ///
    /// # Errors
    ///
    /// As [`before_body`](Self::before_body).
    fn after_body<'a>(
        &'a self,
        tx: &'a mut Transaction,
        call: &'a HookCall<'a>,
        body: &'a [u8],
    ) -> BoxFuture<'a, Result<(), HookError>>;
}

/// What a hook phase sees of the call (read-only; the principal is `tx.env().principal()`).
#[derive(Debug)]
#[non_exhaustive]
pub struct HookCall<'a> {
    /// The model.
    pub model: &'a str,
    /// The method.
    pub method: &'a str,
    /// The method's effect (never `read`).
    pub effect: Effect,
    /// The call's idempotency key (a hooked call always has one).
    pub idempotency_key: &'a IdempotencyKey,
    /// The execution attempt, from 1.
    pub attempt: u32,
    /// The call's correlation id, when the transport gave one.
    pub correlation: Option<&'a CorrelationId>,
}

/// The audit origins a hook may declare; core maps each to its [`AuditOrigin`], so a hook can
/// never claim an origin such as `auth`, `cli`, `lifecycle` or `job`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookOrigin {
    /// A call confirmed through the AI assistant.
    Assistant,
}

impl HookOrigin {
    /// The audit origin core records for this hook origin.
    pub const fn audit_origin(self) -> AuditOrigin {
        match self {
            Self::Assistant => AuditOrigin::Assistant,
        }
    }
}

/// How a [`HookRefusal`] is answered. Consumers match it exhaustively: a new kind must be handled
/// by every renderer and audit mapping, so this enum is deliberately not `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// Answered as an access error with one fixed message ([`CONCEALED_ACCESS_MESSAGE`]); the code
    /// and message stay on the server. Audited as `denied`.
    Concealed,
    /// Answered with status 409, the code as the error name and the message. Audited as `failed`
    /// with class `validation`.
    Refused,
}

/// A typed refusal returned by a hook phase: never retried, never recorded as a method failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRefusal {
    code: &'static str,
    message: String,
    kind: RefusalKind,
}

impl HookRefusal {
    /// A refusal with `code` (`erp.<segment>.<segment>`, each segment non-empty lowercase ASCII
    /// letters, digits or `_`, for example `erp.ai.proposal_expired`), `message` and `kind`.
    ///
    /// # Errors
    ///
    /// `Programming` when `code` does not have that form.
    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        kind: RefusalKind,
    ) -> erp_fields::Result<Self> {
        let mut segments = code.split('.');
        let well_formed = segments.next() == Some("erp")
            && segments.clone().count() == 2
            && segments.all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            });
        if !well_formed {
            return Err(Error::programming(format!(
                "a hook refusal code must be 'erp.<segment>.<segment>' in lowercase ASCII \
                 letters, digits and '_', got {code:?}"
            )));
        }
        Ok(Self {
            code,
            message: message.into(),
            kind,
        })
    }

    /// The code (the error name of a [`RefusalKind::Refused`] answer).
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The message (sent only for [`RefusalKind::Refused`]).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// How it is answered.
    pub fn kind(&self) -> RefusalKind {
        self.kind
    }
}

impl fmt::Display for HookRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HookRefusal {}

/// The error of a hook phase.
#[derive(Debug)]
#[non_exhaustive]
pub enum HookError {
    /// The hook refuses the call (never retried).
    Refused(HookRefusal),
    /// A database or ORM error of the hook's own work: a serialization failure or a superseded
    /// ORM retries the attempt, anything else is the call's error.
    Database(Error),
}

impl From<Error> for HookError {
    fn from(error: Error) -> Self {
        Self::Database(error)
    }
}

impl From<HookRefusal> for HookError {
    fn from(refusal: HookRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(f, "attempt hook refused the call: {refusal}"),
            Self::Database(error) => write!(f, "attempt hook failed: {error}"),
        }
    }
}

impl std::error::Error for HookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            Self::Database(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use erp_fields::ErrorKind;

    use super::*;

    /// The code validator accepts `erp.<segment>.<segment>` only.
    #[test]
    fn refusal_codes_are_validated() {
        for good in ["erp.ai.proposal_expired", "erp.trust.step_2"] {
            let refusal = HookRefusal::new(good, "text", RefusalKind::Refused).unwrap();
            assert_eq!(
                (refusal.code(), refusal.message(), refusal.kind()),
                (good, "text", RefusalKind::Refused)
            );
        }
        for bad in [
            "",
            "erp.ai",
            "erp.ai.proposal.expired",
            "app.ai.expired",
            "erp.AI.expired",
            "erp..expired",
            "erp.ai.",
            "erp.ai.expiré",
            "erp.ai.ex-pired",
            "erp.ai.ex pired",
        ] {
            let error = HookRefusal::new(bad, "text", RefusalKind::Concealed).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming, "{bad:?}");
            assert_eq!(
                error.message(),
                format!(
                    "a hook refusal code must be 'erp.<segment>.<segment>' in lowercase ASCII \
                     letters, digits and '_', got {bad:?}"
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn hook_origins_map_to_audit_origins() {
        assert_eq!(HookOrigin::Assistant.audit_origin(), AuditOrigin::Assistant);
        assert_eq!(AuditOrigin::Assistant.as_str(), "assistant");
    }
}
