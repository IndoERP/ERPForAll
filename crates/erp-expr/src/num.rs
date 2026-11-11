//! Numeric helpers with Python semantics on 64-bit integers and IEEE floats.
//!
//! Every integer operation is checked: a result outside `i64` is an `OverflowError`, identical in
//! debug and release builds.

use std::cmp::Ordering;

use crate::error::{ExprError, ExprResult};

pub(crate) fn overflow() -> ExprError {
    ExprError::overflow("integer result does not fit in 64 bits")
}

pub(crate) fn add(a: i64, b: i64) -> ExprResult<i64> {
    a.checked_add(b).ok_or_else(overflow)
}

pub(crate) fn sub(a: i64, b: i64) -> ExprResult<i64> {
    a.checked_sub(b).ok_or_else(overflow)
}

pub(crate) fn mul(a: i64, b: i64) -> ExprResult<i64> {
    a.checked_mul(b).ok_or_else(overflow)
}

pub(crate) fn neg(a: i64) -> ExprResult<i64> {
    a.checked_neg().ok_or_else(overflow)
}

pub(crate) fn abs(a: i64) -> ExprResult<i64> {
    a.checked_abs().ok_or_else(overflow)
}

/// Python `a // b`: rounds toward negative infinity.
pub(crate) fn floor_div(a: i64, b: i64) -> ExprResult<i64> {
    if b == 0 {
        return Err(ExprError::zero_division(
            "integer division or modulo by zero",
        ));
    }
    let q = a.checked_div(b).ok_or_else(overflow)?;
    let r = a.checked_rem(b).ok_or_else(overflow)?;
    if r != 0 && ((r < 0) != (b < 0)) {
        sub(q, 1)
    } else {
        Ok(q)
    }
}

/// Python `a % b`: the result has the sign of the divisor.
pub(crate) fn modulo(a: i64, b: i64) -> ExprResult<i64> {
    if b == 0 {
        return Err(ExprError::zero_division("integer modulo by zero"));
    }
    // `i64::MIN % -1` is mathematically 0; `checked_rem` reports it as overflow.
    let r = a.checked_rem(b).unwrap_or(0);
    if r != 0 && ((r < 0) != (b < 0)) {
        add(r, b)
    } else {
        Ok(r)
    }
}

/// `a ** b` for `b >= 0`, by squaring with checked multiplication, so `(-1) ** (2 ** 40)` is 1
/// while `2 ** 64` is an `OverflowError`.
pub(crate) fn pow(a: i64, b: i64) -> ExprResult<i64> {
    if b < 0 {
        return Err(ExprError::value_error(
            "integer power with a negative exponent",
        ));
    }
    match a {
        0 => return Ok(if b == 0 { 1 } else { 0 }),
        1 => return Ok(1),
        -1 => return Ok(if b % 2 == 0 { 1 } else { -1 }),
        _ => {}
    }
    // |a| >= 2: anything past 2**63 overflows.
    if b >= 64 {
        return Err(overflow());
    }
    let mut result: i64 = 1;
    let mut base = a;
    let mut exp = b;
    loop {
        if exp & 1 == 1 {
            result = mul(result, base)?;
        }
        exp >>= 1;
        if exp == 0 {
            return Ok(result);
        }
        base = mul(base, base)?;
    }
}

fn check_shift(b: i64) -> ExprResult<u32> {
    if b < 0 {
        return Err(ExprError::value_error("negative shift count"));
    }
    if b >= 64 {
        return Err(ExprError::overflow(format!(
            "shift count {b} is not below 64"
        )));
    }
    u32::try_from(b).map_err(|_| overflow())
}

/// `a << b`: errors when the shifted value leaves the 64-bit range (`1 << 63`).
pub(crate) fn shl(a: i64, b: i64) -> ExprResult<i64> {
    let s = check_shift(b)?;
    let r = a.checked_shl(s).ok_or_else(overflow)?;
    if (r >> s) != a {
        return Err(overflow());
    }
    Ok(r)
}

/// `a >> b` (arithmetic).
pub(crate) fn shr(a: i64, b: i64) -> ExprResult<i64> {
    let s = check_shift(b)?;
    Ok(a >> s)
}

/// The integer a float holds exactly, when it holds one in the `i64` range.
pub(crate) fn float_as_int(f: f64) -> Option<i64> {
    // 2**63 as f64 is exact; every finite f64 below it with no fraction converts exactly.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if f.is_finite() && f.fract() == 0.0 && (-LIMIT..LIMIT).contains(&f) {
        Some(f as i64)
    } else {
        None
    }
}

/// Exact comparison of an integer with a float, as Python does (no rounding of the integer).
pub(crate) fn cmp_int_float(a: i64, f: f64) -> Option<Ordering> {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if f.is_nan() {
        return None;
    }
    if f >= LIMIT {
        return Some(Ordering::Less);
    }
    if f < -LIMIT {
        return Some(Ordering::Greater);
    }
    let fl = f.floor();
    let fi = fl as i64;
    match a.cmp(&fi) {
        Ordering::Equal if f > fl => Some(Ordering::Less),
        other => Some(other),
    }
}

/// Exact `int == float`.
pub(crate) fn eq_int_float(a: i64, f: f64) -> bool {
    float_as_int(f) == Some(a)
}

/// Python's float `divmod`: returns `(floor quotient, remainder with the divisor's sign)`.
/// `zero_message` is CPython's message for a zero divisor (`float modulo`,
/// `float floor division by zero`).
pub(crate) fn float_divmod(a: f64, b: f64, zero_message: &str) -> ExprResult<(f64, f64)> {
    if b == 0.0 {
        return Err(ExprError::zero_division(zero_message));
    }
    let mut m = a % b;
    let mut div = (a - m) / b;
    if m != 0.0 {
        if (b < 0.0) != (m < 0.0) {
            m += b;
            div -= 1.0;
        }
    } else {
        m = 0.0_f64.copysign(b);
    }
    let floordiv = if div != 0.0 {
        let f = div.floor();
        if div - f > 0.5 { f + 1.0 } else { f }
    } else {
        0.0_f64.copysign(a / b)
    };
    Ok((floordiv, m))
}

/// Python float `**`.
pub(crate) fn float_pow(a: f64, b: f64) -> ExprResult<f64> {
    if a == 0.0 && b < 0.0 {
        return Err(ExprError::zero_division(
            "0.0 cannot be raised to a negative power",
        ));
    }
    if a < 0.0 && b.is_finite() && b.fract() != 0.0 {
        return Err(ExprError::value_error(
            "negative number raised to a fractional power has a complex result, which is not supported",
        ));
    }
    let r = a.powf(b);
    if r.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(ExprError::overflow("(34, 'Numerical result out of range')"));
    }
    Ok(r)
}

/// `int(f)`: truncation toward zero with Python's errors.
pub(crate) fn float_to_int(f: f64) -> ExprResult<i64> {
    if f.is_nan() {
        return Err(ExprError::value_error(
            "cannot convert float NaN to integer",
        ));
    }
    if f.is_infinite() {
        return Err(ExprError::overflow(
            "cannot convert float infinity to integer",
        ));
    }
    float_as_int(f.trunc()).ok_or_else(overflow)
}

/// Python `repr(float)`: the shortest string that round-trips, in fixed notation for decimal
/// exponents in `[-4, 16)` and scientific notation otherwise (`1e+16`, `1.5e-05`).
pub(crate) fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    // `{:e}` is Rust's shortest round-trip representation in scientific form: "1.2345e-5".
    let sci = format!("{x:e}");
    let (mantissa, exp) = match sci.split_once('e') {
        Some(parts) => parts,
        None => return sci,
    };
    let Ok(exp) = exp.parse::<i32>() else {
        return sci;
    };
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = usize::try_from(exp).unwrap_or(0) + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            let zeros = usize::try_from(-exp - 1).unwrap_or(0);
            out.extend(std::iter::repeat_n('0', zeros));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exp.unsigned_abs()));
    }
    out
}

/// Python `round(x)` for floats: ties to even, result an integer.
pub(crate) fn round_float_to_int(x: f64) -> ExprResult<i64> {
    float_to_int(x.round_ties_even())
}

/// Python `round(x, n)` for floats: the decimal value nearest to `x` with `n` fraction digits,
/// ties to even on the exact binary value (Rust's fixed-precision formatting rounds exactly that
/// way), so `round(0.125, 2) == 0.12` and `round(2.675, 2) == 2.67`.
pub(crate) fn round_float(x: f64, ndigits: i64) -> ExprResult<f64> {
    if !x.is_finite() {
        return Ok(x);
    }
    if ndigits > 323 {
        return Ok(x);
    }
    if ndigits < -308 {
        return Ok(0.0_f64.copysign(x));
    }
    if ndigits >= 0 {
        let prec = usize::try_from(ndigits).unwrap_or(0);
        let s = format!("{x:.prec$}");
        return s
            .parse::<f64>()
            .map_err(|_| ExprError::value_error(format!("cannot round {s}")));
    }
    let p = 10f64.powi(i32::try_from(-ndigits).unwrap_or(308));
    let r = (x / p).round_ties_even() * p;
    if r.is_infinite() {
        return Err(ExprError::overflow("rounded value too large to represent"));
    }
    Ok(r)
}

/// Python `round(i, n)` for integers with `n < 0`: ties to even.
pub(crate) fn round_int(i: i64, ndigits: i64) -> ExprResult<i64> {
    if ndigits >= 0 {
        return Ok(i);
    }
    if ndigits < -18 {
        return Ok(0);
    }
    let p = pow(10, -ndigits)?;
    let q = floor_div(i, p)?;
    let r = modulo(i, p)?;
    let twice = i128::from(r) * 2;
    let p128 = i128::from(p);
    let q = if twice > p128 || (twice == p128 && q % 2 != 0) {
        add(q, 1)?
    } else {
        q
    };
    mul(q, p)
}
