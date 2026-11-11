//! Value validators and writers shared by the encoder and the decoder.

use crate::error::WireError;
use crate::limits::{Limit, WireLimits};

/// A path built only when an error needs it.
pub(crate) type PathFn<'a> = &'a dyn Fn() -> String;

/// The largest integer a JavaScript number holds exactly.
pub(crate) const MAX_SAFE: i64 = (1 << 53) - 1;

/// Record ids are 1..=2^53-1.
pub(crate) fn check_id(id: u64, path: PathFn<'_>) -> Result<(), WireError> {
    if id == 0 || id > MAX_SAFE as u64 {
        return Err(WireError::BadId {
            path: path(),
            value: id.to_string(),
        });
    }
    Ok(())
}

pub(crate) fn check_safe_int(n: i64, path: PathFn<'_>) -> Result<(), WireError> {
    if !(-MAX_SAFE..=MAX_SAFE).contains(&n) {
        return Err(WireError::ValueOutOfRange {
            path: path(),
            reason: "integer is outside the safe range +-(2^53-1)".to_owned(),
        });
    }
    Ok(())
}

pub(crate) fn check_float(f: f64, path: PathFn<'_>) -> Result<(), WireError> {
    if !f.is_finite() {
        return Err(WireError::ValueOutOfRange {
            path: path(),
            reason: "float is not finite".to_owned(),
        });
    }
    Ok(())
}

pub(crate) fn check_string(
    s: &str,
    limits: &WireLimits,
    path: PathFn<'_>,
) -> Result<(), WireError> {
    if u64::try_from(s.len()).map_or(true, |n| n > limits.get(Limit::StringBytes)) {
        return Err(WireError::LimitExceeded {
            limit: Limit::StringBytes.name(),
            path: path(),
            maximum: limits.get(Limit::StringBytes),
        });
    }
    Ok(())
}

fn digits(s: &[u8]) -> Option<u32> {
    s.iter().try_fold(0_u32, |acc, b| {
        b.is_ascii_digit().then(|| acc * 10 + u32::from(b - b'0'))
    })
}

fn valid_date(s: &[u8]) -> bool {
    if s.len() != 10 || s[4] != b'-' || s[7] != b'-' {
        return false;
    }
    let (Some(y), Some(m), Some(d)) = (digits(&s[0..4]), digits(&s[5..7]), digits(&s[8..10]))
    else {
        return false;
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    y >= 1 && (1..=days).contains(&d)
}

pub(crate) fn check_date(s: &str, path: PathFn<'_>) -> Result<(), WireError> {
    if valid_date(s.as_bytes()) {
        return Ok(());
    }
    Err(WireError::ValueOutOfRange {
        path: path(),
        reason: "not a valid YYYY-MM-DD date".to_owned(),
    })
}

pub(crate) fn check_datetime(s: &str, path: PathFn<'_>) -> Result<(), WireError> {
    let b = s.as_bytes();
    let ok = b.len() == 19
        && valid_date(&b[0..10])
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
        && matches!(digits(&b[11..13]), Some(h) if h < 24)
        && matches!(digits(&b[14..16]), Some(m) if m < 60)
        && matches!(digits(&b[17..19]), Some(s) if s < 60);
    if ok {
        return Ok(());
    }
    Err(WireError::ValueOutOfRange {
        path: path(),
        reason: "not a valid YYYY-MM-DD HH:MM:SS datetime".to_owned(),
    })
}

/// Validate an id list (each id, the length limit, no duplicates) and return it ascending. `path`
/// names the list; element errors use `path[j]`.
pub(crate) fn normalise_ids(
    ids: &[u64],
    limits: &WireLimits,
    path: PathFn<'_>,
) -> Result<Vec<u64>, WireError> {
    limits.check(Limit::IdsPerList, ids.len(), &path())?;
    for (j, id) in ids.iter().enumerate() {
        check_id(*id, &|| format!("{}[{j}]", path()))?;
    }
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    if let Some(pair) = sorted.windows(2).find(|w| w[0] == w[1]) {
        return Err(WireError::DuplicateId {
            path: path(),
            id: pair[0],
        });
    }
    Ok(sorted)
}

/// Append `s` as a JSON string.
pub(crate) fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub(crate) fn write_ids(out: &mut String, ids: &[u64]) {
    out.push('[');
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&id.to_string());
    }
    out.push(']');
}

/// Append a finite float; a non-finite value is an error, never a substitute.
pub(crate) fn write_float(out: &mut String, f: f64, path: PathFn<'_>) -> Result<(), WireError> {
    let number = serde_json::Number::from_f64(f).ok_or_else(|| WireError::ValueOutOfRange {
        path: path(),
        reason: "float is not finite".to_owned(),
    })?;
    out.push_str(&number.to_string());
    Ok(())
}
