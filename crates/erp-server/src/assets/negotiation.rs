//! Bounded Accept-Encoding selection; the HTTP boundary caps total header bytes.
use super::{PublicationError, PublicationErrorKind, refused};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Encoding {
    Identity,
    Gzip,
    Brotli,
}
impl Encoding {
    pub(super) fn index(self) -> usize {
        match self {
            Self::Identity => 0,
            Self::Gzip => 1,
            Self::Brotli => 2,
        }
    }
}
fn invalid() -> PublicationError {
    refused(PublicationErrorKind::Encoding, "invalid Accept-Encoding")
}
fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}
fn quality(value: &str) -> Result<u16, PublicationError> {
    let (whole, fraction) = match value.split_once('.') {
        Some(parts) => parts,
        None => (value, ""),
    };
    if fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    match whole {
        "0" => {
            let mut n = 0;
            for byte in fraction.bytes() {
                n = n * 10 + u16::from(byte - b'0');
            }
            for _ in fraction.len()..3 {
                n *= 10;
            }
            Ok(n)
        }
        "1" if fraction.bytes().all(|b| b == b'0') => Ok(1000),
        _ => Err(invalid()), // RFC qvalue grammar, never a numeric fallback.
    }
}
pub(super) fn select(
    value: Option<&str>,
    available: [bool; 3],
) -> Result<Option<Encoding>, PublicationError> {
    let Some(value) = value else {
        return Ok(available[0].then_some(Encoding::Identity));
    };
    if value.len() > 8192 {
        return Err(invalid());
    }
    if value.trim_matches([' ', '\t']).is_empty() {
        return Ok(available[0].then_some(Encoding::Identity));
    }
    let mut q = [None; 4];
    let mut seen = Vec::new();
    for item in value.split(',') {
        let item = item.trim_matches([' ', '\t']);
        // Empty members are permitted by HTTP list syntax; a wholly empty list is identity.
        if item.is_empty() {
            continue;
        }
        if seen.len() >= 128 {
            return Err(invalid());
        }
        let mut parts = item.split(';');
        let name = parts.next().ok_or_else(invalid)?.trim_matches([' ', '\t']);
        if !token(name) || seen.iter().any(|old: &&str| old.eq_ignore_ascii_case(name)) {
            return Err(invalid());
        }
        seen.push(name);
        let weight = match parts.next() {
            None => 1000,
            Some(parameter) => {
                let (key, value) = parameter
                    .trim_matches([' ', '\t'])
                    .split_once('=')
                    .ok_or_else(invalid)?;
                if !key.eq_ignore_ascii_case("q") {
                    return Err(invalid());
                }
                quality(value)?
            }
        };
        if parts.next().is_some() {
            return Err(invalid());
        }
        let index = if name.eq_ignore_ascii_case("identity") {
            Some(0)
        } else if name.eq_ignore_ascii_case("gzip") {
            Some(1)
        } else if name.eq_ignore_ascii_case("br") {
            Some(2)
        } else if name == "*" {
            Some(3)
        } else {
            None
        };
        // Unknown valid codings have no available representation; their syntax is still checked.
        if let Some(index) = index {
            q[index] = Some(weight);
        }
    }
    if seen.is_empty() {
        return Ok(available[0].then_some(Encoding::Identity));
    }
    let mut best = None;
    let mut best_weight = 0;
    for encoding in [Encoding::Brotli, Encoding::Gzip, Encoding::Identity] {
        let index = encoding.index();
        let weight = match q[index] {
            Some(n) => n,
            None if index == 0 => 0, // Implicit identity is considered only as the fallback below.
            // This is an absent-field protocol default; malformed weights were refused above.
            None => q[3].unwrap_or_default(),
        };
        if available[index] && weight > best_weight {
            best = Some(encoding);
            best_weight = weight;
        }
    }
    if best.is_none() && available[0] && q[0].is_none() && q[3] != Some(0) {
        return Ok(Some(Encoding::Identity));
    }
    Ok(best)
}
