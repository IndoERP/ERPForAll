// SPDX-License-Identifier: MIT OR Apache-2.0
//! Exact decimal token normalization with bounded expansion.
use super::CodecError;
const MAX: usize = 4096;

pub(super) fn normalize(input: &str) -> Result<String, CodecError> {
    if input.len() > MAX {
        return Err(CodecError::Limit("numeric bytes"));
    }
    let bytes = input.as_bytes();
    let negative = bytes.first() == Some(&b'-');
    let mut pos = usize::from(negative);
    let start = pos;
    while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
        pos += 1;
    }
    if pos == start {
        return Err(CodecError::InvalidNumber);
    }
    let whole_end = pos;
    let mut frac_start = pos;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        frac_start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if pos == frac_start {
            return Err(CodecError::InvalidNumber);
        }
    }
    let frac_end = pos;
    let mut exponent: i32 = 0;
    if matches!(bytes.get(pos), Some(b'e' | b'E')) {
        pos += 1;
        let minus = bytes.get(pos) == Some(&b'-');
        if matches!(bytes.get(pos), Some(b'+' | b'-')) {
            pos += 1;
        }
        let exponent_start = pos;
        while let Some(digit) = bytes.get(pos).filter(|b| b.is_ascii_digit()) {
            exponent = exponent
                .checked_mul(10)
                .and_then(|n| n.checked_add(i32::from(*digit - b'0')))
                .ok_or(CodecError::Limit("numeric bytes"))?;
            pos += 1;
        }
        if pos == exponent_start {
            return Err(CodecError::InvalidNumber);
        }
        if minus {
            exponent = -exponent;
        }
    }
    if pos != bytes.len() {
        return Err(CodecError::InvalidNumber);
    }
    let mut digits = String::from(&input[start..whole_end]);
    digits.push_str(&input[frac_start..frac_end]);
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Ok("0".into());
    }
    let fraction =
        i32::try_from(frac_end - frac_start).map_err(|_| CodecError::Limit("numeric bytes"))?;
    let trailing = i32::try_from(trimmed.len() - trimmed.trim_end_matches('0').len())
        .map_err(|_| CodecError::Limit("numeric bytes"))?;
    let trimmed = trimmed.trim_end_matches('0');
    let scale = fraction
        .checked_sub(exponent)
        .and_then(|n| n.checked_sub(trailing))
        .ok_or(CodecError::Limit("numeric bytes"))?;
    let len = i32::try_from(trimmed.len()).map_err(|_| CodecError::Limit("numeric bytes"))?;
    let point = len
        .checked_sub(scale)
        .ok_or(CodecError::Limit("numeric bytes"))?;
    // Bound the expansion before allocating or repeating any digits.
    let expanded = if point <= 0 {
        2_i64 - i64::from(point) + i64::from(len)
    } else {
        i64::from(point.max(len)) + i64::from(point < len)
    } + i64::from(negative);
    if expanded > MAX as i64 {
        return Err(CodecError::Limit("numeric bytes"));
    }
    let mut out = String::with_capacity(expanded as usize);
    if negative {
        out.push('-');
    }
    if point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-i64::from(point)) as usize));
        out.push_str(trimmed);
    } else if point >= len {
        out.push_str(trimmed);
        out.extend(std::iter::repeat_n('0', (point - len) as usize));
    } else {
        out.push_str(&trimmed[..point as usize]);
        out.push('.');
        out.push_str(&trimmed[point as usize..]);
    }
    Ok(out)
}
