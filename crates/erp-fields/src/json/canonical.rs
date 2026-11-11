//! RFC 8785 (JSON Canonicalization Scheme, JCS) serialization of [`Json`], the input of the
//! SHA-256 request digest that binds an idempotency key to its request.
//!
//! Output: no whitespace; object members sorted by key compared as sequences of UTF-16 code
//! units (RFC 8785 §3.2.3); arrays in order; strings with only `"`, `\` and the control
//! characters escaped (§3.2.2.2); numbers as ECMAScript `Number::toString` (§3.2.2.3).
//!
//! One documented deviation: [`Json::Int`] and [`Json::BigInt`] are written as their exact
//! decimal integer. JCS formats every number as an IEEE double, which would make distinct
//! integers beyond 2^53 produce the same canonical text (and the same digest); within ±2^53 the
//! output is identical to JCS.
//!
//! The writer keeps pending work on an explicit stack, so nesting depth never grows the Rust
//! stack.

use crate::json::Json;

/// Append the canonical form of `value` to `out` (UTF-8 bytes).
///
/// Never fails. A [`Json::Float`] is finite when it comes from [`crate::json::parse`]; a
/// non-finite one built elsewhere is written as `NaN`, `Infinity` or `-Infinity`, which no
/// JSON value canonicalizes to, so distinct values still never share a canonical form.
///
/// # Example
///
/// ```
/// use erp_fields::json::{parse, write_canonical};
///
/// let value = parse(br#"{"b": 1, "a": [true, 4.50]}"#, 64).unwrap();
/// let mut out = Vec::new();
/// write_canonical(&value, &mut out);
/// assert_eq!(out, br#"{"a":[true,4.5],"b":1}"#);
/// ```
pub fn write_canonical(value: &Json, out: &mut Vec<u8>) {
    let mut work = vec![Work::Value(value)];
    while let Some(item) = work.pop() {
        match item {
            Work::Punct(byte) => out.push(byte),
            Work::Key(key) => write_string(key, out),
            Work::Value(Json::Null) => out.extend_from_slice(b"null"),
            Work::Value(Json::Bool(true)) => out.extend_from_slice(b"true"),
            Work::Value(Json::Bool(false)) => out.extend_from_slice(b"false"),
            Work::Value(Json::Int(n)) => out.extend_from_slice(n.to_string().as_bytes()),
            Work::Value(Json::BigInt(text)) => out.extend_from_slice(text.as_bytes()),
            Work::Value(Json::Float(f)) => write_float(*f, out),
            Work::Value(Json::Str(text)) => write_string(text, out),
            Work::Value(Json::Array(items)) => {
                out.push(b'[');
                work.push(Work::Punct(b']'));
                // Pushed in reverse so they pop in document order, commas between.
                for (index, item) in items.iter().enumerate().rev() {
                    work.push(Work::Value(item));
                    if index > 0 {
                        work.push(Work::Punct(b','));
                    }
                }
            }
            Work::Value(Json::Object(members)) => {
                out.push(b'{');
                work.push(Work::Punct(b'}'));
                let mut sorted: Vec<&(Box<str>, Json)> = members.iter().collect();
                sorted.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
                for (index, (key, member)) in sorted.into_iter().enumerate().rev() {
                    work.push(Work::Value(member));
                    work.push(Work::Punct(b':'));
                    work.push(Work::Key(key));
                    if index > 0 {
                        work.push(Work::Punct(b','));
                    }
                }
            }
        }
    }
}

/// The canonical form of `value` (see [`write_canonical`]).
///
/// # Example
///
/// ```
/// use erp_fields::json::{canonical, parse};
///
/// let value = parse(br#"{"b": 1, "a": 2}"#, 64).unwrap();
/// assert_eq!(canonical(&value), br#"{"a":2,"b":1}"#);
/// ```
pub fn canonical(value: &Json) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

/// A pending step of [`write_canonical`].
enum Work<'a> {
    /// A value to write.
    Value(&'a Json),
    /// An object key to write as a string.
    Key(&'a str),
    /// A single structural byte (`,` `:` `]` `}`).
    Punct(u8),
}

/// A string literal with the RFC 8785 §3.2.2.2 escapes.
fn write_string(text: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    let mut buf = [0u8; 4];
    for c in text.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\u{0}'..='\u{1f}' => {
                // `\u00XX`, lowercase hex: the high nibble is 0 or 1 below U+0020.
                let code = u32::from(c);
                out.extend_from_slice(if code >= 0x10 { b"\\u001" } else { b"\\u000" });
                out.push(lower_hex_digit(code & 0xF));
            }
            other => out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes()),
        }
    }
    out.push(b'"');
}

/// The lowercase hex digit of a nibble (`nibble` < 16).
fn lower_hex_digit(nibble: u32) -> u8 {
    match nibble {
        0 => b'0',
        1 => b'1',
        2 => b'2',
        3 => b'3',
        4 => b'4',
        5 => b'5',
        6 => b'6',
        7 => b'7',
        8 => b'8',
        9 => b'9',
        10 => b'a',
        11 => b'b',
        12 => b'c',
        13 => b'd',
        14 => b'e',
        // Callers mask with 0xF, so this arm is 15 only.
        _ => b'f',
    }
}

/// A float as ECMAScript `Number::toString` (RFC 8785 §3.2.2.3).
fn write_float(f: f64, out: &mut Vec<u8>) {
    if f.is_nan() {
        out.extend_from_slice(b"NaN");
        return;
    }
    if f.is_infinite() {
        out.extend_from_slice(if f > 0.0 { b"Infinity" } else { b"-Infinity" });
        return;
    }
    if f == 0.0 {
        // Both 0.0 and -0.0.
        out.push(b'0');
        return;
    }
    if f < 0.0 {
        out.push(b'-');
    }
    // Rust's `{:e}` without a precision gives the shortest digits that round-trip.
    let scientific = format!("{:e}", f.abs());
    match ecmascript_digits(&scientific) {
        Some(text) => out.extend_from_slice(text.as_bytes()),
        // Not reachable: `{:e}` of a finite non-zero f64 is always `d[.ddd]e[-]x`. Writing the
        // `{:e}` text keeps the output exact and distinct per value rather than panicking.
        None => out.extend_from_slice(scientific.as_bytes()),
    }
}

/// Lay out the digits of a positive finite float given as Rust's `{:e}` text (`d[.ddd]e[-]x`)
/// by the ECMAScript rules; `None` when the text does not have that shape.
fn ecmascript_digits(scientific: &str) -> Option<String> {
    let (mantissa, exponent) = scientific.split_once('e')?;
    // A malformed exponent leaves the caller's exact fallback in charge (see `write_float`).
    let exponent: i64 = exponent.parse().ok()?;
    let digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // value = 0.<digits> × 10^n, with k digits.
    let k = i64::try_from(digits.len()).ok()?;
    let n = exponent.checked_add(1)?;
    let mut text = String::new();
    if k <= n && n <= 21 {
        text.push_str(&digits);
        for _ in 0..n.checked_sub(k)? {
            text.push('0');
        }
    } else if 0 < n && n <= 21 {
        let (integer, fraction) = digits.split_at_checked(usize::try_from(n).ok()?)?;
        text.push_str(integer);
        text.push('.');
        text.push_str(fraction);
    } else if -6 < n && n <= 0 {
        text.push_str("0.");
        for _ in 0..n.checked_neg()? {
            text.push('0');
        }
        text.push_str(&digits);
    } else {
        let (first, rest) = digits.split_at_checked(1)?;
        text.push_str(first);
        if !rest.is_empty() {
            text.push('.');
            text.push_str(rest);
        }
        let shown = n.checked_sub(1)?;
        text.push('e');
        text.push(if shown < 0 { '-' } else { '+' });
        text.push_str(&shown.unsigned_abs().to_string());
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn text(value: &Json) -> String {
        String::from_utf8(canonical(value)).expect("canonical output is UTF-8")
    }

    fn num(f: f64) -> String {
        text(&Json::Float(f))
    }

    fn parsed(input: &str) -> Json {
        parse(input.as_bytes(), 64).expect("test input parses")
    }

    #[test]
    fn rfc8785_number_examples() {
        assert_eq!(num(1e30), "1e+30");
        assert_eq!(num(4.5), "4.5");
        assert_eq!(num(0.002), "0.002");
        assert_eq!(num(1e-7), "1e-7");
        assert_eq!(num(1.2345678901234568e20), "123456789012345680000");
        assert_eq!(num(1e21), "1e+21");
        assert_eq!(num(333333333.3333333), "333333333.3333333");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(5e-324), "5e-324");
        assert_eq!(num(1.7976931348623157e308), "1.7976931348623157e+308");
    }

    #[test]
    fn number_layout_boundaries() {
        assert_eq!(num(1e20), "100000000000000000000");
        assert_eq!(num(123.0), "123");
        assert_eq!(num(-1.5), "-1.5");
        assert_eq!(num(0.000001), "0.000001");
        assert_eq!(num(1.5e-7), "1.5e-7");
        assert_eq!(num(-2.5e25), "-2.5e+25");
        assert_eq!(num(0.1), "0.1");
        assert_eq!(num(f64::MIN_POSITIVE), "2.2250738585072014e-308");
        assert_eq!(num(f64::NAN), "NaN");
        assert_eq!(num(f64::INFINITY), "Infinity");
        assert_eq!(num(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn integers_are_exact() {
        assert_eq!(text(&Json::Int(0)), "0");
        assert_eq!(text(&Json::Int(-42)), "-42");
        assert_eq!(text(&Json::Int(i64::MIN)), "-9223372036854775808");
        assert_eq!(text(&Json::Int(i64::MAX)), "9223372036854775807");
        // Beyond 2^53 distinct integers stay distinct (JCS would round both to one double).
        assert_ne!(
            canonical(&Json::Int(9_007_199_254_740_993)),
            canonical(&Json::Int(9_007_199_254_740_992))
        );
        assert_eq!(
            text(&parsed("-9223372036854775809")),
            "-9223372036854775809"
        );
        // A float with an integral value canonicalizes like the integer (as in JCS).
        assert_eq!(canonical(&parsed("1.0")), canonical(&parsed("1")));
        assert_eq!(canonical(&parsed("-0")), canonical(&parsed("-0.0")));
    }

    #[test]
    fn rfc8785_section_3_2_example() {
        // The RFC's string member: escapes of U+20AC, `$`, escapes of U+000F and U+000A, `A'`,
        // escapes of U+0042, U+0022 and U+005C, then `\\`, `\"` and `\/`. Assembled from pieces
        // so every `\u` escape stays an escape in the JSON text.
        let escape = |hex: &str| format!("\\u{hex}");
        let string_member = [
            escape("20ac"),
            "$".to_owned(),
            escape("000F"),
            escape("000a"),
            "A'".to_owned(),
            escape("0042"),
            escape("0022"),
            escape("005c"),
            r#"\\\"\/"#.to_owned(),
        ]
        .concat();
        let input = format!(
            r#"{{
            "numbers": [333333333.33333329, 1E30, 4.50, 2e-3, 0.000000000000000000000000001],
            "string": "{string_member}",
            "literals": [null, true, false]
        }}"#
        );
        assert_eq!(
            text(&parsed(&input)),
            r#"{"literals":[null,true,false],"numbers":[333333333.3333333,1e+30,4.5,0.002,1e-27],"string":"€$\u000f\nA'B\"\\\\\"/"}"#
        );
    }

    #[test]
    fn keys_sort_by_utf16_code_units() {
        // U+1F600 is D83D DE00 in UTF-16, below U+FB33, although above it by code point.
        let value = Json::Object(vec![
            ("\u{FB33}".into(), Json::Int(1)),
            ("\u{1F600}".into(), Json::Int(2)),
            ("b".into(), Json::Int(3)),
            ("a".into(), Json::Int(4)),
            ("".into(), Json::Int(5)),
            ("\u{80}".into(), Json::Int(6)),
            ("ab".into(), Json::Int(7)),
        ]);
        assert_eq!(
            text(&value),
            "{\"\":5,\"a\":4,\"ab\":7,\"b\":3,\"\u{80}\":6,\"\u{1F600}\":2,\"\u{FB33}\":1}"
        );
    }

    #[test]
    fn string_escapes() {
        let value = Json::Str("\"\\\u{8}\t\n\u{c}\r\u{0}\u{1}\u{1b}\u{1f}/\u{7f}é\u{1F600}".into());
        assert_eq!(
            text(&value),
            "\"\\\"\\\\\\b\\t\\n\\f\\r\\u0000\\u0001\\u001b\\u001f/\u{7f}é\u{1F600}\""
        );
    }

    #[test]
    fn nested_and_order_independent() {
        let a = parsed(r#"{"b":[1,{"z":null,"y":true}],"a":{"d":false,"c":"x"}}"#);
        assert_eq!(
            text(&a),
            r#"{"a":{"c":"x","d":false},"b":[1,{"y":true,"z":null}]}"#
        );
        let b = parsed(
            "{\r\n  \"a\" : { \"c\" : \"x\" , \"d\" : false } ,\n\t\"b\" : [ 1 , { \"y\" : true , \"z\" : null } ]\n}",
        );
        assert_eq!(canonical(&a), canonical(&b));
        assert_eq!(text(&parsed("[ ]")), "[]");
        assert_eq!(text(&parsed("{ }")), "{}");
        assert_eq!(text(&parsed(r#"[[],{},[[1]]]"#)), "[[],{},[[1]]]");
        // Arrays keep their order.
        assert_ne!(canonical(&parsed("[1,2]")), canonical(&parsed("[2,1]")));
    }

    #[test]
    fn deep_nesting_is_iterative() {
        let depth = 1024;
        let input = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let value = parse(input.as_bytes(), depth).expect("depth at the limit parses");
        assert_eq!(text(&value), input);
    }

    #[test]
    fn write_canonical_appends() {
        let mut out = b"prefix:".to_vec();
        write_canonical(&parsed(r#"{"k": "v"}"#), &mut out);
        assert_eq!(out, b"prefix:{\"k\":\"v\"}");
    }
}
