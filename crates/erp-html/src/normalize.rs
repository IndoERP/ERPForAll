//! Normalisation (spec 4.2, decision D-2): two bounded rewrites around the cleaner.
//!
//! * [`repair_comments`] runs on the input text before the parse. It turns the broken comment
//!   markers the reference environment repairs into well-formed ones, in the reference's order:
//!   first every `--!>` becomes `-->`, then every `<!-->` and `<!--->` becomes `<!-- -->`. It is
//!   one forward scan charged to the work budget; the text it writes is charged to the
//!   working-byte budget before it is allocated. Input with no broken marker is returned
//!   borrowed.
//! * [`wrap_leading_text`] runs on the cleaned items (rule R1): when the fragment starts with text,
//!   the leading run of text, comments and inline elements up to the first block-level element is
//!   wrapped in `<p>` (refused when that extra level would exceed the depth limit). The other wrappers the reference's HTML library adds around top-level
//!   content are not emulated.

use std::borrow::Cow;

use crate::HtmlSanitizeError;
use crate::budget::HtmlBudget;
use crate::clean::Item;

/// Replacement for `<!-->` and `<!--->`.
const EMPTY_COMMENT: &str = "<!-- -->";

fn out_string(capacity: usize, budget: &HtmlBudget) -> Result<String, HtmlSanitizeError> {
    budget.charge_bytes(capacity)?;
    let mut out = String::new();
    if out.try_reserve_exact(capacity).is_err() {
        budget.fail("allocation failed");
        return Err(budget.failure().unwrap_or(HtmlSanitizeError::Internal {
            reason: "allocation failed",
        }));
    }
    Ok(out)
}

/// The repair in one forward scan. Applying the reference's two replacements one after the other
/// (`--!>` to `-->` first, then `<!-->` and `<!--->` to `<!-- -->`) has the same result as this:
/// a `--!>` right after `<!` or `<!-` is what the second replacement then sees as `<!-->` or
/// `<!--->`, so those two spellings (`<!--!>`, `<!---!>`) are recognised here directly.
///
/// The scan is charged to the work budget at its length. The output buffer is only allocated at
/// the first marker, charged for its worst case (a replacement is at most 3 bytes longer than the
/// text it replaces and the shortest marker is 5 bytes, so 8/5 of the rest of the input).
pub(crate) fn repair_comments<'i>(
    input: &'i str,
    budget: &HtmlBudget,
) -> Result<Cow<'i, str>, HtmlSanitizeError> {
    budget.charge_work(input.len())?;
    let bytes = input.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let tail = &bytes[i..];
        let (skip, replacement) = match bytes[i] {
            b'-' if tail.starts_with(b"--!>") => (4, "-->"),
            b'<' if tail.starts_with(b"<!--") => {
                if tail.starts_with(b"<!--->") {
                    (6, EMPTY_COMMENT)
                } else if tail.starts_with(b"<!-->") {
                    (5, EMPTY_COMMENT)
                } else if tail.starts_with(b"<!---!>") {
                    (7, EMPTY_COMMENT)
                } else if tail.starts_with(b"<!--!>") {
                    (6, EMPTY_COMMENT)
                } else {
                    i += 1;
                    continue;
                }
            }
            _ => {
                i += 1;
                continue;
            }
        };
        if out.is_none() {
            let worst = (bytes.len() - i).saturating_mul(8) / 5 + 8;
            out = Some(out_string(i.saturating_add(worst), budget)?);
        }
        if let Some(text) = out.as_mut() {
            text.push_str(&input[copied..i]);
            text.push_str(replacement);
        }
        i += skip;
        copied = i;
    }
    Ok(match out {
        Some(mut text) => {
            text.push_str(&input[copied..]);
            Cow::Owned(text)
        }
        None => Cow::Borrowed(input),
    })
}

/// HTML elements a `<p>` cannot contain: the parser closes the paragraph before them.
const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "listing",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "plaintext",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
    "xmp",
];

fn is_block(name: &str, html: bool) -> bool {
    html && BLOCK.contains(&name)
}

/// Rule R1: wrap the leading run of the fragment in `<p>` when it starts with text. See the module
/// documentation. The scan visits each item at most once and charges it to the work budget.
pub(crate) fn wrap_leading_text<'d>(
    items: &mut Vec<Item<'d>>,
    budget: &HtmlBudget,
) -> Result<(), HtmlSanitizeError> {
    // The leading text may be several items (unwrapped or removed elements leave adjacent text
    // pieces that the parser reads back as one): look at all of them together.
    let mut has_text = false;
    for item in items.iter() {
        let Item::Text(text) = item else { break };
        budget.charge_work(1)?;
        has_text |= text.chars().any(|c| !c.is_whitespace());
    }
    if !has_text {
        return Ok(());
    }
    let mut end = 0;
    'run: while end < items.len() {
        budget.charge_work(1)?;
        match &items[end] {
            Item::Text(_) | Item::Comment(_) => end += 1,
            Item::Close { .. } => break,
            Item::Open {
                name, html, void, ..
            } => {
                if is_block(name, *html) {
                    break;
                }
                if *void {
                    end += 1;
                    continue;
                }
                // Find the matching end tag; a block anywhere inside ends the run before it.
                let mut depth = 0usize;
                let mut at = end;
                loop {
                    budget.charge_work(1)?;
                    match items.get(at) {
                        Some(Item::Open {
                            name, html, void, ..
                        }) => {
                            if is_block(name, *html) {
                                break 'run;
                            }
                            if !*void {
                                depth += 1;
                            }
                        }
                        Some(Item::Close { .. }) => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                break;
                            }
                        }
                        Some(_) => {}
                        None => break 'run,
                    }
                    at += 1;
                }
                end = at + 1;
            }
        }
    }
    // The wrapper is one level the input did not have: refuse if the output would be too deep.
    budget.admit_wrapper_level()?;
    budget.reserve(items, 2)?;
    items.insert(end, Item::Close { name: "p" });
    items.insert(
        0,
        Item::Open {
            name: "p",
            html: true,
            void: false,
            attrs: Vec::new(),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;

    /// The reference's two replacements, applied one after the other.
    fn two_step(input: &str) -> String {
        input
            .replace("--!>", "-->")
            .replace("<!--->", "<!-- -->")
            .replace("<!-->", "<!-- -->")
    }

    fn repaired(input: &str) -> String {
        let budget = HtmlBudget::new(&Limits::ceiling());
        repair_comments(input, &budget)
            .expect("within the ceilings")
            .into_owned()
    }

    #[test]
    fn single_scan_equals_the_two_step_replacement_on_every_short_string() {
        // Every string up to length 8 over the characters the markers are made of.
        let alphabet = ['<', '!', '-', '>', 'x'];
        let mut frontier = vec![String::new()];
        let mut checked = 0usize;
        for _ in 0..8 {
            let mut next = Vec::new();
            for prefix in &frontier {
                for c in alphabet {
                    let mut s = prefix.clone();
                    s.push(c);
                    assert_eq!(repaired(&s), two_step(&s), "{s:?}");
                    checked += 1;
                    next.push(s);
                }
            }
            frontier = next;
        }
        assert_eq!(checked, 488_280);
    }

    #[test]
    fn clean_input_is_returned_borrowed() {
        let budget = HtmlBudget::new(&Limits::ceiling());
        let text = "<p>a<!-- c -->b <!----></p>";
        assert!(matches!(
            repair_comments(text, &budget),
            Ok(Cow::Borrowed(t)) if t == text
        ));
    }

    #[test]
    fn the_scan_is_charged_to_the_work_budget() {
        let input = "<!-->".repeat(1_000);
        let limits = Limits::ceiling().with_work_units(4_999).expect("limit");
        let budget = HtmlBudget::new(&limits);
        assert!(matches!(
            repair_comments(&input, &budget),
            Err(HtmlSanitizeError::WorkLimit { .. })
        ));
        let limits = Limits::ceiling().with_work_units(5_000).expect("limit");
        let budget = HtmlBudget::new(&limits);
        assert_eq!(
            repair_comments(&input, &budget).expect("fits").len(),
            "<!-- -->".len() * 1_000
        );
    }

    #[test]
    fn the_output_buffer_is_charged_before_it_is_allocated() {
        let input = "<!-->".repeat(1_000);
        let limits = Limits::ceiling().with_working_bytes(4_000).expect("limit");
        let budget = HtmlBudget::new(&limits);
        assert!(matches!(
            repair_comments(&input, &budget),
            Err(HtmlSanitizeError::MemoryLimit { .. })
        ));
        assert_eq!(budget.peak_bytes(), 0, "nothing was charged or allocated");
    }
}
