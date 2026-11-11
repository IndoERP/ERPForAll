//! Reader rules and the review: strictness, error kinds and lines,
//! empty attributes, line ends, limits, CSV details. One negative test per refused input.

use erp_data::{
    ConvertMode, CsvImportResult, DataFileContent, DataOperation, DeleteTarget, Expr, FieldValue,
    GroupCommand, MAX_DEPTH, MAX_OPERATION_BYTES, NODE_TEXT_LIMIT, ParseErrorKind, ReaderLimits,
    RootActive, ShorthandTarget, ValueOp, XmlDataReader, XmlId, XmlIdProblem, load_data_file,
    parse_csv, parse_xml_str,
};

#[cfg(test)]
mod support {
    use erp_data::{
        AssetOp, ConvertMode, CsvImport, CsvImportResult, DataOperation, FieldValue, FunctionOp,
        MenuItemOp, ParseError, ParseErrorKind, RecordOp, TemplateOp, XmlId, parse_csv,
        parse_xml_str,
    };

    pub const FILE: &str = "addons/m/data/rules.xml";

    pub fn ops(xml: &str) -> Vec<DataOperation> {
        parse_xml_str(xml, FILE, "m", false).expect("file parses")
    }

    pub fn err(xml: &str) -> ParseError {
        parse_xml_str(xml, FILE, "m", false).expect_err("file must be refused")
    }

    pub fn err_in(body: &str) -> ParseError {
        err(&format!("<odoo>{body}</odoo>"))
    }

    pub fn one(body: &str) -> DataOperation {
        let mut all = ops(&format!("<odoo>{body}</odoo>"));
        assert_eq!(all.len(), 1, "{all:?}");
        all.remove(0)
    }

    pub fn kind_of(body: &str) -> (ParseErrorKind, String) {
        let e = err_in(body);
        (e.kind().clone(), e.detail())
    }

    pub fn xid(s: &str) -> XmlId {
        XmlId::parse(s).expect("valid external ID")
    }

    pub fn record(op: &DataOperation) -> &RecordOp {
        match op {
            DataOperation::Record(r) => r,
            other => panic!("expected a record, got {other:?}"),
        }
    }

    pub fn menu(op: &DataOperation) -> &MenuItemOp {
        match op {
            DataOperation::MenuItem(m) => m,
            other => panic!("expected a menuitem, got {other:?}"),
        }
    }

    pub fn function(op: &DataOperation) -> &FunctionOp {
        match op {
            DataOperation::Function(f) => f,
            other => panic!("expected a function, got {other:?}"),
        }
    }

    pub fn template(op: &DataOperation) -> &TemplateOp {
        match op {
            DataOperation::Template(t) => t,
            other => panic!("expected a template, got {other:?}"),
        }
    }

    pub fn asset(op: &DataOperation) -> &AssetOp {
        match op {
            DataOperation::Asset(a) => a,
            other => panic!("expected an asset, got {other:?}"),
        }
    }

    /// Value of field `name` in a one-record document holding `fields`.
    pub fn field(fields: &str, name: &str) -> FieldValue {
        let op = one(&format!("<record id=\"r\" model=\"m\">{fields}</record>"));
        record(&op)
            .field(name)
            .unwrap_or_else(|| panic!("field {name} missing"))
            .value()
            .clone()
    }

    pub fn field_kind(fields: &str) -> (ParseErrorKind, String) {
        kind_of(&format!("<record id=\"r\" model=\"m\">{fields}</record>"))
    }

    /// Messages of the source conflicts in `field_value_source_conflicts`.
    pub fn expected_conflict(fields: &str) -> &'static str {
        match fields {
            "<field name=\"f\" ref=\"base.x\" eval=\"True\"/>" => {
                "<field> takes one value source; found ref and eval"
            }
            "<field name=\"f\" search=\"[]\" ref=\"base.x\"/>" => {
                "<field> takes one value source; found search and ref"
            }
            "<field name=\"f\" search=\"[]\" eval=\"1\"/>" => {
                "<field> takes one value source; found search and eval"
            }
            "<field name=\"f\" eval=\"1\" type=\"int\"/>" => {
                "'eval' on <field> cannot be combined with 'type' or 'file'"
            }
            "<field name=\"f\" ref=\"x\" file=\"y\"/>" => {
                "'ref' on <field> cannot be combined with 'type' or 'file'"
            }
            "<field name=\"f\" ref=\"x\" model=\"res.partner\"/>"
            | "<field name=\"f\" model=\"res.partner\">x</field>" => {
                "'model' on <field> applies only with 'eval' or 'search'"
            }
            "<field name=\"f\" eval=\"1\" use=\"name\"/>" => {
                "'use' on <field> applies only with 'search'"
            }
            other => panic!("no expected message for {other}"),
        }
    }

    pub fn loaded(csv: &str) -> CsvImport {
        match parse_csv(csv, "data/res.partner.csv", "m", ConvertMode::Init).expect("CSV parses") {
            CsvImportResult::Loaded(import) => import,
            other => panic!("expected a loaded CSV, got {other:?}"),
        }
    }
}

use support::*;

fn conflict(tag: &str) -> ParseErrorKind {
    ParseErrorKind::ConflictingAttributes { tag: tag.into() }
}

fn unknown_attr(tag: &str, attr: &str) -> ParseErrorKind {
    ParseErrorKind::UnknownAttribute {
        tag: tag.into(),
        attr: attr.into(),
    }
}

fn unknown_element(parent: &str, tag: &str) -> ParseErrorKind {
    ParseErrorKind::UnknownElement {
        parent: parent.into(),
        tag: tag.into(),
    }
}

fn invalid_attr(tag: &str, attr: &str) -> ParseErrorKind {
    ParseErrorKind::InvalidAttributeValue {
        tag: tag.into(),
        attr: attr.into(),
    }
}

fn refused(tag: &str) -> ParseErrorKind {
    ParseErrorKind::Refused { tag: tag.into() }
}

fn unexpected_text(tag: &str) -> ParseErrorKind {
    ParseErrorKind::UnexpectedText { tag: tag.into() }
}

fn invalid_value(tag: &str) -> ParseErrorKind {
    ParseErrorKind::InvalidValue { tag: tag.into() }
}

// ------------------------------------------------------------------ external IDs

#[test]
fn xml_id_parse_accepts_reference_names() {
    let id = xid("base.lang_sr@latin");
    assert_eq!(id.module(), Some("base"));
    assert_eq!(id.name(), "lang_sr@latin");
    for name in [
        "brand_rolls-royce",
        "tax_25%",
        "module_category_internet_of_things_(iot)",
        "dc_nota_de_crédito",
        "l10n_tw_tax_20_>5000nt",
        "a,b",
    ] {
        assert_eq!(xid(name).name(), name);
        assert_eq!(xid(name).module(), None);
    }
    assert_eq!(xid("r1").complete_name("m"), "m.r1");
    assert_eq!(xid("x.r1").complete_name("m"), "x.r1");
    assert_eq!(xid("x.r1").to_string(), "x.r1");
    assert_eq!(
        XmlId::qualified("base", "user_root").expect("valid"),
        xid("base.user_root")
    );
}

#[test]
fn xml_id_parse_refuses_invalid_ids() {
    let cases = [
        ("", XmlIdProblem::Empty),
        (".x", XmlIdProblem::EmptyModule),
        ("x.", XmlIdProblem::EmptyName),
        ("a.b.c", XmlIdProblem::TooManyDots),
        ("a b", XmlIdProblem::Whitespace),
        (" a", XmlIdProblem::Whitespace),
        ("a\u{0}", XmlIdProblem::ControlCharacter),
        ("my-mod.x", XmlIdProblem::InvalidModuleName),
    ];
    for (input, problem) in cases {
        let e = XmlId::parse(input).expect_err(input);
        assert_eq!(e.problem(), problem, "{input:?}");
        assert_eq!(e.value(), input);
    }
    let shown = |input: &str| XmlId::parse(input).expect_err(input).to_string();
    assert_eq!(
        shown("a.b.c"),
        "The ID reference \"a.b.c\" must contain maximum one dot. They are used to refer to other \
         modules ID, in the form: module.record_id"
    );
    assert_eq!(shown(""), "an external ID must not be empty");
    assert_eq!(shown(".x"), "external ID \".x\" has an empty module part");
    assert_eq!(shown("x."), "external ID \"x.\" has an empty name part");
    assert_eq!(shown("a b"), "external ID \"a b\" contains whitespace");
    assert_eq!(
        shown("a\u{1}"),
        "external ID \"a\\u{1}\" contains a control character"
    );
    assert_eq!(
        shown("my-mod.x"),
        "external ID \"my-mod.x\": the part before the dot must be a module name (ASCII letters, \
         digits and '_')"
    );
    let problem = |m: &str, n: &str| XmlId::qualified(m, n).expect_err("invalid").problem();
    assert_eq!(problem("", "x"), XmlIdProblem::EmptyModule);
    assert_eq!(problem("m", "a.b"), XmlIdProblem::TooManyDots);
    assert_eq!(problem("m", ""), XmlIdProblem::EmptyName);
}

#[test]
fn invalid_xml_id_in_attributes_is_an_error_with_node() {
    let dots = "must contain maximum one dot. They are used to refer to other modules ID, in the \
                form: module.record_id";
    for (body, value, problem, message) in [
        (
            "<record id=\"a.b.c\" model=\"m\"/>",
            "a.b.c",
            XmlIdProblem::TooManyDots,
            format!("attribute 'id' on <record>: The ID reference \"a.b.c\" {dots}"),
        ),
        (
            "<record id=\"r\" model=\"m\"><field name=\"p\" ref=\"x.\"/></record>",
            "x.",
            XmlIdProblem::EmptyName,
            "attribute 'ref' on <field>: external ID \"x.\" has an empty name part".to_string(),
        ),
        (
            "<menuitem id=\"m1\" action=\".act\"/>",
            ".act",
            XmlIdProblem::EmptyModule,
            "attribute 'action' on <menuitem>: external ID \".act\" has an empty module part"
                .to_string(),
        ),
        (
            "<menuitem id=\"m1\" groups=\"base.g,-\"/>",
            "",
            XmlIdProblem::Empty,
            "attribute 'groups' on <menuitem>: an external ID must not be empty".to_string(),
        ),
        (
            "<delete model=\"m\" id=\"a b\"/>",
            "a b",
            XmlIdProblem::Whitespace,
            "attribute 'id' on <delete>: external ID \"a b\" contains whitespace".to_string(),
        ),
        (
            "<template id=\"t\" inherit_id=\"a.b.c\"><div/></template>",
            "a.b.c",
            XmlIdProblem::TooManyDots,
            format!("attribute 'inherit_id' on <template>: The ID reference \"a.b.c\" {dots}"),
        ),
        (
            "<odoo uid=\"x..y\"><record model=\"m\"/></odoo>",
            "x..y",
            XmlIdProblem::TooManyDots,
            format!("attribute 'uid' on <odoo>: The ID reference \"x..y\" {dots}"),
        ),
    ] {
        let e = err_in(body);
        assert_eq!(
            e.kind(),
            &ParseErrorKind::InvalidXmlId {
                value: value.into(),
                problem
            },
            "{body}"
        );
        assert_eq!(e.detail(), message, "{body}");
        // The cause is kept as the error's source.
        let source = std::error::Error::source(&e).expect("cause kept");
        assert_eq!(e.detail(), format!("{}: {source}", e.message()));
    }
    let e = err_in("\n<record id=\"a.b.c\" model=\"m\"/>");
    assert_eq!(e.line(), 2);
    assert_eq!(e.node(), Some("<record id=\"a.b.c\" model=\"m\"/>"));
}

// ------------------------------------------------------------------ field value forms

#[test]
fn field_type_and_file_combinations() {
    assert_eq!(
        field(
            "<field name=\"i\" type=\"bytes\" file=\"m/static/a.png\"/>",
            "i"
        ),
        FieldValue::FileBytes("m/static/a.png".into())
    );
    assert_eq!(
        field(
            "<field name=\"i\" type=\"base64\" file=\"m/static/a.png\"/>",
            "i"
        ),
        FieldValue::FileBase64("m/static/a.png".into())
    );
    assert_eq!(
        field(
            "<field name=\"t\" type=\"char\" file=\"m/data/t.txt\"/>",
            "t"
        ),
        FieldValue::FileText("m/data/t.txt".into())
    );
    assert_eq!(
        field("<field name=\"t\" file=\"m/data/t.txt\"/>", "t"),
        FieldValue::FileText("m/data/t.txt".into())
    );
    assert_eq!(
        field(
            "<field name=\"p\" type=\"file\"> static/x.txt </field>",
            "p"
        ),
        FieldValue::ModulePath("static/x.txt".into())
    );
    // type="file" and bare file= are different values.
    assert_ne!(
        field("<field name=\"p\" type=\"file\">x.txt</field>", "p"),
        field("<field name=\"p\" file=\"x.txt\"/>", "p")
    );

    assert_eq!(
        field_kind("<field name=\"i\" type=\"bytes\">abc</field>"),
        (
            invalid_value("field"),
            "bytes type is only compatible with file data".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"i\" type=\"base64\"/>"),
        (
            invalid_value("field"),
            "base64 type is only compatible with file data".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"p\" type=\"file\" file=\"x\"/>"),
        (refused("field"), "type=\"file\" with a 'file' attribute is refused (the reference ERP would read the path out of that file): write the module path as the element text".to_string())
    );
    assert_eq!(
        field_kind("<field name=\"p\" type=\"file\"> </field>"),
        (
            invalid_value("field"),
            "type=\"file\" needs a path".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"n\" type=\"int\" file=\"x\"/>"),
        (
            conflict("field"),
            "'file' cannot be used with type=\"int\" (import_xml.rng)".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"n\" type=\"xml\" file=\"x\"><a/></field>"),
        (
            conflict("field"),
            "'file' cannot be used with type=\"xml\" (import_xml.rng)".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"i\" type=\"bytes\" file=\"x\">text</field>"),
        (
            unexpected_text("field"),
            "text is not allowed in <field> here: \"text\"".to_string()
        )
    );
}

#[test]
fn comments_inside_text_values_are_refused() {
    assert_eq!(
        field_kind("<field name=\"n\">Hello<!-- c --> World</field>"),
        (
            refused("field"),
            "comment or processing instruction inside the text of <field name=\"n\">: the reference ERP keeps \
             only the text before it; remove it"
                .to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"n\" type=\"int\">1<!-- c -->2</field>"),
        (
            refused("field"),
            "comment or processing instruction inside the text of <field name=\"n\">: the reference ERP keeps \
             only the text before it; remove it"
                .to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"p\" type=\"file\">a<?pi x?>b</field>"),
        (
            refused("field"),
            "comment or processing instruction inside the text of <field name=\"p\">: the reference ERP keeps \
             only the text before it; remove it"
                .to_string()
        )
    );
    assert_eq!(
        kind_of(
            "<function model=\"m\" name=\"f\"><value type=\"float\">1<!-- c -->.5</value></function>"
        ),
        (
            refused("value"),
            "comment or processing instruction inside the text of <value>: the reference ERP keeps only the \
             text before it; remove it"
                .to_string()
        )
    );
    assert_eq!(
        kind_of(
            "<asset id=\"a\" name=\"A\"><bundle>web.<!-- c -->assets</bundle><path>p</path></asset>"
        ),
        (
            refused("bundle"),
            "comment or processing instruction inside the text of <bundle>: the reference ERP keeps only the \
             text before it; remove it"
                .to_string()
        )
    );
    let e =
        err_in("<record model=\"m\">\n<field name=\"n\">Hello<!-- c --> World</field></record>");
    assert_eq!(e.line(), 2);
    // Where markup is the value, comments stay part of it; comments between sub-records are fine.
    assert_eq!(
        field(
            "<field name=\"h\" type=\"html\"><p>a<!-- c -->b</p></field>",
            "h"
        ),
        FieldValue::Html("<p>a<!-- c -->b</p>".into())
    );
    assert_eq!(
        field(
            "<field name=\"x\" type=\"xml\"><form>a<!-- c -->b</form></field>",
            "x"
        ),
        FieldValue::Xml("<?xml version=\"1.0\"?>\n<form>a<!-- c -->b</form>".into())
    );
    let op = one(
        "<record model=\"m\"><field name=\"lines\"><!-- first --><record model=\"l\"/></field></record>",
    );
    assert!(matches!(
        record(&op).field("lines").map(|f| f.value()),
        Some(FieldValue::SubRecords(subs)) if subs.len() == 1
    ));
    // Comments in a field with no text at all (empty char value) are refused as well.
    assert_eq!(
        field_kind("<field name=\"n\"><!-- c --></field>"),
        (
            refused("field"),
            "comment or processing instruction inside the text of <field name=\"n\">: the reference ERP keeps \
             only the text before it; remove it"
                .to_string()
        )
    );
}

#[test]
fn field_value_source_conflicts() {
    for fields in [
        "<field name=\"f\" ref=\"base.x\" eval=\"True\"/>",
        "<field name=\"f\" search=\"[]\" ref=\"base.x\"/>",
        "<field name=\"f\" search=\"[]\" eval=\"1\"/>",
        "<field name=\"f\" eval=\"1\" type=\"int\"/>",
        "<field name=\"f\" ref=\"x\" file=\"y\"/>",
        "<field name=\"f\" ref=\"x\" model=\"res.partner\"/>",
        "<field name=\"f\" model=\"res.partner\">x</field>",
        "<field name=\"f\" eval=\"1\" use=\"name\"/>",
    ] {
        let (kind, message) = field_kind(fields);
        assert_eq!(kind, conflict("field"), "{fields}");
        assert_eq!(message, expected_conflict(fields), "{fields}");
    }
    assert_eq!(
        field_kind("<field name=\"f\" eval=\"True\">Extra Text</field>"),
        (
            unexpected_text("field"),
            "text is not allowed in <field> here: \"Extra Text\"".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" ref=\"base.x\">text</field>"),
        (
            unexpected_text("field"),
            "text is not allowed in <field> here: \"text\"".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" search=\"[]\"><record model=\"x\"/></field>"),
        (
            unknown_element("field", "record"),
            "element <record> is not allowed in <field>; this element takes no child elements here"
                .to_string()
        )
    );
}

#[test]
fn field_type_rules() {
    assert_eq!(
        field_kind("<field name=\"f\" type=\"magic\">1</field>"),
        (
            invalid_attr("field", "type"),
            "Unknown type 'magic'".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"\">1</field>"),
        (invalid_attr("field", "type"), "Unknown type ''".to_string())
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"int\">4.82</field>"),
        (
            invalid_value("field"),
            "invalid integer '4.82' in <field>: invalid digit found in string".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"float\">abc</field>"),
        (
            invalid_value("field"),
            "invalid float 'abc' in <field>: invalid float literal".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"int\"><b>1</b></field>"),
        (
            unknown_element("field", "b"),
            "element <b> is not allowed in <field>; type=\"int\" holds text only".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"list\"/>"),
        (
            ParseErrorKind::MissingElement {
                parent: "field".into(),
                tag: "value".into()
            },
            "<field type=\"list\"> needs at least one <value> (import_xml.rng)".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"list\"><item>1</item></field>"),
        (
            unknown_element("field", "item"),
            "element <item> is not allowed in <field>; only <value> is allowed".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"list\">x<value>1</value></field>"),
        (
            unexpected_text("field"),
            "text is not allowed in <field> here: \"x\"".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"xml\"> </field>"),
        (
            ParseErrorKind::MissingElement {
                parent: "field".into(),
                tag: "element".into()
            },
            "<field type=\"xml\"> needs at least one child element (import_xml.rng)".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"html\">lead<p/></field>"),
        (
            unexpected_text("field"),
            "text is not allowed in <field> here: \"lead\"".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" type=\"char\"><b/></field>"),
        (
            unknown_element("field", "b"),
            "element <b> is not allowed in <field>; type=\"char\" holds text only".to_string()
        )
    );
    // Markup in a field without type="xml" would lose its structure: refused.
    let e = err_in(
        "<record id=\"r\" model=\"m\"><field name=\"arch\"><form><field name=\"x\"/></form></field></record>",
    );
    assert_eq!(e.kind(), &unknown_element("field", "form"));
    assert_eq!(
        e.detail(),
        "element <form> is not allowed in <field>; a <field> without type=\"xml\" or \
         type=\"html\" holds text or <record> sub-records only"
    );
    assert_eq!(
        field_kind("<field type=\"int\">1</field>"),
        (
            ParseErrorKind::MissingRequiredAttribute {
                tag: "field".into(),
                attr: "name".into()
            },
            "<field> requires a non-empty 'name' attribute".to_string()
        )
    );
    assert_eq!(
        field_kind("<field name=\"f\" bogus=\"1\"/>"),
        (unknown_attr("field", "bogus"), "unknown attribute 'bogus' on <field>; allowed: name, type, file, ref, eval, search, model, use, lang".to_string())
    );
}

#[test]
fn html_serialisation_rules() {
    assert_eq!(
        field_kind("<field name=\"h\" type=\"html\"><p><br>x</br></p></field>"),
        (refused("br"), "<br> is an HTML void element: its content would be dropped by the HTML serialisation; write it empty".to_string())
    );
    assert_eq!(
        field_kind("<field name=\"h\" type=\"html\"><p><?pi x?></p></field>"),
        (refused("field"), "processing instructions are refused in a type=\"html\" field (the HTML serialisation has no form for them)".to_string())
    );
    assert_eq!(
        field(
            "<field name=\"h\" type=\"html\"><style>a &gt; b {}</style><hr/><input disabled=\"disabled\"/></field>",
            "h"
        ),
        FieldValue::Html("<style>a > b {}</style><hr><input disabled=\"disabled\">".into())
    );
}

#[test]
fn value_rules() {
    let op = one(
        "<function model=\"m\" name=\"f\"><value type=\"list\"/><value type=\"bytes\" file=\"a.bin\"/>\
         <value search=\"[]\" model=\"res.users\" use=\"login\"/><value type=\"file\">x.txt</value></function>",
    );
    let f = function(&op);
    let values: Vec<&ValueOp> = f
        .args()
        .iter()
        .map(|a| match a {
            erp_data::FunctionArg::Value(v) => v,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(values[0], &ValueOp::EmptyList);
    assert_eq!(values[1], &ValueOp::FileBytes("a.bin".into()));
    assert_eq!(
        values[2],
        &ValueOp::Search {
            domain: Expr::new("[]"),
            model: Some("res.users".into()),
            use_field: Some("login".into())
        }
    );
    assert_eq!(values[3], &ValueOp::ModulePath("x.txt".into()));

    let wrap = |v: &str| format!("<function model=\"m\" name=\"f\">{v}</function>");
    // `ref` is not a <value> attribute in import_xml.rng.
    assert_eq!(
        kind_of(&wrap("<value ref=\"base.x\"/>")),
        (unknown_attr("value", "ref"), "unknown attribute 'ref' on <value>; allowed: name, type, file, eval, search, model, use".to_string())
    );
    assert_eq!(
        kind_of(&wrap("<value><value>1</value></value>")),
        (
            unknown_element("value", "value"),
            "element <value> is not allowed in <value>; <value> holds text only (import_xml.rng)"
                .to_string()
        )
    );
    assert_eq!(
        kind_of(&wrap("<value model=\"m\">x</value>")),
        (
            conflict("value"),
            "'model' on <value> applies only with 'eval' or 'search'".to_string()
        )
    );
    assert_eq!(
        kind_of(&wrap("<value eval=\"1\">x</value>")),
        (
            unexpected_text("value"),
            "text is not allowed in <value> here: \"x\"".to_string()
        )
    );
    assert_eq!(
        kind_of(&wrap("<value type=\"xml\">x</value>")),
        (
            invalid_attr("value", "type"),
            "Unknown type 'xml'".to_string()
        )
    );
    assert_eq!(
        kind_of(&wrap("<value type=\"int\">x</value>")),
        (
            invalid_value("value"),
            "invalid integer 'x' in <value>: invalid digit found in string".to_string()
        )
    );
    assert_eq!(
        kind_of(&wrap("<value type=\"bytes\"/>")),
        (
            invalid_value("value"),
            "bytes type is only compatible with file data".to_string()
        )
    );
}

// ------------------------------------------------------------------ empty attributes

#[test]
fn empty_optional_attributes_are_absent() {
    let op = one(
        "<record id=\"\" model=\"m\"><field name=\"partner_id\" ref=\"\"/><field name=\"user_id\" eval=\"\"/>\
         <field name=\"x\" ref=\"\" eval=\"1\"/><field name=\"s\" search=\"\" model=\"\">t</field></record>",
    );
    let rec = record(&op);
    assert_eq!(rec.id(), None);
    assert_eq!(
        rec.field("partner_id").map(|f| f.value()),
        Some(&FieldValue::Text(String::new()))
    );
    assert_eq!(
        rec.field("user_id").map(|f| f.value()),
        Some(&FieldValue::Text(String::new()))
    );
    assert_eq!(
        rec.field("x").map(|f| f.value()),
        Some(&FieldValue::Eval {
            expr: Expr::new("1"),
            model: None
        })
    );
    assert_eq!(
        rec.field("s").map(|f| f.value()),
        Some(&FieldValue::Text("t".into()))
    );

    let op = one("<delete model=\"m\" id=\"\" search=\"[('a','=',1)]\"/>");
    let DataOperation::Delete(d) = &op else {
        panic!("{op:?}")
    };
    assert_eq!(
        d.target(),
        &DeleteTarget::Search(Expr::new("[('a','=',1)]"))
    );

    let op = one(
        "<menuitem id=\"m1\" parent=\"\" web_icon=\"m,i.png\" action=\"\" sequence=\"\" groups=\"\"/>",
    );
    let m = menu(&op);
    assert_eq!(m.parent(), None);
    assert_eq!(m.web_icon(), Some("m,i.png"));
    assert_eq!(m.action(), None);
    assert_eq!(m.sequence(), None);
    assert!(m.groups().is_empty());

    let op = one("<odoo uid=\"\" context=\"\"><record model=\"m\" uid=\"\" context=\"\"/></odoo>");
    assert_eq!(op.env().uid(), None);
    assert!(op.env().context_chain().is_empty());

    // Required attributes may not be empty.
    assert_eq!(
        kind_of("<record model=\"\"/>"),
        (
            ParseErrorKind::MissingRequiredAttribute {
                tag: "record".into(),
                attr: "model".into()
            },
            "<record> requires a non-empty 'model' attribute".to_string()
        )
    );
    assert_eq!(
        kind_of("<menuitem id=\"\"/>"),
        (
            ParseErrorKind::MissingRequiredAttribute {
                tag: "menuitem".into(),
                attr: "id".into()
            },
            "<menuitem> requires a non-empty 'id' attribute".to_string()
        )
    );
}

// ------------------------------------------------------------------ containers, env, flags

#[test]
fn noupdate_and_boolean_attributes() {
    let all = ops(
        "<odoo noupdate=\"1\"><data><record id=\"a\" model=\"m\"/></data>\
         <data noupdate=\"off\"><record id=\"b\" model=\"m\"/></data><data noupdate=\"\"><record id=\"c\" model=\"m\"/></data></odoo>",
    );
    let flags: Vec<bool> = all.iter().map(|op| op.env().noupdate()).collect();
    assert_eq!(flags, [true, false, true]);

    // File-level default (demo data).
    let demo =
        parse_xml_str("<odoo><record model=\"m\"/></odoo>", "demo.xml", "m", true).expect("parses");
    assert!(demo[0].env().noupdate());

    for (raw, expected) in [
        ("YES", true),
        ("On", true),
        ("0", false),
        ("False", false),
        (" true ", true),
    ] {
        let op = one(&format!("<menuitem id=\"x\" active=\"{raw}\"/>"));
        assert_eq!(menu(&op).active(), Some(expected), "{raw}");
    }
    let e = err("<odoo noupdate=\"maybe\"><record model=\"m\"/></odoo>");
    assert_eq!(e.kind(), &invalid_attr("odoo", "noupdate"));
    assert_eq!(
        e.detail(),
        "invalid boolean value 'maybe' for attribute 'noupdate' on <odoo>; expected 1/0, \
         true/false, yes/no or on/off"
    );
    assert_eq!(
        kind_of("<record id=\"r\" model=\"m\" forcecreate=\"maybe\"/>"),
        (invalid_attr("record", "forcecreate"), "invalid boolean value 'maybe' for attribute 'forcecreate' on <record>; expected 1/0, true/false, yes/no or on/off".to_string())
    );
    assert_eq!(
        record(&one("<record id=\"r\" model=\"m\" forcecreate=\"0\"/>")).forcecreate(),
        Some(false)
    );
    assert_eq!(
        kind_of("<record model=\"m\" forcecreate=\"1\"/>"),
        (
            conflict("record"),
            "'forcecreate' on <record> requires an 'id' (import_xml.rng)".to_string()
        )
    );
    assert_eq!(
        kind_of("<data bogus=\"1\"/>"),
        (
            unknown_attr("data", "bogus"),
            "unknown attribute 'bogus' on <data>; allowed: noupdate, auto_sequence, uid, context"
                .to_string()
        )
    );
}

#[test]
fn auto_sequence_flag_is_recorded_per_container() {
    // The reader records the flag of the direct container; numbering is.
    let all = ops(
        "<odoo auto_sequence=\"1\"><record id=\"a\" model=\"m\"/><data><record id=\"b\" model=\"m\"/></data>\
         <data auto_sequence=\"0\"><record id=\"c\" model=\"m\"/></data>\
         <record id=\"d\" model=\"m\"><field name=\"lines\"><record id=\"e\" model=\"l\"/></field></record></odoo>",
    );
    let flags: Vec<bool> = all.iter().map(|op| op.env().auto_sequence()).collect();
    assert_eq!(flags, [true, false, false, true]);
    let FieldValue::SubRecords(subs) = record(&all[3]).field("lines").expect("lines").value()
    else {
        panic!("sub-records expected")
    };
    assert!(subs[0].env().auto_sequence());
    let e = err("<odoo auto_sequence=\"sometimes\"/>");
    assert_eq!(e.kind(), &invalid_attr("odoo", "auto_sequence"));
    assert_eq!(
        e.detail(),
        "invalid boolean value 'sometimes' for attribute 'auto_sequence' on <odoo>; expected \
         1/0, true/false, yes/no or on/off"
    );
}

#[test]
fn menus_and_deletes_carry_noupdate_uid_and_context() {
    let all = ops(
        "<odoo context=\"{'a': 1}\"><data noupdate=\"1\" uid=\"base.user_admin\" context=\"{'b': 2}\">\
         <menuitem id=\"root\"><menuitem id=\"leaf\" action=\"act\"/></menuitem>\
         <delete model=\"m\" search=\"[]\"/></data></odoo>",
    );
    let chain = [Expr::new("{'a': 1}"), Expr::new("{'b': 2}")];
    let root = menu(&all[0]);
    for env in [root.env(), root.children()[0].env(), all[1].env()] {
        assert!(env.noupdate());
        assert_eq!(env.uid(), Some(&xid("base.user_admin")));
        assert_eq!(env.context_chain(), &chain);
    }
    assert_eq!(all[1].source_file(), FILE);
    assert_eq!(all[1].line(), 1);
}

// ------------------------------------------------------------------ menuitem, delete, function

#[test]
fn menuitem_schema_rules() {
    assert_eq!(
        kind_of("<menuitem id=\"r\"><menuitem id=\"c\" parent=\"other\"/></menuitem>"),
        (
            conflict("menuitem"),
            "a nested <menuitem> takes the enclosing menu as parent; remove 'parent'".to_string()
        )
    );
    assert_eq!(
        kind_of("<menuitem id=\"r\"><menuitem id=\"c\" web_icon=\"i.png\"/></menuitem>"),
        (
            conflict("menuitem"),
            "only top-level menus have a 'web_icon'".to_string()
        )
    );
    assert_eq!(
        kind_of(
            "<menuitem id=\"r\"><menuitem id=\"c\" action=\"a\"><menuitem id=\"g\"/></menuitem></menuitem>"
        ),
        (
            conflict("menuitem"),
            "a submenu has either an 'action' or child menus, not both (import_xml.rng)"
                .to_string()
        )
    );
    assert_eq!(
        kind_of("<menuitem id=\"r\" parent=\"p\" action=\"a\"><menuitem id=\"g\"/></menuitem>"),
        (
            conflict("menuitem"),
            "a submenu has either an 'action' or child menus, not both (import_xml.rng)"
                .to_string()
        )
    );
    // A root menu may have an action and children.
    let op = one("<menuitem id=\"r\" action=\"a\"><menuitem id=\"c\"/></menuitem>");
    assert_eq!(menu(&op).children().len(), 1);
    assert_eq!(
        kind_of("<menuitem id=\"r\"><record model=\"m\"/></menuitem>"),
        (
            unknown_element("menuitem", "record"),
            "element <record> is not allowed in <menuitem>; only nested <menuitem> is allowed"
                .to_string()
        )
    );
    assert_eq!(
        kind_of("<menuitem id=\"r\">text</menuitem>"),
        (
            unexpected_text("menuitem"),
            "text is not allowed in <menuitem> here: \"text\"".to_string()
        )
    );
    let e = err_in("<menuitem id=\"m1\" sequence=\"not_an_int\"/>");
    assert_eq!(e.kind(), &invalid_attr("menuitem", "sequence"));
    assert_eq!(
        e.detail(),
        "invalid integer 'not_an_int' for attribute 'sequence' on <menuitem>: invalid digit found \
         in string"
    );
    assert_eq!(
        menu(&one("<menuitem id=\"m\" sequence=\" 10 \"/>")).sequence(),
        Some(10)
    );
    assert_eq!(
        menu(&one("<menuitem id=\"m\" groups=\"a,,-b,\"/>")).groups(),
        &[GroupCommand::Link(xid("a")), GroupCommand::Unlink(xid("b"))]
    );
    assert_eq!(
        kind_of("<menuitem id=\"m\" target=\"x\"/>"),
        (unknown_attr("menuitem", "target"), "unknown attribute 'target' on <menuitem>; allowed: id, name, parent, sequence, action, web_icon, groups, active".to_string())
    );
}

#[test]
fn delete_schema_rules() {
    assert_eq!(
        kind_of("<delete model=\"m\" id=\"a\" search=\"[]\"/>"),
        (
            conflict("delete"),
            "<delete> takes either 'id' or 'search', not both (import_xml.rng)".to_string()
        )
    );
    assert_eq!(
        kind_of("<delete model=\"m\"/>"),
        (
            ParseErrorKind::MissingRequiredAttribute {
                tag: "delete".into(),
                attr: "id".into()
            },
            "<delete> requires a non-empty 'id' or 'search' attribute".to_string()
        )
    );
    assert_eq!(
        kind_of("<delete model=\"m\" id=\"a\">x</delete>"),
        (
            unexpected_text("delete"),
            "text is not allowed in <delete> here: \"x\"".to_string()
        )
    );
    assert_eq!(
        kind_of("<delete model=\"m\" id=\"a\" uid=\"u\"/>"),
        (
            unknown_attr("delete", "uid"),
            "unknown attribute 'uid' on <delete>; allowed: model, id, search".to_string()
        )
    );
}

#[test]
fn function_schema_rules() {
    for name in ["mro", "f_globals", "gi_frame", "__class__", "a__b"] {
        let e = err_in(&format!("<function model=\"m\" name=\"{name}\"/>"));
        assert_eq!(
            e.kind(),
            &ParseErrorKind::ForbiddenName { name: name.into() }
        );
        assert_eq!(e.detail(), format!("Access to forbidden name '{name}'"));
    }
    assert_eq!(
        function(&one("<function model=\"m\" name=\"eval\"/>")).name(),
        "eval"
    );
    assert_eq!(
        kind_of("<function model=\"m\" name=\"f\"><function model=\"m\" name=\"g\" uid=\"base.u\"/></function>"),
        (refused("function"), "'uid'/'context' on a nested <function> would be ignored (the reference ERP evaluates it in the enclosing function's environment); set them on the outer <function>".to_string())
    );
    assert_eq!(
        kind_of("<function model=\"m\" name=\"f\" eval=\"[1]\"><value>2</value></function>"),
        (
            conflict("function"),
            "<function> takes either 'eval' or <value>/<function> children (import_xml.rng)"
                .to_string()
        )
    );
    let e = err_in(
        "<function model=\"m\" name=\"f\">\n<value name=\"arg\">1</value>\n<value name=\"arg\">2</value></function>",
    );
    assert_eq!(
        e.kind(),
        &ParseErrorKind::DuplicateKeywordArgument { name: "arg".into() }
    );
    assert_eq!(e.detail(), "repeated keyword argument 'arg' in <function>");
    assert_eq!(e.line(), 3);
    assert_eq!(
        kind_of("<function model=\"m\" name=\"f\"><field name=\"x\"/></function>"),
        (
            unknown_element("function", "field"),
            "element <field> is not allowed in <function>; only <value> and <function> are allowed"
                .to_string()
        )
    );
    assert_eq!(
        kind_of("<function model=\"m\" name=\"f\">text</function>"),
        (
            unexpected_text("function"),
            "text is not allowed in <function> here: \"text\"".to_string()
        )
    );
    let op = one("<function model=\"m\" name=\"f\"><value name=\"\">1</value></function>");
    assert_eq!(function(&op).args().len(), 1);
}

// ------------------------------------------------------------------ template, asset

#[test]
fn template_rules() {
    // Escaping of the id written into t-name.
    let op = one("<template id=\"a&amp;b&quot;c\" name=\"x\"/>");
    assert_eq!(template(&op).full_id(), "m.a&b\"c");
    assert_eq!(
        template(&op).arch(),
        "<t name=\"x\" t-name=\"m.a&amp;b&quot;c\"/>"
    );
    // data-* attributes listed in the schema stay on the arch root; t-name alone gives the id.
    let op = one(
        "<template t-name=\"snip\" name=\"S\" data-number-of-elements=\"4\"><section/></template>",
    );
    let t = template(&op);
    assert_eq!(t.id(), &xid("snip"));
    assert_eq!(t.t_name(), Some("snip"));
    assert_eq!(
        t.arch(),
        "<t t-name=\"m.snip\" name=\"S\" data-number-of-elements=\"4\"><section/></t>"
    );
    assert_eq!(
        kind_of("<template id=\"t\" data-foo=\"1\"/>"),
        (unknown_attr("template", "data-foo"), "unknown attribute 'data-foo' on <template>; allowed: id, t-name, name, forcecreate, context, priority, key, website_id, technical_usage, track, inherit_id, primary, groups, active, customize_show, data-number-of-elements, data-number-of-elements-sm, data-number-of-elements-fetch, data-row-per-slide, data-arrow-position, data-extra-classes, data-extra-snippet-classes, data-container-classes, data-content-classes, data-column-classes, data-thumb".to_string())
    );
    assert_eq!(
        kind_of("<template id=\"t\" uid=\"base.u\"/>"),
        (unknown_attr("template", "uid"), "unknown attribute 'uid' on <template>; allowed: id, t-name, name, forcecreate, context, priority, key, website_id, technical_usage, track, inherit_id, primary, groups, active, customize_show, data-number-of-elements, data-number-of-elements-sm, data-number-of-elements-fetch, data-row-per-slide, data-arrow-position, data-extra-classes, data-extra-snippet-classes, data-container-classes, data-content-classes, data-column-classes, data-thumb".to_string())
    );
    assert_eq!(
        kind_of("<template id=\"t\" active=\"1\"/>"),
        (invalid_attr("template", "active"), "attribute 'active' on <template> must be 'True' or 'False', found '1' (the reference ERP ignores any other value)".to_string())
    );
    assert_eq!(
        kind_of("<template id=\"t\" customize_show=\"yes\"/>"),
        (invalid_attr("template", "customize_show"), "attribute 'customize_show' on <template> must be 'True' or 'False', found 'yes' (the reference ERP ignores any other value)".to_string())
    );
    assert_eq!(
        kind_of("<template id=\"t\" primary=\"True\"/>"),
        (
            conflict("template"),
            "primary=\"True\" on <template> requires 'inherit_id' (import_xml.rng)".to_string()
        )
    );
    assert_eq!(
        kind_of("<template id=\"t\" inherit_id=\"x\" primary=\"1\"/>"),
        (
            invalid_attr("template", "primary"),
            "'primary' on <template> must be 'True', found '1' (import_xml.rng)".to_string()
        )
    );
    assert_eq!(
        kind_of("<template id=\"t\" groups=\"base.a,-base.b\"/>"),
        (
            invalid_attr("template", "groups"),
            "'-base.b': <template> groups cannot remove a group; only menus support '-group'"
                .to_string()
        )
    );
    assert_eq!(
        kind_of("<template id=\"t\" groups=\"base.a,\"/>"),
        (
            ParseErrorKind::InvalidXmlId {
                value: String::new(),
                problem: XmlIdProblem::Empty
            },
            "attribute 'groups' on <template>: an external ID must not be empty".to_string()
        )
    );
    assert_eq!(
        kind_of("<template id=\"t\" priority=\"high\"/>"),
        (invalid_attr("template", "priority"), "invalid integer 'high' for attribute 'priority' on <template>: invalid digit found in string".to_string())
    );
    assert_eq!(
        kind_of("<template name=\"n\"/>"),
        (
            ParseErrorKind::MissingRequiredAttribute {
                tag: "template".into(),
                attr: "id".into()
            },
            "<template> requires a non-empty 'id' or 't-name' attribute".to_string()
        )
    );
    // Remaining record attributes reach the record.
    let op = one(
        "<template id=\"t\" key=\"k.t\" track=\"1\" website_id=\"website.default\" technical_usage=\"u\" \
         forcecreate=\"0\" customize_show=\"True\"><div/></template>",
    );
    let t = template(&op);
    let r = t.to_record_op(ShorthandTarget::Standard, RootActive::Apply);
    assert_eq!(r.forcecreate(), Some(false));
    let names: Vec<&str> = r.fields().iter().map(|f| f.name()).collect();
    assert_eq!(
        names,
        [
            "name",
            "key",
            "type",
            "track",
            "technical_usage",
            "website_id",
            "customize_show",
            "arch"
        ]
    );
    assert_eq!(
        r.field("key").map(|f| f.value()),
        Some(&FieldValue::Text("k.t".into()))
    );
    assert_eq!(
        r.field("website_id").map(|f| f.value()),
        Some(&FieldValue::Ref(xid("website.default")))
    );
}

#[test]
fn asset_rules() {
    let op = one(
        "<asset id=\"a\" name=\"A\" active=\"True\"><bundle directive=\"prepend\">web.assets_backend</bundle>\
         <path>web/static/src/test.js</path><field name=\"sequence\" eval=\"5\"/></asset>",
    );
    let a = asset(&op);
    assert_eq!(a.bundle(), "web.assets_backend");
    assert_eq!(a.directive(), Some("prepend"));
    assert_eq!(a.path(), "web/static/src/test.js");
    assert_eq!(a.active(), Some(true));
    let r = a.to_record_op(ShorthandTarget::Theme, RootActive::Apply);
    assert_eq!(r.model(), "theme.ir.asset");
    let names: Vec<&str> = r.fields().iter().map(|f| f.name()).collect();
    assert_eq!(
        names,
        ["name", "bundle", "directive", "path", "active", "sequence"]
    );
    let skip = a.to_record_op(ShorthandTarget::Standard, RootActive::Skip);
    assert_eq!(skip.model(), "ir.asset");
    assert!(skip.field("active").is_none());

    assert_eq!(
        kind_of("<asset id=\"a\" name=\"A\"><bundle>b</bundle></asset>"),
        (
            ParseErrorKind::MissingElement {
                parent: "asset".into(),
                tag: "path".into()
            },
            "<asset> requires one <path>".to_string()
        )
    );
    assert_eq!(
        kind_of("<asset id=\"a\" name=\"A\"><path>p</path></asset>"),
        (
            ParseErrorKind::MissingElement {
                parent: "asset".into(),
                tag: "bundle".into()
            },
            "<asset> requires one <bundle>".to_string()
        )
    );
    assert_eq!(
        kind_of(
            "<asset id=\"a\" name=\"A\"><bundle>b1</bundle><bundle>b2</bundle><path>p</path></asset>"
        ),
        (
            ParseErrorKind::RepeatedElement {
                parent: "asset".into(),
                tag: "bundle".into()
            },
            "<bundle> may appear only once in <asset>".to_string()
        )
    );
    assert_eq!(
        kind_of(
            "<asset id=\"a\" name=\"A\"><bundle>b</bundle><path>p</path><path>q</path></asset>"
        ),
        (
            ParseErrorKind::RepeatedElement {
                parent: "asset".into(),
                tag: "path".into()
            },
            "<path> may appear only once in <asset>".to_string()
        )
    );
    assert_eq!(
        kind_of("<asset id=\"a\" name=\"A\"><bundle/><path>p</path></asset>"),
        (
            invalid_value("bundle"),
            "<bundle> must not be empty".to_string()
        )
    );
    assert_eq!(
        kind_of("<asset id=\"a\" name=\"A\"><bundle x=\"1\">b</bundle><path>p</path></asset>"),
        (
            unknown_attr("bundle", "x"),
            "unknown attribute 'x' on <bundle>; allowed: directive".to_string()
        )
    );
    assert_eq!(
        kind_of("<asset id=\"a\" name=\"A\"><bundle>b</bundle><path x=\"1\">p</path></asset>"),
        (
            unknown_attr("path", "x"),
            "unknown attribute 'x' on <path>; it takes no attributes".to_string()
        )
    );
    assert_eq!(
        kind_of("<asset id=\"a\" name=\"A\"><bundle>b</bundle><path>p</path><script/></asset>"),
        (
            unknown_element("asset", "script"),
            "element <script> is not allowed in <asset>; allowed: <bundle>, <path>, <field>"
                .to_string()
        )
    );
    assert_eq!(
        kind_of("<asset id=\"a\" name=\"A\" active=\"yes\"><bundle>b</bundle><path>p</path></asset>"),
        (invalid_attr("asset", "active"), "attribute 'active' on <asset> must be 'True' or 'False', found 'yes' (the reference ERP ignores any other value)".to_string())
    );
    assert_eq!(
        kind_of("<asset id=\"a\"><bundle>b</bundle><path>p</path></asset>"),
        (
            ParseErrorKind::MissingRequiredAttribute {
                tag: "asset".into(),
                attr: "name".into()
            },
            "<asset> requires a non-empty 'name' attribute".to_string()
        )
    );
}

// ------------------------------------------------------------------ syntax, entities, limits

#[test]
fn entities_are_resolved_and_others_refused() {
    let op = one(
        "<record id=\"r\" model=\"m\" context=\"{'a': '&lt;&amp;&#233;'}\"><field name=\"name\">Tom &amp; Jerry &lt;&gt; &quot;&#65;&apos;&#x263A;</field></record>",
    );
    let rec = record(&op);
    assert_eq!(
        rec.field("name").map(|f| f.value()),
        Some(&FieldValue::Text("Tom & Jerry <> \"A'\u{263A}".into()))
    );
    assert_eq!(rec.env().context_chain(), &[Expr::new("{'a': '<&é'}")]);

    for (xml, message) in [
        (
            "<odoo><record model=\"m\"><field name=\"n\">&custom_entity;</field></record></odoo>",
            "disallowed or unknown XML entity reference &custom_entity;",
        ),
        (
            "<odoo><record model=\"m\"><field name=\"n\">&lol1;</field></record></odoo>",
            "disallowed or unknown XML entity reference &lol1;",
        ),
        (
            "<odoo>&nbsp;<record model=\"m\"/></odoo>",
            "disallowed or unknown XML entity reference &nbsp;",
        ),
        (
            "<odoo><record model=\"m\"><field name=\"n\">&#1;</field></record></odoo>",
            "character reference &#1; is not an XML character",
        ),
        (
            "<!DOCTYPE foo [ <!ENTITY xxe SYSTEM \"file:///etc/passwd\"> ]>\n<odoo/>",
            "DOCTYPE declarations are not permitted",
        ),
    ] {
        let e = err(xml);
        assert_eq!(
            (e.kind(), e.detail().as_str()),
            (&ParseErrorKind::XmlSyntax, message),
            "{xml}"
        );
    }
    // Attribute values: the text after our prefix is quick-xml's own wording (it names byte
    // ranges and changes between quick-xml releases), so only our parts are compared.
    for xml in [
        "<odoo><record model=\"m\" id=\"&foo;\"/></odoo>",
        "<odoo><record model=\"m\" id=\"a &amp b\"/></odoo>",
    ] {
        let e = err(xml);
        assert_eq!(e.kind(), &ParseErrorKind::XmlSyntax, "{xml}");
        let detail = e.detail();
        assert!(
            detail.starts_with("attribute 'id' on <record> cannot be read: "),
            "{detail}"
        );
        assert!(detail.ends_with(""), "{detail}");
    }
}

#[test]
fn syntax_errors_have_lines() {
    // Our own messages are compared whole. For the two well-formedness errors quick-xml detects
    // itself (duplicate attribute, mismatched end tag) only our prefix is compared: the rest is
    // quick-xml's wording with byte positions.
    for (xml, line, message) in [
        (
            "<odoo>\n<record id=\"r1\" model=\"a\" model=\"b\"/>\n</odoo>",
            2,
            "malformed attribute on <record>: ",
        ),
        (
            "<odoo>\n<record model=\"m\">\n</odoo>",
            3,
            "malformed XML: ",
        ),
        (
            "<odoo>\n<record model=\"m\"/>\n</odoo>\n<odoo/>",
            4,
            "<odoo> after the root element (a document has one root)",
        ),
        ("<odoo/>\ntrailing text", 2, "text outside the root element"),
        ("text first\n<odoo/>", 1, "text outside the root element"),
        ("", 1, "empty XML document (no root element)"),
        (
            "<odoo>\n<record model=\"m\"/>",
            2,
            "unclosed <odoo> at end of file",
        ),
        (
            "<?xml version=\"1.0\"?>\n<odoo>\n<?xml version=\"1.0\"?>\n</odoo>",
            3,
            "the XML declaration must be at the very start of the file",
        ),
    ] {
        let e = err(xml);
        assert_eq!(e.kind(), &ParseErrorKind::XmlSyntax, "{xml:?}: {e}");
        assert_eq!(e.line(), line, "{xml:?}: {e}");
        assert_eq!(e.file(), FILE);
        if message.ends_with(": ") {
            assert!(e.detail().starts_with(message), "{xml:?}: {}", e.detail());
        } else {
            assert_eq!(e.detail(), message, "{xml:?}");
        }
    }
    // Comments, processing instructions and the declaration are fine around the root.
    assert!(ops("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- c -->\n<odoo><!-- x --><?pi y?></odoo>\n").is_empty());
}

#[test]
fn depth_limit_reports_the_real_file() {
    let mut deep = String::new();
    for _ in 0..260 {
        deep.push_str("<data>");
    }
    for _ in 0..260 {
        deep.push_str("</data>");
    }
    let e = parse_xml_str(&deep, "deep.xml", "m", false).expect_err("too deep");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::DepthLimitExceeded { limit: MAX_DEPTH }
    );
    assert_eq!(e.file(), "deep.xml");
    assert_eq!(e.detail(), "maximum element nesting depth (256) exceeded");

    let mut xml = String::from("<odoo><record model=\"m\"><field name=\"a\" type=\"xml\">\n");
    for _ in 0..300 {
        xml.push_str("<div>");
    }
    for _ in 0..300 {
        xml.push_str("</div>");
    }
    xml.push_str("</field></record></odoo>");
    let e = parse_xml_str(&xml, "deep_op.xml", "m", false).expect_err("too deep");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::DepthLimitExceeded { limit: MAX_DEPTH }
    );
    assert_eq!(e.detail(), "maximum element nesting depth (256) exceeded");
    assert_eq!(e.file(), "deep_op.xml");
    assert_eq!(e.line(), 2);

    // A host can lower the limits, never raise them; zero refuses everything.
    let limits = ReaderLimits::default()
        .with_max_depth(3)
        .with_max_depth(1_000);
    assert_eq!(limits.max_depth(), 3);
    let e = XmlDataReader::new(
        "<odoo><data><data><record model=\"m\"/></data></data></odoo>",
        "low.xml",
        "m",
        false,
    )
    .with_limits(limits)
    .collect::<Result<Vec<_>, _>>()
    .expect_err("depth 4 > 3");
    assert_eq!(e.kind(), &ParseErrorKind::DepthLimitExceeded { limit: 3 });
    assert_eq!(e.detail(), "maximum element nesting depth (3) exceeded");
    let zero = ReaderLimits::default().with_max_depth(0);
    let e = XmlDataReader::new("<odoo/>", "zero.xml", "m", false)
        .with_limits(zero)
        .collect::<Result<Vec<_>, _>>()
        .expect_err("zero refuses the root");
    assert_eq!(e.kind(), &ParseErrorKind::DepthLimitExceeded { limit: 0 });
    assert_eq!(e.detail(), "maximum element nesting depth (0) exceeded");

    // Exactly the limit is fine.
    let mut ok = String::from("<odoo><record model=\"m\"><field name=\"a\" type=\"xml\">");
    for _ in 0..(MAX_DEPTH - 3) {
        ok.push_str("<div>");
    }
    for _ in 0..(MAX_DEPTH - 3) {
        ok.push_str("</div>");
    }
    ok.push_str("</field></record></odoo>");
    assert_eq!(ops(&ok).len(), 1);
}

#[test]
fn node_text_in_errors_is_capped() {
    let long = "x".repeat(5000);
    let xml = format!(
        "<odoo><record id=\"r\" model=\"m\"><field name=\"a\">{long}é</field><field name=\"n\" type=\"int\">bad</field></record></odoo>"
    );
    let e = err(&xml);
    assert_eq!(e.kind(), &invalid_value("field"));
    assert_eq!(
        e.detail(),
        "invalid integer 'bad' in <field>: invalid digit found in string"
    );
    let node = e.node().expect("node attached");
    let expected_prefix = format!(
        "<record id=\"r\" model=\"m\"><field name=\"a\">{}",
        "x".repeat(5000)
    );
    assert_eq!(node, format!("{}…", &expected_prefix[..NODE_TEXT_LIMIT]));
    assert!(e.to_string().len() < NODE_TEXT_LIMIT + 400);
}

#[test]
fn line_ends_are_normalised_and_lines_counted() {
    let body = "<odoo>\r\n<record id=\"r\" model=\"m\">\r\n<field name=\"a\">x\r\ny\rz</field>\r\n\
                <field name=\"b\"><![CDATA[p\r\nq]]></field>\r\n<field name=\"c\" type=\"xml\"><form>\r\n</form></field>\r\n\
                <field name=\"d\" type=\"html\"><p>1\r\n2</p></field>\r\n</record>\r\n</odoo>\r\n";
    for content in [body.to_string(), format!("\u{FEFF}{body}")] {
        let all = parse_xml_str(&content, "crlf.xml", "m", false).expect("parses");
        let rec = record(&all[0]);
        assert_eq!(rec.line(), 2);
        let field = |n: &str| rec.field(n).expect("field");
        assert_eq!(field("a").value(), &FieldValue::Text("x\ny\nz".into()));
        assert_eq!(field("a").line(), 3);
        assert_eq!(field("b").value(), &FieldValue::Text("p\nq".into()));
        assert_eq!(field("b").line(), 6);
        assert_eq!(
            field("c").value(),
            &FieldValue::Xml("<?xml version=\"1.0\"?>\n<form>\n</form>".into())
        );
        assert_eq!(field("c").line(), 8);
        assert_eq!(field("d").value(), &FieldValue::Html("<p>1\n2</p>".into()));
        assert_eq!(field("d").line(), 10);
    }
    // Error lines in a CRLF file with a BOM.
    let bad = "\u{FEFF}<odoo>\r\n<record model=\"m\">\r\n\r\n<field name=\"n\" type=\"int\">z</field>\r\n</record>\r\n</odoo>";
    let e = parse_xml_str(bad, "crlf.xml", "m", false).expect_err("bad int");
    assert_eq!(e.line(), 4);
    // Attribute line ends become spaces (XML attribute normalisation).
    let op = one("<record model=\"m\" context=\"{'a':\r\n 1}\"/>");
    assert_eq!(op.env().context_chain(), &[Expr::new("{'a':  1}")]);
}

#[test]
fn template_arch_keeps_crlf_free_markup() {
    let op = one("<template id=\"t\">\r\n<div>\r\n</div>\r\n</template>");
    assert_eq!(
        template(&op).arch(),
        "<t t-name=\"m.t\">\n<div>\n</div>\n</t>"
    );
}

// ------------------------------------------------------------------ error kinds overview

#[test]
fn schema_kinds_display_like_reference() {
    let e = err_in("<record model=\"m\">text</record>");
    assert!(e.kind().is_schema());
    assert_eq!(
        e.to_string(),
        format!(
            "The XML file '{FILE}' does not fit the required schema!\n{FILE}:1: text is not \
             allowed in <record> here: \"text\""
        )
    );
    let e = err_in("<record model=\"m\"><field name=\"n\" type=\"int\">x</field></record>");
    assert!(!e.kind().is_schema());
    assert_eq!(
        e.to_string(),
        format!(
            "while parsing {FILE}:1, somewhere inside\n<record model=\"m\"><field name=\"n\" \
             type=\"int\">x</field></record>\ninvalid integer 'x' in <field>: invalid digit found \
             in string"
        )
    );
    let e = err("<odoo>\n<record model=\"m\"/>");
    assert!(!e.kind().is_schema());
    assert_eq!(
        e.to_string(),
        format!("while parsing {FILE}:2: unclosed <odoo> at end of file")
    );
}

#[test]
fn operation_size_is_limited() {
    let xml = format!(
        "<odoo>\n<record id=\"a\" model=\"m\"/>\n<record id=\"b\" model=\"m\"><field name=\"n\">{}</field></record></odoo>",
        "y".repeat(200)
    );
    let mut reader = XmlDataReader::new(&xml, "big.xml", "m", false)
        .with_limits(ReaderLimits::default().with_max_operation_bytes(100));
    assert!(
        matches!(reader.next(), Some(Ok(_))),
        "the small operation passes"
    );
    let e = reader.next().expect("an item").expect_err("too large");
    assert_eq!(e.kind(), &ParseErrorKind::OperationTooLarge { limit: 100 });
    assert_eq!(e.detail(), "operation is larger than 100 bytes");
    assert_eq!(e.line(), 3);
    assert_eq!(
        ReaderLimits::default().max_operation_bytes(),
        MAX_OPERATION_BYTES
    );
    // A self-closing operation over the limit is refused too.
    let e = XmlDataReader::new("<odoo><record model=\"m\"/></odoo>", "f.xml", "m", false)
        .with_limits(ReaderLimits::default().with_max_operation_bytes(5))
        .collect::<Result<Vec<_>, _>>()
        .expect_err("too large");
    assert_eq!(e.kind(), &ParseErrorKind::OperationTooLarge { limit: 5 });
}

// ------------------------------------------------------------------ CSV

#[test]
fn csv_invalid_ids_are_errors_with_physical_lines() {
    let import = loaded("id,name\np1,A\n\"bad\nid\",B\n");
    let e = import.to_record_ops(false).expect_err("invalid id");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::InvalidXmlId {
            value: "bad\nid".into(),
            problem: XmlIdProblem::Whitespace
        }
    );
    assert_eq!(e.line(), 3);
    assert_eq!(e.file(), "data/res.partner.csv");
    assert_eq!(
        e.detail(),
        "column 'id': external ID \"bad\nid\" contains whitespace"
    );
    assert_eq!(
        e.to_string(),
        "while parsing data/res.partner.csv:3: column 'id': external ID \"bad\nid\" contains whitespace"
    );
    for (csv, line, problem, detail) in [
        (
            "id,name\np1,A\n.x,B\n",
            3,
            XmlIdProblem::EmptyModule,
            "column 'id': external ID \".x\" has an empty module part",
        ),
        (
            "id,name\r\n\r\np1,A\r\nx.,B\r\n",
            4,
            XmlIdProblem::EmptyName,
            "column 'id': external ID \"x.\" has an empty name part",
        ),
        (
            "id,name\na.b.c,A\n",
            2,
            XmlIdProblem::TooManyDots,
            "column 'id': The ID reference \"a.b.c\" must contain maximum one dot. They are used to \
             refer to other modules ID, in the form: module.record_id",
        ),
        (
            "id,name\nm1,\"multi\nline\"\nok,B\n a,C\n",
            5,
            XmlIdProblem::Whitespace,
            "column 'id': external ID \" a\" contains whitespace",
        ),
    ] {
        let e = loaded(csv).to_record_ops(false).expect_err(csv);
        assert_eq!(e.line(), line, "{csv:?}");
        assert!(
            matches!(e.kind(), ParseErrorKind::InvalidXmlId { problem: p, .. } if *p == problem),
            "{csv:?}: {e:?}"
        );
        assert_eq!(e.detail(), detail, "{csv:?}");
    }
    let recs = loaded("id,name\n,A\n").to_record_ops(false).expect("valid");
    assert_eq!(recs[0].id(), None);
}

#[test]
fn csv_header_and_row_rules() {
    // Headers are kept as written; whitespace-only rows are kept, empty rows skipped.
    let import = loaded("id, name ,code\n\n ,  , \n,,\np1,A,X\n");
    assert_eq!(import.fields(), &["id", " name ", "code"]);
    assert_eq!(import.rows().len(), 2);
    assert_eq!(import.rows()[0].values(), &[" ", "  ", " "]);
    assert_eq!(import.rows()[0].line(), 3);
    assert_eq!(import.rows()[1].line(), 5);

    // Translation columns are dropped even with leading spaces (`l10n_mk` header).
    let import = loaded("id,name, \"name@mk\"\np1,A,B\n");
    assert_eq!(import.fields(), &["id", "name"]);

    // Header errors.
    let prefix = "Module loading m failed: file t.csv could not be processed:\n";
    for (csv, detail) in [
        (
            "id,,name\np1,foo,bar\n",
            "CSV header contains an empty column name",
        ),
        (
            "id,name,name\np1,foo,bar\n",
            "duplicate header column 'name' in CSV",
        ),
    ] {
        let e = parse_csv(csv, "t.csv", "m", ConvertMode::Init).expect_err(csv);
        assert_eq!(e.kind(), &ParseErrorKind::Csv);
        assert_eq!(e.line(), 1);
        assert_eq!(e.detail(), format!("{prefix}{detail}"));
    }
    let e = parse_csv("\n\nid,name,name\n", "t.csv", "m", ConvertMode::Init).expect_err("dup");
    assert_eq!(e.line(), 3);
    assert_eq!(
        e.detail(),
        format!("{prefix}duplicate header column 'name' in CSV")
    );
    let e = parse_csv("id,name\n", "", "m", ConvertMode::Init).expect_err("no model");
    assert_eq!(e.kind(), &ParseErrorKind::Csv);
    assert_eq!(
        e.detail(),
        "Module loading m failed: file  could not be processed:\ncannot derive a model name from \
         the file name \"\""
    );

    // Rows without the id column keep whitespace cells as values.
    let recs = loaded("name,code\n , \n")
        .to_record_ops(false)
        .expect("no id column");
    assert_eq!(
        recs[0].field("name").map(|f| f.value()),
        Some(&FieldValue::CsvCell(Some(" ".into())))
    );

    // Empty file; translation-only headers are refused.
    assert!(matches!(
        parse_csv("", "t.csv", "m", ConvertMode::Init),
        Ok(CsvImportResult::Empty)
    ));
    let err = parse_csv("name@fr\nx\n", "t.csv", "m", ConvertMode::Init).unwrap_err();
    assert_eq!(err.kind(), &ParseErrorKind::Csv);
    assert_eq!(err.line(), 1);
    assert_eq!(
        err.detail(),
        "Module loading m failed: file t.csv could not be processed:\n\
         CSV file has only translation columns (first: 'name@fr'); a file of only \
         translation columns is not supported until a later release (translation-only CSV)"
    );
}

#[test]
fn load_data_file_by_extension() {
    let xml = "<odoo><record id=\"r1\" model=\"res.partner\"/></odoo>";
    assert!(matches!(
        load_data_file(xml, "data/partner.XML", "base", ConvertMode::Init, false),
        Ok(DataFileContent::Xml(ops)) if ops.len() == 1
    ));
    let Ok(DataFileContent::Csv(recs)) = load_data_file(
        "id,name\np1,Bob\n",
        "res.partner.csv",
        "base",
        ConvertMode::Init,
        true,
    ) else {
        panic!("CSV expected");
    };
    assert!(recs[0].env().noupdate());
    assert!(matches!(
        load_data_file(
            "name\nx\n",
            "res.partner.csv",
            "base",
            ConvertMode::Update,
            false
        ),
        Ok(DataFileContent::Skipped { .. })
    ));
    let e = load_data_file(
        "id,name\na b,x\n",
        "res.partner.csv",
        "base",
        ConvertMode::Init,
        false,
    )
    .expect_err("invalid id");
    assert_eq!(e.line(), 2);
    assert_eq!(
        e.detail(),
        "column 'id': external ID \"a b\" contains whitespace"
    );
    let e = load_data_file("x", "data/notes.txt", "base", ConvertMode::Init, false)
        .expect_err("unknown type");
    assert_eq!(e.kind(), &ParseErrorKind::UnknownFileType);
    assert_eq!(
        e.detail(),
        "can't load unknown file type data/notes.txt (expected .xml or .csv)"
    );
}

// ------------------------------------------------------------------ translations

#[test]
fn translated_field_attribute_errors() {
    // both halves of `name@lang` are required
    for name in ["@fr", "name@"] {
        assert_eq!(
            field_kind(&format!("<field name=\"{name}\">x</field>")),
            (
                invalid_attr("field", "name"),
                "name attribute with '@' must specify both field name and language (e.g. 'name@fr_FR')".to_string()
            )
        );
    }
    // `name@lang` together with `lang=` is ambiguous
    assert_eq!(
        field_kind("<field name=\"name@fr\" lang=\"de\">x</field>"),
        (
            conflict("field"),
            "cannot specify both 'name@lang' and 'lang' attribute on <field>".to_string()
        )
    );
    // an empty `lang=` is a malformed language, not an absent one
    assert_eq!(
        field_kind("<field name=\"name\" lang=\"\">x</field>"),
        (
            invalid_attr("field", "lang"),
            "lang attribute must not be empty (e.g. lang=\"fr_FR\")".to_string()
        )
    );
}

#[test]
fn a_translated_field_in_an_asset_is_refused() {
    for name_lang in ["name=\"x@fr\"", "name=\"x\" lang=\"fr\""] {
        assert_eq!(
            kind_of(&format!(
                "<asset id=\"a\" name=\"A\"><bundle>b</bundle><path>p</path><field {name_lang}>v</field></asset>"
            )),
            (
                refused("field"),
                "translated <field name=\"x\"> (name@lang or lang=) is not allowed in <asset>"
                    .to_string()
            )
        );
    }
}

#[test]
fn csv_translation_header_errors_name_the_cell_and_the_line() {
    let prefix = "Module loading m failed: file t.csv could not be processed:\n";
    for cell in ["@fr_FR", "name@", "@"] {
        let e = parse_csv(
            &format!("id,{cell}\nx,y\n"),
            "t.csv",
            "m",
            ConvertMode::Init,
        )
        .expect_err("malformed translation header");
        assert_eq!(e.kind(), &ParseErrorKind::Csv);
        assert_eq!(e.line(), 1);
        assert_eq!(
            e.detail(),
            format!(
                "{prefix}CSV header column '{cell}' must name both a field and a language (e.g. 'name@fr_FR')"
            )
        );
    }
    let import = loaded("id,name,name@fr_FR\nx,A,Aa\n");
    assert_eq!(import.translations().len(), 1);
    let tr = &import.translations()[0];
    assert_eq!(
        (tr.field(), tr.lang(), tr.value(), tr.id(), tr.line()),
        ("name", "fr_FR", "Aa", Some("x"), 2)
    );
}

#[test]
fn csv_header_pragma_noupdate_recognition() {
    for (header, expected) in [
        ("# noupdate: 1\n", true),
        ("# noupdate: true\n", true),
        ("# noupdate: on\n", true),
        ("# noupdate: yes\n", true),
        ("# noupdate=1\n", true),
        ("   # noupdate: 1\n", true),
        ("# noupdate: 0\n", false),
        ("# noupdate: false\n", false),
        ("# noupdate: off\n", false),
        ("# noupdate: no\n", false),
    ] {
        let content = format!("{header}id,name\np1,Bob\n");
        let import = match parse_csv(&content, "res.partner.csv", "base", ConvertMode::Init)
            .expect("valid pragma")
        {
            CsvImportResult::Loaded(i) => i,
            other => panic!("expected Loaded, got {other:?}"),
        };
        assert_eq!(
            import.noupdate(),
            expected,
            "header '{header}' must set noupdate={expected}"
        );
        let ops = import.to_record_ops(false).unwrap();
        assert_eq!(ops[0].env().noupdate(), expected);
    }
}

#[test]
fn csv_header_pragma_rejection_of_malformed_directives() {
    let prefix = "Module loading base failed: file res.partner.csv could not be processed:\n";
    // Invalid boolean value
    let err = parse_csv(
        "# noupdate: maybe\nid,name\np1,Bob\n",
        "res.partner.csv",
        "base",
        ConvertMode::Init,
    )
    .unwrap_err();
    assert_eq!(err.kind(), &ParseErrorKind::Csv);
    assert_eq!(err.line(), 1);
    assert_eq!(
        err.detail(),
        format!(
            "{prefix}invalid boolean value 'maybe' for directive 'noupdate'; expected 1/0, true/false, yes/no, on/off"
        )
    );

    // Missing value / delimiter
    let err = parse_csv(
        "# noupdate\nid,name\np1,Bob\n",
        "res.partner.csv",
        "base",
        ConvertMode::Init,
    )
    .unwrap_err();
    assert_eq!(err.kind(), &ParseErrorKind::Csv);
    assert_eq!(err.line(), 1);
    assert_eq!(
        err.detail(),
        format!(
            "{prefix}malformed header directive '#noupdate'; expected 'key: value' or 'key=value'"
        )
    );

    // Unknown directive
    let err = parse_csv(
        "# unknown_flag: 1\nid,name\np1,Bob\n",
        "res.partner.csv",
        "base",
        ConvertMode::Init,
    )
    .unwrap_err();
    assert_eq!(err.kind(), &ParseErrorKind::Csv);
    assert_eq!(err.line(), 1);
    assert_eq!(
        err.detail(),
        format!("{prefix}unknown header directive 'unknown_flag'")
    );

    // Stray comment before header (without key/value delimiter)
    let err = parse_csv(
        "# this is a comment\nid,name\np1,Bob\n",
        "res.partner.csv",
        "base",
        ConvertMode::Init,
    )
    .unwrap_err();
    assert_eq!(err.kind(), &ParseErrorKind::Csv);
    assert_eq!(err.line(), 1);
    assert_eq!(
        err.detail(),
        format!(
            "{prefix}malformed header directive '#this is a comment'; expected 'key: value' or 'key=value'"
        )
    );
}

#[test]
fn csv_preserves_hashes_in_data_and_multiline_quoted_cells() {
    // 1. Data cell starting with # is not treated as a comment or dropped
    let content = "id,name,color\np1,Alice,#FF0000\np2,#leading,green\n";
    let import = match parse_csv(content, "res.partner.csv", "base", ConvertMode::Init).unwrap() {
        CsvImportResult::Loaded(i) => i,
        other => panic!("expected Loaded, got {other:?}"),
    };
    assert!(!import.noupdate());
    assert_eq!(import.rows().len(), 2);
    assert_eq!(import.rows()[0].values(), ["p1", "Alice", "#FF0000"]);
    assert_eq!(import.rows()[1].values(), ["p2", "#leading", "green"]);

    // 2. Multiline cell with embedded # noupdate: 1 does NOT trigger noupdate or drop lines
    let multiline_content = "id,desc\np1,\"Line 1\n# noupdate: 1\nLine 3\"\n";
    let ml_import = match parse_csv(
        multiline_content,
        "res.partner.csv",
        "base",
        ConvertMode::Init,
    )
    .unwrap()
    {
        CsvImportResult::Loaded(i) => i,
        other => panic!("expected Loaded, got {other:?}"),
    };
    assert!(
        !ml_import.noupdate(),
        "multiline cell containing '# noupdate: 1' must not trigger noupdate"
    );
    assert_eq!(ml_import.rows().len(), 1);
    assert_eq!(
        ml_import.rows()[0].values(),
        ["p1", "Line 1\n# noupdate: 1\nLine 3"]
    );
}

#[test]
fn csv_header_pragma_physical_line_numbering() {
    let prefix = "Module loading base failed: file res.partner.csv could not be processed:\n";

    // Duplicate header error on line 2
    let content_dup = "# noupdate: 1\nid,name,name\np1,A,B\n";
    let err_dup = parse_csv(content_dup, "res.partner.csv", "base", ConvertMode::Init).unwrap_err();
    assert_eq!(err_dup.kind(), &ParseErrorKind::Csv);
    assert_eq!(err_dup.line(), 2);
    assert_eq!(
        err_dup.detail(),
        format!("{prefix}duplicate header column 'name' in CSV")
    );

    // Row column mismatch error on line 3
    let content_mismatch = "# noupdate: 1\nid,name\np1\n";
    let err_mismatch = parse_csv(
        content_mismatch,
        "res.partner.csv",
        "base",
        ConvertMode::Init,
    )
    .unwrap_err();
    assert_eq!(err_mismatch.kind(), &ParseErrorKind::Csv);
    assert_eq!(err_mismatch.line(), 3);
    assert_eq!(
        err_mismatch.detail(),
        format!("{prefix}line 3: row has 1 columns, expected 2")
    );
}
