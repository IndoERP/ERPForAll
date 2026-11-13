//! Emergency revocation of API keys and sessions on the owner connection.
//!
//! Ordinary revocation commits with its audit row and fails when the audit store is unavailable.
//! This module is the one exception to that fail-closed rule, and it is narrow: it runs only on
//! the owner connection (the role that owns the credential tables and the audit table), as the
//! core System principal, and it still writes its own audit row in the same transaction. The row
//! carries a closed reason, ids and counts, never a key, prefix, name or login. There is no
//! unaudited revocation.
//!
//! The caller (the `erp auth emergency-revoke` command) opens a read-committed superuser
//! transaction on the primary with an `OperationIdentity`, calls [`revoke`], and commits. When
//! [`revoke`] returns an error the transaction must be rolled back.
//!
//! The owner can rewrite its own audit rows. Only an external witness of the table detects that;
//! the limit is stated.

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::audit::{
    ApiKeyRevoked, AuditOrigin, AuditStoreState, AuthEvent, EmergencyReason, EmergencyRevocation,
    KeyRouteScope, RevokeCause, SessionCause, SessionRevoked, UserSessionsRevoked,
};
use erp_orm::{ApiKeyId, KeyKind as ActorKeyKind, PrincipalKind, Transaction, UserId};
use tokio_postgres::Row;

use super::apikey_store::{KeyKind, KeyScope};
use super::hooks::{EndReason, EndedSession};
use super::session::SessionId;
use super::store::{end_sessions, end_user_sessions};

/// Most keys or sessions one call names, and most keys one chunk of a user's keys ends. One audit
/// row holds at most 512 events ([`erp_orm::audit::MAX_EVENTS`]): a header, up to this many key
/// events and one user-sessions event fit.
pub const MAX_TARGETS: usize = 500;

/// The tables the deleting statements resolve by their unqualified names (`public` in a
/// deployment), and the schema each must resolve to.
const UNQUALIFIED_TABLES: [&str; 2] = ["res_users_apikeys", "erp_session"];

/// The schema the unqualified tables live in.
const TABLE_SCHEMA: &str = "public";

/// The audit table, resolved by catalog names (its schema need not be on the search path).
const AUDIT_SCHEMA: &str = "erp_event";
const AUDIT_TABLE: &str = "ir_audit_entry";

/// Who or what an emergency revocation ends.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Target {
    /// These API keys, by id. At most [`MAX_TARGETS`]; every id must exist.
    Keys(Vec<ApiKeyId>),
    /// These sessions, by id. At most [`MAX_TARGETS`]; every id must exist.
    Sessions(Vec<SessionId>),
    /// A user's credentials: up to [`MAX_TARGETS`] of its keys (call again while
    /// [`Revoked::key_chunk_was_full`]) and/or all of its sessions.
    User {
        /// The user.
        user: UserId,
        /// End the user's API keys, one chunk per call.
        keys: bool,
        /// End all the user's sessions.
        sessions: bool,
    },
}

impl Target {
    /// Named API keys.
    ///
    /// # Errors
    ///
    /// `Validation` for none or more than [`MAX_TARGETS`].
    pub fn keys(ids: Vec<ApiKeyId>) -> Result<Self> {
        let target = Self::Keys(ids);
        target.check()?;
        Ok(target)
    }

    /// Named sessions.
    ///
    /// # Errors
    ///
    /// `Validation` for none or more than [`MAX_TARGETS`].
    pub fn sessions(ids: Vec<SessionId>) -> Result<Self> {
        let target = Self::Sessions(ids);
        target.check()?;
        Ok(target)
    }

    /// The keys and/or sessions of one user.
    ///
    /// # Errors
    ///
    /// `Validation` when neither `keys` nor `sessions` is set.
    pub fn user(user: UserId, keys: bool, sessions: bool) -> Result<Self> {
        let target = Self::User {
            user,
            keys,
            sessions,
        };
        target.check()?;
        Ok(target)
    }

    fn check(&self) -> Result<()> {
        match self {
            Self::Keys(ids) => count_in_bounds("API key ids", ids.len()),
            Self::Sessions(ids) => count_in_bounds("session ids", ids.len()),
            Self::User {
                keys: false,
                sessions: false,
                ..
            } => Err(Error::validation(
                "a later release: a user target needs its keys, its sessions or both",
            )),
            Self::User { .. } => Ok(()),
        }
    }
}

fn count_in_bounds(what: &str, len: usize) -> Result<()> {
    if len == 0 || len > MAX_TARGETS {
        return Err(Error::validation(format!(
            "a later release: name 1 to {MAX_TARGETS} {what} per run, got {len}"
        )));
    }
    Ok(())
}

/// What one [`revoke`] call ended. Ids only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Revoked {
    keys: Vec<ApiKeyId>,
    sessions: Vec<SessionId>,
}

impl Revoked {
    /// The ended API keys, ascending.
    pub fn keys(&self) -> &[ApiKeyId] {
        &self.keys
    }

    /// The ended sessions.
    pub fn sessions(&self) -> &[SessionId] {
        &self.sessions
    }

    /// Whether a user's key chunk reached [`MAX_TARGETS`], so more keys of that user may remain
    /// and the caller commits this chunk and calls [`revoke`] again for the keys. Meaningful for
    /// [`Target::User`] with keys only: a [`Target::Keys`] call that names exactly
    /// [`MAX_TARGETS`] ids also returns `true`, and nothing remains to continue.
    pub fn key_chunk_was_full(&self) -> bool {
        self.keys.len() == MAX_TARGETS
    }
}

/// One deleted key, as the audit event needs it.
struct DeletedKey {
    id: ApiKeyId,
    owner: UserId,
    kind: ActorKeyKind,
    route: KeyRouteScope,
}

fn column<T: tokio_postgres::types::FromSqlOwned>(row: &Row, i: usize) -> Result<T> {
    row.try_get(i)
        .map_err(|e| Error::db(format!("a later release column {i}: {e}")).with_source(e))
}

/// Refuse unless the audit store can take the revocation's row on the connection that checked it.
/// A store that is not installed, has another shape or has no partition day left blocks the owner
/// too; `NoInsertRight` concerns the runtime role and does not occur on the owner connection.
///
/// # Errors
///
/// `Internal` naming the store's state, before anything was revoked.
pub fn require_audit_store(state: &AuditStoreState) -> Result<()> {
    if state.allows_writes() {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::Internal,
        format!(
            "a later release: the audit store cannot take the revocation's row on the \
             owner connection ({}); nothing was revoked. Run 'erp audit maintain' first; there is \
             no unaudited revocation",
            state.describe()
        ),
    ))
}

fn not_owner(table: &str) -> Error {
    Error::access(format!(
        "a later release: the configured owner connection does not own {table}; \
         emergency revocation runs only on the owner connection"
    ))
}

/// Require that `current_user` owns the credential tables and the audit table (strict owner
/// equality: a superuser that is not the owner is refused). The runtime role never passes.
///
/// The deleting statements (this module's and the session-ending ones of [`super::store`]) name
/// `res_users_apikeys` and `erp_session` without a schema, so the check resolves those two names
/// the same way (`to_regclass` on the unqualified name, through the connection's search path) and
/// refuses when a name resolves to a table outside `public`. The audit table is found by its
/// catalog names, because its schema is not on the search path.
///
/// # Errors
///
/// `Access` naming the first table the connected role does not own, or an unqualified name that
/// does not resolve to the `public` table; `Programming` when the catalog answers with the wrong
/// number of rows.
pub async fn require_owner(tx: &mut Transaction) -> Result<()> {
    for table in UNQUALIFIED_TABLES {
        let rows = tx
            .query(
                "SELECT n.nspname::text, pg_get_userbyid(c.relowner) = current_user \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.oid = to_regclass($1::text)",
                &[&table],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::access(format!(
                "a later release: '{table}' does not resolve to a table on this \
                 connection; emergency revocation runs only on the owner connection of the \
                 server's database"
            )));
        };
        let schema: String = column(row, 0)?;
        let owned: bool = column(row, 1)?;
        if schema != TABLE_SCHEMA {
            return Err(Error::access(format!(
                "a later release: '{table}' resolves to the schema {schema}, not \
                 {TABLE_SCHEMA}, on this connection; emergency revocation runs only on the \
                 owner connection of the server's database"
            )));
        }
        if !owned {
            return Err(not_owner(&format!("{TABLE_SCHEMA}.{table}")));
        }
    }
    let rows = tx
        .query(
            "SELECT pg_get_userbyid(c.relowner) = current_user \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&AUDIT_SCHEMA, &AUDIT_TABLE],
        )
        .await?;
    if rows.len() > 1 {
        return Err(Error::programming(
            "a later release: the ownership check found the audit table twice",
        ));
    }
    // no row: the table is absent, so this connection cannot own it
    let owned = match rows.first() {
        Some(row) => column::<bool>(row, 0)?,
        None => false,
    };
    if !owned {
        return Err(not_owner(&format!("{AUDIT_SCHEMA}.{AUDIT_TABLE}")));
    }
    Ok(())
}

fn key_kind(text: &str) -> Result<ActorKeyKind> {
    Ok(match KeyKind::parse(text)? {
        KeyKind::User => ActorKeyKind::User,
        KeyKind::Agent => ActorKeyKind::Agent,
        KeyKind::Service => ActorKeyKind::Service,
    })
}

fn key_route(text: &str) -> Result<KeyRouteScope> {
    Ok(match KeyScope::parse(text)? {
        KeyScope::Rpc => KeyRouteScope::Rpc,
        KeyScope::Mcp => KeyRouteScope::Mcp,
    })
}

fn deleted_keys(rows: &[Row]) -> Result<Vec<DeletedKey>> {
    let mut keys = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = column(row, 0)?;
        let owner: i64 = column(row, 1)?;
        let kind: String = column(row, 2)?;
        let route: String = column(row, 3)?;
        keys.push(DeletedKey {
            id: ApiKeyId::new(id).map_err(|e| {
                Error::programming("a later release deleted an invalid key id").with_source(e)
            })?,
            owner: UserId::new(owner).map_err(|e| {
                Error::programming("a later release deleted a key of an invalid user id")
                    .with_source(e)
            })?,
            kind: key_kind(&kind)?,
            route: key_route(&route)?,
        });
    }
    keys.sort_unstable_by_key(|k| k.id.get());
    Ok(keys)
}

fn id_list(ids: &[String]) -> String {
    ids.join(", ")
}

fn missing(what: &str, ids: &[String]) -> Error {
    Error::new(
        ErrorKind::Missing,
        format!(
            "a later release: no {what} with the id(s) {}; nothing was revoked",
            id_list(ids)
        ),
    )
}

async fn delete_keys_by_id(tx: &mut Transaction, ids: &[ApiKeyId]) -> Result<Vec<DeletedKey>> {
    let mut wanted: Vec<i64> = ids.iter().map(|id| id.get()).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let rows = tx
        .query(
            "DELETE FROM res_users_apikeys WHERE id = ANY($1::int8[]) \
             RETURNING id::int8, user_id::int8, kind, scope",
            &[&wanted],
        )
        .await?;
    let deleted = deleted_keys(&rows)?;
    let absent: Vec<String> = wanted
        .iter()
        .filter(|id| !deleted.iter().any(|k| k.id.get() == **id))
        .map(i64::to_string)
        .collect();
    if !absent.is_empty() {
        return Err(missing("API key", &absent));
    }
    Ok(deleted)
}

async fn delete_user_keys_chunk(tx: &mut Transaction, user: UserId) -> Result<Vec<DeletedKey>> {
    let limit = i64::try_from(MAX_TARGETS)
        .map_err(|_| Error::programming("a later release: chunk size overflow"))?;
    let rows = tx
        .query(
            "DELETE FROM res_users_apikeys WHERE id IN ( \
               SELECT id FROM res_users_apikeys WHERE user_id = $1 ORDER BY id LIMIT $2 \
               FOR UPDATE) \
             RETURNING id::int8, user_id::int8, kind, scope",
            &[&user.get(), &limit],
        )
        .await?;
    deleted_keys(&rows)
}

/// Lock the user's row the way issuance does (subject row first, then the key rows), so a key
/// issued for the user at the same time serializes with this revocation: the issuance either
/// commits first and its key is deleted here, or waits for this transaction. An archived user is
/// found too (offboarding ends the credentials of archived users). The write is a no-op update.
async fn require_user(tx: &mut Transaction, user: UserId) -> Result<()> {
    let rows = tx
        .query(
            "UPDATE res_users SET id = id WHERE id = $1 RETURNING id",
            &[&user.get()],
        )
        .await?;
    if rows.is_empty() {
        return Err(missing("user", &[user.get().to_string()]));
    }
    Ok(())
}

fn count(what: &str, len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|e| {
        Error::programming(format!("a later release: too many {what} to count")).with_source(e)
    })
}

/// Revoke `target` for `reason` in `tx` and buffer the audit row's events. The caller commits;
/// the row is written by the commit preamble in the same transaction as the deletions.
///
/// Order of checks, each before any deletion: the principal, then [`require_audit_store`] (on the
/// state the transaction's ORM last checked), then [`require_owner`], then the target's bounds.
/// Session deletion goes through [`end_sessions`] / [`end_user_sessions`], so the session-end hooks
/// of the installed addons that are linked into this binary run in the same transaction. A user
/// with nothing to revoke succeeds with an empty [`Revoked`] and no audit row.
///
/// # Errors
///
/// `Access` for a principal that is not the core System principal without an actor, and for a
/// connection that does not own the tables ([`require_owner`]); `Internal` when the audit store
/// cannot take the row ([`require_audit_store`]); `Validation` for a target outside its bounds;
/// `Missing` for a key, session or user that does not exist (nothing of the call may be
/// committed: roll the transaction back); the database's and the hooks' errors.
pub async fn revoke(
    tx: &mut Transaction,
    target: &Target,
    reason: EmergencyReason,
) -> Result<Revoked> {
    let env = tx.env();
    if env.principal().kind() != PrincipalKind::System || env.principal().actor().is_some() {
        return Err(Error::access(
            "a later release: emergency revocation requires the core System principal",
        ));
    }
    require_audit_store(&tx.audit_store_state())?;
    require_owner(tx).await?;
    target.check()?;
    let mut keys: Vec<DeletedKey> = Vec::new();
    let mut ended: Vec<EndedSession> = Vec::new();
    let mut whole_user: Option<UserId> = None;
    match target {
        Target::Keys(ids) => keys = delete_keys_by_id(tx, ids).await?,
        Target::Sessions(ids) => {
            ended = end_sessions(tx, ids, EndReason::Revoked).await?;
            let mut wanted: Vec<String> = ids.iter().map(SessionId::to_text).collect();
            wanted.sort_unstable();
            wanted.dedup();
            let absent: Vec<String> = wanted
                .into_iter()
                .filter(|id| !ended.iter().any(|s| s.id().to_text() == *id))
                .collect();
            if !absent.is_empty() {
                return Err(missing("session", &absent));
            }
        }
        Target::User {
            user,
            keys: with_keys,
            sessions: with_sessions,
        } => {
            require_user(tx, *user).await?;
            // sessions first: the audited events do not depend on the order of the deletions
            if *with_sessions {
                ended = end_user_sessions(tx, &[user.get()], EndReason::Revoked).await?;
                whole_user = Some(*user);
            }
            if *with_keys {
                keys = delete_user_keys_chunk(tx, *user).await?;
            }
        }
    }
    if keys.is_empty() && ended.is_empty() {
        return Ok(Revoked {
            keys: Vec::new(),
            sessions: Vec::new(),
        });
    }
    if !keys.is_empty() {
        // The CLI's Orm has no signal configured, so this publishes no cache bump to other nodes.
        // That is harmless today: key verification caches no acceptance
        // (`crates/erp-security/src/auth/apikey_verify.rs:4`), so a deleted key is refused on the
        // next request on every node.
        tx.invalidate_ormcache("apikeys");
    }
    record(tx, reason, &keys, &ended, whole_user)?;
    Ok(Revoked {
        keys: keys.iter().map(|k| k.id).collect(),
        sessions: ended.iter().map(EndedSession::id).collect(),
    })
}

/// Buffer the header and one event per ended credential (one event for all of a user's sessions).
fn record(
    tx: &mut Transaction,
    reason: EmergencyReason,
    keys: &[DeletedKey],
    ended: &[EndedSession],
    whole_user: Option<UserId>,
) -> Result<()> {
    tx.audit_origin(AuditOrigin::Cli)?;
    tx.audit_auth(AuthEvent::EmergencyRevocation(EmergencyRevocation::new(
        reason,
        count("keys", keys.len())?,
        count("sessions", ended.len())?,
    )))?;
    for key in keys {
        tx.audit_auth(AuthEvent::ApiKeyRevoked(ApiKeyRevoked::new(
            key.id,
            key.owner,
            key.kind,
            key.route,
            RevokeCause::Emergency,
        )))?;
    }
    match whole_user {
        Some(user) => {
            if !ended.is_empty() {
                tx.audit_auth(AuthEvent::UserSessionsRevoked(UserSessionsRevoked::new(
                    user,
                    SessionCause::Emergency,
                    count("sessions", ended.len())?,
                )))?;
            }
        }
        None => {
            for session in ended {
                let user = session.user().ok_or_else(|| {
                    Error::programming("a later release: an ended session has no valid user id")
                })?;
                tx.audit_auth(AuthEvent::SessionRevoked(SessionRevoked::new(
                    erp_fields::Uuid::from_bytes(*session.id().bytes()),
                    user,
                    SessionCause::Emergency,
                )))?;
            }
        }
    }
    Ok(())
}
