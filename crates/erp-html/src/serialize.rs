//! The serialiser (spec section 4.9): the cleaned items written as HTML or XHTML.
//!
//! * Void elements are written `<br>` in HTML and `<br/>` in XML; every other element always has
//!   an end tag, so an empty `<div></div>` is never shortened.
//! * Text escapes `&`, `<` and `>`; attribute values also escape `"` and are always double-quoted.
//!   U+00A0 is written `&nbsp;` in HTML and `&#160;` in XML, where `&nbsp;` is not defined
//!   .
//! * A text that starts with a newline right after `<pre>` or `<listing>` gets one extra newline,
//!   because the parser drops the first one and the output must read back the same.
//! * Every byte written is checked against the per-serialisation output limit first, then charged
//!   to the work budget; the buffer's growth is charged to the working-byte budget before it is
//!   allocated and never grows past the output limit.
//!
//! The walk is a loop over the flat item list, so there is no recursion.

use crate::budget::HtmlBudget;
use crate::clean::{Attr, Item};
use crate::policy::OutputMethod;
use crate::{HtmlSanitizeError, Limits};

struct Out<'b> {
    text: String,
    limit: u64,
    budget: &'b HtmlBudget,
}

impl Out<'_> {
    /// Write `s`: the limit first, then the work charge, then room in the buffer.
    fn push(&mut self, s: &str) -> Result<(), HtmlSanitizeError> {
        let total = u64::try_from(self.text.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(s.len()).unwrap_or(u64::MAX));
        if total > self.limit {
            return Err(HtmlSanitizeError::OutputTooLarge { limit: self.limit });
        }
        self.budget.charge_work(s.len())?;
        self.grow(s.len())?;
        self.text.push_str(s);
        Ok(())
    }

    /// Make room for `extra` more bytes: charge the new capacity, then reserve it. The capacity
    /// never exceeds the output limit, which `push` has already checked `extra` against.
    fn grow(&mut self, extra: usize) -> Result<(), HtmlSanitizeError> {
        if self.text.capacity() - self.text.len() >= extra {
            return Ok(());
        }
        let needed = self.text.len().saturating_add(extra);
        let cap_limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        let new_cap = needed
            .max(self.text.capacity().saturating_mul(2))
            .max(64)
            .min(cap_limit.max(needed));
        self.budget.charge_bytes(new_cap - self.text.capacity())?;
        if self
            .text
            .try_reserve_exact(new_cap - self.text.len())
            .is_err()
        {
            self.budget.fail("allocation failed");
            return Err(self
                .budget
                .failure()
                .unwrap_or(HtmlSanitizeError::Internal {
                    reason: "allocation failed",
                }));
        }
        Ok(())
    }

    /// Write `s` escaped. Runs of ordinary characters are written in one piece.
    fn escaped(
        &mut self,
        s: &str,
        method: OutputMethod,
        quotes: bool,
    ) -> Result<(), HtmlSanitizeError> {
        let nbsp = match method {
            OutputMethod::Html => "&nbsp;",
            OutputMethod::Xml => "&#160;",
        };
        let mut run_start = 0;
        for (at, ch) in s.char_indices() {
            let entity = match ch {
                '&' => "&amp;",
                '<' => "&lt;",
                '>' => "&gt;",
                '"' if quotes => "&quot;",
                '\u{a0}' => nbsp,
                _ => continue,
            };
            self.push(&s[run_start..at])?;
            self.push(entity)?;
            run_start = at + ch.len_utf8();
        }
        self.push(&s[run_start..])
    }

    fn attr(&mut self, attr: &Attr<'_>, method: OutputMethod) -> Result<(), HtmlSanitizeError> {
        self.push(" ")?;
        if let Some(prefix) = attr.prefix {
            self.push(prefix)?;
            self.push(":")?;
        }
        self.push(attr.local)?;
        self.push("=\"")?;
        self.escaped(&attr.value, method, true)?;
        self.push("\"")
    }
}

/// Write `items` in `method`. An empty list gives the empty string.
pub(crate) fn serialize(
    items: &[Item<'_>],
    method: OutputMethod,
    limits: &Limits,
    budget: &HtmlBudget,
) -> Result<String, HtmlSanitizeError> {
    let mut out = Out {
        text: String::new(),
        limit: limits.output_bytes(),
        budget,
    };
    for (at, item) in items.iter().enumerate() {
        match item {
            Item::Open {
                name,
                html,
                void,
                attrs,
            } => {
                out.push("<")?;
                out.push(name)?;
                for attr in attrs {
                    out.attr(attr, method)?;
                }
                if *void {
                    out.push(match method {
                        OutputMethod::Html => ">",
                        OutputMethod::Xml => "/>",
                    })?;
                } else {
                    out.push(">")?;
                    let starts_with_newline = matches!(
                        items.get(at + 1),
                        Some(Item::Text(t)) if t.starts_with('\n')
                    );
                    if *html && starts_with_newline && matches!(*name, "pre" | "listing") {
                        out.push("\n")?;
                    }
                }
            }
            Item::Close { name } => {
                out.push("</")?;
                out.push(name)?;
                out.push(">")?;
            }
            Item::Text(text) => out.escaped(text, method, false)?,
            Item::Comment(text) => {
                out.push("<!--")?;
                out.push(text)?;
                out.push("-->")?;
            }
        }
    }
    Ok(out.text)
}
