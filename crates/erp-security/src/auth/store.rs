//! Ending sessions: every deletion of `erp_session` rows goes
//! through [`end_with`], which runs one of the fixed statements below (`DELETE … RETURNING`) and
//! the session-end hooks in the same transaction.

use erp_fields::{Error, Result};
use erp_orm::{Transaction, UserId};
use tokio_postgres::types::ToSql;

use super::hooks::{self, EndReason, EndedSession, SessionEnd};
use super::session::SessionId;

/// Rows a collection batch deletes at most.
pub const GC_BATCH: u32 = 1_000;

/// How long a `pending` (second-factor) row lives, seconds (§4.3). The table's
/// `erp_session_pending_expiry` CHECK holds every pending row's `expires_at` within it.
pub const PENDING_LIFETIME_SECS: f64 = 600.0;

/// A statement that deletes `erp_session` rows and returns `(id::text, user_id::int8)`: one
/// constant per entry point, never assembled at run time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EndStatement(&'static str);

impl EndStatement {
    /// `sql` must be a `DELETE FROM erp_session … RETURNING id::text, user_id::int8`.
    pub(crate) const fn new(sql: &'static str) -> Self {
        Self(sql)
    }
}

/// Logout and revocation of given sessions; the ids are sorted, so concurrent deletions lock the
/// rows in one order.
const END_SESSIONS: EndStatement = EndStatement::new(
    "DELETE FROM erp_session WHERE id = ANY($1::text[]::uuid[]) \
     RETURNING id::text, user_id::int8",
);

/// An asynchronous refusal may refer to a row refreshed since the credential read. PostgreSQL
/// rechecks this version predicate after any concurrent updater releases its row lock.
pub(crate) const END_OBSERVED_SESSION: EndStatement = EndStatement::new(
    "DELETE FROM erp_session WHERE id = $1::text::uuid AND xmin::text::int8 = $2 \
     RETURNING id::text, user_id::int8",
);

/// Every session of given users (archive, delete); the ids are sorted.
const END_USER_SESSIONS: EndStatement = EndStatement::new(
    "DELETE FROM erp_session WHERE user_id = ANY($1::int8[]) \
     RETURNING id::text, user_id::int8",
);

/// Run `statement` with `params` and the hooks for the rows it deleted.
pub(crate) async fn end_with(
    tx: &mut Transaction,
    statement: EndStatement,
    params: &[&(dyn ToSql + Sync)],
    reason: EndReason,
) -> Result<Vec<EndedSession>> {
    // also keeps the table's registration in every binary that ends sessions (`base` does, on
    // archive and delete, without an `Auth`)
    if !super::schema::registered() {
        return Err(Error::programming(
            "erp_session is not registered as a core table in this binary",
        ));
    }
    let rows = tx.query(statement.0, params).await?;
    let mut ended = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: String = row
            .try_get(0)
            .map_err(|e| Error::db(format!("erp_session.id: {e}")).with_source(e))?;
        let user: i64 = row
            .try_get(1)
            .map_err(|e| Error::db(format!("erp_session.user_id: {e}")).with_source(e))?;
        let user = match UserId::new(user) {
            Ok(user) => Some(user),
            Err(e) => {
                // a row no user can own (review item 6: the session still ends, the hooks get
                // no user); never expected, so it is logged
                tracing::warn!(
                    session = %id,
                    user_id = user,
                    error = %e,
                    "an ended session has a user id that is not a user"
                );
                None
            }
        };
        ended.push(EndedSession::new(SessionId::parse(&id)?, user));
    }
    if !ended.is_empty() {
        hooks::run(tx, &SessionEnd::new(reason, ended.clone())).await?;
    }
    Ok(ended)
}

/// End the sessions `ids` in `tx` (logout, revocation), with the hooks.
///
/// # Errors
///
/// The statement's and the hooks' errors.
pub async fn end_sessions(
    tx: &mut Transaction,
    ids: &[SessionId],
    reason: EndReason,
) -> Result<Vec<EndedSession>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // `SessionId` orders as PostgreSQL orders `uuid` (bytewise)
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let texts: Vec<String> = sorted.iter().map(SessionId::to_text).collect();
    end_with(tx, END_SESSIONS, &[&texts], reason).await
}

/// End every session of `users` in `tx` (archive or delete of users), with the hooks.
///
/// It runs in the caller's transaction (REPEATABLE READ): a concurrent touch of one of the rows
/// makes it fail with `40001`, and the whole call is retried (`erp_orm::Orm::run`, the RPC
/// service's retry of a call on `40001`/`40P01`). The deletion is cleanup, not the boundary: the
/// session token covers `active` and the password hash, so a session of an archived user fails
/// its next check anyway.
///
/// # Errors
///
/// The statement's and the hooks' errors.
pub async fn end_user_sessions(
    tx: &mut Transaction,
    users: &[i64],
    reason: EndReason,
) -> Result<Vec<EndedSession>> {
    if users.is_empty() {
        return Ok(Vec::new());
    }
    let mut sorted = users.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    end_with(tx, END_USER_SESSIONS, &[&sorted], reason).await
}
