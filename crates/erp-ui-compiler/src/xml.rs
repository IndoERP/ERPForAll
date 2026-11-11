//! Strict XML front end (design note §6.1): roxmltree's rules, behind
//! `erp_views::check_xml_nesting`, converted into a small tree that borrows the source text.
//!
//! Whitespace follows OWL's observable result (`owl.js:5320-5326` drops a whitespace-only text node
//! holding a line break outside `<pre>`, `compileText` collapses whitespace runs to one space), with
//! one difference: only XML whitespace (space, tab, CR, LF) counts. OWL uses JavaScript's `\s`,
//! which also matches U+00A0, so a template's `&#160;` renders as an ordinary space there
//! (BUG-064); here it stays a non-breaking space.

use crate::diag::{DiagnosticKind, Position};
use crate::limits::{CompilerLimits, LimitHit, LimitKind};
use std::borrow::Cow;

/// The SVG namespace, allowed as a default namespace declared on `<svg>`.
pub(crate) const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// An element of our tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XElement<'s> {
    pub(crate) name: &'s str,
    pub(crate) attrs: Vec<XAttr<'s>>,
    pub(crate) children: Vec<XNode<'s>>,
    /// Byte offset of the start tag in the parsed text.
    pub(crate) offset: u32,
    /// In the SVG namespace.
    pub(crate) svg: bool,
}

impl<'s> XElement<'s> {
    /// The value of attribute `name`.
    pub(crate) fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name == name)
            .map(|a| &*a.value)
    }

    /// The attribute `name`.
    pub(crate) fn attr_node(&self, name: &str) -> Option<&XAttr<'s>> {
        self.attrs.iter().find(|a| a.name == name)
    }

    /// Whether the element has attribute `name`.
    pub(crate) fn has(&self, name: &str) -> bool {
        self.attrs.iter().any(|a| a.name == name)
    }
}

/// An attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XAttr<'s> {
    pub(crate) name: &'s str,
    pub(crate) value: Cow<'s, str>,
    pub(crate) offset: u32,
}

/// A child node. Comments and processing instructions are not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum XNode<'s> {
    Element(XElement<'s>),
    /// Text after the whitespace rule; never empty.
    Text {
        text: Cow<'s, str>,
        offset: u32,
    },
}

/// A problem found while reading XML: a diagnostic without its file and template, which the
/// caller adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XmlProblem {
    pub(crate) kind: DiagnosticKind,
    pub(crate) message: String,
    /// Byte offset when known.
    pub(crate) offset: Option<usize>,
    /// 1-based line and column when the parser reported them instead of an offset.
    pub(crate) line_column: Option<(u32, u32)>,
    /// The limit hit, for a [`DiagnosticKind::Limit`] problem.
    pub(crate) limit: Option<LimitHit>,
}

impl XmlProblem {
    fn at(kind: DiagnosticKind, message: String, offset: usize) -> Self {
        Self {
            kind,
            message,
            offset: Some(offset),
            line_column: None,
            limit: None,
        }
    }

    fn without_position(kind: DiagnosticKind, message: String) -> Self {
        Self {
            kind,
            message,
            offset: None,
            line_column: None,
            limit: None,
        }
    }

    fn limit(kind: LimitKind, max: usize, actual: usize, offset: Option<usize>) -> Self {
        let hit = LimitHit::new(kind, max, actual);
        Self {
            kind: DiagnosticKind::Limit,
            message: hit.message(),
            offset,
            line_column: None,
            limit: Some(hit),
        }
    }
}

/// 1-based line and column (in characters) of byte offsets in one text.
#[derive(Debug, Clone)]
pub(crate) struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { starts }
    }

    /// Line and column of `offset` in `text` (the text the index was built from).
    pub(crate) fn line_column(&self, text: &str, offset: usize) -> (u32, u32) {
        let line = self.starts.partition_point(|&s| s <= offset).max(1);
        let start = self.starts[line - 1];
        let end = offset.min(text.len());
        let column = text
            .get(start..end)
            .map_or(0, |s| s.chars().count())
            .saturating_add(1);
        (saturate(line), saturate(column))
    }

    /// The position of `offset` in `text`.
    pub(crate) fn position(&self, text: &str, offset: usize) -> Option<Position> {
        let (line, column) = self.line_column(text, offset);
        Position::new(line, column)
    }
}

fn saturate(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn offset32(n: usize) -> u32 {
    // Files are bounded by `file_bytes` (4 MiB) before parsing, so offsets fit.
    saturate(n)
}

/// One top-level child of `<templates>`: the element, and its source range (used to hand the
/// template to the inheritance engine without re-serialising it).
#[derive(Debug, Clone)]
pub(crate) struct TopLevel<'s> {
    pub(crate) element: XElement<'s>,
    /// Source range of the whole element.
    pub(crate) range: std::ops::Range<usize>,
    /// Source range of its content (between the start and end tags; empty when self-closing).
    pub(crate) inner: std::ops::Range<usize>,
}

/// Parses a template file.
///
/// `templates_before` is the number of templates the files before this one hold: the file's
/// top-level elements are counted, and the `templates` limit checked, before any tree is built
/// (design note §9).
pub(crate) fn parse_templates_file<'s>(
    xml: &'s str,
    limits: &CompilerLimits,
    templates_before: usize,
) -> Result<Vec<TopLevel<'s>>, XmlProblem> {
    let doc = parse_document(xml, limits)?;
    let root = doc.root_element();
    check_namespace_scope(root)?;
    let root_name = root.tag_name().name();
    let is_container = root.tag_name().namespace().is_none()
        && matches!(root_name, "templates" | "template" | "odoo");
    let count = if is_container {
        root.children().filter(roxmltree::Node::is_element).count()
    } else {
        1
    };
    let max_templates = limits.get(LimitKind::Templates);
    let total = templates_before.saturating_add(count);
    if total > max_templates {
        return Err(XmlProblem::limit(
            LimitKind::Templates,
            max_templates,
            total,
            None,
        ));
    }
    if !is_container {
        return Ok(vec![top_level(root, limits)?]);
    }
    for attr in root.attributes() {
        let allowed = match attr.namespace() {
            Some(roxmltree::NS_XML_URI) => attr.name() == "space",
            Some(_) => false,
            None => attr.name() == "id",
        };
        if !allowed {
            return Err(XmlProblem::at(
                DiagnosticKind::FileShape,
                format!(
                    "attribute '{}' is not allowed on <{root_name}> (only xml:space and id)",
                    qualified_attr_name(&attr)
                ),
                attr.range().start,
            ));
        }
    }
    let mut out = Vec::new();
    for child in root.children() {
        if child.is_element() {
            out.push(top_level(child, limits)?);
        } else if child.is_text() {
            let text = child.text().unwrap_or("");
            if !text.chars().all(is_xml_space) {
                return Err(XmlProblem::at(
                    DiagnosticKind::FileShape,
                    format!("text is not allowed directly under <{root_name}>"),
                    child.range().start,
                ));
            }
        } else if child.is_pi() {
            return Err(XmlProblem::at(
                DiagnosticKind::Xml,
                "processing instructions are not allowed in template files".to_owned(),
                child.range().start,
            ));
        }
        // Comments are dropped.
    }
    Ok(out)
}

fn top_level<'s>(
    node: roxmltree::Node<'_, 's>,
    limits: &CompilerLimits,
) -> Result<TopLevel<'s>, XmlProblem> {
    let element = convert_element(node, limits, false)?;
    let range = node.range();
    let inner = match (node.first_child(), node.last_child()) {
        (Some(first), Some(last)) => first.range().start..last.range().end,
        _ => range.end..range.end,
    };
    Ok(TopLevel {
        element,
        range,
        inner,
    })
}

/// Parses one element document: a combined template from the inheritance engine.
pub(crate) fn parse_element<'s>(
    xml: &'s str,
    limits: &CompilerLimits,
) -> Result<XElement<'s>, XmlProblem> {
    let doc = parse_document(xml, limits)?;
    let root = doc.root_element();
    convert_element(root, limits, false)
}

pub(crate) fn parse_document<'s>(
    xml: &'s str,
    limits: &CompilerLimits,
) -> Result<roxmltree::Document<'s>, XmlProblem> {
    let max_bytes = limits.get(LimitKind::FileBytes);
    if xml.len() > max_bytes {
        return Err(XmlProblem::limit(
            LimitKind::FileBytes,
            max_bytes,
            xml.len(),
            None,
        ));
    }
    let depth = limits.get(LimitKind::Depth);
    let view_limits = erp_views::ViewLimits::default()
        .lowered(erp_views::LimitKind::Depth, depth)
        .map_err(|e| {
            XmlProblem::without_position(
                DiagnosticKind::Internal,
                format!(
                    "internal compiler error: depth limit {depth} is not accepted by the inheritance engine: {e}"
                ),
            )
        })?;
    erp_views::check_xml_nesting(xml, &view_limits).map_err(|e| match e {
        erp_views::ViewError::LimitExceeded { max, actual, .. } => {
            XmlProblem::limit(LimitKind::Depth, max, actual.unwrap_or(max + 1), None)
        }
        erp_views::ViewError::XmlSyntax {
            message,
            line,
            column,
        } => XmlProblem {
            line_column: Some((line, column)),
            ..XmlProblem::without_position(DiagnosticKind::Xml, message)
        },
        other => XmlProblem::without_position(DiagnosticKind::Xml, other.to_string()),
    })?;
    let max_nodes = limits.get(LimitKind::FileNodes);
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: u32::try_from(max_nodes).unwrap_or(u32::MAX),
        entity_resolver: None,
    };
    roxmltree::Document::parse_with_options(xml, options).map_err(|e| match e {
        roxmltree::Error::NodesLimitReached => XmlProblem::limit(
            LimitKind::FileNodes,
            max_nodes,
            max_nodes.saturating_add(1),
            None,
        ),
        roxmltree::Error::DtdDetected => XmlProblem::without_position(
            DiagnosticKind::Xml,
            "DOCTYPE declarations are not allowed in template files".to_owned(),
        ),
        other => {
            let pos = other.pos();
            XmlProblem {
                line_column: Some((pos.row, pos.col)),
                ..XmlProblem::without_position(DiagnosticKind::Xml, other.to_string())
            }
        }
    })
}

/// Whether `c` is XML whitespace (the `S` production).
pub(crate) fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

fn qualified_attr_name(attr: &roxmltree::Attribute<'_, '_>) -> String {
    match attr.namespace() {
        Some(roxmltree::NS_XML_URI) => format!("xml:{}", attr.name()),
        Some(uri) => format!("{{{uri}}}{}", attr.name()),
        None => attr.name().to_owned(),
    }
}

/// Every namespace in scope is `xml`, the SVG default namespace or the unused XLink prefix.
fn check_namespace_scope(node: roxmltree::Node<'_, '_>) -> Result<(), XmlProblem> {
    for ns in node.namespaces() {
        let ok = matches!(
            (ns.name(), ns.uri()),
            (Some("xml"), roxmltree::NS_XML_URI) | (None, SVG_NS) | (Some("xlink"), XLINK_NS)
        );
        if !ok {
            let decl = match ns.name() {
                Some(prefix) => format!("xmlns:{prefix}=\"{}\"", ns.uri()),
                None => format!("xmlns=\"{}\"", ns.uri()),
            };
            return Err(XmlProblem::at(
                DiagnosticKind::Xml,
                format!(
                    "namespace declaration {decl} is not allowed (only the SVG default namespace on <svg>)"
                ),
                node.range().start,
            ));
        }
    }
    Ok(())
}

/// One open element of the conversion.
struct Frame<'s> {
    element: XElement<'s>,
    in_pre: bool,
}

/// Converts `root` and its subtree without recursion.
fn convert_element<'s>(
    root: roxmltree::Node<'_, 's>,
    limits: &CompilerLimits,
    in_pre: bool,
) -> Result<XElement<'s>, XmlProblem> {
    let mut stack: Vec<(Frame<'s>, Option<roxmltree::Node<'_, 's>>)> = Vec::new();
    let first = open(root, limits, in_pre, false)?;
    stack.push((first, root.first_child()));
    loop {
        let Some((frame, next)) = stack.last_mut() else {
            return Err(lost_frame());
        };
        let (parent_pre, parent_svg) = (frame.in_pre, frame.element.svg);
        let Some(node) = *next else {
            let Some((done, _)) = stack.pop() else {
                return Err(lost_frame());
            };
            match stack.last_mut() {
                Some((parent, _)) => parent.element.children.push(XNode::Element(done.element)),
                None => return Ok(done.element),
            }
            continue;
        };
        *next = node.next_sibling();
        if node.is_element() {
            let child = open(node, limits, parent_pre, parent_svg)?;
            stack.push((child, node.first_child()));
        } else if node.is_text() {
            let raw: Cow<'s, str> = match node.text_storage() {
                Some(roxmltree::StringStorage::Borrowed(s)) => Cow::Borrowed(*s),
                Some(roxmltree::StringStorage::Owned(s)) => Cow::Owned(s.to_string()),
                None => Cow::Borrowed(""),
            };
            let offset = offset32(node.range().start);
            if let Some(text) = normalize_text(raw, parent_pre) {
                frame.element.children.push(XNode::Text { text, offset });
            }
        } else if node.is_pi() {
            return Err(XmlProblem::at(
                DiagnosticKind::Xml,
                "processing instructions are not allowed in templates".to_owned(),
                node.range().start,
            ));
        }
        // Comments are dropped (OWL ignores them too, `owl.js:5320-5330`).
    }
}

fn lost_frame() -> XmlProblem {
    XmlProblem::without_position(
        DiagnosticKind::Internal,
        "internal compiler error: XML conversion lost its open element".to_owned(),
    )
}

fn open<'s>(
    node: roxmltree::Node<'_, 's>,
    limits: &CompilerLimits,
    parent_in_pre: bool,
    parent_svg: bool,
) -> Result<Frame<'s>, XmlProblem> {
    let start = node.range().start;
    check_namespace_scope(node)?;
    let name = node.tag_name().name();
    // SVG-ness is decided here, once (codegen reads only `Element.svg`): an `<svg>` element and
    // everything inside it, whether or not it declares the SVG namespace, because the HTML parser
    // and OWL both switch to SVG on the tag (security review S1/S6).
    let svg = match node.tag_name().namespace() {
        None => name == "svg" || parent_svg,
        Some(SVG_NS) if name == "svg" || parent_svg => true,
        Some(SVG_NS) => {
            return Err(XmlProblem::at(
                DiagnosticKind::Xml,
                format!("the SVG namespace may only be declared on <svg>, found it on <{name}>"),
                start,
            ));
        }
        Some(uri) => {
            return Err(XmlProblem::at(
                DiagnosticKind::Xml,
                format!("namespaced element {{{uri}}}{name} is not allowed"),
                start,
            ));
        }
    };
    let count = node.attributes().len();
    let max = limits.get(LimitKind::Attributes);
    if count > max {
        return Err(XmlProblem::limit(
            LimitKind::Attributes,
            max,
            count,
            Some(start),
        ));
    }
    let mut attrs = Vec::with_capacity(count);
    for attr in node.attributes() {
        if attr.namespace().is_some() {
            return Err(XmlProblem::at(
                DiagnosticKind::Xml,
                format!(
                    "namespaced attribute '{}' is not allowed on <{name}>",
                    qualified_attr_name(&attr)
                ),
                attr.range().start,
            ));
        }
        let value: Cow<'s, str> = match attr.value_storage() {
            roxmltree::StringStorage::Borrowed(s) => Cow::Borrowed(*s),
            roxmltree::StringStorage::Owned(s) => Cow::Owned(s.to_string()),
        };
        attrs.push(XAttr {
            name: attr.name(),
            value,
            offset: offset32(attr.range().start),
        });
    }
    Ok(Frame {
        element: XElement {
            name,
            attrs,
            children: Vec::new(),
            offset: offset32(start),
            svg,
        },
        in_pre: parent_in_pre || (name == "pre" && !svg),
    })
}

/// OWL's text rule with XML whitespace: `None` when the node is dropped.
fn normalize_text(raw: Cow<'_, str>, in_pre: bool) -> Option<Cow<'_, str>> {
    if raw.is_empty() {
        return None;
    }
    if in_pre {
        return Some(raw);
    }
    let blank = raw.chars().all(is_xml_space);
    if blank && raw.contains(['\r', '\n']) {
        return None;
    }
    let needs_collapse = {
        let mut prev_space = false;
        let mut needs = false;
        for c in raw.chars() {
            let space = is_xml_space(c);
            if space && (prev_space || c != ' ') {
                needs = true;
                break;
            }
            prev_space = space;
        }
        needs
    };
    if !needs_collapse {
        return Some(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let mut prev_space = false;
    for c in raw.chars() {
        if is_xml_space(c) {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    Some(Cow::Owned(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(xml: &str) -> Result<Vec<TopLevel<'_>>, XmlProblem> {
        parse_templates_file(xml, &CompilerLimits::DEFAULT, 0)
    }

    #[test]
    fn whitespace_follows_owl_with_xml_spaces_only() {
        let xml = "<templates><t t-name=\"a\">\n  <div>  a \n b  </div>\n <pre>\n x  y\n</pre>&#160;<b> </b></t></templates>";
        let tops = file(xml).unwrap();
        let t = &tops[0].element;
        // the line-break-only text nodes are dropped; " " inside <b> is kept
        let XNode::Element(div) = &t.children[0] else {
            panic!()
        };
        let XNode::Text { text, .. } = &div.children[0] else {
            panic!()
        };
        assert_eq!(text, " a b ");
        let XNode::Element(pre) = &t.children[1] else {
            panic!()
        };
        let XNode::Text { text, .. } = &pre.children[0] else {
            panic!()
        };
        assert_eq!(text, "\n x  y\n");
        let XNode::Text { text, .. } = &t.children[2] else {
            panic!()
        };
        assert_eq!(
            text, "\u{a0}",
            "BUG-064: a non-breaking space is not collapsed"
        );
        let XNode::Element(b) = &t.children[3] else {
            panic!()
        };
        let XNode::Text { text, .. } = &b.children[0] else {
            panic!()
        };
        assert_eq!(text, " ");
    }

    #[test]
    fn file_shape_and_namespaces() {
        assert_eq!(
            file("<template><t t-name=\"a\"/><t t-name=\"b\"/></template>")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(file("<odoo><t t-name=\"a\"/></odoo>").unwrap().len(), 1);
        let single = file("<t t-name=\"a\"><b/></t>").unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].element.name, "t");
        assert_eq!(
            file("<template>x</template>").unwrap_err().kind,
            DiagnosticKind::FileShape
        );
        assert_eq!(
            file("<templates>x<t/></templates>").unwrap_err().kind,
            DiagnosticKind::FileShape
        );
        assert!(
            file("<templates xml:space=\"preserve\" id=\"template\"><!-- c --><t/></templates>")
                .is_ok()
        );
        assert_eq!(
            file("<templates foo=\"1\"/>").unwrap_err().kind,
            DiagnosticKind::FileShape
        );
        let svg = "<templates><t t-name=\"a\"><svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"><path d=\"M0\"/></svg></t></templates>";
        let tops = file(svg).unwrap();
        let XNode::Element(s) = &tops[0].element.children[0] else {
            panic!()
        };
        assert!(s.svg);
        let XNode::Element(p) = &s.children[0] else {
            panic!()
        };
        assert!(p.svg);
        for bad in [
            "<templates><t xmlns:x=\"urn:x\"/></templates>",
            "<templates><svg xmlns:xlink=\"http://www.w3.org/1999/xlink\"><use xlink:href=\"#a\"/></svg></templates>",
            "<templates><g xmlns=\"http://www.w3.org/2000/svg\"/></templates>",
            "<templates><t><?pi x?></t></templates>",
            "<!DOCTYPE templates><templates/>",
            "<templates><t a=\"&#xD800;\"/></templates>",
        ] {
            assert_eq!(file(bad).unwrap_err().kind, DiagnosticKind::Xml, "{bad}");
        }
    }

    #[test]
    fn limits_are_errors() {
        let limits = CompilerLimits::DEFAULT
            .lowered(LimitKind::Depth, 3)
            .unwrap();
        let err = parse_templates_file("<templates><t><a><b></b></a></t></templates>", &limits, 0)
            .unwrap_err();
        assert_eq!(err.kind, DiagnosticKind::Limit);
        let limits = CompilerLimits::DEFAULT
            .lowered(LimitKind::Attributes, 1)
            .unwrap();
        let err = parse_templates_file("<templates><t a=\"1\" b=\"2\"/></templates>", &limits, 0)
            .unwrap_err();
        assert_eq!(err.message, "attributes limit exceeded: maximum 1, got 2");
        let limits = CompilerLimits::DEFAULT
            .lowered(LimitKind::FileBytes, 10)
            .unwrap();
        assert_eq!(
            parse_templates_file("<templates/>  ", &limits, 0)
                .unwrap_err()
                .kind,
            DiagnosticKind::Limit
        );
        let limits = CompilerLimits::DEFAULT
            .lowered(LimitKind::FileNodes, 3)
            .unwrap();
        assert_eq!(
            parse_templates_file("<templates><a/><b/><c/></templates>", &limits, 0)
                .unwrap_err()
                .kind,
            DiagnosticKind::Limit
        );
        // The template count is checked before trees are built, with the files before counted.
        let limits = CompilerLimits::DEFAULT
            .lowered(LimitKind::Templates, 3)
            .unwrap();
        assert!(parse_templates_file("<templates><a/><b/></templates>", &limits, 1).is_ok());
        let err = parse_templates_file("<templates><a/><b/></templates>", &limits, 2).unwrap_err();
        assert_eq!(err.limit, Some(LimitHit::new(LimitKind::Templates, 3, 4)));
        assert_eq!(err.message, "templates limit exceeded: maximum 3, got 4");
    }

    #[test]
    fn line_index() {
        let text = "ab\ncdé\nf";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_column(text, 0), (1, 1));
        assert_eq!(idx.line_column(text, 3), (2, 1));
        assert_eq!(idx.line_column(text, 7), (2, 4));
        assert_eq!(idx.line_column(text, 8), (3, 1));
        assert_eq!(idx.line_column(text, 9), (3, 2));
    }

    #[test]
    fn ranges_cover_the_template() {
        let xml = "<templates><t t-name=\"a\"><b/>x</t><t t-name=\"e\"/></templates>";
        let tops = file(xml).unwrap();
        assert_eq!(&xml[tops[0].range.clone()], "<t t-name=\"a\"><b/>x</t>");
        assert_eq!(&xml[tops[0].inner.clone()], "<b/>x");
        assert_eq!(&xml[tops[1].inner.clone()], "");
    }

    /// Security review L-f: the depth pre-scan (erp-views' `check_xml_nesting`) and roxmltree
    /// agree on element depth, also with CDATA sections, comments, processing-instruction-like
    /// text and attribute values that look like tags. The depth is the nesting of elements with
    /// content markup (a start and an end tag): roxmltree recurses once per such level and never
    /// for a self-closing tag, so the pre-scan does not count those.
    mod nesting_agrees_with_roxmltree {
        use proptest::prelude::*;

        fn tree() -> impl Strategy<Value = String> {
            let leaf = prop_oneof![
                Just("x".to_owned()),
                Just("<![CDATA[<a><a><a></a>]]>".to_owned()),
                Just("<!-- <a><a> -->".to_owned()),
                Just("<b x=\"&lt;a&gt;\" y='/>'/>".to_owned()),
                Just("&lt;a&gt;".to_owned()),
                Just("<c z=\">\"></c>".to_owned()),
            ];
            leaf.prop_recursive(10, 64, 3, |inner| {
                proptest::collection::vec(inner, 0..3)
                    .prop_map(|children| format!("<a q=\"-->]]>\">{}</a>", children.concat()))
            })
        }

        fn rox_depth(xml: &str) -> Option<usize> {
            let doc = roxmltree::Document::parse(xml).ok()?;
            doc.descendants()
                .filter(|n| n.is_element() && !xml[n.range()].ends_with("/>"))
                .map(|n| n.ancestors().filter(roxmltree::Node::is_element).count())
                .max()
        }

        proptest! {
            #[test]
            fn depth_judgements_match(body in tree()) {
                let xml = format!("<r>{body}</r>");
                let depth = rox_depth(&xml).expect("the generator writes well-formed XML");
                for max in [2usize, 3, 5, 8, 12] {
                    let limits = erp_views::ViewLimits::default()
                        .lowered(erp_views::LimitKind::Depth, max)
                        .unwrap();
                    let accepted = erp_views::check_xml_nesting(&xml, &limits).is_ok();
                    prop_assert_eq!(accepted, depth <= max, "max {} depth {} in {}", max, depth, xml);
                }
            }
        }
    }
}
