//! Native API key text and keyed digests.
//!
//! These are cryptographic primitives, not authentication: persistence, owner/scope/expiry
//! checks and principal construction belong to.
//! The hasher is derived once from a tenant's current secret and rebuilt when that secret
//! changes. No cache of accepted keys is kept here.
//!
//! ```
//! use erp_security::auth::apikey::{ApiKeyHasher, GeneratedApiKey, NativeApiKey};
//! use erp_security::auth::secret::DatabaseSecret;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Public dummy material for this example; production loads the tenant's database.secret.
//! let secret = DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef")?;
//! let hasher = ApiKeyHasher::derive(&secret)?;
//! let generated = GeneratedApiKey::generate()?;
//! let digest = hasher.digest(&generated.token());
//! assert_eq!(digest.kid(), 1);
//! // An explicit borrow for the one-time response; never log this text.
//! let presented = NativeApiKey::parse(generated.expose())?;
//! assert_eq!(hasher.digest(&presented).bytes(), digest.bytes());
//! # Ok(())
//! # }
//! ```

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use erp_fields::{Error, ErrorKind, Result};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use super::secret::{DatabaseSecret, KID_DATABASE_SECRET, SecretError};

const PURPOSE: &[u8] = b"erp:apikey-digest:v1";
const PREFIX: &str = "erpk_";
const TOKEN_BYTES: usize = 48;
const ENTROPY_BYTES: usize = 32;

/// A validated, borrowed native API key. The caller owns and protects the input buffer.
///
/// Debug is redacted. There is no implicit formatting, serialization or owned copy.
#[non_exhaustive]
pub struct NativeApiKey<'a>(&'a str);

impl<'a> NativeApiKey<'a> {
    /// Validate exactly `erpk_` and 43 canonical unpadded base64url characters.
    ///
    /// Checks the byte length before decoding; no input-proportional allocation or trimming.
    ///
    /// # Errors
    ///
    /// `AccessDenied: Invalid apikey` for every malformed token, without reflecting input.
    pub fn parse(text: &'a str) -> Result<Self> {
        if text.len() != TOKEN_BYTES || !text.starts_with(PREFIX) {
            return Err(invalid_key());
        }
        // PREFIX is ASCII and matched above: this is a UTF-8 boundary. The decoder rejects
        // non-URL-safe characters, padding and nonzero trailing bits. It requires 33 bytes
        // of output capacity even though a valid token decodes to exactly 32.
        let mut scratch = Zeroizing::new([0u8; ENTROPY_BYTES + 1]);
        match URL_SAFE_NO_PAD.decode_slice(&text[PREFIX.len()..], scratch.as_mut_slice()) {
            Ok(ENTROPY_BYTES) => Ok(Self(text)),
            Ok(_) | Err(_) => Err(invalid_key()),
        }
    }

    /// The first eight base64url characters after `erpk_`, for display only.
    ///
    /// Not a unique identifier or credential. Canonical validation guarantees ASCII here.
    pub fn display_prefix(&self) -> &'a str {
        &self.0[PREFIX.len()..PREFIX.len() + 8]
    }
}

impl fmt::Debug for NativeApiKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeApiKey(<redacted>)")
    }
}

fn invalid_key() -> Error {
    Error::new(ErrorKind::AccessDenied, "Invalid apikey")
}

/// Newly generated native key text, wiped on drop and redacted in Debug.
///
/// This value is deliberately neither Clone nor serializable. The eventual issuance API
/// must persist only the digest and return the exposed text once, after commit.
#[non_exhaustive]
pub struct GeneratedApiKey(Zeroizing<String>);

impl GeneratedApiKey {
    /// Generate a native key using 32 bytes from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// [`SecretError::Random`] if the OS entropy source fails; there is no fallback.
    pub fn generate() -> std::result::Result<Self, SecretError> {
        let mut entropy = Zeroizing::new([0u8; ENTROPY_BYTES]);
        getrandom::fill(entropy.as_mut_slice()).map_err(SecretError::Random)?;
        let mut text = Zeroizing::new(String::with_capacity(TOKEN_BYTES));
        text.push_str(PREFIX);
        URL_SAFE_NO_PAD.encode_string(entropy.as_slice(), &mut text);
        Ok(Self(text))
    }

    /// Explicitly borrow the secret text for a one-time response. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// A borrowed validated view, without decoding the text just generated.
    pub fn token(&self) -> NativeApiKey<'_> {
        NativeApiKey(&self.0)
    }
}

impl fmt::Debug for GeneratedApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GeneratedApiKey(<redacted>)")
    }
}

/// A native key's HMAC digest together with the key id that produced it.
///
/// Callers persist both fields. This value carries no verified identity or authority.
#[non_exhaustive]
pub struct ApiKeyDigest {
    kid: i16,
    bytes: [u8; 32],
}

impl ApiKeyDigest {
    /// The digest key id to persist alongside [`Self::bytes`].
    pub fn kid(&self) -> i16 {
        self.kid
    }

    /// Digest bytes for a bound database parameter; never the native secret text.
    pub fn bytes(&self) -> &[u8; 32] {
        &self.bytes
    }
}

impl fmt::Debug for ApiKeyDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKeyDigest(<redacted>)")
    }
}

/// The tenant's API key HMAC state, purpose-separated from sessions and throttling.
///
/// Derived once per tenant secret; callers must rebuild it on secret rotation. The HMAC/SHA
/// zeroize features wipe keyed state when dropped, including per-digest clones. The hasher
/// does not retain presented tokens or successful credential checks.
#[non_exhaustive]
pub struct ApiKeyHasher(Hmac<Sha256>);

impl ApiKeyHasher {
    /// Derive the native digest key from a tenant's `database.secret` using HKDF-SHA256.
    ///
    /// # Errors
    ///
    /// [`SecretError::Derivation`] if key derivation or HMAC initialization fails.
    pub fn derive(secret: &DatabaseSecret) -> std::result::Result<Self, SecretError> {
        let key = secret.purpose_key(PURPOSE)?;
        let mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key.bytes())
            .map_err(|_| SecretError::Derivation)?;
        Ok(Self(mac))
    }

    /// Hash the complete canonical token text under the currently issuing key (kid 1).
    pub fn digest(&self, token: &NativeApiKey<'_>) -> ApiKeyDigest {
        let mut mac = self.0.clone();
        mac.update(token.0.as_bytes());
        ApiKeyDigest {
            kid: KID_DATABASE_SECRET,
            bytes: mac.finalize().into_bytes().into(),
        }
    }

    /// Compute the digest for an explicitly selected stored key id.
    ///
    /// # Errors
    ///
    /// `Programming` naming an unavailable kid. Kid 1 is the only supported id until the
    /// accepted EGRESS rotation contract supplies additional key material.
    pub fn digest_for_kid(&self, token: &NativeApiKey<'_>, kid: i16) -> Result<ApiKeyDigest> {
        if kid != KID_DATABASE_SECRET {
            return Err(Error::programming(format!(
                "API key digest kid {kid} is not available"
            )));
        }
        Ok(self.digest(token))
    }
}

impl fmt::Debug for ApiKeyHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKeyHasher(<redacted>)")
    }
}
