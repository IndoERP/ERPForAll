//! Programmatic native-key lifecycle. Every operation belongs to
//! the caller's transaction; retry the complete transaction on concurrency errors.
use super::apikey::{ApiKeyHasher, NativeApiKey};
use super::apikey_store::{IssueRequest, IssuedApiKey, KeyKind, KeyScope, issue};
use super::secret::DatabaseSecret;
use erp_fields::{DateTime, Error, ErrorKind, Result};
use erp_orm::audit::{
    ApiKeyRevoked, ApiKeysCollected, AuditOrigin, AuthEvent, KeyRouteScope, MAX_COLLECTED_KEYS,
    RevokeCause,
};
use erp_orm::{Actor, ApiKeyId, KeyKind as ActorKeyKind, Principal, PrincipalKind, Transaction};
use zeroize::Zeroizing;

const GENERATE_INVALID: &str =
    "The provided API key is invalid or does not belong to the current user.";
const REVOKE_INVALID: &str = "The provided API key is invalid.";

fn column<T: tokio_postgres::types::FromSqlOwned>(
    row: &tokio_postgres::Row,
    i: usize,
) -> Result<T> {
    row.try_get(i)
        .map_err(|e| Error::db(format!("a later release column {i}: {e}")).with_source(e))
}

async fn hasher(tx: &mut Transaction) -> Result<ApiKeyHasher> {
    let rows = tx.query("SELECT CASE WHEN octet_length(value) BETWEEN 32 AND 512 THEN value END FROM ir_config_parameter WHERE key='database.secret'", &[]).await?;
    let text: Option<String> = match rows.first() {
        Some(row) => column(row, 0)?,
        None => None,
    };
    let text = Zeroizing::new(
        text.ok_or_else(|| Error::validation("database.secret is missing or invalid"))?,
    );
    let secret = DatabaseSecret::from_stored(&text)
        .map_err(|e| Error::validation(e.to_string()).with_source(e))?;
    ApiKeyHasher::derive(&secret).map_err(|e| Error::validation(e.to_string()).with_source(e))
}

async fn gate(tx: &mut Transaction) -> Result<()> {
    if !matches!(
        tx.env().principal().kind(),
        PrincipalKind::User | PrincipalKind::System
    ) {
        return Err(Error::access(
            "Programmatic API key management requires a user or core System principal",
        ));
    }
    tx.flush().await?;
    let uid = tx.env().user_id().get();
    // Match issuance's subject -> key lock order. A real version write makes
    // concurrent RR issuers retry rather than use an obsolete policy/count.
    if tx
        .query(
            "UPDATE res_users SET id=id WHERE id=$1 AND active IS TRUE RETURNING id",
            &[&uid],
        )
        .await?
        .is_empty()
    {
        return Err(Error::access("API key creator is missing or inactive"));
    }
    let rows = tx
        .query(super::apikey_store::POLICY_SQL, &[&uid, &uid])
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::programming("API key management policy row missing"))?;
    let system: bool = column(row, 1)?;
    let enabled: Option<String> = column(row, 8)?;
    if !super::apikey_store::programmatic_enabled(enabled.as_deref())? && !system {
        return Err(Error::user("Programmatic API keys are not enabled"));
    }
    if tx.env().principal().scope().is_some_and(|scope| {
        scope.read_only()
            || !matches!(scope.models(), erp_orm::ModelAllow::All)
            || !matches!(scope.methods(), erp_orm::MethodAllow::All)
    }) {
        return Err(Error::access(
            "Restricted principals cannot manage unrestricted API keys",
        ));
    }
    Ok(())
}

/// The API key a principal authenticated with, looking through an impersonation chain to the
/// original principal. `None` for a principal without an actor (session or core System).
///
/// # Errors
/// Programming for a link-token actor or an actor kind this code does not know: neither can
/// manage API keys, and neither may silently count as the owner.
fn key_actor(principal: &Principal) -> Result<Option<(ApiKeyId, ActorKeyKind)>> {
    match principal.actor() {
        None => Ok(None),
        Some(Actor::ApiKey { id, kind, .. }) => Ok(Some((*id, *kind))),
        Some(Actor::Impersonator(original)) => key_actor(original),
        Some(Actor::Link { .. }) => Err(Error::programming(
            "auth: a link token cannot manage API keys",
        )),
        Some(_) => Err(Error::programming(
            "auth: unknown actor kind reached API key management",
        )),
    }
}

/// The id of the active native user key `key` of the current user and `scope`, locked.
/// Every refused lookup (malformed, unknown, foreign, expired, wrong scope) answers
/// AccessDenied with the one `refusal` text of the calling operation, so the answer
/// never tells whether the key exists.
async fn key_id(
    tx: &mut Transaction,
    hasher: &ApiKeyHasher,
    key: &str,
    scope: &str,
    refusal: &'static str,
) -> Result<i64> {
    let invalid = || Error::new(ErrorKind::AccessDenied, refusal);
    let token = NativeApiKey::parse(key).map_err(|_| invalid())?;
    let digest = hasher.digest(&token);
    let uid = tx.env().user_id().get();
    let rows = tx.query("SELECT id::int8 FROM res_users_apikeys WHERE digest=$1 AND digest_kid=$2 AND user_id=$3 AND scope=$4 AND kind='user' AND models IS NULL AND methods IS NULL AND read_only IS FALSE AND capabilities='subject' AND (expiration_date IS NULL OR expiration_date>=now() AT TIME ZONE 'utc') FOR UPDATE", &[&digest.bytes().as_slice(), &i32::from(digest.kid()), &uid, &scope]).await?;
    let id: i64 = column(rows.first().ok_or_else(invalid)?, 0)?;
    // Agent and service keys are refused completely, also behind an impersonation chain and
    // under elevated environments; they cannot manage keys, not even their own.
    if let Some((_, kind)) = key_actor(tx.env().principal())?
        && !matches!(kind, ActorKeyKind::User)
    {
        return Err(Error::access(
            "Agent and service keys cannot manage API keys",
        ));
    }
    Ok(id)
}

/// Renew a native user key by creating another key for the same current user
/// and route scope. The issuance is audited by [`issue`] (one `api_key.issued` event; the
/// transaction needs an `OperationIdentity`). Does not revoke the old key. Publish the returned secret
/// only after commit. Agent/service/legacy keys remain gated.
pub async fn generate(
    tx: &mut Transaction,
    key: &str,
    scope: &str,
    name: &str,
    expiry: Option<DateTime>,
) -> Result<IssuedApiKey> {
    gate(tx).await?;
    let hasher = hasher(tx).await?;
    key_id(tx, &hasher, key, scope, GENERATE_INVALID).await?;
    let request = IssueRequest::new(
        tx.env().user_id(),
        name,
        KeyKind::User,
        KeyScope::Rpc,
        expiry,
    )?;
    // `issue` records `api_key.issued` in this transaction; no second event.
    issue(tx, &hasher, &request).await
}

/// Delete an active native key owned by the current user. Self-revocation is
/// allowed; the next transaction on every replica observes the deletion.
pub async fn revoke(tx: &mut Transaction, key: &str) -> Result<bool> {
    gate(tx).await?;
    let hasher = hasher(tx).await?;
    let id = key_id(tx, &hasher, key, "rpc", REVOKE_INVALID).await?;
    let deleted = tx
        .execute("DELETE FROM res_users_apikeys WHERE id=$1", &[&id])
        .await?;
    if deleted == 1 {
        // Commit publishes cache:apikeys on signal-enabled nodes atomically
        // with deletion; native verification itself never caches acceptance.
        tx.invalidate_ormcache("apikeys");
        // The audit event commits with the deletion. The cause comes from the
        // transaction's own authenticated actor, never from caller input.
        let revoked = ApiKeyId::new(id).map_err(|error| {
            Error::programming("a later release revoked an invalid key id").with_source(error)
        })?;
        let cause = match key_actor(tx.env().principal())? {
            Some((actor, _)) if actor == revoked => RevokeCause::SelfRevoked,
            Some((actor, _)) => RevokeCause::Programmatic { by_key: actor },
            None => RevokeCause::Owner,
        };
        let owner = tx.env().user_id();
        tx.audit_auth(AuthEvent::ApiKeyRevoked(ApiKeyRevoked::new(
            revoked,
            owner,
            erp_orm::KeyKind::User,
            KeyRouteScope::Rpc,
            cause,
        )))?;
    }
    Ok(deleted == 1)
}

/// Delete at most `limit` expired keys in this transaction. Only core System
/// may collect; user elevation does not confer collection authority. Concurrent
/// collectors skip locked rows. Inclusive expiry matches bearer verification.
/// No process-local cursor or leader is required; rollback releases all work.
///
/// # Errors
/// Validation unless `limit` is in `1..=`[`MAX_COLLECTED_KEYS`] (1000); Access for non-System principals;
/// database/concurrency errors propagate for whole-transaction retry.
pub async fn collect_expired(tx: &mut Transaction, limit: u32) -> Result<u64> {
    if tx.env().principal().kind() != PrincipalKind::System
        || tx.env().principal().actor().is_some()
    {
        return Err(Error::access(
            "API key collection requires a core System principal",
        ));
    }
    if usize::try_from(limit).map_or(true, |l| l == 0 || l > MAX_COLLECTED_KEYS) {
        return Err(Error::validation(format!(
            "API key collection limit must be between 1 and {MAX_COLLECTED_KEYS}"
        )));
    }
    // Raw-only: scheduled collection intentionally uses READ COMMITTED.
    let rows = tx.query("WITH expired AS (SELECT id FROM res_users_apikeys WHERE expiration_date < now() AT TIME ZONE 'utc' ORDER BY expiration_date,id LIMIT $1 FOR UPDATE SKIP LOCKED) DELETE FROM res_users_apikeys k USING expired e WHERE k.id=e.id RETURNING k.id::int8", &[&i64::from(limit)]).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    tx.invalidate_ormcache("apikeys");
    // One event names the collected chunk (at most 1000 ids, the same bound as `limit`);
    // it commits with the deletion, so a rolled back batch leaves no record.
    let mut ids = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = column(row, 0)?;
        ids.push(ApiKeyId::new(id).map_err(|error| {
            Error::programming("a later release collected an invalid key id").with_source(error)
        })?);
    }
    tx.audit_origin(AuditOrigin::Auth)?;
    tx.audit_auth(AuthEvent::ApiKeysCollected(ApiKeysCollected::new(ids)?))?;
    Ok(rows.len() as u64)
}
