//! String formatting: printf-style `%`, the format-spec mini-language (f-strings, `str.format`) and
//! `!r`/`!s`/`!a` conversions. Output goes through a capped buffer, so no formatting can allocate
//! past the budget (review round 2, blocking 1 and 6). Widths and precisions are checked per field
//! against the format-width limit.

use crate::datetime;
use crate::error::{ExprError, ExprResult};
use crate::limits::{Budget, Out};
use crate::num;
use crate::value::{ExprValue, Kwargs};

/// A parsed format specification `[[fill]align][sign][z][#][0][width][grouping][.precision][type]`.
#[derive(Debug, Clone, Default)]
struct Spec {
    fill: Option<char>,
    align: Option<char>,
    sign: Option<char>,
    no_neg_zero: bool,
    alt: bool,
    zero: bool,
    width: usize,
    grouping: Option<char>,
    precision: Option<usize>,
    ty: Option<char>,
}

fn parse_number(chars: &[char], i: &mut usize, budget: &Budget) -> ExprResult<Option<usize>> {
    let start = *i;
    let mut n: usize = 0;
    while let Some(c) = chars.get(*i).and_then(|c| c.to_digit(10)) {
        n = n.saturating_mul(10).saturating_add(c as usize);
        *i += 1;
    }
    if *i == start {
        return Ok(None);
    }
    budget.check_format_width(n)?;
    Ok(Some(n))
}

fn invalid_spec(spec: &str, v: &ExprValue) -> ExprError {
    ExprError::value_error(format!(
        "Invalid format specifier '{spec}' for object of type '{}'",
        v.type_name()
    ))
}

fn parse_spec(spec: &str, v: &ExprValue, budget: &Budget) -> ExprResult<Spec> {
    let chars: Vec<char> = spec.chars().collect();
    let mut s = Spec::default();
    let mut i = 0;
    let is_align = |c: Option<&char>| matches!(c, Some('<' | '>' | '=' | '^'));
    if is_align(chars.get(1)) {
        s.fill = chars.first().copied();
        s.align = chars.get(1).copied();
        i = 2;
    } else if is_align(chars.first()) {
        s.align = chars.first().copied();
        i = 1;
    }
    if let Some(c @ ('+' | '-' | ' ')) = chars.get(i) {
        s.sign = Some(*c);
        i += 1;
    }
    if chars.get(i) == Some(&'z') {
        s.no_neg_zero = true;
        i += 1;
    }
    if chars.get(i) == Some(&'#') {
        s.alt = true;
        i += 1;
    }
    if chars.get(i) == Some(&'0') {
        s.zero = true;
        i += 1;
    }
    if let Some(w) = parse_number(&chars, &mut i, budget)? {
        s.width = w;
    }
    if let Some(c @ (',' | '_')) = chars.get(i) {
        s.grouping = Some(*c);
        i += 1;
    }
    if chars.get(i) == Some(&'.') {
        i += 1;
        match parse_number(&chars, &mut i, budget)? {
            Some(p) => s.precision = Some(p),
            None => return Err(ExprError::value_error("Format specifier missing precision")),
        }
    }
    // A single remaining character is a presentation type. Its formatter reports
    // an unknown code with the value's type; extra trailing text is an invalid spec.
    if i + 1 == chars.len() {
        s.ty = chars.get(i).copied();
        i += 1;
    }
    if i != chars.len() {
        return Err(invalid_spec(spec, v));
    }
    Ok(s)
}

/// Formats `v` with a format spec (`format(v, spec)`, f-string `{v:spec}`).
pub(crate) fn format_value(v: &ExprValue, spec: &str, budget: &mut Budget) -> ExprResult<String> {
    if spec.is_empty() {
        return v.str_budgeted(budget);
    }
    match v {
        ExprValue::Date(d) => {
            return datetime::strftime(*d, time::Time::MIDNIGHT, spec, budget);
        }
        ExprValue::DateTime(dt) => return datetime::strftime(dt.date(), dt.time(), spec, budget),
        _ => {}
    }
    let s = parse_spec(spec, v, budget)?;
    let mut out = budget.out();
    match v {
        ExprValue::Str(text) => format_str(text, &s, &mut out)?,
        ExprValue::Int(_) | ExprValue::Bool(_) => {
            let i = v
                .as_int()
                .ok_or_else(|| ExprError::type_error("format of a non-integer"))?;
            match s.ty {
                Some('e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%') => {
                    format_float(i as f64, &s, &mut out)?
                }
                _ => format_int(i, &s, &mut out, v)?,
            }
        }
        ExprValue::Float(f) => match s.ty {
            Some('d' | 'b' | 'o' | 'x' | 'X' | 'c' | 's') => {
                return Err(ExprError::value_error(format!(
                    "Unknown format code '{}' for object of type 'float'",
                    s.ty.unwrap_or('?')
                )));
            }
            _ => format_float(*f, &s, &mut out)?,
        },
        other => {
            return Err(ExprError::type_error(format!(
                "unsupported format string passed to {}.__format__",
                other.type_name()
            )));
        }
    }
    budget.finish(out)
}

fn pad(out: &mut Out, body: &str, prefix: &str, s: &Spec, default_align: char) -> ExprResult<()> {
    let len = body.chars().count() + prefix.chars().count();
    // A `0` before the width means "fill with zeros"; for numbers without an explicit alignment it
    // also means sign-aware padding (`=`).
    let fill = s.fill.unwrap_or(if s.zero { '0' } else { ' ' });
    let align = s.align.unwrap_or(if s.zero && default_align == '>' {
        '='
    } else {
        default_align
    });
    let padding = s.width.saturating_sub(len);
    match align {
        '<' => {
            out.push_str(prefix)?;
            out.push_str(body)?;
            out.pad(fill, padding)
        }
        '^' => {
            out.pad(fill, padding / 2)?;
            out.push_str(prefix)?;
            out.push_str(body)?;
            out.pad(fill, padding - padding / 2)
        }
        '=' => {
            out.push_str(prefix)?;
            out.pad(fill, padding)?;
            out.push_str(body)
        }
        _ => {
            out.pad(fill, padding)?;
            out.push_str(prefix)?;
            out.push_str(body)
        }
    }
}

fn format_str(text: &str, s: &Spec, out: &mut Out) -> ExprResult<()> {
    if s.sign.is_some() {
        return Err(ExprError::value_error(
            "Sign not allowed in string format specifier",
        ));
    }
    if s.alt {
        return Err(ExprError::value_error(
            "Alternate form (#) not allowed in string format specifier",
        ));
    }
    if s.align == Some('=') {
        return Err(ExprError::value_error(
            "'=' alignment not allowed in string format specifier",
        ));
    }
    if !matches!(s.ty, None | Some('s')) {
        return Err(ExprError::value_error(format!(
            "Unknown format code '{}' for object of type 'str'",
            s.ty.unwrap_or('?')
        )));
    }
    let body: String = match s.precision {
        Some(p) => text.chars().take(p).collect(),
        None => text.to_string(),
    };
    pad(out, &body, "", s, '<')
}

fn group_digits(digits: &str, sep: char, every: usize) -> String {
    let n = digits.len();
    let mut out = String::with_capacity(n + n / every);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(every) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

fn sign_prefix(negative: bool, s: &Spec) -> &'static str {
    if negative {
        "-"
    } else {
        match s.sign {
            Some('+') => "+",
            Some(' ') => " ",
            _ => "",
        }
    }
}

fn format_int(i: i64, s: &Spec, out: &mut Out, v: &ExprValue) -> ExprResult<()> {
    if s.precision.is_some() {
        return Err(ExprError::value_error(
            "Precision not allowed in integer format specifier",
        ));
    }
    let mag = i.unsigned_abs();
    let (digits, alt_prefix, group_every) = match s.ty {
        None | Some('d') | Some('n') => (mag.to_string(), "", 3),
        Some('b') => (format!("{mag:b}"), "0b", 4),
        Some('o') => (format!("{mag:o}"), "0o", 4),
        Some('x') => (format!("{mag:x}"), "0x", 4),
        Some('X') => (format!("{mag:X}"), "0X", 4),
        Some('c') => {
            let c = u32::try_from(i)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| ExprError::overflow("%c arg not in range(0x110000)"))?;
            return pad(out, &c.to_string(), "", s, '<');
        }
        Some(other) => {
            return Err(ExprError::value_error(format!(
                "Unknown format code '{other}' for object of type '{}'",
                v.type_name()
            )));
        }
    };
    let body = match s.grouping {
        Some(sep) => group_digits(&digits, sep, group_every),
        None => digits,
    };
    let mut prefix = sign_prefix(i < 0, s).to_string();
    if s.alt {
        prefix.push_str(alt_prefix);
    }
    pad(out, &body, &prefix, s, '>')
}

/// Rust's `{:e}` exponent (`e3`, `e-7`) rewritten as Python's (`e+03`, `e-07`).
fn python_exponent(sci: &str, upper: bool) -> String {
    // Rust's `{:e}` output always has the form `<mantissa>e<int>`.
    let Some((mant, exp)) = sci.split_once('e') else {
        return sci.to_string();
    };
    let Ok(exp) = exp.parse::<i32>() else {
        return sci.to_string();
    };
    let e = if upper { 'E' } else { 'e' };
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mant}{e}{sign}{:02}", exp.unsigned_abs())
}

fn fixed(x: f64, prec: usize) -> String {
    format!("{x:.prec$}")
}

fn exponent(x: f64, prec: usize, upper: bool) -> String {
    python_exponent(&format!("{x:.prec$e}"), upper)
}

/// Decimal exponent of `x` once rounded to `sig` significant digits.
fn decimal_exponent(x: f64, sig: usize) -> i32 {
    let sci = format!("{x:.prec$e}", prec = sig.saturating_sub(1));
    // Rust's `{:e}` output always has the form `<mantissa>e<int>`; 0 is never used in practice.
    sci.split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0)
}

fn strip_zeros(s: &str) -> String {
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let mant = if mant.contains('.') {
        mant.trim_end_matches('0').trim_end_matches('.')
    } else {
        mant
    };
    format!("{mant}{exp}")
}

/// `'g'` formatting; `repr_style` is the no-type variant with a precision (at least one fraction
/// digit in fixed notation, scientific from `exp >= p - 1`).
fn general(x: f64, prec: usize, upper: bool, alt: bool, repr_style: bool) -> String {
    let p = prec.max(1);
    if x == 0.0 {
        let base = if alt {
            fixed(0.0, p - 1)
        } else {
            "0".to_string()
        };
        return if repr_style && !base.contains('.') {
            format!("{base}.0")
        } else {
            base
        };
    }
    let exp = decimal_exponent(x, p);
    let p_i = i32::try_from(p).unwrap_or(i32::MAX);
    let sci_from = if repr_style { p_i - 1 } else { p_i };
    let s = if exp >= -4 && exp < sci_from {
        let decimals = usize::try_from(p_i - 1 - exp).unwrap_or(0);
        fixed(x, decimals)
    } else {
        exponent(x, p - 1, upper)
    };
    let s = if alt { s } else { strip_zeros(&s) };
    if repr_style && !s.contains(['.', 'e', 'E']) {
        format!("{s}.0")
    } else {
        s
    }
}

fn format_float(x: f64, s: &Spec, out: &mut Out) -> ExprResult<()> {
    let upper = matches!(s.ty, Some('E' | 'F' | 'G'));
    let negative = x.is_sign_negative() && !(s.no_neg_zero && x == 0.0) && !x.is_nan();
    let mag = x.abs();
    let body = if !mag.is_finite() {
        let t = if mag.is_nan() { "nan" } else { "inf" };
        if upper {
            t.to_uppercase()
        } else {
            t.to_string()
        }
    } else {
        match s.ty {
            Some('f' | 'F') => fixed(mag, s.precision.unwrap_or(6)),
            Some('e' | 'E') => exponent(mag, s.precision.unwrap_or(6), upper),
            Some('g' | 'G' | 'n') => general(mag, s.precision.unwrap_or(6), upper, s.alt, false),
            Some('%') => format!("{}%", fixed(mag * 100.0, s.precision.unwrap_or(6))),
            None => match s.precision {
                Some(p) => general(mag, p, false, s.alt, true),
                None => num::float_repr(mag),
            },
            Some(other) => {
                return Err(ExprError::value_error(format!(
                    "Unknown format code '{other}' for object of type 'float'"
                )));
            }
        }
    };
    let body = match s.grouping {
        Some(sep) => {
            let (int_part, rest) = match body.find(|c: char| !c.is_ascii_digit()) {
                Some(i) => (&body[..i], &body[i..]),
                None => (body.as_str(), ""),
            };
            format!("{}{rest}", group_digits(int_part, sep, 3))
        }
        None => body,
    };
    pad(out, &body, sign_prefix(negative, s), s, '>')
}

/// Writes one replacement field (f-string `{v!c:spec}`, `str.format` field) into `out`. The common
/// cases (no spec) write straight into the output without an intermediate copy.
pub(crate) fn write_field(
    out: &mut Out,
    value: &ExprValue,
    conv: Option<char>,
    spec: &str,
    budget: &mut Budget,
) -> ExprResult<()> {
    if spec.is_empty() {
        match conv {
            None | Some('s') => {
                budget.steps(value.meta().nodes)?;
                return value.write_str(out);
            }
            Some('r') => {
                budget.steps(value.meta().nodes)?;
                return value.write_repr(out);
            }
            _ => {}
        }
    }
    let piece = match conv {
        None => format_value(value, spec, budget)?,
        Some(_) => {
            let converted = convert(value, conv, budget)?;
            format_value(&converted, spec, budget)?
        }
    };
    out.push_str(&piece)
}

/// Applies an f-string / `str.format` conversion (`!r`, `!s`, `!a`).
pub(crate) fn convert(
    v: &ExprValue,
    conv: Option<char>,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    match conv {
        None => Ok(v.clone()),
        Some('s') => Ok(ExprValue::Str(v.str_budgeted(budget)?)),
        Some('r') => Ok(ExprValue::Str(v.repr_budgeted(budget)?)),
        Some('a') => Ok(ExprValue::Str(ascii(&v.repr_budgeted(budget)?, budget)?)),
        Some(other) => Err(ExprError::value_error(format!(
            "Unknown conversion specifier {other}"
        ))),
    }
}

/// Python `ascii()` escaping of an already-`repr`'d string.
fn ascii(r: &str, budget: &mut Budget) -> ExprResult<String> {
    let mut out = budget.out();
    for c in r.chars() {
        let cp = c as u32;
        if c.is_ascii() {
            out.push(c)?;
        } else if cp <= 0xff {
            out.push_str(&format!("\\x{cp:02x}"))?;
        } else if cp <= 0xffff {
            out.push_str(&format!("\\u{cp:04x}"))?;
        } else {
            out.push_str(&format!("\\U{cp:08x}"))?;
        }
    }
    budget.finish(out)
}

// ---------------------------------------------------------------------------------------------
// printf-style formatting: `'%s and %(key)d' % args`
// ---------------------------------------------------------------------------------------------

/// `template % arg` for strings.
pub(crate) fn printf(template: &str, arg: &ExprValue, budget: &mut Budget) -> ExprResult<String> {
    budget.scan(template.len())?;
    let (args, mapping): (&[ExprValue], Option<&crate::value::Map>) = match arg {
        ExprValue::Tuple(items) => (items.items(), None),
        ExprValue::Dict(m) => (std::slice::from_ref(arg), Some(m)),
        _ => (std::slice::from_ref(arg), None),
    };
    let mut next_arg = 0usize;
    let mut used_mapping = false;
    let mut out = budget.out();
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        i += 1;
        if c != '%' {
            out.push(c)?;
            continue;
        }
        // Mapping key.
        budget.tick()?;
        let mut value: Option<&ExprValue> = None;
        if chars.get(i) == Some(&'(') {
            let Some(map) = mapping else {
                return Err(ExprError::type_error("format requires a mapping"));
            };
            let mut depth = 1;
            let key_start = i + 1;
            let mut j = key_start;
            while let Some(&k) = chars.get(j) {
                if k == '(' {
                    depth += 1;
                } else if k == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            if depth != 0 {
                return Err(ExprError::value_error("incomplete format key"));
            }
            let key: String = chars.get(key_start..j).unwrap_or(&[]).iter().collect();
            let found = map
                .get_str(&key)
                .ok_or_else(|| ExprError::key_error(ExprValue::Str(key.clone()).repr_short()))?;
            value = Some(found);
            used_mapping = true;
            i = j + 1;
        }
        // Flags.
        let mut flags = Spec::default();
        let mut left = false;
        while let Some(&f) = chars.get(i) {
            match f {
                '-' => left = true,
                '+' => flags.sign = Some('+'),
                ' ' => {
                    if flags.sign.is_none() {
                        flags.sign = Some(' ');
                    }
                }
                '#' => flags.alt = true,
                '0' => flags.zero = true,
                _ => break,
            }
            i += 1;
        }
        // Width.
        let digit_chars = &chars;
        if digit_chars.get(i) == Some(&'*') {
            i += 1;
            let w = take_arg(args, &mut next_arg)?
                .as_int()
                .ok_or_else(|| ExprError::type_error("* wants int"))?;
            if w < 0 {
                left = true;
            }
            let w = usize::try_from(w.unsigned_abs()).unwrap_or(usize::MAX);
            budget.check_format_width(w)?;
            flags.width = w;
        } else if let Some(w) = parse_number(digit_chars, &mut i, budget)? {
            flags.width = w;
        }
        // Precision.
        if digit_chars.get(i) == Some(&'.') {
            i += 1;
            if digit_chars.get(i) == Some(&'*') {
                i += 1;
                let p = take_arg(args, &mut next_arg)?
                    .as_int()
                    .ok_or_else(|| ExprError::type_error("* wants int"))?;
                let p = usize::try_from(p.max(0)).unwrap_or(usize::MAX);
                budget.check_format_width(p)?;
                flags.precision = Some(p);
            } else {
                flags.precision = Some(parse_number(digit_chars, &mut i, budget)?.unwrap_or(0));
            }
        }
        // Length modifiers are accepted and ignored, as in Python.
        while matches!(digit_chars.get(i), Some('h' | 'l' | 'L')) {
            i += 1;
        }
        let Some(&conv) = digit_chars.get(i) else {
            return Err(ExprError::value_error("incomplete format"));
        };
        i += 1;
        if conv == '%' {
            out.push('%')?;
            continue;
        }
        if left {
            flags.align = Some('<');
            flags.zero = false;
        }
        let v = match value {
            Some(v) => v,
            None => {
                if mapping.is_some() && !matches!(arg, ExprValue::Tuple(_)) {
                    // `'%s' % {'a': 1}` formats the dict itself.
                    used_mapping = true;
                }
                take_arg(args, &mut next_arg)?
            }
        };
        printf_one(conv, i - 1, v, &flags, &mut out, budget)?;
    }
    if next_arg < args.len() && !used_mapping && mapping.is_none() {
        return Err(ExprError::type_error(
            "not all arguments converted during string formatting",
        ));
    }
    budget.finish(out)
}

fn take_arg<'a>(args: &'a [ExprValue], next: &mut usize) -> ExprResult<&'a ExprValue> {
    let v = args
        .get(*next)
        .ok_or_else(|| ExprError::type_error("not enough arguments for format string"))?;
    *next += 1;
    Ok(v)
}

fn printf_one(
    conv: char,
    index: usize,
    v: &ExprValue,
    flags: &Spec,
    out: &mut Out,
    budget: &mut Budget,
) -> ExprResult<()> {
    match conv {
        'd' | 'i' | 'u' => {
            let i = match v {
                ExprValue::Float(f) => num::float_to_int(*f)?,
                ExprValue::Object(o) if o.is_record() => o.as_int()?,
                other => other.as_int().ok_or_else(|| {
                    ExprError::type_error(format!(
                        "%{conv} format: a real number is required, not {}",
                        other.type_name()
                    ))
                })?,
            };
            printf_int(i, 10, false, flags, out)
        }
        'o' | 'x' | 'X' => {
            let i = v.as_int().ok_or_else(|| {
                ExprError::type_error(format!(
                    "%{conv} format: an integer is required, not {}",
                    v.type_name()
                ))
            })?;
            let radix = if conv == 'o' { 8 } else { 16 };
            printf_int(i, radix, conv == 'X', flags, out)
        }
        'e' | 'E' | 'f' | 'F' | 'g' | 'G' => {
            let f = v.as_f64().ok_or_else(|| {
                ExprError::type_error(format!("must be real number, not {}", v.type_name()))
            })?;
            let mut spec = flags.clone();
            spec.ty = Some(conv);
            spec.precision = Some(flags.precision.unwrap_or(6));
            if spec.zero && spec.align.is_none() {
                spec.align = Some('=');
                spec.fill = Some('0');
            }
            format_float(f, &spec, out)
        }
        'c' => {
            let c = match v {
                ExprValue::Str(s) if s.chars().count() == 1 => s.clone(),
                other => {
                    let i = other
                        .as_int()
                        .ok_or_else(|| ExprError::type_error("%c requires int or char"))?;
                    u32::try_from(i)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| ExprError::overflow("%c arg not in range(0x110000)"))?
                        .to_string()
                }
            };
            let spec = Spec {
                zero: false,
                ..flags.clone()
            };
            pad(out, &c, "", &spec, '>')
        }
        's' | 'r' if flags.width == 0 && flags.precision.is_none() => {
            budget.steps(v.meta().nodes)?;
            if conv == 's' {
                v.write_str(out)
            } else {
                v.write_repr(out)
            }
        }
        's' | 'r' | 'a' => {
            let text = match conv {
                's' => v.str_budgeted(budget)?,
                'r' => v.repr_budgeted(budget)?,
                _ => ascii(&v.repr_budgeted(budget)?, budget)?,
            };
            let body: String = match flags.precision {
                Some(p) => text.chars().take(p).collect(),
                None => text,
            };
            let spec = Spec {
                zero: false,
                ..flags.clone()
            };
            pad(out, &body, "", &spec, '>')
        }
        other => Err(ExprError::value_error(format!(
            "unsupported format character '{other}' (0x{:x}) at index {index}",
            other as u32
        ))),
    }
}

fn printf_int(i: i64, radix: u32, upper: bool, flags: &Spec, out: &mut Out) -> ExprResult<()> {
    let mag = i.unsigned_abs();
    let mut digits = match (radix, upper) {
        (8, _) => format!("{mag:o}"),
        (16, false) => format!("{mag:x}"),
        (16, true) => format!("{mag:X}"),
        _ => mag.to_string(),
    };
    if let Some(p) = flags.precision
        && digits.len() < p
    {
        digits = format!("{}{digits}", "0".repeat(p - digits.len()));
    }
    let mut prefix = sign_prefix(i < 0, flags).to_string();
    if flags.alt {
        prefix.push_str(match (radix, upper) {
            (8, _) => "0o",
            (16, false) => "0x",
            (16, true) => "0X",
            _ => "",
        });
    }
    let spec = Spec {
        fill: Some(if flags.zero && flags.align.is_none() {
            '0'
        } else {
            ' '
        }),
        align: Some(match flags.align {
            Some(a) => a,
            None if flags.zero => '=',
            None => '>',
        }),
        zero: false,
        width: flags.width,
        ..Spec::default()
    };
    pad(out, &digits, &prefix, &spec, '>')
}

// ---------------------------------------------------------------------------------------------
// str.format
// ---------------------------------------------------------------------------------------------

/// `template.format(*args, **kwargs)`.
pub(crate) fn str_format(
    template: &str,
    args: &[ExprValue],
    kwargs: &Kwargs,
    budget: &mut Budget,
) -> ExprResult<String> {
    budget.scan(template.len())?;
    let mut out = budget.out();
    let mut chars = template.chars().peekable();
    let mut auto: Option<usize> = None;
    let mut manual = false;
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{')?;
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}')?;
            }
            '}' => {
                return Err(ExprError::value_error(
                    "Single '}' encountered in format string",
                ));
            }
            '{' => {
                let mut field = String::new();
                let mut closed = false;
                for f in chars.by_ref() {
                    if f == '}' {
                        closed = true;
                        break;
                    }
                    if f == '{' {
                        return Err(ExprError::value_error(
                            "nested replacement fields in format specs are not supported",
                        ));
                    }
                    field.push(f);
                }
                if !closed {
                    return Err(ExprError::value_error(
                        "Single '{' encountered in format string",
                    ));
                }
                let (head, spec) = match field.split_once(':') {
                    Some((h, s)) => (h, s),
                    None => (field.as_str(), ""),
                };
                let (name, conv) = match head.split_once('!') {
                    Some((n, c)) => {
                        let mut cs = c.chars();
                        let conv = cs.next();
                        if cs.next().is_some() || conv.is_none() {
                            return Err(ExprError::value_error(
                                "expected ':' after conversion specifier",
                            ));
                        }
                        (n, conv)
                    }
                    None => (head, None),
                };
                if name.contains(['.', '[']) {
                    return Err(ExprError::value_error(
                        "format field accessors ('.' and '[') are not supported",
                    ));
                }
                let value = if name.is_empty() {
                    if manual {
                        return Err(ExprError::value_error(
                            "cannot switch from manual field specification to automatic field numbering",
                        ));
                    }
                    let idx = auto.map_or(0, |a| a + 1);
                    auto = Some(idx);
                    args.get(idx).ok_or_else(|| {
                        ExprError::index_error(format!(
                            "Replacement index {idx} out of range for positional args tuple"
                        ))
                    })?
                } else if let Ok(idx) = name.parse::<usize>() {
                    if auto.is_some() {
                        return Err(ExprError::value_error(
                            "cannot switch from automatic field numbering to manual field specification",
                        ));
                    }
                    manual = true;
                    args.get(idx).ok_or_else(|| {
                        ExprError::index_error(format!(
                            "Replacement index {idx} out of range for positional args tuple"
                        ))
                    })?
                } else {
                    kwargs
                        .get(name)
                        .ok_or_else(|| ExprError::key_error(ExprValue::str(name).repr_short()))?
                };
                write_field(&mut out, value, conv, spec, budget)?;
            }
            other => out.push(other)?,
        }
    }
    budget.finish(out)
}
