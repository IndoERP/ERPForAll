//! Work bounds of one combination through the public API: the evaluation-step budget
//! (`LimitKind::EvaluationSteps`), many `$0` locations, and failed views leaving
//! nothing behind.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures and helpers"
)]

#[path = "support/partner_form.rs"]
mod partner_form;

use erp_views::{LimitKind, ViewCatalogue, ViewDescriptor, ViewError, ViewLimits};
use std::error::Error as _;

fn view(id: i64, arch: &str) -> ViewDescriptor {
    ViewDescriptor::new(id, format!("view{id}"), "m", arch)
}

fn in_view(id: i64, error: ViewError) -> ViewError {
    ViewError::InView {
        view_id: id,
        view_name: format!("view{id}"),
        source: Box::new(error),
    }
}

fn steps_exceeded(max: usize) -> ViewError {
    ViewError::LimitExceeded {
        limit: LimitKind::EvaluationSteps,
        max,
        actual: Some(max + 1),
    }
}

/// A base arch of 10,000 elements (the per-arch maximum) whose last element is the only
/// `name="last"`.
fn wide_base() -> String {
    format!(r#"<form>{}<f name="last"/></form>"#, "<f/>".repeat(9_998))
}

/// 10,000 elements again, the leaves 65 levels deep: `<form>`, 63 nested `<d>`, then 9,935
/// `<f/>` and the `name="last"` one, so a path of 63 `//*` steps still reaches the leaves.
fn deep_base() -> String {
    format!(
        r#"<form>{}{}<f name="last"/>{}</form>"#,
        "<d>".repeat(63),
        "<f/>".repeat(9_935),
        "</d>".repeat(63)
    )
}

/// `count` copies of `spec` in one `<data>` view.
fn many_specs(spec: &str, count: usize) -> String {
    format!("<data>{}</data>", spec.repeat(count))
}

/// 4,999 specs (the spec arch's element maximum) against 10,000 nodes, each spec an XPath of 64
/// steps (the step maximum) that walks the whole tree per step and matches the last leaf: about
/// 3.3 × 10⁹ evaluation steps without a budget. The default budget stops the combination at its
/// 20,000,001st step, around the 31st spec.
#[test]
fn hostile_xpath_combination_fails_fast_with_the_typed_error() {
    let expr = format!("{}//f[@name='last']", "//*".repeat(63));
    let spec = format!(
        r#"<xpath expr="{expr}" position="attributes"><attribute name="x">1</attribute></xpath>"#
    );
    // Three such specs fit: each matches the last leaf and costs more than 64 walks of the tree.
    let few = ViewCatalogue::new(vec![
        view(1, &deep_base()),
        view(2, &many_specs(&spec, 3)).extension_of(1),
    ])
    .unwrap()
    .combine(1)
    .unwrap();
    let last_leaf = format!(r#"<f name="last" x="1"/>{}</form>"#, "</d>".repeat(63));
    assert!(few.to_xml().ends_with(&last_leaf));
    assert!(few.evaluation_steps() > 3 * 64 * 10_000);

    let catalogue = ViewCatalogue::new(vec![
        view(1, &deep_base()),
        view(2, &many_specs(&spec, 4_999)).extension_of(1),
    ])
    .unwrap();
    let err = catalogue.combine(1).unwrap_err();
    assert_eq!(err, in_view(2, steps_exceeded(20_000_000)));
    assert_eq!(
        err.source().map(ToString::to_string),
        Some(
            "evaluation steps per combination limit exceeded: maximum 20000000, reached 20000001"
                .to_owned()
        )
    );
}

/// Element locators (`<field name>` and tag-plus-attributes) walk the arch too and are charged
/// the same way.
#[test]
fn hostile_element_locator_combination_fails_with_the_typed_error() {
    let field_base = wide_base().replace(r#"<f name="last"/>"#, r#"<field name="last"/>"#);
    for (base, spec) in [
        (
            wide_base(),
            r#"<f name="last" position="attributes"><attribute name="x">1</attribute></f>"#,
        ),
        (
            field_base,
            r#"<field name="last" position="attributes"><attribute name="x">1</attribute></field>"#,
        ),
    ] {
        let catalogue = ViewCatalogue::new(vec![
            view(1, &base),
            view(2, &many_specs(spec, 4_999)).extension_of(1),
        ])
        .unwrap();
        assert_eq!(
            catalogue.combine(1),
            Err(in_view(2, steps_exceeded(20_000_000))),
            "spec {spec}"
        );
    }
}

/// The fixture (the bench's `res.partner` form with 40 extensions) uses a tiny
/// fraction of the budget: real views stay far from the limit.
#[test]
fn realistic_fixture_stays_well_under_the_budget() {
    let catalogue = ViewCatalogue::new(partner_form::fixture()).unwrap();
    let combined = catalogue.combine(1).unwrap();
    let max = ViewLimits::default().get(LimitKind::EvaluationSteps);
    let steps = combined.evaluation_steps();
    eprintln!("res.partner fixture: {steps} evaluation steps");
    assert!(steps > 0, "the combination is charged");
    assert!(
        steps.saturating_mul(1_000) < max,
        "fixture used {steps} of {max} evaluation steps (expected under 0.1%)"
    );
}

/// The budget belongs to one combination: repeated and batched combinations each start from
/// zero, and a host can lower it to exactly what a combination needs.
#[test]
fn budget_is_per_combination_and_can_be_lowered() {
    let views = vec![
        view(1, "<form><a/><b/></form>"),
        view(2, r#"<b position="inside"><c/></b>"#).extension_of(1),
    ];
    let catalogue = ViewCatalogue::new(views.clone()).unwrap();
    let steps = catalogue.combine(1).unwrap().evaluation_steps();
    assert_eq!(catalogue.combine(1).unwrap().evaluation_steps(), steps);
    let batch = catalogue.combine_many(&[1, 1, 1]).unwrap();
    assert_eq!(
        batch
            .iter()
            .map(|c| c.evaluation_steps())
            .collect::<Vec<_>>(),
        vec![steps; 3]
    );

    let exact = ViewLimits::default()
        .lowered(LimitKind::EvaluationSteps, steps)
        .unwrap();
    let fits = ViewCatalogue::with_limits(views.clone(), exact).unwrap();
    assert_eq!(fits.combine(1).unwrap().evaluation_steps(), steps);

    let short = ViewLimits::default()
        .lowered(LimitKind::EvaluationSteps, steps - 1)
        .unwrap();
    let too_short = ViewCatalogue::with_limits(views, short).unwrap();
    assert_eq!(
        too_short.combine(1),
        Err(in_view(2, steps_exceeded(steps - 1)))
    );
    assert_eq!(
        ViewLimits::default().lowered(LimitKind::EvaluationSteps, 20_000_001),
        Err(ViewError::InvalidLimit {
            limit: LimitKind::EvaluationSteps,
            value: 20_000_001,
            max: 20_000_000
        })
    );
}

/// A `$0` wrap with thousands of locations: each location gets its own copy of the target, in
/// order (the copy lookup is indexed, not a scan of the locations per copied node).
#[test]
fn many_dollar_zero_locations_each_get_the_target() {
    let n = 5_000;
    let locations: String = (0..n).map(|i| format!(r#"<w n="{i}">$0</w>"#)).collect();
    let catalogue = ViewCatalogue::new(vec![
        view(1, r#"<form><field name="a"/></form>"#),
        view(
            2,
            &format!(r#"<field name="a" position="replace"><g>{locations}</g></field>"#),
        )
        .extension_of(1),
    ])
    .unwrap();
    let combined = catalogue.combine(1).unwrap();
    let expected: String = (0..n)
        .map(|i| format!(r#"<w n="{i}"><field name="a"/></w>"#))
        .collect();
    assert_eq!(combined.to_xml(), format!("<form><g>{expected}</g></form>"));
}

/// The same with the root as the target: every location gets a copy of the old root.
#[test]
fn many_dollar_zero_locations_in_a_root_replacement() {
    let n = 2_000;
    let locations: String = (0..n).map(|i| format!(r#"<w n="{i}">$0</w>"#)).collect();
    let catalogue = ViewCatalogue::new(vec![
        view(1, r#"<form><a/></form>"#),
        view(
            2,
            &format!(r#"<form position="replace"><main>{locations}</main></form>"#),
        )
        .extension_of(1),
    ])
    .unwrap();
    let expected: String = (0..n)
        .map(|i| format!(r#"<w n="{i}"><form><a/></form></w>"#))
        .collect();
    assert_eq!(
        catalogue.combine(1).unwrap().to_xml(),
        format!("<main>{expected}</main>")
    );
}

/// A view whose first spec applies and whose second fails leaves no trace: the other
/// combinations of the catalogue, and a retry of the failed one, see the pristine archs.
#[test]
fn failed_view_leaves_no_partial_result_behind() {
    let catalogue = ViewCatalogue::new(vec![
        view(1, "<form><a/></form>"),
        view(2, r#"<a position="after"><b/></a>"#).primary_of(1),
        view(
            3,
            r#"<data><b position="inside"><c/></b><c position="sideways"/></data>"#,
        )
        .extension_of(2),
    ])
    .unwrap();
    let root_before = catalogue.combine(1).unwrap().to_xml();
    let failed = in_view(3, ViewError::InvalidPosition("sideways".to_owned()));
    // The second spec locates the <c/> the first one inserted, so the first one did apply.
    assert_eq!(catalogue.combine(2), Err(failed.clone()));
    assert_eq!(
        catalogue
            .combine(2)
            .unwrap_err()
            .source()
            .map(ToString::to_string),
        Some("Invalid position attribute: 'sideways'".to_owned())
    );
    assert_eq!(catalogue.combine(1).unwrap().to_xml(), root_before);
    assert_eq!(root_before, "<form><a/></form>");
    assert_eq!(catalogue.combine_many(&[1, 2]), Err(failed));
    assert_eq!(
        catalogue.combine_many(&[1]).unwrap()[0].to_xml(),
        root_before
    );
}

/// One budget bounds a whole batch (`LimitKind::BatchSteps`, review S-2): each combination of
/// `combine_many` keeps its own `EvaluationSteps` budget and draws from the batch's, so a batch of
/// combinations that each fit alone is refused once their sum passes the batch limit, naming the
/// batch limit and the view whose spec crossed it.
#[test]
fn a_batch_of_combinations_shares_one_budget() {
    let views = vec![
        view(1, "<form><a/><b/></form>"),
        view(2, r#"<b position="inside"><c/></b>"#).extension_of(1),
    ];
    let steps = ViewCatalogue::new(views.clone())
        .unwrap()
        .combine(1)
        .unwrap()
        .evaluation_steps();
    // three combinations fit exactly
    let exact = ViewLimits::default()
        .lowered(LimitKind::BatchSteps, 3 * steps)
        .unwrap();
    let fits = ViewCatalogue::with_limits(views.clone(), exact).unwrap();
    assert_eq!(fits.combine_many(&[1, 1, 1]).unwrap().len(), 3);
    // one step short: the third combination crosses the batch limit
    let short = ViewLimits::default()
        .lowered(LimitKind::BatchSteps, 3 * steps - 1)
        .unwrap();
    let too_short = ViewCatalogue::with_limits(views, short).unwrap();
    let err = too_short.combine_many(&[1, 1, 1]).unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            ViewError::LimitExceeded {
                limit: LimitKind::BatchSteps,
                max: 3 * steps - 1,
                actual: Some(3 * steps),
            }
        )
    );
    assert_eq!(
        err.source().map(ToString::to_string),
        Some(format!(
            "evaluation steps per batch of combinations limit exceeded: maximum {}, reached {}",
            3 * steps - 1,
            3 * steps
        ))
    );
    // a single combination is not bounded by the batch limit below its own
    assert_eq!(too_short.combine(1).unwrap().evaluation_steps(), steps);
    assert_eq!(
        ViewLimits::default().get(LimitKind::BatchSteps),
        100_000_000
    );
}

// --- one hostile combination per data-heavy path -------------------------------------------------
//
// Each runs under a budget of 1,000,000 steps. Before attribute scans, string bytes, children,
// mark-table slots and attribute edits were charged, each of these combinations stayed under
// 50,000 steps (and succeeded or failed to locate) while doing work that grows with the data.

const SMALL: usize = 1_000_000;

/// Combines `views` under the small budget and checks that view `failing` stops on it, with the
/// typed error and its message.
fn assert_small_budget_stops(views: Vec<ViewDescriptor>, failing: i64) {
    let limits = ViewLimits::default()
        .lowered(LimitKind::EvaluationSteps, SMALL)
        .unwrap();
    let err = ViewCatalogue::with_limits(views, limits)
        .unwrap()
        .combine(1)
        .unwrap_err();
    assert_eq!(err, in_view(failing, steps_exceeded(SMALL)));
    assert_eq!(
        err.source().map(ToString::to_string),
        Some(
            "evaluation steps per combination limit exceeded: maximum 1000000, reached 1000001"
                .to_owned()
        )
    );
}

/// `count` elements `<f>` with the attributes `a199` down to `a0` (value `1`), the last one with
/// `a0="2"`.
fn wide_attribute_base(count: usize) -> String {
    let attrs: String = (0..200).rev().map(|k| format!(r#" a{k}="1""#)).collect();
    let last = attrs.replace(r#" a0="1""#, r#" a0="2""#);
    format!(
        "<form>{}<f{last}/></form>",
        format!("<f{attrs}/>").repeat(count - 1)
    )
}

/// An element locator naming 200 attributes, each found only after scanning the attributes
/// before it: about 20,000 name comparisons per node, 2,000,000 for 100 nodes.
#[test]
fn wide_attributes_in_an_element_locator_are_charged() {
    let selector: String = (0..200).map(|k| format!(r#" a{k}="1""#)).collect();
    let spec =
        format!(r#"<f{selector} position="attributes"><attribute name="x">1</attribute></f>"#)
            .replace(r#" a199="1" position"#, r#" a199="2" position"#);
    assert_small_budget_stops(
        vec![
            view(1, &wide_attribute_base(100)),
            view(2, &spec).extension_of(1),
        ],
        2,
    );
}

/// `@a0='2'` where `a0` is the last of 200 attributes: 200 comparisons per node, 20,000 per spec
/// over 100 nodes, 2,000,000 for 100 specs.
#[test]
fn wide_attributes_in_an_xpath_predicate_are_charged() {
    let spec = r#"<xpath expr="//f[@a0='2']" position="attributes"><attribute name="x">1</attribute></xpath>"#;
    assert_small_budget_stops(
        vec![
            view(1, &wide_attribute_base(100)),
            view(2, &many_specs(spec, 100)).extension_of(1),
        ],
        2,
    );
}

/// `hasclass('zz')` against 20 class values of 20,000 bytes: about 25,000 steps of splitting per
/// spec, 2,500,000 for 100 specs.
#[test]
fn long_class_values_in_hasclass_are_charged() {
    let classes = "c ".repeat(10_000);
    let base = format!(
        r#"<form>{}<f class="{classes}zz"/></form>"#,
        format!(r#"<f class="{classes}"/>"#).repeat(19)
    );
    let spec = r#"<xpath expr="//f[hasclass('zz')]" position="attributes"><attribute name="x">1</attribute></xpath>"#;
    assert_small_budget_stops(
        vec![
            view(1, &base),
            view(2, &many_specs(spec, 100)).extension_of(1),
        ],
        2,
    );
}

/// Ten extensions insert 10,000 nodes each and ten more remove them: 100,000 dead arena slots
/// under a 63-node live tree. Each 63-step path then zeroes 62 mark tables of 100,000 slots
/// (about 390,000 steps), so the third spec of view 22 crosses the budget.
#[test]
fn mark_tables_over_dead_slots_are_charged() {
    let base = format!("<form>{}{}</form>", "<d>".repeat(62), "</d>".repeat(62));
    let block = format!(
        r#"<form position="inside"><g>{}</g></form>"#,
        "<i/>".repeat(9_998)
    );
    let mut views = vec![view(1, &base)];
    for pair in 0..10 {
        views.push(view(2 + 2 * pair, &block).extension_of(1));
        views.push(view(3 + 2 * pair, r#"<g position="replace"/>"#).extension_of(1));
    }
    let expr = format!("/form{}", "/d".repeat(62));
    let spec = format!(
        r#"<xpath expr="{expr}" position="attributes"><attribute name="x">1</attribute></xpath>"#
    );
    views.push(view(22, &many_specs(&spec, 100)).extension_of(1));
    assert_small_budget_stops(views, 22);
}

/// `remove` with 2,000 tokens against a class of 2,000 tokens: about 5,400 steps per spec,
/// 1,600,000 for 300 specs (the removals are a set, so the real work is linear too).
#[test]
fn attribute_edits_with_many_tokens_are_charged() {
    let current: String = (0..2_000).map(|k| format!("t{k} ")).collect();
    let remove: String = (0..2_000).map(|k| format!("r{k} ")).collect();
    let spec = format!(
        r#"<f position="attributes"><attribute name="class" separator=" " remove="{remove}"/></f>"#
    );
    assert_small_budget_stops(
        vec![
            view(1, &format!(r#"<form><f class="{current}"/></form>"#)),
            view(2, &many_specs(&spec, 300)).extension_of(1),
        ],
        2,
    );
}

/// `text()='…'` with a 4,000-byte literal against ten elements of 101 text nodes of 4,000 bytes
/// each: about 254,000 steps of comparisons and child visits per spec.
#[test]
fn long_text_comparisons_are_charged() {
    let text = "a".repeat(4_000);
    let wanted = format!("{}b", "a".repeat(3_999));
    let body = format!("{text}{}", format!("<c/>{text}").repeat(100));
    let last = format!("{text}{}<c/>{wanted}", format!("<c/>{text}").repeat(99));
    let base = format!(
        "<form>{}<f>{last}</f></form>",
        format!("<f>{body}</f>").repeat(9)
    );
    let spec = format!(
        r#"<xpath expr="//f[text()='{wanted}']" position="attributes"><attribute name="x">1</attribute></xpath>"#
    );
    assert_small_budget_stops(
        vec![
            view(1, &base),
            view(2, &many_specs(&spec, 10)).extension_of(1),
        ],
        2,
    );
}

/// `position="attributes"` cannot grow an element past the per-element attribute limit;
/// replacing an existing attribute at the limit is fine.
#[test]
fn attribute_edits_respect_the_element_attribute_limit() {
    let attrs: String = (0..256).map(|k| format!(r#" a{k}="1""#)).collect();
    let base = format!("<form><f{attrs}/></form>");
    let replace = view(
        2,
        r#"<f position="attributes"><attribute name="a0">2</attribute></f>"#,
    )
    .extension_of(1);
    let combined = ViewCatalogue::new(vec![view(1, &base), replace])
        .unwrap()
        .combine(1)
        .unwrap();
    assert_eq!(
        combined.arch().root().children().next().unwrap().attr("a0"),
        Some("2")
    );
    let add = view(
        2,
        r#"<f position="attributes"><attribute name="b">1</attribute></f>"#,
    )
    .extension_of(1);
    let err = ViewCatalogue::new(vec![view(1, &base), add])
        .unwrap()
        .combine(1)
        .unwrap_err();
    assert_eq!(
        err,
        in_view(
            2,
            ViewError::LimitExceeded {
                limit: LimitKind::ElementAttributes,
                max: 256,
                actual: Some(257)
            }
        )
    );
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("attributes per element limit exceeded: maximum 256, reached 257".to_owned())
    );
}
