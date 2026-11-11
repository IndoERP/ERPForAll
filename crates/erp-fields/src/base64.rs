//! Standard base64 (RFC 4648 §4, padded) with strict decoding: the binary value format of JSON-2
//! and of binary fields.
//!
//! Decoding refuses anything a lenient decoder would repair: characters outside the standard
//! alphabet (the URL-safe `-`/`_` included), whitespace and line breaks, missing or extra padding,
//! and non-zero bits after the last encoded byte (a non-canonical encoding).
//!
//! ```
//! use erp_fields::base64::{decode_strict, encode};
//! assert_eq!(encode(b"x  \x01-"), "eCAgAS0=");
//! assert_eq!(decode_strict("WCABIQ==").unwrap(), b"X \x01!");
//! assert_eq!(
//!     decode_strict("WCAB IQ=").unwrap_err().to_string(),
//!     "invalid character ' ' at position 4"
//! );
//! ```

use std::fmt;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Why a text is not strict base64.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Base64Error {
    /// The length is not a multiple of 4.
    Length(usize),
    /// A character outside the alphabet (or `=` before the end).
    Character {
        /// The character.
        character: char,
        /// Its byte position.
        position: usize,
    },
    /// The last group's padding bits are not zero (a non-canonical encoding).
    TrailingBits,
}

impl fmt::Display for Base64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(n) => write!(f, "length {n} is not a multiple of 4"),
            Self::Character {
                character,
                position,
            } => write!(f, "invalid character {character:?} at position {position}"),
            Self::TrailingBits => f.write_str("non-zero padding bits"),
        }
    }
}

impl std::error::Error for Base64Error {}

/// Encode `bytes` as standard padded base64.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        out.extend(quad(chunk).map(char::from));
    }
    out
}

/// Append `bytes` as standard padded base64 to `out` (the JSON-2 writer: no `String` per value).
///
/// ```
/// let mut out = b"=".to_vec();
/// erp_fields::base64::encode_into(&mut out, b"x  \x01-");
/// assert_eq!(out, b"=eCAgAS0=");
/// ```
pub fn encode_into(out: &mut Vec<u8>, bytes: &[u8]) {
    out.reserve(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        out.extend_from_slice(&quad(chunk));
    }
}

/// The four base64 characters of a chunk of one to three bytes, padded with `=`.
fn quad(chunk: &[u8]) -> [u8; 4] {
    // a short last chunk is padded with zero bits, as base64 specifies
    let byte = |i: usize| chunk.get(i).copied().map_or(0, u32::from);
    let n = (byte(0) << 16) | (byte(1) << 8) | byte(2);
    let sextet = |shift: u32| ALPHABET[((n >> shift) & 63) as usize];
    [
        sextet(18),
        sextet(12),
        if chunk.len() > 1 { sextet(6) } else { b'=' },
        if chunk.len() > 2 { sextet(0) } else { b'=' },
    ]
}

fn value_of(byte: u8) -> Option<u32> {
    let v = match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

/// Decode standard padded base64, refusing every non-canonical form.
///
/// # Errors
///
/// [`Base64Error`] naming the first problem.
pub fn decode_strict(text: &str) -> Result<Vec<u8>, Base64Error> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(Base64Error::Length(bytes.len()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let groups = bytes.len() / 4;
    for (g, group) in bytes.chunks(4).enumerate() {
        let last = g + 1 == groups;
        let pad = if last {
            group.iter().rev().take_while(|b| **b == b'=').count()
        } else {
            0
        };
        if pad > 2 {
            return Err(bad_char(text, g * 4 + 4 - pad));
        }
        let mut n: u32 = 0;
        for (i, byte) in group.iter().enumerate().take(4 - pad) {
            let v = value_of(*byte).ok_or_else(|| bad_char(text, g * 4 + i))?;
            n |= v << (18 - 6 * i as u32);
        }
        let [_, b0, b1, b2] = n.to_be_bytes();
        match pad {
            0 => out.extend_from_slice(&[b0, b1, b2]),
            1 => {
                if b2 != 0 {
                    return Err(Base64Error::TrailingBits);
                }
                out.extend_from_slice(&[b0, b1]);
            }
            _ => {
                if b1 != 0 || b2 != 0 {
                    return Err(Base64Error::TrailingBits);
                }
                out.push(b0);
            }
        }
    }
    Ok(out)
}

fn bad_char(text: &str, position: usize) -> Base64Error {
    let character = text
        .get(position..)
        .and_then(|rest| rest.chars().next())
        .unwrap_or('\u{fffd}');
    Base64Error::Character {
        character,
        position,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_length() {
        let data: Vec<u8> = (0..=255u8).collect();
        for len in 0..data.len() {
            let text = encode(&data[..len]);
            assert_eq!(decode_strict(&text).unwrap(), &data[..len], "len {len}");
        }
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"test"), "dGVzdA==");
        assert_eq!(encode(b"content"), "Y29udGVudA==");
    }

    #[test]
    fn refuses_non_canonical_forms() {
        let cases: &[(&str, &str)] = &[
            ("dGVzdA", "length 6 is not a multiple of 4"),
            ("dGVzdA=", "length 7 is not a multiple of 4"),
            ("dGVz\ndA==", "length 9 is not a multiple of 4"),
            ("dGV zdA=", "invalid character ' ' at position 3"),
            ("dGVzdA-_", "invalid character '-' at position 6"),
            ("d===", "invalid character '=' at position 1"),
            ("dG=z", "invalid character '=' at position 2"),
            ("dG==dGVz", "invalid character '=' at position 2"),
            ("dGVzdB==", "non-zero padding bits"),
            ("dGVzdA=B", "invalid character '=' at position 6"),
            ("é==", "invalid character 'é' at position 0"),
        ];
        for (text, message) in cases {
            assert_eq!(
                decode_strict(text).unwrap_err().to_string(),
                *message,
                "{text:?}"
            );
        }
    }
}
