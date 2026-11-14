// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded numeric wire primitives. No Decimal/f64 conversion or SQL text cast.
use super::BoxError;
use crate::money::{CanonicalInteger, ExactNumericError, GridValue, MAX_BITS, MAX_GRID_SCALE};
use bytes::{BufMut, BytesMut};
use postgres_types::{FromSql, IsNull, ToSql, Type, accepts, to_sql_checked};

// A 4096-bit integer has at most 1234 decimal digits; allow seven fractional groups
// for the finite-grid decoder. Bounds derive from the frozen representation, not a setting.
const MAX_DIGITS: usize = (MAX_BITS as usize * 30103) / 100000 + 1;
const MAX_GROUPS: usize = MAX_DIGITS.div_ceil(4) + MAX_GRID_SCALE as usize / 4 + 1;

fn bad() -> BoxError {
    ExactNumericError::InvalidEncoding.into()
}
fn oid(ty: &Type, operation: &str) -> Result<(), BoxError> {
    if *ty != Type::NUMERIC {
        return Err(crate::Error::programming(format!(
            "wide numeric SQL {operation} expected numeric"
        ))
        .into());
    }
    Ok(())
}
fn word(raw: &[u8], offset: usize) -> Result<u16, BoxError> {
    let bytes: [u8; 2] = raw
        .get(offset..offset + 2)
        .ok_or_else(bad)?
        .try_into()
        .map_err(|_| bad())?;
    Ok(u16::from_be_bytes(bytes))
}

/// Decode only bounded weights/groups, with significant scale distinct from display dscale.
fn decode(ty: &Type, raw: &[u8]) -> Result<(String, u32), BoxError> {
    oid(ty, "decoder")?;
    if raw.len() < 8 || raw.len() > 8 + MAX_GROUPS * 2 {
        return Err(bad());
    }
    let count = usize::from(word(raw, 0)?);
    let weight = i32::from(i16::from_be_bytes(word(raw, 2)?.to_be_bytes()));
    let sign = word(raw, 4)?;
    let dscale = usize::from(word(raw, 6)?);
    if count > MAX_GROUPS
        || raw.len() != 8 + count * 2
        || (sign != 0 && sign != 0x4000)
        || dscale > 0x3fff
    {
        return Err(bad());
    }
    if count == 0 {
        if sign != 0 || weight != 0 {
            return Err(bad());
        }
        return Ok(("0".to_owned(), 0));
    }
    // Reject huge exponent work before allocating or walking missing groups.
    let last = weight - i32::try_from(count).map_err(|_| bad())? + 1;
    if weight > i32::try_from(MAX_DIGITS.div_ceil(4)).map_err(|_| bad())? - 1
        || last < -7
        || word(raw, 8)? == 0
        || word(raw, 8 + (count - 1) * 2)? == 0
    {
        return Err(bad());
    }
    for i in 0..count {
        if word(raw, 8 + i * 2)? >= 10000 {
            return Err(bad());
        }
    }
    let mut text = String::with_capacity(MAX_DIGITS + MAX_GRID_SCALE as usize + 3);
    for exponent in (last.min(0)..=weight.max(0)).rev() {
        if exponent == -1 {
            text.push('.');
        }
        let index = weight - exponent;
        let group = if index >= 0 && usize::try_from(index).map_err(|_| bad())? < count {
            word(raw, 8 + usize::try_from(index).map_err(|_| bad())? * 2)?
        } else {
            0
        }; // A missing base-10000 position is exactly zero by the wire format.
        if exponent == weight.max(0) {
            text.push_str(&group.to_string());
        } else {
            text.push_str(&format!("{group:04}"));
        }
    }
    let scale = match text.split_once('.') {
        Some((_, fractional)) => fractional.trim_end_matches('0').len(),
        None => 0,
    };
    if scale > dscale || scale > MAX_GRID_SCALE as usize {
        return Err(bad());
    }
    if text.contains('.') {
        let length = text.trim_end_matches('0').trim_end_matches('.').len();
        text.truncate(length);
    }
    if sign == 0x4000 {
        text.insert(0, '-');
    }
    Ok((text, u32::try_from(scale).map_err(|_| bad())?))
}

fn encode(ty: &Type, text: &str, scale: u32, out: &mut BytesMut) -> Result<IsNull, BoxError> {
    oid(ty, "encoder")?;
    let (negative, body) = match text.strip_prefix('-') {
        Some(body) => (true, body),
        None => (false, text),
    };
    let (whole, fraction) = match body.split_once('.') {
        Some(pair) => pair,
        None => (body, ""),
    };
    let whole_groups = whole.len().div_ceil(4);
    let mut digits = "0".repeat(whole_groups * 4 - whole.len());
    digits.push_str(whole);
    digits.push_str(fraction);
    digits.extend(std::iter::repeat_n('0', (4 - fraction.len() % 4) % 4));
    let mut groups = Vec::with_capacity(digits.len() / 4);
    let (chunks, remainder) = digits.as_bytes().as_chunks::<4>();
    if !remainder.is_empty() {
        return Err(bad());
    }
    for chunk in chunks {
        // Validated ASCII digits and exactly four positions: each accumulation is <=9999.
        let group = chunk
            .iter()
            .fold(0_u16, |v, digit| v * 10 + u16::from(digit - b'0'));
        groups.push(group);
    }
    let start = match groups.iter().position(|g| *g != 0) {
        Some(i) => i,
        None => groups.len(),
    };
    let end = match groups.iter().rposition(|g| *g != 0) {
        Some(i) => i + 1,
        None => groups.len(),
    };
    let groups = &groups[start..end];
    let weight = if groups.is_empty() {
        0
    } else {
        i16::try_from(whole_groups).map_err(|_| bad())?
            - 1
            - i16::try_from(start).map_err(|_| bad())?
    };
    let count = u16::try_from(groups.len()).map_err(|_| bad())?;
    let scale = u16::try_from(scale).map_err(|_| bad())?;
    // Complete the fallible work before appending any bytes to the caller's buffer.
    out.put_u16(count);
    out.put_i16(weight);
    out.put_u16(if negative { 0x4000 } else { 0 });
    out.put_u16(scale);
    for group in groups {
        out.put_u16(*group);
    }
    Ok(IsNull::No)
}
impl<'a> FromSql<'a> for GridValue {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        let (text, scale) = decode(ty, raw)?;
        Self::parse(&text, scale).map_err(|e| match e {
            ExactNumericError::InvalidInput => bad(),
            other => other.into(),
        })
    }
    accepts!(NUMERIC);
}
impl ToSql for GridValue {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        encode(ty, &self.to_string(), self.scale(), out)
    }
    accepts!(NUMERIC);
    to_sql_checked!();
}
impl<'a> FromSql<'a> for CanonicalInteger {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        let (text, scale) = decode(ty, raw)?;
        if scale != 0 {
            return Err(bad());
        }
        Ok(Self::parse(&text)?)
    }
    accepts!(NUMERIC);
}
impl ToSql for CanonicalInteger {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        encode(ty, &self.to_string(), 0, out)
    }
    accepts!(NUMERIC);
    to_sql_checked!();
}
