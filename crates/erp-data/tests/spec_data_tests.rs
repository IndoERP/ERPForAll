//! SPEC-DATA cases on the reader side. Database effects are/S3.

use erp_data::{
    ConvertMode, CsvImportResult, DataOperation, DeleteTarget, Expr, FieldValue, FunctionArg,
    GroupCommand, ParseErrorKind, RootActive, ShorthandTarget, ValueOp, XmlDataReader,
    XmlIdProblem, parse_csv, parse_xml_str,
};

#[cfg(test)]
mod support {
    use erp_data::{
        AssetOp, DataOperation, DeleteOp, FunctionOp, MenuItemOp, ParseError, RecordOp, TemplateOp,
        XmlId, parse_xml_str,
    };

    pub const FILE: &str = "addons/m/data/t.xml";

    pub fn ops(xml: &str) -> Vec<DataOperation> {
        parse_xml_str(xml, FILE, "m", false).expect("file parses")
    }

    pub fn err(xml: &str) -> ParseError {
        parse_xml_str(xml, FILE, "m", false).expect_err("file must be refused")
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

    pub fn delete(op: &DataOperation) -> &DeleteOp {
        match op {
            DataOperation::Delete(d) => d,
            other => panic!("expected a delete, got {other:?}"),
        }
    }

    pub fn value<'r>(rec: &'r RecordOp, name: &str) -> &'r erp_data::FieldValue {
        rec.field(name)
            .unwrap_or_else(|| panic!("field {name} missing"))
            .value()
    }
}

use support::*;

#[test]
fn spec_data_001_schema_validation_before_loading() {
    // Unknown top-level tag: schema error naming file, line and element.
    let e = err("<odoo>\n    <foo id=\"bar\"/>\n</odoo>");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::UnknownElement {
            parent: "odoo".into(),
            tag: "foo".into()
        }
    );
    assert!(e.kind().is_schema());
    assert_eq!(e.line(), 2);
    assert_eq!(e.file(), FILE);
    assert_eq!(
        e.to_string(),
        format!(
            "The XML file '{FILE}' does not fit the required schema!\n{FILE}:2: element <foo> is \
             not allowed in <odoo>; allowed: erp, odoo, openerp, data, record, template, asset, \
             menuitem, delete, function"
        )
    );

    // Bad root.
    let e = err("<bad_root><record id=\"r1\" model=\"res.partner\"/></bad_root>");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::InvalidRootTag {
            tag: "bad_root".into()
        }
    );
    assert_eq!(
        e.detail(),
        "root XML tag must be <erp> (or legacy <odoo>, <openerp>, <data>), found <bad_root>"
    );

    // Per-element attribute allow-list.
    let e = err("<odoo>\n<record id=\"r1\" model=\"res.partner\" unknown_attr=\"1\"/>\n</odoo>");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::UnknownAttribute {
            tag: "record".into(),
            attr: "unknown_attr".into()
        }
    );
    assert_eq!(e.line(), 2);
    assert_eq!(
        e.detail(),
        "unknown attribute 'unknown_attr' on <record>; allowed: id, model, forcecreate, uid, context"
    );

    // Required attribute.
    let e = err("<odoo><function name=\"f\"/></odoo>");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::MissingRequiredAttribute {
            tag: "function".into(),
            attr: "model".into()
        }
    );
    assert_eq!(
        e.detail(),
        "<function> requires a non-empty 'model' attribute"
    );

    // Per-element child allow-list.
    let e = err("<odoo>\n<record model=\"m\">\n<value>1</value>\n</record>\n</odoo>");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::UnknownElement {
            parent: "record".into(),
            tag: "value".into()
        }
    );
    assert_eq!(e.line(), 3);
    assert_eq!(
        e.detail(),
        "element <value> is not allowed in <record>; only <field> is allowed"
    );

    // Nested containers are allowed, with their attributes.
    let all =
        ops("<openerp><data noupdate=\"1\"><odoo><record model=\"m\"/></odoo></data></openerp>");
    assert_eq!(all.len(), 1);
}

#[test]
fn spec_data_024_error_context_from_real_parse_errors() {
    let xml = "<odoo>\n  <record id=\"r1\" model=\"res.partner\">\n    <field name=\"name\">A</field>\n    \
               <field name=\"count\" type=\"int\">4.82</field>\n  </record>\n</odoo>";
    let e = err(xml);
    assert_eq!(
        e.kind(),
        &ParseErrorKind::InvalidValue {
            tag: "field".into()
        }
    );
    assert_eq!(e.line(), 4);
    assert_eq!(
        e.detail(),
        "invalid integer '4.82' in <field>: invalid digit found in string"
    );
    let node = e.node().expect("value errors carry the operation source");
    assert!(
        node.starts_with("<record id=\"r1\" model=\"res.partner\">"),
        "{node}"
    );
    assert!(node.ends_with("</record>"), "{node}");
    let shown = e.to_string();
    assert!(
        shown.starts_with(&format!(
            "while parsing {FILE}:4, somewhere inside\n<record id=\"r1\""
        )),
        "{shown}"
    );
    assert!(shown.ends_with("invalid integer '4.82' in <field>: invalid digit found in string"));

    let e = err("<odoo><record model=\"m\" x=\"1\"/></odoo>");
    assert_eq!(e.node(), None);
}

#[test]
fn spec_data_011_xml_id_maximum_one_dot() {
    // Valid external ID with maximum one dot is accepted.
    let ok = ops("<odoo><record id=\"a.b\" model=\"res.partner\"/></odoo>");
    assert_eq!(ok.len(), 1);
    assert_eq!(record(&ok[0]).id(), Some(&xid("a.b")));

    let xml = "<odoo><record id=\"a.b.c\" model=\"res.partner\"/></odoo>";
    let e = err(xml);
    assert_eq!(
        e.kind(),
        &ParseErrorKind::InvalidXmlId {
            value: "a.b.c".into(),
            problem: XmlIdProblem::TooManyDots,
        }
    );
    assert_eq!(e.line(), 1);
    assert_eq!(
        e.detail(),
        "attribute 'id' on <record>: The ID reference \"a.b.c\" must contain maximum one dot. \
         They are used to refer to other modules ID, in the form: module.record_id"
    );
    let node = e.node().expect("value error carries operation node");
    assert_eq!(node, "<record id=\"a.b.c\" model=\"res.partner\"/>");
}

#[test]
fn spec_data_012_field_value_forms() {
    let xml = r#"<odoo>
    <record id="r_all_fields" model="res.partner">
        <field name="name">Text Name</field>
        <field name="groups_id" eval="[Command.set([ref('base.group_user')])]"/>
        <field name="company_id" ref="base.main_company"/>
        <field name="country_id" search="[('code','=','BE')]" model="res.country" use="id"/>
        <field name="parent_id" eval="obj().search([], limit=1).id" model="res.partner"/>
        <field name="count" type="int">None</field>
        <field name="max_count" type="int"> 42  </field>
        <field name="rate" type="float">3.25</field>
        <field name="items" type="list">
            <value>foo</value>
            <value type="int">5</value>
            <value type="float">4.76</value>
            <value type="int">None</value>
        </field>
        <field name="pair" type="tuple"><value>a</value><value eval="2"/></field>
        <field name="attachment" type="file">static/src/img/logo.png</field>
        <field name="image_1920" type="bytes" file="m/static/img/a.png"/>
        <field name="legacy" type="base64" file="m/static/img/b.png"/>
        <field name="body" file="m/data/body.txt"/>
        <field name="body2" type="char" file="m/data/body2.txt"/>
        <field name="explicit_char" type="char">None</field>
        <field name="name@fr_FR">Nom</field>
    </record>
</odoo>"#;
    let all = ops(xml);
    assert_eq!(all.len(), 1);
    let rec = record(&all[0]);
    assert_eq!(rec.id(), Some(&xid("r_all_fields")));
    assert_eq!(rec.model(), "res.partner");
    assert_eq!(rec.line(), 2);
    assert_eq!(value(rec, "name"), &FieldValue::Text("Text Name".into()));
    assert_eq!(
        value(rec, "groups_id"),
        &FieldValue::Eval {
            expr: Expr::new("[Command.set([ref('base.group_user')])]"),
            model: None
        }
    );
    assert_eq!(
        value(rec, "company_id"),
        &FieldValue::Ref(xid("base.main_company"))
    );
    assert_eq!(
        value(rec, "country_id"),
        &FieldValue::Search {
            domain: Expr::new("[('code','=','BE')]"),
            model: Some("res.country".into()),
            use_field: Some("id".into()),
        }
    );
    assert_eq!(
        value(rec, "parent_id"),
        &FieldValue::Eval {
            expr: Expr::new("obj().search([], limit=1).id"),
            model: Some("res.partner".into())
        }
    );
    assert_eq!(value(rec, "count"), &FieldValue::Int(None));
    assert_eq!(value(rec, "max_count"), &FieldValue::Int(Some(42)));
    assert_eq!(value(rec, "rate"), &FieldValue::Float(3.25));
    assert_eq!(
        value(rec, "items"),
        &FieldValue::List(vec![
            ValueOp::Text("foo".into()),
            ValueOp::Int(Some(5)),
            ValueOp::Float(4.76),
            ValueOp::Int(None),
        ])
    );
    assert_eq!(
        value(rec, "pair"),
        &FieldValue::Tuple(vec![
            ValueOp::Text("a".into()),
            ValueOp::Eval {
                expr: Expr::new("2"),
                model: None
            },
        ])
    );
    // type="file" is a module path; bare file= is the file's text; bytes/base64 read file=.
    assert_eq!(
        value(rec, "attachment"),
        &FieldValue::ModulePath("static/src/img/logo.png".into())
    );
    assert_eq!(
        value(rec, "image_1920"),
        &FieldValue::FileBytes("m/static/img/a.png".into())
    );
    assert_eq!(
        value(rec, "legacy"),
        &FieldValue::FileBase64("m/static/img/b.png".into())
    );
    assert_eq!(
        value(rec, "body"),
        &FieldValue::FileText("m/data/body.txt".into())
    );
    assert_eq!(
        value(rec, "body2"),
        &FieldValue::FileText("m/data/body2.txt".into())
    );
    assert_eq!(
        value(rec, "explicit_char"),
        &FieldValue::Text("None".into())
    );
    // Translation fields are not loaded by the record.
    assert!(rec.field("name@fr_FR").is_none());
    assert_eq!(rec.fields().len(), 16);
    assert_eq!(rec.field("rate").map(|f| f.line()), Some(10));
}

#[test]
fn spec_data_014_one2many_sub_records() {
    let xml = r#"<odoo>
    <data context="{'c': 1}">
    <record id="r_parent" model="test_tools.convert" context="{'p': 1}">
        <field name="name">Parent Record</field>
        <field name="usered_ids">
            <record id="s_child" model="test_tools.convert.usered" context="{'s': 1}">
                <field name="name">Sub Child</field>
            </record>
            <record model="test_tools.convert.usered"/>
        </field>
    </record>
    </data>
</odoo>"#;
    let all = ops(xml);
    assert_eq!(all.len(), 1);
    let rec = record(&all[0]);
    let FieldValue::SubRecords(subs) = value(rec, "usered_ids") else {
        panic!("expected sub-records");
    };
    assert_eq!(subs.len(), 2);
    assert_eq!(subs[0].id(), Some(&xid("s_child")));
    assert_eq!(subs[0].model(), "test_tools.convert.usered");
    assert_eq!(subs[0].line(), 6);
    assert_eq!(
        value(&subs[0], "name"),
        &FieldValue::Text("Sub Child".into())
    );
    assert_eq!(subs[1].id(), None);
    assert_eq!(
        subs[0].env().context_chain(),
        &[Expr::new("{'c': 1}"), Expr::new("{'s': 1}")]
    );
    assert_eq!(
        rec.env().context_chain(),
        &[Expr::new("{'c': 1}"), Expr::new("{'p': 1}")]
    );

    // Text next to sub-records is refused (the schema allows one or the other).
    let e = err(
        "<odoo><record model=\"m\"><field name=\"o\">x<record model=\"l\"/></field></record></odoo>",
    );
    assert_eq!(
        e.kind(),
        &ParseErrorKind::UnexpectedText {
            tag: "field".into()
        }
    );
    assert_eq!(e.detail(), "text is not allowed in <field> here: \"x\"");
}

#[test]
fn spec_data_015_xml_and_html_fields() {
    // Two roots: XML declaration, wrapped in <data>, `%(xid)d` kept for.
    let xml = "<odoo><record id=\"v\" model=\"ir.ui.view\"><field name=\"arch\" type=\"xml\">\n  \
               <tree><field name=\"name\"/></tree>\n  <form><field name=\"o\">%(m.other)d</field></form>\n\
               </field></record></odoo>";
    let all = ops(xml);
    assert_eq!(
        value(record(&all[0]), "arch"),
        &FieldValue::Xml(
            "<?xml version=\"1.0\"?>\n<data><tree><field name=\"name\"/></tree>\n  \
             <form><field name=\"o\">%(m.other)d</field></form>\n</data>"
                .into()
        )
    );

    // One root (comments do not count): no wrap.
    let xml = "<odoo><record id=\"v\" model=\"ir.ui.view\"><field name=\"arch\" type=\"xml\">\n  \
               <!-- c --><form string=\"S\"><field name=\"name\"/></form>\n</field></record></odoo>";
    let all = ops(xml);
    assert_eq!(
        value(record(&all[0]), "arch"),
        &FieldValue::Xml(
            "<?xml version=\"1.0\"?>\n<!-- c --><form string=\"S\"><field name=\"name\"/></form>\n"
                .into()
        )
    );

    // HTML: empty elements as <t></t>, void elements without end tag, comments kept, script
    // text unescaped, leading text dropped and tails kept as lxml's method="html" does.
    let xml = "<odoo><record id=\"h\" model=\"m\"><field name=\"description\" type=\"html\">\n  \
               <p>Welcome &amp; <b>ERPForAll</b><t/><br/><img src=\"x.png\"/><!-- note -->\
               <script>if (a &lt; b) {}</script></p>\n  <div class=\"a&quot;b\"/>\n</field></record></odoo>";
    let all = ops(xml);
    assert_eq!(
        value(record(&all[0]), "description"),
        &FieldValue::Html(
            "<p>Welcome &amp; <b>ERPForAll</b><t></t><br><img src=\"x.png\"><!-- note -->\
             <script>if (a < b) {}</script></p>\n  <div class=\"a&quot;b\"></div>\n"
                .into()
        )
    );

    let xml = "<odoo><record id=\"h\" model=\"m\"><field name=\"f\" type=\"html\"><parent>\n    \
               <t t-if=\"True\">\n        <t t-out=\"'text'\"/>\n    </t>\n</parent></field></record></odoo>";
    let all = ops(xml);
    assert_eq!(
        value(record(&all[0]), "f"),
        &FieldValue::Html(
            "<parent>\n    <t t-if=\"True\">\n        <t t-out=\"'text'\"></t>\n    </t>\n</parent>"
                .into()
        )
    );
}

#[test]
fn spec_data_017_uid_and_context_attributes() {
    let xml = r#"<odoo uid="base.user_root" context="{'tz': 'A'}">
    <record id="rec_outer" model="res.partner"/>
    <record id="rec_inner" model="res.partner" uid="base.user_admin" context="{'tz': 'B'}"/>
    <data uid="base.user_portal" context="{'lang': 'fr'}" noupdate="1">
        <function model="res.partner" name="action_do" context="{'x': 1}"/>
        <menuitem id="menu_x" name="X"/>
        <delete model="res.partner" id="base.p1"/>
        <template id="tpl" context="{'t': 1}"><div/></template>
        <asset id="as" name="As"><bundle>web.assets</bundle><path>m/static/a.js</path></asset>
    </data>
</odoo>"#;
    let all = ops(xml);
    assert_eq!(all.len(), 7);
    let a = Expr::new("{'tz': 'A'}");
    let lang = Expr::new("{'lang': 'fr'}");

    let outer = record(&all[0]).env();
    assert_eq!(outer.uid(), Some(&xid("base.user_root")));
    assert_eq!(outer.context_chain(), std::slice::from_ref(&a));
    assert!(!outer.noupdate());

    // Inner attributes: innermost uid wins, contexts chained outer then inner.
    let inner = record(&all[1]).env();
    assert_eq!(inner.uid(), Some(&xid("base.user_admin")));
    assert_eq!(
        inner.context_chain(),
        &[a.clone(), Expr::new("{'tz': 'B'}")]
    );

    let f = function(&all[2]).env();
    assert_eq!(f.uid(), Some(&xid("base.user_portal")));
    assert_eq!(
        f.context_chain(),
        &[a.clone(), lang.clone(), Expr::new("{'x': 1}")]
    );
    assert!(f.noupdate());

    assert_eq!(menu(&all[3]).id(), &xid("menu_x"));
    assert_eq!(
        delete(&all[4]).target(),
        &erp_data::DeleteTarget::Id(xid("base.p1"))
    );
    assert_eq!(asset(&all[6]).bundle(), "web.assets");
    let container_chain = [a.clone(), lang.clone()];
    for op in [&all[3], &all[4], &all[6]] {
        assert_eq!(op.env().uid(), Some(&xid("base.user_portal")), "{op:?}");
        assert_eq!(op.env().context_chain(), &container_chain, "{op:?}");
        assert!(op.env().noupdate(), "{op:?}");
    }
    let t = template(&all[5]).env();
    assert_eq!(t.uid(), Some(&xid("base.user_portal")));
    assert_eq!(t.context_chain(), &[a, lang, Expr::new("{'t': 1}")]);
}

#[test]
fn spec_data_018_function_arguments() {
    let xml = r#"<odoo>
    <function model="res.partner" name="setup_initial_data" eval="[1, 2]"/>
    <function model="res.partner" name="with_args">
        <value>extra_arg</value>
        <value eval="ref('base.main_company')"/>
        <value type="int">5</value>
        <value name="foo">bar</value>
        <value name="context" eval="{'test': True}"/>
        <function model="res.partner" name="get_nested_val" eval="[3]"/>
    </function>
</odoo>"#;
    let all = ops(xml);
    assert_eq!(all.len(), 2);
    let f1 = function(&all[0]);
    assert_eq!(f1.eval(), Some(&Expr::new("[1, 2]")));
    assert!(f1.args().is_empty());

    let f2 = function(&all[1]);
    assert_eq!(f2.model(), "res.partner");
    assert_eq!(f2.name(), "with_args");
    assert_eq!(f2.args().len(), 4);
    assert_eq!(
        f2.args()[0],
        FunctionArg::Value(ValueOp::Text("extra_arg".into()))
    );
    assert_eq!(
        f2.args()[1],
        FunctionArg::Value(ValueOp::Eval {
            expr: Expr::new("ref('base.main_company')"),
            model: None
        })
    );
    assert_eq!(f2.args()[2], FunctionArg::Value(ValueOp::Int(Some(5))));
    let FunctionArg::Function(nested) = &f2.args()[3] else {
        panic!("expected a nested function");
    };
    assert_eq!(nested.name(), "get_nested_val");
    assert_eq!(nested.eval(), Some(&Expr::new("[3]")));
    assert_eq!(nested.env(), f2.env());
    assert_eq!(f2.kwargs().len(), 2);
    assert_eq!(f2.kwarg("foo"), Some(&ValueOp::Text("bar".into())));
    assert_eq!(
        f2.kwarg("context"),
        Some(&ValueOp::Eval {
            expr: Expr::new("{'test': True}"),
            model: None
        })
    );
}

#[test]
fn spec_data_019_forbidden_function_name() {
    let e = err("<odoo><function model=\"res.partner\" name=\"__init__\"/></odoo>");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::ForbiddenName {
            name: "__init__".into()
        }
    );
    assert_eq!(e.line(), 1);
    assert_eq!(e.detail(), "Access to forbidden name '__init__'");
    assert_eq!(
        e.node(),
        Some("<function model=\"res.partner\" name=\"__init__\"/>")
    );

    // Unsafe attributes (e.g. from Python's restricted execution list like mro) are also forbidden.
    let e2 = err("<odoo><function model=\"res.partner\" name=\"mro\"/></odoo>");
    assert_eq!(
        e2.kind(),
        &ParseErrorKind::ForbiddenName { name: "mro".into() }
    );
    assert_eq!(e2.line(), 1);
    assert_eq!(e2.detail(), "Access to forbidden name 'mro'");
    assert_eq!(
        e2.node(),
        Some("<function model=\"res.partner\" name=\"mro\"/>")
    );
}

#[test]
fn spec_data_020_delete_operation_parsing() {
    let xml = r#"<odoo>
    <data noupdate="1">
        <delete model="res.partner.category" id="m.gone"/>
        <delete model="res.partner.category" search="[('name','=','foo')]"/>
    </data>
</odoo>"#;
    let all = ops(xml);
    assert_eq!(all.len(), 2);

    let d1 = delete(&all[0]);
    assert_eq!(d1.model(), "res.partner.category");
    assert_eq!(d1.target(), &DeleteTarget::Id(xid("m.gone")));
    assert!(d1.env().noupdate());
    assert_eq!(d1.line(), 3);

    let d2 = delete(&all[1]);
    assert_eq!(d2.model(), "res.partner.category");
    assert_eq!(
        d2.target(),
        &DeleteTarget::Search(Expr::new("[('name','=','foo')]"))
    );
    assert!(d2.env().noupdate());
    assert_eq!(d2.line(), 4);

    // Conflicting attributes: both id and search
    let e_both = err(
        "<odoo><delete model=\"res.partner\" id=\"m.x\" search=\"[('name','=','a')]\"/></odoo>",
    );
    assert_eq!(
        e_both.kind(),
        &ParseErrorKind::ConflictingAttributes {
            tag: "delete".into()
        }
    );
    assert_eq!(
        e_both.detail(),
        "<delete> takes either 'id' or 'search', not both (import_xml.rng)"
    );

    // Missing attributes: neither id nor search
    let e_neither = err("<odoo><delete model=\"res.partner\"/></odoo>");
    assert_eq!(
        e_neither.kind(),
        &ParseErrorKind::MissingRequiredAttribute {
            tag: "delete".into(),
            attr: "id".into(),
        }
    );
    assert_eq!(
        e_neither.detail(),
        "<delete> requires a non-empty 'id' or 'search' attribute"
    );
}

#[test]
fn spec_data_021_menuitem_rules() {
    let xml = r#"<odoo>
    <menuitem id="root" name="Root" web_icon="m,static/i.png" sequence="5">
        <menuitem id="child" action="act" groups="base.group_user,-base.group_portal"/>
        <menuitem id="hidden" active="0"/>
    </menuitem>
    <menuitem id="with_parent" parent="root" action="base.act2"/>
</odoo>"#;
    let all = ops(xml);
    assert_eq!(all.len(), 2);
    let root = menu(&all[0]);
    assert_eq!(root.id(), &xid("root"));
    assert_eq!(root.name(), Some("Root"));
    assert_eq!(root.web_icon(), Some("m,static/i.png"));
    assert_eq!(root.parent(), None);
    assert_eq!(root.sequence(), Some(5));
    assert_eq!(root.children().len(), 2);

    let child = &root.children()[0];
    assert_eq!(child.id(), &xid("child"));
    assert_eq!(child.parent(), Some(&xid("root")));
    assert_eq!(child.action(), Some(&xid("act")));
    assert_eq!(child.name(), None);
    assert_eq!(child.line(), 3);
    assert_eq!(
        child.groups(),
        &[
            GroupCommand::Link(xid("base.group_user")),
            GroupCommand::Unlink(xid("base.group_portal")),
        ]
    );
    assert_eq!(root.children()[1].active(), Some(false));

    let wp = menu(&all[1]);
    assert_eq!(wp.parent(), Some(&xid("root")));
    assert_eq!(wp.web_icon(), None);
    assert_eq!(wp.action(), Some(&xid("base.act2")));

    let e = err("<odoo><menuitem id=\"x\" parent=\"root\" web_icon=\"i.png\"/></odoo>");
    assert_eq!(
        e.kind(),
        &ParseErrorKind::ConflictingAttributes {
            tag: "menuitem".into()
        }
    );
    assert_eq!(
        e.detail(),
        "a <menuitem> with 'parent' cannot have a 'web_icon' (import_xml.rng)"
    );
}

#[test]
fn spec_data_022_template_transformation() {
    let xml = r#"<odoo>
    <template id="t1" name="T"><div>Hello</div></template>
    <template id="t2" inherit_id="m.t1" priority="20"><xpath expr="." position="inside"><span>Ext</span></xpath></template>
    <template id="t3" inherit_id="m.t1" primary="True"><p>Primary</p></template>
    <template id="t4" active="False" groups="base.group_user,base.group_portal"><b>Inactive</b></template>
</odoo>"#;
    let all = ops(xml);
    assert_eq!(all.len(), 4);

    let t1 = template(&all[0]);
    assert_eq!(t1.id(), &xid("t1"));
    assert_eq!(t1.full_id(), "m.t1");
    assert_eq!(
        t1.arch(),
        "<t name=\"T\" t-name=\"m.t1\"><div>Hello</div></t>"
    );
    let r1 = t1.to_record_op(ShorthandTarget::Standard, RootActive::Apply);
    assert_eq!(r1.model(), "ir.ui.view");
    assert_eq!(r1.id(), Some(&xid("t1")));
    let names: Vec<&str> = r1.fields().iter().map(|f| f.name()).collect();
    assert_eq!(names, ["name", "key", "type", "arch"]);
    assert_eq!(value(&r1, "name"), &FieldValue::Text("T".into()));
    assert_eq!(value(&r1, "key"), &FieldValue::Text("m.t1".into()));
    assert_eq!(value(&r1, "type"), &FieldValue::Text("qweb".into()));
    assert_eq!(
        value(&r1, "arch"),
        &FieldValue::Xml(
            "<?xml version=\"1.0\"?>\n<t name=\"T\" t-name=\"m.t1\"><div>Hello</div></t>".into()
        )
    );
    assert_eq!(
        t1.to_record_op(ShorthandTarget::Theme, RootActive::Apply)
            .model(),
        "theme.ir.ui.view"
    );

    let t2 = template(&all[1]);
    assert_eq!(t2.inherit_id(), Some(&xid("m.t1")));
    assert_eq!(t2.priority(), Some(20));
    assert_eq!(
        t2.arch(),
        "<data><xpath expr=\".\" position=\"inside\"><span>Ext</span></xpath></data>"
    );
    let r2 = t2.to_record_op(ShorthandTarget::Standard, RootActive::Apply);
    assert_eq!(value(&r2, "priority"), &FieldValue::Int(Some(20)));
    assert_eq!(value(&r2, "inherit_id"), &FieldValue::Ref(xid("m.t1")));
    assert!(r2.field("mode").is_none());

    let t3 = template(&all[2]);
    assert!(t3.primary());
    assert_eq!(
        t3.arch(),
        "<data><p>Primary</p><xpath expr=\".\" position=\"attributes\">\
         <attribute name=\"t-name\">m.t3</attribute></xpath></data>"
    );
    let r3 = t3.to_record_op(ShorthandTarget::Standard, RootActive::Apply);
    assert_eq!(value(&r3, "mode"), &FieldValue::Text("primary".into()));

    let t4 = template(&all[3]);
    assert_eq!(t4.active(), Some(false));
    assert_eq!(t4.arch(), "<t t-name=\"m.t4\"><b>Inactive</b></t>");
    let r4 = t4.to_record_op(ShorthandTarget::Standard, RootActive::Apply);
    assert_eq!(
        value(&r4, "active"),
        &FieldValue::Eval {
            expr: Expr::new("False"),
            model: None
        }
    );
    assert_eq!(
        value(&r4, "group_ids"),
        &FieldValue::Eval {
            expr: Expr::new("[Command.set([ref('base.group_user'), ref('base.group_portal')])]"),
            model: None
        }
    );
    let r4_update = t4.to_record_op(ShorthandTarget::Standard, RootActive::Skip);
    assert!(r4_update.field("active").is_none());
}

#[test]
fn spec_data_023_auto_sequence_flag_per_container() {
    let xml = r#"<odoo auto_sequence="1">
    <record id="r1" model="res.partner"/>
    <data>
        <record id="r2" model="res.partner"/>
    </data>
    <data auto_sequence="0">
        <record id="r3" model="res.partner"/>
    </data>
    <data auto_sequence="1">
        <record id="r4" model="res.partner"/>
    </data>
</odoo>"#;
    let all = ops(xml);
    assert_eq!(all.len(), 4);

    let r1 = record(&all[0]);
    assert_eq!(r1.id(), Some(&xid("r1")));
    assert!(r1.env().auto_sequence());

    let r2 = record(&all[1]);
    assert_eq!(r2.id(), Some(&xid("r2")));
    assert!(!r2.env().auto_sequence());

    // Inner <data auto_sequence="0"> has auto_sequence = false
    let r3 = record(&all[2]);
    assert_eq!(r3.id(), Some(&xid("r3")));
    assert!(!r3.env().auto_sequence());

    // Inner <data auto_sequence="1"> has auto_sequence = true
    let r4 = record(&all[3]);
    assert_eq!(r4.id(), Some(&xid("r4")));
    assert!(r4.env().auto_sequence());
}

#[test]
fn spec_data_025_csv_file_loading() {
    let csv = "\u{FEFF}id,name,model_id:id,group_id/id,perm_read,name@fr_FR\r\n\
               access_user,\"User, Read\",base.model_res_partner,base.group_user,1,\"Nom Français\"\r\n\
               \r\n\
               access_multi,\"Line one\r\nline two\",base.model_res_partner,,0,x\r\n\
               access_ws, ,base.model_res_partner,base.group_user,1,\r\n";
    let CsvImportResult::Loaded(import) =
        parse_csv(csv, "security/ir.access.csv", "base", ConvertMode::Init).expect("CSV parses")
    else {
        panic!("expected a loaded CSV");
    };
    assert_eq!(import.model(), "ir.access");
    assert_eq!(
        import.fields(),
        &["id", "name", "model_id:id", "group_id/id", "perm_read"]
    );
    // The blank line is ignored; the translation column is dropped.
    let rows = import.rows();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].values(),
        &[
            "access_user",
            "User, Read",
            "base.model_res_partner",
            "base.group_user",
            "1"
        ]
    );
    assert_eq!(rows[1].values()[1], "Line one\r\nline two");
    assert_eq!(rows[2].values()[1], " ");
    // Physical lines (the quoted cell spans lines 4-5).
    let lines: Vec<usize> = rows.iter().map(|r| r.line()).collect();
    assert_eq!(lines, [2, 4, 6]);

    let recs = import.to_record_ops(true).expect("valid ids");
    assert_eq!(recs.len(), 3);
    assert_eq!(recs[0].id(), Some(&xid("access_user")));
    assert_eq!(recs[0].model(), "ir.access");
    assert!(recs[0].env().noupdate());
    assert_eq!(recs[1].line(), 4);
    assert_eq!(
        value(&recs[0], "model_id:id"),
        &FieldValue::CsvCell(Some("base.model_res_partner".into()))
    );
    assert_eq!(value(&recs[1], "group_id/id"), &FieldValue::CsvCell(None));
    assert!(recs[0].field("id").is_none());

    // res.partner-extra.csv → res.partner.
    let CsvImportResult::Loaded(extra) = parse_csv(
        "id,name\np1,Partner 1\n",
        "data/res.partner-extra.csv",
        "base",
        ConvertMode::Init,
    )
    .expect("CSV parses") else {
        panic!("expected a loaded CSV");
    };
    assert_eq!(extra.model(), "res.partner");

    let e = parse_csv(
        "id,name\np1,Partner 1\np2,Partner 2,Extra\n",
        "data/ragged.csv",
        "base",
        ConvertMode::Init,
    )
    .expect_err("ragged row");
    assert_eq!(e.kind(), &ParseErrorKind::Csv);
    assert_eq!(e.line(), 3);
    assert_eq!(
        e.detail(),
        "Module loading base failed: file data/ragged.csv could not be processed:\nline 3: row \
         has 3 columns, expected 2"
    );
}

#[test]
fn spec_data_026_csv_without_id_column_in_update_mode() {
    let csv = "name,code\nBelgium,BE\nFrance,FR\n";
    let skipped = parse_csv(csv, "data/res.country.csv", "base", ConvertMode::Update)
        .expect("skipped, not an error");
    assert!(matches!(skipped, CsvImportResult::Skipped { .. }));
    let init =
        parse_csv(csv, "data/res.country.csv", "base", ConvertMode::Init).expect("init loads");
    assert!(matches!(init, CsvImportResult::Loaded(_)));
}

/// The reader yields each operation when its end tag arrives; a later error does not hide the
/// operations read before it.
#[test]
fn streaming_yields_operations_before_a_later_error() {
    let xml = "<odoo>\n<record id=\"a\" model=\"m\"/>\n<record id=\"b\" model=\"m\">\
               <field name=\"n\" type=\"int\">x</field></record>\n<record id=\"c\" model=\"m\"/>\n</odoo>";
    let mut reader = XmlDataReader::new(xml, "s.xml", "m", false);
    let first = reader
        .next()
        .expect("an item")
        .expect("first record is fine");
    assert_eq!(record(&first).id(), Some(&xid("a")));
    let second = reader
        .next()
        .expect("an item")
        .expect_err("second record fails");
    assert_eq!(second.line(), 3);
    assert_eq!(
        second.kind(),
        &ParseErrorKind::InvalidValue {
            tag: "field".into()
        }
    );
    assert_eq!(
        second.detail(),
        "invalid integer 'x' in <field>: invalid digit found in string"
    );
    assert!(reader.next().is_none(), "the reader stops after an error");

    // Truncated file: the complete operation still comes first.
    let mut reader = XmlDataReader::new(
        "<odoo><record id=\"a\" model=\"m\"/><record",
        "t.xml",
        "m",
        false,
    );
    assert!(matches!(reader.next(), Some(Ok(DataOperation::Record(_)))));
    let e = reader.next().expect("an item").expect_err("truncated");
    assert_eq!(e.kind(), &ParseErrorKind::XmlSyntax);
    // After our prefix comes quick-xml's own wording for the unclosed tag.
    assert!(e.detail().starts_with("malformed XML: "), "{}", e.detail());
    assert!(reader.next().is_none());

    // Many operations: each comes out on its own.
    let mut big = String::from("<odoo>");
    for i in 0..2000 {
        big.push_str(&format!(
            "<record id=\"r{i}\" model=\"m\"><field name=\"n\">{i}</field></record>"
        ));
    }
    big.push_str("</odoo>");
    let mut count = 0;
    for op in XmlDataReader::new(&big, "big.xml", "m", false) {
        assert_eq!(
            record(&op.expect("valid")).id(),
            Some(&xid(&format!("r{count}")))
        );
        count += 1;
    }
    assert_eq!(count, 2000);
}

/// A file read entirely by `parse_xml_str` matches the streamed operations.
#[test]
fn parse_xml_str_collects_the_stream() {
    let xml = "<odoo><record id=\"a\" model=\"m\"/><delete model=\"m\" id=\"b\"/></odoo>";
    let streamed: Vec<DataOperation> = XmlDataReader::new(xml, "f.xml", "m", false)
        .collect::<Result<_, _>>()
        .expect("valid");
    let collected = parse_xml_str(xml, "f.xml", "m", false).expect("valid");
    assert_eq!(streamed, collected);
    assert_eq!(delete(&collected[1]).model(), "m");
}
