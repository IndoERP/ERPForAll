//! String literals in generated modules (design note §7.1, §8 item 1): every string the code
//! generator writes is a double-quoted literal escaped like JSON, with `<`, `>`, `&`, `/`, U+2028
//! and U+2029 escaped too, so no string content can end the literal, start a comment, close a
//! `<script>` element or break a line in any JavaScript version.

use std::fmt::Write as _;

/// Appends `s` as a double-quoted JavaScript string literal.
pub(crate) fn push_js_string(out: &mut String, s: &str) {
    out.push('"');
    push_escaped(out, s, Quote::Double);
    out.push('"');
}

/// The quote context of [`push_escaped`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quote {
    /// Inside `"…"`.
    Double,
    /// Inside a template literal: `` ` `` and `${` are escaped too.
    Backtick,
}

/// Appends the escaped characters of `s`, without quotes.
pub(crate) fn push_escaped(out: &mut String, s: &str, quote: Quote) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quote == Quote::Double => out.push_str("\\\""),
            '`' if quote == Quote::Backtick => out.push_str("\\`"),
            '$' if quote == Quote::Backtick && chars.peek() == Some(&'{') => out.push_str("\\$"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '/' => out.push_str("\\/"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                // Writing to a String cannot fail.
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> String {
        let mut out = String::new();
        push_js_string(&mut out, s);
        out
    }

    #[test]
    fn breakouts_are_escaped() {
        assert_eq!(lit("a\"b"), "\"a\\\"b\"");
        assert_eq!(lit("</script>"), "\"\\u003c\\/script\\u003e\"");
        assert_eq!(lit("*/ //"), "\"*\\/ \\/\\/\"");
        assert_eq!(
            lit("\u{2028}\u{2029}\u{0}\u{7f}"),
            "\"\\u2028\\u2029\\u0000\\u007f\""
        );
        assert_eq!(lit("a\\nb"), "\"a\\\\nb\"");
        assert_eq!(lit("é ${x} `"), "\"é ${x} `\"");
        let mut out = String::new();
        push_escaped(&mut out, "`${x}` $", Quote::Backtick);
        assert_eq!(out, "\\`\\${x}\\` $");
    }
}
