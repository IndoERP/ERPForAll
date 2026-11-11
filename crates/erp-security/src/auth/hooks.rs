//! The session-end hook (lead note T-PUSH.8): modules that keep
//! rows per session (push subscriptions, presence, device records) subscribe, and every path that
//! deletes `erp_session` rows (logout, revocation, archiving or deleting users, a failed check,
//! expiry collection) runs the hooks of the installed addons **in the transaction that deletes
//! the rows**, so a subscriber's rows go with the session atomically. A failing hook fails the
//! deletion. Hooks use raw statements only (`query`, `execute`, `batch`), on every path: expiry
//! collection retains its READ COMMITTED record-API refusal, and a scoped restriction enforces
//! the same contract for callers with REPEATABLE READ transactions. The scope restores the
//! caller's previous restriction even if a hook errors or its future is cancelled.
//!
//! ```ignore
//! erp_sdk::auth::session_end_hook!(DROP_MY_ROWS, addon = "my_addon", run = drop_my_rows);
//!
//! fn drop_my_rows<'a>(tx: &'a mut Transaction, ended: &'a SessionEnd) -> BoxFuture<'a, Result<()>> {
//!     Box::pin(async move {
//!         let ids: Vec<String> = ended.sessions().iter().map(|s| s.id().to_text()).collect();
//!         tx.execute("DELETE FROM my_rows WHERE session_id = ANY($1::text[]::uuid[])", &[&ids])
//!             .await?;
//!         Ok(())
//!     })
//! }
//! ```

use erp_fields::Result;
use erp_orm::{BoxFuture, Transaction, UserId};

use super::session::SessionId;

#[doc(hidden)]
pub use linkme as __linkme;

/// Why sessions ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndReason {
    /// The user logged out.
    Logout,
    /// Revoked: by the user or an administrator, or because the user was archived or deleted.
    Revoked,
    /// Idle or past its lifetime: collected, or deleted after a failed check.
    Expired,
    /// The user's stored password changed: set through `res.users`, or rehashed by a login (the
    /// sessions' tokens covered the old hash).
    CredentialChanged,
    /// A login took the user past the sessions a user may keep (the oldest end).
    OverCap,
}

/// One ended session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndedSession {
    id: SessionId,
    user: Option<UserId>,
}

impl EndedSession {
    pub(crate) fn new(id: SessionId, user: Option<UserId>) -> Self {
        Self { id, user }
    }

    /// The session row's id.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Its user (`None` only for a row whose `user_id` held 0, which no ORM write produces).
    pub fn user(&self) -> Option<UserId> {
        self.user
    }
}

/// What the hooks receive.
#[derive(Debug, Clone)]
pub struct SessionEnd {
    reason: EndReason,
    sessions: Vec<EndedSession>,
}

impl SessionEnd {
    pub(crate) fn new(reason: EndReason, sessions: Vec<EndedSession>) -> Self {
        Self { reason, sessions }
    }

    /// Why they ended.
    pub fn reason(&self) -> EndReason {
        self.reason
    }

    /// The ended sessions (never empty when a hook runs).
    pub fn sessions(&self) -> &[EndedSession] {
        &self.sessions
    }
}

/// A hook's function. It uses raw statements only (`query`, `execute`, `batch`), never the record
/// API: every invocation is restricted regardless of the caller's isolation (module doc).
pub type SessionEndFn =
    for<'a> fn(&'a mut Transaction, &'a SessionEnd) -> BoxFuture<'a, Result<()>>;

/// A registered hook ([`session_end_hook!`](crate::session_end_hook)).
#[derive(Debug)]
pub struct SessionEndHook {
    /// The addon that registers it: the hook runs only in databases where it is installed.
    pub addon: &'static str,
    /// Its name (order within one addon; diagnostics).
    pub name: &'static str,
    /// The function.
    pub run: SessionEndFn,
}

/// Every hook linked into the binary.
#[linkme::distributed_slice]
pub static SESSION_END_HOOKS: [SessionEndHook];

/// Register a session-end hook: `session_end_hook!(IDENT, addon = "my_addon", run = my_fn)`.
#[macro_export]
macro_rules! session_end_hook {
    ($ident:ident, addon = $addon:literal, run = $run:path) => {
        #[$crate::auth::hooks::__linkme::distributed_slice($crate::auth::hooks::SESSION_END_HOOKS)]
        #[linkme(crate = $crate::auth::hooks::__linkme)]
        static $ident: $crate::auth::hooks::SessionEndHook = $crate::auth::hooks::SessionEndHook {
            addon: $addon,
            name: stringify!($ident),
            run: $run,
        };
    };
}

/// Run the hooks of the addons installed in `tx`'s registry, in addon load order and by name
/// within an addon.
///
/// # Errors
///
/// The first hook error (the caller's transaction then fails, and the rows stay).
pub(crate) async fn run(tx: &mut Transaction, ended: &SessionEnd) -> Result<()> {
    if ended.sessions.is_empty() {
        return Ok(());
    }
    let registry = std::sync::Arc::clone(tx.env().registry());
    let mut raw = tx.raw_statements_only();
    for addon in registry.addons() {
        let mut hooks: Vec<&SessionEndHook> = SESSION_END_HOOKS
            .iter()
            .filter(|h| h.addon == addon.name)
            .collect();
        hooks.sort_by_key(|h| h.name);
        for hook in hooks {
            (hook.run)(&mut raw, ended).await?;
        }
    }
    Ok(())
}
