// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{BarcodeError, Encoding, Result};

pub(super) fn check_digit(body: &str) -> Result<u8> {
    if body.is_empty() || body.len() > 17 || !body.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BarcodeError::new("uri", "Invalid EPC URI components."));
    }
    // At most 17 decimal digits, each weighted by at most 3: u16 cannot overflow.
    let sum: u16 = body
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, b)| u16::from(b - b'0') * if i % 2 == 0 { 3 } else { 1 })
        .sum();
    Ok(((10 - sum % 10) % 10) as u8)
}
pub(super) fn valid(code: &str, encoding: Encoding) -> bool {
    let Some(width) = encoding.width() else {
        return true;
    };
    if code.len() != width || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match check_digit(&code[..width - 1]) {
        Ok(digit) => code.as_bytes()[width - 1] - b'0' == digit,
        Err(_) => false,
    }
}
pub(super) fn repair(code: &str) -> Result<String> {
    let end = code
        .len()
        .checked_sub(1)
        .ok_or_else(|| BarcodeError::new("configuration", "Empty checked base code."))?;
    let body = code
        .get(..end)
        .ok_or_else(|| BarcodeError::new("configuration", "Invalid checked base code."))?;
    Ok(format!("{body}{}", check_digit(body)?))
}
