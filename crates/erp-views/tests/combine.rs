//! Combination through the public API: order, errors in view context,
//! attributes (/054), text, catalogue scoping and
//! limits.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures and helpers"
)]

use erp_views::{
    LimitKind, NodeRef, ViewCatalogue, ViewDescriptor, ViewError, ViewLimits, XpathErrorKind,
};
use std::error::Error as _;

fn view(id: i64, model: &str, arch: &str) -> ViewDescriptor {
    ViewDescriptor::new(id, format!("view{id}"), model, arch)
}

fn after(tag: &str) -> String {
    format!(r#"<xpath expr="//a1" position="after"><{tag}/></xpath>"#)
}

fn compact(node: NodeRef<'_>) -> String {
    fn write(node: NodeRef<'_>, out: &mut String) {
        out.push('<');
        out.push_str(node.tag());
        for (k, v) in node.attributes() {
            out.push_str(&format!(" {k}=\"{v}\""));
        }
        let text = node.text();
        let children: Vec<NodeRef<'_>> = node.children().collect();
        if children.is_empty() && text.trim().is_empty() {
            out.push_str("/>");
        } else {
            out.push('>');
            if !text.trim().is_empty() {
                out.push_str(text);
            }
            for child in children {
                write(child, out);
                if !child.tail().trim().is_empty() {
                    out.push_str(child.tail());
                }
            }
            out.push_str(&format!("</{}>", node.tag()));
        }
    }
    let mut out = String::new();
    write(node, &mut out);
    out
}

/// TestViewCombined.setUp
fn combined_fixture() -> Vec<ViewDescriptor> {
    vec![
        view(1, "a", "<qweb><a1/></qweb>"),
        view(2, "a", &after("a2")).extension_of(1).with_priority(5),
        view(3, "a", &after("a3")).extension_of(1),
        view(4, "a", &after("a4")).primary_of(1),
        view(5, "b", &after("b1")).primary_of(3),
        view(6, "b", &after("b2")).extension_of(5),
        view(7, "c", &after("c1")).primary_of(1),
        view(8, "c", &after("c2")).extension_of(7).with_priority(5),
        view(9, "c", &after("c3")).extension_of(8).with_priority(10),
        view(10, "d", &after("d1")).primary_of(5),
    ]
}

#[test]
fn combination_order_follows_test_view_combined() {
    let catalogue = ViewCatalogue::new(combined_fixture()).unwrap();
    let check = |id: i64, xml: &str, applied: &[i64]| {
        let combined = catalogue.combine(id).unwrap();
        assert_eq!(combined.to_xml(), xml, "view {id}");
        assert_eq!(combined.applied_view_ids(), applied, "view {id}");
    };
    // test_basic_read, test_read_from_child
    check(1, "<qweb><a1/><a3/><a2/></qweb>", &[1, 2, 3]);
    check(3, "<qweb><a1/><a3/><a2/></qweb>", &[1, 2, 3]);
    // test_read_from_child_primary
    check(4, "<qweb><a1/><a4/><a3/><a2/></qweb>", &[1, 2, 3, 4]);
    // test_cross_model_simple
    check(
        8,
        "<qweb><a1/><c3/><c2/><c1/><a3/><a2/></qweb>",
        &[1, 2, 3, 7, 8, 9],
    );
    // test_cross_model_double
    check(
        10,
        "<qweb><a1/><d1/><b2/><b1/><a3/><a2/></qweb>",
        &[1, 2, 3, 5, 6, 10],
    );

    // test_primary_after_extensions: d (primary child of extension b) applies after c; applied
    // before c, c would find no <a>.
    let tricky = ViewCatalogue::new(vec![
        view(11, "a", "<qweb><a/></qweb>"),
        view(12, "a", r#"<a position="after"><b/></a>"#).extension_of(11),
        view(13, "a", r#"<a position="after"><c/></a>"#).extension_of(11),
        view(14, "a", r#"<a position="replace"/>"#).primary_of(12),
    ])
    .unwrap();
    let d = tricky.combine(14).unwrap();
    assert_eq!(d.to_xml(), "<qweb><c/><b/></qweb>");
    assert_eq!(d.applied_view_ids(), &[11, 12, 13, 14]);
}

#[test]
fn batch_combination_equals_single_combinations() {
    // test_multi_combine adds n1..n4 to the fixture.
    let mut views = combined_fixture();
    views.extend([
        view(11, "a", "<qweb><n1/></qweb>"),
        view(
            12,
            "a",
            r#"<xpath expr="//n1" position="after"><n2/></xpath>"#,
        )
        .extension_of(11)
        .with_priority(5),
        view(
            13,
            "a",
            r#"<xpath expr="//n1" position="after"><n3/></xpath>"#,
        )
        .extension_of(11)
        .with_priority(1),
        view(
            14,
            "a",
            r#"<xpath expr="//n1" position="after"><n4/></xpath>"#,
        )
        .primary_of(13),
    ]);
    let catalogue = ViewCatalogue::new(views).unwrap();
    let a4 = catalogue.combine(4).unwrap();
    let n4 = catalogue.combine(14).unwrap();
    assert_eq!(n4.to_xml(), "<qweb><n1/><n4/><n2/><n3/></qweb>");
    assert_eq!(
        catalogue.combine_many(&[4, 14]).unwrap(),
        vec![a4.clone(), n4]
    );
    // test_multi_combine_with_same_ancestor
    let c2 = catalogue.combine(8).unwrap();
    assert_eq!(
        catalogue.combine_many(&[4, 8]).unwrap(),
        vec![a4, c2.clone()]
    );
    // a view repeated in a batch gives the same result twice
    assert_eq!(
        catalogue.combine_many(&[8, 8]).unwrap(),
        vec![c2.clone(), c2]
    );
    assert_eq!(catalogue.combine_many(&[]).unwrap(), vec![]);
}

const BOB: &str = r#"
                <form string="Base title">
                    <separator name="separator" string="Separator" colspan="4"/>
                    <footer>
                        <button name="action_archive" type="object" string="Next button" class="btn-primary"/>
                        <button string="Skip" special="cancel" class="btn-secondary"/>
                    </footer>
                </form>
            "#;

#[test]
fn extensions_of_another_model_are_not_applied() {
    let catalogue = ViewCatalogue::new(vec![
        ViewDescriptor::new(1, "bob", "ir.ui.view.custom", BOB),
        ViewDescriptor::new(
            2,
            "edmund",
            "ir.ui.view",
            r#"
                <data>
                    <form position="attributes">
                        <attribute name="string">Replacement title</attribute>
                    </form>
                    <footer position="replace">
                        <footer>
                            <button name="action_unarchive" type="object" string="New button"/>
                        </footer>
                    </footer>
                    <separator name="separator" position="replace">
                        <p>Replacement data</p>
                    </separator>
                </data>
            "#,
        )
        .extension_of(1),
        ViewDescriptor::new(
            3,
            "jake",
            "ir.ui.menu",
            r#"
                <footer position="attributes">
                    <attribute name="thing">bob</attribute>
                </footer>
            "#,
        )
        .extension_of(1)
        .with_priority(17),
    ])
    .unwrap();
    let combined = catalogue.combine(2).unwrap();
    assert_eq!(
        compact(combined.arch().root()),
        r#"<form string="Replacement title"><p>Replacement data</p><footer><button name="action_unarchive" type="object" string="New button"/></footer></form>"#
    );
    assert_eq!(combined.applied_view_ids(), &[1, 2]);
}

fn optional_views(active: [bool; 3]) -> ViewCatalogue {
    ViewCatalogue::new(vec![
        view(1, "a", "<qweb><base/></qweb>"),
        view(
            2,
            "a",
            r#"<xpath expr="//base" position="after"><v1/></xpath>"#,
        )
        .extension_of(1)
        .with_priority(10)
        .with_active(active[0]),
        view(
            3,
            "a",
            r#"<xpath expr="//base" position="after"><v2/></xpath>"#,
        )
        .extension_of(1)
        .with_priority(9)
        .with_active(active[1]),
        view(
            4,
            "a",
            r#"<xpath expr="//base" position="after"><v3/></xpath>"#,
        )
        .extension_of(1)
        .with_priority(8)
        .with_active(active[2]),
    ])
    .unwrap()
}

#[test]
fn inactive_extensions_are_skipped() {
    let xml = |active| optional_views(active).combine(1).unwrap().to_xml();
    // test_applied
    assert_eq!(xml([true, true, false]), "<qweb><base/><v1/><v2/></qweb>");
    // test_applied_state_toggle: archive v2, unarchive v3, unarchive v2
    assert_eq!(xml([true, false, false]), "<qweb><base/><v1/></qweb>");
    assert_eq!(xml([true, false, true]), "<qweb><base/><v1/><v3/></qweb>");
    assert_eq!(
        xml([true, true, true]),
        "<qweb><base/><v1/><v2/><v3/></qweb>"
    );
}

#[test]
fn an_inactive_view_on_the_requested_path_is_an_error() {
    let catalogue = optional_views([true, true, false]);
    let err = catalogue.combine(4).unwrap_err();
    assert_eq!(err, ViewError::InactiveViewOnPath(4));
    assert_eq!(
        err.to_string(),
        "View 4 on the requested inheritance path is inactive"
    );
    let root_inactive =
        ViewCatalogue::new(vec![view(1, "a", "<qweb/>").with_active(false)]).unwrap();
    assert_eq!(
        root_inactive.combine(1),
        Err(ViewError::InactiveViewOnPath(1))
    );
}

fn in_view(view_id: i64, view_name: &str, source: ViewError) -> ViewError {
    ViewError::InView {
        view_id,
        view_name: view_name.to_owned(),
        source: Box::new(source),
    }
}

#[test]
fn position_errors_carry_the_view() {
    let catalogue = ViewCatalogue::new(vec![
        view(
            1,
            "a",
            r#"<form string="Title"><field name="target"/></form>"#,
        ),
        ViewDescriptor::new(
            2,
            "invalid position",
            "a",
            r#"<field name="target" position="serious_series"><field name="whoops"/></field>"#,
        )
        .extension_of(1),
        view(
            10,
            "a",
            r#"<form string="Title"><field name="target"/></form>"#,
        ),
        ViewDescriptor::new(11, "target not found", "a", r#"<field name="targut"/>"#)
            .extension_of(10),
        view(20, "a", r#"<form><element foo="42"/></form>"#),
        ViewDescriptor::new(
            21,
            "incorrect version",
            "a",
            r#"<element foo="42" version="7.0"><field name="placeholder"/></element>"#,
        )
        .extension_of(20),
    ])
    .unwrap();

    // test_invalid_position
    let err = catalogue.combine(1).unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            "invalid position",
            ViewError::InvalidPosition("serious_series".to_owned())
        )
    );
    assert_eq!(err.to_string(), "error in view 'invalid position' (id 2)");
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("Invalid position attribute: 'serious_series'".to_owned())
    );
    // test_target_not_found
    let err = catalogue.combine(10).unwrap_err();
    assert_eq!(
        err,
        in_view(
            11,
            "target not found",
            ViewError::TargetNotFound(r#"<field name="targut">"#.to_owned())
        )
    );
    assert_eq!(
        err.source().map(ToString::to_string),
        Some(r#"Element '<field name="targut">' cannot be located in parent view"#.to_owned())
    );
    // test_incorrect_version
    assert_eq!(
        catalogue.combine(20),
        Err(in_view(
            21,
            "incorrect version",
            ViewError::TargetNotFound(r#"<element foo="42" version="7.0">"#.to_owned())
        ))
    );
}

#[test]
fn attributes_with_add_and_remove() {
    let catalogue = ViewCatalogue::new(vec![
        ViewDescriptor::new(1, "bob", "ir.ui.view", BOB),
        ViewDescriptor::new(
            2,
            "edmund",
            "ir.ui.view",
            r#"
                <data>
                    <form position="attributes">
                        <attribute name="string">Replacement title</attribute>
                    </form>
                    <footer position="replace">
                        <footer>
                            <button name="action_archive" type="object" string="New button"/>
                        </footer>
                    </footer>
                    <separator name="separator" position="replace">
                        <p>Replacement data</p>
                    </separator>
                </data>
            "#,
        )
        .extension_of(1),
        ViewDescriptor::new(
            3,
            "jake",
            "ir.ui.view",
            r#"
                <footer position="attributes">
                    <attribute name="thing">bob tata lolo</attribute>
                    <attribute name="thing" add="bibi and co" remove="tata" separator=" " />
                    <attribute name="otherthing">bob, tata,lolo</attribute>
                    <attribute name="otherthing" remove="tata, bob"/>
                </footer>
            "#,
        )
        .extension_of(1)
        .with_priority(17),
    ])
    .unwrap();
    let combined = catalogue.combine(2).unwrap();
    assert_eq!(
        compact(combined.arch().root()),
        r#"<form string="Replacement title"><p>Replacement data</p><footer thing="bob lolo bibi and co" otherthing="lolo"><button name="action_archive" type="object" string="New button"/></footer></form>"#
    );
    assert_eq!(combined.applied_view_ids(), &[1, 2, 3]);
}

const MAIN_FORM: &str = r#"
                <form>
                    <sheet>
                        <field name="name"/>
                    </sheet>
                </form>"#;

/// `test_inherit` of test_inherit_python_expression: a primary view on the main form.
fn invisible_after(arch: &str) -> Result<Option<String>, ViewError> {
    let catalogue = ViewCatalogue::new(vec![
        view(1, "res.partner", MAIN_FORM),
        ViewDescriptor::new(2, "inherit", "res.partner", arch).primary_of(1),
    ])
    .unwrap();
    let combined = catalogue.combine(2)?;
    let field = combined
        .arch()
        .root()
        .children()
        .next()
        .and_then(|sheet| sheet.children().next())
        .expect("form/sheet/field");
    Ok(field.attr("invisible").map(str::to_owned))
}

fn attrs(lines: &[&str]) -> String {
    format!(
        "<data><xpath expr=\"//field[@name='name']\" position=\"attributes\">{}</xpath></data>",
        lines.join("\n")
    )
}

#[test]
fn python_expression_attributes_follow_test_inherit_python_expression() {
    let cases: Vec<(String, Option<&str>)> = vec![
        (
            attrs(&[r#"<attribute name="invisible">name == 'a'</attribute>"#]),
            Some("name == 'a'"),
        ),
        (
            r#"<data><xpath expr="//field[@name='name']" position="attributes"><attribute name="invisible">name == 'a'</attribute></xpath><xpath expr="//field[@name='name']" position="attributes"><attribute name="invisible">True</attribute></xpath></data>"#
                .to_owned(),
            Some("True"),
        ),
        (
            r#"<data><xpath expr="//field[@name='name']" position="attributes"><attribute name="invisible">name == 'a'</attribute></xpath><xpath expr="//field[@name='name']" position="attributes"><attribute name="invisible" add="name == 'b'" separator="or"/></xpath></data>"#
                .to_owned(),
            Some("(name == 'a') or (name == 'b')"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">name == 'e' and name == 'f'</attribute>"#,
                r#"<attribute name="invisible" add="id == 33" separator="and"/>"#,
            ]),
            Some("(name == 'e' and name == 'f') and (id == 33)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">name == 'e' and name == 'f'</attribute>"#,
                r#"<attribute name="invisible" add="id == 33" separator="and"/>"#,
                r#"<attribute name="invisible" add="id == 42" separator="or"/>"#,
                r#"<attribute name="invisible" add="id == 1" separator=" and "/>"#,
            ]),
            Some("(((name == 'e' and name == 'f') and (id == 33)) or (id == 42)) and (id == 1)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">id == 1</attribute>"#,
                r#"<attribute name="invisible" add="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" add="id == 3" separator="and"/>"#,
                r#"<attribute name="invisible" add="id == 4" separator="and"/>"#,
            ]),
            Some("(((id == 1) and (id == 2)) and (id == 3)) and (id == 4)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">(((id == 1) and (id == 2)) and (id == 3)) and (id == 4)</attribute>"#,
                r#"<attribute name="invisible" remove="id == 2" separator="and"/>"#,
            ]),
            Some("(((id == 1)) and (id == 3)) and (id == 4)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">(id == 1) and (id == 2) and (id == 3)</attribute>"#,
                r#"<attribute name="invisible" remove="id == 2" separator="and"/>"#,
            ]),
            Some("(id == 1) and (id == 3)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">(((id == 1) and (id == 2)) and (id == 3)) and (id == 4)</attribute>"#,
                r#"<attribute name="invisible" remove="id == 3" separator="and"/>"#,
            ]),
            Some("(((id == 1) and (id == 2))) and (id == 4)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">id == 1</attribute>"#,
                r#"<attribute name="invisible" add="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" remove="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" remove="id == 1" separator="and"/>"#,
            ]),
            None,
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">id == 1</attribute>"#,
                r#"<attribute name="invisible" add="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" add="id == 3" separator="and"/>"#,
                r#"<attribute name="invisible" add="id == 4" separator="and"/>"#,
                r#"<attribute name="invisible" remove="id == 3" separator="and"/>"#,
            ]),
            Some("(((id == 1) and (id == 2))) and (id == 4)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">(((id == 1) and (id == 2)) and (id == 3)) and (id == 4)</attribute>"#,
                r#"<attribute name="invisible" remove="id == 3" separator="and"/>"#,
                r#"<attribute name="invisible" remove="NO_MATCH" separator="and"/>"#,
            ]),
            Some("(((id == 1) and (id == 2))) and (id == 4)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">id == 1</attribute>"#,
                r#"<attribute name="invisible" add="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" remove="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" remove="id == 1" add="name" separator="and"/>"#,
            ]),
            Some("name"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">id == 1</attribute>"#,
                r#"<attribute name="invisible" add="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" remove="id == 2" add="name == 'foo'" separator="and"/>"#,
                r#"<attribute name="invisible" add="name" separator="and"/>"#,
            ]),
            Some("(((id == 1)) and (name == 'foo')) and (name)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">1 or not name</attribute>"#,
                r#"<attribute name="invisible" remove="1" separator="or"/>"#,
            ]),
            Some("not name"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">1 or not name</attribute>"#,
                r#"<attribute name="invisible" add="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" remove="1" separator="or"/>"#,
                r#"<attribute name="invisible" remove="not name" separator="and"/>"#,
            ]),
            Some("(id == 2)"),
        ),
        (
            attrs(&[
                r#"<attribute name="invisible">1 or not name</attribute>"#,
                r#"<attribute name="invisible" add="id == 2" separator="and"/>"#,
                r#"<attribute name="invisible" remove="1" separator="or"/>"#,
            ]),
            Some("(not name) and (id == 2)"),
        ),
    ];
    for (arch, expected) in &cases {
        assert_eq!(
            invisible_after(arch).unwrap().as_deref(),
            *expected,
            "spec {arch}"
        );
    }

    // The three assertInvalid cases.
    let err = invisible_after(&attrs(&[
        r#"<attribute name="invisible" position="add">True</attribute>"#,
    ]))
    .unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            "inherit",
            ViewError::InvalidAttributeAttributes("'position'".to_owned())
        )
    );
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("Invalid attributes 'position' in element <attribute>".to_owned())
    );
    let err = invisible_after(&attrs(&[
        r#"<attribute name="invisible" add="True">text</attribute>"#,
    ]))
    .unwrap_err();
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("Element <attribute> with 'add' or 'remove' cannot contain text 'text'".to_owned())
    );
    let err = invisible_after(&attrs(&[
        r#"<attribute name="invisible">id == 1</attribute>"#,
        r#"<attribute name="invisible" add="id == 2" separator="else"/>"#,
    ]))
    .unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            "inherit",
            ViewError::InvalidPythonSeparator {
                separator: "else".to_owned(),
                expression: "invisible".to_owned()
            }
        )
    );
    assert_eq!(
        err.source().map(ToString::to_string),
        Some(
            "Invalid separator 'else' for python expression 'invisible'; valid values are 'and' and 'or'"
                .to_owned()
        )
    );
}

#[test]
fn attribute_operands_and_divergences() {
    // operands match on identifier boundaries.
    assert_eq!(
        invisible_after(&attrs(&[
            r#"<attribute name="invisible">xid == 1 and y</attribute>"#,
            r#"<attribute name="invisible" remove="id == 1" separator="and"/>"#,
        ]))
        .unwrap()
        .as_deref(),
        Some("xid == 1 and y")
    );
    // ... the remove operand is literal, not a regular expression
    assert_eq!(
        invisible_after(&attrs(&[
            r#"<attribute name="invisible">a+b</attribute>"#,
            r#"<attribute name="invisible" remove="a+b" separator="and"/>"#,
        ]))
        .unwrap(),
        None
    );
    // ... the separator defaults to `and`
    assert_eq!(
        invisible_after(&attrs(&[
            r#"<attribute name="invisible">a</attribute>"#,
            r#"<attribute name="invisible" add="b"/>"#,
        ]))
        .unwrap()
        .as_deref(),
        Some("(a) and (b)")
    );
    let err = invisible_after(&attrs(&["<div/>"])).unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            "inherit",
            ViewError::UnexpectedAttributeChild("div".to_owned())
        )
    );
    let err = invisible_after(&attrs(&[r#"<attribute>1</attribute>"#])).unwrap_err();
    assert_eq!(err, in_view(2, "inherit", ViewError::AttributeWithoutName));
    let err = invisible_after(&attrs(&[r#"<attribute name="x"><b/></attribute>"#])).unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            "inherit",
            ViewError::AttributeChildElement("b".to_owned())
        )
    );
    assert_eq!(
        invisible_after(&attrs(&[
            r#"<attribute name="invisible" add="" data-oe-id="1"> 1 </attribute>"#
        ]))
        .unwrap()
        .as_deref(),
        Some(" 1 ")
    );
    let err = invisible_after(&attrs(&[
        r#"<attribute name="invisible" add="x"> </attribute>"#,
    ]))
    .unwrap_err();
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("Element <attribute> with 'add' or 'remove' cannot contain text ' '".to_owned())
    );
    let err = invisible_after(&attrs(&[
        r#"<attribute name="invisible" a="1" b="2">1</attribute>"#,
    ]))
    .unwrap_err();
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("Invalid attributes 'a', 'b' in element <attribute>".to_owned())
    );
}

#[test]
fn list_attribute_separators() {
    let class_after = |spec: &str| {
        let catalogue = ViewCatalogue::new(vec![
            view(1, "m", r#"<form><div class="a b  c"/></form>"#),
            view(
                2,
                "m",
                &format!(r#"<div position="attributes">{spec}</div>"#),
            )
            .extension_of(1),
        ])
        .unwrap();
        catalogue.combine(1).map(|c| c.to_xml())
    };
    assert_eq!(
        class_after(r#"<attribute name="class" add="d e" remove="b" separator=" "/>"#),
        Ok(r#"<form><div class="a c d e"/></form>"#.to_owned())
    );
    assert_eq!(
        class_after(r#"<attribute name="class" add="x"/>"#),
        Ok(r#"<form><div class="a b  c,x"/></form>"#.to_owned())
    );
    assert_eq!(
        class_after(r#"<attribute name="class" remove="a b  c"/>"#),
        Ok("<form><div/></form>".to_owned())
    );
    assert_eq!(
        class_after(r#"<attribute name="style" add="x" separator=";"/>"#),
        Ok(r#"<form><div class="a b  c" style="x"/></form>"#.to_owned())
    );
}

#[test]
fn text_around_inserted_nodes_is_preserved() {
    let combine = |spec: &str| {
        ViewCatalogue::new(vec![
            ViewDescriptor::new(
                1,
                "alpha",
                "ir.ui.view",
                r#"<form string="F">(<div/>)</form>"#,
            ),
            ViewDescriptor::new(2, "beta", "ir.ui.view", spec).extension_of(1),
        ])
        .unwrap()
        .combine(1)
        .unwrap()
        .to_xml()
    };
    assert_eq!(
        combine(r#"<div position="inside">a<p/>b<p/>c</div>"#),
        r#"<form string="F">(<div>a<p/>b<p/>c</div>)</form>"#
    );
    assert_eq!(
        combine(r#"<div position="after">a<p/>b<p/>c</div>"#),
        r#"<form string="F">(<div/>a<p/>b<p/>c)</form>"#
    );
    assert_eq!(
        combine(r#"<div position="before">a<p/>b<p/>c</div>"#),
        r#"<form string="F">(a<p/>b<p/>c<div/>)</form>"#
    );
}

#[test]
fn translated_selectors_are_reported_with_the_view() {
    let catalogue = ViewCatalogue::new(vec![
        view(1, "m", r#"<form><field name="foo" string="Name"/></form>"#),
        view(
            2,
            "m",
            r#"<xpath expr="//field[@string='Name']" position="after"><field name="bar"/></xpath>"#,
        )
        .extension_of(1),
    ])
    .unwrap();
    assert_eq!(
        catalogue.combine(1),
        Err(in_view(
            2,
            "view2",
            ViewError::TranslatedAttributeSelector("string".to_owned())
        ))
    );
}

#[test]
fn broken_views_outside_the_requested_tree_do_not_matter() {
    let catalogue = ViewCatalogue::new(vec![
        view(1, "m", r#"<form><field name="a"/></form>"#),
        view(
            2,
            "m",
            r#"<field name="a" position="after"><field name="b"/></field>"#,
        )
        .extension_of(1),
        // missing parent
        view(50, "m", "<form/>").extension_of(999),
        // unparsable extension of another root
        view(60, "m", "<form/>"),
        view(61, "m", "<<<").extension_of(60),
        // cycles: two views, a self-reference, a deep cycle with a tail
        view(70, "m", "<a/>").extension_of(71),
        view(71, "m", "<a/>").extension_of(70),
        view(80, "m", "<a/>").extension_of(80),
        view(90, "m", "<a/>").extension_of(91),
        view(91, "m", "<a/>").extension_of(92),
        view(92, "m", "<a/>").extension_of(93),
        view(93, "m", "<a/>").extension_of(91),
    ])
    .unwrap();
    assert_eq!(
        catalogue.combine(1).map(|c| c.to_xml()),
        Ok(r#"<form><field name="a"/><field name="b"/></form>"#.to_owned())
    );
    assert_eq!(
        catalogue.combine(50),
        Err(ViewError::ParentNotFound {
            view: 50,
            parent: 999
        })
    );
    assert_eq!(
        catalogue.combine(50).unwrap_err().to_string(),
        "Parent view 999 of view 50 not found"
    );
    let err = catalogue.combine(60).unwrap_err();
    assert_eq!(
        err,
        in_view(
            61,
            "view61",
            ViewError::XmlSyntax {
                message: "invalid name token at 1:2".to_owned(),
                line: 1,
                column: 2
            }
        )
    );
    assert_eq!(
        catalogue.combine(70),
        Err(ViewError::InheritanceCycle(vec![70, 71, 70]))
    );
    assert_eq!(
        catalogue.combine(80),
        Err(ViewError::InheritanceCycle(vec![80, 80]))
    );
    let err = catalogue.combine(90).unwrap_err();
    assert_eq!(err, ViewError::InheritanceCycle(vec![90, 91, 92, 93, 91]));
    assert_eq!(
        err.to_string(),
        "Inheritance cycle through views 90 -> 91 -> 92 -> 93 -> 91"
    );
    assert_eq!(catalogue.combine(5), Err(ViewError::ViewNotFound(5)));
    assert_eq!(
        catalogue.combine_many(&[1, 5]),
        Err(ViewError::ViewNotFound(5))
    );
}

#[test]
fn catalogue_errors() {
    let err = ViewCatalogue::new(vec![view(1, "m", "<a/>"), view(1, "m", "<b/>")]).unwrap_err();
    assert_eq!(err, ViewError::DuplicateViewId(1));
    assert_eq!(err.to_string(), "Duplicate view id 1 in the catalogue");
    let catalogue = ViewCatalogue::new(vec![
        view(1, "m", "<form/>"),
        ViewDescriptor::new(2, "blank", "m", "  \n ").extension_of(1),
    ])
    .unwrap();
    assert_eq!(
        catalogue.combine(1),
        Err(in_view(2, "blank", ViewError::EmptyArch))
    );
    let invalid_xpath = ViewCatalogue::new(vec![
        view(1, "m", "<form/>"),
        view(2, "m", r#"<xpath expr="//a/.." position="inside"/>"#).extension_of(1),
    ])
    .unwrap();
    assert_eq!(
        invalid_xpath.combine(1),
        Err(in_view(
            2,
            "view2",
            ViewError::InvalidXpath {
                expr: "//a/..".to_owned(),
                kind: XpathErrorKind::Unsupported("the parent step '..'")
            }
        ))
    );
}

#[test]
fn combined_node_cap_stops_many_large_extensions() {
    // 30 extensions of 9,000 elements each would build 270,000 nodes.
    let block: String = std::iter::repeat_n("<i/>", 9_000).collect();
    let mut views = vec![view(1, "m", "<form/>")];
    for id in 2..=31 {
        views.push(
            ViewDescriptor::new(
                id,
                format!("ext{id}"),
                "m",
                format!(r#"<form position="inside">{block}</form>"#),
            )
            .extension_of(1),
        );
    }
    let catalogue = ViewCatalogue::new(views).unwrap();
    let err = catalogue.combine(1).unwrap_err();
    // 1 + 22 × (9,000 + 1 sentinel) nodes fit; the 23rd extension (id 24) crosses 200,000.
    assert_eq!(
        err,
        in_view(
            24,
            "ext24",
            ViewError::LimitExceeded {
                limit: LimitKind::CombinedNodes,
                max: 200_000,
                actual: Some(200_001)
            }
        )
    );
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("nodes in a combined arch limit exceeded: maximum 200000, reached 200001".to_owned())
    );
}

#[test]
fn combined_byte_cap_and_lowered_limits() {
    let limits = ViewLimits::default()
        .lowered(LimitKind::CombinedBytes, 100)
        .unwrap();
    let text = "x".repeat(200);
    let catalogue = ViewCatalogue::with_limits(
        vec![
            view(1, "m", r#"<form><field name="a"/></form>"#),
            view(
                2,
                "m",
                &format!(r#"<form position="inside"><p>{text}</p></form>"#),
            )
            .extension_of(1),
        ],
        limits,
    )
    .unwrap();
    // "form" + "field" + "name" + "a" = 14 bytes, then "p" + 200 bytes of text.
    assert_eq!(
        catalogue.combine(1),
        Err(in_view(
            2,
            "view2",
            ViewError::LimitExceeded {
                limit: LimitKind::CombinedBytes,
                max: 100,
                actual: Some(215)
            }
        ))
    );
    let depth = ViewLimits::default().lowered(LimitKind::Depth, 2).unwrap();
    let shallow = ViewCatalogue::with_limits(
        vec![
            view(1, "m", "<form><a/></form>"),
            view(2, "m", r#"<a position="inside"><b/></a>"#).extension_of(1),
        ],
        depth,
    )
    .unwrap();
    assert_eq!(
        shallow.combine(1),
        Err(in_view(
            2,
            "view2",
            ViewError::LimitExceeded {
                limit: LimitKind::Depth,
                max: 2,
                actual: Some(3)
            }
        ))
    );
}

#[test]
fn reading_a_combined_arch() {
    let catalogue = ViewCatalogue::new(vec![view(
        1,
        "m",
        r#"<form string="F">x<group name="g"><field name="a"/>tail</group></form>"#,
    )])
    .unwrap();
    let combined = catalogue.combine(1).unwrap();
    let root = combined.arch().root();
    assert_eq!(root.tag(), "form");
    assert_eq!(root.attributes().collect::<Vec<_>>(), vec![("string", "F")]);
    assert_eq!(root.text(), "x");
    assert!(root.parent().is_none());
    let group = root.children().next().unwrap();
    assert_eq!(group.attr("name"), Some("g"));
    assert_eq!(group.parent().map(|p| p.tag()), Some("form"));
    let field = group.children().next().unwrap();
    assert_eq!(field.tail(), "tail");
    assert_eq!(
        group.to_xml(),
        r#"<group name="g"><field name="a"/>tail</group>"#
    );
    let descriptor = catalogue.get(1).unwrap();
    assert_eq!(
        (descriptor.id(), descriptor.name(), descriptor.model()),
        (1, "view1", "m")
    );
    assert_eq!(
        combined.into_arch().to_xml(),
        r#"<form string="F">x<group name="g"><field name="a"/>tail</group></form>"#
    );
}

/// The UI adapter opts into root merging; ordinary server-view primary inheritance is unchanged.
#[test]
fn primary_template_merges_root_between_primary_generations() {
    let catalogue = ViewCatalogue::new(vec![
        view(1, "ui.template", r#"<div t-name="a.P" class="parent" data-kept="yes"><span/></div>"#),
        view(2, "ui.template", r#"<section t-name="b.C" t-inherit="a.P" class="child"><xpath expr="//span" position="inside"><b/></xpath></section>"#).primary_template_of(1),
        view(3, "ui.template", r#"<article t-name="c.G" t-inherit="b.C"><xpath expr="//section" position="attributes"><attribute name="data-grandchild">yes</attribute></xpath></article>"#).primary_template_of(2),
    ]).unwrap();
    let child = catalogue.combine(2).unwrap();
    assert_eq!(
        child.to_xml(),
        r#"<section t-name="b.C" class="child" data-kept="yes"><span><b/></span></section>"#
    );
    let grandchild = catalogue.combine(3).unwrap();
    assert_eq!(
        grandchild.to_xml(),
        r#"<article t-name="c.G" class="child" data-kept="yes" data-grandchild="yes"><span><b/></span></article>"#
    );
    assert_eq!(grandchild.applied_view_ids(), &[1, 2, 3]);
    assert_eq!(grandchild.arch().root().origin().view_id(), 3);

    let ordinary = ViewCatalogue::new(vec![
        view(1, "a", "<form><a/></form>"),
        view(
            2,
            "a",
            r#"<data><xpath expr="//a" position="after"><b/></xpath></data>"#,
        )
        .primary_of(1),
    ])
    .unwrap();
    assert_eq!(
        ordinary.combine(2).unwrap().to_xml(),
        "<form><a/><b/></form>"
    );
}

#[test]
fn primary_template_root_merge_retains_attribute_and_byte_limits() {
    let limits = ViewLimits::default()
        .lowered(LimitKind::ElementAttributes, 1)
        .unwrap();
    let catalogue = ViewCatalogue::with_limits(
        vec![
            view(1, "ui.template", r#"<div a="1"/>"#),
            view(2, "ui.template", r#"<section b="2"/>"#).primary_template_of(1),
        ],
        limits,
    )
    .unwrap();
    let err = catalogue.combine(2).unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            "view2",
            ViewError::LimitExceeded {
                limit: LimitKind::ElementAttributes,
                max: 1,
                actual: Some(2),
            }
        )
    );
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("attributes per element limit exceeded: maximum 1, reached 2".to_owned())
    );

    let limits = ViewLimits::default()
        .lowered(LimitKind::CombinedBytes, 4)
        .unwrap();
    let catalogue = ViewCatalogue::with_limits(
        vec![
            view(1, "ui.template", "<a/>"),
            view(2, "ui.template", "<section/>").primary_template_of(1),
        ],
        limits,
    )
    .unwrap();
    let err = catalogue.combine(2).unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            "view2",
            ViewError::LimitExceeded {
                limit: LimitKind::CombinedBytes,
                max: 4,
                actual: Some(7),
            }
        )
    );
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("bytes in a combined arch limit exceeded: maximum 4, reached 7".to_owned())
    );
}

#[test]
fn template_dollar_zero_keeps_its_text_position_and_server_views_keep_append() {
    let root = r#"<div><span>original</span>between<u>tail</u></div>"#;
    let extension =
        r#"<xpath expr="//span" position="replace"><section>$0<i>added</i></section></xpath>"#;
    let ui = ViewCatalogue::new(vec![
        view(1, "ui.template", root),
        view(2, "ui.template", extension)
            .extension_of(1)
            .with_template_replacements(),
    ])
    .unwrap();
    assert_eq!(
        ui.combine(1).unwrap().to_xml(),
        "<div><section><span>original</span><i>added</i></section>between<u>tail</u></div>"
    );
    let ordinary = ViewCatalogue::new(vec![
        view(1, "m", root),
        view(2, "m", extension).extension_of(1),
    ])
    .unwrap();
    assert_eq!(
        ordinary.combine(1).unwrap().to_xml(),
        "<div><section><i>added</i><span>original</span>between</section><u>tail</u></div>"
    );
}

#[test]
fn ui_spec_errors_retain_operation_context_and_underlying_kind() {
    let extension = "<data>\n<xpath expr=\"//span\" position=\"attributes\"><attribute name=\"class\">kept</attribute></xpath>\n<data>\n<xpath expr=\"//gone\" position=\"inside\"/>\n</data>\n</data>";
    let descriptors = || {
        vec![
            view(1, "ui.template", "<div><span/></div>"),
            view(2, "ui.template", extension)
                .extension_of(1)
                .with_template_replacements(),
        ]
    };
    let error = ViewCatalogue::new(descriptors())
        .unwrap()
        .combine(1)
        .unwrap_err();
    assert_eq!(
        error,
        in_view(
            2,
            "view2",
            ViewError::InSpec {
                index: 2,
                line: 4,
                spec: "<xpath expr=\"//gone\">".to_owned(),
                source: Box::new(ViewError::TargetNotFound(
                    "<xpath expr=\"//gone\">".to_owned()
                )),
            }
        )
    );
    assert_eq!(
        error.source().unwrap().to_string(),
        "inheritance spec 2 at entry line 4 <xpath expr=\"//gone\">: Element '<xpath expr=\"//gone\">' cannot be located in parent view"
    );
    let ordinary = ViewCatalogue::new(vec![
        view(1, "m", "<div><span/></div>"),
        view(2, "m", extension).extension_of(1),
    ])
    .unwrap();
    assert_eq!(
        ordinary.combine(1),
        Err(in_view(
            2,
            "view2",
            ViewError::TargetNotFound("<xpath expr=\"//gone\">".to_owned())
        ))
    );
}

#[test]
fn copied_nodes_retain_distinct_original_source_ordinals() {
    let catalogue = ViewCatalogue::new(vec![
        view(1, "ui.template", "<div><span/><span/></div>"),
        view(2, "ui.template", "<data><xpath expr=\"//span[1]\" position=\"replace\"><section>$0</section></xpath></data>").extension_of(1).with_template_replacements(),
    ]).unwrap();
    let combined = catalogue.combine(1).unwrap();
    let root = combined.arch().root();
    let children: Vec<_> = root.children().collect();
    let copied = children[0].children().next().unwrap();
    assert_eq!(copied.origin().view_id(), 1);
    assert_eq!(copied.source_node_index(), Some(1));
    assert_eq!(children[1].source_node_index(), Some(2));
    assert_eq!(children[0].origin().view_id(), 2);
    assert_eq!(children[0].source_node_index(), Some(2));
}
