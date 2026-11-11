//! The inline-style scrub and the property whitelist (`style_whitelist`, spec 4.7).
//!
//! [`scrub_style`] reads a `style` attribute with a small hand-written scanner: one forward pass
//! over the bytes, no regular expressions and no backtracking, a flat multiple of the value length
//! charged to the work budget as a proxy for the bytes scanned. It splits the value into
//! declarations at `;` outside quotes, parentheses and comments
//! (a backslash escapes the next byte), and then:
//!
//! 1. Drops every declaration that holds dangerous content, in every mode. The check runs on a
//!    normalised copy of the declaration (comments removed, CSS escapes such as `\6a` decoded,
//!    whitespace and control characters removed, ASCII case folded), so `ja/**/va\73cript:` and
//!    `EXPR\65SSION(` are seen as what a browser would read. Dangerous content is `expression(`,
//!    `javascript:`, `vbscript:`, `@import`, `behavior:`, `-moz-binding`, `image-set(`, and a
//!    `url(`, `src(`, `image(` or `-moz-element(` whose argument is not an allowed URL ([`crate::url::is_allowed_url`]).
//!    An argument that holds a `(` (a nested function such as `var(`, `env(`, `attr(` or `if(`,
//!    which a browser resolves before it reads the URL) or an odd number of quote characters (either kind, escapes decoded) drops the declaration;
//!    that rule has no deny-list of function names. For the last three, a comment marker or a second quote in the argument also drops the
//!    declaration, because the two readings of the text may then disagree about where the URL is.
//!    The whole declaration goes (the reference leaves fragments such as `width: );` behind; that
//!    divergence is decision D-3).
//! 2. With the whitelist on, keeps only declarations whose property is in [`STYLE_WHITELIST`],
//!    writes them as `name:value` joined by `; ` with the name lower-cased and comments replaced
//!    by a space, collapses duplicates to the last value at the first position, and drops the
//!    attribute when nothing is left.
//! 3. With the whitelist off, leaves the value alone when nothing was dropped, otherwise joins the
//!    surviving declarations with `; `. A value with no declaration at all drops the attribute.
//!
//! Source: spec 4.7, not captured: the list is read from the prose of
//! because the golden file does not record it
//! (plan decision D-5; recapture it into the golden JSON later). A property outside the list is
//! dropped by the style scrub. Bare `border-left`, `border-right` and `max-height` are not in it;
//! the 20 generated `border-{top,bottom,left,right}-{style,color,width,left-radius,right-radius}`
//! names are, as the spec states.

use std::borrow::Cow;

use crate::budget::HtmlBudget;
use html5ever::budget::Budget;

use crate::HtmlSanitizeError;
use crate::clean::Reporter;
use crate::report::{RemovalKind, RemovalReason};

/// Allowed style properties: sorted, unique, lower-case ASCII.
#[rustfmt::skip]
pub(crate) static STYLE_WHITELIST: &[&str] = &[
    "background-color",
    "background-image",
    "background-origin",
    "background-position",
    "background-repeat",
    "background-size",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
    "border-bottom-style",
    "border-bottom-width",
    "border-collapse",
    "border-color",
    "border-left-color",
    "border-left-left-radius",
    "border-left-right-radius",
    "border-left-style",
    "border-left-width",
    "border-radius",
    "border-right-color",
    "border-right-left-radius",
    "border-right-right-radius",
    "border-right-style",
    "border-right-width",
    "border-spacing",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-top-style",
    "border-top-width",
    "border-width",
    "caption-side",
    "color",
    "direction",
    "display",
    "empty-cells",
    "float",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "height",
    "letter-spacing",
    "line-height",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-width",
    "min-height",
    "min-width",
    "object-fit",
    "opacity",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "table-layout",
    "text-align",
    "text-decoration",
    "text-transform",
    "vertical-align",
    "white-space",
    "width",
];

/// Position of the style property `name` in [`STYLE_WHITELIST`]. The comparison ignores ASCII
/// case, as the spec lower-cases property names before the lookup.
#[must_use]
fn property_index(name: &str) -> Option<usize> {
    STYLE_WHITELIST
        .binary_search_by(|entry| {
            entry
                .bytes()
                .cmp(name.bytes().map(|b| b.to_ascii_lowercase()))
        })
        .ok()
}

/// Whether the style property `name` is whitelisted.
#[cfg(test)]
#[must_use]
pub(crate) fn is_allowed_property(name: &str) -> bool {
    property_index(name).is_some()
}

/// Splits a style value into declarations (slices of the value, possibly empty or blank).
struct Declarations<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Iterator for Declarations<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.pos > self.src.len() {
            return None;
        }
        let bytes = self.src.as_bytes();
        let start = self.pos;
        let mut i = start;
        let mut quote: Option<u8> = None;
        let mut depth = 0u32;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\\' {
                i += 2;
                continue;
            }
            if let Some(q) = quote {
                if b == q || b == b'\n' {
                    quote = None;
                }
            } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                match self.src[i + 2..].find("*/") {
                    Some(end) => i += 2 + end + 2,
                    None => i = bytes.len(),
                }
                continue;
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b == b'(' {
                depth = depth.saturating_add(1);
            } else if b == b')' {
                depth = depth.saturating_sub(1);
            } else if b == b';' && depth == 0 {
                self.pos = i + 1;
                return Some(&self.src[start..i]);
            }
            i += 1;
        }
        self.pos = bytes.len() + 1;
        Some(&self.src[start..])
    }
}

/// Write the normalised form of `decl` into `buf` (cleared first): comments removed (an
/// unterminated comment is kept as text, so what it hides is still checked), CSS escapes decoded,
/// whitespace and control characters dropped, ASCII lower-cased. A decoded character is never
/// longer than the bytes it came from, so `buf` does not outgrow the capacity charged for it.
fn normalise(decl: &str, buf: &mut String, keep_comments: bool) {
    buf.clear();
    let mut rest = decl;
    // Once a search for a closing marker fails, none exists in the rest of the declaration, so
    // later openers are text without another search: a run of `/*` costs one scan, not one each.
    let mut no_closer = false;
    while !rest.is_empty() {
        if !keep_comments
            && !no_closer
            && let Some(after) = rest.strip_prefix("/*")
        {
            if let Some(end) = after.find("*/") {
                rest = &after[end + 2..];
                continue;
            }
            no_closer = true;
        }
        if let Some(after) = rest.strip_prefix('\\') {
            let hex_len = after
                .bytes()
                .take(6)
                .take_while(u8::is_ascii_hexdigit)
                .count();
            if hex_len > 0 {
                if let Some(c) = u32::from_str_radix(&after[..hex_len], 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    push_normal(c, buf);
                }
                rest = &after[hex_len..];
                // One whitespace character ends the escape (CRLF counts as one).
                if let Some(tail) = rest.strip_prefix("\r\n") {
                    rest = tail;
                } else if rest.starts_with(|c: char| c.is_ascii_whitespace()) {
                    rest = &rest[1..];
                }
            } else if let Some(c) = after.chars().next() {
                push_normal(c, buf);
                rest = &after[c.len_utf8()..];
            } else {
                rest = after;
            }
            continue;
        }
        let Some(c) = rest.chars().next() else { break };
        push_normal(c, buf);
        rest = &rest[c.len_utf8()..];
    }
}

fn push_normal(c: char, buf: &mut String) {
    if !(c.is_whitespace() || c.is_control()) {
        buf.push(c.to_ascii_lowercase());
    }
}

/// Substrings of the normalised form that make a declaration dangerous wherever they appear.
const DANGEROUS: &[&str] = &[
    "expression(",
    "javascript:",
    "vbscript:",
    "livescript:",
    "@import",
    "behavior:",
    "-moz-binding",
    "image-set(",
];

/// Functions whose argument is a resource reference and so must pass the URL allow-list.
const RESOURCE_FUNCTIONS: [&str; 4] = ["url(", "src(", "image(", "-moz-element("];

/// Whether `decl` is dangerous as a browser may read it. A browser does not treat `/*` inside a
/// quoted string or an unquoted `url(` as a comment, while the splitter and the first
/// normalisation do, so the declaration is checked in both readings: comments removed, and
/// comment markers kept as plain text. Either one matching drops the declaration.
fn decl_is_dangerous(decl: &str, buf: &mut String) -> bool {
    normalise(decl, buf, false);
    if is_dangerous(buf) {
        return true;
    }
    normalise(decl, buf, true);
    is_dangerous(buf)
}

/// Whether the normalised declaration holds dangerous content.
fn is_dangerous(normal: &str) -> bool {
    if DANGEROUS.iter().any(|token| normal.contains(token)) {
        return true;
    }
    for function in RESOURCE_FUNCTIONS {
        let mut from = 0;
        while let Some(found) = normal[from..].find(function) {
            let start = from + found + function.len();
            let rest = &normal[start..];
            let end = rest.find(')').unwrap_or(rest.len());
            let arg = &rest[..end];
            // A browser reads a nested function (`var()`, `env()`, `attr()`, `if()`, `inherit()`
            // and any other) before it takes the URL, so the text here is not what will be
            // fetched: an argument holding any `(` is refused (fails closed). This also covers
            // another resource function inside the argument and keeps the scan moving past it.
            // An odd number of quote characters before the first `)` means that `)` may be
            // inside a string and the real argument is longer than this one: refused as well.
            if arg.contains('(') || arg.matches(['"', '\'']).count() % 2 == 1 {
                return true;
            }
            let checked = if function == "url(" {
                arg
            } else {
                // `src(`, `image(` and `-moz-element(` take an optional direction tag and then
                // one string or a plain URL. A comment marker, a second quote or an odd first
                // character means the two readings of the declaration may disagree about where
                // the URL is, so the declaration is refused (fails closed).
                let body = ["ltr", "rtl"]
                    .iter()
                    .find_map(|tag| arg.strip_prefix(tag))
                    .unwrap_or(arg);
                let first_ok = body.chars().next().is_none_or(|c| {
                    matches!(c, '"' | '\'')
                        || c.is_ascii_alphanumeric()
                        || matches!(c, '/' | '.' | '#' | '?' | '_' | '-' | '~' | '%')
                });
                if arg.contains("/*") || quote_inside(body) || !first_ok {
                    return true;
                }
                body
            };
            if !crate::url::is_allowed_url(checked.trim_start_matches(['\'', '"'])) {
                return true;
            }
            from = start + end;
        }
    }
    false
}

/// Whether `body` holds a quote character other than the pair that opens and closes it.
fn quote_inside(body: &str) -> bool {
    let inner = match body.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let rest = &body[1..];
            rest.strip_suffix(q).unwrap_or(rest)
        }
        _ => body,
    };
    inner.contains(['"', '\''])
}

fn reserve_string(
    buf: &mut String,
    bytes: usize,
    budget: &HtmlBudget,
) -> Result<(), HtmlSanitizeError> {
    budget.charge_bytes(bytes)?;
    if buf.try_reserve_exact(bytes).is_err() {
        budget.fail("allocation failed");
        return Err(budget.failure().unwrap_or(HtmlSanitizeError::Internal {
            reason: "allocation failed",
        }));
    }
    Ok(())
}

/// Scrub the value of a `style` attribute; see the module documentation. `None` means the
/// attribute is dropped. A flat multiple of the value length is charged to the work budget up front
/// as a proxy for the bytes scanned (the scans are linear, see the comment in the body), and every
/// buffer to the working-byte budget before it is allocated.
pub(crate) fn scrub_style<'v>(
    value: &'v str,
    whitelist: bool,
    budget: &HtmlBudget,
    report: &mut Reporter<'_>,
) -> Result<Option<Cow<'v, str>>, HtmlSanitizeError> {
    // One pass to split, two to normalise each declaration (comments cut, comments kept), two to
    // check them and one to write: six times the length.
    budget.charge_work(value.len().saturating_mul(6))?;
    let mut normal = String::new();
    reserve_string(&mut normal, value.len(), budget)?;
    let result = if whitelist {
        whitelist_style(value, &mut normal, budget, report)?
    } else {
        raw_style(value, &mut normal, budget, report)?
    };
    // The scratch buffers are gone; only a returned replacement string stays charged.
    budget.release_bytes(value.len().saturating_mul(if whitelist { 3 } else { 1 }));
    Ok(result)
}

/// Count a dropped declaration under its property name (the text before the first colon, when it
/// is a plain property name; `(invalid)` otherwise). The value is never recorded.
fn record_css(
    decl: &str,
    reason: RemovalReason,
    budget: &HtmlBudget,
    report: &mut Reporter<'_>,
) -> Result<(), HtmlSanitizeError> {
    let name = decl
        .split_once(':')
        .map(|(n, _)| n.trim())
        .filter(|n| {
            !n.is_empty()
                && n.len() <= 64
                && n.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-')
        })
        .map_or_else(|| "(invalid)".to_owned(), str::to_ascii_lowercase);
    report.record(RemovalKind::Css, &name, reason, budget)
}

fn raw_style<'v>(
    value: &'v str,
    normal: &mut String,
    budget: &HtmlBudget,
    report: &mut Reporter<'_>,
) -> Result<Option<Cow<'v, str>>, HtmlSanitizeError> {
    let mut kept: Vec<&str> = Vec::new();
    let mut dropped = false;
    for decl in (Declarations { src: value, pos: 0 }) {
        budget.charge_work(1)?;
        let trimmed = decl.trim();
        if trimmed.is_empty() {
            continue;
        }
        if decl_is_dangerous(decl, normal) {
            record_css(decl, RemovalReason::DangerousCss, budget, report)?;
            dropped = true;
        } else {
            budget.reserve(&mut kept, 1)?;
            kept.push(trimmed);
        }
    }
    if kept.is_empty() {
        return Ok(None);
    }
    if !dropped {
        return Ok(Some(Cow::Borrowed(value)));
    }
    let mut out = String::new();
    reserve_string(&mut out, value.len() + 2 * kept.len(), budget)?;
    for (n, decl) in kept.iter().enumerate() {
        if n > 0 {
            out.push_str("; ");
        }
        out.push_str(decl);
    }
    Ok(Some(Cow::Owned(out)))
}

/// Replace each comment in `decl` with a space (an unterminated one is kept as it is).
fn comments_to_space(decl: &str, out: &mut String) {
    out.clear();
    let mut rest = decl;
    while let Some(start) = rest.find("/*") {
        match rest[start + 2..].find("*/") {
            Some(end) => {
                out.push_str(&rest[..start]);
                out.push(' ');
                rest = &rest[start + 2 + end + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
}

fn whitelist_style<'v>(
    value: &'v str,
    normal: &mut String,
    budget: &HtmlBudget,
    report: &mut Reporter<'_>,
) -> Result<Option<Cow<'v, str>>, HtmlSanitizeError> {
    // Text of the kept values; every value is shorter than the attribute, so `store` never grows.
    let mut store = String::new();
    reserve_string(&mut store, value.len(), budget)?;
    let mut plain = String::new();
    reserve_string(&mut plain, value.len(), budget)?;
    // Per whitelist entry: the range of its value in `store`; `order` lists entries by first use.
    let mut slots: [Option<(usize, usize)>; STYLE_WHITELIST.len()] = [None; STYLE_WHITELIST.len()];
    let mut order: Vec<usize> = Vec::new();
    for decl in (Declarations { src: value, pos: 0 }) {
        budget.charge_work(1)?;
        if decl.trim().is_empty() {
            continue;
        }
        if decl_is_dangerous(decl, normal) {
            record_css(decl, RemovalReason::DangerousCss, budget, report)?;
            continue;
        }
        comments_to_space(decl, &mut plain);
        let Some((name, rest)) = plain.split_once(':') else {
            record_css(decl, RemovalReason::MalformedCss, budget, report)?;
            continue;
        };
        let name = name.trim();
        let val = rest.trim();
        if val.is_empty() || !name.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-') {
            record_css(decl, RemovalReason::MalformedCss, budget, report)?;
            continue;
        }
        let Some(index) = property_index(name) else {
            record_css(decl, RemovalReason::CssNotWhitelisted, budget, report)?;
            continue;
        };
        let start = store.len();
        store.push_str(val);
        if slots[index].is_none() {
            order.push(index);
        } else {
            record_css(decl, RemovalReason::DuplicateCss, budget, report)?;
        }
        slots[index] = Some((start, store.len()));
    }
    if order.is_empty() {
        return Ok(None);
    }
    let mut out = String::new();
    reserve_string(&mut out, value.len() + 2 * order.len() + 2, budget)?;
    for (n, index) in order.iter().enumerate() {
        let Some((start, end)) = slots[*index] else {
            continue;
        };
        if n > 0 {
            out.push_str("; ");
        }
        out.push_str(STYLE_WHITELIST[*index]);
        out.push(':');
        out.push_str(&store[start..end]);
    }
    Ok(Some(Cow::Owned(out)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A literal copy of the groups in spec section 4.7, written out independently of the table.
    fn spec_4_7() -> BTreeSet<String> {
        let mut names: Vec<String> = [
            // Typography
            "font-size",
            "font-family",
            "font-weight",
            "font-style",
            "line-height",
            "letter-spacing",
            "text-transform",
            "text-decoration",
            "text-align",
            "direction",
            "white-space",
            // Color and background
            "color",
            "background-color",
            "background-image",
            "background-position",
            "background-size",
            "background-repeat",
            "background-origin",
            "opacity",
            // Layout and box model
            "display",
            "float",
            "vertical-align",
            "object-fit",
            "width",
            "min-width",
            "max-width",
            "height",
            "min-height",
            "margin",
            "margin-top",
            "margin-bottom",
            "margin-left",
            "margin-right",
            "padding",
            "padding-top",
            "padding-bottom",
            "padding-left",
            "padding-right",
            // Borders (bare)
            "border",
            "border-color",
            "border-style",
            "border-width",
            "border-radius",
            "border-top",
            "border-bottom",
            // Tables
            "border-collapse",
            "border-spacing",
            "caption-side",
            "empty-cells",
            "table-layout",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        for side in ["top", "bottom", "left", "right"] {
            for part in ["style", "color", "width", "left-radius", "right-radius"] {
                names.push(format!("border-{side}-{part}"));
            }
        }
        names.into_iter().collect()
    }

    #[test]
    fn css_whitelist_equals_spec_4_7() {
        let table: BTreeSet<String> = STYLE_WHITELIST.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(table, spec_4_7());
        assert_eq!(STYLE_WHITELIST.len(), 70);
    }

    #[test]
    fn css_whitelist_is_sorted_and_unique() {
        assert!(STYLE_WHITELIST.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn css_lookup_cases() {
        for allowed in [
            "color",
            "COLOR",
            "Background-Color",
            "border-top-left-radius",
            "margin-top",
        ] {
            assert!(is_allowed_property(allowed), "{allowed}");
        }
        for refused in [
            "position",
            "top",
            "left",
            "bottom",
            "right",
            "z-index",
            "max-height",
            "border-left",
            "border-right",
            "behavior",
            "-moz-binding",
            "",
            "colo",
            "colors",
        ] {
            assert!(!is_allowed_property(refused), "{refused}");
        }
    }
}
