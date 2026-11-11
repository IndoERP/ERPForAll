//! The cleaner (spec sections 4.3, 4.6 and 4.8).
//!
//! It walks the parsed tree once, without recursion (an explicit stack of child lists), and writes
//! the cleaned document as a flat list of [`Item`]s for the serialiser. Nothing is edited in
//! place: the parsed tree stays read-only and the item list borrows names and text from it.
//!
//! What it does, in this order for each element:
//! 1. Elements in the kill list are removed with their whole subtree, whatever their namespace:
//!    the match is on the lower-cased local name, so `<svg><script>` and `<math><style>` go as
//!    well as the HTML ones. The form controls (`input`, `button`, `select`, `textarea`) are
//!    removed the same way, which takes the `option` children of a `select` with it.
//! 2. `html`, `body` and `form` are unwrapped (children kept, element dropped), and so is every
//!    element that is not in the tag allow-list. Raw-text elements such as `xmp` and `plaintext`
//!    are not in the allow-list, so their text is kept as ordinary text and always escaped.
//! 3. Attributes: only the attribute allow-list survives, `on*` never does, `class` goes under
//!    `strip_classes`, and a link attribute whose value is not an allowed URL keeps its name with an
//!    empty value. `style` goes under `strip_style` and otherwise passes through the CSS scrub
//!    ([`crate::css`]).
//!
//! Comments: conditional comments (`[if ...]`, `[endif]`, `<![...`) and the bogus comment the
//! parser makes from `<![CDATA[...]]>` are removed; every other comment is kept.
//!
//! A result with no element and no non-whitespace text is empty (the empty-result rule).

use std::borrow::Cow;

use html5ever::ns;

use crate::budget::HtmlBudget;
use crate::dom::{Dom, NodeId, NodeKind};
use crate::policy::DefaultPolicy;
use crate::report::{RemovalKind, RemovalReason, ReportBuilder};
use crate::{HtmlSanitizeError, css, tables, url};

/// An attribute that survived cleaning.
#[derive(Debug)]
pub(crate) struct Attr<'d> {
    /// Namespace prefix as the parser gave it (`xlink`), if any.
    pub(crate) prefix: Option<&'d str>,
    /// Local name as the parser gave it.
    pub(crate) local: &'d str,
    /// The value; empty when the scheme check blanked it, rewritten when the style scrub changed it.
    pub(crate) value: Cow<'d, str>,
}

/// One piece of the cleaned document, in document order.
#[derive(Debug)]
pub(crate) enum Item<'d> {
    /// A start tag.
    Open {
        /// Local name.
        name: &'d str,
        /// Whether the element is in the HTML namespace.
        html: bool,
        /// Whether it is an HTML void element: no content and no end tag.
        void: bool,
        /// The attributes that were kept.
        attrs: Vec<Attr<'d>>,
    },
    /// The end tag of the matching earlier `Open` (never written for void elements).
    Close {
        /// Local name.
        name: &'d str,
    },
    /// Character data, unescaped.
    Text(&'d str),
    /// A comment's text.
    Comment(&'d str),
}

const VOID: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];

/// Controls removed with their subtree when form filtering is on (spec 4.8).
const FORM_KILL: &[&str] = &["button", "input", "select", "textarea"];

/// Whether `name` is an HTML void element (no end tag, no content).
pub(crate) fn is_void(name: &str) -> bool {
    VOID.contains(&name)
}

struct Frame<'d> {
    children: &'d [NodeId],
    next: usize,
    /// End tag to write when the frame ends; `None` for an unwrapped element or the root.
    close: Option<&'d str>,
}

fn internal(reason: &'static str) -> HtmlSanitizeError {
    HtmlSanitizeError::Internal { reason }
}

/// ASCII lower-case of `s`, borrowing when it already is.
fn lower(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        std::borrow::Cow::Owned(s.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Why a comment is removed, if it is: a conditional comment or CDATA remnant, or text that would
/// not read back as the same comment (it holds `<!--`, `-->` or `--!>`, starts with `>` or `->`, or
/// ends with `<!` or `<!-`; the parser lets a comment nest or end early on those, which would
/// change the output when it is parsed again).
fn dropped_comment(text: &str) -> Option<RemovalReason> {
    let trimmed = text.trim_start();
    let head: String = trimmed
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    if head.starts_with("[if")
        || head.starts_with("[endif")
        || head.starts_with("[cdata[")
        || head.starts_with("<![")
    {
        Some(RemovalReason::ConditionalComment)
    } else if text.contains("<!--")
        || text.contains("-->")
        || text.contains("--!>")
        || text.starts_with('>')
        || text.starts_with("->")
        || text.ends_with("<!-")
        || text.ends_with("<!")
    {
        Some(RemovalReason::UnstableComment)
    } else {
        None
    }
}

/// The reporting state of one cleaning run: the collector (absent on the reparse pass) and the
/// ordinal of the node being visited.
pub(crate) struct Reporter<'r> {
    builder: Option<&'r mut ReportBuilder>,
    location: u64,
}

impl Reporter<'_> {
    pub(crate) fn record(
        &mut self,
        kind: RemovalKind,
        name: &str,
        reason: RemovalReason,
        budget: &HtmlBudget,
    ) -> Result<(), HtmlSanitizeError> {
        match self.builder.as_deref_mut() {
            Some(builder) => builder.record(kind, name, reason, self.location, budget),
            None => Ok(()),
        }
    }
}

enum Verdict {
    Kill,
    Unwrap,
    Keep,
}

fn judge(local: &str) -> Verdict {
    let name = lower(local);
    if tables::is_kill_tag(&name) || FORM_KILL.contains(&&*name) {
        Verdict::Kill
    } else if tables::is_remove_tag(&name) || &*name == "form" || !tables::is_allowed_tag(&name) {
        Verdict::Unwrap
    } else {
        Verdict::Keep
    }
}

/// Whether an element has an attribute named `encoding` (no prefix, ASCII case ignored).
fn has_encoding_attr(attrs: &[html5ever::Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.name.prefix.is_none() && (*a.name.local).eq_ignore_ascii_case("encoding"))
}

/// Clean the attributes of one element into `out`.
fn clean_attrs<'d>(
    attrs: &'d [html5ever::Attribute],
    policy: &DefaultPolicy,
    budget: &HtmlBudget,
    report: &mut Reporter<'_>,
) -> Result<Vec<Attr<'d>>, HtmlSanitizeError> {
    let mut kept: Vec<Attr<'d>> = Vec::new();
    budget.reserve(&mut kept, attrs.len())?;
    for attr in attrs {
        let local: &str = &attr.name.local;
        let prefix: Option<&str> = attr.name.prefix.as_deref();
        // The key the tables use: lower-case, `prefix:local` when there is a prefix.
        let lowered = lower(local);
        let key = match prefix {
            Some(p) => format!("{}:{lowered}", lower(p)),
            None => lowered.to_string(),
        };
        if lowered.starts_with("on") || key.starts_with("on") {
            report.record(
                RemovalKind::Attribute,
                &key,
                RemovalReason::EventHandler,
                budget,
            )?;
            continue;
        }
        if !tables::is_safe_attr(&key) {
            report.record(
                RemovalKind::Attribute,
                &key,
                RemovalReason::ForbiddenAttribute,
                budget,
            )?;
            continue;
        }
        if key == "class" && policy.strip_classes() {
            report.record(
                RemovalKind::Attribute,
                &key,
                RemovalReason::StrippedClass,
                budget,
            )?;
            continue;
        }
        let value: &str = &attr.value;
        if key == "style" {
            // Never passed through unscrubbed: strip_style drops it, otherwise the scrub keeps,
            // rewrites or drops it (dangerous declarations go in every mode).
            if policy.strip_style() {
                report.record(
                    RemovalKind::Attribute,
                    &key,
                    RemovalReason::StrippedStyle,
                    budget,
                )?;
                continue;
            }
            if let Some(scrubbed) =
                css::scrub_style(value, policy.sanitize_style(), budget, report)?
            {
                kept.push(Attr {
                    prefix,
                    local,
                    value: scrubbed,
                });
            }
            continue;
        }
        let blank = (tables::is_link_attr(&key) || tables::is_link_attr(&lowered))
            && !url::is_allowed_url(value);
        if blank {
            report.record(RemovalKind::Url, &key, RemovalReason::DisallowedUrl, budget)?;
        }
        kept.push(Attr {
            prefix,
            local,
            value: Cow::Borrowed(if blank { "" } else { value }),
        });
    }
    Ok(kept)
}

/// Report attributes discarded with an unwrapped element, without copying values or building
/// a survivor buffer. Identifier normalization uses a fixed buffer before the report copies it.
fn report_unwrapped_attrs(
    attrs: &[html5ever::Attribute],
    budget: &HtmlBudget,
    report: &mut Reporter<'_>,
) -> Result<(), HtmlSanitizeError> {
    budget.charge_work(attrs.len())?;
    for attr in attrs {
        let local: &str = &attr.name.local;
        let prefix = attr.name.prefix.as_deref();
        let event = |name: &str| {
            name.get(..2)
                .is_some_and(|head| head.eq_ignore_ascii_case("on"))
        };
        let mut bytes = [0_u8; 64];
        let len = local
            .len()
            .saturating_add(prefix.map_or(0, |p| p.len().saturating_add(1)));
        let key = if len > 0 && len <= bytes.len() {
            let names = prefix
                .into_iter()
                .flat_map(|p| p.bytes().chain(std::iter::once(b':')))
                .chain(local.bytes());
            budget.charge_work(len)?;
            let mut valid = true;
            for (slot, byte) in bytes.iter_mut().zip(names) {
                valid &= byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.');
                *slot = byte.to_ascii_lowercase();
            }
            if valid {
                std::str::from_utf8(&bytes[..len]).map_err(|_| HtmlSanitizeError::Internal {
                    reason: "report identifier is not ASCII",
                })?
            } else {
                "(invalid)"
            }
        } else {
            "(invalid)"
        };
        let reason = if event(local) || prefix.is_some_and(event) {
            RemovalReason::EventHandler
        } else if !tables::is_safe_attr(key) {
            RemovalReason::ForbiddenAttribute
        } else {
            RemovalReason::RemovedWithElement
        };
        report.record(RemovalKind::Attribute, key, reason, budget)?;
    }
    Ok(())
}

/// Clean `dom` under `policy`. The returned items borrow from `dom`. Every item, attribute and
/// stack slot is charged to the working-byte budget before it is stored, and every node visited to
/// the work budget.
pub(crate) fn clean<'d>(
    dom: &'d Dom,
    policy: &DefaultPolicy,
    budget: &HtmlBudget,
    report: Option<&mut ReportBuilder>,
) -> Result<Vec<Item<'d>>, HtmlSanitizeError> {
    let mut report = Reporter {
        builder: report,
        location: 0,
    };
    let root = dom
        .children(dom.document())
        .iter()
        .copied()
        .find(|id| matches!(dom.kind(*id), Some(NodeKind::Element { .. })))
        .ok_or_else(|| internal("parsed tree has no root element"))?;
    let Some(NodeKind::Element { attrs, .. }) = dom.kind(root) else {
        return Err(internal("parsed root is not an element"));
    };
    // Explicit `html` tokens can contribute attributes to the generated fragment root. Its
    // wrapper is canonicalisation, but the retained attributes are still discarded by us.
    if !attrs.is_empty() {
        budget.charge_work(1)?;
        report.location = 1;
        report_unwrapped_attrs(attrs, budget, &mut report)?;
    }
    let mut items: Vec<Item<'d>> = Vec::new();
    let mut stack: Vec<Frame<'d>> = Vec::new();
    budget.reserve(&mut stack, 1)?;
    stack.push(Frame {
        children: dom.children(root),
        next: 0,
        close: None,
    });
    let mut has_content = false;
    while let Some(frame) = stack.last_mut() {
        let Some(&id) = frame.children.get(frame.next) else {
            if let Some(name) = frame.close {
                budget.reserve(&mut items, 1)?;
                items.push(Item::Close { name });
            }
            stack.pop();
            continue;
        };
        frame.next += 1;
        budget.charge_work(1)?;
        report.location = report.location.saturating_add(1);
        match dom.kind(id) {
            Some(NodeKind::Text(text)) => {
                if text.is_empty() {
                    continue;
                }
                has_content |= text.chars().any(|c| !c.is_whitespace());
                budget.reserve(&mut items, 1)?;
                items.push(Item::Text(text));
            }
            Some(NodeKind::Comment(text)) => {
                if let Some(reason) = dropped_comment(text) {
                    let name = if reason == RemovalReason::ConditionalComment {
                        "conditional"
                    } else {
                        "unstable"
                    };
                    report.record(RemovalKind::Comment, name, reason, budget)?;
                    continue;
                }
                budget.reserve(&mut items, 1)?;
                items.push(Item::Comment(text));
            }
            Some(NodeKind::Element {
                name,
                attrs,
                template_contents,
                ..
            }) => {
                let local: &str = &name.local;
                let content = template_contents.unwrap_or(id);
                let verdict = match judge(local) {
                    // An HTML element carrying an `encoding` attribute is unwrapped (the
                    // reference deletes its start tag; spec 4.2 step 2).
                    Verdict::Keep if name.ns == ns!(html) && has_encoding_attr(attrs) => {
                        budget.charge_work(attrs.len())?;
                        Verdict::Unwrap
                    }
                    other => other,
                };
                match verdict {
                    Verdict::Kill => {
                        report.record(
                            RemovalKind::Tag,
                            &lower(local),
                            RemovalReason::ForbiddenElement,
                            budget,
                        )?;
                    }
                    Verdict::Unwrap => {
                        // HTML-namespace `html` and `body` wrappers are canonicalisation, not
                        // policy removals; a foreign-content element of that name is reported.
                        if name.ns != ns!(html) || !tables::is_remove_tag(&lower(local)) {
                            report.record(
                                RemovalKind::Tag,
                                &lower(local),
                                RemovalReason::UnwrappedElement,
                                budget,
                            )?;
                        }
                        report_unwrapped_attrs(attrs, budget, &mut report)?;
                        budget.reserve(&mut stack, 1)?;
                        stack.push(Frame {
                            children: dom.children(content),
                            next: 0,
                            close: None,
                        });
                    }
                    Verdict::Keep => {
                        budget.charge_work(attrs.len())?;
                        let html = name.ns == ns!(html);
                        let void = html && is_void(local);
                        let attrs = clean_attrs(attrs, policy, budget, &mut report)?;
                        budget.reserve(&mut items, 1)?;
                        items.push(Item::Open {
                            name: local,
                            html,
                            void,
                            attrs,
                        });
                        has_content = true;
                        if !void {
                            budget.reserve(&mut stack, 1)?;
                            stack.push(Frame {
                                children: dom.children(content),
                                next: 0,
                                close: Some(local),
                            });
                        }
                    }
                }
            }
            Some(NodeKind::Document | NodeKind::TemplateContents) => {
                return Err(internal(
                    "a template fragment or document is listed as a child",
                ));
            }
            Some(NodeKind::Doctype | NodeKind::ProcessingInstruction) => {}
            None => return Err(internal("child id names no node")),
        }
    }
    if !has_content {
        items.clear();
    }
    Ok(items)
}
