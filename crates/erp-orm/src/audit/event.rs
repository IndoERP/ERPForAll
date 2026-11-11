//! The typed vocabulary of auth lifecycle events (the `events` array of a row).
//!
//! No constructor takes a key string, digest, display prefix, key name, password, hash, token,
//! cookie, login text, address, issuer subject or provider assertion (decision
//! L11). Every value is an id, a closed enum, a flag or a count, so none can carry a credential.

use erp_fields::{Result, Uuid};
use serde_json::{Value, json};

use super::MAX_COLLECTED_KEYS;
use crate::{ApiKeyId, KeyKind, UserId};

/// The route audience of an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyRouteScope {
    /// JSON-2 and RPC bearer routes.
    Rpc,
    /// MCP routes.
    Mcp,
}

impl KeyRouteScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Rpc => "rpc",
            Self::Mcp => "mcp",
        }
    }
}

const fn kind_str(kind: KeyKind) -> &'static str {
    match kind {
        KeyKind::User => "user",
        KeyKind::Agent => "agent",
        KeyKind::Service => "service",
    }
}

/// Why a key was revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RevokeCause {
    /// The key revoked itself.
    SelfRevoked,
    /// Its owner revoked it with another credential.
    Owner,
    /// An administrator revoked it.
    Administrator,
    /// A renewal replaced it.
    Renewed {
        /// The replacement key.
        replacement: ApiKeyId,
    },
    /// Another key revoked it programmatically.
    Programmatic {
        /// The revoking key.
        by_key: ApiKeyId,
    },
    /// Its owner was deactivated.
    UserDeactivated,
}

impl RevokeCause {
    fn to_json(self) -> Value {
        match self {
            Self::SelfRevoked => json!({"kind": "self"}),
            Self::Owner => json!({"kind": "owner"}),
            Self::Administrator => json!({"kind": "administrator"}),
            Self::Renewed { replacement } => {
                json!({"kind": "renewed", "replacement": replacement.get()})
            }
            Self::Programmatic { by_key } => {
                json!({"kind": "programmatic", "by_key": by_key.get()})
            }
            Self::UserDeactivated => json!({"kind": "user_deactivated"}),
        }
    }
}

/// Why sessions were revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionCause {
    /// The user asked (devices page).
    UserRequest,
    /// An administrator revoked them.
    Administrator,
    /// The password changed.
    PasswordChanged,
    /// The user was deactivated.
    UserDeactivated,
    /// The database secret rotated.
    SecretRotated,
}

impl SessionCause {
    const fn as_str(self) -> &'static str {
        match self {
            Self::UserRequest => "user_request",
            Self::Administrator => "administrator",
            Self::PasswordChanged => "password_changed",
            Self::UserDeactivated => "user_deactivated",
            Self::SecretRotated => "secret_rotated",
        }
    }
}

/// Who linked or unlinked an external identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkedBy {
    /// The user, through an authenticated session.
    User,
    /// An administrator.
    Administrator,
    /// First login provisioning.
    Provisioning,
}

impl LinkedBy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Administrator => "administrator",
            Self::Provisioning => "provisioning",
        }
    }
}

/// An API key was issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyIssued {
    key: ApiKeyId,
    owner: UserId,
    created_by: UserId,
    kind: KeyKind,
    route: KeyRouteScope,
    capabilities_subject: bool,
    read_only: bool,
    expires_utc_micros: Option<i64>,
    model_allowlist_len: Option<u16>,
    method_allowlist_len: Option<u16>,
}

impl ApiKeyIssued {
    /// A key's issuance. `capabilities_subject` is whether it inherits its subject's
    /// capabilities; the allowlist lengths are counts, never the names.
    #[allow(
        clippy::too_many_arguments,
        reason = "one flat typed record of the issuance"
    )]
    pub fn new(
        key: ApiKeyId,
        owner: UserId,
        created_by: UserId,
        kind: KeyKind,
        route: KeyRouteScope,
        capabilities_subject: bool,
        read_only: bool,
        expires_utc_micros: Option<i64>,
        model_allowlist_len: Option<u16>,
        method_allowlist_len: Option<u16>,
    ) -> Self {
        Self {
            key,
            owner,
            created_by,
            kind,
            route,
            capabilities_subject,
            read_only,
            expires_utc_micros,
            model_allowlist_len,
            method_allowlist_len,
        }
    }
}

/// An API key was revoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRevoked {
    key: ApiKeyId,
    owner: UserId,
    kind: KeyKind,
    route: KeyRouteScope,
    cause: RevokeCause,
}

impl ApiKeyRevoked {
    /// A key's revocation.
    pub fn new(
        key: ApiKeyId,
        owner: UserId,
        kind: KeyKind,
        route: KeyRouteScope,
        cause: RevokeCause,
    ) -> Self {
        Self {
            key,
            owner,
            kind,
            route,
            cause,
        }
    }
}

/// Expired keys were collected (one event per collected chunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeysCollected {
    keys: Vec<ApiKeyId>,
}

impl ApiKeysCollected {
    /// The collected key ids, `1..=`[`MAX_COLLECTED_KEYS`] of them.
    ///
    /// # Errors
    ///
    /// `Validation` for none or more than [`MAX_COLLECTED_KEYS`].
    pub fn new(keys: Vec<ApiKeyId>) -> Result<Self> {
        if keys.is_empty() || keys.len() > MAX_COLLECTED_KEYS {
            return Err(erp_fields::Error::validation(format!(
                "audit: a collection event names 1 to {MAX_COLLECTED_KEYS} keys, got {}",
                keys.len()
            )));
        }
        Ok(Self { keys })
    }
}

/// One session was revoked explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRevoked {
    session: Uuid,
    user: UserId,
    cause: SessionCause,
}

impl SessionRevoked {
    /// A session's revocation; `session` is the server-side session id, never the cookie.
    pub fn new(session: Uuid, user: UserId, cause: SessionCause) -> Self {
        Self {
            session,
            user,
            cause,
        }
    }
}

/// All sessions of a user were revoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSessionsRevoked {
    user: UserId,
    cause: SessionCause,
    count: u32,
}

impl UserSessionsRevoked {
    /// The revocation of `count` sessions of `user`.
    pub fn new(user: UserId, cause: SessionCause, count: u32) -> Self {
        Self { user, cause, count }
    }
}

/// A password was changed (the password itself is never recorded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordChanged {
    user: UserId,
    by: UserId,
}

impl PasswordChanged {
    /// `user`'s password changed by `by`.
    pub fn new(user: UserId, by: UserId) -> Self {
        Self { user, by }
    }
}

/// An external identity row was linked to or unlinked from a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityLink {
    identity_row: i64,
    user: UserId,
    provider_id: i64,
    by: LinkedBy,
}

impl IdentityLink {
    /// Row ids only: never the issuer subject, login or assertion.
    pub fn new(identity_row: i64, user: UserId, provider_id: i64, by: LinkedBy) -> Self {
        Self {
            identity_row,
            user,
            provider_id,
            by,
        }
    }
}

/// The database secret rotated to a new key id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRotated {
    new_kid: i16,
}

impl SecretRotated {
    /// The key id of the new secret (never the secret).
    pub fn new(new_kid: i16) -> Self {
        Self { new_kid }
    }
}

/// A typed auth lifecycle event, recorded with
/// [`Transaction::audit_auth`](crate::Transaction::audit_auth).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthEvent {
    /// A key was issued.
    ApiKeyIssued(ApiKeyIssued),
    /// A key was revoked.
    ApiKeyRevoked(ApiKeyRevoked),
    /// Expired keys were collected.
    ApiKeysCollected(ApiKeysCollected),
    /// One session was revoked.
    SessionRevoked(SessionRevoked),
    /// All sessions of a user were revoked.
    UserSessionsRevoked(UserSessionsRevoked),
    /// A password changed.
    PasswordChanged(PasswordChanged),
    /// An identity was linked.
    IdentityLinked(IdentityLink),
    /// An identity was unlinked.
    IdentityUnlinked(IdentityLink),
    /// The database secret rotated.
    DatabaseSecretRotated(SecretRotated),
}

impl AuthEvent {
    /// The event as the JSON object stored in the `events` array: stable keys, integers only.
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::ApiKeyIssued(e) => json!({
                "type": "auth", "event": "api_key.issued",
                "key_id": e.key.get(), "owner": e.owner.get(), "created_by": e.created_by.get(),
                "key_kind": kind_str(e.kind), "route": e.route.as_str(),
                "capabilities_subject": e.capabilities_subject, "read_only": e.read_only,
                "expires_utc_micros": e.expires_utc_micros,
                "model_allowlist_len": e.model_allowlist_len,
                "method_allowlist_len": e.method_allowlist_len,
            }),
            Self::ApiKeyRevoked(e) => json!({
                "type": "auth", "event": "api_key.revoked",
                "key_id": e.key.get(), "owner": e.owner.get(),
                "key_kind": kind_str(e.kind), "route": e.route.as_str(),
                "cause": e.cause.to_json(),
            }),
            Self::ApiKeysCollected(e) => json!({
                "type": "auth", "event": "api_keys.collected",
                "key_ids": e.keys.iter().map(|k| k.get()).collect::<Vec<_>>(),
            }),
            Self::SessionRevoked(e) => json!({
                "type": "auth", "event": "session.revoked",
                "session": e.session.to_string(), "user": e.user.get(),
                "cause": e.cause.as_str(),
            }),
            Self::UserSessionsRevoked(e) => json!({
                "type": "auth", "event": "user_sessions.revoked",
                "user": e.user.get(), "cause": e.cause.as_str(), "count": e.count,
            }),
            Self::PasswordChanged(e) => json!({
                "type": "auth", "event": "password.changed",
                "user": e.user.get(), "by": e.by.get(),
            }),
            Self::IdentityLinked(e) => identity_json("identity.linked", e),
            Self::IdentityUnlinked(e) => identity_json("identity.unlinked", e),
            Self::DatabaseSecretRotated(e) => json!({
                "type": "auth", "event": "database_secret.rotated", "new_kid": e.new_kid,
            }),
        }
    }
}

fn identity_json(event: &str, link: &IdentityLink) -> Value {
    json!({
        "type": "auth", "event": event,
        "identity_row": link.identity_row, "user": link.user.get(),
        "provider_id": link.provider_id, "by": link.by.as_str(),
    })
}
