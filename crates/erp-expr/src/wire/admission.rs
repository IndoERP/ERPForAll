//! Keep browser numeric rounding local to AST JSON admission.
use super::{MAX_ENCODED_BYTES, MAX_JSON_DEPTH, WireError, byte_limit};
use std::borrow::Cow;

// Every unquoted numeric schema slot is u32 (or a byte narrowed from u32). An expanding
// exponent spelling needs at least three bytes; its canonical u32 needs at most ten.
const MAX_NORMALIZED_BYTES: usize = 4 * MAX_ENCODED_BYTES;

pub(super) fn normalize(bytes: &[u8]) -> Result<Cow<'_, [u8]>, WireError> {
    let mut output: Option<Vec<u8>> = None;
    let mut copied = 0;
    scan(bytes, |start, end, value, unchanged| {
        if !unchanged {
            let buffer = match &mut output {
                Some(buffer) => buffer,
                None => {
                    let mut buffer = Vec::new();
                    reserve(&mut buffer, bytes.len(), MAX_NORMALIZED_BYTES)?;
                    output.insert(buffer)
                }
            };
            append(buffer, &bytes[copied..start], MAX_NORMALIZED_BYTES)?;
            let mut digits = [0; 10];
            append(buffer, decimal(value, &mut digits), MAX_NORMALIZED_BYTES)?;
            copied = end;
        }
        Ok(())
    })?;
    match output {
        None => Ok(Cow::Borrowed(bytes)),
        Some(mut buffer) => {
            append(&mut buffer, &bytes[copied..], MAX_NORMALIZED_BYTES)?;
            Ok(Cow::Owned(buffer))
        }
    }
}

fn normalized_limit() -> WireError {
    WireError::new("$", "normalized AST JSON exceeds 67108864 bytes")
}

fn reserve(output: &mut Vec<u8>, required: usize, limit: usize) -> Result<(), WireError> {
    if required > limit {
        return Err(normalized_limit());
    }
    if required > output.capacity() {
        let capacity = output
            .capacity()
            .checked_mul(2)
            .ok_or_else(normalized_limit)?
            .min(limit)
            .max(required);
        let additional = capacity
            .checked_sub(output.len())
            .ok_or_else(normalized_limit)?;
        output.try_reserve_exact(additional).map_err(|error| {
            WireError::new("$", format!("cannot reserve normalized AST JSON: {error}"))
        })?;
    }
    Ok(())
}

fn append(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), WireError> {
    let required = output
        .len()
        .checked_add(bytes.len())
        .ok_or_else(normalized_limit)?;
    reserve(output, required, limit)?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn decimal(mut value: u32, buffer: &mut [u8; 10]) -> &[u8] {
    let mut cursor = buffer.len();
    loop {
        // A u32 has at most ten digits, including zero's single digit.
        cursor -= 1;
        buffer[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buffer[cursor..];
        }
    }
}

fn invalid(offset: usize, detail: &str) -> WireError {
    WireError::json(<serde_json::Error as serde::de::Error>::custom(format!(
        "{detail} at input byte {offset}"
    )))
}

fn number(bytes: &[u8], start: usize) -> Result<(usize, u32, bool), WireError> {
    let mut cursor = start;
    if bytes[cursor] == b'-' {
        cursor += 1; // cursor stays within this already byte-bounded input.
    }
    match bytes.get(cursor) {
        Some(b'0') => cursor += 1,
        Some(b'1'..=b'9') => {
            cursor += 1;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
        }
        _ => return Err(invalid(start, "invalid JSON number integer part")),
    }
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let first = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if first == cursor {
            return Err(invalid(start, "invalid JSON number fraction"));
        }
    }
    if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
        cursor += 1;
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            cursor += 1;
        }
        let first = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if first == cursor {
            return Err(invalid(start, "invalid JSON number exponent"));
        }
    }
    if bytes
        .get(cursor)
        .is_some_and(|byte| !matches!(byte, b',' | b']' | b'}' | b' ' | b'\t' | b'\r' | b'\n'))
    {
        return Err(invalid(start, "invalid JSON number delimiter"));
    }
    let token = std::str::from_utf8(&bytes[start..cursor])
        .map_err(|_| invalid(start, "invalid JSON number encoding"))?;
    let canonical = !token.starts_with('-') && !token.contains(['.', 'e', 'E']);
    let range_error = || invalid(start, "expected an integer in 0..=4294967295");
    let (value, unchanged) = if canonical {
        (token.parse::<u32>().map_err(|_| range_error())?, true)
    } else if let Some(integer) = token
        .strip_suffix("e0")
        .or_else(|| token.strip_suffix("E0"))
        && integer.bytes().all(|byte| byte.is_ascii_digit())
    {
        // Exact u32 significands with exponent zero remain exact in ordinary serde_json's
        // f64 parser. Every numeric schema slot uses IntegerVisitor::visit_f64, not the
        // default u32 deserializer. Grammar/delimiters were validated above.
        (integer.parse::<u32>().map_err(|_| range_error())?, true)
    } else {
        let value = token.parse::<f64>().map_err(|_| range_error())?;
        if !value.is_finite()
            || !(0.0..=f64::from(u32::MAX)).contains(&value)
            || value.fract() != 0.0
        {
            return Err(range_error());
        }
        // Finite range and integrality checked; -0 and signed underflow admit zero.
        (value as u32, false)
    };
    Ok((cursor, value, unchanged))
}

fn scan(
    bytes: &[u8],
    mut visit: impl FnMut(usize, usize, u32, bool) -> Result<(), WireError>,
) -> Result<(), WireError> {
    if bytes.len() > MAX_ENCODED_BYTES {
        return Err(byte_limit());
    }
    let (mut cursor, mut depth, mut string, mut escaped) = (0, 0_usize, false, false);
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string = false;
            }
        } else {
            match byte {
                b'"' => string = true,
                b'[' | b'{' => {
                    depth += 1; // At most MAX_JSON_DEPTH before this increment.
                    if depth > MAX_JSON_DEPTH {
                        return Err(WireError::new("$", "JSON nesting exceeds 16"));
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1), // serde rejects unmatched closers.
                b'-' | b'0'..=b'9' => {
                    let (end, value, canonical) = number(bytes, cursor)?;
                    visit(cursor, end, value, canonical)?;
                    cursor = end;
                    continue;
                }
                // Unchanged syntax/UTF-8 is subsequently checked by serde; never accepted here.
                _ => {}
            }
        }
        cursor += 1; // Bounded by bytes.len() <= MAX_ENCODED_BYTES.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_input_borrows_and_alternate_numbers_expand_exactly() {
        let canonical = br#"{"schema":1,"nodes":[{"kind":"name","name":"0e0"}]}"#;
        assert!(matches!(normalize(canonical).unwrap(), Cow::Borrowed(_)));
        assert_eq!(
            normalize(b"[1e9,-0,1e-9999,4294967294.9999999]")
                .unwrap()
                .as_ref(),
            b"[1000000000,0,0,4294967295]"
        );
    }

    #[test]
    fn normalization_append_refuses_growth_past_bound_before_mutation() {
        let mut bytes = vec![b'0'];
        let error = append(&mut bytes, b"1", 1).unwrap_err();
        assert_eq!(error.kind(), super::super::WireErrorKind::InvalidAst);
        assert_eq!(
            error.to_string(),
            "InvalidAst at $: normalized AST JSON exceeds 67108864 bytes"
        );
        assert_eq!(bytes, b"0");
    }
    #[test]
    fn error_after_rewrite_never_returns_partial_output() {
        let error = normalize(b"[1e9,01]").unwrap_err();
        assert_eq!(error.kind(), super::super::WireErrorKind::InvalidAst);
        assert_eq!(
            error.to_string(),
            "InvalidAst at $: invalid JSON: invalid JSON number delimiter at input byte 5"
        );
    }

    #[test]
    fn expanding_tokens_and_escaped_strings_keep_their_meaning() {
        let input = br#"[1e9,1e9,1e9,1e9,"1e9\\\"0e0"]"#;
        assert_eq!(
            normalize(input).unwrap().as_ref(),
            br#"[1000000000,1000000000,1000000000,1000000000,"1e9\\\"0e0"]"#
        );
    }

    #[test]
    fn initial_reservation_refuses_bound_before_allocation() {
        let mut output = Vec::new();
        let error = reserve(&mut output, 2, 1).unwrap_err();
        assert_eq!(error.kind(), super::super::WireErrorKind::InvalidAst);
        assert_eq!(
            error.to_string(),
            "InvalidAst at $: normalized AST JSON exceeds 67108864 bytes"
        );
        assert_eq!(output.capacity(), 0);
        assert!(output.is_empty());
    }

    #[test]
    fn exact_unsigned_zero_exponents_borrow_including_u32_boundaries() {
        let input = b"[0e0,1E0,4294967295e0]";
        assert!(matches!(normalize(input).unwrap(), Cow::Borrowed(_)));
        for input in [b"[-0e0]".as_slice(), b"[1.0e0]", b"[1e+0]", b"[1e00]"] {
            assert!(matches!(normalize(input).unwrap(), Cow::Owned(_)));
        }
        for (input, detail) in [
            (
                b"[4294967296e0]".as_slice(),
                "expected an integer in 0..=4294967295",
            ),
            (b"[01e0]", "invalid JSON number delimiter"),
            (b"[1e0x]", "invalid JSON number delimiter"),
        ] {
            let error = normalize(input).unwrap_err();
            assert_eq!(error.kind(), super::super::WireErrorKind::InvalidAst);
            assert_eq!(
                error.to_string(),
                format!("InvalidAst at $: invalid JSON: {detail} at input byte 1")
            );
        }
    }
}
