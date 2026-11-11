//! Namespaces, digests and the keys of content and staging objects.

use sha2::{Digest as _, Sha256};

use crate::BlobError;
use crate::error::LimitKind;

/// A namespace: the random UUID created with a database, never its name. Content is addressed and
/// deduplicated inside one namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Namespace(String);

impl Namespace {
    /// Parse the lowercase hyphenated text of a UUID.
    ///
    /// # Errors
    ///
    /// `Configuration` when the text is not a lowercase hyphenated UUID.
    pub fn parse(text: &str) -> Result<Self, BlobError> {
        if is_uuid_text(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(BlobError::config(
                "a namespace must be a lowercase hyphenated UUID",
            ))
        }
    }

    /// A random version 4 namespace.
    ///
    /// # Errors
    ///
    /// `Unavailable` when the operating system has no randomness.
    pub fn random() -> Result<Self, BlobError> {
        Ok(Self(random_uuid_text()?))
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Namespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A random version 4 UUID as lowercase hyphenated text.
pub(crate) fn random_uuid_text() -> Result<String, BlobError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| BlobError::Unavailable {
        retryable: true,
        message: "the operating system has no randomness".to_owned(),
        source: Some(Box::new(error)),
    })?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = to_hex(&bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn is_uuid_text(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// Lowercase hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// Parse lowercase hex into `N` bytes.
///
/// # Errors
///
/// `Configuration` for the wrong length or a non-hex digit.
pub fn from_hex<const N: usize>(text: &str) -> Result<[u8; N], BlobError> {
    let bad = || BlobError::config(format!("expected {} lowercase hex digits", N * 2));
    if text.len() != N * 2 {
        return Err(bad());
    }
    let mut out = [0u8; N];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let high = nibble(*pair.first().ok_or_else(bad)?).ok_or_else(bad)?;
        let low = nibble(*pair.get(1).ok_or_else(bad)?).ok_or_else(bad)?;
        *slot = (high << 4) | low;
    }
    Ok(out)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// The address of immutable content: a namespace and the SHA-256 of the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentKey {
    namespace: Namespace,
    sha256: [u8; 32],
}

impl ContentKey {
    /// The key of `sha256` in `namespace`.
    pub fn new(namespace: Namespace, sha256: [u8; 32]) -> Self {
        Self { namespace, sha256 }
    }

    /// The namespace.
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    /// The digest.
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// `<ns>/sha256/<hex[0..2]>/<hex[2..4]>/<hex>`, relative to the store root.
    pub fn path(&self) -> String {
        let hex = to_hex(&self.sha256);
        format!(
            "{}/sha256/{}/{}/{}",
            self.namespace.as_str(),
            &hex[0..2],
            &hex[2..4],
            hex
        )
    }
}

impl std::fmt::Display for ContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.path())
    }
}

/// The address of a staging object: a namespace and an upload identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StagingKey {
    namespace: Namespace,
    upload_id: String,
}

impl StagingKey {
    /// The staging key of the upload `upload_id` (a lowercase hyphenated UUID).
    ///
    /// # Errors
    ///
    /// `LimitExceeded` (name) when the identifier is not a UUID: a staging key is built from
    /// validated parts only, so a path never carries caller text.
    pub fn new(namespace: Namespace, upload_id: &str) -> Result<Self, BlobError> {
        if !is_uuid_text(upload_id) {
            return Err(BlobError::LimitExceeded {
                kind: LimitKind::NameBytes,
                limit: 36,
            });
        }
        Ok(Self {
            namespace,
            upload_id: upload_id.to_owned(),
        })
    }

    /// The namespace.
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    /// The upload identifier.
    pub fn upload_id(&self) -> &str {
        &self.upload_id
    }

    /// `<ns>/staging/<h[0..2]>/<upload id>`: `h` is the first byte of the SHA-256 of the
    /// identifier, so time-ordered identifiers do not share one prefix.
    pub fn path(&self) -> String {
        let digest = Sha256::digest(self.upload_id.as_bytes());
        let prefix = to_hex(&digest[..1]);
        format!(
            "{}/staging/{}/{}",
            self.namespace.as_str(),
            prefix,
            self.upload_id
        )
    }
}

impl std::fmt::Display for StagingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_refuses_uppercase_and_wrong_length() {
        let bytes: [u8; 4] = [0, 15, 16, 255];
        let text = to_hex(&bytes);
        assert_eq!(text, "000f10ff");
        assert_eq!(from_hex::<4>(&text).unwrap_or_default(), bytes);
        assert!(from_hex::<4>("000F10FF").is_err());
        assert!(from_hex::<4>("000f10").is_err());
    }

    #[test]
    fn random_namespaces_parse_and_differ() {
        let a = Namespace::random().unwrap_or_else(|_| Namespace(String::new()));
        let b = Namespace::random().unwrap_or_else(|_| Namespace(String::new()));
        assert_eq!(Namespace::parse(a.as_str()).as_ref().ok(), Some(&a));
        assert_ne!(a, b);
        assert_eq!(&a.as_str()[14..15], "4");
    }
}
