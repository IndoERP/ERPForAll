//! Sessions (§4.3, §14.3): one `erp_session` row per logged-in
//! browser, in the tenant database, so any node serves any session.
//!
//! - The cookie carries a 32-byte random [`SessionSecret`] (43 base64url characters); the row
//!   stores only its SHA-256 (`sid_digest`), so a database read yields nothing a client can
//!   present.
//! - The row's [`SessionId`] (a UUIDv7) is the stable identity: rotation changes the secret, not
//!   the id, so CSRF tokens and audit references survive it.
//! - The row's `token` is an HMAC (key `erp:session-token:v1` of `database.secret`, kid in
//!   `token_kid`) over the session id and the user's id, login, password hash and active flag: a
//!   change of any of them makes every other session of the user fail its next check (SEC-059).
//! - The check runs in the transaction's opening flight ([`SessionCheck`], through
//!   `erp_orm::Orm::begin_credential`); touch, rotation and the end of sessions are short
//!   transactions on the primary, outside the request's transaction.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use erp_fields::{Error, ErrorKind, Result};
use erp_orm::{AccessPolicy, CredentialCheck, CredentialVerdict, Orm, ResolverStatement, UserId};
use erp_sql::param::{Param, Scalar};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio_postgres::Row;
use zeroize::{Zeroize, Zeroizing};

use super::secret::SessionKeys;
use super::settings::{AuthConfig, AuthSettings, CACHE_NAME};
use super::token_plan::TokenPlan;
use crate::resolver::Resolver;

/// The cookie secret of a session: 32 bytes from the OS CSPRNG. Never stored, never formatted.
///
/// The bytes live in one heap block from the moment they are made or decoded until the secret is
/// dropped (wiped then): moving a `SessionSecret` moves only the pointer, so no copy of the
/// secret is left behind on a stack.
pub struct SessionSecret(Box<[u8; 32]>);

impl SessionSecret {
    /// A fresh secret.
    ///
    /// # Errors
    ///
    /// `Programming` when the OS CSPRNG fails.
    pub fn generate() -> Result<Self> {
        let mut secret = Self(Box::new([0u8; 32]));
        getrandom::fill(secret.0.as_mut_slice()).map_err(|e| {
            Error::programming(format!("the OS random number generator failed: {e}"))
        })?;
        Ok(secret)
    }

    /// The secret a cookie value names: exactly 43 base64url characters without padding; `None`
    /// for anything else (a malformed cookie is no session, never an error).
    pub fn from_cookie(text: &str) -> Option<Self> {
        if text.len() != 43 {
            return None;
        }
        // 43 characters decode to 32 bytes; the decoder wants room for 33
        let mut bytes = Zeroizing::new([0u8; 33]);
        match URL_SAFE_NO_PAD.decode_slice(text, bytes.as_mut_slice()) {
            Ok(32) => {
                let mut secret = Self(Box::new([0u8; 32]));
                secret.0.copy_from_slice(&bytes[..32]);
                Some(secret)
            }
            _ => None,
        }
    }

    /// The cookie text (43 characters), wiped when dropped: the caller writes it into the
    /// `Set-Cookie` header and drops it.
    pub fn to_cookie(&self) -> Zeroizing<String> {
        Zeroizing::new(URL_SAFE_NO_PAD.encode(self.0.as_slice()))
    }

    /// The digest stored in `sid_digest` / `prev_digest`.
    pub fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_slice()).into()
    }
}

impl Drop for SessionSecret {
    fn drop(&mut self) {
        self.0.as_mut_slice().zeroize();
    }
}

impl fmt::Debug for SessionSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionSecret(<redacted>)")
    }
}

/// The id of a session row (a UUIDv7: 48-bit Unix milliseconds, version 7, random bits).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId([u8; 16]);

impl SessionId {
    /// A new UUIDv7 (made in Rust so the login's insert can be pipelined with its `COMMIT`).
    ///
    /// # Errors
    ///
    /// `Programming` when the OS CSPRNG fails.
    pub fn generate() -> Result<Self> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|e| {
            Error::programming(format!("the OS random number generator failed: {e}"))
        })?;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::programming("the system clock is before 1970"))?
            .as_millis();
        // 48 bits of milliseconds last until the year 10889
        let ms = (millis as u64).to_be_bytes();
        b[..6].copy_from_slice(&ms[2..8]);
        b[6] = 0x70 | (b[6] & 0x0f);
        b[8] = 0x80 | (b[8] & 0x3f);
        Ok(Self(b))
    }

    /// The id in PostgreSQL's text form (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`).
    pub fn to_text(&self) -> String {
        let mut s = String::with_capacity(36);
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                s.push('-');
            }
            s.push(char::from(HEX[usize::from(byte >> 4)]));
            s.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        s
    }

    /// The id of PostgreSQL's text form.
    ///
    /// # Errors
    ///
    /// `Database` for text that is not a hyphenated UUID.
    pub fn parse(text: &str) -> Result<Self> {
        let bytes = text.as_bytes();
        let well_formed =
            bytes.len() == 36 && [8usize, 13, 18, 23].iter().all(|&i| bytes[i] == b'-');
        if !well_formed {
            return Err(Error::db(format!("'{text}' is not a session id")));
        }
        let mut out = [0u8; 16];
        let mut source = 0;
        for byte in &mut out {
            if matches!(source, 8 | 13 | 18 | 23) {
                source += 1;
            }
            // The validated 36-byte layout and sixteen iterations bound these indexes.
            match (nibble(bytes[source]), nibble(bytes[source + 1])) {
                (Some(hi), Some(lo)) => *byte = (hi << 4) | lo,
                _ => return Err(Error::db(format!("'{text}' is not a session id"))),
            }
            source += 2;
        }
        Ok(Self(out))
    }

    /// The 16 bytes.
    pub fn bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hexadecimal of `bytes` (digests travel as text: `decode($n, 'hex')` in SQL).
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    s
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({})", self.to_text())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

/// The type of an added token field's value; its tag byte separates, for example, the integer 1
/// from the text "1" in the encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtraKind {
    /// A boolean (`true` / `false`).
    Bool,
    /// An integer.
    Int,
    /// Text: char, text and selection fields.
    Text,
    /// A many2one: the id of the target record.
    Id,
}

impl ExtraKind {
    const fn tag(self) -> u8 {
        match self {
            Self::Bool => b'b',
            Self::Int => b'i',
            Self::Text => b's',
            Self::Id => b'm',
        }
    }
}

/// Longest value of an added token field, bytes: a longer one is refused, never cut.
pub const MAX_EXTRA_VALUE_BYTES: usize = 64 * 1024;

/// One added field of the token plan with its current value (its text form as the database
/// renders it).
#[derive(Clone, Copy)]
pub struct ExtraValue<'a> {
    /// The field name.
    pub name: &'a str,
    /// The value's type.
    pub kind: ExtraKind,
    pub value: Option<&'a str>,
}

impl fmt::Debug for ExtraValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtraValue")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
pub struct TokenFields<'a> {
    /// The user.
    pub user: UserId,
    /// `res_users.login`.
    pub login: &'a str,
    /// `res_users.password` (the stored hash; empty when NULL).
    pub password: &'a str,
    /// `res_users.active`.
    pub active: bool,
    /// The fields the plan adds, in strictly increasing name order (the plan sorts them); empty for
    /// the base plan, whose token is byte-identical to the one made before plans existed.
    pub extra: &'a [ExtraValue<'a>],
}

impl fmt::Debug for TokenFields<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenFields")
            .field("user", &self.user)
            .field("login", &self.login)
            .field("extra", &self.extra)
            .finish_non_exhaustive()
    }
}

/// The token of session `id` for `fields` under `key`: HMAC-SHA256 over the length-prefixed
/// fields (a 4-byte big-endian length before each, so no two field lists share an encoding).
/// After the base parts come the added fields whose value is not `NULL`, each as its name and then
/// its one-byte type tag followed by the value, all length-prefixed.
///
/// # Errors
///
/// `Programming` when HMAC refuses the key (it accepts every length), a field is longer than
/// 4 GiB, an added value is longer than [`MAX_EXTRA_VALUE_BYTES`], or the added fields are not
/// in strictly increasing name order.
pub(crate) fn token(key: &[u8; 32], id: &SessionId, fields: &TokenFields<'_>) -> Result<[u8; 32]> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key)
        .map_err(|_| Error::programming("HMAC-SHA256 refused a 32-byte session token key"))?;
    let user = fields.user.get().to_be_bytes();
    let active = [u8::from(fields.active)];
    for part in [
        id.0.as_slice(),
        user.as_slice(),
        fields.login.as_bytes(),
        fields.password.as_bytes(),
        active.as_slice(),
    ] {
        update_part(&mut mac, &[part])?;
    }
    let mut previous: Option<&str> = None;
    for extra in fields.extra {
        if previous.is_some_and(|name| name >= extra.name) {
            return Err(Error::programming(
                "the session token fields are not in strictly increasing name order",
            ));
        }
        previous = Some(extra.name);
        let Some(value) = extra.value else {
            continue;
        };
        if value.len() > MAX_EXTRA_VALUE_BYTES {
            return Err(Error::programming(format!(
                "the value of the session token field '{}' is longer than {MAX_EXTRA_VALUE_BYTES} bytes",
                extra.name
            )));
        }
        update_part(&mut mac, &[extra.name.as_bytes()])?;
        update_part(&mut mac, &[&[extra.kind.tag()], value.as_bytes()])?;
    }
    Ok(mac.finalize().into_bytes().into())
}

/// One length-prefixed part made of `pieces` (the length is the pieces' total).
fn update_part(mac: &mut Hmac<Sha256>, pieces: &[&[u8]]) -> Result<()> {
    let total: usize = pieces.iter().map(|piece| piece.len()).sum();
    let len = u32::try_from(total)
        .map_err(|_| Error::programming("a session token field is longer than 4 GiB"))?;
    mac.update(&len.to_be_bytes());
    for piece in pieces {
        mac.update(piece);
    }
    Ok(())
}

/// A session accepted by the check.
#[derive(Debug, Clone)]
pub struct CheckedSession {
    id: SessionId,
    user: UserId,
    via_previous: bool,
    rotation_due: bool,
    touch_due: bool,
}

impl CheckedSession {
    /// The row id.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The user.
    pub fn user(&self) -> UserId {
        self.user
    }

    /// The request presented the previous secret, within its grace after a rotation.
    pub fn via_previous(&self) -> bool {
        self.via_previous
    }

    /// The secret is older than `session.rotation_interval_secs` ([`super::Auth::rotate`]).
    pub fn rotation_due(&self) -> bool {
        self.rotation_due
    }

    /// `last_seen_at` is older than `session.touch_interval_secs` ([`super::Auth::touch`]).
    pub fn touch_due(&self) -> bool {
        self.touch_due
    }
}

/// Longest stored user agent, characters.
const USER_AGENT_CHARS: usize = 1_024;

/// `agent` cut to the stored length: the one place a user agent is cut, for a login and a touch
/// alike (type design N6).
pub(crate) fn user_agent(agent: &str) -> String {
    agent.chars().take(USER_AGENT_CHARS).collect()
}

/// A session to touch ([`super::Auth::touch`]) with the address and user agent of the request
/// that used it (stored as `last_addr` and `last_agent`; the address as is, not its throttle key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    id: SessionId,
    addr: Option<std::net::IpAddr>,
    agent: Option<String>,
}

impl Touch {
    /// Touch `id`; its address and agent stay as they are unless set.
    pub fn new(id: SessionId) -> Self {
        Self {
            id,
            addr: None,
            agent: None,
        }
    }

    /// The request's client address (after the trusted-proxy rule).
    #[must_use]
    pub fn addr(mut self, addr: std::net::IpAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// The request's `User-Agent` (at most 1,024 characters are stored, cut here).
    #[must_use]
    pub fn agent(mut self, agent: &str) -> Self {
        self.agent = Some(user_agent(agent));
        self
    }

    /// The session.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The address, when set.
    pub fn address(&self) -> Option<std::net::IpAddr> {
        self.addr
    }

    /// The user agent, when set.
    pub fn user_agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }
}

/// Why a session check refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// No row has this digest (never created, logged out, collected, or a client-chosen value).
    NotFound,
    /// The row waits for a second factor; it authenticates nothing yet.
    Pending,
    /// The previous secret, presented after the rotation grace ("session is too old").
    TooOld,
    /// Idle longer than `sessions.max_inactivity_seconds`.
    Idle,
    /// Older than `sessions.max_lifetime_seconds`.
    Expired,
    /// The user is archived.
    UserInactive,
    /// The token no longer matches the user (password, login or active changed, or the secret
    /// was rotated by `erp db rotate-secret`).
    TokenMismatch,
    /// The row's `token_kid` is not a key this server holds (during a key rollout another node may
    /// hold it: the row is kept, security review Low 7).
    UnknownKid,
    /// The cached keys are older than the `config_parameter` generation the flight applied:
    /// reload and check again.
    KeysStale,
}

impl Refusal {
    /// Whether the row should be deleted (a found row that can never pass again). Not for
    /// [`Refusal::UnknownKid`]: the row may be valid on a node that holds the key.
    pub fn deletes(self) -> bool {
        // exhaustive: a new refusal has to choose
        match self {
            Self::Idle | Self::Expired | Self::UserInactive | Self::TokenMismatch => true,
            Self::NotFound | Self::Pending | Self::TooOld | Self::UnknownKid | Self::KeysStale => {
                false
            }
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::TooOld => "session is too old",
            Self::NotFound
            | Self::Pending
            | Self::Idle
            | Self::Expired
            | Self::UserInactive
            | Self::TokenMismatch
            | Self::UnknownKid
            | Self::KeysStale => "user is not connected",
        }
    }
}

/// A refused check, with the row to delete only when the reason deletes it (type design T9: the
/// invariant lives in the constructor, not in the caller).
#[derive(Debug, Clone, Copy)]
pub struct Refused {
    reason: Refusal,
    row: Option<SessionId>,
    /// The observed row version, never inferred by a caller constructing a refusal.
    version: Option<i64>,
}

impl Refused {
    /// The refusal of `row` (when one was found) for `reason`; the row is kept only when
    /// [`Refusal::deletes`].
    pub fn new(reason: Refusal, row: Option<SessionId>) -> Self {
        Self {
            reason,
            row: row.filter(|_| reason.deletes()),
            version: None,
        }
    }

    /// Why.
    pub fn reason(&self) -> Refusal {
        self.reason
    }

    /// The row to delete, if any.
    pub fn row_to_delete(&self) -> Option<SessionId> {
        self.row
    }

    /// Cleanup authority includes the version observed by the credential read. A manually
    /// constructed refusal carries no version and cannot authorize deferred deletion.
    pub(crate) fn observed_row(&self) -> Option<(SessionId, i64)> {
        self.row.zip(self.version)
    }
}

/// The `SessionExpired` error of `reason`.
pub fn session_expired(reason: Refusal) -> Error {
    Error::new(ErrorKind::SessionExpired, reason.message())
}

/// The cached settings a check decides with (shared with the login, `super::Auth`).
#[derive(Debug)]
pub(crate) struct Cached {
    pub(crate) generation: u64,
    pub(crate) settings: AuthSettings,
    pub(crate) keys: SessionKeys,
    pub(crate) throttle: super::throttle::ThrottleHasher,
    pub(crate) apikey: super::apikey::ApiKeyHasher,
    pub(crate) browser: Arc<super::browser::PrefsSigner>,
    /// The token plan of the registry (immutable; shared by every generation of the settings).
    pub(crate) token_plan: Arc<TokenPlan>,
}

/// The check of one presented secret, run by `Orm::begin_credential`.
pub(crate) struct SessionCheck<'a> {
    pub(crate) orm: &'a Orm,
    pub(crate) sql: &'a str,
    pub(crate) digest: [u8; 32],
    pub(crate) cached: Arc<Cached>,
    pub(crate) config: &'a AuthConfig,
}

/// Private wire contract: PostgreSQL's unsigned 32-bit xmin in the upper bits, current-digest
/// flag in bit zero. This is request-local optimistic deletion evidence, not a durable id.
/// Keeping the existing result width avoids a separate column for the observed version.
pub(super) const OBSERVATION_STAMP_SQL: &str =
    "s.xmin::text::int8 * 2 + CASE WHEN s.sid_digest = d.digest THEN 1 ELSE 0 END";

pub(super) struct ObservationStamp {
    version: i64,
    current: bool,
}

impl ObservationStamp {
    pub(super) fn version(&self) -> i64 {
        self.version
    }

    pub(super) fn current(&self) -> bool {
        self.current
    }

    pub(super) fn decode(encoded: i64) -> Result<Self> {
        const MAX: i64 = 2 * u32::MAX as i64 + 1;
        if !(0..=MAX).contains(&encoded) {
            return Err(Error::db(
                "session row stamp is outside the PostgreSQL xid/flag range",
            ));
        }
        Ok(Self {
            version: encoded / 2,
            current: encoded & 1 == 1,
        })
    }
}

/// The check statement over `subject_columns` (the resolver's, alias `u`); the columns of the
/// plan's added token fields (as text) follow `u.login, u.password`, from column 16.
pub(crate) fn check_sql(resolver: &Resolver, plan: &TokenPlan) -> String {
    format!(
        "SELECT {},\n       \
         s.id::text, {}, s.state, s.token, s.token_kid,\n       \
         extract(epoch FROM now() - COALESCE(s.rotated_at, s.created_at))::float8,\n       \
         extract(epoch FROM now() - s.last_seen_at)::float8,\n       \
         extract(epoch FROM now() - s.rotated_at)::float8,\n       \
         extract(epoch FROM s.expires_at - now())::float8,\n       \
         u.login, u.password{}\n  \
         FROM (SELECT decode($1::text, 'hex') AS digest) d\n  \
         JOIN erp_session s ON s.sid_digest = d.digest OR s.prev_digest = d.digest\n  \
         JOIN res_users u ON u.id = s.user_id",
        resolver.subject_columns(),
        OBSERVATION_STAMP_SQL,
        plan.select_list("u")
    )
}

fn col<'r, T: tokio_postgres::types::FromSql<'r>>(row: &'r Row, idx: usize) -> Result<T> {
    row.try_get(idx).map_err(|e| {
        Error::db(format!(
            "could not decode column {idx} of a session row: {e}"
        ))
        .with_source(e)
    })
}

fn secs(value: f64) -> Duration {
    if value.is_finite() && value > 0.0 {
        Duration::from_secs_f64(value)
    } else {
        Duration::ZERO
    }
}

impl CredentialCheck for SessionCheck<'_> {
    type Accepted = CheckedSession;
    type Refused = Refused;

    fn statement(&self) -> Result<ResolverStatement> {
        Ok(ResolverStatement::new(
            self.sql.to_owned(),
            vec![Param::Scalar(Scalar::Text(hex(&self.digest).into()))],
        ))
    }

    fn finish(
        &self,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> Result<CredentialVerdict<CheckedSession, Refused>> {
        let refuse = |reason: Refusal, row: Option<SessionId>| {
            Ok(CredentialVerdict::Refused(Refused::new(reason, row)))
        };
        let row = match rows {
            [] => return refuse(Refusal::NotFound, None),
            [row] => row,
            more => {
                return Err(Error::programming(format!(
                    "{} erp_session rows answered one session digest",
                    more.len()
                )));
            }
        };
        // columns 0..=4 are the subject's (Resolver::user_from_row)
        let id = SessionId::parse(&col::<String>(row, 5)?)?;
        let observation = ObservationStamp::decode(col(row, 6)?)?;
        let refuse = |reason: Refusal, row: Option<SessionId>| {
            let mut refused = Refused::new(reason, row);
            refused.version = refused.row.map(|_| observation.version());
            Ok(CredentialVerdict::Refused(refused))
        };
        let current = observation.current();
        let state: String = col(row, 7)?;
        let stored_token: Vec<u8> = col(row, 8)?;
        let kid: i16 = col(row, 9)?;
        let since_rotation = secs(col(row, 10)?);
        let since_seen = secs(col(row, 11)?);
        let since_rotated: Option<f64> = col(row, 12)?;
        let expires_in: f64 = col(row, 13)?;
        let login: String = col(row, 14)?;
        let password: Option<String> = col(row, 15)?;
        let extra_values = self.cached.token_plan.read_values(row, 16)?;
        // first: a stale cache must not refuse (and delete) a session the current settings
        // accept, e.g. right after max_inactivity_seconds was raised (security review round 2 N2)
        if self.orm.ormcache_generation(CACHE_NAME) != self.cached.generation {
            return refuse(Refusal::KeysStale, None);
        }
        if state != "active" {
            return refuse(Refusal::Pending, Some(id));
        }
        if !current {
            let within = since_rotated.is_some_and(|s| secs(s) <= self.config.rotation_grace());
            if !within {
                return refuse(Refusal::TooOld, None);
            }
        }
        let settings = &self.cached.settings;
        if since_seen >= settings.max_inactivity() {
            return refuse(Refusal::Idle, Some(id));
        }
        if !(expires_in.is_finite() && expires_in > 0.0) {
            return refuse(Refusal::Expired, Some(id));
        }
        let Some(principal) = Resolver::user_from_row(row, policy)? else {
            return refuse(Refusal::UserInactive, Some(id));
        };
        let Some(key) = self.cached.keys.key(kid) else {
            tracing::error!(
                target: "erp::auth",
                "session {id} has token_kid {kid}, which this server does not hold"
            );
            return refuse(Refusal::UnknownKid, Some(id));
        };
        let extra = self.cached.token_plan.bind(&extra_values)?;
        let expected = token(
            key.bytes(),
            &id,
            &TokenFields {
                user: principal.subject(),
                login: &login,
                password: password.as_deref().unwrap_or(""),
                active: true,
                extra: &extra,
            },
        )?;
        if !bool::from(expected.as_slice().ct_eq(&stored_token)) {
            return refuse(Refusal::TokenMismatch, Some(id));
        }
        let checked = CheckedSession {
            id,
            user: principal.subject(),
            via_previous: !current,
            rotation_due: current && since_rotation >= self.config.rotation_interval(),
            touch_due: since_seen >= self.config.touch_interval(),
        };
        Ok(CredentialVerdict::accepted(principal, checked))
    }
}

/// Rotation (`Auth::rotate`): the new digest replaces the current one only while the current one
/// is still the old one, so two concurrent rotations converge on one successor (READ COMMITTED:
/// the one that waited re-reads the row and matches nothing). A returned row: this one rotated.
pub(crate) const ROTATE_SQL: &str = "\
UPDATE erp_session SET prev_digest = sid_digest, sid_digest = decode($2::text, 'hex'),
       rotated_at = now()
 WHERE id = $1::text::uuid AND sid_digest = decode($3::text, 'hex') AND state = 'active'
RETURNING id";

/// Touch (`Auth::touch`): only rows not seen within the interval (no write otherwise), with the
/// address and user agent of the request that touched them, and the previous digest cleared once
/// its grace is over. The rows are locked first, in id order, skipping rows another transaction
/// holds (it is rotating, deleting or touching them; the next request touches again), so two
/// touches never lock in opposite orders (database review M1).
/// `$1`: ids (sorted, distinct), `$2`: addresses (text or NULL), `$3`: user agents (or NULL), `$4`:
/// the touch interval, `$5`: the rotation grace, seconds.
pub(crate) const TOUCH_SQL: &str = "\
UPDATE erp_session s
   SET last_seen_at = now(),
       last_addr = COALESCE(v.addr::inet, s.last_addr),
       last_agent = COALESCE(v.agent, s.last_agent),
       prev_digest = CASE WHEN s.rotated_at < now() - make_interval(secs => $5) THEN NULL
                          ELSE s.prev_digest END
  FROM unnest($1::text[], $2::text[], $3::text[]) AS v (id, addr, agent)
 WHERE s.id = v.id::uuid
   AND s.id IN (SELECT id FROM erp_session
                 WHERE id = ANY($1::text[]::uuid[])
                   AND last_seen_at < now() - make_interval(secs => $4)
                 ORDER BY id
                   FOR NO KEY UPDATE SKIP LOCKED)
RETURNING s.id";

#[cfg(test)]
mod parser_contract_tests {
    use super::SessionId;
    use erp_fields::{ErrorKind, Result};

    /// Characterize the existing public parser before removing its allocating scratch buffer.
    #[test]
    fn session_id_parser_preserves_uuid_text_contract() -> Result<()> {
        let id = SessionId::parse("01234567-89AB-CDEF-0123-456789ABCDEF")?;
        assert_eq!(
            id.bytes(),
            &[
                1, 35, 69, 103, 137, 171, 205, 239, 1, 35, 69, 103, 137, 171, 205, 239
            ]
        );
        assert_eq!(id.to_text(), "01234567-89ab-cdef-0123-456789abcdef");
        assert_eq!(
            SessionId::parse("00000000-0000-0000-0000-000000000000")?.bytes(),
            &[0; 16]
        );
        assert_eq!(
            SessionId::parse("ffffffff-ffff-ffff-ffff-ffffffffffff")?.bytes(),
            &[255; 16]
        );
        Ok(())
    }

    #[test]
    #[allow(clippy::unwrap_used, reason = "parser contract assertions")]
    fn session_id_parser_preserves_malformed_error() {
        for text in [
            "",
            "0123456789abcdef0123456789abcdef",
            "01234567_89ab-cdef-0123-456789abcdef",
            "01234567-89ab-cdef-0123-456789abcde-",
            "01234567-89ab-cdef-0123-456789abcdeg",
            "01234567-89ab-cdef-0123-456789abcdef0",
            "é123456-89ab-cdef-0123-456789abcdef",
        ] {
            let error = SessionId::parse(text).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Database);
            assert_eq!(error.message(), format!("'{text}' is not a session id"));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "token encoding contract assertions")]
mod token_encoding_tests {
    use super::{
        ExtraKind, ExtraValue, MAX_EXTRA_VALUE_BYTES, SessionId, TokenFields, UserId, token,
    };
    use erp_fields::ErrorKind;
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    const KEY: [u8; 32] = [7; 32];

    fn id() -> SessionId {
        SessionId::parse("01234567-89ab-cdef-0123-456789abcdef").unwrap()
    }

    fn fields<'a>(extra: &'a [ExtraValue<'a>]) -> TokenFields<'a> {
        TokenFields {
            user: UserId::new(42).unwrap(),
            login: "bob",
            password: "hash",
            active: true,
            extra,
        }
    }

    fn text<'a>(name: &'a str, value: Option<&'a str>) -> ExtraValue<'a> {
        ExtraValue {
            name,
            kind: ExtraKind::Text,
            value,
        }
    }

    /// The encoding that shipped before plans existed, written out again here (an independent
    /// characterization): the base plan must not change any session's token.
    #[test]
    fn the_base_plan_keeps_the_encoding_sessions_already_have() {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&KEY).unwrap();
        for part in [
            id().bytes().as_slice(),
            &42_i64.to_be_bytes(),
            b"bob",
            b"hash",
            &[1],
        ] {
            mac.update(&u32::try_from(part.len()).unwrap().to_be_bytes());
            mac.update(part);
        }
        let legacy: [u8; 32] = mac.finalize().into_bytes().into();
        assert_eq!(token(&KEY, &id(), &fields(&[])).unwrap(), legacy);
    }

    #[test]
    fn a_null_added_field_adds_nothing_and_a_value_changes_the_token() {
        let base = token(&KEY, &id(), &fields(&[])).unwrap();
        let null = [text("tok_a", None)];
        assert_eq!(token(&KEY, &id(), &fields(&null)).unwrap(), base);
        let set = [text("tok_a", Some("x"))];
        let with_x = token(&KEY, &id(), &fields(&set)).unwrap();
        assert_ne!(with_x, base);
        let other = [text("tok_a", Some("y"))];
        assert_ne!(token(&KEY, &id(), &fields(&other)).unwrap(), with_x);
        // an empty string is a value, not NULL
        let empty = [text("tok_a", Some(""))];
        assert_ne!(token(&KEY, &id(), &fields(&empty)).unwrap(), base);
    }

    #[test]
    fn name_kind_and_position_are_part_of_the_encoding() {
        let a = [text("tok_a", Some("1"))];
        let b = [text("tok_b", Some("1"))];
        assert_ne!(
            token(&KEY, &id(), &fields(&a)).unwrap(),
            token(&KEY, &id(), &fields(&b)).unwrap()
        );
        let int = [ExtraValue {
            name: "tok_a",
            kind: ExtraKind::Int,
            value: Some("1"),
        }];
        assert_ne!(
            token(&KEY, &id(), &fields(&a)).unwrap(),
            token(&KEY, &id(), &fields(&int)).unwrap()
        );
        // "ab" + "c" is not "a" + "bc"
        let split_one = [text("a", Some("bc"))];
        let split_two = [text("ab", Some("c"))];
        assert_ne!(
            token(&KEY, &id(), &fields(&split_one)).unwrap(),
            token(&KEY, &id(), &fields(&split_two)).unwrap()
        );
    }

    #[test]
    fn unordered_or_oversized_fields_are_programming_errors() {
        let unordered = [text("b", Some("1")), text("a", Some("1"))];
        let error = token(&KEY, &id(), &fields(&unordered)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "the session token fields are not in strictly increasing name order"
        );
        let repeated = [text("a", None), text("a", None)];
        assert_eq!(
            token(&KEY, &id(), &fields(&repeated)).unwrap_err().kind(),
            ErrorKind::Programming
        );
        let big = "x".repeat(MAX_EXTRA_VALUE_BYTES + 1);
        let oversized = [text("tok_a", Some(&big))];
        let error = token(&KEY, &id(), &fields(&oversized)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "the value of the session token field 'tok_a' is longer than 65536 bytes"
        );
        let largest = "x".repeat(MAX_EXTRA_VALUE_BYTES);
        let at_limit = [text("tok_a", Some(&largest))];
        assert!(token(&KEY, &id(), &fields(&at_limit)).is_ok());
    }
}
