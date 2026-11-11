//! Parsing untrusted archs: DTDs, entities, characters, namespaces, size, element count, depth;
//! serialization escaping and round trips.

use super::{arch, on_small_stack};
use crate::arch::{Arch, Caps, NodeId, NodeKind};
use crate::error::{LimitKind, ViewError};
use crate::limits::ViewLimits;
use crate::parse::parse_arch;
use proptest::prelude::*;

fn parse(xml: &str) -> Result<Arch, ViewError> {
    parse_arch(xml, &ViewLimits::default(), 1)
}

fn syntax(message: &str, line: u32, column: u32) -> ViewError {
    ViewError::XmlSyntax {
        message: message.to_owned(),
        line,
        column,
    }
}

#[test]
fn empty_arch() {
    assert_eq!(parse(""), Err(ViewError::EmptyArch));
    assert_eq!(parse(" \n\t "), Err(ViewError::EmptyArch));
}

#[test]
fn doctype_and_entities_are_rejected() {
    let doctype = r#"<!DOCTYPE form [ <!ENTITY xxe SYSTEM "file:///etc/passwd"> ]><form><field name="&xxe;"/></form>"#;
    let err = parse(doctype).unwrap_err();
    assert_eq!(err, ViewError::DoctypeNotAllowed);
    assert_eq!(
        err.to_string(),
        "DOCTYPE declarations are not allowed in view archs"
    );
    // No DOCTYPE, undeclared entity: a syntax error, never an empty or literal value.
    assert_eq!(
        parse(r#"<form string="&nbsp;"/>"#),
        Err(syntax("unknown entity reference 'nbsp' at 1:15", 1, 15))
    );
    assert_eq!(
        parse("<form>&custom;</form>"),
        Err(syntax("unknown entity reference 'custom' at 1:7", 1, 7))
    );
    // Predefined and numeric references are resolved.
    assert_eq!(
        arch(r#"<form string="&lt;&amp;&#233;&#x4E2D;">&quot;&apos;</form>"#).to_xml(),
        r#"<form string="&lt;&amp;é中">"'</form>"#
    );
}

#[test]
fn invalid_characters_are_rejected() {
    // roxmltree reports a reference to a non-XML character as a malformed reference.
    assert_eq!(
        parse(r#"<form string="a&#0;b"/>"#),
        Err(syntax("malformed entity reference at 1:16", 1, 16))
    );
    assert_eq!(
        parse("<form>&#x0;</form>"),
        Err(syntax("malformed entity reference at 1:7", 1, 7))
    );
    assert_eq!(
        parse("<form>&#00000;</form>"),
        Err(syntax("malformed entity reference at 1:7", 1, 7))
    );
    assert_eq!(
        parse("<form>a\0b</form>"),
        Err(syntax("a non-XML character '\\0' found at 1:8", 1, 8))
    );
    assert_eq!(
        parse("<form>\u{1}</form>"),
        Err(syntax("a non-XML character '\\u{1}' found at 1:7", 1, 7))
    );
    // roxmltree would turn these into U+FFFD; lxml rejects them, so do we.
    assert_eq!(
        parse("<form>\n  é&#xD800;</form>"),
        Err(syntax(
            "invalid character reference '&#xD800;' at 2:4",
            2,
            4
        ))
    );
    assert_eq!(
        parse(r#"<form string="&#1114112;"/>"#),
        Err(syntax(
            "invalid character reference '&#1114112;' at 1:15",
            1,
            15
        ))
    );
    // A literal "&#xD800;" inside a comment or CDATA section is text, not a reference.
    assert_eq!(
        arch("<form><!-- &#xD800; --><![CDATA[&#xD800;]]></form>").to_xml(),
        "<form>&amp;#xD800;</form>"
    );
}

#[test]
fn duplicate_attributes_are_rejected() {
    assert_eq!(
        parse(r#"<form string="A" string="B"/>"#),
        Err(syntax(
            "attribute 'string' at 1:18 is already defined",
            1,
            18
        ))
    );
}

#[test]
fn namespaces_are_rejected() {
    assert_eq!(
        parse(r#"<form xmlns="urn:x"/>"#),
        Err(ViewError::NamespaceNotSupported {
            name: "{urn:x}form".to_owned()
        })
    );
    assert_eq!(
        parse(r#"<form xmlns:t="urn:t"><field/></form>"#),
        Err(ViewError::NamespaceNotSupported {
            name: "xmlns:t".to_owned()
        })
    );
    let err = parse(r#"<form><field xml:lang="en"/></form>"#).unwrap_err();
    assert_eq!(
        err,
        ViewError::NamespaceNotSupported {
            name: "{http://www.w3.org/XML/1998/namespace}lang".to_owned()
        }
    );
    assert_eq!(
        err.to_string(),
        "XML namespaces are not supported in view archs: '{http://www.w3.org/XML/1998/namespace}lang'"
    );
}

#[test]
fn comments_and_processing_instructions_are_dropped() {
    assert_eq!(
        arch("<form>a<!-- c -->b<?pi x?><field/><!-- d -->tail</form>").to_xml(),
        "<form>ab<field/>tail</form>"
    );
}

#[test]
fn crlf_cdata_and_non_ascii() {
    assert_eq!(
        arch("<form>\r\n<field name=\"ção\"/>\r\n</form>").to_xml(),
        "<form>\n<field name=\"ção\"/>\n</form>"
    );
    assert_eq!(
        arch("<form><![CDATA[a<b]]>c</form>").to_xml(),
        "<form>a&lt;bc</form>"
    );
    assert_eq!(
        arch("<表单 名=\"值\">文本</表单>").to_xml(),
        "<表单 名=\"值\">文本</表单>"
    );
}

#[test]
fn serializer_escapes_text_and_attributes() {
    let xml = r#"<a b="&amp;&lt;&gt;&quot;&#10;&#9;&#13;'">&amp;&lt;&gt;&#13;"'<c/>&amp;</a>"#;
    let parsed = arch(xml);
    assert_eq!(parsed.root().attr("b"), Some("&<>\"\n\t\r'"));
    assert_eq!(parsed.root().text(), "&<>\r\"'");
    assert_eq!(
        parsed.to_xml(),
        r#"<a b="&amp;&lt;&gt;&quot;&#10;&#9;&#13;'">&amp;&lt;&gt;&#13;"'<c/>&amp;</a>"#
    );
}

#[test]
fn size_limit() {
    let limits = ViewLimits::default()
        .lowered(LimitKind::ArchBytes, 64)
        .unwrap();
    let xml = format!("<form>{}</form>", "x".repeat(60));
    assert_eq!(
        parse_arch(&xml, &limits, 1),
        Err(ViewError::LimitExceeded {
            limit: LimitKind::ArchBytes,
            max: 64,
            actual: Some(73)
        })
    );
    let big = format!("<form>{}</form>", "x".repeat(10 * 1024 * 1024));
    assert_eq!(
        parse(&big),
        Err(ViewError::LimitExceeded {
            limit: LimitKind::ArchBytes,
            max: 10 * 1024 * 1024,
            actual: Some(10 * 1024 * 1024 + 13)
        })
    );
}

#[test]
fn element_count_limit() {
    let ok = format!("<form>{}</form>", "<f/>".repeat(9_999));
    assert!(parse(&ok).is_ok());
    let over = format!("<form>{}</form>", "x<f/>".repeat(10_000));
    let err = parse(&over).unwrap_err();
    assert_eq!(
        err,
        ViewError::LimitExceeded {
            limit: LimitKind::ArchElements,
            max: 10_000,
            actual: Some(10_001)
        }
    );
    assert_eq!(
        err.to_string(),
        "elements per arch limit exceeded: maximum 10000, reached 10001"
    );
    // Text and comment nodes hit the XML node limit, not the element limit.
    let texts = format!("<form>{}</form>", "<!--c-->x".repeat(30_000));
    assert_eq!(
        parse(&texts),
        Err(ViewError::LimitExceeded {
            limit: LimitKind::ArchNodes,
            max: 40_000,
            actual: Some(40_001)
        })
    );
}

/// The parser counts the document node, elements, text, comments and processing instructions:
/// a comment-heavy arch far under the element limit reports the node limit by its own name.
#[test]
fn comment_heavy_arch_reports_the_node_limit() {
    // Document + <form> + 9,999 elements + 29,999 comments = 40,000 nodes: the maximum.
    let at = format!(
        "<form>{}{}</form>",
        "<f/>".repeat(9_999),
        "<!--c-->".repeat(29_999)
    );
    assert!(parse(&at).is_ok());
    // One comment more, or 5,001 elements with 35,000 comments and processing instructions.
    let over = format!(
        "<form>{}{}</form>",
        "<f/>".repeat(9_999),
        "<!--c-->".repeat(30_000)
    );
    let sparse = format!(
        "<form>{}</form>",
        "<f/><!--c--><?p?><!--c--><?p?><!--c--><?p?><!--c-->".repeat(5_000)
    );
    for xml in [&over, &sparse] {
        let err = parse(xml).unwrap_err();
        assert_eq!(
            err,
            ViewError::LimitExceeded {
                limit: LimitKind::ArchNodes,
                max: 40_000,
                actual: Some(40_001)
            }
        );
        assert_eq!(
            err.to_string(),
            "XML nodes per arch limit exceeded: maximum 40000, reached 40001"
        );
    }
    // The host can lower it on its own.
    let limits = ViewLimits::default()
        .lowered(LimitKind::ArchNodes, 4)
        .unwrap();
    assert!(parse_arch("<form><a/><!--c--></form>", &limits, 1).is_ok());
    assert_eq!(
        parse_arch("<form><a/><!--c-->x</form>", &limits, 1),
        Err(ViewError::LimitExceeded {
            limit: LimitKind::ArchNodes,
            max: 4,
            actual: Some(5)
        })
    );
}

/// One element may carry at most 256 attributes: attribute lookups scan them, so the count is
/// bounded where they are parsed (roxmltree's node limit does not count attributes).
#[test]
fn element_attribute_limit() {
    let attrs = |n: usize| -> String { (0..n).map(|k| format!(r#" a{k}="1""#)).collect() };
    assert!(parse(&format!("<form><f{}/></form>", attrs(256))).is_ok());
    for xml in [
        format!("<form><f{}/></form>", attrs(257)),
        format!("<form{}/>", attrs(257)),
    ] {
        let err = parse(&xml).unwrap_err();
        assert_eq!(
            err,
            ViewError::LimitExceeded {
                limit: LimitKind::ElementAttributes,
                max: 256,
                actual: Some(257)
            }
        );
        assert_eq!(
            err.to_string(),
            "attributes per element limit exceeded: maximum 256, reached 257"
        );
    }
    let lowered = ViewLimits::default()
        .lowered(LimitKind::ElementAttributes, 2)
        .unwrap();
    assert_eq!(
        parse_arch(r#"<form><f a="1" b="2" c="3"/></form>"#, &lowered, 1),
        Err(ViewError::LimitExceeded {
            limit: LimitKind::ElementAttributes,
            max: 2,
            actual: Some(3)
        })
    );
}

#[test]
fn depth_limit() {
    let at = format!("{}{}", "<d>".repeat(256), "</d>".repeat(256));
    assert_eq!(parse(&at).map(|a| a.max_depth()), Ok(256));
    let over = format!("{}{}", "<d>".repeat(300), "</d>".repeat(300));
    assert_eq!(
        parse(&over).map(|a| a.max_depth()),
        Err(ViewError::LimitExceeded {
            limit: LimitKind::Depth,
            max: 256,
            actual: Some(257)
        })
    );
}

/// roxmltree recurses per element level, so the depth limit is enforced by a pre-scan before it
/// runs: a bomb is rejected without any parser recursion. Comments, CDATA, processing
/// instructions and quoted `>`/`/>` do not fool the count.
#[test]
fn depth_bomb_fails_cleanly_on_a_small_stack() {
    let bomb = format!("{}{}", "<d>".repeat(1_000_000), "</d>".repeat(1_000_000));
    let tricky = format!(
        "{}{}",
        r#"<d a="/>" b='>-->'><!-- <e> --><![CDATA[<e>]]><?pi <e>?><e/>"#.repeat(300),
        "</d>".repeat(300)
    );
    let (bomb, tricky) = on_small_stack(move || {
        (
            parse(&bomb).map(|a| a.max_depth()),
            parse(&tricky).map(|a| a.max_depth()),
        )
    });
    let exceeded = Err(ViewError::LimitExceeded {
        limit: LimitKind::Depth,
        max: 256,
        actual: Some(257),
    });
    assert_eq!(bomb, exceeded);
    assert_eq!(tricky, exceeded);
}

/// The deepest allowed arch makes roxmltree recurse 256 levels: measured to need more than
/// 256 KiB but less than 512 KiB of stack in a debug build (far less in release), well inside
/// the 2 MiB default of a spawned thread.
#[test]
fn deepest_allowed_arch_parses_within_half_a_mebibyte_of_stack() {
    let deepest = format!("{}{}", "<d x=\"1\">".repeat(256), "</d>".repeat(256));
    let depth = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || parse(&deepest).map(|a| a.max_depth()))
        .expect("spawn")
        .join()
        .expect("no overflow");
    assert_eq!(depth, Ok(256));
}

#[test]
fn limits_can_only_be_lowered() {
    let limits = ViewLimits::default();
    assert_eq!(
        limits.lowered(LimitKind::Depth, 0),
        Err(ViewError::InvalidLimit {
            limit: LimitKind::Depth,
            value: 0,
            max: 256
        })
    );
    let err = limits
        .lowered(LimitKind::CombinedNodes, 200_001)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid value 200001 for the nodes in a combined arch limit: allowed 1 to 200000"
    );
    assert_eq!(
        limits
            .lowered(LimitKind::Depth, 8)
            .map(|l| l.get(LimitKind::Depth)),
        Ok(8)
    );
}

// --- round trip ---------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TestNode {
    tag: String,
    attrs: Vec<(String, String)>,
    text: String,
    tail: String,
    children: Vec<TestNode>,
}

fn content() -> impl Strategy<Value = String> {
    // XML-valid characters, the escaped ones, whitespace, CR and non-ASCII.
    "[a-zA-Z0-9 &<>\"'=/\t\n\r\u{e9}\u{4e2d}\u{1F600}]{0,12}"
}

fn node() -> impl Strategy<Value = TestNode> {
    let leaf = (
        "[a-z][a-z0-9_-]{0,5}",
        prop::collection::btree_map("[a-z]{1,4}", content(), 0..3),
        content(),
        content(),
    )
        .prop_map(|(tag, attrs, text, tail)| TestNode {
            tag,
            attrs: attrs.into_iter().collect(),
            text,
            tail,
            children: Vec::new(),
        });
    leaf.prop_recursive(4, 24, 4, |inner| {
        (inner.clone(), prop::collection::vec(inner, 0..4)).prop_map(|(mut n, children)| {
            n.children = children;
            n
        })
    })
}

fn build(arch: &mut Arch, parent: NodeId, node: &TestNode) {
    let id = arch
        .alloc(
            NodeKind::Element,
            node.tag.clone(),
            node.attrs.clone(),
            node.text.clone(),
            node.tail.clone(),
        )
        .unwrap();
    arch.append_child(parent, id);
    for child in &node.children {
        build(arch, id, child);
    }
}

proptest! {
    #[test]
    fn serialize_parse_round_trip(root in node()) {
        let limits = ViewLimits::default();
        let mut tree = Arch::with_root(root.tag.clone(), root.attrs.clone(), Caps::parsed(&limits)).unwrap();
        let top = tree.root_id();
        tree.set_text(top, root.text.clone()).unwrap();
        for child in &root.children {
            build(&mut tree, top, child);
        }
        let xml = tree.to_xml();
        let reparsed = parse(&xml).unwrap();
        prop_assert_eq!(reparsed.to_xml(), xml);
        // and the values read back are the values written
        prop_assert_eq!(reparsed.root().text(), root.text.as_str());
        let attrs: Vec<(String, String)> = reparsed
            .root()
            .attributes()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        prop_assert_eq!(attrs, root.attrs.clone());
        let tails: Vec<&str> = reparsed.root().children().map(|c| c.tail()).collect();
        let expected: Vec<&str> = root.children.iter().map(|c| c.tail.as_str()).collect();
        prop_assert_eq!(tails, expected);
    }
}

/// `root_tag` reads the first element's name without parsing the arch (review R-8): it skips
/// the prolog (BOM, XML declaration, processing instructions, comments, whitespace) and gives
/// `None` where the full parse would not reach a plain root name, so the check after the insert
/// reports the error with the view's name.
#[test]
fn root_tag_reads_only_the_first_element() {
    use crate::parse::root_tag;
    assert_eq!(root_tag("<form/>"), Some("form"));
    assert_eq!(
        root_tag("<list string='x'><field name='a'/></list>"),
        Some("list")
    );
    assert_eq!(
        root_tag("\u{feff}<?xml version='1.0'?>\n<!-- c -->\n<?pi x?>\t<kanban>"),
        Some("kanban")
    );
    // the body is not read: an unclosed element after the root still has its root tag
    assert_eq!(root_tag("<form><sheet></form>"), Some("form"));
    assert_eq!(root_tag("<search\n>"), Some("search"));
    for no_root in [
        "",
        "  ",
        "text<form/>",
        "<!-- unterminated <form/>",
        "<?xml version='1.0'",
        "<!DOCTYPE form><form/>",
        "</form>",
        "<>",
        "< form/>",
        "<1form/>",
        // a prefixed name is refused by the parse (namespaces are not supported)
        "<a:form xmlns:a='u'/>",
    ] {
        assert_eq!(root_tag(no_root), None, "{no_root:?}");
    }
    // wherever the full parse succeeds, the tags agree
    for xml in [
        "<form/>",
        "<?xml version='1.0'?><calendar date_start='d'/>",
        "<!--a--><!--b--><graph><field name='x'/></graph>",
        "<qweb_like_t\u{e9}g/>",
    ] {
        let parsed = parse(xml).unwrap();
        assert_eq!(root_tag(xml), Some(parsed.tag(parsed.root_id())), "{xml:?}");
    }
}
