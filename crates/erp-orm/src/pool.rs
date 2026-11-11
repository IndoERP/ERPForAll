//! Connection pool helpers, the PostgreSQL 18 version gate and the string-reading
//! gate (PERF.STREAM-WINDOW review round 5, M4).
//!
//! ERPForAll requires PostgreSQL 18 or later. The version gate is installed
//! as a deadpool `post_create` connection hook on every pool constructor we ship.
//! Connections to servers below PostgreSQL 18 (`server_version_num < 180000`) are refused
//! with a typed error naming and the found version. PostgreSQL 19 and later are
//! accepted and logged once per pool via `tracing::warn!` as "untested until CI.PG19".

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use deadpool_postgres::{
    BuildError, Hook, HookError, Manager, ManagerConfig, Pool, PoolBuilder, RecyclingMethod,
    Runtime,
};
use tokio_postgres::{Config, NoTls};

/// Minimum PostgreSQL `server_version_num` required by ERPForAll (PostgreSQL 18.0).
pub const MIN_POSTGRES_VERSION_NUM: i32 = 180_000;

/// Error returned when building a connection pool fails.
#[derive(Debug)]
pub enum PoolBuildError {
    /// The pool's maximum size must be greater than zero.
    ZeroMaxSize,
    /// An error occurred in the deadpool pool builder.
    Build(BuildError),
}

impl std::fmt::Display for PoolBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMaxSize => write!(f, "pool max_size must be greater than zero; got 0"),
            Self::Build(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PoolBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ZeroMaxSize => None,
            Self::Build(err) => Some(err),
        }
    }
}

impl From<BuildError> for PoolBuildError {
    fn from(err: BuildError) -> Self {
        Self::Build(err)
    }
}

/// Error returned when the PostgreSQL server is below the minimum required version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPostgresError {
    /// The `server_version_num` reported by the server.
    pub server_version_num: i32,
    /// The minimum `server_version_num` required (180000).
    pub min_required: i32,
    /// The version string reported by `current_setting('server_version')`.
    pub found_version: String,
}

impl UnsupportedPostgresError {
    /// Distinct prefix embedded in hook error messages so it can be reliably parsed back.
    pub const HOOK_PREFIX: &'static str = "[: unsupported PostgreSQL] ";

    /// Create a new error for the given version.
    pub fn new(server_version_num: i32, found_version: impl Into<String>) -> Self {
        Self {
            server_version_num,
            min_required: MIN_POSTGRES_VERSION_NUM,
            found_version: found_version.into(),
        }
    }

    /// Format this error as a message for deadpool's [`HookError::message`].
    pub fn to_hook_message(&self) -> String {
        format!(
            "{}{};{};{}:{}: {}",
            Self::HOOK_PREFIX,
            self.server_version_num,
            self.min_required,
            self.found_version.len(),
            self.found_version,
            self
        )
    }

    /// Parse an [`UnsupportedPostgresError`] from raw hook payload text following [`Self::HOOK_PREFIX`].
    pub fn parse_hook_payload(payload: &str) -> Result<Self, HookParseError> {
        let colon_pos = payload.find(':').ok_or(HookParseError::MissingColon)?;
        let header = &payload[..colon_pos];
        let mut parts = header.split(';');

        let s_num = parts
            .next()
            .ok_or(HookParseError::MissingField("server_version_num"))?;
        let s_num_trimmed = s_num.trim();
        if s_num_trimmed.is_empty() {
            return Err(HookParseError::MissingField("server_version_num"));
        }
        let server_version_num =
            s_num_trimmed
                .parse::<i32>()
                .map_err(|_| HookParseError::InvalidNumber {
                    field: "server_version_num",
                    value: s_num.to_string(),
                })?;

        let s_min = parts
            .next()
            .ok_or(HookParseError::MissingField("min_required"))?;
        let s_min_trimmed = s_min.trim();
        if s_min_trimmed.is_empty() {
            return Err(HookParseError::MissingField("min_required"));
        }
        let min_required =
            s_min_trimmed
                .parse::<i32>()
                .map_err(|_| HookParseError::InvalidNumber {
                    field: "min_required",
                    value: s_min.to_string(),
                })?;

        let s_len = parts
            .next()
            .ok_or(HookParseError::MissingField("ver_len"))?;
        let s_len_trimmed = s_len.trim();
        if s_len_trimmed.is_empty() {
            return Err(HookParseError::MissingField("ver_len"));
        }
        let ver_len =
            s_len_trimmed
                .parse::<usize>()
                .map_err(|_| HookParseError::InvalidNumber {
                    field: "ver_len",
                    value: s_len.to_string(),
                })?;

        if parts.next().is_some() {
            return Err(HookParseError::TooManyFields);
        }

        if ver_len > MAX_VERSION_LEN {
            return Err(HookParseError::OversizedLength(ver_len));
        }

        let after_colon = &payload[colon_pos + 1..];
        if after_colon.len() < ver_len {
            return Err(HookParseError::Truncated {
                expected: ver_len,
                found: after_colon.len(),
            });
        }

        // `ver_len` counts bytes: a length that ends inside a multi-byte character is an error,
        // never a panic (the payload comes from the server error text).
        let found_version = after_colon
            .get(..ver_len)
            .ok_or(HookParseError::NotCharBoundary { at: ver_len })?;
        if after_colon.as_bytes().get(ver_len) != Some(&b':') {
            return Err(HookParseError::MissingTrailingColon);
        }

        Ok(Self {
            server_version_num,
            min_required,
            found_version: found_version.to_string(),
        })
    }

    /// Extract an [`UnsupportedPostgresError`] from any error whose display string contains the hook message.
    ///
    /// Returns `Ok(Some(...))` if a valid hook payload was parsed, `Ok(None)` if the error does not
    /// contain the hook prefix, or `Err(...)` if the hook prefix was present but the payload was malformed.
    pub fn from_pool_error<E: std::fmt::Display + ?Sized>(
        err: &E,
    ) -> Result<Option<Self>, HookParseError> {
        let text = err.to_string();
        let Some(pos) = text.find(Self::HOOK_PREFIX) else {
            return Ok(None);
        };
        let payload = &text[pos + Self::HOOK_PREFIX.len()..];
        Self::parse_hook_payload(payload).map(Some)
    }
}

/// Maximum accepted length of a PostgreSQL version string in the hook payload (safety limit).
pub const MAX_VERSION_LEN: usize = 1024;

/// Error parsing an [`UnsupportedPostgresError`] from a hook payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookParseError {
    /// The hook prefix or header colon was missing.
    MissingColon,
    /// A required field in the header is missing.
    MissingField(&'static str),
    /// More header fields were found than expected.
    TooManyFields,
    /// A numeric field failed to parse.
    InvalidNumber {
        /// Name of the header field that failed to parse.
        field: &'static str,
        /// Raw value that failed numeric parsing.
        value: String,
    },
    /// The version length exceeds the maximum allowed bound ([`MAX_VERSION_LEN`]).
    OversizedLength(usize),
    /// The payload was truncated before the specified version length could be read.
    Truncated {
        /// Expected byte length of the version string from the header.
        expected: usize,
        /// Actual byte length found before end of payload.
        found: usize,
    },
    /// The delimiter ':' after the version slice is missing (expected ':' before display message).
    MissingTrailingColon,
    /// The version length ends inside a multi-byte UTF-8 character.
    NotCharBoundary {
        /// Byte offset the header's length points at.
        at: usize,
    },
}

impl std::fmt::Display for HookParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingColon => write!(f, "missing colon delimiter in hook header"),
            Self::MissingField(field) => write!(f, "missing field '{field}' in hook header"),
            Self::TooManyFields => write!(f, "too many fields in hook header"),
            Self::InvalidNumber { field, value } => {
                write!(f, "invalid numeric value '{value}' for field '{field}'")
            }
            Self::OversizedLength(len) => {
                write!(
                    f,
                    "version length {len} exceeds maximum allowed {MAX_VERSION_LEN}"
                )
            }
            Self::Truncated { expected, found } => {
                write!(
                    f,
                    "payload truncated: expected {expected} bytes for version, found {found}"
                )
            }
            Self::MissingTrailingColon => {
                write!(
                    f,
                    "expected ':' delimiter immediately following version slice"
                )
            }
            Self::NotCharBoundary { at } => {
                write!(f, "version length {at} ends inside a multi-byte character")
            }
        }
    }
}

impl std::error::Error for HookParseError {}

impl std::fmt::Display for UnsupportedPostgresError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PostgreSQL {} (server_version_num {}) is below the minimum required version 18 \
             (server_version_num {}); ERPForAll requires PostgreSQL 18 or later",
            self.found_version, self.server_version_num, self.min_required,
        )
    }
}

impl std::error::Error for UnsupportedPostgresError {}

/// A connection whose settings make the server read the text of a statement differently from
/// the ORM's word scanner (`standard_conforming_strings` other than `on`, `client_encoding` other
/// than `UTF8`), refused when the pool opens it (review round 5, M4). For
/// `standard_conforming_strings` a role's or database's default (or the connection's `options`)
/// is the usual cause. `client_encoding` cannot come from them: the driver sends
/// `client_encoding=UTF8` in its startup packet (first, with `options` last), a setting given in
/// the startup packet outranks role and database defaults, and the server applies it after
/// `options` in its startup processing, so it wins over an `options` value too (tested in
/// `string_reading.rs`, review round 6, L4). Only something that runs after startup can change
/// it (a login event trigger, a connection pooler between). The check stays as a guard, because
/// that ordering is the server's behaviour, not something the driver or the ORM controls. A change made later on an open connection is seen from the
/// server's reports instead (`Transaction`, round 5, M3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedSessionSetting {
    /// The setting.
    pub setting: &'static str,
    /// Its value on the connection.
    pub value: String,
    /// The value ERPForAll needs.
    pub required: &'static str,
}

impl UnsupportedSessionSetting {
    /// Prefix of the hook error message that carries this error (see
    /// [`UnsupportedSessionSetting::from_pool_error`]).
    pub const HOOK_PREFIX: &'static str = "[unsupported session setting] ";

    /// The settings a new connection must have: `standard_conforming_strings` (`on`) and
    /// `client_encoding` (`UTF8`, the encoding the driver sends and reads), as `SHOW` reports them.
    ///
    /// # Errors
    ///
    /// The first setting with another value.
    pub fn check(standard_conforming_strings: &str, client_encoding: &str) -> Result<(), Self> {
        for (setting, value, required) in [
            (
                "standard_conforming_strings",
                standard_conforming_strings,
                "on",
            ),
            ("client_encoding", client_encoding, "UTF8"),
        ] {
            if !value.eq_ignore_ascii_case(required) {
                return Err(Self {
                    setting,
                    value: value.to_owned(),
                    required,
                });
            }
        }
        Ok(())
    }

    /// The message of a pool error that carries this refusal (a `post_create` hook error with
    /// [`Self::HOOK_PREFIX`]), without the prefix; `None` for any other pool error. The refusal
    /// travels as text because deadpool's `HookError` holds only a message or a driver error, so
    /// the typed value cannot be carried (review round 6, L5).
    pub fn from_pool_error(error: &deadpool_postgres::PoolError) -> Option<&str> {
        match error {
            deadpool_postgres::PoolError::PostCreateHook(HookError::Message(message)) => {
                message.strip_prefix(Self::HOOK_PREFIX)
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for UnsupportedSessionSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            setting,
            value,
            required,
        } = self;
        write!(
            f,
            "the connection's {setting} is '{value}', and ERPForAll needs '{required}': with \
             another value PostgreSQL reads the text of a statement differently from the ORM; "
        )?;
        if *setting == "client_encoding" {
            // the driver asks for UTF8 at startup, which role and database defaults cannot
            // override (review round 6, L4)
            write!(
                f,
                "the driver asks for UTF8 when it connects, so something that runs after startup \
                 changed it (a login event trigger, a connection pooler between)"
            )
        } else {
            write!(
                f,
                "a role or database default sets it (ALTER ROLE … RESET {setting} or ALTER \
                 DATABASE … RESET {setting}), or the connection's options"
            )
        }
    }
}

impl std::error::Error for UnsupportedSessionSetting {}

/// Validate the PostgreSQL server version.
///
/// Refuses server versions below PostgreSQL 18 (`server_version_num < 180000`) with [`UnsupportedPostgresError`].
pub fn validate_postgres_version(
    server_version_num: i32,
    found_version: &str,
) -> Result<(), UnsupportedPostgresError> {
    if server_version_num < MIN_POSTGRES_VERSION_NUM {
        return Err(UnsupportedPostgresError::new(
            server_version_num,
            found_version,
        ));
    }
    Ok(())
}

/// Validate the PostgreSQL server version and warn once per pool if 19+.
pub(crate) fn check_version_and_warn(
    version_num: i32,
    found_version: &str,
    min_version: i32,
    warned_pg19: &AtomicBool,
) -> Result<bool, UnsupportedPostgresError> {
    if version_num < min_version {
        return Err(UnsupportedPostgresError {
            server_version_num: version_num,
            min_required: min_version,
            found_version: found_version.to_string(),
        });
    }
    if version_num >= 190_000 && !warned_pg19.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "erp::orm",
            "PostgreSQL {found_version} (server_version_num {version_num}) is 19+; untested until CI.PG19"
        );
        return Ok(true);
    }
    Ok(false)
}

/// The default deadpool `post_create` connection hook that validates the PostgreSQL server version.
///
/// Refuses connections below PostgreSQL 18 (`server_version_num < 180000`) with an error
/// naming and the found version. PostgreSQL 19 and later are accepted and logged once
/// per pool via `tracing::warn!` as "untested until CI.PG19". The same query reads
/// `standard_conforming_strings` and `client_encoding`; a connection with other values than `on`
/// and `UTF8` is refused ([`UnsupportedSessionSetting`], no extra round trip).
pub fn pg_version_hook() -> Hook {
    pg_version_hook_with_min(MIN_POSTGRES_VERSION_NUM)
}

/// Like [`pg_version_hook`], but with a custom minimum required version number.
///
/// Used for testing the live rejection behavior against real PostgreSQL clusters without
/// needing an older database server.
#[doc(hidden)]
pub fn pg_version_hook_with_min(min_version: i32) -> Hook {
    let warned_pg19 = Arc::new(AtomicBool::new(false));
    Hook::async_fn(move |client, _metrics| {
        let warned_pg19 = Arc::clone(&warned_pg19);
        Box::pin(async move {
            let row = client
                .query_one(
                    "SELECT current_setting('server_version_num')::int, \
                     current_setting('server_version'), \
                     current_setting('standard_conforming_strings'), \
                     current_setting('client_encoding')",
                    &[],
                )
                .await
                .map_err(HookError::Backend)?;
            let version_num: i32 = row
                .try_get(0)
                .map_err(|e| HookError::message(format!("reading server_version_num: {e}")))?;
            let found_version: String = row
                .try_get(1)
                .map_err(|e| HookError::message(format!("reading server_version: {e}")))?;
            if let Err(err) =
                check_version_and_warn(version_num, &found_version, min_version, &warned_pg19)
            {
                return Err(HookError::message(err.to_hook_message()));
            }
            let conforming: String = row.try_get(2).map_err(|e| {
                HookError::message(format!("reading standard_conforming_strings: {e}"))
            })?;
            let encoding: String = row
                .try_get(3)
                .map_err(|e| HookError::message(format!("reading client_encoding: {e}")))?;
            UnsupportedSessionSetting::check(&conforming, &encoding).map_err(|err| {
                HookError::message(format!("{}{err}", UnsupportedSessionSetting::HOOK_PREFIX))
            })?;
            Ok(())
        })
    })
}

/// Create a deadpool [`PoolBuilder`] configured with the PG18 version gate hook.
pub fn pool_builder(config: Config) -> PoolBuilder {
    pool_builder_with_min_version(config, MIN_POSTGRES_VERSION_NUM)
}

/// Like [`pool_builder`], but with a custom minimum required version number.
#[doc(hidden)]
pub fn pool_builder_with_min_version(config: Config, min_version: i32) -> PoolBuilder {
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager).post_create(pg_version_hook_with_min(min_version))
}

/// Create a deadpool pool configured with the PG18 version gate hook and Tokio1 runtime.
pub fn create_pool(config: Config, max_size: usize) -> Result<Pool, PoolBuildError> {
    if max_size == 0 {
        return Err(PoolBuildError::ZeroMaxSize);
    }
    pool_builder(config)
        .max_size(max_size)
        .runtime(Runtime::Tokio1)
        .build()
        .map_err(PoolBuildError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_below_18_is_refused() {
        let err = validate_postgres_version(170002, "17.2").unwrap_err();
        assert_eq!(err.server_version_num, 170002);
        assert_eq!(err.min_required, 180000);
        assert_eq!(err.found_version, "17.2");
        let msg = err.to_string();
        assert!(msg.contains("170002"), "{msg}");
        assert!(msg.contains("17.2"), "{msg}");
        assert!(msg.contains("180000"), "{msg}");
        assert!(msg.contains(""), "{msg}");

        let err16 = validate_postgres_version(160000, "16.0").unwrap_err();
        assert_eq!(err16.server_version_num, 160000);
        assert!(err16.to_string().contains(""));
    }

    /// Review round 5, M4: only `on` and `UTF8` pass; the error names the setting, its value and
    /// the value needed, and a pool error carrying it gives the message back.
    #[test]
    fn session_settings_that_read_strings_differently_are_refused() {
        assert!(UnsupportedSessionSetting::check("on", "UTF8").is_ok());
        assert!(UnsupportedSessionSetting::check("ON", "utf8").is_ok());
        let off = UnsupportedSessionSetting::check("off", "UTF8").unwrap_err();
        assert_eq!(off.setting, "standard_conforming_strings");
        assert_eq!(off.value, "off");
        let sjis = UnsupportedSessionSetting::check("on", "SJIS").unwrap_err();
        assert_eq!(sjis.setting, "client_encoding");
        assert_eq!(
            sjis.to_string(),
            "the connection's client_encoding is 'SJIS', and ERPForAll needs 'UTF8': with another \
             value PostgreSQL reads the text of a statement differently from the ORM; the driver \
             asks for UTF8 when it connects, so something that runs after startup changed it (a \
             login event trigger, a connection pooler between)"
        );
        assert_eq!(
            off.to_string(),
            "the connection's standard_conforming_strings is 'off', and ERPForAll needs 'on': with \
             another value PostgreSQL reads the text of a statement differently from the ORM; a \
             role or database default sets it (ALTER ROLE … RESET standard_conforming_strings or \
             ALTER DATABASE … RESET standard_conforming_strings), or the connection's options"
        );
        let pool_error = deadpool_postgres::PoolError::PostCreateHook(HookError::message(format!(
            "{}{sjis}",
            UnsupportedSessionSetting::HOOK_PREFIX
        )));
        assert_eq!(
            UnsupportedSessionSetting::from_pool_error(&pool_error),
            Some(sjis.to_string().as_str())
        );
        let other = deadpool_postgres::PoolError::PostCreateHook(HookError::message("x"));
        assert_eq!(UnsupportedSessionSetting::from_pool_error(&other), None);
    }

    #[test]
    fn version_18_and_19_are_accepted() {
        assert!(validate_postgres_version(180000, "18.0").is_ok());
        assert!(validate_postgres_version(180001, "18.1").is_ok());
        assert!(validate_postgres_version(190000, "19.0").is_ok());
    }

    #[test]
    fn zero_max_size_is_rejected_loudly() {
        let err = create_pool(Config::new(), 0).unwrap_err();
        assert!(matches!(err, PoolBuildError::ZeroMaxSize));
        assert!(err.to_string().contains("greater than zero"));
    }

    #[test]
    fn unsupported_postgres_error_round_trip() {
        let cases = [
            (170002, 180000, "17.2"),
            (160006, 180000, "16.6 (Ubuntu 16.6-1.pgdg22.04+1)"),
            (170011, 180000, "17.11"),
            (150004, 180000, "15.4"),
            (170002, 180000, "17.2:beta1;extra:build"),
            (160005, 180000, "16.5; custom:build; rel:1"),
        ];

        for (num, min, ver) in cases {
            let err = UnsupportedPostgresError {
                server_version_num: num,
                min_required: min,
                found_version: ver.to_string(),
            };
            let hook_msg = err.to_hook_message();
            let parsed = UnsupportedPostgresError::from_pool_error(&hook_msg)
                .expect("should parse from hook message")
                .expect("should find hook prefix");
            assert_eq!(parsed, err);

            // Also test wrapped in deadpool HookError representation
            let deadpool_wrapped = format!("PostCreateHook: {hook_msg}");
            let parsed_wrapped = UnsupportedPostgresError::from_pool_error(&deadpool_wrapped)
                .expect("should parse from deadpool formatted message")
                .expect("should find hook prefix");
            assert_eq!(parsed_wrapped, err);
        }
    }

    #[test]
    fn pg19_warning_fires_once_per_pool() {
        let flag = AtomicBool::new(false);
        // First call for PG19 on this pool warns
        assert!(check_version_and_warn(190000, "19.0", 180000, &flag).unwrap());
        // Subsequent calls on the same pool do not warn again
        assert!(!check_version_and_warn(190001, "19.1", 180000, &flag).unwrap());
        assert!(!check_version_and_warn(200000, "20.0", 180000, &flag).unwrap());

        // A separate pool has its own flag and warns once
        let flag2 = AtomicBool::new(false);
        assert!(check_version_and_warn(190000, "19.0", 180000, &flag2).unwrap());
        assert!(!check_version_and_warn(190000, "19.0", 180000, &flag2).unwrap());
    }

    #[test]
    fn parse_hook_payload_validation_and_errors() {
        // Valid roundtrip
        let valid = "170002;180000;4:17.2: display message";
        let parsed = UnsupportedPostgresError::parse_hook_payload(valid).unwrap();
        assert_eq!(parsed.server_version_num, 170002);
        assert_eq!(parsed.min_required, 180000);
        assert_eq!(parsed.found_version, "17.2");

        // 0. A length that ends inside a multi-byte character ('é' is two bytes) is an error, not a panic
        let err =
            UnsupportedPostgresError::parse_hook_payload("170002;180000;1:\u{e9}:msg").unwrap_err();
        assert_eq!(err, HookParseError::NotCharBoundary { at: 1 });
        assert_eq!(
            err.to_string(),
            "version length 1 ends inside a multi-byte character"
        );

        // 1. Truncated
        let err =
            UnsupportedPostgresError::parse_hook_payload("170002;180000;10:17.2").unwrap_err();
        assert_eq!(
            err,
            HookParseError::Truncated {
                expected: 10,
                found: 4
            }
        );
        assert_eq!(
            err.to_string(),
            "payload truncated: expected 10 bytes for version, found 4"
        );

        // 2. Oversized
        let err =
            UnsupportedPostgresError::parse_hook_payload("170002;180000;2000:abc").unwrap_err();
        assert_eq!(err, HookParseError::OversizedLength(2000));
        assert_eq!(
            err.to_string(),
            "version length 2000 exceeds maximum allowed 1024"
        );

        // 3. usize::MAX length
        let max_str = format!("170002;180000;{}:abc", usize::MAX);
        let err = UnsupportedPostgresError::parse_hook_payload(&max_str).unwrap_err();
        assert_eq!(err, HookParseError::OversizedLength(usize::MAX));
        assert_eq!(
            err.to_string(),
            format!("version length {} exceeds maximum allowed 1024", usize::MAX)
        );

        // 4. Non-numeric
        let err = UnsupportedPostgresError::parse_hook_payload("170002;180000;not_num:17.2: msg")
            .unwrap_err();
        assert_eq!(
            err,
            HookParseError::InvalidNumber {
                field: "ver_len",
                value: "not_num".to_string()
            }
        );
        assert_eq!(
            err.to_string(),
            "invalid numeric value 'not_num' for field 'ver_len'"
        );

        let err_num =
            UnsupportedPostgresError::parse_hook_payload("abc;180000;4:17.2: msg").unwrap_err();
        assert_eq!(
            err_num,
            HookParseError::InvalidNumber {
                field: "server_version_num",
                value: "abc".to_string()
            }
        );
        assert_eq!(
            err_num.to_string(),
            "invalid numeric value 'abc' for field 'server_version_num'"
        );

        // 5. Missing-length
        let err =
            UnsupportedPostgresError::parse_hook_payload("170002;180000;:17.2: msg").unwrap_err();
        assert_eq!(err, HookParseError::MissingField("ver_len"));
        assert_eq!(err.to_string(), "missing field 'ver_len' in hook header");

        let err_no_len =
            UnsupportedPostgresError::parse_hook_payload("170002;180000:17.2: msg").unwrap_err();
        assert_eq!(err_no_len, HookParseError::MissingField("ver_len"));
        assert_eq!(
            err_no_len.to_string(),
            "missing field 'ver_len' in hook header"
        );

        // 6. Trailing garbage (missing trailing ':' delimiter after version slice)
        let err = UnsupportedPostgresError::parse_hook_payload("170002;180000;4:17.2GARBAGE")
            .unwrap_err();
        assert_eq!(err, HookParseError::MissingTrailingColon);
        assert_eq!(
            err.to_string(),
            "expected ':' delimiter immediately following version slice"
        );

        // 7. Fourth header field returns TooManyFields
        let err = UnsupportedPostgresError::parse_hook_payload("170002;180000;4;extra:17.2: msg")
            .unwrap_err();
        assert_eq!(err, HookParseError::TooManyFields);
        assert_eq!(err.to_string(), "too many fields in hook header");
    }

    #[test]
    fn from_pool_error_malformed_payload_and_error_chain() {
        use std::error::Error as _;

        // Not a hook error: returns Ok(None)
        let generic_err = "failed to connect: connection refused";
        assert_eq!(
            UnsupportedPostgresError::from_pool_error(&generic_err).unwrap(),
            None
        );

        // Hook error with malformed payload (e.g. TooManyFields): returns Err(HookParseError)
        let malformed_msg = format!(
            "{}170002;180000;4;extra:17.2: msg",
            UnsupportedPostgresError::HOOK_PREFIX
        );
        let parse_err = UnsupportedPostgresError::from_pool_error(&malformed_msg).unwrap_err();
        assert_eq!(parse_err, HookParseError::TooManyFields);
        assert_eq!(parse_err.to_string(), "too many fields in hook header");

        // Hook error with truncated payload: returns Err(Truncated)
        let truncated_msg = format!(
            "{}170002;180000;10:17.2",
            UnsupportedPostgresError::HOOK_PREFIX
        );
        let parse_err = UnsupportedPostgresError::from_pool_error(&truncated_msg).unwrap_err();
        assert_eq!(
            parse_err,
            HookParseError::Truncated {
                expected: 10,
                found: 4
            }
        );

        // checkout_error preserves the HookParseError in the returned error chain
        let pool_err = deadpool_postgres::PoolError::PostCreateHook(
            deadpool_postgres::HookError::message(malformed_msg),
        );
        let orm_err = crate::transaction::checkout_error(pool_err);
        assert_eq!(orm_err.kind(), erp_fields::ErrorKind::Database);
        assert!(
            orm_err
                .to_string()
                .contains("malformed PostgreSQL version hook payload"),
            "expected malformed hook payload in error message, got: {orm_err}"
        );
        let src = orm_err.source().expect("should have source error");
        assert_eq!(
            src.downcast_ref::<HookParseError>(),
            Some(&HookParseError::TooManyFields)
        );
    }
}
