//! Native keys checked with optional cookie evidence in one primary opening statement.
//!
//! Only digests and the exact route audience reach PostgreSQL. A cookie is evidence for
//! precedence, never authority for a presented key. No successful check is cached.
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::{
    AccessPolicy, ApiKeyId, CredentialCheck, CredentialVerdict, MethodAllow, ModelAllow, Orm,
    PrincipalBuilder, ResolverStatement, Scope, Transaction, UserId,
};
use erp_sql::param::{Param, Scalar};
use tokio_postgres::Row;

use super::apikey_store::KeyScope;
pub use super::login::ApiKeyLastUse;
use super::session::{Cached, Refusal, SessionCheck, SessionSecret};
use super::settings::{AuthConfig, CACHE_NAME};
use crate::resolver::Resolver;

/// A borrowed API-key attempt. Raw credentials are never formatted or retained by `Auth`.
/// Malformed input remains an attempt so it receives the shared address throttle and cookie
/// precedence check. HTTP callers must supply their trusted-proxy-resolved client address.
///
/// ```
/// use erp_security::auth::ApiKeyRequest;
/// use erp_security::auth::apikey_store::KeyScope;
/// let request = ApiKeyRequest::new("malformed", KeyScope::Rpc, "192.0.2.1".parse()?);
/// assert!(!format!("{request:?}").contains("malformed"));
/// # Ok::<(), std::net::AddrParseError>(())
/// ```
#[non_exhaustive]
pub struct ApiKeyRequest<'a> {
    pub(super) raw: &'a str,
    pub(super) scope: KeyScope,
    pub(super) address: IpAddr,
    pub(super) session: Option<&'a SessionSecret>,
    pub(super) uid: Option<UserId>,
}
impl<'a> ApiKeyRequest<'a> {
    /// A key presented to an exact route audience from the resolved client address.
    pub fn new(raw: &'a str, scope: KeyScope, address: IpAddr) -> Self {
        Self {
            raw,
            scope,
            address,
            session: None,
            uid: None,
        }
    }
    /// Optional browser cookie, checked for subject agreement without touch or deletion.
    #[must_use]
    pub fn session(mut self, session: &'a SessionSecret) -> Self {
        self.session = Some(session);
        self
    }
    /// Require this uid as well as a valid key, for typed RPC credential checks.
    #[must_use]
    pub fn uid(mut self, uid: UserId) -> Self {
        self.uid = Some(uid);
        self
    }
}
impl fmt::Debug for ApiKeyRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyRequest")
            .field("credential", &"<redacted>")
            .field("scope", &self.scope)
            .field("address", &self.address)
            .field("session", &self.session)
            .field("uid", &self.uid)
            .finish()
    }
}

/// An authenticated request, carrying key-derived authority only.
#[non_exhaustive]
pub struct ApiKeyBegin {
    /// Open request transaction on the tenant primary.
    pub tx: Transaction,
    /// The optional cookie failed validation; the HTTP boundary clears it.
    pub drop_cookie: bool,
    /// `Some` when this key's `last_used_at` is due (never set, or older than one hour by the
    /// database clock). Dropping the ticket launches the bookkeeping write; hold it until the
    /// request transaction has ended. See [`ApiKeyLastUse`].
    pub last_use: Option<ApiKeyLastUse>,
}
impl fmt::Debug for ApiKeyBegin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyBegin")
            .field("drop_cookie", &self.drop_cookie)
            .field("last_use", &self.last_use)
            .finish_non_exhaustive()
    }
}

/// Typed source on an expected API-key refusal, preserving cookie-clearing evidence even
/// when no request transaction is returned. Contains no credential or database identity.
#[derive(Debug)]
#[non_exhaustive]
pub struct ApiKeyRefusal {
    drop_cookie: bool,
}
impl ApiKeyRefusal {
    /// Whether the HTTP boundary should clear a known-invalid browser cookie.
    pub fn drop_cookie(&self) -> bool {
        self.drop_cookie
    }
    pub(super) fn attach(error: Error, drop_cookie: bool) -> Error {
        error.with_source(Self { drop_cookie })
    }
}
impl fmt::Display for ApiKeyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("API key request refused")
    }
}
impl std::error::Error for ApiKeyRefusal {}

/// Typed source on the address cooldown error; the transport emits 429 and `Retry-After`.
#[derive(Debug)]
#[non_exhaustive]
pub struct ApiKeyCooldown {
    retry_after: Duration,
    drop_cookie: bool,
}
impl ApiKeyCooldown {
    /// Remaining cooldown (round up to seconds when encoding `Retry-After`).
    pub fn retry_after(&self) -> Duration {
        self.retry_after
    }
    /// Whether a presented cookie was invalid and should be cleared.
    pub fn drop_cookie(&self) -> bool {
        self.drop_cookie
    }
    pub(super) fn error(retry_after: Duration, drop_cookie: bool) -> Error {
        Error::new(ErrorKind::AccessDenied, super::login::COOLDOWN_MESSAGE).with_source(Self {
            retry_after,
            drop_cookie,
        })
    }
}
impl fmt::Display for ApiKeyCooldown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(super::login::COOLDOWN_MESSAGE)
    }
}
impl std::error::Error for ApiKeyCooldown {}

/// A key's `last_used_at` is written at most once in this many seconds, cluster-wide: the
/// opening statement flags a key as due by it and the write repeats the same predicate.
pub(super) const LAST_USED_INTERVAL_SECS: i64 = 3600;

/// What an accepted key check hands to [`super::Auth::begin_api_key`].
pub(super) struct KeyAccepted {
    pub drop_cookie: bool,
    pub key: ApiKeyId,
    /// The key's `last_used_at` is NULL or older than [`LAST_USED_INTERVAL_SECS`].
    pub last_use_due: bool,
}

/// The write of a key's last use: one row, only while it is still due and not locked by anyone
/// else. Bookkeeping, like the session touch: `write_date` and `write_uid` stay as they are.
/// `$1` is the key id (`int8`), `$2` the interval in seconds (`int8`). Returns the rows it
/// changed, so an empty answer means "not due, locked or gone".
pub(super) const LAST_USE_SQL: &str = "\
UPDATE res_users_apikeys SET last_used_at = now() AT TIME ZONE 'utc'
 WHERE id IN (SELECT id FROM res_users_apikeys
               WHERE id = $1 AND (last_used_at IS NULL
                     OR last_used_at < (now() AT TIME ZONE 'utc') - make_interval(secs => $2))
                 FOR NO KEY UPDATE SKIP LOCKED)
RETURNING id";

pub(super) fn invalid_key() -> Error {
    Error::new(ErrorKind::AccessDenied, "Invalid apikey")
}

pub(super) enum KeyRefused {
    Stale,
    Cooldown {
        retry_after: Duration,
        drop_cookie: bool,
    },
    Denied {
        error: Error,
        drop_cookie: bool,
        failure: Option<super::login::ApiKeyFailureTicket>,
    },
}

/// The first sixteen columns exactly preserve SessionCheck's private wire contract, followed by
/// the plan's added token fields (`plan.len()` text columns, NULL in the key and throttle rows).
/// Key evidence occupies the remaining columns (the key row ends with the last-use due flag, NULL
/// in the other rows); the tag column at `first + 4` distinguishes rows, so an absent
/// key still permits checking a presented cookie before refusing the key. At most two rows
/// per digest are returned, allowing duplicate corruption to fail loudly without unbounded
/// transfer. Stored JSON allowlists are tested in PostgreSQL and never transferred.
pub(super) fn check_sql(resolver: &Resolver, plan: &super::token_plan::TokenPlan) -> String {
    let cookie = super::session::check_sql(resolver, plan).replace("$1", "$4");
    let pad = ", NULL::text".repeat(plan.len());
    let secs = LAST_USED_INTERVAL_SECS;
    format!(
        r#"WITH cookie AS ({cookie} LIMIT 2),
address_throttle AS MATERIALIZED (
 SELECT failures, extract(epoch FROM now()-last_failure_at)::float8 AS elapsed
   FROM erp_login_throttle WHERE kind='address' AND key_hash=decode($5::text,'hex')
), eligible AS (
 SELECT 1 WHERE NOT EXISTS(SELECT FROM address_throttle WHERE failures >= $6::int8 AND elapsed < $7::float8)
)
SELECT {},
       NULL::text, NULL::int8, NULL::text, NULL::bytea, NULL::int2,
       NULL::float8, NULL::float8, NULL::float8, NULL::float8, NULL::text, NULL::text{pad},
       k.id::int8,
       CASE WHEN octet_length(k.name) BETWEEN 1 AND 128 AND length(btrim(k.name))>0 THEN k.name ELSE NULL END,
       CASE WHEN octet_length(k.kind)<=7 THEN k.kind ELSE NULL END,
       (k.models IS NULL AND k.methods IS NULL AND k.read_only IS FALSE AND k.capabilities='subject'),
       0::int4, NULL::int4, NULL::float8,
       (k.last_used_at IS NULL
        OR k.last_used_at < (now() AT TIME ZONE 'utc') - make_interval(secs => {secs}))
  FROM (SELECT * FROM res_users_apikeys
         WHERE EXISTS(SELECT FROM eligible)
           AND scope=$3::text
           AND CASE WHEN $8::int8 = 0
                    THEN digest=decode($1::text,'hex') AND digest_kid=$2::int8 AND legacy_hash IS NULL
                    ELSE id=$8::int8 AND legacy_hash=$9::text END
           AND (expiration_date IS NULL OR expiration_date>=now() AT TIME ZONE 'utc')
         LIMIT 2) k
  JOIN res_users u ON u.id=k.user_id
UNION ALL
SELECT cookie.*, NULL::int8, NULL::text, NULL::text, NULL::boolean, 1::int4, NULL::int4, NULL::float8, NULL::boolean FROM cookie
UNION ALL
SELECT NULL::int8, NULL::boolean, NULL::int8, NULL::int8[], NULL::int8[],
       NULL::text, NULL::int8, NULL::text, NULL::bytea, NULL::int2,
       NULL::float8, NULL::float8, NULL::float8, NULL::float8, NULL::text, NULL::text{pad},
       NULL::int8, NULL::text, NULL::text, NULL::boolean, 2::int4, failures, elapsed, NULL::boolean
  FROM address_throttle"#,
        resolver.subject_columns()
    )
}

pub(super) struct KeyCheck<'a> {
    pub orm: &'a Orm,
    pub auth: &'a super::Auth,
    pub admission: &'a super::login::ApiKeyAdmission,
    pub settlement: &'a super::login::ApiKeySettlementRelease,
    pub sql: &'a str,
    pub session_sql: &'a str,
    pub digest: String,
    pub kid: i16,
    pub malformed: bool,
    /// An imported key already verified against its stored hash (id and that exact hash).
    pub legacy: Option<super::apikey_legacy::LegacyMatch>,
    pub request: &'a ApiKeyRequest<'a>,
    pub cached: Arc<Cached>,
    pub config: &'a AuthConfig,
    pub address_hash: [u8; 32],
    #[cfg(feature = "test-support")]
    pub verdict_gate: &'a std::sync::Mutex<Option<ApiKeyVerdictGate>>,
}
fn col<'r, T: tokio_postgres::types::FromSql<'r>>(row: &'r Row, index: usize) -> Result<T> {
    row.try_get(index).map_err(|e| {
        Error::db(format!(
            "could not decode API key evidence column {index}: {e}"
        ))
        .with_source(e)
    })
}
fn stored_authority() -> Error {
    Error::db("Invalid stored API key authority")
}

impl CredentialCheck for KeyCheck<'_> {
    type Accepted = KeyAccepted;
    type Refused = KeyRefused;
    fn statement(&self) -> Result<ResolverStatement> {
        let audience = match self.request.scope {
            KeyScope::Rpc => "rpc",
            KeyScope::Mcp => "mcp",
        };
        // An absent cookie binds a zero-length digest, which cannot match a stored session.
        let cookie = match self.request.session {
            Some(secret) => super::session::hex(&secret.digest()),
            None => String::new(),
        };
        Ok(ResolverStatement::new(
            self.sql.to_owned(),
            vec![
                Param::Scalar(Scalar::Text(self.digest.clone().into())),
                Param::Scalar(Scalar::Int8(i64::from(self.kid))),
                Param::Scalar(Scalar::Text(audience.into())),
                Param::Scalar(Scalar::Text(cookie.into())),
                Param::Scalar(Scalar::Text(super::session::hex(&self.address_hash).into())),
                Param::Scalar(Scalar::Int8(i64::from(
                    self.cached.settings.address_cooldown().after(),
                ))),
                Param::Scalar(Scalar::Float8(
                    self.cached
                        .settings
                        .address_cooldown()
                        .duration()
                        .as_secs_f64(),
                )),
                Param::Scalar(Scalar::Int8(self.legacy.as_ref().map_or(0, |m| m.id))),
                Param::Scalar(Scalar::Text(
                    self.legacy
                        .as_ref()
                        .map_or_else(|| "".into(), |m| Arc::clone(&m.hash)),
                )),
            ],
        ))
    }
    fn finish(
        &self,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> Result<CredentialVerdict<KeyAccepted, KeyRefused>> {
        if self.orm.ormcache_generation(CACHE_NAME) != self.cached.generation {
            return Ok(CredentialVerdict::Refused(KeyRefused::Stale));
        }
        // the key evidence follows the sixteen cookie columns and the plan's added fields
        let first = 16 + self.cached.token_plan.len();
        let mut key_rows = Vec::new();
        let mut cookie_rows = Vec::new();
        let mut throttle_row = None;
        for row in rows {
            match col::<i32>(row, first + 4)? {
                0 => key_rows.push(row),
                1 => cookie_rows.push(row.clone()),
                2 => {
                    if throttle_row.replace(row).is_some() {
                        return Err(Error::db("Multiple address throttle rows answered one key"));
                    }
                }
                _ => return Err(Error::db("Invalid API key evidence row kind")),
            }
        }
        let mut cookie_user = None;
        let mut drop_cookie = false;
        if let Some(secret) = self.request.session {
            let check = SessionCheck {
                orm: self.orm,
                sql: self.session_sql,
                digest: secret.digest(),
                cached: Arc::clone(&self.cached),
                config: self.config,
            };
            match check.finish(&cookie_rows, policy)? {
                CredentialVerdict::Accepted { principal, .. } => {
                    cookie_user = Some(principal.subject())
                }
                CredentialVerdict::Refused(refused) if refused.reason() == Refusal::KeysStale => {
                    return Ok(CredentialVerdict::Refused(KeyRefused::Stale));
                }
                CredentialVerdict::Refused(_) => drop_cookie = true,
                _ => {
                    return Err(Error::programming(
                        "Unsupported session credential verdict during API key check",
                    ));
                }
            }
        } else if !cookie_rows.is_empty() {
            return Err(Error::db("Unexpected cookie evidence without a cookie"));
        }
        if let Some(row) = throttle_row
            && let Some(retry_after) =
                cooldown(row, first, self.cached.settings.address_cooldown())?
        {
            return Ok(CredentialVerdict::Refused(KeyRefused::Cooldown {
                retry_after,
                drop_cookie,
            }));
        }
        let denied = |error, count| {
            // The ticket is created synchronously with the counted verdict, before the ORM
            // can yield while rolling back. Its admission lease bounds detached persistence.
            let failure = if count {
                Some(self.auth.api_key_failure_ticket(
                    self.address_hash,
                    self.admission,
                    self.settlement,
                )?)
            } else {
                None
            };
            let refused = KeyRefused::Denied {
                error,
                drop_cookie,
                failure,
            };
            // Test-only pause after the counted verdict exists, before the ORM awaits rollback.
            #[cfg(feature = "test-support")]
            if count {
                pause_at_counted_verdict(self.verdict_gate)?;
            }
            Ok(CredentialVerdict::Refused(refused))
        };
        if self.malformed {
            return denied(invalid_key(), true);
        }
        let row = match key_rows.as_slice() {
            [] => return denied(invalid_key(), true),
            [row] => *row,
            _ => {
                return Err(Error::db(
                    "Multiple API key rows answered one native digest",
                ));
            }
        };
        let Some(principal) = Resolver::user_from_row(row, policy)? else {
            return denied(invalid_key(), true);
        };
        if self
            .request
            .uid
            .is_some_and(|uid| uid != principal.subject())
        {
            return denied(invalid_key(), true);
        }
        let kind: Option<String> = col(row, first + 2)?;
        match kind.as_deref() {
            Some("user") => {}
            Some(kind @ ("agent" | "service")) => {
                return denied(
                    Error::new(
                        ErrorKind::AccessDenied,
                        format!("API keys of kind '{kind}' are not accepted until a later release"),
                    ),
                    true,
                );
            }
            _ => return Err(stored_authority()),
        }
        if col::<Option<bool>>(row, first + 3)? != Some(true) {
            return Err(stored_authority());
        }
        let id = ApiKeyId::new(col(row, first)?).map_err(|_| stored_authority())?;
        // a NULL flag on a key row is a broken statement, never "not due"
        let last_use_due = col::<Option<bool>>(row, first + 7)?
            .ok_or_else(|| Error::db("API key evidence lacks the last-use flag"))?;
        let name: Option<String> = col(row, first + 1)?;
        let name = name.ok_or_else(stored_authority)?;
        let scope = Scope::builder(id, name)
            .models(ModelAllow::All)
            .methods(MethodAllow::All)
            .read_only(false)
            .build()
            .map_err(|_| stored_authority())?;
        if cookie_user.is_some_and(|uid| uid != principal.subject()) {
            return denied(
                Error::access("Session user does not match the used apikey."),
                false,
            );
        }
        let principal = PrincipalBuilder::user(principal.subject())
            .resolution(principal.resolution().as_ref().clone())
            .companies(principal.companies().allowed().to_vec())
            .scope(scope)
            .build()?;
        Ok(CredentialVerdict::accepted(
            principal,
            KeyAccepted {
                drop_cookie,
                key: id,
                last_use_due,
            },
        ))
    }
}

pub(super) const FAILURE_SQL: &str = "INSERT INTO erp_login_throttle AS t(kind,key_hash,failures,last_failure_at) VALUES('address',$1::bytea,1,now()) ON CONFLICT(kind,key_hash) DO UPDATE SET failures=LEAST(t.failures,2147483646)+1,last_failure_at=now()";

fn cooldown(row: &Row, first: usize, rule: super::settings::Cooldown) -> Result<Option<Duration>> {
    let failures: i32 = col(row, first + 5)?;
    let elapsed: f64 = col(row, first + 6)?;
    if failures < 0 || !elapsed.is_finite() {
        return Err(Error::db("Invalid stored address throttle evidence"));
    }
    let since = Duration::try_from_secs_f64(elapsed.max(0.0))
        .map_err(|_| Error::db("Invalid stored address throttle elapsed time"))?;
    let failures =
        u32::try_from(failures).map_err(|_| Error::db("Invalid stored address throttle count"))?;
    Ok(rule
        .in_force(failures, since)
        .then(|| rule.duration().saturating_sub(since)))
}

/// A per-Auth deterministic cancellation seam, absent from production builds.
#[cfg(feature = "test-support")]
pub(super) struct ApiKeyVerdictGate {
    pub reached: std::sync::mpsc::SyncSender<()>,
    pub release: std::sync::mpsc::Receiver<()>,
}

#[cfg(feature = "test-support")]
fn pause_at_counted_verdict(gate: &std::sync::Mutex<Option<ApiKeyVerdictGate>>) -> Result<()> {
    let gate = gate
        .lock()
        .map_err(|_| Error::programming("API key verdict test gate was poisoned"))?
        .take();
    if let Some(gate) = gate {
        gate.reached
            .send(())
            .map_err(|_| Error::programming("API key verdict test gate receiver disconnected"))?;
        gate.release
            .recv_timeout(Duration::from_secs(30))
            .map_err(|error| {
                Error::programming(format!(
                    "API key verdict test gate was not released: {error}"
                ))
            })?;
    }
    Ok(())
}
