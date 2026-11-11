//! Native API key issuance. No verification or HTTP dispatch.
//!
//! Call [`issue`] inside `Orm::run` to retry the entire transaction on contention.
//! A caller with an explicit transaction receives `Concurrency` unchanged and must
//! roll back. Never retry only the insert. Publish the returned secret only after
//! commit succeeds; failed attempts must not produce any external side effects.
use super::apikey::{ApiKeyHasher, GeneratedApiKey};
use erp_fields::{DateTime, Error, ErrorKind, Result};
use erp_orm::audit::{ApiKeyIssued, AuthEvent, KeyRouteScope};
use erp_orm::{ApiKeyId, PrincipalKind, Transaction, UserId};

/// Principal kind requested for a new key. Agent/service remain gated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyKind {
    /// Acts as the subject.
    User,
    /// Delegates for the subject; currently refused.
    Agent,
    /// Dedicated service account; currently refused.
    Service,
}
impl KeyKind {
    /// Parse the closed kind vocabulary; unknown values are Validation errors.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "agent" => Ok(Self::Agent),
            "service" => Ok(Self::Service),
            _ => Err(Error::validation("kind must be user, agent or service")),
        }
    }
}
/// Route audience. MCP only admits agent keys, which remain gated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyScope {
    /// JSON-2 and RPC bearer routes.
    Rpc,
    /// MCP routes only; currently refused for user keys.
    Mcp,
}
impl KeyScope {
    /// Parse the closed route vocabulary; unknown values are Validation errors.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "rpc" => Ok(Self::Rpc),
            "mcp" => Ok(Self::Mcp),
            _ => Err(Error::validation("scope must be rpc or mcp")),
        }
    }
}
/// Validated native issuance request; contains no caller-supplied privilege flags.
/// User keys have no allowlists, are writable and inherit subject capabilities.
#[non_exhaustive]
pub struct IssueRequest {
    subject: UserId,
    name: Box<str>,
    expiry: Option<DateTime>,
}
impl IssueRequest {
    /// Validate name (nonempty after trimming, at most 128 bytes), kind and scope.
    /// Agent/service return the AccessDenied gate; MCP user keys and
    /// invalid names return Validation. Expiry is checked against database policy
    /// by [`issue`], never against an application clock.
    pub fn new(
        subject: UserId,
        name: &str,
        kind: KeyKind,
        scope: KeyScope,
        expiry: Option<DateTime>,
    ) -> Result<Self> {
        if name.len() > 128 || name.trim().is_empty() {
            return Err(Error::validation(
                "name must be nonempty after trimming and at most 128 bytes",
            ));
        }
        let gated = match kind {
            KeyKind::User => None,
            KeyKind::Agent => Some("agent"),
            KeyKind::Service => Some("service"),
        };
        if let Some(kind) = gated {
            return Err(Error::new(
                ErrorKind::AccessDenied,
                format!("API keys of kind '{kind}' are not accepted until a later release"),
            ));
        }
        if scope != KeyScope::Rpc {
            return Err(Error::validation(
                "scope mcp requires kind agent and capabilities none",
            ));
        }
        Ok(Self {
            subject,
            name: name.into(),
            expiry,
        })
    }
}
/// One-time result. Deliberately neither Debug, Display, Clone nor serializable.
/// Do not expose its secret before the caller's commit succeeds.
///
/// ```compile_fail
/// use erp_security::auth::apikey_store::IssuedApiKey;
/// fn cannot_print(key: IssuedApiKey) { println!("{key:?}"); }
/// ```
/// ```compile_fail
/// use erp_security::auth::apikey_store::IssuedApiKey;
/// fn cannot_clone(key: IssuedApiKey) { let _copy = key.clone(); }
/// ```
#[non_exhaustive]
pub struct IssuedApiKey {
    id: ApiKeyId,
    prefix: Box<str>,
    kid: i16,
    expiry: Option<DateTime>,
    secret: IssuedSecret,
}
impl IssuedApiKey {
    /// Persisted row id (only durable after commit).
    pub fn id(&self) -> ApiKeyId {
        self.id
    }
    /// Eight-character display fragment, never a credential.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
    /// Active digest key identifier.
    pub fn kid(&self) -> i16 {
        self.kid
    }
    /// Inclusive expiration in naive UTC; None is persistent.
    pub fn expiration_date(&self) -> Option<DateTime> {
        self.expiry
    }
    /// Consume this handle for the explicit one-time response after commit.
    /// The caller must protect and promptly drop the zeroizing returned secret.
    pub fn into_secret(self) -> IssuedSecret {
        self.secret
    }
}
/// Zeroizing credential returned only by consuming an issuance result. It has
/// no Debug, Display, Serialize or Clone implementation. Borrow explicitly for
/// the single response after commit, then drop; never log the returned text.
///
/// ```compile_fail
/// use erp_security::auth::apikey_store::IssuedSecret;
/// fn cannot_print(secret: IssuedSecret) { println!("{secret}"); }
/// ```
#[non_exhaustive]
pub struct IssuedSecret(GeneratedApiKey);
impl IssuedSecret {
    /// Borrow the credential for the committed one-time response.
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

fn column<T: tokio_postgres::types::FromSqlOwned>(
    row: &tokio_postgres::Row,
    index: usize,
) -> Result<T> {
    row.try_get(index)
        .map_err(|e| Error::db(format!("a later release column {index}: {e}")).with_source(e))
}

/// Create a typed request inside a caller-owned transaction:
///
/// ```no_run
/// use erp_fields::DateTime;
/// use erp_orm::{Transaction, UserId};
/// use erp_security::auth::apikey::ApiKeyHasher;
/// use erp_security::auth::apikey_store::{issue, IssueRequest, IssuedApiKey, KeyKind, KeyScope};
/// async fn create(tx: &mut Transaction, hasher: &ApiKeyHasher, uid: UserId, expiry: DateTime)
///     -> erp_fields::Result<IssuedApiKey>
/// {
///     let request = IssueRequest::new(uid, "Integration", KeyKind::User, KeyScope::Rpc, Some(expiry))?;
///     issue(tx, hasher, &request).await
/// }
/// // Caller commits successfully before consuming/returning the secret.
/// ```
///
/// Issue a native user key in the caller's transaction. Pass the hasher belonging
/// to this tenant; rebuild it on database.secret rotation. All authoritative
/// state, limits and locks live in this tenant's PostgreSQL database.
///
/// The issuance is audited in the caller's transaction: `api_key.issued` is built from the
/// values the INSERT returned and written with the transaction's audit row, so it commits or
/// rolls back with the key. The transaction needs an `OperationIdentity` (RPC, the collector and
/// jobs attach one); without it the commit fails with an error and no key survives.
/// There is no unaudited issuance path.
///
/// # Errors
/// Validation for expiry/configuration, Access for inactive/missing subjects or
/// unauthorized delegation, Programming for a missing model, Database for missing
/// or incompatible private schema, and unchanged
/// database/Concurrency errors. Generate inside each retry attempt and release
/// the secret only after successful commit. No effects occur outside this tx.
pub async fn issue(
    tx: &mut Transaction,
    hasher: &ApiKeyHasher,
    request: &IssueRequest,
) -> Result<IssuedApiKey> {
    // A restricted key must never mint an unrestricted user credential, including
    // when a caller elevated its environment. Kind alone is not sufficient.
    let env = tx.env();
    if let Some(scope) = env.principal().scope()
        && (scope.read_only()
            || !matches!(scope.models(), erp_orm::ModelAllow::All)
            || !matches!(scope.methods(), erp_orm::MethodAllow::All))
    {
        return Err(Error::access(
            "Restricted principals cannot issue unrestricted API keys",
        ));
    }
    let caller = env.user_id();
    if !matches!(
        env.principal().kind(),
        PrincipalKind::System | PrincipalKind::User
    ) {
        return Err(Error::access(
            "API key issuance requires a user or core System principal",
        ));
    }
    // Flush caller-owned ORM changes before authoritative raw reads.
    tx.flush().await?;
    if tx.env().registry().model("res.users.apikeys").is_none() {
        return Err(Error::programming(
            "a later release requires res.users.apikeys model",
        ));
    }
    // A real row version write is essential: a waiting RR transaction must fail
    // with 40001 rather than count keys through an older snapshot (D4 amendment).
    let fenced = tx
        .query(
            "UPDATE res_users SET id=id WHERE id=$1 AND active IS TRUE RETURNING id::int8",
            &[&request.subject.get()],
        )
        .await?;
    if fenced.is_empty() {
        return Err(Error::access("API key subject is missing or inactive"));
    }
    // Preserve subject -> key-table lock order during schema validation too.
    ensure_schema(tx).await?;
    let policy = tx
        .query(POLICY_SQL, &[&request.subject.get(), &caller.get()])
        .await?;
    let row = policy
        .first()
        .ok_or_else(|| Error::programming("a later release policy row missing"))?;
    let caller_active: bool = column(row, 0)?;
    let caller_system: bool = column(row, 1)?;
    let subject_system: bool = column(row, 2)?;
    if !caller_active {
        return Err(Error::access("API key creator is missing or inactive"));
    }
    if caller != request.subject && !caller_system {
        return Err(Error::access(
            "API key delegation requires base.group_system",
        ));
    }
    let maximum: Option<f64> = column(row, 3)?;
    let invalid: bool = column(row, 4)?;
    if invalid {
        return Err(Error::validation(
            "api_key_duration must be finite and nonnegative",
        ));
    }
    let now = DateTime::from_micros(column(row, 5)?);
    let count: i64 = column(row, 6)?;
    let limit: Option<String> = column(row, 7)?;
    let limit = match limit {
        None => 10,
        Some(limit) => {
            if limit.len() > 10 || limit.is_empty() || !limit.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::validation(
                    "base.programmatic_api_keys_limit must be a positive 32-bit integer",
                ));
            }
            let parsed = limit.parse::<u32>().map_err(|_| {
                Error::validation(
                    "base.programmatic_api_keys_limit must be a positive 32-bit integer",
                )
            })?;
            if parsed == 0 {
                return Err(Error::validation(
                    "base.programmatic_api_keys_limit must be a positive 32-bit integer",
                ));
            }
            parsed
        }
    };
    let enabled: Option<String> = column(row, 8)?;
    let enabled = programmatic_enabled(enabled.as_deref())?;
    if !caller_system && !enabled {
        return Err(Error::user("Programmatic API keys are not enabled"));
    }
    if count >= i64::from(limit) {
        return Err(Error::user(format!(
            "Limit of {limit} API keys is reached for programmatic creation"
        )));
    }
    validate_expiry(request.expiry, now, subject_system, maximum)?;
    let secret = GeneratedApiKey::generate()?;
    let token = secret.token();
    let prefix: Box<str> = token.display_prefix().into();
    let digest = hasher.digest(&token);
    let expires = request.expiry;
    let rows = tx
        .query(
            INSERT_SQL,
            &[
                &request.name.as_ref(),
                &request.subject.get(),
                &caller.get(),
                &digest.bytes().as_slice(),
                &i32::from(digest.kid()),
                &prefix.as_ref(),
                &expires,
            ],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::programming("a later release insert returned no row"))?;
    let id = ApiKeyId::new(column(row, 0)?).map_err(|error| {
        Error::programming("a later release insert returned an invalid key id").with_source(error)
    })?;
    let owner = user_id(row, 1)?;
    let created_by = user_id(row, 2)?;
    let kind: String = column(row, 3)?;
    let route: String = column(row, 4)?;
    let read_only: bool = column(row, 5)?;
    let capabilities: String = column(row, 6)?;
    let stored_expiry: Option<DateTime> = column(row, 7)?;
    let no_models: bool = column(row, 8)?;
    let no_methods: bool = column(row, 9)?;
    if !(no_models && no_methods) {
        return Err(Error::programming(
            "a later release issued a key with allowlists; user keys have none",
        ));
    }
    // built from the stored row, never from the request.
    // Ids, closed enums, flags and the expiry only: no secret, digest, prefix or name.
    tx.audit_auth(AuthEvent::ApiKeyIssued(ApiKeyIssued::new(
        id,
        owner,
        created_by,
        match KeyKind::parse(&kind)? {
            KeyKind::User => erp_orm::KeyKind::User,
            KeyKind::Agent => erp_orm::KeyKind::Agent,
            KeyKind::Service => erp_orm::KeyKind::Service,
        },
        match KeyScope::parse(&route)? {
            KeyScope::Rpc => KeyRouteScope::Rpc,
            KeyScope::Mcp => KeyRouteScope::Mcp,
        },
        capabilities == "subject",
        read_only,
        stored_expiry.map(DateTime::micros),
        None,
        None,
    )))?;
    Ok(IssuedApiKey {
        id,
        prefix,
        kid: digest.kid(),
        expiry: request.expiry,
        secret: IssuedSecret(secret),
    })
}

fn user_id(row: &tokio_postgres::Row, i: usize) -> Result<UserId> {
    UserId::new(column(row, i)?).map_err(|error| {
        Error::programming("a later release insert returned an invalid user id").with_source(error)
    })
}

fn validate_expiry(
    expiry: Option<DateTime>,
    now: DateTime,
    system: bool,
    maximum: Option<f64>,
) -> Result<()> {
    let Some(expiry) = expiry else {
        return if system {
            Ok(())
        } else {
            Err(Error::validation(
                "The API key must have an expiration date",
            ))
        };
    };
    if expiry <= now {
        return Err(Error::validation(
            "You cannot set an expiration date in the past.",
        ));
    }
    if system {
        return Ok(());
    }
    let days = match maximum {
        None | Some(0.0) => 1.0,
        Some(days) => days,
    };
    if !days.is_finite() || days < 0.0 {
        return Err(Error::validation(
            "api_key_duration must be finite and nonnegative",
        ));
    }
    let micros = days * 86_400_000_000.0;
    if !micros.is_finite() || micros >= i64::MAX as f64 {
        return Err(Error::validation(
            "api_key_duration exceeds timestamp range",
        ));
    }
    let limit = now
        .micros()
        .checked_add(micros.round() as i64)
        .ok_or_else(|| Error::validation("api_key_duration exceeds timestamp range"))?;
    if expiry.micros() > limit {
        return Err(Error::validation(format!(
            "You cannot exceed {days:?} days."
        )));
    }
    Ok(())
}

const INSERT_SQL: &str = "INSERT INTO res_users_apikeys
(name,user_id,created_by,kind,scope,digest,digest_kid,prefix,models,methods,
 read_only,capabilities,expiration_date,create_date)
VALUES($1,$2,$3,'user','rpc',$4,$5,$6,NULL,NULL,false,'subject',
       $7::timestamp,now() AT TIME ZONE 'utc') RETURNING id::int8, user_id::int8, created_by::int8, kind, scope, read_only,
 capabilities, expiration_date, models IS NULL, methods IS NULL";

pub(super) const POLICY_SQL: &str = r#"
WITH RECURSIVE effective(uid,gid) AS (
 SELECT uid,gid FROM res_groups_users_rel WHERE uid IN ($1,$2)
 UNION
 SELECT e.uid,r.hid FROM effective e JOIN res_groups_implied_rel r ON r.gid=e.gid
), system_group AS (
 SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system' AND model='res.groups'
), durations AS (SELECT g.api_key_duration FROM effective e JOIN res_groups g ON g.id=e.gid WHERE e.uid=$1)
SELECT EXISTS(SELECT FROM res_users WHERE id=$2 AND active IS TRUE),
 EXISTS(SELECT FROM effective WHERE uid=$2 AND gid IN (SELECT res_id FROM system_group)),
 EXISTS(SELECT FROM effective WHERE uid=$1 AND gid IN (SELECT res_id FROM system_group)),
 (SELECT max(api_key_duration) FROM durations),
 EXISTS(SELECT FROM durations WHERE api_key_duration<0 OR api_key_duration IN ('NaN'::float8,'Infinity'::float8,'-Infinity'::float8)),
 (extract(epoch FROM now())*1000000)::int8,
 (SELECT count(*) FROM res_users_apikeys WHERE user_id=$1 AND (expiration_date IS NULL OR expiration_date>=now() AT TIME ZONE 'utc')),
 -- Only an absent row returns SQL NULL. Invalid present values use an
 -- unparseable sentinel so they fail loudly, with bounded client allocation.
 (SELECT CASE WHEN value IS NULL OR octet_length(value)>10 THEN 'invalid' ELSE value END FROM ir_config_parameter WHERE key='base.programmatic_api_keys_limit'),
 (SELECT CASE WHEN value IS NULL OR octet_length(value)>128 THEN 'invalid' ELSE value END FROM ir_config_parameter WHERE key='base.enable_programmatic_api_keys')
"#;

async fn ensure_schema(tx: &mut Transaction) -> Result<()> {
    tx.batch(super::schema::API_KEY_REQUIRE_SCHEMA_DDL).await?;
    Ok(())
}

pub(super) fn programmatic_enabled(enabled: Option<&str>) -> Result<bool> {
    match enabled
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None => Ok(false),
        Some("y" | "yes" | "1" | "true" | "t" | "on") => Ok(true),
        Some("n" | "no" | "0" | "false" | "f" | "off") => Ok(false),
        Some(_) => Err(Error::validation(
            "base.enable_programmatic_api_keys must be a boolean",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiry_arithmetic_never_overflows() {
        let e = validate_expiry(
            Some(DateTime::from_micros(i64::MAX)),
            DateTime::from_micros(i64::MAX - 1),
            false,
            Some(1.0),
        )
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(e.to_string(), "api_key_duration exceeds timestamp range");
    }
}
