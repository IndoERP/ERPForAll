//! The settings of authentication.
//!
//! - [`AuthConfig`]: per process, from the server's configuration (style keys), validated
//!   when built; a value outside its floor and hard bound is an error naming the setting.
//! - [`AuthSettings`]: per database, from `ir.config_parameter` rows.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use erp_fields::{Error, Result};
use erp_orm::Transaction;

use super::password::{HashParams, PasswordError, Scheme, limits};
use super::secret::{DatabaseSecret, SecretError, SessionKeys};
use super::throttle::ThrottleHasher;

/// `ir.config_parameter` keys read by authentication.
pub mod keys {
    /// The scheme of new password hashes.
    pub const SCHEME: &str = "password.hashing.scheme";
    pub const ROUNDS: &str = "password.hashing.rounds";
    /// Argon2id memory, KiB.
    pub const ARGON2_MEMORY: &str = "password.hashing.argon2_memory_kib";
    /// Argon2id passes.
    pub const ARGON2_TIME: &str = "password.hashing.argon2_time";
    pub const COOLDOWN_AFTER: &str = "base.login_cooldown_after";
    pub const COOLDOWN_DURATION: &str = "base.login_cooldown_duration";
    /// Failures per login before a cooldown (lead's answer 1).
    pub const COOLDOWN_AFTER_LOGIN: &str = "base.login_cooldown_after_login";
    pub const MAX_INACTIVITY: &str = "sessions.max_inactivity_seconds";
    /// Absolute lifetime of a session, seconds (D-AUTH-5).
    pub const MAX_LIFETIME: &str = "sessions.max_lifetime_seconds";
    /// The database secret.
    pub const SECRET: &str = "database.secret";

    /// Every key, in the order the settings statement asks for them.
    pub const ALL: [&str; 10] = [
        SCHEME,
        ROUNDS,
        ARGON2_MEMORY,
        ARGON2_TIME,
        COOLDOWN_AFTER,
        COOLDOWN_DURATION,
        COOLDOWN_AFTER_LOGIN,
        MAX_INACTIVITY,
        MAX_LIFETIME,
        SECRET,
    ];
}

pub const COOLDOWN_AFTER_FALLBACK: u32 = 5;
pub const COOLDOWN_DURATION_FALLBACK: u32 = 60;
/// Default idle limit (one week).
pub const MAX_INACTIVITY_DEFAULT: u32 = 604_800;
/// Default absolute lifetime (90 days).
pub const MAX_LIFETIME_DEFAULT: u32 = 7_776_000;

/// The ormcache name whose generation tells whether cached settings are current; every write
/// through `ir.config_parameter` invalidates it (`cache:config_parameter` across processes).
pub const CACHE_NAME: &str = "config_parameter";

/// A login cooldown rule: after `after` failures, `duration` per attempt (0 disables: only the
/// per-login rule can be 0, `base.login_cooldown_after` refuses it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cooldown {
    after: u32,
    duration: Duration,
}

impl Cooldown {
    /// A rule of the login cooldown policy (`auth::cooldown_policy`): already range-checked there.
    pub(crate) const fn new(after: u32, duration: Duration) -> Self {
        Self { after, duration }
    }

    /// Failures before the cooldown; 0 disables the rule (per-login rule only).
    pub const fn after(&self) -> u32 {
        self.after
    }

    /// How long one cooldown lasts after the last failure.
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    pub fn in_force(&self, failures: u32, since_last: Duration) -> bool {
        self.after != 0 && failures >= self.after && since_last < self.duration
    }
}

/// The settings of one database (`ir.config_parameter`), validated.
#[derive(Debug)]
pub struct AuthSettings {
    params: HashParams,
    address_cooldown: Cooldown,
    login_cooldown: Cooldown,
    max_inactivity: Duration,
    max_lifetime: Duration,
}

impl AuthSettings {
    /// The hashing parameters of new hashes.
    pub fn hash_params(&self) -> &HashParams {
        &self.params
    }

    /// The cooldown per client address.
    pub fn address_cooldown(&self) -> Cooldown {
        self.address_cooldown
    }

    /// The cooldown per login.
    pub fn login_cooldown(&self) -> Cooldown {
        self.login_cooldown
    }

    /// The idle limit of a session.
    pub fn max_inactivity(&self) -> Duration {
        self.max_inactivity
    }

    /// The absolute lifetime of a session.
    pub fn max_lifetime(&self) -> Duration {
        self.max_lifetime
    }

    /// # Errors
    ///
    /// `Validation` naming the key for a value that does not parse or is out of range.
    pub fn from_rows(rows: &HashMap<String, Option<String>>) -> Result<Self> {
        let get = |key: &str| {
            rows.get(key)
                .and_then(|v| v.as_deref())
                .filter(|v| !v.is_empty())
        };
        let int = |key: &'static str, min: u32, max: u32| -> Result<Option<u32>> {
            get(key).map(|v| parse_u32(key, v, min, max)).transpose()
        };
        let params = hash_params_from_rows(rows)?;
        let after = int(keys::COOLDOWN_AFTER, 1, 1_000_000)?.unwrap_or(COOLDOWN_AFTER_FALLBACK);
        let duration =
            int(keys::COOLDOWN_DURATION, 1, 604_800)?.unwrap_or(COOLDOWN_DURATION_FALLBACK);
        let after_login = match int(keys::COOLDOWN_AFTER_LOGIN, 0, 4_000_000)? {
            Some(n) => n,
            None => after.saturating_mul(4),
        };
        let duration = Duration::from_secs(u64::from(duration));
        let max_inactivity =
            int(keys::MAX_INACTIVITY, 60, 31_536_000)?.unwrap_or(MAX_INACTIVITY_DEFAULT);
        let max_lifetime =
            int(keys::MAX_LIFETIME, 3_600, 31_536_000)?.unwrap_or(MAX_LIFETIME_DEFAULT);
        Ok(Self {
            params,
            address_cooldown: Cooldown { after, duration },
            login_cooldown: Cooldown {
                after: after_login,
                duration,
            },
            max_inactivity: Duration::from_secs(u64::from(max_inactivity)),
            max_lifetime: Duration::from_secs(u64::from(max_lifetime)),
        })
    }
}

/// The hashing parameters of new hashes from `rows` (absent keys: the floors of §3.1, which are
/// the defaults).
///
/// # Errors
///
/// `Validation` naming the key for a malformed value or one below its floor.
pub fn hash_params_from_rows(rows: &HashMap<String, Option<String>>) -> Result<HashParams> {
    let get = |key: &str| {
        rows.get(key)
            .and_then(|v| v.as_deref())
            .filter(|v| !v.is_empty())
    };
    let int = |key: &'static str| -> Result<Option<u32>> {
        get(key).map(|v| parse_u32(key, v, 0, u32::MAX)).transpose()
    };
    let scheme = get(keys::SCHEME)
        .map(Scheme::parse)
        .transpose()
        .map_err(Error::from)?
        .unwrap_or(Scheme::Argon2id);
    match scheme {
        Scheme::Pbkdf2Sha512 => {
            HashParams::pbkdf2_sha512(int(keys::ROUNDS)?.unwrap_or(limits::PBKDF2_ROUNDS_MIN))
        }
        Scheme::Argon2id => HashParams::argon2id(
            int(keys::ARGON2_MEMORY)?.unwrap_or(limits::ARGON2_MEMORY_KIB_MIN),
            int(keys::ARGON2_TIME)?.unwrap_or(limits::ARGON2_TIME_MIN),
        ),
    }
    .map_err(|e: PasswordError| Error::from(e))
}

/// Check the value written to the `ir.config_parameter` key `key` (`None` or empty: the
/// parameter is undefined) with the parsers the settings are read with, so a value that would
/// make every login and session check fail is refused when it is written (round 1 decision 5).
/// Keys that are not settings of authentication are accepted. Each hashing value is checked
/// against its own floor and bound whatever the configured scheme, so switching the scheme later
/// cannot expose a bad value.
///
/// # Errors
///
/// `Validation` naming the key and the value (never the value of `database.secret`): a value
/// that does not parse or is out of range, `base.login_cooldown_after` = 0, an empty
/// `database.secret`.
pub fn validate_setting(key: &str, value: Option<&str>) -> Result<()> {
    let Some(&key) = keys::ALL.iter().find(|k| **k == key) else {
        return Ok(());
    };
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        if key == keys::SECRET {
            return Err(Error::validation(
                "ir.config_parameter database.secret cannot be empty: every login and session \
                 check derives its keys from it",
            ));
        }
        return Ok(());
    };
    let whole = |v: &str| parse_u32(key, v, 0, u32::MAX);
    let hash = |e: PasswordError| hash_refusal(key, value, e);
    match key {
        keys::SCHEME => Scheme::parse(value).map(drop).map_err(hash),
        keys::ROUNDS => HashParams::pbkdf2_sha512(whole(value)?)
            .map(drop)
            .map_err(hash),
        keys::ARGON2_MEMORY => HashParams::argon2id(whole(value)?, limits::ARGON2_TIME_MIN)
            .map(drop)
            .map_err(hash),
        keys::ARGON2_TIME => HashParams::argon2id(limits::ARGON2_MEMORY_KIB_MIN, whole(value)?)
            .map(drop)
            .map_err(hash),
        // the message names the key and the length, never the value
        keys::SECRET => DatabaseSecret::from_stored(value)
            .map(drop)
            .map_err(|e| Error::validation(e.to_string()).with_source(e)),
        keys::COOLDOWN_AFTER
        | keys::COOLDOWN_DURATION
        | keys::COOLDOWN_AFTER_LOGIN
        | keys::MAX_INACTIVITY
        | keys::MAX_LIFETIME => {
            let rows = HashMap::from([(key.to_owned(), Some(value.to_owned()))]);
            AuthSettings::from_rows(&rows).map(drop)
        }
        other => Err(Error::programming(format!(
            "the authentication setting {other} has no write-time parser: add it to \
             settings::validate_setting"
        ))),
    }
}

/// A hashing value refused when written: `Validation` naming the `ir.config_parameter` key and
/// the value, whatever the parameter the hasher's error names.
fn hash_refusal(key: &str, value: &str, e: PasswordError) -> Error {
    let why = match &e {
        PasswordError::ParamOutOfRange { min, max, .. } => format!("is outside {min}..={max}"),
        PasswordError::UnknownScheme { .. } => {
            "is not a scheme: expected 'argon2id' or 'pbkdf2_sha512'".to_owned()
        }
        other => format!("is refused: {other}"),
    };
    Error::validation(format!(
        "ir.config_parameter {key} = '{}' {why}",
        super::password::bounded(value)
    ))
    .with_source(e)
}

/// The hashing keys of `ir.config_parameter`.
const HASH_KEYS: [&str; 4] = [
    keys::SCHEME,
    keys::ROUNDS,
    keys::ARGON2_MEMORY,
    keys::ARGON2_TIME,
];

/// Read the hashing parameters of new hashes in `tx` (one statement, after flushing pending
/// `ir.config_parameter` writes): what setting a password needs, without the secret.
///
/// # Errors
///
/// As [`load`], without the secret's.
pub async fn load_hash_params(tx: &mut Transaction) -> Result<HashParams> {
    let map = read_rows(tx, &HASH_KEYS).await?;
    hash_params_from_rows(&map)
}

/// Destructive GC policy read inside one batch. Both cooldown kinds currently share the
/// duration key; keep that rule in the same parser as authentication settings.
pub(crate) struct GcPolicy {
    pub(crate) max_inactivity: Duration,
    pub(crate) cooldown: Duration,
}

pub(crate) async fn load_gc_policy(tx: &mut Transaction) -> Result<GcPolicy> {
    let rows = read_rows(tx, &[keys::COOLDOWN_DURATION, keys::MAX_INACTIVITY]).await?;
    // The existing parser owns validation/defaults. Only the two requested fields escape
    // this helper; unrelated defaults are not exposed as a complete settings snapshot.
    let parsed = AuthSettings::from_rows(&rows)?;
    Ok(GcPolicy {
        max_inactivity: parsed.max_inactivity(),
        cooldown: parsed
            .address_cooldown()
            .duration()
            .max(parsed.login_cooldown().duration()),
    })
}

async fn read_rows(
    tx: &mut Transaction,
    names: &[&str],
) -> Result<HashMap<String, Option<String>>> {
    let model = tx
        .env()
        .registry()
        .model_id("ir.config_parameter")
        .ok_or_else(|| {
            Error::programming(
                "authentication needs the model ir.config_parameter (addon base) in the registry",
            )
        })?;
    tx.flush_model(model).await?;
    let rows = tx.query(SETTINGS_SQL, &[&names]).await?;
    rows_to_map(&rows)
}

/// The `(key, value)` rows of [`SETTINGS_SQL`] by key.
pub(crate) fn rows_to_map(rows: &[tokio_postgres::Row]) -> Result<HashMap<String, Option<String>>> {
    let mut map: HashMap<String, Option<String>> = HashMap::with_capacity(rows.len());
    for row in rows {
        let key: String = row
            .try_get(0)
            .map_err(|e| Error::db(format!("ir_config_parameter.key: {e}")).with_source(e))?;
        let value: Option<String> = row
            .try_get(1)
            .map_err(|e| Error::db(format!("ir_config_parameter.value: {e}")).with_source(e))?;
        map.insert(key, value);
    }
    Ok(map)
}

/// A decimal `u32` within `min..=max`, digits only.
pub(crate) fn parse_u32(key: &str, value: &str, min: u32, max: u32) -> Result<u32> {
    let parsed =
        if !value.is_empty() && value.len() <= 10 && value.bytes().all(|b| b.is_ascii_digit()) {
            value.parse::<u64>().ok()
        } else {
            None
        };
    match parsed {
        Some(n) if (u64::from(min)..=u64::from(max)).contains(&n) => {
            u32::try_from(n).map_err(|_| out_of_range(key, value, min, max))
        }
        _ => Err(out_of_range(key, value, min, max)),
    }
}

fn out_of_range(key: &str, value: &str, min: u32, max: u32) -> Error {
    Error::validation(format!(
        "ir.config_parameter {key} = '{}' is not a whole number in {min}..={max}",
        super::password::bounded(value)
    ))
}

/// The settings and keys of one database as read in one statement.
#[derive(Debug)]
pub struct Loaded {
    /// The settings.
    pub settings: AuthSettings,
    /// The session token keys.
    pub keys: SessionKeys,
    /// Purpose-separated native API-key digest state from this secret generation.
    pub apikey: super::apikey::ApiKeyHasher,
    /// The keyed hash of the login throttle's rows.
    pub throttle: ThrottleHasher,
    /// Purpose-limited browser signing keys from the same secret generation.
    pub browser: std::sync::Arc<super::browser::PrefsSigner>,
}

/// The rows of the settings `$1` (keys).
pub(crate) const SETTINGS_SQL: &str =
    "SELECT key, value FROM ir_config_parameter WHERE key = ANY($1::text[]) ORDER BY key";

/// Read the settings and keys in `tx` (one statement, after flushing pending
/// `ir.config_parameter` writes).
///
/// # Errors
///
/// `Programming` when `ir.config_parameter` is not in the registry or `database.secret` is
/// missing; `Validation` for a malformed value; the statement's error.
pub async fn load(tx: &mut Transaction) -> Result<Loaded> {
    loaded_from(read_rows(tx, &keys::ALL).await?)
}

/// The settings and keys of `map` (the rows of every key of [`keys::ALL`]).
///
/// # Errors
///
/// As [`load`], without the statement's.
pub(crate) fn loaded_from(mut map: HashMap<String, Option<String>>) -> Result<Loaded> {
    let secret = match map.remove(keys::SECRET).flatten() {
        Some(text) => {
            let secret = DatabaseSecret::from_stored(&text);
            // the text is a secret too: wipe our copy
            drop(zeroize::Zeroizing::new(text));
            secret?
        }
        None => return Err(SecretError::Missing.into()),
    };
    Ok(Loaded {
        settings: AuthSettings::from_rows(&map)?,
        keys: SessionKeys::derive(&secret)?,
        apikey: super::apikey::ApiKeyHasher::derive(&secret)?,
        throttle: ThrottleHasher::derive(&secret)?,
        browser: std::sync::Arc::new(
            super::browser::PrefsSigner::derive(&secret)
                .map_err(|_| Error::programming("browser token key derivation failed"))?,
        ),
    })
}

/// Refuse hashing parameters whose job needs more memory than a permit of the hashing pool
/// reserves (`reserved_kib`, `admission.hash_memory_kib`): no rehash could be written, and a hash
/// stored at those parameters (by a node that reserves more) would be refused at every login
/// (security review round 2 M1, delta SD-1).
///
/// # Errors
///
/// `Validation` naming both settings.
pub fn check_reservation(params: &HashParams, reserved_kib: u32) -> Result<()> {
    let needed = params.memory_kib();
    if needed > reserved_kib {
        return Err(Error::validation(format!(
            "ir.config_parameter {} = {needed} KiB is more than the {reserved_kib} KiB a hashing \
             permit reserves (admission.hash_memory_kib): raise admission.hash_memory_kib on every \
             node first, or lower {}",
            keys::ARGON2_MEMORY,
            keys::ARGON2_MEMORY
        )));
    }
    Ok(())
}

/// [`validate_setting`], and for `password.hashing.argon2_memory_kib` also
/// [`check_reservation`] against the hashing pool of `env`'s database (security review round 2
/// M1): a value the pool cannot run is refused when it is written. The check reads the value
/// alone (whatever the configured scheme, like [`validate_setting`]) and only against the writing
/// node's pool. A node that reserves less (a cluster whose nodes differ in
/// `admission.hash_memory_kib`), or a scheme switched to `argon2id` over a memory value written
/// earlier, reaches a running service: it logs the parameters once per settings generation,
/// verifies dummies within its own reservation, refuses and counts stored hashes over it, and
/// skips rehashes, until it restarts, where [`check_reservation`] stops it (security review
/// round 2 delta SD-1).
///
/// # Errors
///
/// As [`validate_setting`] and [`check_reservation`].
pub fn validate_setting_in(env: &erp_orm::Env, key: &str, value: Option<&str>) -> Result<()> {
    validate_setting(key, value)?;
    if key == keys::ARGON2_MEMORY
        && let Some(value) = value.filter(|v| !v.is_empty())
    {
        let memory = parse_u32(key, value, 0, u32::MAX)?;
        let params = HashParams::argon2id(memory, limits::ARGON2_TIME_MIN).map_err(Error::from)?;
        check_reservation(&params, super::pool::for_env(env).config().memory_kib())?;
    }
    Ok(())
}

/// Read and check the stored settings and keys of `orm`'s database once, in a short read-only
/// transaction on the primary: where the authentication service starts, so a bad row written
/// around the model (SQL, an older revision) is reported at startup, naming the key, instead of
/// refusing every login (review of R1B, silent 9). It reads what [`load`] reads and returns it;
/// `Auth::new` keeps it as its first cached generation (rust review round 3 RD-7).
///
/// # Errors
///
/// As [`load`]; the transaction's.
pub async fn check_stored(orm: &erp_orm::Orm) -> Result<Loaded> {
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID)
                .on_primary()
                .read_only("ir.config_parameter.check_auth_settings"),
        )
        .await?;
    let outcome = load(&mut tx).await;
    match (outcome, tx.rollback().await) {
        (Ok(loaded), Ok(())) => Ok(loaded),
        (Err(e), Ok(())) | (Ok(_), Err(e)) => Err(e),
        (Err(e), Err(r)) => Err(e.and_also(&r)),
    }
}

/// Settings of the process (the server's configuration), validated.
///
/// ```
/// use erp_security::auth::settings::AuthConfig;
/// let c = AuthConfig::DEFAULT.with_credential_bytes(64)?;
/// assert_eq!(c.credential_bytes(), 64);
/// assert_eq!(
///     AuthConfig::DEFAULT.with_rotation_grace_secs(601).unwrap_err().to_string(),
///     "session.rotation_grace_secs = 601 is outside 0..=600"
/// );
/// # Ok::<(), erp_security::auth::settings::ConfigError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AuthConfig {
    touch_interval: Duration,
    rotation_interval: Duration,
    rotation_grace: Duration,
    credential_bytes: usize,
    login_burst: u32,
    login_rate_per_sec: f64,
    throttle_entries: usize,
    cooldown_hook_deadline: Duration,
    token_plan_hook_deadline: Duration,
}

impl AuthConfig {
    /// The defaults of the design note (§4.1, §4.3, §14.5).
    pub const DEFAULT: Self = Self {
        touch_interval: Duration::from_secs(60),
        rotation_interval: Duration::from_secs(10_800),
        rotation_grace: Duration::from_secs(120),
        credential_bytes: 1_024,
        login_burst: 20,
        login_rate_per_sec: 2.0,
        throttle_entries: 4_096,
        cooldown_hook_deadline: Duration::from_secs(5),
        token_plan_hook_deadline: Duration::from_secs(10),
    };

    /// `session.touch_interval_secs` (1 – 3,600).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the setting.
    pub fn with_touch_interval_secs(mut self, secs: u32) -> Result<Self, ConfigError> {
        bound("session.touch_interval_secs", f64::from(secs), 1.0, 3_600.0)?;
        self.touch_interval = Duration::from_secs(u64::from(secs));
        Ok(self)
    }

    /// `session.rotation_interval_secs` (60 – 86,400).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the setting.
    pub fn with_rotation_interval_secs(mut self, secs: u32) -> Result<Self, ConfigError> {
        bound(
            "session.rotation_interval_secs",
            f64::from(secs),
            60.0,
            86_400.0,
        )?;
        self.rotation_interval = Duration::from_secs(u64::from(secs));
        Ok(self)
    }

    /// `session.rotation_grace_secs` (0 – 600).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the setting.
    pub fn with_rotation_grace_secs(mut self, secs: u32) -> Result<Self, ConfigError> {
        bound("session.rotation_grace_secs", f64::from(secs), 0.0, 600.0)?;
        self.rotation_grace = Duration::from_secs(u64::from(secs));
        Ok(self)
    }

    /// `limits.credential_bytes` (64 – 4,096).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the setting.
    pub fn with_credential_bytes(mut self, bytes: u32) -> Result<Self, ConfigError> {
        bound("limits.credential_bytes", f64::from(bytes), 64.0, 4_096.0)?;
        self.credential_bytes = bytes as usize;
        Ok(self)
    }

    /// `throttle.login_burst` (5 – 1,000) and `throttle.login_rate_per_sec` (0.1 – 1,000).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the setting.
    pub fn with_login_prefilter(
        mut self,
        burst: u32,
        rate_per_sec: f64,
    ) -> Result<Self, ConfigError> {
        bound("throttle.login_burst", f64::from(burst), 5.0, 1_000.0)?;
        bound("throttle.login_rate_per_sec", rate_per_sec, 0.1, 1_000.0)?;
        self.login_burst = burst;
        self.login_rate_per_sec = rate_per_sec;
        Ok(self)
    }

    /// `throttle.table_entries` (64 – 1,048,576).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the setting.
    pub fn with_throttle_entries(mut self, entries: u32) -> Result<Self, ConfigError> {
        bound(
            "throttle.table_entries",
            f64::from(entries),
            64.0,
            1_048_576.0,
        )?;
        self.throttle_entries = entries as usize;
        Ok(self)
    }

    /// `limits.cooldown_hook_deadline_ms` (10 – 60,000): longest one `_on_login_cooldown` call
    /// may take before the attempt is refused.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the setting.
    pub fn with_cooldown_hook_deadline_ms(mut self, ms: u32) -> Result<Self, ConfigError> {
        bound(
            "limits.cooldown_hook_deadline_ms",
            f64::from(ms),
            10.0,
            60_000.0,
        )?;
        self.cooldown_hook_deadline = Duration::from_millis(u64::from(ms));
        Ok(self)
    }

    /// `limits.token_plan_hook_deadline_ms` (10 – 60,000): longest the
    /// `_get_session_token_fields` hook may take when the service is built (H58 TOKEN).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the setting.
    pub fn with_token_plan_hook_deadline_ms(mut self, ms: u32) -> Result<Self, ConfigError> {
        bound(
            "limits.token_plan_hook_deadline_ms",
            f64::from(ms),
            10.0,
            60_000.0,
        )?;
        self.token_plan_hook_deadline = Duration::from_millis(u64::from(ms));
        Ok(self)
    }

    /// How often a session's `last_seen_at` is written at most.
    pub const fn touch_interval(&self) -> Duration {
        self.touch_interval
    }

    /// Age after which a session's secret is rotated.
    pub const fn rotation_interval(&self) -> Duration {
        self.rotation_interval
    }

    /// How long the previous secret keeps working after a rotation.
    pub const fn rotation_grace(&self) -> Duration {
        self.rotation_grace
    }

    /// Longest login or password accepted, bytes.
    pub const fn credential_bytes(&self) -> usize {
        self.credential_bytes
    }

    /// Pre-filter burst per address.
    pub const fn login_burst(&self) -> u32 {
        self.login_burst
    }

    /// Pre-filter refill per address, per second.
    pub const fn login_rate_per_sec(&self) -> f64 {
        self.login_rate_per_sec
    }

    /// Pre-filter table size.
    pub const fn throttle_entries(&self) -> usize {
        self.throttle_entries
    }

    /// Longest one `_on_login_cooldown` call may take.
    pub const fn cooldown_hook_deadline(&self) -> Duration {
        self.cooldown_hook_deadline
    }

    /// Longest the `_get_session_token_fields` hook may take when the service is built.
    pub const fn token_plan_hook_deadline(&self) -> Duration {
        self.token_plan_hook_deadline
    }
}

fn bound(setting: &'static str, value: f64, min: f64, max: f64) -> Result<(), ConfigError> {
    if value.is_finite() && (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError {
            setting,
            value,
            min,
            max,
        })
    }
}

/// A process setting outside its floor and hard bound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfigError {
    setting: &'static str,
    value: f64,
    min: f64,
    max: f64,
}

impl ConfigError {
    /// The setting's name.
    pub const fn setting(&self) -> &'static str {
        self.setting
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} = {} is outside {}..={}",
            self.setting, self.value, self.min, self.max
        )
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        Error::validation(e.to_string()).with_source(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cooldown_hook_deadline_is_bounded() {
        assert_eq!(
            AuthConfig::DEFAULT.cooldown_hook_deadline(),
            Duration::from_secs(5)
        );
        let ok = AuthConfig::DEFAULT
            .with_cooldown_hook_deadline_ms(10)
            .unwrap();
        assert_eq!(ok.cooldown_hook_deadline(), Duration::from_millis(10));
        for bad in [0, 9, 60_001] {
            let e = AuthConfig::DEFAULT
                .with_cooldown_hook_deadline_ms(bad)
                .unwrap_err();
            assert_eq!(e.setting(), "limits.cooldown_hook_deadline_ms");
        }
    }

    #[test]
    fn the_token_plan_hook_deadline_is_bounded() {
        assert_eq!(
            AuthConfig::DEFAULT.token_plan_hook_deadline(),
            Duration::from_secs(10)
        );
        let ok = AuthConfig::DEFAULT
            .with_token_plan_hook_deadline_ms(50)
            .unwrap();
        assert_eq!(ok.token_plan_hook_deadline(), Duration::from_millis(50));
        for bad in [0, 9, 60_001] {
            let e = AuthConfig::DEFAULT
                .with_token_plan_hook_deadline_ms(bad)
                .unwrap_err();
            assert_eq!(e.setting(), "limits.token_plan_hook_deadline_ms");
        }
    }

    fn rows(pairs: &[(&str, &str)]) -> HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Some((*v).to_owned())))
            .collect()
    }

    #[test]
    fn absent_rows_take_the_fallbacks() -> Result<()> {
        let s = AuthSettings::from_rows(&HashMap::new())?;
        assert_eq!(*s.hash_params(), HashParams::DEFAULT);
        assert_eq!(s.address_cooldown().after(), 5);
        assert_eq!(s.login_cooldown().after(), 20);
        assert_eq!(s.address_cooldown().duration(), Duration::from_secs(60));
        assert_eq!(s.max_inactivity(), Duration::from_secs(604_800));
        assert_eq!(s.max_lifetime(), Duration::from_secs(7_776_000));
        Ok(())
    }

    #[test]
    fn shipped_rows_and_the_login_factor() -> Result<()> {
        let s = AuthSettings::from_rows(&rows(&[
            (keys::COOLDOWN_AFTER, "10"),
            (keys::COOLDOWN_DURATION, "60"),
        ]))?;
        assert_eq!(
            (s.address_cooldown().after(), s.login_cooldown().after()),
            (10, 40)
        );
        let off = AuthSettings::from_rows(&rows(&[(keys::COOLDOWN_AFTER_LOGIN, "0")]))?;
        assert!(!off.login_cooldown().in_force(1_000, Duration::ZERO));
        Ok(())
    }

    #[test]
    fn malformed_values_are_errors_naming_the_key() {
        for (key, value, message) in [
            (
                keys::COOLDOWN_AFTER,
                "ten",
                "ir.config_parameter base.login_cooldown_after = 'ten' is not a whole number in \
                 1..=1000000",
            ),
            (
                keys::MAX_INACTIVITY,
                "0",
                "ir.config_parameter sessions.max_inactivity_seconds = '0' is not a whole \
                 number in 60..=31536000",
            ),
            (
                keys::ROUNDS,
                "-1",
                "ir.config_parameter password.hashing.rounds = '-1' is not a whole number in \
                 0..=4294967295",
            ),
        ] {
            let mut r = rows(&[(key, value)]);
            if key == keys::ROUNDS {
                r.insert(keys::SCHEME.to_owned(), Some("pbkdf2_sha512".to_owned()));
            }
            let e = AuthSettings::from_rows(&r).unwrap_err();
            assert_eq!(
                (e.kind(), e.message()),
                (erp_fields::ErrorKind::Validation, message)
            );
        }
        let e = AuthSettings::from_rows(&rows(&[
            (keys::SCHEME, "pbkdf2_sha512"),
            (keys::ROUNDS, "1000"),
        ]))
        .unwrap_err();
        assert_eq!(
            e.message(),
            "password.hashing.rounds = 1000 is outside 600000..=10000000"
        );
        let e = AuthSettings::from_rows(&rows(&[(keys::SCHEME, "md5")])).unwrap_err();
        assert_eq!(
            e.message(),
            "password.hashing.scheme = 'md5' is not a scheme: expected 'argon2id' or \
             'pbkdf2_sha512'"
        );
    }

    #[test]
    fn cooldown_rule() {
        let c = Cooldown {
            after: 3,
            duration: Duration::from_secs(60),
        };
        assert!(!c.in_force(2, Duration::ZERO));
        assert!(c.in_force(3, Duration::from_secs(59)));
        assert!(!c.in_force(3, Duration::from_secs(60)));
    }

    /// Round 1 decision 5: the write-time check uses the read parsers, accepts keys that are not
    /// settings and undefined values, and refuses the address cooldown at 0.
    #[test]
    fn validate_setting_uses_the_read_parsers() {
        validate_setting("web.base.url", Some("not a number")).unwrap();
        validate_setting(keys::MAX_LIFETIME, None).unwrap();
        validate_setting(keys::MAX_LIFETIME, Some("")).unwrap();
        validate_setting(keys::COOLDOWN_AFTER_LOGIN, Some("0")).unwrap();
        let e = validate_setting(keys::COOLDOWN_AFTER, Some("0")).unwrap_err();
        assert_eq!(
            (e.kind(), e.message()),
            (
                erp_fields::ErrorKind::Validation,
                "ir.config_parameter base.login_cooldown_after = '0' is not a whole number in \
                 1..=1000000"
            )
        );
        let read = AuthSettings::from_rows(&rows(&[(keys::COOLDOWN_AFTER, "0")])).unwrap_err();
        assert_eq!(read.message(), e.message(), "the same parser at read");
    }

    /// Review of R1B (types 5, silent 6): every key of `keys::ALL` has a write-time parser that
    /// refuses garbage, naming the key; a key added later without one fails this test.
    #[test]
    fn every_setting_key_refuses_garbage_when_written() {
        for key in keys::ALL {
            let e = validate_setting(key, Some("garbage")).unwrap_err();
            assert_eq!(e.kind(), erp_fields::ErrorKind::Validation, "{key}: {e}");
            assert!(
                e.message()
                    .starts_with(&format!("ir.config_parameter {key} ")),
                "{key}: {e}"
            );
        }
    }
}
