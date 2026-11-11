//! Correlation metadata: never an authority or an idempotency key.
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use erp_fields::{Error, ErrorKind, Result};

/// A syntactically valid UUID, canonically formatted. Kept source-compatible with
/// RPC's original UUID parser; HTTP correlation headers use [`Self::parse_header`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CorrelationId(Box<str>);

impl CorrelationId {
    /// Parse UUID text, accepting upper case and any UUID version for existing
    /// X-Request-ID/RPC callers.
    ///
    /// # Errors
    /// `Programming` for malformed UUID text.
    pub fn parse(value: &str) -> Result<Self> {
        let bytes = value.as_bytes();
        if bytes.len() != 36
            || bytes.iter().enumerate().any(|(i, b)| {
                let dash = matches!(i, 8 | 13 | 18 | 23);
                dash != (*b == b'-') || (!dash && !b.is_ascii_hexdigit())
            })
        {
            return Err(Error::programming(
                "a correlation id must be a UUID (RFC 9562) in text form",
            ));
        }
        Ok(Self(value.to_ascii_lowercase().into()))
    }

    /// Parse a canonical, nonnil RFC 9562 variant UUIDv4/UUIDv7 header.
    ///
    /// # Errors
    /// `Validation` with a named `X-Correlation-ID` reason. Duplicate-header
    /// detection belongs to HTTP, before this scalar parser.
    pub fn parse_header(value: &str) -> Result<Self> {
        let id = Self::parse(value).map_err(|_| header_error("malformed"))?;
        if id.as_str() == "00000000-0000-0000-0000-000000000000" {
            return Err(header_error("nil"));
        }
        if id.as_str() != value {
            return Err(header_error("noncanonical"));
        }
        if !matches!(value.as_bytes()[14], b'4' | b'7') {
            return Err(header_error("unsupported version"));
        }
        if !matches!(value.as_bytes()[19], b'8' | b'9' | b'a' | b'b') {
            return Err(header_error("unsupported variant"));
        }
        Ok(id)
    }

    /// Format 128 bits as UUID text; retained for existing RPC/request tracing.
    /// This does not assert that the bits have a particular UUID version.
    pub fn from_u128(bits: u128) -> Self {
        let hex = format!("{bits:032x}");
        Self(
            format!(
                "{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            )
            .into(),
        )
    }

    /// Generate a UUIDv7 before database selection, using the system CSPRNG.
    ///
    /// # Errors
    /// `Internal` if the clock or entropy source is unavailable. No fallback id
    /// or deterministic entropy is substituted.
    pub fn generate() -> Result<Self> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                Error::new(
                    ErrorKind::Internal,
                    "UUIDv7: system clock precedes Unix epoch",
                )
                .with_source(error)
            })?;
        let milliseconds = u64::try_from(elapsed.as_millis()).map_err(|error| {
            Error::new(
                ErrorKind::Internal,
                "UUIDv7: timestamp exceeds supported range",
            )
            .with_source(error)
        })?;
        if milliseconds >= (1_u64 << 48) {
            return Err(Error::new(
                ErrorKind::Internal,
                "UUIDv7: timestamp exceeds 48 bits",
            ));
        }
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| {
            Error::new(ErrorKind::Internal, "UUIDv7: entropy source unavailable").with_source(error)
        })?;
        bytes[..6].copy_from_slice(&milliseconds.to_be_bytes()[2..]);
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self::from_u128(u128::from_be_bytes(bytes)))
    }

    /// Canonical lower-case UUID text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn header_error(reason: &str) -> Error {
    Error::validation(format!("X-Correlation-ID: {reason}"))
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A server-generated execution id, distinct from an untrusted root correlation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationId(CorrelationId);

impl OperationId {
    /// Generate a fresh execution UUIDv7.
    ///
    /// # Errors
    /// As [`CorrelationId::generate`].
    pub fn generate() -> Result<Self> {
        CorrelationId::generate().map(Self)
    }

    /// Restore a persisted UUIDv7 execution id. Parsing confers no authority.
    ///
    /// # Errors
    /// `Validation` for a noncanonical UUIDv7.
    pub fn parse(value: &str) -> Result<Self> {
        let id = CorrelationId::parse_header(value)
            .map_err(|_| Error::validation("operation id must be a canonical UUIDv7"))?;
        if value.as_bytes()[14] != b'7' {
            return Err(Error::validation("operation id must be a canonical UUIDv7"));
        }
        Ok(Self(id))
    }

    /// Canonical UUID text.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Immutable tracing metadata shared by all attempts of one execution. It lives
/// outside request context and never changes access-policy or idempotency bindings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationIdentity {
    correlation: CorrelationId,
    operation: OperationId,
}

impl OperationIdentity {
    /// Bind a root correlation to a new server execution id.
    ///
    /// # Errors
    /// As [`OperationId::generate`].
    pub fn new(correlation: CorrelationId) -> Result<Self> {
        Ok(Self {
            correlation,
            operation: OperationId::generate()?,
        })
    }

    /// Generate independent root correlation and execution ids for a local call.
    ///
    /// # Errors
    /// As [`CorrelationId::generate`].
    pub fn generate() -> Result<Self> {
        Self::new(CorrelationId::generate()?)
    }

    /// Root tracing label; untrusted and shared across retries.
    pub fn correlation(&self) -> &CorrelationId {
        &self.correlation
    }
    /// Server execution identity; shared across retries, fresh for a new call.
    pub fn operation(&self) -> &OperationId {
        &self.operation
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::{Context, Env, Principal, UserId};
    use erp_registry::{AddonDescriptor, RegistryBuilder};
    use std::sync::Arc;

    #[test]
    fn environment_transforms_preserve_root_identity() {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "correlation_test",
            depends: &["base"],
            exact_only: false,
        };
        let registry = Arc::new(RegistryBuilder::new([&ADDON]).build().unwrap());
        let identity = Arc::new(OperationIdentity::generate().unwrap());
        let env = Env::new(
            registry.clone(),
            Principal::for_uid(UserId::new(2).unwrap()),
            Context::new(),
        )
        .with_operation_identity(Some(identity.clone()));
        for transformed in [
            env.clone(),
            env.sudo(true),
            env.with_user(3),
            env.with_debug(true),
            env.with_context([("lang", erp_fields::Value::from("id_ID"))]),
            env.rebind(registry, Arc::default()),
            env.acting_as(&env.with_user(4)),
            env.through_link(),
        ] {
            assert_eq!(transformed.operation_identity(), Some(identity.as_ref()));
        }
        // Metadata is never an authorization/cache identity dimension.
        assert_eq!(
            env,
            env.clone()
                .with_operation_identity(Some(Arc::new(OperationIdentity::generate().unwrap())))
        );
    }
}
