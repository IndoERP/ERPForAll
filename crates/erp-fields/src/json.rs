//! Strict JSON parser and RFC 8785 canonical writer.
//!
//! Used for JSON-2 request bodies and digests (`erp-rpc`) and for query ASTs (`erp-domain`).
//!
//! [`parse`] produces the
//! same detail texts and positions for the common error categories (positions count code points,
//! like Python: `<message>: line L column C (char N)`), and is stricter than Python where Python
//! accepts input we cannot or do not want to represent:
//!
//! - duplicate keys in one object are refused (`duplicate key '<key>'`);
//! - nesting deeper than the caller's limit is refused (`nesting deeper than <n> levels`);
//! - `NaN`, `Infinity` and `-Infinity` are not JSON and give `Expecting value`;
//! - a fraction/exponent literal that overflows `f64` gives `number out of range` (Python: `inf`);
//! - a number literal with more than 4096 digits gives `number too long`;
//! - a lone UTF-16 surrogate escape gives `Invalid \uXXXX escape` (Python keeps it; a Rust
//!   string cannot hold it);
//! - invalid UTF-8 gives `invalid UTF-8 at byte N` (Python decodes bytes before parsing).
//!
//! The parser keeps its open containers on an explicit stack, so nesting never grows the Rust
//! stack, and every byte is examined a bounded number of times (work is linear in input size).
//! Positions are byte offsets into a slice, so they never exceed `isize::MAX`; the
//! `saturating_add` calls on them therefore never saturate.

use std::collections::HashSet;
use std::fmt;
use std::num::IntErrorKind;

mod canonical;

pub use canonical::{canonical, write_canonical};

/// Most digits a number literal may have; a longer literal gives `number too long`.
pub(crate) const MAX_NUMBER_DIGITS: usize = 4096;

/// Most characters of a duplicate key shown in the error detail (longer keys end with `…`).
const DUPLICATE_KEY_SHOWN_CHARS: usize = 64;

/// A parsed JSON value. Objects keep insertion order (keys are unique: duplicates are refused).
///
/// Not `#[non_exhaustive]` on purpose: consumers match it exhaustively, so a new variant must
/// break them (same reasoning as [`crate::ErrorKind`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer that fits i64.
    Int(i64),
    /// An integer literal outside i64 (kept as its exact text, e.g. "-9223372036854775809"); the
    /// binder reports it as 422 naming the argument.
    BigInt(Box<str>),
    /// A number with a fraction or exponent (always finite).
    Float(f64),
    /// A string, with escapes decoded.
    Str(Box<str>),
    /// An array, in document order.
    Array(Vec<Json>),
    /// An object's members, in document order, with unique keys.
    Object(Vec<(Box<str>, Json)>),
}

impl Json {
    /// The JSON type of this value as named in error messages: `"null"`, `"boolean"`,
    /// `"integer"` (both [`Json::Int`] and [`Json::BigInt`]), `"number"`, `"string"`, `"array"`
    /// or `"object"`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Int(_) | Json::BigInt(_) => "integer",
            Json::Float(_) => "number",
            Json::Str(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }
}

/// Why a body is not JSON: `detail()` is the text after "could not parse the body as json: ".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    detail: String,
}

impl JsonError {
    /// The error text, e.g. `Expecting value: line 1 column 1 (char 0)`.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for JsonError {}

/// Parse a whole document. `max_depth` bounds nesting of arrays/objects (1 = a top-level
/// array/object may contain only scalars; a document of depth `max_depth` is accepted,
/// `max_depth + 1` refused; 0 refuses every array and object).
///
/// # Errors
///
/// A [`JsonError`] whose [`detail`](JsonError::detail) names the first problem and, except for
/// invalid UTF-8, its position (see the module documentation for the texts).
///
/// # Example
///
/// ```
/// use erp_fields::json::parse;
///
/// let value = parse(br#"{"ids": [1, 2]}"#, 64).unwrap();
/// assert_eq!(value.type_name(), "object");
/// let err = parse(b"{\"a\": }", 64).unwrap_err();
/// assert_eq!(err.detail(), "Expecting value: line 1 column 7 (char 6)");
/// ```
pub fn parse(bytes: &[u8], max_depth: usize) -> Result<Json, JsonError> {
    let text = std::str::from_utf8(bytes).map_err(|e| JsonError {
        detail: format!("invalid UTF-8 at byte {}", e.valid_up_to()),
    })?;
    let parser = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
        max_depth,
    };
    if text.starts_with('\u{FEFF}') {
        return Err(parser.error_at("Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
    }
    parser.document()
}

/// An array or object that has been opened and not yet closed.
enum Frame {
    /// The elements parsed so far.
    Array(Vec<Json>),
    /// The members parsed so far, their keys (for the duplicate check), and the key whose value
    /// is being parsed.
    Object {
        members: Vec<(Box<str>, Json)>,
        keys: HashSet<Box<str>>,
        key: Box<str>,
    },
}

impl Frame {
    /// The finished container.
    fn into_json(self) -> Json {
        match self {
            Frame::Array(items) => Json::Array(items),
            Frame::Object { members, .. } => Json::Object(members),
        }
    }
}

/// Cursor over a document already known to be valid UTF-8.
struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    /// Byte offset of the next unread byte.
    pos: usize,
    max_depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, at: usize) -> Option<u8> {
        self.bytes.get(at).copied()
    }

    fn advance(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.advance(1);
        }
    }

    fn rest_starts_with(&self, literal: &[u8]) -> bool {
        self.bytes
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(literal))
    }

    /// The text between two byte offsets that lie on ASCII bytes (always character boundaries).
    fn slice(&self, from: usize, to: usize) -> Result<&'a str, JsonError> {
        self.text.get(from..to).ok_or_else(|| JsonError {
            detail: format!("internal error: bytes {from}..{to} are not on character boundaries"),
        })
    }

    /// An error `message` at byte offset `at`, with Python's position suffix (code points).
    fn error_at(&self, message: &str, at: usize) -> JsonError {
        let prefix = &self.bytes[..at.min(self.bytes.len())];
        let char_pos = count_code_points(prefix);
        let line = prefix
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            .saturating_add(1);
        let column = match prefix.iter().rposition(|&b| b == b'\n') {
            // Python: pos - index of the last newline before pos.
            Some(newline) => count_code_points(&prefix[newline..]),
            None => char_pos.saturating_add(1),
        };
        JsonError {
            detail: format!("{message}: line {line} column {column} (char {char_pos})"),
        }
    }

    /// The whole document: one value surrounded by optional whitespace.
    fn document(mut self) -> Result<Json, JsonError> {
        self.skip_ws();
        let value = self.value()?;
        self.skip_ws();
        if self.pos < self.bytes.len() {
            return Err(self.error_at("Extra data", self.pos));
        }
        Ok(value)
    }

    /// One value starting at the cursor (whitespace already skipped), nested containers
    /// included, without recursion.
    fn value(&mut self) -> Result<Json, JsonError> {
        let mut stack: Vec<Frame> = Vec::new();
        'value: loop {
            // The cursor is where a value must start.
            let mut done = match self.peek() {
                Some(b'[') => {
                    self.enter(stack.len())?;
                    self.skip_ws();
                    if self.peek() == Some(b']') {
                        self.advance(1);
                        Json::Array(Vec::new())
                    } else {
                        stack.push(Frame::Array(Vec::new()));
                        continue 'value;
                    }
                }
                Some(b'{') => {
                    self.enter(stack.len())?;
                    self.skip_ws();
                    if self.peek() == Some(b'}') {
                        self.advance(1);
                        Json::Object(Vec::new())
                    } else {
                        let mut keys = HashSet::new();
                        let key = self.member_key(&mut keys)?;
                        stack.push(Frame::Object {
                            members: Vec::new(),
                            keys,
                            key,
                        });
                        continue 'value;
                    }
                }
                Some(b'"') => Json::Str(self.string()?),
                Some(b'n') if self.rest_starts_with(b"null") => {
                    self.advance(4);
                    Json::Null
                }
                Some(b't') if self.rest_starts_with(b"true") => {
                    self.advance(4);
                    Json::Bool(true)
                }
                Some(b'f') if self.rest_starts_with(b"false") => {
                    self.advance(5);
                    Json::Bool(false)
                }
                Some(b'-' | b'0'..=b'9') => self.number()?,
                // Includes `NaN`/`Infinity` (not JSON), a bad literal, and the end of input.
                _ => return Err(self.error_at("Expecting value", self.pos)),
            };
            // Hand the finished value to the enclosing containers, closing those that end here.
            loop {
                let Some(mut frame) = stack.pop() else {
                    return Ok(done);
                };
                match &mut frame {
                    Frame::Array(items) => {
                        items.push(done);
                        self.skip_ws();
                        match self.peek() {
                            Some(b',') => {
                                self.advance(1);
                                self.skip_ws();
                                stack.push(frame);
                                continue 'value;
                            }
                            Some(b']') => self.advance(1),
                            _ => return Err(self.error_at("Expecting ',' delimiter", self.pos)),
                        }
                    }
                    Frame::Object { members, keys, key } => {
                        members.push((std::mem::take(key), done));
                        self.skip_ws();
                        match self.peek() {
                            Some(b',') => {
                                self.advance(1);
                                self.skip_ws();
                                *key = self.member_key(keys)?;
                                stack.push(frame);
                                continue 'value;
                            }
                            Some(b'}') => self.advance(1),
                            _ => return Err(self.error_at("Expecting ',' delimiter", self.pos)),
                        }
                    }
                }
                done = frame.into_json();
            }
        }
    }

    /// Step over the `[`/`{` at the cursor, refusing it when `open` containers already enclose
    /// it and that is the limit.
    fn enter(&mut self, open: usize) -> Result<(), JsonError> {
        if open >= self.max_depth {
            let message = format!("nesting deeper than {} levels", self.max_depth);
            return Err(self.error_at(&message, self.pos));
        }
        self.advance(1);
        Ok(())
    }

    /// A member key, its `:` and the whitespace after it; refuses a key already in `keys`.
    fn member_key(&mut self, keys: &mut HashSet<Box<str>>) -> Result<Box<str>, JsonError> {
        if self.peek() != Some(b'"') {
            return Err(self.error_at(
                "Expecting property name enclosed in double quotes",
                self.pos,
            ));
        }
        let key_pos = self.pos;
        let key = self.string()?;
        if !keys.insert(key.clone()) {
            let message = format!("duplicate key '{}'", shown_key(&key));
            return Err(self.error_at(&message, key_pos));
        }
        self.skip_ws();
        if self.peek() != Some(b':') {
            return Err(self.error_at("Expecting ':' delimiter", self.pos));
        }
        self.advance(1);
        self.skip_ws();
        Ok(key)
    }

    /// A string literal; the cursor is on its opening quote.
    fn string(&mut self) -> Result<Box<str>, JsonError> {
        let start = self.pos;
        self.advance(1);
        let mut out = String::new();
        let mut chunk = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.error_at("Unterminated string starting at", start)),
                Some(b'"') => {
                    out.push_str(self.slice(chunk, self.pos)?);
                    self.advance(1);
                    return Ok(out.into_boxed_str());
                }
                Some(b'\\') => {
                    out.push_str(self.slice(chunk, self.pos)?);
                    self.escape(start, &mut out)?;
                    chunk = self.pos;
                }
                Some(0x00..=0x1F) => {
                    return Err(self.error_at("Invalid control character at", self.pos));
                }
                Some(_) => self.advance(1),
            }
        }
    }

    /// One escape; the cursor is on its backslash. `start` is the string's opening quote.
    fn escape(&mut self, start: usize, out: &mut String) -> Result<(), JsonError> {
        let backslash = self.pos;
        let decoded = match self.peek_at(backslash.saturating_add(1)) {
            None => return Err(self.error_at("Unterminated string starting at", start)),
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'/') => '/',
            Some(b'b') => '\u{8}',
            Some(b'f') => '\u{c}',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            Some(b'u') => return self.unicode_escape(out),
            Some(_) => return Err(self.error_at("Invalid \\escape", backslash)),
        };
        out.push(decoded);
        self.advance(2);
        Ok(())
    }

    /// A `\uXXXX` escape (or a surrogate pair of two); the cursor is on the backslash. Errors
    /// point at the `u`, as Python's do.
    fn unicode_escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        let u_pos = self.pos.saturating_add(1);
        let first = self.hex4(u_pos)?;
        self.pos = u_pos.saturating_add(5);
        let code = match first {
            0xD800..=0xDBFF => {
                if self.peek() != Some(b'\\')
                    || self.peek_at(self.pos.saturating_add(1)) != Some(b'u')
                {
                    return Err(self.error_at("Invalid \\uXXXX escape", u_pos));
                }
                let low_u_pos = self.pos.saturating_add(1);
                let second = self.hex4(low_u_pos)?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return Err(self.error_at("Invalid \\uXXXX escape", u_pos));
                }
                self.pos = low_u_pos.saturating_add(5);
                // Cannot overflow: first is in D800..=DBFF and second in DC00..=DFFF.
                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(self.error_at("Invalid \\uXXXX escape", u_pos)),
            other => other,
        };
        let decoded =
            char::from_u32(code).ok_or_else(|| self.error_at("Invalid \\uXXXX escape", u_pos))?;
        out.push(decoded);
        Ok(())
    }

    /// The four hex digits after the `u` at `u_pos`. Like Python, at least one character must
    /// follow them (a string cannot end there), else the escape is invalid.
    fn hex4(&self, u_pos: usize) -> Result<u32, JsonError> {
        let invalid = || self.error_at("Invalid \\uXXXX escape", u_pos);
        if u_pos.saturating_add(5) >= self.bytes.len() {
            return Err(invalid());
        }
        let mut code: u32 = 0;
        for offset in 1..5 {
            let digit = self
                .peek_at(u_pos.saturating_add(offset))
                .and_then(|b| char::from(b).to_digit(16))
                .ok_or_else(invalid)?;
            // Cannot overflow: at most four hex digits, so code stays below 0x10000.
            code = (code << 4) | digit;
        }
        Ok(code)
    }

    /// The end offset of the run of ASCII digits starting at `from`.
    fn digits_end(&self, from: usize) -> usize {
        let mut at = from;
        while matches!(self.peek_at(at), Some(b'0'..=b'9')) {
            at = at.saturating_add(1);
        }
        at
    }

    /// A number: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?`, taking the longest prefix of
    /// that grammar at the cursor (so `01` is `0` followed by extra data, and `1.` is `1`).
    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        let mut at = start;
        if self.peek_at(at) == Some(b'-') {
            at = at.saturating_add(1);
        }
        match self.peek_at(at) {
            Some(b'0') => at = at.saturating_add(1),
            Some(b'1'..=b'9') => at = self.digits_end(at),
            _ => return Err(self.error_at("Expecting value", start)),
        }
        let mut is_float = false;
        let after_dot = at.saturating_add(1);
        if self.peek_at(at) == Some(b'.') && matches!(self.peek_at(after_dot), Some(b'0'..=b'9')) {
            at = self.digits_end(after_dot);
            is_float = true;
        }
        if matches!(self.peek_at(at), Some(b'e' | b'E')) {
            let mut exponent = at.saturating_add(1);
            if matches!(self.peek_at(exponent), Some(b'+' | b'-')) {
                exponent = exponent.saturating_add(1);
            }
            // Without digits the `e` is not part of the number (Python backtracks the same way).
            if matches!(self.peek_at(exponent), Some(b'0'..=b'9')) {
                at = self.digits_end(exponent);
                is_float = true;
            }
        }
        let literal = self.slice(start, at)?;
        if literal.bytes().filter(u8::is_ascii_digit).count() > MAX_NUMBER_DIGITS {
            return Err(self.error_at("number too long", start));
        }
        self.pos = at;
        if is_float {
            return match literal.parse::<f64>() {
                Ok(value) if value.is_finite() => Ok(Json::Float(value)),
                Ok(_) => Err(self.error_at("number out of range", start)),
                Err(e) => Err(self.error_at(&format!("invalid number literal ({e})"), start)),
            };
        }
        match literal.parse::<i64>() {
            Ok(value) => Ok(Json::Int(value)),
            Err(e) => match e.kind() {
                IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => {
                    Ok(Json::BigInt(literal.into()))
                }
                // The grammar above admits only valid integer literals; anything else is a bug
                // and is reported, never turned into a value.
                _ => Err(self.error_at(&format!("invalid number literal ({e})"), start)),
            },
        }
    }
}

/// Appends `text` as a JSON string literal: `"` and `\\` escaped, `\\b \\t \\n \\f \\r` for those control
/// characters, `\\u00xx` (lowercase hex) for the other characters below U+0020, everything else as
/// literal UTF-8. These are the escapes of RFC 8785 §3.2.2.2 and of Python's `json.dumps` with
/// `ensure_ascii=False`, which is what the JSON-2 result writer and the row batch layouts
/// (`crate::rows`) produce.
///
/// Scans bytes and copies runs that need no escape in one go: every byte of a multi-byte UTF-8
/// character is at least 0x80, so only ASCII bytes are ever escaped. Allocates nothing beyond the
/// growth of `out`.
///
/// ```
/// let mut out = Vec::new();
/// erp_fields::json::write_string(&mut out, "é \"q\"\n\u{1}");
/// assert_eq!(String::from_utf8(out).unwrap(), r#""é \"q\"\n\u0001""#);
/// ```
pub fn write_string(out: &mut Vec<u8>, text: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = text.as_bytes();
    out.reserve(bytes.len().saturating_add(2));
    out.push(b'"');
    let mut unicode = *b"\\u0000";
    let mut run = 0;
    for (at, &byte) in bytes.iter().enumerate() {
        let escape: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0x00..=0x1f => {
                unicode[4] = HEX[usize::from(byte >> 4)];
                unicode[5] = HEX[usize::from(byte & 0xf)];
                &unicode
            }
            _ => continue,
        };
        out.extend_from_slice(&bytes[run..at]);
        out.extend_from_slice(escape);
        run = at + 1;
    }
    out.extend_from_slice(&bytes[run..]);
    out.push(b'"');
}

/// Code points in a slice of valid UTF-8 (bytes that are not continuation bytes).
fn count_code_points(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count()
}

/// A key as shown in the duplicate-key detail: control characters, `'` and `\` escaped
/// `escape_debug`-style, at most [`DUPLICATE_KEY_SHOWN_CHARS`] characters, then `…` if longer.
fn shown_key(key: &str) -> String {
    let mut shown = String::new();
    let mut chars = key.chars();
    for c in chars.by_ref().take(DUPLICATE_KEY_SHOWN_CHARS) {
        if c == '\'' || c == '\\' || c.is_control() {
            shown.extend(c.escape_debug());
        } else {
            shown.push(c);
        }
    }
    if chars.next().is_some() {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str) -> Json {
        match parse(input.as_bytes(), 64) {
            Ok(value) => value,
            Err(e) => panic!("{input:?} should parse, got {e}"),
        }
    }

    fn err(input: &str) -> String {
        err_depth(input, 64)
    }

    fn err_depth(input: &str, max_depth: usize) -> String {
        match parse(input.as_bytes(), max_depth) {
            Ok(value) => panic!("{input:?} should fail, got {value:?}"),
            Err(e) => e.detail().to_owned(),
        }
    }

    fn s(text: &str) -> Json {
        Json::Str(text.into())
    }

    #[test]
    fn parses_nested_structure() {
        let value = ok(r#"{"a": [1, 2.5, "x", true, false, null, {"b": {}}]}"#);
        let expected = Json::Object(vec![(
            "a".into(),
            Json::Array(vec![
                Json::Int(1),
                Json::Float(2.5),
                s("x"),
                Json::Bool(true),
                Json::Bool(false),
                Json::Null,
                Json::Object(vec![("b".into(), Json::Object(vec![]))]),
            ]),
        )]);
        assert_eq!(value, expected);
    }

    #[test]
    fn keeps_key_order_and_surrounding_whitespace() {
        let value = ok(" \t\r\n{\"z\":1,\"a\":2,\"m\":[]}\n ");
        let expected = Json::Object(vec![
            ("z".into(), Json::Int(1)),
            ("a".into(), Json::Int(2)),
            ("m".into(), Json::Array(vec![])),
        ]);
        assert_eq!(value, expected);
    }

    #[test]
    fn scalars_at_top_level() {
        assert_eq!(ok("null"), Json::Null);
        assert_eq!(ok("true"), Json::Bool(true));
        assert_eq!(ok("false"), Json::Bool(false));
        assert_eq!(ok("\"\""), s(""));
        assert_eq!(ok("[]"), Json::Array(vec![]));
        assert_eq!(ok("{}"), Json::Object(vec![]));
    }

    #[test]
    fn decodes_escapes() {
        assert_eq!(
            ok(r#""\"\\\/\b\f\n\r\t\u0041\u00C9\u00e9""#),
            s("\"\\/\u{8}\u{c}\n\r\tAÉé")
        );
        assert_eq!(ok("\"\\u00e9\""), s("é"));
        assert_eq!(ok("\"é\""), s("é"));
        assert_eq!(ok("\"\\ud83d\\ude00\""), s("\u{1F600}"));
        assert_eq!(ok("\"a\\uD83D\\uDE00b\""), s("a\u{1F600}b"));
        // DEL and other non-control characters are accepted raw, as in Python.
        assert_eq!(ok("\"\u{7f}\u{80}\u{2028}\""), s("\u{7f}\u{80}\u{2028}"));
    }

    #[test]
    fn numbers() {
        assert_eq!(ok("0"), Json::Int(0));
        assert_eq!(ok("-0"), Json::Int(0));
        match ok("-0.0") {
            Json::Float(f) => assert!(f == 0.0 && f.is_sign_negative(), "got {f}"),
            other => panic!("expected a float, got {other:?}"),
        }
        assert_eq!(ok("1E2"), Json::Float(100.0));
        assert_eq!(ok("1e-2"), Json::Float(0.01));
        assert_eq!(ok("1.5e+3"), Json::Float(1500.0));
        assert_eq!(ok("-12.25"), Json::Float(-12.25));
        assert_eq!(ok("1e-400"), Json::Float(0.0));
        assert_eq!(ok("9223372036854775807"), Json::Int(i64::MAX));
        assert_eq!(ok("-9223372036854775808"), Json::Int(i64::MIN));
        assert_eq!(
            ok("9223372036854775808"),
            Json::BigInt("9223372036854775808".into())
        );
        assert_eq!(
            ok("-9223372036854775809"),
            Json::BigInt("-9223372036854775809".into())
        );
        let longest = "7".repeat(MAX_NUMBER_DIGITS);
        assert_eq!(ok(&longest), Json::BigInt(longest.clone().into()));
    }

    #[test]
    fn number_errors() {
        let too_long = "1".repeat(MAX_NUMBER_DIGITS + 1);
        assert_eq!(err(&too_long), "number too long: line 1 column 1 (char 0)");
        assert_eq!(
            err(&format!("[1, {too_long}]")),
            "number too long: line 1 column 5 (char 4)"
        );
        let long_float = format!("0.{}", "1".repeat(MAX_NUMBER_DIGITS));
        assert_eq!(
            err(&long_float),
            "number too long: line 1 column 1 (char 0)"
        );
        assert_eq!(
            err("1e400"),
            "number out of range: line 1 column 1 (char 0)"
        );
        assert_eq!(
            err("[0, -1.5e309]"),
            "number out of range: line 1 column 5 (char 4)"
        );
    }

    #[test]
    fn expecting_value() {
        assert_eq!(err(""), "Expecting value: line 1 column 1 (char 0)");
        assert_eq!(err(" "), "Expecting value: line 1 column 2 (char 1)");
        assert_eq!(err("\n\n"), "Expecting value: line 3 column 1 (char 2)");
        assert_eq!(err("not json"), "Expecting value: line 1 column 1 (char 0)");
        assert_eq!(
            err(r#"{"a": }"#),
            "Expecting value: line 1 column 7 (char 6)"
        );
        assert_eq!(err(r#"{"a":"#), "Expecting value: line 1 column 6 (char 5)");
        assert_eq!(err("[1,]"), "Expecting value: line 1 column 4 (char 3)");
        assert_eq!(err("[1,"), "Expecting value: line 1 column 4 (char 3)");
        assert_eq!(err("["), "Expecting value: line 1 column 2 (char 1)");
        assert_eq!(err("[ "), "Expecting value: line 1 column 3 (char 2)");
        for token in [
            "NaN",
            "Infinity",
            "-Infinity",
            "-",
            "+1",
            ".5",
            "tru",
            "nul",
            "fals",
        ] {
            assert_eq!(
                err(token),
                "Expecting value: line 1 column 1 (char 0)",
                "{token}"
            );
        }
        assert_eq!(err("[1, NaN]"), "Expecting value: line 1 column 5 (char 4)");
    }

    #[test]
    fn extra_data() {
        assert_eq!(err("01"), "Extra data: line 1 column 2 (char 1)");
        assert_eq!(err("-01"), "Extra data: line 1 column 3 (char 2)");
        assert_eq!(err("1."), "Extra data: line 1 column 2 (char 1)");
        assert_eq!(err("1.e5"), "Extra data: line 1 column 2 (char 1)");
        assert_eq!(err("1e"), "Extra data: line 1 column 2 (char 1)");
        assert_eq!(err("1e+"), "Extra data: line 1 column 2 (char 1)");
        assert_eq!(err("{} {}"), "Extra data: line 1 column 4 (char 3)");
        assert_eq!(err("nullx"), "Extra data: line 1 column 5 (char 4)");
        assert_eq!(err("[]\n ]"), "Extra data: line 2 column 2 (char 4)");
    }

    #[test]
    fn delimiters_and_property_names() {
        assert_eq!(
            err("[1"),
            "Expecting ',' delimiter: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err("[1 2]"),
            "Expecting ',' delimiter: line 1 column 4 (char 3)"
        );
        assert_eq!(
            err("[1.]"),
            "Expecting ',' delimiter: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err("{"),
            "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"
        );
        assert_eq!(
            err(r#"{"a":1,}"#),
            "Expecting property name enclosed in double quotes: line 1 column 8 (char 7)"
        );
        assert_eq!(
            err("{1:2}"),
            "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"
        );
        assert_eq!(
            err("{'a':1}"),
            "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"
        );
        assert_eq!(
            err(r#"{"a" 1}"#),
            "Expecting ':' delimiter: line 1 column 6 (char 5)"
        );
        assert_eq!(
            err(r#"{"a""#),
            "Expecting ':' delimiter: line 1 column 5 (char 4)"
        );
        assert_eq!(
            err(r#"{"a":1 2}"#),
            "Expecting ',' delimiter: line 1 column 8 (char 7)"
        );
        assert_eq!(
            err(r#"{"a":1"#),
            "Expecting ',' delimiter: line 1 column 7 (char 6)"
        );
    }

    #[test]
    fn string_errors() {
        assert_eq!(err(r#""\x""#), r"Invalid \escape: line 1 column 2 (char 1)");
        assert_eq!(
            err(r#""\u12""#),
            r"Invalid \uXXXX escape: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err(r#""\u12G4""#),
            r"Invalid \uXXXX escape: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err(r#""\u12"x"#),
            r"Invalid \uXXXX escape: line 1 column 3 (char 2)"
        );
        // Python: the four digits may not end the document (no room for the closing quote).
        assert_eq!(
            err(r#""\u0041"#),
            r"Invalid \uXXXX escape: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err(r#""\ud83d\u12""#),
            r"Invalid \uXXXX escape: line 1 column 9 (char 8)"
        );
        assert_eq!(
            err(r#""ab\ud83d\u12G4""#),
            r"Invalid \uXXXX escape: line 1 column 11 (char 10)"
        );
        assert_eq!(
            err("\"a\u{1}\""),
            "Invalid control character at: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err("\"a\nb\""),
            "Invalid control character at: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err("\"abc"),
            "Unterminated string starting at: line 1 column 1 (char 0)"
        );
        assert_eq!(
            err("\"\\"),
            "Unterminated string starting at: line 1 column 1 (char 0)"
        );
        assert_eq!(
            err("[1, \"\\u00e9\\"),
            "Unterminated string starting at: line 1 column 5 (char 4)"
        );
    }

    /// Divergence from Python, which keeps lone surrogates: a Rust string cannot hold them.
    #[test]
    fn lone_surrogates_are_refused() {
        assert_eq!(
            err(r#""\ud83d""#),
            r"Invalid \uXXXX escape: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err(r#""\ude00""#),
            r"Invalid \uXXXX escape: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err(r#""\ud83d\u0041""#),
            r"Invalid \uXXXX escape: line 1 column 3 (char 2)"
        );
        assert_eq!(
            err(r#""x\ud83dy""#),
            r"Invalid \uXXXX escape: line 1 column 4 (char 3)"
        );
    }

    #[test]
    fn positions_count_code_points_and_lines() {
        assert_eq!(
            err("{\r\n\"a\"\r\n: x}"),
            "Expecting value: line 3 column 3 (char 10)"
        );
        assert_eq!(
            err("[\"é\", x]"),
            "Expecting value: line 1 column 7 (char 6)"
        );
        assert_eq!(
            err("[\n\"é\", x]"),
            "Expecting value: line 2 column 6 (char 7)"
        );
        assert_eq!(
            err("[\"\u{1F600}\", x]"),
            "Expecting value: line 1 column 7 (char 6)"
        );
    }

    #[test]
    fn invalid_utf8_and_bom() {
        assert_eq!(
            parse(b"[\"\xff\"]", 64).map_err(|e| e.detail().to_owned()),
            Err("invalid UTF-8 at byte 2".to_owned())
        );
        assert_eq!(
            parse(b"\"\xc3", 64).map_err(|e| e.detail().to_owned()),
            Err("invalid UTF-8 at byte 1".to_owned())
        );
        assert_eq!(
            err("\u{FEFF}1"),
            "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)"
        );
        assert_eq!(
            err("\u{FEFF}"),
            "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)"
        );
        // A BOM anywhere else is just an unexpected character.
        assert_eq!(
            err(" \u{FEFF}1"),
            "Expecting value: line 1 column 2 (char 1)"
        );
    }

    #[test]
    fn duplicate_keys_are_refused() {
        assert_eq!(
            err(r#"{"a":1,"a":2}"#),
            "duplicate key 'a': line 1 column 8 (char 7)"
        );
        assert_eq!(
            err(r#"{"x": {"k": 1, "k": 2}}"#),
            "duplicate key 'k': line 1 column 16 (char 15)"
        );
        // Equality is after unescaping.
        assert_eq!(
            err(r#"{"a":1,"\u0061":2}"#),
            "duplicate key 'a': line 1 column 8 (char 7)"
        );
        assert_eq!(
            err(r#"{"q'\\\n":1,"q'\\\n":2}"#),
            r"duplicate key 'q\'\\\n': line 1 column 13 (char 12)"
        );
        let long = "k".repeat(70);
        assert_eq!(
            err(&format!(r#"{{"{long}":1,"{long}":2}}"#)),
            format!(
                "duplicate key '{}…': line 1 column 77 (char 76)",
                "k".repeat(64)
            )
        );
        // The same key in different objects is fine.
        assert_eq!(
            ok(r#"[{"a":1},{"a":2}]"#),
            Json::Array(vec![
                Json::Object(vec![("a".into(), Json::Int(1))]),
                Json::Object(vec![("a".into(), Json::Int(2))]),
            ])
        );
    }

    fn nested_arrays(depth: usize) -> String {
        format!("{}{}", "[".repeat(depth), "]".repeat(depth))
    }

    #[test]
    fn depth_limit() {
        for limit in [64, 1024] {
            let value = parse(nested_arrays(limit).as_bytes(), limit);
            assert!(value.is_ok(), "depth {limit} should parse: {value:?}");
            assert_eq!(
                err_depth(&nested_arrays(limit + 1), limit),
                format!(
                    "nesting deeper than {limit} levels: line 1 column {} (char {limit})",
                    limit + 1
                )
            );
        }
        assert_eq!(
            parse(b"[1, {\"a\": 2}]", 2).map(|v| v.type_name()),
            Ok("array")
        );
        assert_eq!(
            err_depth("[1, {\"a\": [2]}]", 2),
            "nesting deeper than 2 levels: line 1 column 11 (char 10)"
        );
        assert_eq!(parse(b"[1]", 1).map(|v| v.type_name()), Ok("array"));
        assert_eq!(
            err_depth("[[]]", 1),
            "nesting deeper than 1 levels: line 1 column 2 (char 1)"
        );
        assert_eq!(parse(b"7", 0), Ok(Json::Int(7)));
        assert_eq!(
            err_depth("{}", 0),
            "nesting deeper than 0 levels: line 1 column 1 (char 0)"
        );
        let objects = format!("{}1{}", "{\"a\":".repeat(65), "}".repeat(65));
        assert_eq!(
            err_depth(&objects, 64),
            "nesting deeper than 64 levels: line 1 column 321 (char 320)"
        );
        let objects = format!("{}1{}", "{\"a\":".repeat(64), "}".repeat(64));
        assert_eq!(
            parse(objects.as_bytes(), 64).map(|v| v.type_name()),
            Ok("object")
        );
    }

    #[test]
    fn large_array() {
        let body = format!(
            "[{}]",
            (0..100_000)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        match ok(&body) {
            Json::Array(items) => {
                assert_eq!(items.len(), 100_000);
                assert_eq!(items.first(), Some(&Json::Int(0)));
                assert_eq!(items.last(), Some(&Json::Int(99_999)));
            }
            other => panic!("expected an array, got {}", other.type_name()),
        }
    }

    #[test]
    fn type_names() {
        let names: Vec<&str> = [
            Json::Null,
            Json::Bool(true),
            Json::Int(1),
            Json::BigInt("99999999999999999999".into()),
            Json::Float(1.5),
            s("x"),
            Json::Array(vec![]),
            Json::Object(vec![]),
        ]
        .iter()
        .map(Json::type_name)
        .collect();
        assert_eq!(
            names,
            [
                "null", "boolean", "integer", "integer", "number", "string", "array", "object"
            ]
        );
    }

    #[test]
    fn display_is_the_detail() {
        let e = match parse(b"[", 64) {
            Ok(v) => panic!("should fail, got {v:?}"),
            Err(e) => e,
        };
        assert_eq!(e.to_string(), "Expecting value: line 1 column 2 (char 1)");
        let boxed: Box<dyn std::error::Error> = Box::new(e);
        assert_eq!(
            boxed.to_string(),
            "Expecting value: line 1 column 2 (char 1)"
        );
    }
}
