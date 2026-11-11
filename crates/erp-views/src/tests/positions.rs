use super::apply;
use crate::error::ViewError;

const BASE: &str = r#"<form string="Title"><field name="target"/></form>"#;
const ADV: &str = r#"<form string="Title"><field name="target">TEXT1<field name="subtarget"/>TEXT2<field name="anothersubtarget"/>TEXT3</field></form>"#;

#[test]
fn positions_follow_test_apply_inheritance_specs() {
    // test_replace_outer
    assert_eq!(
        apply(
            BASE,
            r#"<field name="target" position="replace"><field name="replacement"/></field>"#
        ),
        Ok(r#"<form string="Title"><field name="replacement"/></form>"#.to_owned())
    );
    // test_delete
    assert_eq!(
        apply(BASE, r#"<field name="target" position="replace"/>"#),
        Ok(r#"<form string="Title"/>"#.to_owned())
    );
    // test_insert_after
    assert_eq!(
        apply(
            BASE,
            r#"<field name="target" position="after"><field name="inserted"/></field>"#
        ),
        Ok(
            r#"<form string="Title"><field name="target"/><field name="inserted"/></form>"#
                .to_owned()
        )
    );
    // test_insert_before
    assert_eq!(
        apply(
            BASE,
            r#"<field name="target" position="before"><field name="inserted"/></field>"#
        ),
        Ok(
            r#"<form string="Title"><field name="inserted"/><field name="target"/></form>"#
                .to_owned()
        )
    );
    // test_insert_inside: default position, then explicit inside
    let once = apply(
        BASE,
        r#"<field name="target"><field name="inserted"/></field>"#,
    )
    .unwrap();
    assert_eq!(
        apply(&once, r#"<field name="target" position="inside"><field name="inserted 2"/></field>"#),
        Ok(r#"<form string="Title"><field name="target"><field name="inserted"/><field name="inserted 2"/></field></form>"#.to_owned())
    );
    // test_replace_inner: same result on a target with and without text and children
    let inner = r#"<field name="target" position="replace" mode="inner">TEXT 4<field name="replacement"/>TEXT 5<field name="replacement2"/>TEXT 6</field>"#;
    let expected = r#"<form string="Title"><field name="target">TEXT 4<field name="replacement"/>TEXT 5<field name="replacement2"/>TEXT 6</field></form>"#;
    assert_eq!(apply(BASE, inner), Ok(expected.to_owned()));
    assert_eq!(apply(ADV, inner), Ok(expected.to_owned()));
    // test_replace_inner_2: moves take existing children into the new content
    assert_eq!(
        apply(
            ADV,
            r#"<field name="target" position="replace" mode="inner">TEXT 4<xpath position="move" expr="//field[2]"/>TEXT 5<xpath expr="//field[@name='subtarget']" position="move"/>TEXT 6</field>"#
        ),
        Ok(r#"<form string="Title"><field name="target">TEXT 4<field name="anothersubtarget"/>TEXT 5<field name="subtarget"/>TEXT 6</field></form>"#.to_owned())
    );
    // test_unpack_data
    assert_eq!(
        apply(
            BASE,
            r#"<data><field name="target"><field name="inserted 0"/></field><field name="target"><field name="inserted 1"/></field><field name="target"><field name="inserted 2"/></field><field name="target"><field name="inserted 3"/></field></data>"#
        ),
        Ok(r#"<form string="Title"><field name="target"><field name="inserted 0"/><field name="inserted 1"/><field name="inserted 2"/><field name="inserted 3"/></field></form>"#.to_owned())
    );
}

#[test]
fn nested_data_containers_unpack_breadth_first() {
    assert_eq!(
        apply(
            "<r><x/></r>",
            r#"<data><data><x position="inside"><a/></x></data><x position="inside"><b/></x></data>"#
        ),
        Ok("<r><x><b/><a/></x></r>".to_owned())
    );
}

#[test]
fn indentation_moves_with_inserted_content() {
    // add_stripped_items_before: the newline/indent before the anchor ends up after the inserted
    // content; the spec's own indentation is kept.
    assert_eq!(
        apply(
            "<form>\n    <group>\n        <field name=\"a\"/>\n    </group>\n</form>",
            "<xpath expr=\"//group\" position=\"inside\">\n    <field name=\"b\"/>\n</xpath>"
        ),
        Ok("<form>\n    <group>\n        <field name=\"a\"/>\n    <field name=\"b\"/>\n    </group>\n</form>".to_owned())
    );
    // replace outer: the spec text is dropped, the target goes with its tail (lxml `remove`)
    assert_eq!(
        apply(
            "<form>\n  <field name=\"a\"/>\n  <field name=\"b\"/>\n</form>",
            "<field name=\"a\" position=\"replace\">\n  <field name=\"c\"/>\n</field>"
        ),
        Ok("<form>\n  <field name=\"c\"/>\n<field name=\"b\"/>\n</form>".to_owned())
    );
}

#[test]
fn replace_outer_rejects_text_it_would_drop() {
    let err = apply(
        BASE,
        r#"<field name="target" position="replace">gone<p/></field>"#,
    )
    .unwrap_err();
    assert_eq!(err, ViewError::ReplaceTextDropped("gone".to_owned()));
    assert_eq!(
        err.to_string(),
        "Text 'gone' in a position='replace' specification would be dropped"
    );
    // `$0` as the spec's own text is not a wrap location (`.//*` excludes the spec element).
    assert_eq!(
        apply(
            BASE,
            r#"<field name="target" position="replace">$0</field>"#
        ),
        Err(ViewError::ReplaceTextDropped("$0".to_owned()))
    );
    assert_eq!(
        apply(
            r#"<r><p/><t/></r>"#,
            r#"<xpath expr="//t" position="replace"><xpath expr="//p" position="move"/>tail</xpath>"#
        ),
        Err(ViewError::ReplaceTextDropped("tail".to_owned()))
    );
}

#[test]
fn invalid_mode_and_position_names() {
    let err = apply(
        BASE,
        r#"<field name="target" position="replace" mode="sideways"/>"#,
    )
    .unwrap_err();
    assert_eq!(err, ViewError::InvalidMode("sideways".to_owned()));
    assert_eq!(err.to_string(), "Invalid mode attribute: “sideways”");
    // `move` is only valid on a direct child of a spec.
    let err = apply(BASE, r#"<field name="target" position="move"/>"#).unwrap_err();
    assert_eq!(err, ViewError::InvalidPosition("move".to_owned()));
    assert_eq!(err.to_string(), "Invalid position attribute: 'move'");
}

#[test]
fn root_element_positions() {
    // replace: one element, which keeps the old root's t-name
    assert_eq!(
        apply(
            r#"<form><a/></form>"#,
            r#"<xpath expr="/form" position="replace"><form string="New"><b/></form></xpath>"#
        ),
        Ok(r#"<form string="New"><b/></form>"#.to_owned())
    );
    assert_eq!(
        apply(
            r#"<t t-name="web.x"><div/></t>"#,
            r#"<xpath expr="." position="replace"><div class="new">$0</div></xpath>"#
        ),
        Ok(r#"<div class="new" t-name="web.x"><t t-name="web.x"><div/></t></div>"#.to_owned())
    );
    let err = apply(r#"<form/>"#, r#"<form position="replace"/>"#).unwrap_err();
    assert_eq!(err, ViewError::RootReplacementCount { found: 0 });
    assert_eq!(
        err.to_string(),
        "Replacing the root element requires exactly one element, found 0"
    );
    assert_eq!(
        apply(r#"<form/>"#, r#"<form position="replace"><a/><b/></form>"#),
        Err(ViewError::RootReplacementCount { found: 2 })
    );
    assert_eq!(
        apply(
            r#"<form><a/></form>"#,
            r#"<form position="replace"><xpath expr="//a" position="move"/></form>"#
        ),
        Err(ViewError::MoveInRootReplacement)
    );
    assert_eq!(
        apply(r#"<form/>"#, r#"<form position="after"><a/></form>"#),
        Err(ViewError::RootSiblingPosition("after".to_owned()))
    );
    assert_eq!(
        apply(r#"<form/>"#, r#"<form position="before"><a/></form>"#),
        Err(ViewError::RootSiblingPosition("before".to_owned()))
    );
    // inside the root is fine
    assert_eq!(
        apply(
            r#"<form/>"#,
            r#"<xpath expr="." position="inside"><a/></xpath>"#
        ),
        Ok("<form><a/></form>".to_owned())
    );
}

#[test]
fn wrap_with_dollar_zero() {
    assert_eq!(
        apply(
            "<template><div><p>Content</p></div></template>",
            r#"<xpath expr="//p" position="replace"><div class="some">$0</div></xpath>"#
        ),
        Ok(r#"<template><div><div class="some"><p>Content</p></div></div></template>"#.to_owned())
    );
}

const MOVE_BASE: &str =
    r#"<template><div><p class="some">Content</p></div><div class="target"/></template>"#;
const MOVE_WRAPPED: &str =
    r#"<template><div>aaaa<p class="some">Content</p>bbbb</div><div class="target"/></template>"#;

fn move_spec(position: &str) -> String {
    format!(
        r#"<xpath expr="//div[@class='target']" position="{position}"><xpath expr="//p" position="move"/></xpath>"#
    )
}

#[test]
fn move_follows_test_apply_inheritance_move_specs() {
    // test_move_replace
    assert_eq!(
        apply(MOVE_BASE, &move_spec("replace")),
        Ok(r#"<template><div/><p class="some">Content</p></template>"#.to_owned())
    );
    assert_eq!(
        apply(MOVE_WRAPPED, &move_spec("replace")),
        Ok(r#"<template><div>aaaabbbb</div><p class="some">Content</p></template>"#.to_owned())
    );
    // test_move_inside
    assert_eq!(
        apply(MOVE_BASE, &move_spec("inside")),
        Ok(
            r#"<template><div/><div class="target"><p class="some">Content</p></div></template>"#
                .to_owned()
        )
    );
    assert_eq!(
        apply(MOVE_WRAPPED, &move_spec("inside")),
        Ok(r#"<template><div>aaaabbbb</div><div class="target"><p class="some">Content</p></div></template>"#.to_owned())
    );
    // test_move_before
    assert_eq!(
        apply(MOVE_BASE, &move_spec("before")),
        Ok(
            r#"<template><div/><p class="some">Content</p><div class="target"/></template>"#
                .to_owned()
        )
    );
    assert_eq!(
        apply(MOVE_WRAPPED, &move_spec("before")),
        Ok(r#"<template><div>aaaabbbb</div><p class="some">Content</p><div class="target"/></template>"#.to_owned())
    );
    // test_move_after
    assert_eq!(
        apply(MOVE_BASE, &move_spec("after")),
        Ok(
            r#"<template><div/><div class="target"/><p class="some">Content</p></template>"#
                .to_owned()
        )
    );
    assert_eq!(
        apply(MOVE_WRAPPED, &move_spec("after")),
        Ok(r#"<template><div>aaaabbbb</div><div class="target"/><p class="some">Content</p></template>"#.to_owned())
    );
    // test_move_with_other_1
    assert_eq!(
        apply(
            MOVE_BASE,
            r#"<xpath expr="//div[@class='target']" position="after"><xpath expr="//p" position="move"/><p class="new_p">Content2</p></xpath>"#
        ),
        Ok(r#"<template><div/><div class="target"/><p class="some">Content</p><p class="new_p">Content2</p></template>"#.to_owned())
    );
    // test_move_with_other_2
    assert_eq!(
        apply(
            MOVE_WRAPPED,
            r#"<xpath expr="//div[@class='target']" position="after"><p class="new_p">Content2</p><xpath expr="//p" position="move"/></xpath>"#
        ),
        Ok(r#"<template><div>aaaabbbb</div><div class="target"/><p class="new_p">Content2</p><p class="some">Content</p></template>"#.to_owned())
    );
    // test_move_with_tail
    assert_eq!(
        apply(
            MOVE_WRAPPED,
            r#"<xpath expr="//div[@class='target']" position="after"><p class="new_p">Content2</p><xpath expr="//p" position="move"/>tail of paragraph</xpath>"#
        ),
        Ok(r#"<template><div>aaaabbbb</div><div class="target"/><p class="new_p">Content2</p><p class="some">Content</p>tail of paragraph</template>"#.to_owned())
    );
    // test_incorrect_move_1: the moved element does not exist
    let err = apply(
        MOVE_BASE,
        r#"<xpath expr="//div[@class='target']" position="after"><xpath expr="//p[@name='none']" position="move"/></xpath>"#,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ViewError::MoveTargetNotFound(
            r#"<xpath expr="//p[@name='none']" position="move"/>"#.to_owned()
        )
    );
    assert_eq!(
        err.to_string(),
        r#"Element “<xpath expr="//p[@name='none']" position="move"/>” cannot be located in parent view"#
    );
    // test_incorrect_move_2: a move spec cannot have children
    let err = apply(
        MOVE_BASE,
        r#"<xpath expr="//div[@class='target']" position="after"><xpath expr="//p" position="move"><p class="new_p">Content2</p></xpath></xpath>"#,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ViewError::InvalidMoveSpec(
            r#"<xpath expr="//p" position="move"><p class="new_p">Content2</p></xpath>"#.to_owned()
        )
    );
    assert_eq!(
        err.to_string(),
        r#"Invalid specification for moved nodes: “<xpath expr="//p" position="move"><p class="new_p">Content2</p></xpath>”"#
    );
    // test_incorrect_move_3: a move below a direct spec child is inserted literally
    assert_eq!(
        apply(
            MOVE_BASE,
            r#"<xpath expr="//div[@class='target']" position="after"><div class="wrapper"><xpath expr="//p" position="move"><p class="new_p">Content2</p></xpath></div></xpath>"#
        ),
        Ok(r#"<template><div><p class="some">Content</p></div><div class="target"/><div class="wrapper"><xpath expr="//p" position="move"><p class="new_p">Content2</p></xpath></div></template>"#.to_owned())
    );
}

#[test]
fn moving_the_target_an_ancestor_or_the_root_is_an_error() {
    let base =
        r#"<template><div class="outer"><div class="target"><p>Inner</p></div></div></template>"#;
    for position in ["inside", "after", "before", "replace"] {
        let into_itself = format!(
            r#"<xpath expr="//div[@class='target']" position="{position}"><xpath expr="//div[@class='target']" position="move"/></xpath>"#
        );
        assert_eq!(
            apply(base, &into_itself),
            Err(ViewError::MoveTargetOrAncestor(
                r#"<xpath expr="//div[@class='target']" position="move"/>"#.to_owned()
            )),
            "{position}"
        );
        let ancestor = format!(
            r#"<xpath expr="//div[@class='target']" position="{position}"><xpath expr="//div[@class='outer']" position="move"/></xpath>"#
        );
        assert_eq!(
            apply(base, &ancestor),
            Err(ViewError::MoveTargetOrAncestor(
                r#"<xpath expr="//div[@class='outer']" position="move"/>"#.to_owned()
            )),
            "{position}"
        );
    }
    assert_eq!(
        apply(
            base,
            r#"<xpath expr="//p" position="inside"><xpath expr="/template" position="move"/></xpath>"#
        ),
        Err(ViewError::MoveRootElement(
            r#"<xpath expr="/template" position="move"/>"#.to_owned()
        ))
    );
    // a move spec with text is not empty
    assert_eq!(
        apply(
            base,
            r#"<xpath expr="//p" position="after"><xpath expr="//p" position="move">x</xpath></xpath>"#
        ),
        Err(ViewError::InvalidMoveSpec(
            r#"<xpath expr="//p" position="move">x</xpath>"#.to_owned()
        ))
    );
}

#[test]
fn replace_with_a_moved_descendant_unwraps() {
    assert_eq!(
        apply(
            r#"<r><div class="w">a<p>x</p>b</div>tail</r>"#,
            r#"<xpath expr="//div" position="replace"><xpath expr="//p" position="move"/></xpath>"#
        ),
        Ok("<r><p>x</p></r>".to_owned())
    );
}

#[test]
fn a_move_can_take_a_node_inserted_by_the_same_spec() {
    assert_eq!(
        apply(
            r#"<r><a/><b/></r>"#,
            r#"<xpath expr="//b" position="after"><c/><xpath expr="//c" position="move"/></xpath>"#
        ),
        Ok("<r><a/><b/><c/></r>".to_owned())
    );
}
