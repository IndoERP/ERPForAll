//! Browser-token primitives owned by authentication; the server never reads database secrets.
pub use erp_security::auth::browser::{PrefsError, PrefsSigner, SignedPrefs};

/// Failure to obtain cryptographic randomness for a response-local nonce.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NonceError {
    /// The operating system random source failed; no weaker fallback is used.
    #[error("response nonce random source failed")]
    RandomSource,
}

/// A response-local 128-bit random value encoded as 32 lowercase hexadecimal characters.
///
/// Generate once for a response and pass the same instance to that response's HTML and CSP
/// renderer. Never store it as a session identifier or reuse it across responses. Debug output
/// redacts the value so ordinary diagnostics do not disclose an active response's CSP nonce.
///
/// ```
/// let nonce = erp_server::prefs::ResponseNonce::generate()?;
/// assert_eq!(nonce.as_str().len(), 32);
/// # Ok::<(), erp_server::prefs::NonceError>(())
/// ```
#[non_exhaustive]
pub struct ResponseNonce(String);

impl std::fmt::Debug for ResponseNonce {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("ResponseNonce(<redacted>)")
    }
}

impl ResponseNonce {
    /// Obtain fresh cryptographic randomness from the operating system.
    ///
    /// # Errors
    /// [`NonceError::RandomSource`] if the operating system cannot supply randomness.
    pub fn generate() -> Result<Self, NonceError> {
        Self::generate_with(|bytes| getrandom::fill(bytes).map_err(|_| ()))
    }

    fn generate_with(
        fill: impl FnOnce(&mut [u8; 16]) -> Result<(), ()>,
    ) -> Result<Self, NonceError> {
        let mut bytes = [0; 16];
        fill(&mut bytes).map_err(|()| NonceError::RandomSource)?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(32);
        for byte in bytes {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0xf)]));
        }
        Ok(Self(encoded))
    }

    /// Header- and HTML-attribute-safe representation; the caller owns response rendering.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn nonce_entropy_failure_is_returned_without_a_fallback() {
        let error = ResponseNonce::generate_with(|_| Err(())).unwrap_err();
        assert_eq!(error, NonceError::RandomSource);
        assert_eq!(error.to_string(), "response nonce random source failed");
    }

    #[test]
    fn nonce_encodes_every_random_byte() {
        let nonce = ResponseNonce::generate_with(|bytes| {
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = u8::try_from(index).unwrap();
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(nonce.as_str(), "000102030405060708090a0b0c0d0e0f");
    }
}
