//! `database.secret` and the purpose keys derived from it.
//!
//! The secret is created with the database (`ir.config_parameter.init`) as 64 hexadecimal
//! characters of 32 bytes from the OS CSPRNG.
//! Purpose keys are
//! HKDF-SHA256 (salt empty, `info` = the purpose label) of that material, one per key id (`kid`);
//! kid 1 is "derived from `database.secret`" (§3.5, and 5).
//!
//! Neither the secret nor a key is ever formatted: their `Debug` prints a placeholder, they have no
//! `Display`, and their bytes are wiped when dropped.

use std::fmt;

use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

/// The purpose label of the session token key (§3.5).
pub const SESSION_TOKEN_INFO: &[u8] = b"erp:session-token:v1";

/// The purpose label of the login throttle's key (security review round 2 S4): throttle rows are
/// keyed by an HMAC under it, never a plain hash of the login (a password typed into the login
/// field is no fast hash in the table).
pub const THROTTLE_KEY_INFO: &[u8] = b"erp:throttle-key:v1";

/// The key id of keys derived from `database.secret`.
pub const KID_DATABASE_SECRET: i16 = 1;

const SECRET_MIN_BYTES: usize = 32;
const SECRET_MAX_BYTES: usize = 512;

/// A random UUID (version 4) in its hyphenated text form: the value of `database.uuid`.
///
/// # Errors
///
/// [`SecretError::Random`] when the OS CSPRNG fails.
pub fn random_uuid_text() -> Result<String, SecretError> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(SecretError::Random)?;
    b[6] = 0x40 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let mut text = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push(hex_digit(byte >> 4));
        text.push(hex_digit(byte & 0x0f));
    }
    Ok(text)
}

/// The value of `database.secret` (key material only; never formatted).
pub struct DatabaseSecret(Zeroizing<Vec<u8>>);

impl DatabaseSecret {
    /// A new secret's stored text: 64 hexadecimal characters of 32 CSPRNG bytes.
    ///
    /// # Errors
    ///
    /// [`SecretError::Random`] when the OS CSPRNG fails.
    pub fn generate_text() -> Result<Zeroizing<String>, SecretError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(bytes.as_mut_slice()).map_err(SecretError::Random)?;
        let mut text = Zeroizing::new(String::with_capacity(64));
        for b in bytes.iter() {
            text.push(hex_digit(b >> 4));
            text.push(hex_digit(b & 0x0f));
        }
        Ok(text)
    }

    /// The secret stored as `text`.
    ///
    /// # Errors
    ///
    /// [`SecretError::Length`] for a value shorter than 32 or longer than 512 bytes.
    pub fn from_stored(text: &str) -> Result<Self, SecretError> {
        let len = text.len();
        if !(SECRET_MIN_BYTES..=SECRET_MAX_BYTES).contains(&len) {
            return Err(SecretError::Length { len });
        }
        Ok(Self(Zeroizing::new(text.as_bytes().to_vec())))
    }

    /// The purpose key for `info` (kid 1), derived into its own heap block and returned boxed, so
    /// moving it moves a pointer and the key's bytes are wiped where they live (rust review Low).
    /// HKDF's internal state holds key material on the stack while it runs; this claims nothing
    /// about that.
    ///
    /// # Errors
    ///
    /// [`SecretError::Derivation`] when HKDF refuses (not reachable for 32-byte keys, which are
    /// far below HKDF-SHA256's 8,160-byte limit).
    pub fn purpose_key(&self, info: &[u8]) -> Result<Box<PurposeKey>, SecretError> {
        let mut key = Box::new(PurposeKey([0u8; 32]));
        Hkdf::<Sha256>::new(None, &self.0)
            .expand(info, &mut key.0)
            .map_err(|_| SecretError::Derivation)?;
        Ok(key)
    }
}

fn hex_digit(n: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(n & 0x0f)])
}

impl fmt::Debug for DatabaseSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DatabaseSecret(<redacted>)")
    }
}

/// A 32-byte purpose key (never formatted, wiped on drop). This module makes it only boxed
/// ([`DatabaseSecret::purpose_key`]); its field is private, so no other module makes one.
pub struct PurposeKey([u8; 32]);

impl PurposeKey {
    /// The key bytes, for the HMACs of this crate.
    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for PurposeKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for PurposeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PurposeKey(<redacted>)")
    }
}

/// The session token keys of one database by kid (§3.5): kid 1 only until EGRESS adds a
/// KEK-derived kid 2.
#[derive(Debug)]
pub struct SessionKeys {
    kid1: Box<PurposeKey>,
}

impl SessionKeys {
    /// The keys derived from `secret`.
    ///
    /// # Errors
    ///
    /// [`SecretError::Derivation`] when HKDF refused (not reachable with 32-byte keys).
    pub fn derive(secret: &DatabaseSecret) -> Result<Self, SecretError> {
        Ok(Self {
            kid1: secret.purpose_key(SESSION_TOKEN_INFO)?,
        })
    }

    /// The kid new tokens are made with and its key (always held: type design N1).
    pub fn issuing(&self) -> (i16, &PurposeKey) {
        (KID_DATABASE_SECRET, &self.kid1)
    }

    /// The key of `kid`, `None` for a kid this server does not hold.
    pub fn key(&self, kid: i16) -> Option<&PurposeKey> {
        (kid == KID_DATABASE_SECRET).then_some(&*self.kid1)
    }
}

/// Why a secret or key cannot be used.
#[derive(Debug)]
#[non_exhaustive]
pub enum SecretError {
    /// The OS CSPRNG failed.
    Random(getrandom::Error),
    /// The stored `database.secret` has a length outside 32..=512 bytes.
    Length {
        /// Its length.
        len: usize,
    },
    /// The row `database.secret` is missing.
    Missing,
    /// HKDF refused (not reachable with 32-byte keys).
    Derivation,
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Random(e) => write!(f, "the OS random number generator failed: {e}"),
            Self::Length { len } => write!(
                f,
                "ir.config_parameter database.secret has {len} bytes; expected 32 to 512"
            ),
            Self::Missing => f.write_str(
                "ir.config_parameter database.secret is missing: ir.config_parameter.init() \
                 creates it when the base module is installed",
            ),
            Self::Derivation => f.write_str("deriving a purpose key from database.secret failed"),
        }
    }
}

impl std::error::Error for SecretError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Random(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SecretError> for erp_fields::Error {
    fn from(e: SecretError) -> Self {
        erp_fields::Error::programming(e.to_string()).with_source(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_text_is_64_hex_and_fresh() -> Result<(), SecretError> {
        let a = DatabaseSecret::generate_text()?;
        let b = DatabaseSecret::generate_text()?;
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(*a, *b);
        Ok(())
    }

    #[test]
    fn keys_depend_on_secret_and_purpose_and_never_print() -> Result<(), SecretError> {
        let s1 = DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef")?;
        let s2 = DatabaseSecret::from_stored("0123456789abcdef0123456789abcdeF")?;
        let k1 = s1.purpose_key(SESSION_TOKEN_INFO)?;
        assert_ne!(k1.bytes(), s2.purpose_key(SESSION_TOKEN_INFO)?.bytes());
        assert_ne!(k1.bytes(), s1.purpose_key(b"erp:csrf:v1")?.bytes());
        assert_eq!(k1.bytes(), s1.purpose_key(SESSION_TOKEN_INFO)?.bytes());
        assert_eq!(
            format!("{s1:?} {k1:?}"),
            "DatabaseSecret(<redacted>) PurposeKey(<redacted>)"
        );
        let keys = SessionKeys::derive(&s1)?;
        assert!(keys.key(1).is_some() && keys.key(2).is_none() && keys.key(0).is_none());
        Ok(())
    }

    #[test]
    fn stored_length_bounds() {
        assert_eq!(
            DatabaseSecret::from_stored("short")
                .unwrap_err()
                .to_string(),
            "ir.config_parameter database.secret has 5 bytes; expected 32 to 512"
        );
        // round 2 N8: 31 bytes are refused, 32 accepted
        assert!(DatabaseSecret::from_stored(&"x".repeat(31)).is_err());
        assert!(DatabaseSecret::from_stored(&"x".repeat(32)).is_ok());
        assert!(DatabaseSecret::from_stored(&"x".repeat(513)).is_err());
        assert!(DatabaseSecret::from_stored("2b0f6b0e-8a5e-4a4f-9d55-3b8b5e1c0e42").is_ok());
    }
}
