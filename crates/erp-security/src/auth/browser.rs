//! Signed anonymous preferences and form CSRF (auth design §5).
use std::fmt;

use super::secret::{DatabaseSecret, PurposeKey};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use thiserror::Error;

const VALIDITY: u64 = 48 * 60 * 60;
const EMPTY_PREFS: &[u8] = b"{}";

/// Secret-free browser token refusal.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PrefsError {
    /// Invalid preferences authentication.
    #[error("invalid signed preferences cookie")]
    Cookie,
    /// Invalid or expired form token.
    #[error("Session expired (invalid CSRF token)")]
    Csrf,
    /// Invalid key material or unavailable cryptographic primitive.
    #[error("browser token key or entropy unavailable")]
    Key,
}
/// Verified anonymous preference identity. Construction requires MAC verification.
pub struct SignedPrefs([u8; 16]);
impl fmt::Debug for SignedPrefs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignedPrefs(<redacted>)")
    }
}
/// Purpose-separated signing keys; never formats key material.
pub struct PrefsSigner {
    prefs: Box<PurposeKey>,
    csrf: Box<PurposeKey>,
}
impl fmt::Debug for PrefsSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrefsSigner(<redacted>)")
    }
}
impl PrefsSigner {
    /// Derive browser keys from the private database secret using auth's HKDF labels.
    /// # Errors
    /// Invalid key material or derivation failure.
    pub fn new(secret: &str) -> Result<Self, PrefsError> {
        let secret = DatabaseSecret::from_stored(secret).map_err(|_| PrefsError::Key)?;
        Self::derive(&secret)
    }
    /// Derive purpose keys without exposing raw database-secret bytes.
    /// # Errors
    /// Purpose-key derivation failure.
    pub(crate) fn derive(secret: &DatabaseSecret) -> Result<Self, PrefsError> {
        Ok(Self {
            prefs: secret
                .purpose_key(b"erp:prefs:v1")
                .map_err(|_| PrefsError::Key)?,
            csrf: secret
                .purpose_key(b"erp:csrf:v1")
                .map_err(|_| PrefsError::Key)?,
        })
    }
    /// Issue a fresh signed anonymous cookie with the currently empty preference payload.
    /// # Errors
    /// Entropy or signing failure.
    pub fn issue(&self) -> Result<String, PrefsError> {
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|_| PrefsError::Key)?;
        let mut mac = self.prefs_mac(&id)?;
        mac.update(EMPTY_PREFS);
        let tag = mac.finalize().into_bytes();
        Ok(format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(id),
            URL_SAFE_NO_PAD.encode(EMPTY_PREFS),
            URL_SAFE_NO_PAD.encode(&tag[..16])
        ))
    }
    /// Verify the signature and supported payload before trusting its identity.
    /// # Errors
    /// Malformed cookie, unsupported preferences or invalid MAC.
    pub fn verify(&self, cookie: &str) -> Result<SignedPrefs, PrefsError> {
        if cookie.len() > 128 {
            return Err(PrefsError::Cookie);
        }
        let mut parts = cookie.split('.');
        let id: [u8; 16] = decode(parts.next().ok_or(PrefsError::Cookie)?)?;
        let prefs = parts.next().ok_or(PrefsError::Cookie)?;
        let tag: [u8; 16] = decode(parts.next().ok_or(PrefsError::Cookie)?)?;
        if parts.next().is_some() || prefs != URL_SAFE_NO_PAD.encode(EMPTY_PREFS) {
            return Err(PrefsError::Cookie);
        }
        let mut mac = self.prefs_mac(&id)?;
        mac.update(EMPTY_PREFS);
        mac.verify_truncated_left(&tag)
            .map_err(|_| PrefsError::Cookie)?;
        Ok(SignedPrefs(id))
    }
    /// Issue a bounded form token for a verified anonymous preferences identity.
    /// # Errors
    /// Clock overflow or key failure.
    pub fn csrf(&self, prefs: &SignedPrefs, now: u64) -> Result<String, PrefsError> {
        let expires = now
            .checked_add(VALIDITY)
            .ok_or(PrefsError::Csrf)?
            .to_string();
        let mac = self.csrf_mac(prefs, &expires)?.finalize().into_bytes();
        let mut token = String::with_capacity(85);
        for byte in mac {
            token.push(hex(byte >> 4));
            token.push(hex(byte & 15));
        }
        token.push('o');
        token.push_str(&expires);
        Ok(token)
    }
    /// Check binding, signature and expiry without disclosing which check failed.
    /// # Errors
    /// Invalid, expired or excessively future-dated token.
    pub fn verify_csrf(
        &self,
        prefs: &SignedPrefs,
        token: &str,
        now: u64,
    ) -> Result<(), PrefsError> {
        if !token.is_ascii() || !(66..=85).contains(&token.len()) || token.as_bytes()[64] != b'o' {
            return Err(PrefsError::Csrf);
        }
        let expires = &token[65..];
        let expiry = expires.parse::<u64>().map_err(|_| PrefsError::Csrf)?;
        if expiry.to_string() != expires || expiry < now || expiry.saturating_sub(now) > VALIDITY {
            return Err(PrefsError::Csrf);
        }
        let mut tag = [0; 32];
        for (index, pair) in token.as_bytes()[..64].as_chunks::<2>().0.iter().enumerate() {
            tag[index] = unhex(pair[0])?.checked_mul(16).ok_or(PrefsError::Csrf)? + unhex(pair[1])?;
        }
        self.csrf_mac(prefs, expires)?
            .verify_slice(&tag)
            .map_err(|_| PrefsError::Csrf)
    }
    fn prefs_mac(&self, id: &[u8; 16]) -> Result<Hmac<Sha256>, PrefsError> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(self.prefs.bytes())
            .map_err(|_| PrefsError::Key)?;
        mac.update(id);
        Ok(mac)
    }
    fn csrf_mac(&self, prefs: &SignedPrefs, expires: &str) -> Result<Hmac<Sha256>, PrefsError> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(self.csrf.bytes())
            .map_err(|_| PrefsError::Key)?;
        mac.update(&prefs.0);
        mac.update(expires.as_bytes());
        Ok(mac)
    }
}
fn decode<const N: usize>(value: &str) -> Result<[u8; N], PrefsError> {
    let mut bytes = [0; N];
    if URL_SAFE_NO_PAD
        .decode_slice(value, &mut bytes)
        .map_err(|_| PrefsError::Cookie)?
        != N
    {
        return Err(PrefsError::Cookie);
    }
    Ok(bytes)
}
fn hex(value: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(value)])
}
fn unhex(value: u8) -> Result<u8, PrefsError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(PrefsError::Csrf),
    }
}
