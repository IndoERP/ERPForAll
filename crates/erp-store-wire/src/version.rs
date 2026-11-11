//! Wire major and schema generation (3,/010).

use std::fmt;

use crate::error::WireError;
use crate::limits::{Limit, WireLimits};

/// The wire major version. Field kinds and operations are part of it: adding one bumps the
/// major on both sides together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WireMajor(u32);

impl WireMajor {
    /// The only major this build speaks.
    pub const CURRENT: WireMajor = WireMajor(1);

    /// The number.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

/// Decide whether a client's announced wire major is served.
///
/// The HTTP status and the socket close code belong to the transports.
///
/// # Errors
/// [`WireError::ClientOutdated`] for any major other than [`WireMajor::CURRENT`], older or newer.
pub fn accept_client_major(client: u64) -> Result<WireMajor, WireError> {
    if client == u64::from(WireMajor::CURRENT.get()) {
        Ok(WireMajor::CURRENT)
    } else {
        Err(WireError::ClientOutdated {
            client,
            supported: WireMajor::CURRENT.get(),
        })
    }
}

/// An opaque, bounded token naming one generation of the store schema.
///
/// Characters: ASCII letters and digits, `.`, `_`, `-`, `:`. Length 1 up to the
/// `schema_token_bytes` limit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaGeneration(String);

impl SchemaGeneration {
    /// Validate a token under the default limits.
    ///
    /// # Errors
    /// [`WireError::BadName`] (path `schema`) or [`WireError::LimitExceeded`].
    pub fn new(token: &str) -> Result<Self, WireError> {
        Self::new_with_limits(token, &WireLimits::default())
    }

    /// Validate a token under the host's limits.
    ///
    /// # Errors
    /// As [`new`](Self::new).
    pub fn new_with_limits(token: &str, limits: &WireLimits) -> Result<Self, WireError> {
        Self::new_at(token, limits, "schema")
    }

    pub(crate) fn new_at(token: &str, limits: &WireLimits, path: &str) -> Result<Self, WireError> {
        if token.is_empty() {
            return Err(WireError::BadName {
                path: path.to_owned(),
                reason: "empty generation token".to_owned(),
            });
        }
        limits.check(Limit::SchemaTokenBytes, token.len(), path)?;
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':');
        if !token.bytes().all(allowed) {
            return Err(WireError::BadName {
                path: path.to_owned(),
                reason: "generation token has a character outside A-Z a-z 0-9 . _ - :".to_owned(),
            });
        }
        Ok(Self(token.to_owned()))
    }

    /// The token text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SchemaGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
