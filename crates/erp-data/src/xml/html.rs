//! Serialisation of `type="html"` field content and escaping helpers.
//!
//! text before the first child is not part of the value (the schema allows only whitespace);
//! - each child's tail text is part of it;
//! - empty non-void elements are written `<t></t>`, HTML void elements (`<br>`, `<img>`, …) are
//!   written without an end tag, comments are kept, `<script>`/`<style>` text is not escaped.
//!
//! Known difference: libxml2 percent-escapes non-ASCII characters and spaces in `href`/`src`/
//! `action` attributes and may pick single quotes; this writer escapes `& < > "` and always uses
//! double quotes. Both forms parse to the same document.

use super::convert::Cx;
use super::tree::{Child, Node};
use crate::error::{ParseError, ParseErrorKind};

/// HTML 4 empty elements, as in libxml2's `html40ElementTable`.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "basefont", "br", "col", "embed", "frame", "hr", "img", "input", "isindex",
    "link", "meta", "param",
];

const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style"];

pub(crate) fn serialize_children(cx: &Cx<'_>, field: &Node) -> Result<String, ParseError> {
    let mut out = String::new();
    let mut started = false;
    for child in &field.children {
        match child {
            Child::Text(text) => {
                if started {
                    push_text(&mut out, text);
                }
            }
            other => {
                started = true;
                node(cx, other, &mut out, false)?;
            }
        }
    }
    Ok(out)
}

fn node(cx: &Cx<'_>, child: &Child, out: &mut String, raw_text: bool) -> Result<(), ParseError> {
    match child {
        Child::Text(text) => {
            if raw_text {
                out.push_str(text);
            } else {
                push_text(out, text);
            }
        }
        Child::Comment { text, .. } => {
            out.push_str("<!--");
            out.push_str(text);
            out.push_str("-->");
        }
        Child::ProcessingInstruction { line, .. } => {
            return Err(cx.err(
                *line,
                ParseErrorKind::Refused {
                    tag: "field".to_string(),
                },
                "processing instructions are refused in a type=\"html\" field (the HTML serialisation has no form for them)",
            ));
        }
        Child::Element(element) => {
            let lower = element.tag.to_ascii_lowercase();
            out.push('<');
            out.push_str(&element.tag);
            for (key, value) in &element.attrs {
                push_attribute(out, key, value);
            }
            out.push('>');
            if VOID_ELEMENTS.contains(&lower.as_str()) {
                if !element.children.is_empty() {
                    return Err(cx.err(
                        element.line,
                        ParseErrorKind::Refused {
                            tag: element.tag.clone(),
                        },
                        format!(
                            "<{}> is an HTML void element: its content would be dropped by the \
                             HTML serialisation; write it empty",
                            element.tag
                        ),
                    ));
                }
                return Ok(());
            }
            let raw = RAW_TEXT_ELEMENTS.contains(&lower.as_str());
            for grandchild in &element.children {
                node(cx, grandchild, out, raw)?;
            }
            out.push_str("</");
            out.push_str(&element.tag);
            out.push('>');
        }
    }
    Ok(())
}

/// ` key="value"` with the value escaped for a double-quoted attribute (as lxml writes it).
pub(crate) fn push_attribute(out: &mut String, key: &str, value: &str) {
    out.push(' ');
    out.push_str(key);
    out.push_str("=\"");
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Character data escaped for element content.
pub(crate) fn push_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            other => out.push(other),
        }
    }
}
