//! Conservative, allocation-free admission of source parsing and wire-AST admission storage.
//!
//! Each family below names actual allocations in parser.rs, lexer.rs and wire/convert.rs. The
//! result is a reservation ceiling, not an estimate of allocator usage. In particular a formatted
//! string with replacement fields uses its raw body bytes as the token bound: decoding escapes
//! can introduce braces and nested expressions, so inspecting raw braces is not authorization to
//! skip them. This deliberately over-reserves some large f-strings.
use super::{P, Parser, StrPart};
use crate::ast::{CompareOp, Comprehension, Expr};
use crate::error::{ExprError, ExprResult};
use crate::limits::EvalLimits;
use crate::wire::{WireCompareOp, WireComprehension, WireKeyword, WireNode};
use std::mem::size_of;

/// A conservative source-derived reservation for parsing and admitting one expression.
///
/// The fields are private and the only producer is [`source_preflight`]. Bounds apply to the same
/// source and parsing limits, including the original parsed [`Expr`] remaining alive during
/// [`crate::wire::ValidatedAst::from_expr`]. Keep the reservation until both ASTs are dropped.
/// This is not a performance measurement, a syntax validator, or an authority-bearing handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceParsePreflight {
    reserve_bytes: usize,
    token_bound: usize,
    depth_bound: usize,
}
impl SourceParsePreflight {
    /// Heap bytes to reserve before parsing, including wire-admission scratch and retained ASTs.
    ///
    /// This is conservative: a request can refuse this reservation even when an unbudgeted parse
    /// would use less memory. Formatted strings with fields or escapes have the broadest bound.
    #[must_use]
    pub fn reserve_bytes(&self) -> usize {
        self.reserve_bytes
    }
    /// Upper bound on tokens and successful output nodes, including one end-of-input position.
    /// This is not an exact lexical token count: punctuation and f-string contents can overcount.
    #[must_use]
    pub fn token_bound(&self) -> usize {
        self.token_bound
    }
    /// The configured parser/tree depth ceiling used by this reservation, not observed depth.
    #[must_use]
    pub fn depth_bound(&self) -> usize {
        self.depth_bound
    }
}

fn overflow() -> ExprError {
    ExprError::limit("source parse admission bound arithmetic overflow")
}
fn add(left: usize, right: usize) -> ExprResult<usize> {
    left.checked_add(right).ok_or_else(overflow)
}
fn mul(left: usize, right: usize) -> ExprResult<usize> {
    left.checked_mul(right).ok_or_else(overflow)
}

/// Capacity and one simultaneously live old buffer for Vec families filled by push/collect.
///
/// A non-byte Vec starts at at most four elements and doubles: retained capacity is at most
/// four times its nonzero length, and an old buffer during growth is at most twice that length.
/// Summing the two terms over independent containers avoids pretending all entries share one
/// Vec. With-capacity and vec![first] producers are no larger. Byte/string growth is separate.
fn vectors(entries: usize, slot: usize) -> ExprResult<usize> {
    let retained = mul(mul(entries, 4)?, slot)?;
    let old_during_growth = mul(mul(entries, 2)?, slot)?;
    add(retained, old_during_growth)
}

/// String/byte payload capacity plus an old buffer: at most twice logical bytes retained plus
/// once logical bytes during growth; the eight-byte initial allocation and its four-byte old
/// buffer are charged for every possible text object (including short numeric formatting).
fn text(bytes: usize, objects: usize) -> ExprResult<usize> {
    add(mul(bytes, 3)?, mul(objects, 8 + 4)?)
}

#[derive(Default)]
struct Profile {
    tokens: usize,
    strings: usize,
    fstring_nodes: usize,
    dynamic_fstring_bytes: usize,
    dynamic_fstring: bool,
}
impl Profile {
    fn token(&mut self) -> ExprResult<()> {
        self.tokens = add(self.tokens, 1)?;
        Ok(())
    }
    fn string(&mut self, body: &[u8], formatted: bool, raw: bool) -> ExprResult<()> {
        self.token()?;
        self.strings = add(self.strings, 1)?;
        if formatted {
            // A decoded escape can introduce a replacement field even without a raw '{'.
            let dynamic = body.contains(&b'{') || (!raw && body.contains(&b'\\'));
            let nodes = if dynamic {
                self.dynamic_fstring = true;
                self.dynamic_fstring_bytes = add(self.dynamic_fstring_bytes, body.len())?;
                body.len()
            } else {
                usize::from(!body.is_empty())
            };
            self.fstring_nodes = add(self.fstring_nodes, nodes)?;
            self.tokens = add(self.tokens, nodes)?;
        }
        Ok(())
    }
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}
fn prefix(value: &str) -> bool {
    ["r", "u", "b", "f", "rb", "br", "rf", "fr"]
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

/// Locate a lexical quoted body without decoding or allocating it. Skipping a backslash and the
/// following byte preserves both ordinary and raw escaped-quote termination. A remaining UTF-8
/// continuation byte cannot be an ASCII quote/backslash. Unterminated literals keep the rest as
/// their payload bound; the real parser remains responsible for their syntax diagnostic.
fn quoted<'s>(input: &'s [u8], position: &mut usize) -> &'s [u8] {
    let quote = input[*position];
    let triple =
        input.get(*position..position.saturating_add(3)) == Some([quote, quote, quote].as_slice());
    *position += if triple { 3 } else { 1 };
    let begin = *position;
    while *position < input.len() {
        if input[*position] == b'\\' {
            *position += 1;
            if *position < input.len() {
                *position += 1;
            }
            continue;
        }
        if input[*position] == quote
            && (!triple
                || input.get(*position..position.saturating_add(3))
                    == Some([quote, quote, quote].as_slice()))
        {
            let end = *position;
            *position += if triple { 3 } else { 1 };
            return &input[begin..end];
        }
        *position += 1;
    }
    &input[begin..]
}

/// Upper-count root tokens without building any lexer payload. Ordinary literal bodies are
/// skipped. Digit runs deliberately do not consume exponent/dot/radix suffixes, and punctuation
/// is counted one byte at a time, so multi-character numeric/operator tokens can only overcount.
fn profile(input: &str) -> ExprResult<Profile> {
    let bytes = input.as_bytes();
    let mut at = 0;
    let mut result = Profile::default();
    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
        } else if byte == b'#' {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
        } else if byte == b'\\' && bytes.get(at + 1) == Some(&b'\n') {
            at += 2;
        } else if byte == b'\\'
            && bytes.get(at + 1) == Some(&b'\r')
            && bytes.get(at + 2) == Some(&b'\n')
        {
            at += 3;
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let begin = at;
            at += 1;
            while at < bytes.len() && word(bytes[at]) {
                at += 1;
            }
            let name = &input[begin..at];
            if matches!(bytes.get(at), Some(b'\'' | b'"')) && prefix(name) {
                let formatted = name
                    .as_bytes()
                    .iter()
                    .any(|b| b.eq_ignore_ascii_case(&b'f'));
                let raw = name
                    .as_bytes()
                    .iter()
                    .any(|b| b.eq_ignore_ascii_case(&b'r'));
                result.string(quoted(bytes, &mut at), formatted, raw)?;
            } else {
                result.token()?;
            }
        } else if byte.is_ascii_digit() {
            at += 1;
            while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'_') {
                at += 1;
            }
            result.token()?;
        } else if matches!(byte, b'\'' | b'"') {
            result.string(quoted(bytes, &mut at), false, false)?;
        } else {
            at += 1;
            result.token()?;
        }
    }
    Ok(result)
}

/// Bound parsing and subsequent wire-AST admission before either builds an owned AST.
///
/// The successful preflight itself allocates nothing. It leaves syntax and expression-language
/// validation to [`super::parse_with_limits`], which must receive the same source and limits.
/// Reserve [`SourceParsePreflight::reserve_bytes`] from the caller's shared allowance first.
/// Keep that reservation until the original parsed AST and admitted AST are both dropped.
///
/// The bound sums named Vec, Box, text, wire-arena and reconstruction families read from the
/// producer source. It is intentionally conservative, especially for decoded/nested f-strings;
/// it can over-refuse a caller's memory allowance and is not an exact memory measurement.
/// Existing parsing semantics and intrinsic limits are unchanged when the reservation succeeds.
///
/// # Errors
/// `LimitExceeded` if the source exceeds its configured limit, the depth ceiling is zero, or
/// checked bound arithmetic overflows. No source text appears in those diagnostics.
///
/// ```
/// use erp_expr::{EvalLimits, parse_with_limits, source_preflight, wire::ValidatedAst};
/// let source = "[('name', '=', 'Example')]";
/// let limits = EvalLimits::default();
/// let bound = source_preflight(source, limits)?;
/// // A host reserves bound.reserve_bytes() from its shared allowance here.
/// let parsed = parse_with_limits(source, limits)?;
/// let admitted = ValidatedAst::from_expr(&parsed)?;
/// assert!(bound.token_bound() >= 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn source_preflight(input: &str, limits: EvalLimits) -> ExprResult<SourceParsePreflight> {
    if input.len() > limits.max_source_bytes() {
        return Err(ExprError::limit(format!(
            "expression source of {} bytes exceeds the limit of {} bytes",
            input.len(),
            limits.max_source_bytes()
        )));
    }
    if limits.max_depth() == 0 {
        return Err(ExprError::limit(
            "expression nesting depth limit of 0 exceeded",
        ));
    }
    let shape = profile(input)?;
    let tokens = add(shape.tokens, 1)?; // EOF also covers the empty/error input.
    let string_parts = add(shape.strings, shape.dynamic_fstring_bytes)?;
    let nested = if shape.dynamic_fstring {
        limits.max_depth()
    } else {
        0
    };
    let text_levels = add(nested, 1)?;

    // Primary Expr collections partition child values. Dict pairs own two Expr slots and
    // keywords have both a name token and a value; comprehension clauses have a `for` token.
    // These source positions pay for their extra structural slot instead of charging the
    // complete token bound separately for every mutually exclusive collection variant.
    let primary = size_of::<Expr>()
        .max(size_of::<(Expr, Expr)>().div_ceil(2))
        .max(size_of::<(Option<String>, Expr)>().div_ceil(2))
        .max(size_of::<Comprehension>())
        .max(size_of::<CompareOp>());
    let mut bytes = vectors(tokens, primary)?;
    bytes = add(bytes, vectors(tokens, size_of::<usize>())?)?; // temporary heights
    bytes = add(bytes, vectors(string_parts, size_of::<StrPart>())?)?;
    bytes = add(bytes, vectors(shape.fstring_nodes, size_of::<P>())?)?;
    bytes = add(bytes, mul(tokens, size_of::<Expr>())?)?; // parser child boxes
    bytes = add(bytes, mul(add(nested, 1)?, size_of::<Parser<'_>>())?)?;

    // Lexer values, adjacent-string concatenation and f-string literal assembly each have at
    // most one source-sized payload per nested parser. Diagnostics can additionally copy input
    // names while wrapping a lexical/syntax error. The entire producer source texts upper-bound
    // all fixed diagnostic literal text; variable source data is charged separately below.
    let one_text_family = mul(input.len(), text_levels)?;
    let parser_text = add(add(one_text_family, one_text_family)?, one_text_family)?;
    let diagnostics = add(add(input.len(), input.len())?, input.len())?;
    let parser_objects = add(mul(mul(tokens, text_levels)?, 3)?, 3)?;
    bytes = add(bytes, text(add(parser_text, diagnostics)?, parser_objects)?)?;
    const DIAGNOSTIC_TEXT: usize = include_str!("parser.rs").len()
        + include_str!("lexer.rs").len()
        + include_str!("error.rs").len()
        + include_str!("wire/mod.rs").len()
        + include_str!("wire/convert.rs").len()
        + include_str!("wire/validate.rs").len();
    bytes = add(bytes, DIAGNOSTIC_TEXT)?;

    // The temporary postorder wire arena starts at one slot and doubles. Edge vectors plus
    // auxiliary keyword/comprehension/operator entries are two disjoint token-paid families.
    let arena = tokens.checked_next_power_of_two().ok_or_else(overflow)?;
    bytes = add(bytes, mul(mul(arena, 2)?, size_of::<WireNode>())?)?;
    let wire_slot = size_of::<WireKeyword>()
        .max(size_of::<WireComprehension>())
        .max(size_of::<(u32, u32)>())
        .max(size_of::<WireCompareOp>());
    bytes = add(bytes, vectors(mul(tokens, 2)?, wire_slot)?)?;
    // Int decimal and float-bit formatting: at most 20/16 characters, String capacity at most
    // 32 and an old buffer at most 16; reserving two 32-byte buffers also covers conversion chars.
    bytes = add(bytes, mul(tokens, 32 + 32)?)?;
    bytes = add(bytes, text(one_text_family, tokens)?)?; // owned text/byte clones
    bytes = add(bytes, mul(tokens, size_of::<(bool, u8)>())?)?; // validation scratch

    // Reconstructing the admitted Expr retains the original tree and wire arena above. The
    // decoder's built slots, child boxes and primary vectors are separate simultaneous storage.
    bytes = add(bytes, mul(tokens, size_of::<Option<Expr>>())?)?;
    bytes = add(bytes, mul(tokens, size_of::<Expr>())?)?;
    bytes = add(bytes, vectors(tokens, primary)?)?;
    Ok(SourceParsePreflight {
        reserve_bytes: bytes,
        token_bound: tokens,
        depth_bound: limits.max_depth(),
    })
}
