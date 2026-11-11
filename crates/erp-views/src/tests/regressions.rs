//! Regression tests for the review findings (
//! round 2), with the review's inputs.

use super::{apply, apply_with, on_small_stack};
use crate::error::{LimitKind, ViewError};
use crate::limits::ViewLimits;

/// Round 2, blocking 1: `<b>$0</b>` target and `<a>$0</a>` spec recursed into the pushed copy.
#[test]
fn dollar_zero_does_not_recurse_into_the_copy() {
    let result = on_small_stack(|| {
        apply(
            "<root><b>$0</b></root>",
            r#"<xpath expr="//b" position="replace"><a>$0</a></xpath>"#,
        )
    });
    assert_eq!(result, Ok("<root><a><b>$0</b></a></root>".to_owned()));
}

#[test]
fn dollar_zero_locations_follow_reference() {
    // Every descendant with a `$0` text node gets its own copy; the spec element itself does not.
    assert_eq!(
        apply(
            "<r><p>x</p></r>",
            r#"<xpath expr="//p" position="replace"><div>$0</div><section><span>$0</span></section></xpath>"#
        ),
        Ok("<r><div><p>x</p></div><section><span><p>x</p></span></section></r>".to_owned())
    );
    assert_eq!(
        apply(
            "<r><p>x</p></r>",
            r#"<xpath expr="//p" position="replace"><div><i/>$0</div></xpath>"#
        ),
        Ok("<r><div><i/>$0<p>x</p></div></r>".to_owned())
    );
    // The copy keeps the target's tail (lxml `deepcopy`); the target leaves with its tail.
    assert_eq!(
        apply(
            "<r><b>x</b>tail</r>",
            r#"<xpath expr="//b" position="replace"><a>$0</a></xpath>"#
        ),
        Ok("<r><a><b>x</b>tail</a></r>".to_owned())
    );
    // Only an exact `$0` text node counts.
    assert_eq!(
        apply(
            "<r><p/></r>",
            r#"<xpath expr="//p" position="replace"><a> $0 </a></xpath>"#
        ),
        Ok("<r><a> $0 </a></r>".to_owned())
    );
    // `$0` is replace-only: other positions insert the text literally.
    assert_eq!(
        apply(
            "<r><p/></r>",
            r#"<xpath expr="//p" position="after"><a>$0</a></xpath>"#
        ),
        Ok("<r><p/><a>$0</a></r>".to_owned())
    );
}

#[test]
fn dollar_zero_copy_is_taken_after_the_moves() {
    // the moved paragraph is not duplicated inside the wrapper.
    assert_eq!(
        apply(
            r#"<r><div class="w"><p>x</p></div></r>"#,
            r#"<xpath expr="//div" position="replace"><xpath expr="//p" position="move"/><section>$0</section></xpath>"#
        ),
        Ok(r#"<r><p>x</p><section><div class="w"/></section></r>"#.to_owned())
    );
    assert_eq!(
        apply(
            "<r><p/><t/></r>",
            r#"<xpath expr="//t" position="replace"><xpath expr="//p" position="move">$0</xpath></xpath>"#
        ),
        Err(ViewError::InvalidMoveSpec(
            r#"<xpath expr="//p" position="move">$0</xpath>"#.to_owned()
        ))
    );
}

#[test]
fn dollar_zero_copies_are_bounded() {
    // 2,000 wrap locations around a 100-element target: 200,000 copied nodes.
    let target: String = std::iter::repeat_n("<i/>", 99).collect();
    let base = format!("<r><p>{target}</p></r>");
    let locations: String = std::iter::repeat_n("<w>$0</w>", 2_000).collect();
    let spec = format!(r#"<xpath expr="//p" position="replace">{locations}</xpath>"#);
    let limits = ViewLimits::default();
    let err = apply_with(&base, &spec, &limits).unwrap_err();
    assert_eq!(
        err,
        ViewError::LimitExceeded {
            limit: LimitKind::CombinedNodes,
            max: 200_000,
            actual: Some(200_001)
        }
    );
}

/// Round 2, blocking 3: a multibyte `remove` operand panicked (`idx + 1` inside a character).
#[test]
fn multibyte_remove_operand() {
    let spec = |remove: &str| {
        format!(
            r#"<field name="x" position="attributes"><attribute name="invisible" remove="{remove}" separator="and"/></field>"#
        )
    };
    assert_eq!(
        apply(
            r#"<form><field name="x" invisible="xé == 1 and é == 1"/></form>"#,
            &spec("é == 1")
        ),
        Ok(r#"<form><field name="x" invisible="xé == 1"/></form>"#.to_owned())
    );
    assert_eq!(
        apply(
            r#"<form><field name="x" invisible="中文 and 文"/></form>"#,
            &spec("文")
        ),
        Ok(r#"<form><field name="x" invisible="中文"/></form>"#.to_owned())
    );
    assert_eq!(
        apply(
            r#"<form><field name="x" invisible="(😀) and b"/></form>"#,
            &spec("😀")
        ),
        Ok(r#"<form><field name="x" invisible="b"/></form>"#.to_owned())
    );
}

/// Round 2, blocking 8: root replacement with nothing, and empty separators.
#[test]
fn empty_root_replacement_and_empty_separators() {
    assert_eq!(
        apply(
            "<form><a/></form>",
            r#"<xpath expr="/form" position="replace"/>"#
        ),
        Err(ViewError::RootReplacementCount { found: 0 })
    );
    let err = apply(
        r#"<form><field name="x" invisible="a"/></form>"#,
        r#"<field name="x" position="attributes"><attribute name="invisible" add="b" separator=""/></field>"#,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ViewError::InvalidPythonSeparator {
            separator: String::new(),
            expression: "invisible".to_owned()
        }
    );
    assert_eq!(
        err.to_string(),
        "Invalid separator '' for python expression 'invisible'; valid values are 'and' and 'or'"
    );
    let err = apply(
        r#"<form><field name="x" class="a b"/></form>"#,
        r#"<field name="x" position="attributes"><attribute name="class" add="c" separator=""/></field>"#,
    )
    .unwrap_err();
    assert_eq!(err, ViewError::EmptySeparator("class".to_owned()));
    assert_eq!(err.to_string(), "Empty separator for attribute 'class'");
}

/// Depth is checked after every spec: inserted content can make the tree deeper than parsing
/// allowed (round 1, blocking 8).
#[test]
fn depth_after_applying_a_spec_is_checked() {
    let base = format!(
        "<form>{}<field name=\"target\"/>{}</form>",
        "<div>".repeat(200),
        "</div>".repeat(200)
    );
    let spec = format!(
        "<xpath expr=\"//field[@name='target']\" position=\"inside\">{}<p/>{}</xpath>",
        "<div>".repeat(70),
        "</div>".repeat(70)
    );
    assert_eq!(
        apply(&base, &spec),
        Err(ViewError::LimitExceeded {
            limit: LimitKind::Depth,
            max: 256,
            actual: Some(273)
        })
    );
}

#[test]
fn stray_text_in_containers_is_an_error() {
    assert_eq!(
        apply("<r/>", r#"<data>oops<r position="inside"><a/></r></data>"#),
        Err(ViewError::StrayText {
            container: "data".to_owned(),
            text: "oops".to_owned()
        })
    );
    let err = apply(
        r#"<r a="1"/>"#,
        r#"<r position="attributes"><attribute name="a">2</attribute>oops</r>"#,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ViewError::StrayText {
            container: "r".to_owned(),
            text: "oops".to_owned()
        }
    );
    assert_eq!(err.to_string(), "Unexpected text 'oops' in <r>");
}
