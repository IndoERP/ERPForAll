// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
//! The report compiler and evaluator: templates in, a `DocTree` out. Every expected string is
//! compared with what the evaluator built from the template, the context and the record store.
use erp_expr::{EvalContext, ExprValue};
use erp_report::{
    Alignment, AttachmentPolicy, DocTree, DocumentOptions, EvalOptions, FieldInfo, FieldValue,
    ImageFit, LangFormat, Language, LimitKind, Limits, Node, NodeId, PageCounter, PageSetup,
    ProfileKind, RecordStore, ReportError, ReportLimits, ReportProgram, Style, TableLook,
    TemplateSource, probe_image, render_html,
};
use std::{sync::Arc, time::Duration};

const NBSP: &str = "\u{a0}";
const MINUS: &str = "-\u{feff}";

fn options() -> DocumentOptions {
    DocumentOptions::new("doc-1", "Test", PageSetup::new(595.0, 842.0, 40.0).unwrap())
}
fn english() -> Language {
    Language::new("en_US").unwrap()
}
fn french() -> LangFormat {
    LangFormat::new(",", " ", &[3, 0], "%d/%m/%Y", "%H:%M:%S").unwrap()
}
fn compile_all(root: &str, sources: &[(&str, &str)]) -> Result<ReportProgram, ReportError> {
    let sources: Vec<_> = sources
        .iter()
        .map(|(key, arch)| TemplateSource::new(key, arch))
        .collect();
    ReportProgram::compile(root, &sources, &english(), ReportLimits::default())
}
fn compile(arch: &str) -> ReportProgram {
    compile_all("test.report", &[("test.report", arch)]).unwrap()
}
fn style_flags(style: &Style) -> String {
    let mut flags = String::new();
    if style.bold() {
        flags.push('b');
    }
    if style.italic() {
        flags.push('i');
    }
    if style.underline() {
        flags.push('u');
    }
    if style.keep_together() {
        flags.push('k');
    }
    match style.alignment() {
        Alignment::Center => flags.push('c'),
        Alignment::End => flags.push('e'),
        _ => {}
    }
    if style.size() != 10.0 {
        flags.push_str(&format!(" {}pt", style.size()));
    }
    if style.padding() != 0.0 {
        flags.push_str(&format!(" pad{}", style.padding()));
    }
    if style.color() != [0, 0, 0] {
        let [r, g, b] = style.color();
        flags.push_str(&format!(" #{r:02x}{g:02x}{b:02x}"));
    }
    flags.trim().to_owned()
}
fn dump_node(doc: &DocTree, id: &NodeId, out: &mut String) {
    let list = |ids: &[NodeId], out: &mut String| {
        for (i, id) in ids.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            dump_node(doc, id, out);
        }
    };
    match &doc.nodes()[id.index()] {
        Node::Text(text) => out.push_str(&format!("{text:?}")),
        Node::Flow { children } => {
            out.push_str("flow(");
            list(children, out);
            out.push(')');
        }
        Node::Block { children, style } => {
            out.push_str(&format!("block[{}](", style_flags(style)));
            list(children, out);
            out.push(')');
        }
        Node::Inline { children, style } => {
            out.push_str(&format!("inline[{}](", style_flags(style)));
            list(children, out);
            out.push(')');
        }
        Node::Grid {
            columns,
            gutter,
            cells,
            align,
        } => {
            let columns: Vec<String> = columns.iter().map(f64::to_string).collect();
            // the top of the row (the default) is not written
            let align = match align {
                erp_report::CrossAlignment::Center => "|center",
                erp_report::CrossAlignment::End => "|end",
                _ => "",
            };
            out.push_str(&format!("grid[{}|{gutter}{align}](", columns.join(",")));
            list(cells, out);
            out.push(')');
        }
        Node::Table {
            columns,
            header,
            cells,
        } => {
            out.push_str(&format!("table[{columns}](head: "));
            list(header, out);
            out.push_str("; cells: ");
            list(cells, out);
            out.push(')');
        }
        Node::List { ordered, items } => {
            out.push_str(if *ordered { "ol(" } else { "ul(" });
            list(items, out);
            out.push(')');
        }
        Node::Image {
            data,
            width,
            height,
            alt,
            ..
        } => out.push_str(&format!(
            "image[{width:?} {height:?} {alt:?} {} bytes]",
            data.len()
        )),
        Node::Cell {
            child,
            colspan,
            rowspan,
        } => {
            out.push_str(&format!("cell[{colspan}x{rowspan}]("));
            dump_node(doc, child, out);
            out.push(')');
        }
        Node::Rule => out.push_str("hr"),
        Node::LineBreak => out.push_str("br"),
        Node::PageBreak => out.push_str("pagebreak"),
        Node::PageCounter(PageCounter::Current) => out.push_str("page"),
        Node::PageCounter(PageCounter::Total) => out.push_str("pages"),
        other => out.push_str(&format!("{other:?}")),
    }
}
fn dump(doc: &DocTree) -> String {
    let mut out = String::new();
    dump_node(doc, doc.body(), &mut out);
    out
}
fn part(doc: &DocTree, id: Option<&NodeId>) -> String {
    let mut out = String::new();
    dump_node(
        doc,
        id.expect("the document has this page region"),
        &mut out,
    );
    out
}
fn evaluate(program: &ReportProgram, context: &EvalContext) -> Result<DocTree, ReportError> {
    program.evaluate(
        context,
        &RecordStore::new(),
        &EvalOptions::default(),
        options(),
    )
}
/// Compile one template and print it with plain values.
fn render(arch: &str, values: &[(&str, ExprValue)]) -> String {
    let mut context = EvalContext::standard();
    for (name, value) in values {
        context.set(*name, value.clone());
    }
    dump(&evaluate(&compile(arch), &context).unwrap())
}
fn strs(values: &[&str]) -> ExprValue {
    ExprValue::list(values.iter().map(|v| ExprValue::str(*v)).collect())
}
/// The header of a PNG of `width` by `height` pixels: what image admission reads.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}
/// Standard padded base64, written here so the decoder under test meets another encoder.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let bits = chunk
            .iter()
            .fold(0u32, |bits, byte| (bits << 8) | u32::from(*byte))
            << (8 * (3 - chunk.len()));
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (bits >> (18 - 6 * index)) & 63;
                out.push(char::from(ALPHABET[sextet as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}
/// Width, height, alt text and bytes of one image.
type ImageParts = (Option<f64>, Option<f64>, String, Vec<u8>);
/// Every image of a document, in document order.
fn images(doc: &DocTree) -> Vec<ImageParts> {
    doc.nodes()
        .iter()
        .filter_map(|node| match node {
            Node::Image {
                data,
                width,
                height,
                alt,
                ..
            } => Some((*width, *height, alt.clone(), data.to_vec())),
            _ => None,
        })
        .collect()
}
fn order_doc(arch: &str, options: &EvalOptions) -> Result<DocTree, ReportError> {
    let store = store();
    let context = EvalContext::standard().with_var("doc", store.recordset("sale.order", &[1]));
    compile(arch).evaluate(&context, &store, options, self::options())
}
/// One sale order with its currency, partner and two lines.
fn store() -> Arc<RecordStore> {
    let mut store = RecordStore::new();
    store
        .insert(
            "res.currency",
            1,
            "USD",
            vec![
                ("symbol", FieldValue::char("$")),
                ("position", FieldValue::selection("before", "Before")),
                ("decimal_places", FieldValue::integer(2)),
            ],
        )
        .unwrap();
    store
        .insert(
            "res.currency",
            2,
            "EUR",
            vec![
                ("symbol", FieldValue::char("€")),
                ("position", FieldValue::selection("after", "After")),
                ("decimal_places", FieldValue::integer(2)),
            ],
        )
        .unwrap();
    store
        .insert(
            "res.partner",
            7,
            "Agrolait, Belgium",
            vec![
                ("name", FieldValue::char("Agrolait")),
                ("email", FieldValue::char("orders@example.test")),
                ("street", FieldValue::char("Kerkstraat 1")),
                ("street2", FieldValue::char("")),
                ("zip", FieldValue::char("1000")),
                ("city", FieldValue::char("Brussel")),
            ],
        )
        .unwrap();
    for (id, name, qty) in [(31, "Desk", 2), (32, "Chair", 5)] {
        store
            .insert(
                "sale.order.line",
                id,
                name,
                vec![
                    ("name", FieldValue::char(name)),
                    ("qty", FieldValue::integer(qty)),
                    ("order_id", FieldValue::many2one("sale.order", Some(1))),
                ],
            )
            .unwrap();
    }
    store
        .insert(
            "sale.order",
            1,
            "S00001",
            vec![
                ("name", FieldValue::char("S00001")),
                ("note", FieldValue::char("")),
                ("description", FieldValue::text("line1\nline2\n\nline3")),
                ("narration", FieldValue::html("<p>Terms</p>")),
                ("amount_total", FieldValue::monetary(1250.5, "currency_id")),
                ("amount_eur", FieldValue::monetary(1250.5, "eur_id")),
                ("debt", FieldValue::monetary(-1250.5, "currency_id")),
                ("currency_id", FieldValue::many2one("res.currency", Some(1))),
                ("eur_id", FieldValue::many2one("res.currency", Some(2))),
                ("weight", FieldValue::float(4500.1267, None)),
                ("ratio", FieldValue::float(0.5, None)),
                ("quantity", FieldValue::integer(1_000_000)),
                ("date_order", FieldValue::date(2026, 10, 15).unwrap()),
                ("no_date", FieldValue::no_date()),
                (
                    "create_date",
                    FieldValue::datetime((2026, 10, 15), (14, 30, 0)).unwrap(),
                ),
                ("state", FieldValue::selection("posted", "Comptabilisé")),
                ("partner_id", FieldValue::many2one("res.partner", Some(7))),
                ("carrier_id", FieldValue::many2one("res.partner", None)),
                ("line_ids", FieldValue::many("sale.order.line", &[31, 32])),
                ("active", FieldValue::boolean(true)),
                ("logo", FieldValue::binary(&png(200, 100))),
                ("no_logo", FieldValue::binary(&[])),
            ],
        )
        .unwrap();
    Arc::new(store)
}
fn print_order(arch: &str, options: &EvalOptions) -> Result<String, ReportError> {
    let store = store();
    let context = EvalContext::standard().with_var("doc", store.recordset("sale.order", &[1]));
    let doc = compile(arch).evaluate(&context, &store, options, self::options())?;
    Ok(dump(&doc))
}
fn order(arch: &str) -> String {
    print_order(arch, &EvalOptions::default()).unwrap()
}
fn order_in(arch: &str, formats: LangFormat) -> String {
    print_order(arch, &EvalOptions::default().with_formats(formats)).unwrap()
}

#[test]
fn foreach_binds_the_item_and_its_loop_names() {
    let out = render(
        "<t><p t-foreach='lines' t-as='line'><t t-out='line_index'/>:<t t-out='line'/>:<t \
         t-out='line_size'/><t t-if='line_first'>:FIRST</t><t t-if='line_last'>:LAST</t></p></t>",
        &[("lines", strs(&["alpha", "beta", "gamma"]))],
    );
    assert_eq!(
        out,
        r#"flow(block[]("0:alpha:3:FIRST") block[]("1:beta:3") block[]("2:gamma:3:LAST"))"#
    );
}

#[test]
fn foreach_loops_over_counts_mappings_and_records_and_keeps_its_names() {
    assert_eq!(
        render("<p><t t-foreach='3' t-as='n' t-out='n'/></p>", &[]),
        r#"flow(block[]("012"))"#
    );
    let pairs = ExprValue::dict_str([("a", ExprValue::Int(1)), ("b", ExprValue::Int(2))]);
    assert_eq!(
        render(
            "<p><t t-foreach='pairs' t-as='key'><t t-out='key'/>=<t t-out='key_value'/><t \
             t-out='key_parity'/> </t></p>",
            &[("pairs", pairs)]
        ),
        r#"flow(block[]("a=1even b=2odd"))"#
    );
    assert_eq!(
        order("<p><t t-foreach='doc.line_ids' t-as='line'><t t-out='line.name'/>,</t></p>"),
        r#"flow(block[]("Desk,Chair,"))"#
    );
    // The loop's names stay set after the loop, as in the reference.
    assert_eq!(
        render("<p><t t-foreach='2' t-as='n'/><t t-out='n'/></p>", &[]),
        r#"flow(block[]("1"))"#
    );
    assert_eq!(
        render(
            "<p><t t-foreach='none' t-as='n'>x</t></p>",
            &[("none", ExprValue::None)]
        ),
        "flow()"
    );
}

#[test]
fn only_the_matching_branch_is_evaluated() {
    let arch = "<p><t t-if=\"status == 'draft'\">DRAFT</t>\n<t t-elif=\"status == \
                'posted'\">POSTED</t>\n<t t-else=''><t t-out='missing_name'/>OTHER</t></p>";
    for (status, expected) in [("draft", "DRAFT"), ("posted", "POSTED")] {
        assert_eq!(
            render(arch, &[("status", ExprValue::str(status))]),
            format!("flow(block[]({expected:?}))")
        );
    }
    // The else branch reads an unbound name: reaching it is an error, not reaching it is not.
    let context = EvalContext::standard().with_var("status", ExprValue::str("cancelled"));
    let err = evaluate(&compile(arch), &context).unwrap_err();
    assert!(matches!(err, ReportError::Expression { .. }), "{err}");
    let arch = arch.replace("<t t-out='missing_name'/>", "");
    assert_eq!(
        render(&arch, &[("status", ExprValue::str("cancelled"))]),
        r#"flow(block[]("OTHER"))"#
    );
}

#[test]
fn set_assigns_a_value_or_a_body() {
    let out = render(
        "<t><t t-set='subtotal' t-value='price * qty'/><span t-out='subtotal'/><t \
         t-set='badge'><b>ACTIVE</b></t><div t-out='badge'/></t>",
        &[
            ("price", ExprValue::Float(10.5)),
            ("qty", ExprValue::Int(4)),
        ],
    );
    assert_eq!(out, r#"flow("42.0" block[](inline[b]("ACTIVE")))"#);
    // A body keeps the names of the place it was written at.
    let out = render(
        "<p><t t-set='who' t-value=\"'first'\"/><t t-set='greeting'>hello <t t-out='who'/></t><t \
         t-set='who' t-value=\"'second'\"/><t t-out='greeting'/></p>",
        &[],
    );
    assert_eq!(out, r#"flow(block[]("hello first"))"#);
}

#[test]
fn out_prints_strings_as_text_and_nothing_for_absent_values() {
    let script = "<script>alert(\"xss\")</script>";
    assert_eq!(
        render(
            "<p t-out='content'/>",
            &[("content", ExprValue::str(script))]
        ),
        format!("flow(block[]({script:?}))")
    );
    // t-esc is the same directive under its older name.
    assert_eq!(
        render("<p t-esc='content'/>", &[("content", ExprValue::Int(7))]),
        r#"flow(block[]("7"))"#
    );
    // An absent value leaves the element out; with a body, the body prints instead.
    for absent in [ExprValue::None, ExprValue::Bool(false)] {
        assert_eq!(
            render("<div><p t-out='v'/></div>", &[("v", absent.clone())]),
            "flow()"
        );
        assert_eq!(
            render("<div><p t-out='v'>none</p></div>", &[("v", absent)]),
            r#"flow(block[]("none"))"#
        );
    }
    assert_eq!(
        render("<p t-out='v'>none</p>", &[("v", ExprValue::Int(0))]),
        r#"flow(block[]("0"))"#
    );
}

#[test]
fn call_passes_arguments_and_the_body() {
    let sources = [
        (
            "web.box_layout",
            "<div class='p-2'><h3 t-out='box_title'/><t t-out='0'/></div>",
        ),
        (
            "test.report",
            "<t><t t-set='shown' t-value=\"'outer'\"/><t t-call='web.box_layout' \
             box_title=\"'Summary'\"><p>Box Content <t t-out='shown'/></p></t></t>",
        ),
    ];
    let program = compile_all("test.report", &sources).unwrap();
    assert_eq!(
        dump(&evaluate(&program, &EvalContext::standard()).unwrap()),
        r#"flow(block[pad5](block[b 17.5pt]("Summary") block[]("Box Content outer")))"#
    );
    // The called template works on a copy: what it sets does not come back.
    let sources = [
        (
            "test.inner",
            "<t><t t-set='a' t-value='3'/><p t-out='a'/><p t-out='b'/></t>",
        ),
        (
            "test.report",
            "<t><t t-set='a' t-value='1'/><t t-set='b' t-value='1'/><t t-call='test.inner' \
             b='2'/><p t-out='a'/><p t-out='b'/></t>",
        ),
    ];
    let program = compile_all("test.report", &sources).unwrap();
    assert_eq!(
        dump(&evaluate(&program, &EvalContext::standard()).unwrap()),
        r#"flow(block[]("3") block[]("2") block[]("1") block[]("1"))"#
    );
    // A format-string argument, and a body that itself puts out the caller's body.
    let sources = [
        ("test.frame", "<div class='fw-bold'><t t-out='0'/></div>"),
        (
            "test.titled",
            "<t t-call='test.frame'><p t-out='title'/><t t-out='0'/></t>",
        ),
        (
            "test.report",
            "<t t-call='test.titled' title.f='No {{ number }}'><p>body</p></t>",
        ),
    ];
    let program = compile_all("test.report", &sources).unwrap();
    let context = EvalContext::standard().with_var("number", ExprValue::Int(5));
    assert_eq!(
        dump(&evaluate(&program, &context).unwrap()),
        r#"flow(block[b](block[b]("No 5") block[b]("body")))"#
    );
}

#[test]
fn call_errors_name_the_template_and_the_reason() {
    let err = compile_all(
        "test.report",
        &[("test.report", "<t t-call='test.missing'/>")],
    )
    .unwrap_err();
    assert!(matches!(err, ReportError::Compile { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "test.report:1: template `test.missing` is not among the supplied templates"
    );
    let cycle = [
        ("test.a", "<t t-call='test.b'/>"),
        ("test.b", "<p><t t-call='test.a'/></p>"),
    ];
    let err = compile_all("test.a", &cycle).unwrap_err();
    assert!(matches!(err, ReportError::Compile { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "test.b:1: t-call cycle: test.a -> test.b -> test.a"
    );
    for (arch, message) in [
        (
            "<div t-call='test.report'/>",
            "test.report:1: t-call must be on a <t> element, not on <div>",
        ),
        (
            "<p><t t-else=''>x</t></p>",
            "test.report:1: t-elif and t-else need a t-if on the element before",
        ),
        (
            "<p><t t-field='doc.name'/></p>",
            "test.report:1: t-field cannot be on a <t> element; put it on the element that shows the value",
        ),
        (
            "<p><span t-lang='lang'>x</span></p>",
            "test.report:1: t-lang is only read on the element that has t-call",
        ),
        (
            "<p t-foreach='lines'>x</p>",
            "test.report:1: t-foreach needs a t-as name",
        ),
    ] {
        let err = compile_all("test.report", &[("test.report", arch)]).unwrap_err();
        assert!(
            matches!(err, ReportError::Compile { .. }),
            "{arch}: {err:?}"
        );
        assert_eq!(err.to_string(), message);
    }
}

#[test]
fn evaluated_attributes_apply_like_written_ones() {
    let arch = "<div t-att-id=\"'row_%s' % line_id\" t-attf-class='text-{{align}} fw-{{weight}}' \
                t-att=\"{'title': title, 'style': None}\">x</div>";
    let values = [
        ("line_id", ExprValue::Int(101)),
        ("align", ExprValue::str("end")),
        ("weight", ExprValue::str("bold")),
        ("title", ExprValue::Bool(true)),
    ];
    assert_eq!(render(arch, &values), r#"flow(block[be]("x"))"#);
    // A later evaluated attribute replaces the written one of its name.
    assert_eq!(
        render(
            "<div class='text-center' t-att-class='cls'>x</div>",
            &[("cls", ExprValue::str("fst-italic"))]
        ),
        r#"flow(block[i]("x"))"#
    );
    assert_eq!(
        render(
            "<div class='text-center' t-att-class='cls'>x</div>",
            &[("cls", ExprValue::None)]
        ),
        r#"flow(block[]("x"))"#
    );
}

#[test]
fn an_evaluated_attribute_outside_the_profile_is_an_error() {
    let program = compile("<div>\n<p t-attf-class='item-{{status}}'>x</p></div>");
    let context = EvalContext::standard().with_var("status", ExprValue::str("open"));
    let err = evaluate(&program, &context).unwrap_err();
    let ReportError::Profile(found) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(found.kind(), ProfileKind::Class);
    assert_eq!(
        (found.template(), found.line(), found.token()),
        ("test.report", 2, "item-open")
    );
    let program = compile("<p t-att=\"{'onclick': 'x'}\">x</p>");
    let err = evaluate(&program, &EvalContext::standard()).unwrap_err();
    let ReportError::Profile(found) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(
        (found.kind(), found.token()),
        (ProfileKind::Attribute, "onclick")
    );
}

#[test]
fn an_unknown_directive_stops_the_compile() {
    let err = compile_all(
        "sale.report_order",
        &[(
            "sale.report_order",
            "<div>\n<div t-custom-command='run'>Invalid</div></div>",
        )],
    )
    .unwrap_err();
    let ReportError::Profile(found) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(found.kind(), ProfileKind::Directive);
    assert_eq!(
        err.to_string(),
        "sale.report_order:2: directive `t-custom-command` is not in print profile 1; see the addon guide page on reports"
    );
}

#[test]
fn monetary_fields_print_in_their_currency_and_the_language_format() {
    let usd = format!("${NBSP}1{NBSP}250,50");
    let eur = format!("1{NBSP}250,50{NBSP}€");
    assert_eq!(
        order_in(
            "<p><span t-field='doc.amount_total' t-options=\"{'widget': 'monetary'}\"/></p>",
            french()
        ),
        format!("flow(block[]({usd:?}))")
    );
    assert_eq!(
        order_in("<p><span t-field='doc.amount_eur'/></p>", french()),
        format!("flow(block[]({eur:?}))")
    );
    // The display_currency option wins over the field's own currency; a minus keeps its digits.
    assert_eq!(
        order_in(
            "<p><span t-field='doc.debt' t-options=\"{'display_currency': doc.eur_id}\"/></p>",
            french()
        ),
        format!("flow(block[]({:?}))", format!("{MINUS}{eur}"))
    );
    assert_eq!(
        order(
            "<p><span t-out='1250.5' t-options=\"{'widget': 'monetary', 'display_currency': doc.currency_id}\"/></p>"
        ),
        format!("flow(block[]({:?}))", format!("${NBSP}1,250.50"))
    );
    let err = print_order(
        "<p><span t-out='1250.5' t-options=\"{'widget': 'monetary'}\"/></p>",
        &EvalOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ReportError::Evaluate { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "test.report:1: the monetary widget needs the display_currency option"
    );
}

#[test]
fn float_fields_round_to_the_precision_and_use_the_language_separators() {
    let german = LangFormat::new(",", ".", &[3, 0], "%d.%m.%Y", "%H:%M:%S").unwrap();
    assert_eq!(
        order_in(
            "<p><span t-field='doc.weight' t-options=\"{'widget': 'float', 'precision': 2}\"/></p>",
            german
        ),
        r#"flow(block[]("4.500,13"))"#
    );
    // Without a precision: at most six decimals, trailing zeros dropped down to one.
    assert_eq!(
        order("<p><span t-field='doc.weight'/> <span t-field='doc.ratio'/></p>"),
        r#"flow(block[]("4,500.1267 0.5"))"#
    );
    assert_eq!(
        order(
            "<p><span t-out='2.675' t-options=\"{'widget': 'float', 'precision': 2}\"/> <span t-out='-3.0' t-options=\"{'widget': 'float'}\"/></p>"
        ),
        format!("flow(block[]({:?}))", format!("2.68 {MINUS}3.0"))
    );
}

#[test]
fn integer_fields_group_their_digits_by_the_language() {
    let arch = "<p><span t-field='doc.quantity' t-options=\"{'widget': 'integer'}\"/></p>";
    assert_eq!(order(arch), r#"flow(block[]("1,000,000"))"#);
    assert_eq!(order_in(arch, french()), r#"flow(block[]("1 000 000"))"#);
}

#[test]
fn digit_grouping_follows_the_language_sizes() {
    let moroccan = LangFormat::new(",", " ", &[3, 0], "%d/%m/%Y", "%H:%M:%S").unwrap();
    assert_eq!(
        order_in(
            "<p t-out='1200.5' t-options=\"{'widget': 'float'}\"/>",
            moroccan
        ),
        r#"flow(block[]("1 200,5"))"#
    );
    let indian = LangFormat::new(".", ",", &[3, 2, 0], "%d/%m/%Y", "%H:%M:%S").unwrap();
    assert_eq!(
        order_in(
            "<p t-out='12345678' t-options=\"{'widget': 'integer'}\"/>",
            indian
        ),
        r#"flow(block[]("1,23,45,678"))"#
    );
    // A list that ends without 0 groups once; -1 and an empty separator group nothing.
    let once = LangFormat::new(".", ",", &[3], "%d/%m/%Y", "%H:%M:%S").unwrap();
    let never = LangFormat::new(".", ",", &[-1], "%d/%m/%Y", "%H:%M:%S").unwrap();
    let bare = LangFormat::new(".", "", &[3, 0], "%d/%m/%Y", "%H:%M:%S").unwrap();
    let arch = "<p t-out='12345678' t-options=\"{'widget': 'integer'}\"/>";
    assert_eq!(order_in(arch, once), r#"flow(block[]("12345,678"))"#);
    assert_eq!(order_in(arch, never), r#"flow(block[]("12345678"))"#);
    assert_eq!(order_in(arch, bare), r#"flow(block[]("12345678"))"#);
}

#[test]
fn date_fields_use_the_language_date_format() {
    let arch = "<p><span t-field='doc.date_order'/></p>";
    assert_eq!(order(arch), r#"flow(block[]("10/15/2026"))"#);
    assert_eq!(order_in(arch, french()), r#"flow(block[]("15/10/2026"))"#);
    // An unset date leaves its element out.
    assert_eq!(order("<p><span t-field='doc.no_date'/></p>"), "flow()");
}

#[test]
fn datetime_fields_shift_to_the_user_offset() {
    let new_york = EvalOptions::default()
        .with_utc_offset_seconds(-4 * 3600)
        .unwrap();
    let arch = "<p><span t-field='doc.create_date'/></p>";
    assert_eq!(
        print_order(arch, &new_york).unwrap(),
        r#"flow(block[]("10/15/2026 10:30:00"))"#
    );
    assert_eq!(order(arch), r#"flow(block[]("10/15/2026 14:30:00"))"#);
    // The shift can cross midnight; the options pick the parts.
    let tokyo = EvalOptions::default()
        .with_utc_offset_seconds(10 * 3600)
        .unwrap();
    assert_eq!(
        print_order(
            "<p><span t-field='doc.create_date' t-options=\"{'hide_seconds': True}\"/>|<span \
             t-field='doc.create_date' t-options=\"{'date_only': True}\"/>|<span \
             t-field='doc.create_date' t-options=\"{'time_only': True}\"/></p>",
            &tokyo
        )
        .unwrap(),
        r#"flow(block[]("10/16/2026 00:30|10/16/2026|00:30:00"))"#
    );
    let err = EvalOptions::default()
        .with_utc_offset_seconds(19 * 3600)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid report document: UTC offset must be within 18 hours"
    );
}

#[test]
fn datetime_fields_shift_by_the_offset_of_their_own_timestamp() {
    // 2026-10-15 14:30:00 UTC is 1_792_074_600: the zone answers for that instant only.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let zone = EvalOptions::default().with_utc_offsets(move |unix| {
        log.lock().unwrap().push(unix);
        Some(7 * 3600)
    });
    let arch = "<p><span t-field='doc.create_date'/></p>";
    assert_eq!(
        print_order(arch, &zone).unwrap(),
        r#"flow(block[]("10/15/2026 21:30:00"))"#
    );
    assert_eq!(*seen.lock().unwrap(), vec![1_792_074_600]);
    // An unknown instant or an offset past 18 hours stops the document.
    for offset in [None, Some(19 * 3600)] {
        let err = print_order(
            arch,
            &EvalOptions::default().with_utc_offsets(move |_| offset),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timezone"), "{err}");
    }
}

#[test]
fn selection_fields_print_their_label() {
    assert_eq!(
        order("<p><span t-field='doc.state'/></p>"),
        r#"flow(block[]("Comptabilisé"))"#
    );
    assert_eq!(
        order("<p><span t-out='doc.state'/></p>"),
        r#"flow(block[]("posted"))"#
    );
    assert_eq!(
        order(
            "<p><span t-field='doc.state' t-options=\"{'selection': {'posted': 'Booked'}}\"/></p>"
        ),
        r#"flow(block[]("Booked"))"#
    );
}

#[test]
fn the_widget_option_replaces_the_widget_of_the_field_kind() {
    assert_eq!(
        order("<p><span t-field='doc.quantity' t-options=\"{'widget': 'char'}\"/></p>"),
        r#"flow(block[]("1000000"))"#
    );
    let err = print_order(
        "<p><span t-field='doc.quantity' t-options=\"{'widget': 'sparkline'}\"/></p>",
        &EvalOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ReportError::Evaluate { .. }), "{err:?}");
    assert_eq!(err.to_string(), "test.report:1: unknown widget `sparkline`");
}

#[test]
fn text_fields_break_at_line_ends() {
    assert_eq!(
        order("<p><span t-field='doc.description' t-options=\"{'widget': 'text'}\"/></p>"),
        r#"flow(block[]("line1" br "line2" br br "line3"))"#
    );
}

#[test]
fn many2one_fields_print_the_display_name() {
    assert_eq!(
        order("<p><span t-field='doc.partner_id'/></p>"),
        r#"flow(block[]("Agrolait, Belgium"))"#
    );
    assert_eq!(order("<p><span t-field='doc.carrier_id'/></p>"), "flow()");
    assert_eq!(
        order("<p><span t-field='doc.line_ids'/></p>"),
        r#"flow(block[]("Desk, Chair"))"#
    );
}

#[test]
fn a_comprehension_iterates_the_records_of_a_relation() {
    assert_eq!(
        order(
            "<p><t t-out=\"', '.join([line.name for line in doc.line_ids])\"/>|<t \
             t-out='sum([line.qty for line in doc.line_ids if line.qty > 2])'/>|<t \
             t-out='[line.id for line in doc.line_ids if line.order_id == doc]'/>|<t \
             t-out='[line for line in doc.carrier_id]'/></p>"
        ),
        r#"flow(block[]("Desk, Chair|5|[31, 32]|[]"))"#
    );
}

#[test]
fn records_read_only_what_was_loaded() {
    // Fields of related records, the empty value through an empty relation, and equality.
    assert_eq!(
        order(
            "<p><t t-out='doc.partner_id.name'/>|<t t-out='doc.carrier_id.name'/>|<t \
             t-out='doc.line_ids.order_id == doc'/>|<t t-out='doc.line_ids[1].qty'/>|<t \
             t-out='doc.id'/>|<t t-out='doc.display_name'/>|<t t-out='doc.active'/></p>"
        ),
        r#"flow(block[]("Agrolait||True|5|1|S00001|True"))"#
    );
    // An unset char field is the reference's False: its element is left out.
    assert_eq!(order("<div><p t-field='doc.note'/></div>"), "flow()");
    for (arch, message) in [
        (
            "<p t-field='doc.missing'/>",
            "test.report:1: field sale.order.missing of record 1 was not loaded for this report",
        ),
        (
            "<p t-field='doc.line_ids.name'/>",
            "test.report:1: t-field needs one record, got 2 of sale.order.line",
        ),
        (
            "<p t-field='doc.name.upper'/>",
            "test.report:1: t-field needs a record, got a value of type str",
        ),
    ] {
        let err = print_order(arch, &EvalOptions::default()).unwrap_err();
        assert!(
            matches!(err, ReportError::Evaluate { .. }),
            "{arch}: {err:?}"
        );
        assert_eq!(err.to_string(), message);
    }
    // An expression cannot reach a field or a method that was not loaded.
    for arch in [
        "<p t-out='doc.secret'/>",
        "<p t-out='doc.line_ids.name'/>",
        "<p t-out=\"doc.sudo().name\"/>",
    ] {
        let err = print_order(arch, &EvalOptions::default()).unwrap_err();
        let ReportError::Expression {
            template,
            line,
            expression,
            ..
        } = &err
        else {
            panic!("{arch}: {err:?}");
        };
        assert_eq!((template.as_str(), *line), ("test.report", 1));
        assert!(arch.contains(expression.as_str()), "{expression}");
    }
    let mut twice = RecordStore::new();
    twice.insert("res.partner", 1, "A", vec![]).unwrap();
    let err = twice.insert("res.partner", 1, "B", vec![]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid report document: record was added to the store twice"
    );
}

#[test]
fn what_later_slices_add_is_refused_by_task() {
    for (arch, message) in [
        (
            "<p><span t-field='doc.name' t-options=\"{'widget': 'image_url'}\"/></p>",
            "report images read by their path while printing requires a later release",
        ),
        (
            "<p><img src='/web/image/1'/></p>",
            "report images read by their path while printing requires a later release",
        ),
        (
            "<p><i class='oi' data-icon='phone'/></p>",
            "report icons requires a later release",
        ),
        (
            "<table><tr><td rowspan='2'>x</td></tr></table>",
            "report table row spans requires a later release",
        ),
    ] {
        let err = print_order(arch, &EvalOptions::default()).unwrap_err();
        assert_eq!(err.to_string(), message, "{arch}");
    }
    let err = compile_all(
        "test.report",
        &[("test.report", "<t t-call='{{ name }}'/>")],
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "report a t-call target computed at render time requires a later release"
    );
}

#[test]
fn t_lang_must_name_the_language_of_the_program() {
    let sources = [
        ("test.inner", "<p>inner</p>"),
        ("test.report", "<t t-call='test.inner' t-lang='lang'/>"),
    ];
    let program = compile_all("test.report", &sources).unwrap();
    for lang in [ExprValue::str("en_US"), ExprValue::Bool(false)] {
        let context = EvalContext::standard().with_var("lang", lang);
        assert_eq!(
            dump(&evaluate(&program, &context).unwrap()),
            r#"flow(block[]("inner"))"#
        );
    }
    let context = EvalContext::standard().with_var("lang", ExprValue::str("de_DE"));
    let err = evaluate(&program, &context).unwrap_err();
    assert!(matches!(err, ReportError::Evaluate { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "test.report:1: t-lang is `de_DE` but this program was compiled for `en_US`; evaluate a record with the program of its language"
    );
}

#[test]
fn terms_are_translated_at_compile_time_unless_switched_off() {
    let french = Language::new("fr_FR")
        .unwrap()
        .with_term("Shipping Address", "Adresse de livraison")
        .with_term("Invoice Date", "Date de facturation");
    let sources = [TemplateSource::new(
        "test.report",
        "<div><p> Shipping Address </p><p><span t-translation='off'>Invoice Date</span>: <b>Invoice Date</b></p></div>",
    )];
    let program =
        ReportProgram::compile("test.report", &sources, &french, ReportLimits::default()).unwrap();
    assert_eq!(program.language().code(), "fr_FR");
    let doc = evaluate(&program, &EvalContext::standard()).unwrap();
    assert_eq!(
        dump(&doc),
        r#"flow(block[]("Adresse de livraison") block[]("Invoice Date: " inline[b]("Date de facturation")))"#
    );
    // The document carries the language for the layout engine.
    assert_eq!(doc.options().language(), "fr");
    for code in ["", "EN", "en-US", "en_", "english_US"] {
        let err = Language::new(code).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid report document: language code must look like en, en_US, ar_001 or sr@latin"
        );
    }
    for code in ["en", "ar_001", "sr@latin", "sr_RS@latin"] {
        assert_eq!(Language::new(code).unwrap().code(), code);
    }
}

#[test]
fn header_footer_and_page_counters_are_document_parts() {
    let program = compile(
        "<div><div class='header'>Head</div><div class='article'><p>Body</p></div><div \
         class='footer text-center'>Page <span class='page'/> / <span class='topage'/></div></div>",
    );
    let doc = evaluate(&program, &EvalContext::standard()).unwrap();
    assert_eq!(dump(&doc), r#"flow(block[]("Body"))"#);
    assert_eq!(part(&doc, doc.header()), r#""Head""#);
    assert_eq!(
        part(&doc, doc.footer()),
        r#"block[c]("Page " page " / " pages)"#
    );
    let program = compile("<div><div class='header'>a</div><div class='header'>b</div></div>");
    let err = evaluate(&program, &EvalContext::standard()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "test.report:1: a second page header in one document"
    );
}

#[test]
fn page_break_controls_become_document_nodes() {
    assert_eq!(
        render(
            "<div><p>one</p><p class='page-break-before'>two</p><div \
             style='page-break-after: always'>three</div><div \
             class='avoid-page-break-inside'>four</div><p \
             style='page-break-inside: avoid; page-break-before: auto'>five</p></div>",
            &[]
        ),
        r#"flow(block[]("one") pagebreak block[]("two") block[]("three") pagebreak block[k]("four") block[k]("five"))"#
    );
    // A page break that the engine cannot lay out is an error, not a dropped break.
    for arch in [
        "<table><tr><td><p class='page-break-before'>x</p></td></tr></table>",
        "<div class='p-3'><p class='page-break-before'>x</p></div>",
        "<div class='row'><div class='col-6' style='page-break-after: always'>x</div></div>",
    ] {
        let err = evaluate(&compile(arch), &EvalContext::standard()).unwrap_err();
        assert!(
            matches!(err, ReportError::Evaluate { .. }),
            "{arch}: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            "test.report:1: a page break inside a table, grid, list, inline element or padded block cannot be laid out"
        );
    }
}

#[test]
fn a_cell_spans_the_columns_its_colspan_names() {
    // A section row over the whole table; the table counts its columns by span, and a short row
    // after it is filled to the column count.
    assert_eq!(
        render(
            "<table><thead><tr><th>Item</th><th>Qty</th><th>Price</th></tr></thead><tbody>\
             <tr><td t-att-colspan='width' class='fw-bold'>Section</td></tr>\
             <tr><td>a</td><td colspan='2'>b</td></tr><tr><td>c</td></tr></tbody></table>",
            &[("width", ExprValue::Int(3))]
        ),
        r#"flow(table[3](head: block[b]("Item") block[b]("Qty") block[b]("Price"); cells: cell[3x1](block[b]("Section")) "a" cell[2x1]("b") "c" "" ""))"#
    );
    // The widest row sets the column count.
    assert_eq!(
        render(
            "<table><tr><td colspan='3'>wide</td></tr><tr><td>x</td></tr></table>",
            &[]
        ),
        r#"flow(table[3](head: ; cells: cell[3x1]("wide") "x" "" ""))"#
    );
    // The profile admits a positive whole number; a cell wider than HTML allows is refused.
    let program = compile("<table><tr><td colspan='1001'>x</td></tr></table>");
    let err = evaluate(&program, &EvalContext::standard()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "test.report:1: colspan must be a whole number from 1 to 1000"
    );
}

/// further lines, as a Bootstrap row does, so a long information row wraps instead of squeezing)
#[test]
fn a_row_of_more_than_twelve_columns_wraps() {
    assert_eq!(
        render(
            "<div class='row'><div class='col-4'>a</div><div class='col-4'>b</div><div \
             class='col-4'>c</div><div class='col-4'>d</div><div class='col-6'>e</div></div>",
            &[]
        ),
        r#"flow(block[](grid[4,4,4|15](block[]("a") block[]("b") block[]("c")) grid[4,6,2|15](block[]("d") block[]("e") "")))"#
    );
    // Twelve or fewer twelfths, or a sharing column, stay one line as before.
    assert_eq!(
        render(
            "<div class='row'><div class='col-6'>a</div><div class='col-6'>b</div></div>",
            &[]
        ),
        r#"flow(grid[6,6|15](block[]("a") block[]("b")))"#
    );
    assert_eq!(
        render(
            "<div class='row'><div class='col-8'>a</div><div class='col-8'>b</div><div \
             class='col'>c</div></div>",
            &[]
        ),
        r#"flow(grid[8,8,1|15](block[]("a") block[]("b") block[]("c")))"#
    );
}

/// language, as a text node is; `t-translation="off"` keeps it)
#[test]
fn a_translate_argument_of_a_call_is_a_term() {
    let french = Language::new("fr_FR")
        .unwrap()
        .with_term("Due Date", "Date d'échéance");
    let sources = [
        TemplateSource::new(
            "test.report",
            "<div><t t-call='test.item' label.translate='Due Date'/><p t-translation='off'><t \
             t-call='test.item' label.translate='Due Date'/></p><t t-call='test.item' \
             label.translate='Paid on {{ day }}'/></div>",
        ),
        TemplateSource::new("test.item", "<p t-out='label'/>"),
    ];
    let program =
        ReportProgram::compile("test.report", &sources, &french, ReportLimits::default()).unwrap();
    let context = EvalContext::standard().with_var("day", ExprValue::str("3"));
    assert_eq!(
        dump(&evaluate(&program, &context).unwrap()),
        r#"flow(block[]("Date d'échéance") block[]("Due Date") block[]("Paid on 3"))"#
    );
}

/// the report)
#[test]
fn a_translated_argument_is_printed_as_text() {
    let french = Language::new("fr_FR")
        .unwrap()
        .with_term("Total", "Total {{ secret }} #{ secret }")
        .with_term("Open", "Ouvert {{");
    let sources = [
        TemplateSource::new(
            "test.report",
            "<div><t t-call='test.item' label.translate='Total'/><t t-call='test.item' \
             label.translate='Open'/></div>",
        ),
        TemplateSource::new("test.item", "<p t-out='label'/>"),
    ];
    let program =
        ReportProgram::compile("test.report", &sources, &french, ReportLimits::default()).unwrap();
    let context = EvalContext::standard().with_var("secret", ExprValue::str("leaked"));
    assert_eq!(
        dump(&evaluate(&program, &context).unwrap()),
        r#"flow(block[]("Total {{ secret }} #{ secret }") block[]("Ouvert {{"))"#
    );
}

#[test]
fn elements_map_to_document_nodes() {
    // Tables: header rows repeat, short rows are filled, row styles reach their cells.
    assert_eq!(
        render(
            "<table class='table table-sm'><thead><tr><th>Name</th><th \
             class='text-end'>Qty</th></tr></thead><tbody><tr t-foreach='rows' t-as='row' \
             t-att-class=\"'fw-bold' if row_last else None\"><td t-out='row'/><td \
             t-if='row_first'>1</td></tr></tbody></table>",
            &[("rows", strs(&["a", "b"]))]
        ),
        r#"flow(table[2](head: block[b]("Name") block[be]("Qty"); cells: "a" "1" block[b]("b") ""))"#
    );
    // A 12-column row and a flex row.
    assert_eq!(
        render(
            "<div class='row'><div class='col-8'>wide</div><div class='col-4 \
             text-end'><p>narrow</p></div></div>",
            &[]
        ),
        r#"flow(grid[8,4|15](block[]("wide") block[e](block[e]("narrow"))))"#
    );
    assert_eq!(
        render(
            "<div class='row'><div class='col-3'>a</div><div class='col'>b</div><div \
             class='col'>c</div></div>",
            &[]
        ),
        r#"flow(grid[3,4.5,4.5|15](block[]("a") block[]("b") block[]("c")))"#
    );
    assert_eq!(
        render(
            "<div class='d-flex justify-content-between gap-3'><span>left</span><strong>right</strong></div>",
            &[]
        ),
        r#"flow(grid[6,6|10]("left" inline[b]("right")))"#
    );
    // `align-items-*` on a row or a flex container places the cells in their row; the top is the
    // default and `align-items-start` says it.
    assert_eq!(
        render(
            "<div class='row align-items-center'><div class='col-4'>logo</div><div \
             class='col-8'>name</div></div>",
            &[]
        ),
        r#"flow(grid[4,8|15|center](block[]("logo") block[]("name")))"#
    );
    assert_eq!(
        render(
            "<div class='d-flex align-items-end'><span>left</span><strong>right</strong></div>",
            &[]
        ),
        r#"flow(grid[6,6|0|end]("left" inline[b]("right")))"#
    );
    assert_eq!(
        render(
            "<div class='row align-items-start'><div class='col'>a</div><div \
             class='col'>b</div></div>",
            &[]
        ),
        r#"flow(grid[6,6|15](block[]("a") block[]("b")))"#
    );
    // Lists, rules, line breaks, headings and inline styles.
    assert_eq!(
        render(
            "<div><ul><li>one</li><li><b>two</b></li></ul><ol><li>first</li></ol><hr/><p>a<br/>b</p></div>",
            &[]
        ),
        r#"flow(ul("one" inline[b]("two")) ol("first") hr block[]("a" br "b"))"#
    );
    assert_eq!(
        render(
            "<div><h2>Title</h2><p class='text-muted small'>muted <em>em</em> <u>u</u></p><p \
             style='font-size: 16px; color: #ff0000; font-weight: bold; text-align: \
             center; padding: 4pt'>styled</p><p class='text-primary p-2'>primary</p></div>",
            &[]
        ),
        r#"flow(block[b 20pt]("Title") block[8.75pt #6c757d]("muted " inline[i 8.75pt #6c757d]("em") " " inline[u 8.75pt #6c757d]("u")) block[bc 12pt pad4 #ff0000]("styled") block[pad5 #0d6efd]("primary"))"#
    );
    // Hidden elements print nothing.
    assert_eq!(
        render(
            "<div><p class='d-none'>no</p><p style='display: none'>no</p><p>yes</p></div>",
            &[]
        ),
        r#"flow(block[]("yes"))"#
    );
    // Theme colours come from the options.
    let program = compile("<p class='text-primary'>x <span class='text-secondary'>y</span></p>");
    let themed = EvalOptions::default().with_theme_colors([1, 2, 3], [4, 5, 6]);
    let doc = program
        .evaluate(
            &EvalContext::standard(),
            &RecordStore::new(),
            &themed,
            options(),
        )
        .unwrap();
    assert_eq!(
        dump(&doc),
        r#"flow(block[#010203]("x " inline[#040506]("y")))"#
    );
    for arch in [
        "<div><tr><td>x</td></tr></div>",
        "<div><li>x</li></div>",
        "<table><tr><p>x</p></tr></table>",
        "<ul><p>x</p></ul>",
    ] {
        let err = evaluate(&compile(arch), &EvalContext::standard()).unwrap_err();
        assert!(
            matches!(err, ReportError::Evaluate { .. }),
            "{arch}: {err:?}"
        );
    }
}

#[test]
fn white_space_is_laid_out_as_html_does() {
    assert_eq!(
        render(
            "<div>\n    <p>\n  two\n  words  </p>\n    <p>a <b> b </b> c</p>\n  <span>tail</span>\n</div>",
            &[]
        ),
        r#"flow(block[](block[]("two words") block[]("a " inline[b](" b ") " c") "tail"))"#
    );
    // A no-break space is content, not white space.
    assert_eq!(
        render(
            "<p t-out='v'/>",
            &[("v", ExprValue::str("\u{a0}a\u{a0} b\n"))]
        ),
        format!("flow(block[]({:?}))", "\u{a0}a\u{a0} b")
    );
}

#[test]
fn wrappers_emit_no_node_and_depth_is_bounded() {
    // Forty wrappers around one paragraph: none of them is a document node.
    let nested = format!("{}<p>deep</p>{}", "<div>".repeat(40), "</div>".repeat(40));
    let doc = evaluate(&compile(&nested), &EvalContext::standard()).unwrap();
    assert_eq!(dump(&doc), r#"flow(block[]("deep"))"#);
    assert_eq!(doc.nodes().len(), 3);
    // Containers that carry layout do nest, up to the document depth limit.
    let padded = |levels: usize| {
        format!(
            "{}<p>deep</p>{}",
            "<div class='p-1'>".repeat(levels),
            "</div>".repeat(levels)
        )
    };
    evaluate(&compile(&padded(29)), &EvalContext::standard()).unwrap();
    let err = evaluate(&compile(&padded(40)), &EvalContext::standard()).unwrap_err();
    let ReportError::DocumentLimit { kind, max, .. } = err else {
        panic!("{err:?}");
    };
    assert_eq!((kind, max), (LimitKind::Depth, 32));
}

#[test]
fn evaluation_and_program_limits_trip_with_their_names() {
    let limits = ReportLimits::default().with_evaluation_steps(50).unwrap();
    let sources = [TemplateSource::new(
        "test.report",
        "<p><t t-foreach='1000' t-as='n' t-out='n'/></p>",
    )];
    let program = ReportProgram::compile("test.report", &sources, &english(), limits).unwrap();
    let err = evaluate(&program, &EvalContext::standard()).unwrap_err();
    assert!(
        matches!(
            err,
            ReportError::Limit {
                name: "evaluation_steps",
                maximum: 50
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "report limit evaluation_steps exceeded or invalid (maximum 50)"
    );
    let limits = ReportLimits::default().with_program_nodes(3).unwrap();
    let sources = [TemplateSource::new(
        "test.report",
        "<div><p>a</p><p>b</p></div>",
    )];
    let err = ReportProgram::compile("test.report", &sources, &english(), limits).unwrap_err();
    assert!(
        matches!(
            err,
            ReportError::Limit {
                name: "program_nodes",
                maximum: 3
            }
        ),
        "{err:?}"
    );
    let limits = ReportLimits::default().with_program_depth(3).unwrap();
    let sources = [TemplateSource::new(
        "test.report",
        "<div><div><div><p>a</p></div></div></div>",
    )];
    let err = ReportProgram::compile("test.report", &sources, &english(), limits).unwrap_err();
    assert!(
        matches!(
            err,
            ReportError::Limit {
                name: "program_depth",
                maximum: 3
            }
        ),
        "{err:?}"
    );
    // A call adds the called template's depth to the place of the call.
    let sources = [
        TemplateSource::new("test.inner", "<div><p>a</p></div>"),
        TemplateSource::new("test.report", "<div><t t-call='test.inner'/></div>"),
    ];
    let err = ReportProgram::compile("test.report", &sources, &english(), limits).unwrap_err();
    assert!(
        matches!(
            err,
            ReportError::Limit {
                name: "program_depth",
                maximum: 3
            }
        ),
        "{err:?}"
    );
    let limits = ReportLimits::default().with_call_depth(2).unwrap();
    let sources = [
        TemplateSource::new("test.a", "<t t-call='test.b'/>"),
        TemplateSource::new("test.b", "<t t-call='test.c'/>"),
        TemplateSource::new("test.c", "<p>c</p>"),
    ];
    let err = ReportProgram::compile("test.a", &sources, &english(), limits).unwrap_err();
    assert!(
        matches!(
            err,
            ReportError::Limit {
                name: "call_depth",
                maximum: 2
            }
        ),
        "{err:?}"
    );
    for (result, name) in [
        (
            ReportLimits::default().with_evaluation_steps(0),
            "evaluation_steps",
        ),
        (
            ReportLimits::default().with_evaluation_steps(10_000_001),
            "evaluation_steps",
        ),
        (
            ReportLimits::default().with_program_nodes(0),
            "program_nodes",
        ),
        (
            ReportLimits::default().with_program_nodes(50_001),
            "program_nodes",
        ),
        (
            ReportLimits::default().with_program_depth(0),
            "program_depth",
        ),
        (
            ReportLimits::default().with_program_depth(129),
            "program_depth",
        ),
        (
            ReportLimits::default().with_template_fragments(0),
            "template_fragments",
        ),
        (
            ReportLimits::default().with_template_fragments(65_537),
            "template_fragments",
        ),
    ] {
        let err = result.unwrap_err();
        assert!(
            matches!(err, ReportError::Limit { name: found, .. } if found == name),
            "{err:?}"
        );
    }
    let limits = ReportLimits::default().with_template_fragments(2).unwrap();
    let sources = [TemplateSource::new(
        "test.report",
        "<t t-foreach='3' t-as='n'><t t-set='body'>x</t></t>",
    )];
    let program = ReportProgram::compile("test.report", &sources, &english(), limits).unwrap();
    let err = evaluate(&program, &EvalContext::standard()).unwrap_err();
    assert!(
        matches!(
            err,
            ReportError::Limit {
                name: "template_fragments",
                maximum: 2
            }
        ),
        "{err:?}"
    );
    // Fragments that print each other are bounded at run time, whatever their number.
    let mut chain = String::from("<t><t t-set='f0'>x</t>");
    for n in 1..600 {
        chain.push_str(&format!("<t t-set='f{n}'><t t-out='f{}'/></t>", n - 1));
    }
    chain.push_str("<p t-out='f599'/></t>");
    let err = evaluate(&compile(&chain), &EvalContext::standard()).unwrap_err();
    assert!(
        matches!(
            err,
            ReportError::Limit {
                name: "evaluation_depth",
                maximum: 512
            }
        ),
        "{err:?}"
    );
}

#[test]
fn an_attachment_report_with_an_unacknowledged_restricted_field_does_not_compile() {
    let fields = |model: &str, field: &str| match (model, field) {
        ("sale.order", "name") => Some(FieldInfo::scalar(false)),
        ("sale.order", "margin") => Some(FieldInfo::scalar(true)),
        _ => None,
    };
    let sources = [TemplateSource::new(
        "sale.report_order",
        "<t t-foreach='docs' t-as='doc'><p t-field='doc.name'/><p t-field='doc.margin'/></t>",
    )];
    let policy = |acknowledged| {
        AttachmentPolicy::new("sale.action_report", "sale.order")
            .with_attachment(true)
            .with_acknowledgement(acknowledged)
    };
    let compile = |acknowledged| {
        ReportProgram::compile_for_attachment(
            &policy(acknowledged),
            "sale.report_order",
            &sources,
            &fields,
            &english(),
            ReportLimits::default(),
        )
    };
    let err = compile(false).unwrap_err();
    assert!(matches!(err, ReportError::Restricted { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "report sale.action_report: sale.report_order:1 prints restricted field sale.order.margin; set allow_attachment_group_fields explicitly to acknowledge the first printer's field view"
    );
    let (program, restricted) = compile(true).unwrap();
    assert_eq!(
        restricted.iter().map(|f| f.field()).collect::<Vec<_>>(),
        ["sale.order.margin"]
    );
    assert!(program.node_count() > 0);
}

#[test]
fn language_formats_are_validated() {
    for (result, message) in [
        (
            LangFormat::new("", ",", &[3, 0], "%d", "%H"),
            "decimal point must be 1..=8 bytes and the thousands separator at most 8",
        ),
        (
            LangFormat::new(".", ",", &[3, 12], "%d", "%H"),
            "digit grouping takes at most 8 sizes, each -1..=9",
        ),
        (
            LangFormat::new(".", ",", &[3, 0], "", "%H"),
            "date and time formats must be 1..=64 bytes",
        ),
        (
            LangFormat::new(".", ",", &[3, 0], "%d", "%Q"),
            "date format directive is not supported",
        ),
    ] {
        let err = result.unwrap_err();
        assert!(
            matches!(err, ReportError::InvalidDocument { .. }),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("invalid report document: {message}")
        );
    }
    let twelve_hour = LangFormat::new(".", ",", &[3, 0], "%y-%j", "%I:%M %p %%").unwrap();
    assert_eq!(
        order_in("<p t-field='doc.create_date'/>", twelve_hour),
        r#"flow(block[]("26-288 02:30 PM %"))"#
    );
}

/// The compile-and-evaluate example kept compiling and true.
#[test]
fn the_guide_example_prints_a_letter() {
    let sources = [TemplateSource::new(
        "example.letter",
        "<p>Dear <b t-out='name'/>,</p>",
    )];
    let program = ReportProgram::compile(
        "example.letter",
        &sources,
        &Language::new("en_US").unwrap(),
        ReportLimits::default(),
    )
    .unwrap();
    let context = EvalContext::standard().with_var("name", ExprValue::str("Ada"));
    let page = PageSetup::new(595.0, 842.0, 40.0).unwrap();
    let document = program
        .evaluate(
            &context,
            &RecordStore::new(),
            &EvalOptions::default(),
            DocumentOptions::new("letter-1", "Letter", page),
        )
        .unwrap();
    assert_eq!(document.options().language(), "en");
    assert_eq!(
        dump(&document),
        r#"flow(block[]("Dear " inline[b]("Ada") ","))"#
    );
}

fn compile_with(limits: ReportLimits, sources: &[(&str, &str)]) -> ReportProgram {
    let sources: Vec<_> = sources
        .iter()
        .map(|(key, arch)| TemplateSource::new(key, arch))
        .collect();
    ReportProgram::compile("test.report", &sources, &english(), limits).unwrap()
}
fn limit_name(result: Result<DocTree, ReportError>) -> (&'static str, usize) {
    match result {
        Err(ReportError::Limit { name, maximum }) => (name, maximum),
        other => panic!("{other:?}"),
    }
}

#[test]
fn printed_text_is_charged_as_it_is_printed() {
    let limits = ReportLimits::default()
        .with_evaluation_bytes(1_000)
        .unwrap();
    let small = compile_with(
        limits,
        &[("test.report", "<p><t t-foreach='100' t-as='n'>ab</t></p>")],
    );
    evaluate(&small, &EvalContext::standard()).unwrap();
    let large = compile_with(
        limits,
        &[("test.report", "<p><t t-foreach='2000' t-as='n'>ab</t></p>")],
    );
    assert_eq!(
        limit_name(evaluate(&large, &EvalContext::standard())),
        ("evaluation_bytes", 1_000)
    );
    for result in [
        ReportLimits::default().with_evaluation_bytes(0),
        ReportLimits::default().with_evaluation_bytes(128 * 1024 * 1024 + 1),
    ] {
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                ReportError::Limit {
                    name: "evaluation_bytes",
                    ..
                }
            ),
            "{err:?}"
        );
    }
}

#[test]
fn names_copied_by_kept_fragments_are_charged_and_finished_calls_give_theirs_back() {
    let limits = ReportLimits::default()
        .with_evaluation_bytes(100_000)
        .unwrap();
    let big = "<t t-set='big' t-value=\"'x' * 10000\"/>";
    // Each kept `t-set` body holds its own copy of `big`.
    let fragments = format!("<t>{big}<t t-foreach='100' t-as='i'><t t-set='f'>a</t></t></t>");
    let program = compile_with(limits, &[("test.report", fragments.as_str())]);
    assert_eq!(
        limit_name(evaluate(&program, &EvalContext::standard())),
        ("evaluation_bytes", 100_000)
    );
    // A call's copies end with the call, so the same loop of calls stays under the bound.
    let calls = format!("<t>{big}<t t-foreach='100' t-as='i'><t t-call='test.inner'/></t></t>");
    let program = compile_with(
        limits,
        &[("test.inner", "<p>a</p>"), ("test.report", calls.as_str())],
    );
    evaluate(&program, &EvalContext::standard()).unwrap();
}

#[test]
fn an_evaluation_past_its_deadline_is_refused_by_name() {
    let limits = ReportLimits::default()
        .with_evaluation_deadline(Duration::from_millis(1))
        .unwrap();
    let program = compile_with(
        limits,
        &[(
            "test.report",
            "<p><t t-foreach='1000000' t-as='n' t-out='n'/></p>",
        )],
    );
    assert_eq!(
        limit_name(evaluate(&program, &EvalContext::standard())),
        ("evaluation_deadline", 1)
    );
    for value in [Duration::ZERO, Duration::from_secs(31)] {
        let err = ReportLimits::default()
            .with_evaluation_deadline(value)
            .unwrap_err();
        assert!(
            matches!(
                err,
                ReportError::Limit {
                    name: "evaluation_deadline",
                    maximum: 30_000
                }
            ),
            "{err:?}"
        );
    }
}

/// Block spacing, background, edge rules and the table look reach the document style; a table
/// without a look class keeps the default lined look and needs no wrapping block.
#[test]
fn block_decoration_and_table_look_reach_the_style() {
    let program = compile(
        "<div><div class='mt-3 mb-2 border-bottom' style='background-color: #f4f6f8'>A</div>\
         <table class='table table-bordered'><tr><td>x</td></tr></table>\
         <table class='table'><thead><tr><th>H</th></tr></thead><tr><td>y</td></tr></table></div>",
    );
    let doc = evaluate(&program, &EvalContext::standard()).unwrap();
    let styles: Vec<&Style> = doc
        .nodes()
        .iter()
        .filter_map(|node| match node {
            Node::Block { style, .. } => Some(style),
            _ => None,
        })
        .collect();
    let filled = styles
        .iter()
        .find(|style| style.fill().is_some())
        .expect("the filled block");
    assert_eq!(filled.fill(), Some([0xf4, 0xf6, 0xf8]));
    assert_eq!(filled.spacing(), (10.0, 5.0));
    assert_eq!(filled.rules(), (false, true));
    assert!(filled.decorated());
    let looks: Vec<TableLook> = styles
        .iter()
        .map(|style| style.table_look())
        .filter(|look| *look != TableLook::Lined)
        .collect();
    assert_eq!(looks, vec![TableLook::Bordered]);
}

/// Borders that draw nothing and translucent fills have no effect; the `margin` shorthand gives the
/// top and the bottom; an out-of-range spacing is refused naming its element.
#[test]
fn inline_border_fill_and_margin_values() {
    let first_block = |arch: &str| -> Style {
        let doc = evaluate(&compile(arch), &EvalContext::standard()).unwrap();
        doc.nodes()
            .iter()
            .find_map(|node| match node {
                Node::Block { style, .. } => Some(style.clone()),
                _ => None,
            })
            .expect("a block")
    };
    let quiet = first_block(
        "<div><div style='border-top: 0px solid black; border-bottom: 1px none #000; \
         background-color: #0000; margin: 4pt 0 8pt'>A</div></div>",
    );
    assert_eq!(quiet.rules(), (false, false));
    assert_eq!(quiet.fill(), None);
    assert_eq!(quiet.spacing(), (4.0, 8.0));
    let drawn = first_block(
        "<div><div style='border-top: 1px solid #cccccc; border-bottom: 1px solid transparent'>A</div></div>",
    );
    assert_eq!(drawn.rules(), (true, false));
    let error = evaluate(
        &compile("<div><section style='margin-top: 600mm'>A</section></div>"),
        &EvalContext::standard(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("<section>"), "{error}");
}

/// The HTML preview writes only the margin that is set, and a block holding a table is a `div`.
#[test]
fn preview_spacing_and_table_blocks() {
    let doc = evaluate(
        &compile(
            "<div><div class='mt-3'>A</div>\
             <div class='text-end'><table class='table'><tr><td>x</td></tr></table></div></div>",
        ),
        &EvalContext::standard(),
    )
    .unwrap();
    let html = erp_report::render_html(&doc, &[]).unwrap();
    assert!(html.contains("margin-top:"), "{html}");
    assert!(!html.contains("margin-bottom:0pt"), "{html}");
    assert!(
        !html.contains("<p style=\"font-size:10pt;text-align:end\"><table"),
        "{html}"
    );
}

/// `image_data_uri` and through the `image` widget, sized by its attributes, its style and the
/// widget's `max_width`/`max_height`; naming the field of corrupted bytes is the data stage's)
#[test]
fn an_image_field_prints_as_an_image_node_at_the_size_asked() {
    let logo = png(200, 100);
    // 200 by 100 pixels are 150 by 75 points.
    for (arch, width, height, alt) in [
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' alt='Logo'/></p>",
            150.0,
            75.0,
            "Logo",
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' style='max-height: 45px'/></p>",
            67.5,
            33.75,
            "",
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' style='max-height: 200px'/></p>",
            150.0,
            75.0,
            "",
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' width='100'/></p>",
            75.0,
            37.5,
            "",
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' width='100' style='height: 12pt; width: 30pt'/></p>",
            30.0,
            12.0,
            "",
        ),
        (
            "<p><img t-att=\"{'src': image_data_uri(doc.logo), 'alt': doc.name}\"/></p>",
            150.0,
            75.0,
            "S00001",
        ),
        ("<p><span t-field='doc.logo'/></p>", 150.0, 75.0, ""),
        (
            "<p><span t-field='doc.logo' t-options=\"{'widget': 'image', 'max_width': 100, 'max_height': 100}\"/></p>",
            75.0,
            37.5,
            "",
        ),
        (
            "<p><span t-out='doc.logo' t-options=\"{'widget': 'image', 'max_height': 20}\"/></p>",
            30.0,
            15.0,
            "",
        ),
    ] {
        let doc = order_doc(arch, &EvalOptions::default()).unwrap();
        assert_eq!(
            images(&doc),
            [(Some(width), Some(height), alt.to_owned(), logo.clone())],
            "{arch}"
        );
    }
}

/// The box and the fit of every picture of a document, in document order.
fn boxes(doc: &DocTree) -> Vec<(Option<f64>, Option<f64>, ImageFit)> {
    doc.nodes()
        .iter()
        .filter_map(|node| match node {
            Node::Image {
                width, height, fit, ..
            } => Some((*width, *height, *fit)),
            _ => None,
        })
        .collect()
}

/// also when that enlarges it; the document side)
/// never stretched; the centred print is in the renderer tests)
/// contain; the crop is in the renderer tests)
#[test]
fn declared_sides_size_a_picture_and_two_of_them_are_its_box() {
    // the logo is 200 by 100 pixels: 150 by 75 points
    for (arch, width, height, fit) in [
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' height='200'/></p>",
            300.0,
            150.0,
            ImageFit::Contain,
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' width='48' height='48'/></p>",
            36.0,
            36.0,
            ImageFit::Contain,
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' style='width: 48px; height: 48px; object-fit: cover'/></p>",
            36.0,
            36.0,
            ImageFit::Cover,
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' style='width: 48px; height: 48px; object-fit: fill'/></p>",
            36.0,
            36.0,
            ImageFit::Contain,
        ),
        // a maximum shrinks the box, both sides alike
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' width='48' height='48' style='max-width: 24px; object-fit: cover'/></p>",
            18.0,
            18.0,
            ImageFit::Cover,
        ),
        // without a box there is nothing to cover: the picture keeps its proportions
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' style='width: 48px; object-fit: cover'/></p>",
            36.0,
            18.0,
            ImageFit::Cover,
        ),
    ] {
        let doc = order_doc(arch, &EvalOptions::default()).unwrap();
        assert_eq!(boxes(&doc), [(Some(width), Some(height), fit)], "{arch}");
    }
}

/// carries the reason, and the document is built; the printed box and the returned warning are
/// in the renderer tests)
#[test]
fn an_unreadable_picture_is_a_placeholder_box_and_not_an_error() {
    for (arch, width, height) in [
        (
            "<p><img src='data:image/png;base64,AAAA' alt='Seal'/></p>",
            72.0,
            72.0,
        ),
        (
            "<p><img src='data:image/png;base64,AAAA' alt='Seal' width='48' height='32'/></p>",
            36.0,
            24.0,
        ),
        (
            "<p><img src='data:image/png;base64,AAAA' alt='Seal' style='max-height: 40px'/></p>",
            30.0,
            30.0,
        ),
    ] {
        let doc = order_doc(arch, &EvalOptions::default()).unwrap();
        let found: Vec<_> = doc
            .nodes()
            .iter()
            .filter_map(|node| match node {
                Node::Placeholder {
                    width,
                    height,
                    alt,
                    reason,
                } => Some((*width, *height, alt.as_str(), reason.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            found,
            [(
                width,
                height,
                "Seal",
                "the picture at test.report:1 cannot be read (image bytes are not PNG, JPEG, GIF, \
                 WebP or SVG); an empty box is printed"
            )],
            "{arch}"
        );
        assert_eq!(images(&doc), [], "{arch}");
    }
    // the preview shows the same box and lists the reason above the page
    let doc = order_doc(
        "<p><img src='data:image/png;base64,AAAA' alt='Seal' width='48' height='32'/></p>",
        &EvalOptions::default(),
    )
    .unwrap();
    let page = render_html(&doc, &[]).unwrap();
    assert!(
        page.contains(
            "<span class=\"erp-image\" role=\"img\" aria-label=\"Seal\" \
             style=\"width:36pt;height:24pt;\">Seal</span>"
        ),
        "{page}"
    );
    assert!(
        page.contains(
            "<li>the picture at test.report:1 cannot be read (image bytes are not PNG, JPEG, \
             GIF, WebP or SVG); an empty box is printed</li>"
        ),
        "{page}"
    );
    // a size outside the document's bound is still refused
    let err = order_doc(
        "<p><img src='data:image/png;base64,AAAA' width='0'/></p>",
        &EvalOptions::default(),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "test.report:1: the width and height of an image must be in (0, 14400] points"
    );
}

#[test]
fn an_embedded_image_is_decoded_and_an_img_without_a_source_prints_nothing() {
    for size in [(4, 2), (5, 2), (6, 2)] {
        // Three lengths, so the data ends with no, one and two padding characters.
        let mut picture = png(size.0, size.1);
        picture.extend(std::iter::repeat_n(0, size.0 as usize - 4));
        let arch = format!(
            "<p><img src='data:image/png;base64,{}' alt='Seal'/></p>",
            base64(&picture)
        );
        let doc = order_doc(&arch, &EvalOptions::default()).unwrap();
        assert_eq!(
            images(&doc),
            [(
                Some(f64::from(size.0) * 0.75),
                Some(1.5),
                "Seal".to_owned(),
                picture
            )]
        );
    }
    for arch in [
        "<p>a<img t-att-src='doc.no_logo and image_data_uri(doc.no_logo)'/>b</p>",
        "<p>a<img t-if='doc.no_logo' t-att-src='image_data_uri(doc.no_logo)'/>b</p>",
        "<p>a<img alt='nothing'/>b</p>",
        "<p>a<img t-att-src='image_data_uri(doc.logo)' class='d-none'/>b</p>",
    ] {
        let doc = order_doc(arch, &EvalOptions::default()).unwrap();
        assert_eq!(images(&doc), [], "{arch}");
        assert_eq!(dump(&doc), "flow(block[](\"ab\"))", "{arch}");
    }
    // A set image is true and an unset one false, and neither prints its bytes as text.
    assert_eq!(
        order("<p><t t-if='doc.logo'>set</t><t t-if='not doc.no_logo'> unset</t></p>"),
        "flow(block[](\"set unset\"))"
    );
}

#[test]
fn html_preview_embeds_a_repeated_authorized_raster_once_and_keeps_each_placement() {
    let doc = order_doc(
        "<p><img t-att-src='image_data_uri(doc.logo)' style='max-height: 45px' alt='Logo'/><img t-att-src='image_data_uri(doc.logo)' alt='Another logo'/></p>",
        &EvalOptions::default(),
    ).unwrap();
    let page = render_html(&doc, &[]).unwrap();
    assert_eq!(
        page.matches("background-image:url(data:image/png;base64,")
            .count(),
        1
    );
    assert_eq!(page.matches("class=\"erp-image erp-raster-").count(), 2);
    assert!(page.contains(
        "aria-label=\"Logo\" style=\"width:67.5pt;height:33.75pt;background-size:contain;"
    ));
    assert!(page.contains("aria-label=\"Another logo\""));
    assert!(page.contains("img-src data:"));
}

#[test]
fn an_image_that_cannot_be_printed_is_an_error_naming_why() {
    for (arch, message) in [
        (
            "<p><img t-att-src='image_data_uri(doc.name)'/></p>",
            "test.report:1: expression `image_data_uri(doc.name)`: TypeError: image_data_uri() \
             takes the value of an image field that is set, got 'str'",
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.no_logo)'/></p>",
            "test.report:1: expression `image_data_uri(doc.no_logo)`: TypeError: image_data_uri() \
             takes the value of an image field that is set, got 'bool'",
        ),
        (
            "<p><img t-att-src='image_data_uri()'/></p>",
            "test.report:1: expression `image_data_uri()`: TypeError: image_data_uri() takes one \
             argument, the value of an image field",
        ),
        (
            "<p><img src='data:image/png;base64,A'/></p>",
            "report image: the data of an embedded image is not base64",
        ),
        (
            "<p><img src='data:image/png;base64,AA=A'/></p>",
            "report image: the data of an embedded image is not base64",
        ),
        (
            "<p><img src='data:image/png;base64,AB=='/></p>",
            "report image: the data of an embedded image is not base64",
        ),
        (
            "<p><img src='data:image/png,AAAA'/></p>",
            "report image: an embedded image must be `data:image/<type>;base64,<data>`",
        ),
        (
            "<p><img t-att-src='image_data_uri(doc.logo)' width='0'/></p>",
            "test.report:1: the width and height of an image must be in (0, 14400] points",
        ),
        (
            "<p><span t-field='doc.name' t-options=\"{'widget': 'image'}\"/></p>",
            "test.report:1: the image widget needs the value of an image field, got a value of \
             type str",
        ),
        (
            "<p><span t-field='doc.logo' t-options=\"{'max_width': 0}\"/></p>",
            "test.report:1: widget option `max_width` must be a number of pixels in 1..=19200",
        ),
    ] {
        let err = order_doc(arch, &EvalOptions::default()).unwrap_err();
        assert_eq!(err.to_string(), message, "{arch}");
    }
    let err = order_doc(
        "<p><img t-att-src='image_data_uri(doc.name)'/></p>",
        &EvalOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ReportError::Expression { .. }), "{err:?}");
    // an embedded picture whose data is not base64 is the template's mistake: still an error
    let err = order_doc(
        "<p><img src='data:image/png;base64,A'/></p>",
        &EvalOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ReportError::Image { .. }), "{err:?}");
    // The image limits of the document hold for a field and for an embedded image alike.
    let limits = Limits::default()
        .lowered(LimitKind::ImagePixels, 19_999)
        .unwrap();
    let message = probe_image(&png(200, 100), &limits)
        .unwrap_err()
        .to_string();
    let embedded = format!(
        "<p><img src='data:image/png;base64,{}'/></p>",
        base64(&png(200, 100))
    );
    for arch in [
        "<p><img t-att-src='image_data_uri(doc.logo)'/></p>",
        "<p><span t-field='doc.logo'/></p>",
        embedded.as_str(),
    ] {
        let options = EvalOptions::default().with_document_limits(limits);
        let err = order_doc(arch, &options).unwrap_err();
        assert!(matches!(err, ReportError::DocumentLimit { .. }), "{err:?}");
        assert_eq!(err.to_string(), message, "{arch}");
    }
}

#[test]
fn a_contact_widget_prints_the_authorized_relation_label() {
    assert_eq!(
        order(
            "<p><span t-field='doc.partner_id' t-options=\"{'widget': 'contact', 'fields': ['name']}\"/></p>"
        ),
        order("<p><span t-field='doc.partner_id'/></p>")
    );
}

#[test]
fn contact_reads_the_requested_fields_and_reports_missing_data() {
    let arch = |fields: &str| {
        format!(
            r#"<p><span t-field='doc.partner_id' t-options="{{'widget':'contact','fields':[{fields}]}}"/></p>"#
        )
    };
    let text = order(&arch("'address','email'"));
    for item in ["Kerkstraat 1", "1000 Brussel", "orders@example.test"] {
        assert!(text.contains(item), "{text}");
    }
    let missing = order_doc(&arch("'vat'"), &EvalOptions::default()).unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("res.partner.vat was not loaded"),
        "{missing}"
    );
}

#[test]
fn audience_selection_and_markup_slots_are_checked_before_rendering() {
    let sources = [TemplateSource::new(
        "test",
        r#"<div><p groups="base.private">Private</p><p>One <b>A</b> and <i>B</i></p></div>"#,
    )];
    let build = |line: &str, member: bool| {
        ReportProgram::compile_for_groups(
            "test",
            &sources,
            &english().with_template_term("test", "One {0} and {1}", line),
            ReportLimits::default(),
            &std::collections::BTreeMap::from([("base.private".into(), member)]),
        )
    };
    for member in [false, true] {
        let doc = build("{1} before {0}", member)
            .unwrap()
            .evaluate(
                &EvalContext::standard(),
                &RecordStore::new(),
                &EvalOptions::default(),
                options(),
            )
            .unwrap();
        let html = render_html(&doc, &[]).unwrap();
        assert_eq!(html.contains("Private"), member);
        assert!(
            html.find(">B<").unwrap() < html.find(">A<").unwrap(),
            "{html}"
        );
    }
    for term in ["{0}", "{1} {1}", "{2} {0}"] {
        assert!(build(term, true).is_err(), "{term}");
    }
}
