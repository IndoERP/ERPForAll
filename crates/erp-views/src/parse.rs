//! Arch parsing: `roxmltree` (no DTD, no external entities, node limit) converted into an
//! [`Arch`] without recursion.
//!
//! Comments and processing instructions carry no view semantics and are dropped; the text around
//! them is joined.
//! Namespaced tags and attributes, and namespace declarations, are
//! rejected: roxmltree resolves prefixes, and keeping only local names would change the arch
//! silently.

use crate::arch::{Arch, Caps, NodeId, NodeKind, NodeOrigin};
use crate::error::{LimitKind, ViewError, ViewResult};
use crate::limits::ViewLimits;
use std::num::NonZeroU32;

/// Parses one arch string of view `view_id`; every element records its origin (the view and the
/// line of its start tag).
pub(crate) fn parse_arch(xml: &str, limits: &ViewLimits, view_id: i64) -> ViewResult<Arch> {
    if xml.trim().is_empty() {
        return Err(ViewError::EmptyArch);
    }
    if xml.len() > limits.get(LimitKind::ArchBytes) {
        return Err(limits.exceeded(LimitKind::ArchBytes, Some(xml.len())));
    }
    prescan(xml, limits)?;
    // roxmltree counts the document node, text, comments and processing instructions too; its
    // own node limit bounds the parser's memory before our element count applies.
    let nodes_limit = u32::try_from(limits.get(LimitKind::ArchNodes))
        .map_err(|_| ViewError::InvariantViolation("XML node limit does not fit in u32"))?;
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit,
        entity_resolver: None,
    };
    let doc = roxmltree::Document::parse_with_options(xml, options)
        .map_err(|e| map_parse_error(e, limits))?;
    let mut lines = Lines {
        xml,
        offset: 0,
        line: 1,
        view_id,
    };
    convert(doc.root_element(), limits, &mut lines)
}

/// Line numbers of element start tags, counted incrementally: elements are read in document
/// order, so the newlines between two start tags are counted once (linear in the arch, where
/// roxmltree's `text_pos_at` scans from the start each time).
struct Lines<'x> {
    xml: &'x str,
    /// Byte offset up to which newlines are counted.
    offset: usize,
    /// The line at `offset`.
    line: u32,
    view_id: i64,
}

impl Lines<'_> {
    /// The origin of the element whose start tag begins at byte `start`.
    fn origin_at(&mut self, start: usize) -> ViewResult<NodeOrigin> {
        let gap =
            self.xml
                .as_bytes()
                .get(self.offset..start)
                .ok_or(ViewError::InvariantViolation(
                    "element start tags are not in document order",
                ))?;
        let newlines = gap.iter().filter(|&&b| b == b'\n').count();
        let newlines = u32::try_from(newlines)
            .map_err(|_| ViewError::InvariantViolation("line number beyond u32"))?;
        self.line = self
            .line
            .checked_add(newlines)
            .ok_or(ViewError::InvariantViolation("line number beyond u32"))?;
        self.offset = start;
        let line = NonZeroU32::new(self.line)
            .ok_or(ViewError::InvariantViolation("line numbers start at 1"))?;
        Ok(NodeOrigin::new(self.view_id, line))
    }
}

/// The name of the root element of `xml`, read without parsing the document (review R-8): the
/// prolog before it (a byte order mark, whitespace, the XML declaration, processing instructions
/// and comments) is skipped and the name of the first start tag returned; nothing after it is read.
///
/// `None` when no plain root name is found there: text or a `DOCTYPE` before the first element, an
/// unterminated comment or declaration, an end tag, a name that does not start like an XML name,
/// or a prefixed name (namespaces are refused by the parse). Callers that need the arch to be
/// valid still parse it; this only reads the tag a view's `type` defaults to.
///
/// ```
/// assert_eq!(erp_views::root_tag("<?xml version='1.0'?><form><sheet/></form>"), Some("form"));
/// assert_eq!(erp_views::root_tag("text<form/>"), None);
/// ```
pub fn root_tag(xml: &str) -> Option<&str> {
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let b = xml.as_bytes();
    let mut i = 0;
    loop {
        i += b
            .get(i..)?
            .iter()
            .take_while(|c| c.is_ascii_whitespace())
            .count();
        let rest = b.get(i..)?;
        if rest.starts_with(b"<?") {
            i = find_end(b, i + 2, b"?>")?;
        } else if rest.starts_with(b"<!--") {
            i = find_end(b, i + 4, b"-->")?;
        } else if rest.first() == Some(&b'<') {
            let first = *rest.get(1)?;
            if !(first.is_ascii_alphabetic() || first == b'_' || first >= 0x80) {
                return None;
            }
            // the name ends at ASCII markup, so the slice ends on a character boundary
            let len = rest[1..]
                .iter()
                .take_while(|&&c| !(c.is_ascii_whitespace() || c == b'/' || c == b'>'))
                .count();
            let name = xml.get(i + 1..i + 1 + len)?;
            let plain = name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.') || !c.is_ascii());
            return plain.then_some(name);
        } else {
            return None;
        }
    }
}

/// The nesting and character-reference pre-scan of [`parse_arch`], for other crates that parse XML
/// with roxmltree under the same rules (the template compiler): one implementation of
/// the guard against roxmltree's recursion.
///
/// ```
/// use erp_views::{check_xml_nesting, LimitKind, ViewError, ViewLimits};
///
/// let limits = ViewLimits::default().lowered(LimitKind::Depth, 2)?;
/// // A self-closing tag opens no level the parser recurses into, so it is not counted.
/// assert!(check_xml_nesting("<a><b><c/></b></a>", &limits).is_ok());
/// assert!(matches!(
///     check_xml_nesting("<a><b><c></c></b></a>", &limits),
///     Err(ViewError::LimitExceeded { limit: LimitKind::Depth, .. })
/// ));
/// # Ok::<(), ViewError>(())
/// ```
///
/// # Errors
///
/// [`ViewError::LimitExceeded`] for [`LimitKind::Depth`](crate::LimitKind::Depth) when elements nest
/// deeper than the limit; [`ViewError::XmlSyntax`] for a character reference to a surrogate or a
/// code point beyond U+10FFFF.
pub fn check_xml_nesting(xml: &str, limits: &ViewLimits) -> ViewResult<()> {
    prescan(xml, limits)
}

/// Checks what roxmltree would get wrong before it runs, in one linear pass over the bytes:
///
/// - Nesting depth: roxmltree's tokenizer recurses once per element level (`parse_element` ->
///   `parse_content` in roxmltree 0.21), so a deeply nested arch would exhaust the stack inside
///   the parser. Open elements are counted the way roxmltree reads them (comments, CDATA and
///   processing instructions skipped, quoted attribute values honoured).
/// - Character references to surrogates or beyond U+10FFFF: roxmltree turns them into U+FFFD
///   (`char::from_u32(n).unwrap_or('\u{FFFD}')`), lxml rejects them; they are errors here.
///
/// Where the input stops being well-formed the pass stops too: roxmltree reports the error at
/// that point without nesting further.
fn prescan(xml: &str, limits: &ViewLimits) -> ViewResult<()> {
    let max = limits.get(LimitKind::Depth);
    let b = xml.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while let Some(off) = b
        .get(i..)
        .and_then(|rest| rest.iter().position(|&c| c == b'<' || c == b'&'))
    {
        let at = i + off;
        if b[at] == b'&' {
            check_char_ref(xml, at)?;
            i = at + 1;
            continue;
        }
        i = match markup_at(xml, at)? {
            Markup::Skip(next) => next,
            Markup::Close(next) => {
                depth = depth.saturating_sub(1);
                next
            }
            Markup::Open(next) => {
                depth = depth.saturating_add(1);
                if depth > max {
                    return Err(limits.exceeded(LimitKind::Depth, Some(depth)));
                }
                next
            }
            Markup::Stop => return Ok(()),
        };
    }
    Ok(())
}

/// The markup starting with `<` at some offset, as the pre-scan counts it, with the offset where
/// scanning resumes.
enum Markup {
    /// A comment, CDATA section, processing instruction or self-closing tag.
    Skip(usize),
    /// A start tag that opens an element.
    Open(usize),
    /// An end tag.
    Close(usize),
    /// A `DOCTYPE` (rejected by roxmltree before any element) or malformed markup: roxmltree
    /// reports it at this point without nesting further, so the pre-scan stops.
    Stop,
}

fn markup_at(xml: &str, at: usize) -> ViewResult<Markup> {
    let b = xml.as_bytes();
    let rest = &b[at..];
    let skip_past = |from: usize, needle: &[u8]| match find_end(b, from, needle) {
        Some(next) => Markup::Skip(next),
        // Unterminated comment, CDATA or PI: roxmltree reports it.
        None => Markup::Stop,
    };
    Ok(if rest.starts_with(b"<!--") {
        skip_past(at + 4, b"-->")
    } else if rest.starts_with(b"<![CDATA[") {
        skip_past(at + 9, b"]]>")
    } else if rest.starts_with(b"<?") {
        skip_past(at + 2, b"?>")
    } else if rest.starts_with(b"<!") {
        Markup::Stop
    } else if rest.starts_with(b"</") {
        Markup::Close(at + 2)
    } else {
        match start_tag_end(xml, at)? {
            None => Markup::Stop,
            Some((end, true)) => Markup::Skip(end + 1),
            Some((end, false)) => Markup::Open(end + 1),
        }
    })
}

/// The offset just past the first `needle` at or after `from`.
fn find_end(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    b.get(from..)
        .and_then(|rest| rest.windows(needle.len()).position(|w| w == needle))
        .map(|p| from + p + needle.len())
}

/// The `>` ending the start tag that begins at `at` (outside quoted attribute values) and whether
/// the tag is self-closing; character references in values are checked on the way. `None` when
/// this is not a well-formed start tag: no name, no `>`, or a `<` inside it (roxmltree stops
/// there, also inside a value).
fn start_tag_end(xml: &str, at: usize) -> ViewResult<Option<(usize, bool)>> {
    let b = xml.as_bytes();
    let name_start = b
        .get(at + 1)
        .is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c == b':' || c >= 0x80);
    if !name_start {
        return Ok(None);
    }
    let mut quote = None;
    for (j, &c) in b.iter().enumerate().skip(at + 1) {
        match (quote, c) {
            (_, b'<') => return Ok(None),
            (Some(_), b'&') => check_char_ref(xml, j)?,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(c),
            (None, b'>') => return Ok(Some((j, b[j - 1] == b'/'))),
            (None, _) => {}
        }
    }
    Ok(None)
}

/// Rejects `&#...;` references that are not XML characters but that roxmltree would replace by
/// U+FFFD. Malformed references are left to roxmltree, which rejects them.
fn check_char_ref(xml: &str, at: usize) -> ViewResult<()> {
    let b = xml.as_bytes();
    if b.get(at + 1) != Some(&b'#') {
        return Ok(());
    }
    let (radix, from) = if b.get(at + 2) == Some(&b'x') {
        (16, at + 3)
    } else {
        (10, at + 2)
    };
    let len = b.get(from..).map_or(0, |rest| {
        rest.iter().take_while(|c| c.is_ascii_hexdigit()).count()
    });
    let end = from + len;
    if len == 0 || b.get(end) != Some(&b';') {
        return Ok(());
    }
    let Ok(value) = u32::from_str_radix(&xml[from..end], radix) else {
        return Ok(());
    };
    if (0xD800..=0xDFFF).contains(&value) || value > 0x10_FFFF {
        let before = &xml[..at];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        let position = |n: usize| {
            u32::try_from(n).map_err(|_| ViewError::InvariantViolation("position beyond u32"))
        };
        return Err(ViewError::XmlSyntax {
            message: format!(
                "invalid character reference '{}' at {line}:{column}",
                &xml[at..=end]
            ),
            line: position(line)?,
            column: position(column)?,
        });
    }
    Ok(())
}

fn map_parse_error(error: roxmltree::Error, limits: &ViewLimits) -> ViewError {
    match error {
        roxmltree::Error::DtdDetected => ViewError::DoctypeNotAllowed,
        // roxmltree refuses the node that would take it past the limit
        // (`nodes.len() >= nodes_limit`, roxmltree 0.21 `parse.rs:562`).
        roxmltree::Error::NodesLimitReached => {
            let max = limits.get(LimitKind::ArchNodes);
            limits.exceeded(LimitKind::ArchNodes, max.checked_add(1))
        }
        // Every other roxmltree error is a well-formedness error (unknown entity, duplicate
        // attribute, invalid character, ...): reported with its position, never recovered.
        other => {
            let pos = other.pos();
            ViewError::XmlSyntax {
                message: other.to_string(),
                line: pos.row,
                column: pos.col,
            }
        }
    }
}

fn check_namespaces(node: roxmltree::Node<'_, '_>) -> ViewResult<()> {
    if let Some(uri) = node.tag_name().namespace() {
        return Err(ViewError::NamespaceNotSupported {
            name: format!("{{{uri}}}{}", node.tag_name().name()),
        });
    }
    for attr in node.attributes() {
        if let Some(uri) = attr.namespace() {
            return Err(ViewError::NamespaceNotSupported {
                name: format!("{{{uri}}}{}", attr.name()),
            });
        }
    }
    // The `xml` prefix is always in scope; anything else was declared in the arch.
    for ns in node.namespaces() {
        if ns.uri() != roxmltree::NS_XML_URI {
            return Err(ViewError::NamespaceNotSupported {
                name: match ns.name() {
                    Some(prefix) => format!("xmlns:{prefix}"),
                    None => "xmlns".to_owned(),
                },
            });
        }
    }
    Ok(())
}

fn element_parts(
    node: roxmltree::Node<'_, '_>,
    limits: &ViewLimits,
) -> ViewResult<(String, Vec<(String, String)>)> {
    let count = node.attributes().len();
    let max = limits.get(LimitKind::ElementAttributes);
    if count > max {
        return Err(limits.exceeded(LimitKind::ElementAttributes, Some(count)));
    }
    check_namespaces(node)?;
    let attrs = node
        .attributes()
        .map(|a| (a.name().to_owned(), a.value().to_owned()))
        .collect();
    Ok((node.tag_name().name().to_owned(), attrs))
}

/// One open element of the conversion: its arena id and the last element child seen so far
/// (text after that child is its tail).
struct Open {
    id: NodeId,
    last_element: Option<NodeId>,
}

const LOST_OPEN_ELEMENT: ViewError = ViewError::InvariantViolation("parser lost its open element");

/// Allocates `node` as the last child of the innermost open element.
fn open_element(
    arch: &mut Arch,
    open: &mut [Open],
    node: roxmltree::Node<'_, '_>,
    limits: &ViewLimits,
    lines: &mut Lines<'_>,
) -> ViewResult<NodeId> {
    let (tag, attrs) = element_parts(node, limits)?;
    let id = arch.alloc(NodeKind::Element, tag, attrs, String::new(), String::new())?;
    arch.set_origin(id, lines.origin_at(node.range().start)?);
    let parent = open.last_mut().ok_or(LOST_OPEN_ELEMENT)?;
    arch.append_child(parent.id, id);
    parent.last_element = Some(id);
    Ok(id)
}

fn convert(
    root: roxmltree::Node<'_, '_>,
    limits: &ViewLimits,
    lines: &mut Lines<'_>,
) -> ViewResult<Arch> {
    let (tag, attrs) = element_parts(root, limits)?;
    let mut arch = Arch::with_root(tag, attrs, Caps::parsed(limits))?;
    let root_id = arch.root_id();
    arch.set_origin(root_id, lines.origin_at(root.range().start)?);
    let mut open = vec![Open {
        id: arch.root_id(),
        last_element: None,
    }];

    // Pre-order walk over roxmltree's links; `open` holds the chain of open elements, so its
    // length is the depth of the children being read.
    let mut next = root.first_child();
    while let Some(node) = next {
        let opened = read_node(&mut arch, &mut open, node, limits, lines)?;
        next = if opened {
            match node.first_child() {
                Some(child) => Some(child),
                None => {
                    open.pop();
                    climb(node, root, &mut open)
                }
            }
        } else {
            climb(node, root, &mut open)
        };
    }
    Ok(arch)
}

/// Adds `node` to the arch: an element is allocated and pushed on `open` (returns `true`), text
/// joins the text or tail it belongs to; comments and processing instructions are dropped.
fn read_node(
    arch: &mut Arch,
    open: &mut Vec<Open>,
    node: roxmltree::Node<'_, '_>,
    limits: &ViewLimits,
    lines: &mut Lines<'_>,
) -> ViewResult<bool> {
    if node.is_element() {
        let depth = open.len() + 1;
        if depth > limits.get(LimitKind::Depth) {
            return Err(limits.exceeded(LimitKind::Depth, Some(depth)));
        }
        let id = open_element(arch, open, node, limits, lines)?;
        open.push(Open {
            id,
            last_element: None,
        });
        return Ok(true);
    }
    if node.is_text()
        && let Some(text) = node.text()
    {
        let parent = open.last().ok_or(LOST_OPEN_ELEMENT)?;
        match parent.last_element {
            Some(prev) => arch.append_tail(prev, text)?,
            None => arch.append_text(parent.id, text)?,
        }
    }
    // Comments and processing instructions: nothing to keep.
    Ok(false)
}

/// The next node after `node` whose subtree is finished: its next sibling, or the next sibling of
/// the nearest ancestor that has one (closing the ancestors passed on the way).
fn climb<'a, 'i>(
    node: roxmltree::Node<'a, 'i>,
    root: roxmltree::Node<'a, 'i>,
    open: &mut Vec<Open>,
) -> Option<roxmltree::Node<'a, 'i>> {
    let mut cur = node;
    loop {
        if let Some(sibling) = cur.next_sibling() {
            return Some(sibling);
        }
        let parent = cur.parent()?;
        if parent == root {
            return None;
        }
        open.pop();
        cur = parent;
    }
}
