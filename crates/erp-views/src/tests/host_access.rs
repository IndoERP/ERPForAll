//! Tests for the `host-access` bridge: `plan_of`, the decision step of `filter` and
//! `FilteredArch` (GA-2 WP-B, §5.3, §21.2).
//!
//! `filter` itself needs a `GuardEvaluation`, which only a transaction can mint, so the checks
//! after the principal check are exercised through the private `decide` step with a stand-in
//! `Verdicts`. The principal check and the real evaluation are covered by the database tests of
//! WP-C.

use std::collections::{HashMap, HashSet};

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::{GroupId, GroupSpec, GuardPlan, GuardPlanBuilder, GuardPlanLimits};

use super::guard_capture::{ROOT, capture, capture_with, combined, lookup};
use crate::ViewLimits;
use crate::arch::{FilterDecision, NodeIdx};
use crate::guard::GuardedArch;
use crate::host_access::{Verdicts, decide, deny_unreadable_comodel_readers, plan_of};

/// Verdicts as plain vectors, with the same out-of-range behaviour as a real evaluation.
struct Fake {
    plan: GuardPlan,
    generation: u64,
    node: Vec<bool>,
    record: Vec<bool>,
    fields: Vec<bool>,
}

impl Fake {
    /// Mark the plan entry `(model, path)` unreadable.
    fn deny(&mut self, model: &str, path: &str) {
        let at = self
            .plan
            .fields()
            .iter()
            .position(|f| f.model() == model && f.path() == path)
            .unwrap_or_else(|| panic!("no plan field {model}.{path}"));
        self.fields[at] = false;
    }

    /// Everything allowed, for the plan of `ga` itself.
    fn allow_all(ga: &GuardedArch) -> Self {
        let plan = plan_of(ga).expect("plan");
        Self {
            node: vec![true; plan.node_guards().len()],
            record: vec![true; plan.record_guards().len()],
            fields: vec![true; plan.fields().len()],
            generation: ga.registry_generation(),
            plan,
        }
    }
}

fn pick(v: &[bool], i: usize, what: &str) -> Result<bool> {
    v.get(i)
        .copied()
        .ok_or_else(|| Error::programming(format!("{what} index {i} outside plan")))
}

impl Verdicts for Fake {
    fn plan(&self) -> &GuardPlan {
        &self.plan
    }
    fn registry_generation(&self) -> u64 {
        self.generation
    }
    fn node_allowed(&self, guard: usize) -> Result<bool> {
        pick(&self.node, guard, "guard")
    }
    fn record_allowed(&self, guard: usize) -> Result<bool> {
        pick(&self.record, guard, "record guard")
    }
    fn field_readable(&self, field: usize) -> Result<bool> {
        pick(&self.fields, field, "field")
    }
}

fn spec(text: &str) -> GroupSpec {
    GroupSpec::parse(text).expect("spec")
}

fn gid(id: i64) -> GroupId {
    GroupId::new(id).expect("group id")
}

fn builder(root: &str) -> GuardPlanBuilder {
    GuardPlanBuilder::new(root, GuardPlanLimits::default()).expect("builder")
}

fn refuse(r: Result<impl std::fmt::Debug>, kind: ErrorKind) -> String {
    let e = r.expect_err("is refused");
    assert_eq!(e.kind(), kind, "{e:?}");
    e.message().to_owned()
}

const FORM: &str = "<form><group groups=\"b.y,a.x\"><span/><div groups=\"c.z,!d.w\">\
                    <field name=\"name\"/></div></group><field name=\"secret\"/></form>";

#[test]
fn plan_of_matches_group_spec_parse_and_intersect() {
    let plan = plan_of(&capture(FORM)).expect("plan");
    let outer = spec("b.y,a.x");
    let inner = outer.intersect(&spec("c.z,!d.w"));
    let secret = spec("base.group_system");
    assert_eq!(plan.root_model(), ROOT);
    assert_eq!(plan.node_guards(), [outer, inner, secret]);
}

#[test]
fn plan_of_keeps_never_guards() {
    let plan = plan_of(&capture(
        "<form><a groups=\".\"><b groups=\"x.y\"/></a></form>",
    ))
    .expect("plan");
    assert_eq!(plan.node_guards(), [GroupSpec::never()]);
}

#[test]
fn equal_tables_give_equal_plans() {
    let a = plan_of(&capture(FORM)).expect("plan");
    let b = plan_of(&capture(FORM)).expect("plan");
    assert_eq!(a, b);
    assert_eq!(a.content_hash(), b.content_hash());
    let other = plan_of(&capture(&FORM.replace("a.x", "a.q"))).expect("plan");
    assert_ne!(a, other);
}

#[test]
fn plan_of_lists_distinct_fields_in_first_occurrence_order() {
    let plan = plan_of(&capture(
        "<form><field name=\"name\"/><field name=\"line_ids\"><list>\
         <field name=\"product_id\"/></list></field><field name=\"name\"/></form>",
    ))
    .expect("plan");
    let got: Vec<_> = plan
        .fields()
        .iter()
        .map(|f| (f.model().to_owned(), f.path().to_owned()))
        .collect();
    assert_eq!(
        got,
        [
            (ROOT.to_owned(), "name".to_owned()),
            (ROOT.to_owned(), "line_ids".to_owned()),
            // the probe of the comodel of a relational field that holds a sub-view
            (ROOT.to_owned(), "line_ids.id".to_owned()),
            ("res.line".to_owned(), "product_id".to_owned()),
        ]
    );
}

#[test]
fn plan_of_carries_record_guards() {
    let ga = GuardedArch::capture(
        combined("<form/>"),
        ROOT,
        &[(5, &[3, 1, 3]), (2, &[9])],
        &lookup(),
        &ViewLimits::default(),
    )
    .expect("captures");
    let plan = plan_of(&ga).expect("plan");
    let got: Vec<_> = plan
        .record_guards()
        .iter()
        .map(|r| (r.view_id(), r.groups().to_vec()))
        .collect();
    assert_eq!(got, [(2, vec![gid(9)]), (5, vec![gid(1), gid(3)])]);
}

#[test]
fn own_plan_passes_and_everything_allowed_equals_the_arch() {
    let ga = capture(FORM);
    let out = decide(&ga, &Fake::allow_all(&ga)).expect("passes");
    assert_eq!(out.to_xml(), ga.arch().to_xml());
    assert_eq!(out.allowed_node_count(), 6);
}

#[test]
fn generation_mismatch_is_a_validation_error() {
    let ga = capture(FORM);
    let mut fake = Fake::allow_all(&ga);
    fake.generation += 1;
    let msg = refuse(decide(&ga, &fake), ErrorKind::Validation);
    assert!(
        msg.contains("generation 7 served under generation 8"),
        "{msg}"
    );
}

#[test]
fn plan_of_another_bundle_is_an_access_error() {
    let ga = capture(FORM);
    let other = capture(&FORM.replace("a.x", "a.q"));
    let mut fake = Fake::allow_all(&ga);
    fake.plan = plan_of(&other).expect("plan");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "guard evaluation does not belong to this view bundle (guard 0 differs)"
    );
}

#[test]
fn plan_with_an_omitted_guard_is_an_access_error() {
    let ga = capture(FORM);
    let mut b = builder(ROOT);
    b.node_guard(spec("b.y,a.x")).expect("guard");
    b.node_guard(spec("b.y,a.x").intersect(&spec("c.z,!d.w")))
        .expect("guard");
    b.field(ROOT, "name").expect("field");
    b.field(ROOT, "secret").expect("field");
    let mut fake = Fake::allow_all(&ga);
    fake.plan = b.finish().expect("plan");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "guard evaluation does not belong to this view bundle (guard count: 2 against 3 differs)"
    );
}

#[test]
fn plan_of_another_model_is_an_access_error() {
    let ga = capture("<form><field name=\"name\"/></form>");
    let mut b = builder("res.line");
    b.field("res.line", "product_id").expect("field");
    let mut fake = Fake::allow_all(&ga);
    fake.plan = b.finish().expect("plan");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert!(msg.contains("root model"), "{msg}");
}

#[test]
fn plan_with_other_fields_or_record_guards_is_an_access_error() {
    let ga = capture("<form><field name=\"name\"/></form>");
    let mut b = builder(ROOT);
    b.field(ROOT, "line_ids").expect("field");
    let mut fake = Fake::allow_all(&ga);
    fake.plan = b.finish().expect("plan");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert!(
        msg.contains("field 0 ('res.partner.name') differs"),
        "{msg}"
    );

    let mut b = builder(ROOT);
    b.record_guard(4, &[gid(1)]).expect("record guard");
    b.field(ROOT, "name").expect("field");
    let mut fake = Fake::allow_all(&ga);
    fake.plan = b.finish().expect("plan");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert!(msg.contains("record guard count: 1 against 0"), "{msg}");
}

#[test]
fn denied_record_guard_names_the_view() {
    let ga = GuardedArch::capture(
        combined("<form/>"),
        ROOT,
        &[(1, &[7])],
        &lookup(),
        &ViewLimits::default(),
    )
    .expect("captures");
    let mut fake = Fake::allow_all(&ga);
    assert!(decide(&ga, &fake).is_ok());
    fake.record[0] = false;
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(msg, "view 1 is restricted to groups the user is not in");
}

#[test]
fn missing_verdict_is_a_programming_error() {
    let ga = capture(FORM);
    let mut fake = Fake::allow_all(&ga);
    fake.node.pop();
    refuse(decide(&ga, &fake), ErrorKind::Programming);
}

#[test]
fn denied_node_is_removed_with_subtree_and_keeps_its_tail() {
    let ga = capture("<form><span>(<b groups=\"a.x\"><i/></b>)</span><c/></form>");
    let mut fake = Fake::allow_all(&ga);
    fake.node[0] = false;
    let out = decide(&ga, &fake).expect("decides");
    assert_eq!(out.to_xml(), "<form><span>()</span><c/></form>");
    assert_eq!(out.allowed_node_count(), 3);
}

#[test]
fn unreadable_field_is_removed() {
    let ga = capture("<form><field name=\"name\"/>x<field name=\"secret\"/>y</form>");
    let mut fake = Fake::allow_all(&ga);
    // fields: name (0), secret (1); secret also carries its definition guard 0
    fake.fields[1] = false;
    let out = decide(&ga, &fake).expect("decides");
    assert_eq!(out.to_xml(), "<form><field name=\"name\"/>xy</form>");
}

#[test]
fn allowed_t_is_unwrapped() {
    let ga = capture("<form><group><t groups=\"a.x\"><field name=\"name\"/></t></group></form>");
    let mut fake = Fake::allow_all(&ga);
    let out = decide(&ga, &fake).expect("decides");
    assert_eq!(
        out.to_xml(),
        "<form><group><field name=\"name\"/></group></form>"
    );
    fake.node[0] = false;
    let out = decide(&ga, &fake).expect("decides");
    assert_eq!(out.to_xml(), "<form><group></group></form>");
}

#[test]
fn filtered_arch_equals_write_xml_filtered_for_the_same_decisions() {
    let ga = capture(FORM);
    let mut fake = Fake::allow_all(&ga);
    fake.node[1] = false;
    let out = decide(&ga, &fake).expect("decides");
    let by_hand = ga.arch().to_xml_filtered(|node: NodeIdx| {
        if ga.arch().tag_of(node) == "div" {
            FilterDecision::Deny
        } else {
            FilterDecision::Allow
        }
    });
    assert_eq!(out.to_xml(), by_hand);
    let mut appended = String::from("<!-- -->");
    out.write_xml(&mut appended);
    assert_eq!(appended, format!("<!-- -->{by_hand}"));
}

fn checked_spec() -> &'static erp_registry::ModelSpec {
    static MODEL: erp_registry::ModelDecl = erp_registry::ModelDecl::new("base", ROOT);
    static REGISTRY: std::sync::OnceLock<erp_registry::Registry> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            erp_registry::RegistryBuilder::new([])
                .with_decls([&MODEL])
                .build()
                .unwrap()
        })
        .model(ROOT)
        .unwrap()
}

#[test]
fn checked_typed_traversal_cannot_reach_denied_nodes() {
    let ga = capture(
        "<form>before<group groups=\"a.x\"><span secret=\"hidden\"/></group>after<field name=\"name\"/></form>",
    );
    let mut verdicts = Fake::allow_all(&ga);
    verdicts.node[0] = false;
    let filtered = decide(&ga, &verdicts).unwrap();
    let input = filtered.into_checked_input(checked_spec());
    let root = input.root().unwrap();
    assert_eq!(input.root_model(), ROOT);
    assert_eq!(root.tag(), "form");
    assert_eq!(root.text(), "before");
    let children: Vec<_> = root.children().collect();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].tag(), "field");
    assert_eq!(children[0].attr("name"), Some("name"));
    assert!(!input.to_xml().contains("hidden"));
    assert_eq!(
        input.to_xml(),
        "<form>beforeafter<field name=\"name\"/></form>"
    );
}

#[test]
fn checked_typed_traversal_retains_unwrap_and_xml_text_semantics() {
    let ga = capture("<form>a<t groups=\"a.x\">b<span label=\"Visible\"/>c</t>d</form>");
    let verdicts = Fake::allow_all(&ga);
    let filtered = decide(&ga, &verdicts).unwrap();
    let expected = filtered.to_xml();
    let input = filtered.into_checked_input(checked_spec());
    let root = input.root().unwrap();
    let unwrapped = root.children().next().unwrap();
    assert!(unwrapped.is_unwrapped());
    assert_eq!(unwrapped.text(), "b");
    assert_eq!(unwrapped.tail(), "d");
    let span = unwrapped.children().next().unwrap();
    assert_eq!(
        span.attributes().collect::<Vec<_>>(),
        [("label", "Visible")]
    );
    assert_eq!(span.tail(), "c");
    assert_eq!(input.to_xml(), expected);
    assert_eq!(expected, "<form>ab<span label=\"Visible\"/>cd</form>");
}

#[test]
fn checked_content_has_allowed_text_and_denied_tail_only() {
    use crate::CheckedContent;
    let ga = capture(
        "<form>a<group groups=\"a.x\">hidden<span/></group>b<field name=\"name\"/>c</form>",
    );
    let mut verdicts = Fake::allow_all(&ga);
    verdicts.node[0] = false;
    let input = decide(&ga, &verdicts)
        .unwrap()
        .into_checked_input(checked_spec());
    let parts: Vec<_> = input
        .root()
        .unwrap()
        .content()
        .map(|part| match part {
            CheckedContent::Text(text) => format!("text:{text}"),
            CheckedContent::Element(node) => format!("element:{}", node.tag()),
        })
        .collect();
    assert_eq!(parts, ["text:a", "text:b", "element:field", "text:c"]);
}

#[test]
fn node_and_expression_references_share_a_plan_entry() {
    let ga = capture("<form><field name=\"name\" invisible=\"name\"/></form>");
    let plan = plan_of(&ga).expect("plan");
    let got: Vec<_> = plan
        .fields()
        .iter()
        .map(|f| (f.model().to_owned(), f.path().to_owned()))
        .collect();
    assert_eq!(got, [(ROOT.to_owned(), "name".to_owned())]);
    // and the bundle is accepted by the comparison
    decide(&ga, &Fake::allow_all(&ga)).expect("passes");
}

#[test]
fn expression_references_are_plan_fields() {
    let ga = capture(
        "<form><field name=\"line_ids\" domain=\"[('qty', '>', amount)]\" invisible=\"state\"/></form>",
    );
    let plan = plan_of(&ga).expect("plan");
    let got: Vec<_> = plan
        .fields()
        .iter()
        .map(|f| (f.model().to_owned(), f.path().to_owned()))
        .collect();
    assert_eq!(
        got,
        [
            (ROOT.to_owned(), "line_ids".to_owned()),
            (ROOT.to_owned(), "line_ids.id".to_owned()),
            ("res.line".to_owned(), "qty".to_owned()),
            (ROOT.to_owned(), "amount".to_owned()),
            (ROOT.to_owned(), "state".to_owned()),
        ]
    );
}

const READS_SECRET: &str =
    "<form>\n<group invisible=\"secret == 1\"><field name=\"name\"/></group></form>";

#[test]
fn visible_node_reading_an_unreadable_field_is_an_access_error() {
    let ga = capture(READS_SECRET);
    let mut fake = Fake::allow_all(&ga);
    // fields: secret (0, read by the group's invisible), name (1)
    fake.fields[0] = false;
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "view 1, line 2: the invisible attribute reads the field 'secret' of model \
         'res.partner', which the user may not read"
    );
}

#[test]
fn denied_nodes_raise_nothing_for_their_expressions() {
    let denied = capture(
        "<form><group groups=\"a.x\" invisible=\"secret == 1\"><field name=\"name\"/></group></form>",
    );
    let mut fake = Fake::allow_all(&denied);
    fake.node[0] = false;
    fake.fields[0] = false;
    let out = decide(&denied, &fake).expect("decides");
    assert_eq!(out.to_xml(), "<form></form>");

    let below =
        capture("<form><div groups=\"a.x\"><span invisible=\"secret == 1\"/></div><i/></form>");
    let mut fake = Fake::allow_all(&below);
    fake.node[0] = false;
    fake.fields[0] = false;
    let out = decide(&below, &fake).expect("decides");
    assert_eq!(out.to_xml(), "<form><i/></form>");

    // an unreadable field node is removed, and its own expression raises nothing
    let node = capture("<form><field name=\"secret\" invisible=\"secret == 1\"/></form>");
    let mut fake = Fake::allow_all(&node);
    fake.fields[0] = false;
    let out = decide(&node, &fake).expect("decides");
    assert_eq!(out.to_xml(), "<form></form>");
}

#[test]
fn a_denied_sibling_does_not_hide_a_surviving_reader() {
    let ga = capture(
        "<form><div groups=\"a.x\" invisible=\"secret == 1\"/><span invisible=\"secret == 1\"/></form>",
    );
    let mut fake = Fake::allow_all(&ga);
    fake.node[0] = false;
    fake.fields[0] = false;
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "view 1, line 1: the invisible attribute reads the field 'secret' of model \
         'res.partner', which the user may not read"
    );
}

#[test]
fn readable_expression_fields_are_served() {
    let ga = capture(READS_SECRET);
    let out = decide(&ga, &Fake::allow_all(&ga)).expect("passes");
    assert_eq!(out.to_xml(), ga.arch().to_xml());
}

const SIBLING_AFTER_SUB_VIEW: &str = "<form>\n<field name=\"line_ids\"><list>\
     <span/><span invisible=\"cost == 1\"/></list></field>\
     <span invisible=\"secret == 1\"/></form>";

#[test]
fn the_sub_view_scope_ends_with_the_sub_view() {
    let ga = capture(SIBLING_AFTER_SUB_VIEW);
    // inside the sub-view the reader goes quietly
    let mut fake = Fake::allow_all(&ga);
    fake.deny(ROOT, "line_ids.id");
    fake.deny("res.line", "cost");
    decide(&ga, &fake).expect("the reader inside the sub-view is removed");
    // the sibling after it is judged by the normal rule
    fake.deny(ROOT, "secret");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "view 1, line 2: the invisible attribute reads the field 'secret' of model \
         'res.partner', which the user may not read"
    );
}

#[test]
fn a_readable_comodel_with_a_restricted_field_is_an_access_error() {
    let ga = capture(
        "<form>\n<field name=\"line_ids\"><list><field name=\"qty\" invisible=\"cost == 1\"/>\
         </list></field></form>",
    );
    let mut fake = Fake::allow_all(&ga);
    fake.deny("res.line", "cost");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "view 1, line 2: the invisible attribute reads the field 'cost' of model \
         'res.line', which the user may not read"
    );
}

#[test]
fn a_node_that_reads_nothing_restricted_stays_in_a_denied_sub_view() {
    let ga = capture(
        "<form><field name=\"line_ids\"><list><span/>\
         <field name=\"qty\" invisible=\"cost == 1\"/></list></field></form>",
    );
    let mut fake = Fake::allow_all(&ga);
    fake.deny(ROOT, "line_ids.id");
    fake.deny("res.line", "cost");
    let out = decide(&ga, &fake).expect("decides");
    assert_eq!(
        out.to_xml(),
        "<form><field name=\"line_ids\"><list><span/></list></field></form>"
    );
}

fn nested() -> super::guard_capture::StaticLookup {
    lookup()
        .field("res.line", "sub_ids", None, Some("res.sub"))
        .field("res.sub", "val", None, None)
}

const NESTED: &str = "<form>\n<field name=\"line_ids\"><list>\
     <field name=\"sub_ids\"><list><span invisible=\"val == 1\"/></list></field>\
     <span invisible=\"qty == 1\"/></list></field></form>";

#[test]
fn nested_sub_views_are_judged_per_comodel() {
    let ga = capture_with(NESTED, &nested()).expect("captures");

    // only the inner comodel is unreadable: its reader goes, the outer reader is judged normally
    let mut fake = Fake::allow_all(&ga);
    fake.deny("res.line", "sub_ids.id");
    fake.deny("res.sub", "val");
    let out = decide(&ga, &fake).expect("the inner reader is removed");
    assert_eq!(
        out.to_xml(),
        "<form>\n<field name=\"line_ids\"><list><field name=\"sub_ids\"><list></list></field>\
         <span invisible=\"qty == 1\"/></list></field></form>"
    );
    fake.deny("res.line", "qty");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "view 1, line 2: the invisible attribute reads the field 'qty' of model \
         'res.line', which the user may not read"
    );

    // the outer comodel is unreadable: everything below it that reads a restricted field goes
    // (the fake leaves `sub_ids.id` readable; a real evaluation would deny it too, which changes nothing here)
    let mut fake = Fake::allow_all(&ga);
    fake.deny(ROOT, "line_ids.id");
    fake.deny("res.sub", "val");
    fake.deny("res.line", "qty");
    let out = decide(&ga, &fake).expect("both readers are removed");
    assert_eq!(
        out.to_xml(),
        "<form>\n<field name=\"line_ids\"><list><field name=\"sub_ids\"><list></list></field>\
         </list></field></form>"
    );
}

const DOMAIN_ONLY: &str =
    "<form>\n<field name=\"line_ids\" domain=\"[('qty', '>', 1)]\"/><span/></form>";

#[test]
fn a_field_domain_without_a_sub_view() {
    let ga = capture(DOMAIN_ONLY);

    // unreadable comodel and a domain that reads an unreadable field: the field is removed
    let mut fake = Fake::allow_all(&ga);
    fake.deny(ROOT, "line_ids.id");
    fake.deny("res.line", "qty");
    let out = decide(&ga, &fake).expect("decides");
    assert_eq!(out.to_xml(), "<form>\n<span/></form>");

    // the unreadable field the domain reads is on the root model, not the comodel: still removed
    let rooted = capture(
        "<form>\n<field name=\"line_ids\" domain=\"[('qty', '>', amount)]\"/><span/></form>",
    );
    let mut fake = Fake::allow_all(&rooted);
    fake.deny(ROOT, "line_ids.id");
    fake.deny(ROOT, "amount");
    let out = decide(&rooted, &fake).expect("decides");
    assert_eq!(out.to_xml(), "<form>\n<span/></form>");

    // unreadable comodel, the domain reads only readable fields: the field stays
    let mut fake = Fake::allow_all(&ga);
    fake.deny(ROOT, "line_ids.id");
    let out = decide(&ga, &fake).expect("decides");
    assert_eq!(out.to_xml(), ga.arch().to_xml());

    // readable comodel and a domain that reads an unreadable field: Access
    let mut fake = Fake::allow_all(&ga);
    fake.deny("res.line", "qty");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "view 1, line 2: the domain attribute reads the field 'qty' of model \
         'res.line', which the user may not read"
    );
}

#[test]
fn a_t_wrapped_sub_view_is_not_scoped_and_gives_access() {
    let ga = capture(
        "<form>\n<field name=\"line_ids\">\n<t><list>\n\
         <span invisible=\"qty == 1\"/></list></t></field></form>",
    );
    // no comodel probe is captured for a field whose sub-view sits under a `<t>`
    assert!(
        plan_of(&ga)
            .expect("plan")
            .fields()
            .iter()
            .all(|f| f.path() != "line_ids.id")
    );
    let mut fake = Fake::allow_all(&ga);
    fake.deny("res.line", "qty");
    let msg = refuse(decide(&ga, &fake), ErrorKind::Access);
    assert_eq!(
        msg,
        "view 1, line 4: the invisible attribute reads the field 'qty' of model \
         'res.line', which the user may not read"
    );
}

#[test]
fn a_missing_decision_slot_keeps_the_reader_registered() {
    let ga = capture(
        "<form><field name=\"line_ids\"><list><span invisible=\"qty == 1\"/></list></field></form>",
    );
    let table = ga.table();
    let comodel = table
        .fields
        .iter()
        .find(|f| &*f.path == "line_ids.id")
        .expect("probe");
    let reader = table
        .fields
        .iter()
        .find(|f| &*f.path == "qty")
        .expect("reader");
    let comodels: HashSet<usize> = [comodel.node.index()].into();
    let reads = || -> HashMap<usize, _> { [(reader.node.index(), reader)].into() };

    // control: with the slot present the reader is removed and unregistered
    let mut decisions = vec![crate::arch::FilterDecision::Allow; ga.arch().node_count()];
    let mut registered = reads();
    deny_unreadable_comodel_readers(
        &ga,
        &mut decisions,
        &mut registered,
        &comodels,
        &HashSet::new(),
    );
    assert!(registered.is_empty());
    assert_eq!(
        decisions[reader.node.index()],
        crate::arch::FilterDecision::Deny
    );

    // no slot for the reader: it stays registered, so the request still fails afterwards
    let mut none: Vec<crate::arch::FilterDecision> = Vec::new();
    let mut registered = reads();
    deny_unreadable_comodel_readers(&ga, &mut none, &mut registered, &comodels, &HashSet::new());
    assert!(registered.contains_key(&reader.node.index()));
}

#[test]
fn typed_panel_admission_prunes_complete_sections_before_callbacks() {
    let ga = capture(
        "<search><searchpanel><field name='secret'/><field name='line_ids' domain=\"[('cost', '=', secret)]\"/></searchpanel><field name='secret'/></search>",
    );
    let mut denied = Fake::allow_all(&ga);
    denied.deny(ROOT, "secret");
    denied.deny("res.line", "cost");
    // The only inherited field-definition guards here come from the denied secret fields.
    denied.node.fill(false);
    let typed = crate::host_access::decide_panel(&ga, &denied).unwrap();
    assert_eq!(
        typed.to_xml(),
        "<search><searchpanel></searchpanel></search>"
    );
    // The existing XML constructor still removes/refuses denied field readers.
    let xml = decide(&ga, &denied);
    assert_eq!(xml.unwrap_err().kind(), ErrorKind::Access);
}

#[test]
fn typed_panel_admission_still_applies_explicit_xml_group_guards() {
    let ga = capture(
        "<search><searchpanel><field name='name' groups='a.x'/><field name='state'/></searchpanel></search>",
    );
    let mut denied = Fake::allow_all(&ga);
    denied.node.fill(false);
    let typed = crate::host_access::decide_panel(&ga, &denied).unwrap();
    assert_eq!(
        typed.to_xml(),
        "<search><searchpanel><field name=\"state\"/></searchpanel></search>"
    );
}

#[test]
fn typed_panel_category_group_denial_removes_dependent_filters_and_preserves_ordinals() {
    let ga = capture(
        "<search><searchpanel><field name='name' groups='a.x'/><field name='line_ids' select='multi' domain=\"[('qty', '=', name)]\"/><field name='state'/></searchpanel></search>",
    );
    let mut denied = Fake::allow_all(&ga);
    denied.node.fill(false);
    let typed = crate::host_access::decide_panel(&ga, &denied).unwrap();
    assert_eq!(
        typed.to_xml(),
        "<search><searchpanel><field name=\"state\"/></searchpanel></search>"
    );
    let input = typed.into_checked_input(checked_spec());
    let section = input
        .root()
        .unwrap()
        .children()
        .next()
        .unwrap()
        .children()
        .next()
        .unwrap();
    assert_eq!(section.source_sibling_ordinal().unwrap(), 3);
    let panel = input.root().unwrap().children().next().unwrap();
    assert_eq!(panel.children().count(), 1);
    assert_eq!(panel.source_child_count(), 3);
}

#[test]
fn dashboard_denied_domain_or_measure_removes_whole_region() {
    for (xml, denied) in [
        (
            r#"<list><dashboard><count name="s" string="Secret" domain="[('secret', '=', 'x')]"/><count name="n" string="Other" domain="[]"/></dashboard><field name="name"/></list>"#,
            "secret",
        ),
        (
            r#"<list><dashboard><kpi name="s" string="Secret" field="secret" date_field="name" aggregate="sum"/></dashboard><field name="name"/></list>"#,
            "secret",
        ),
    ] {
        let ga = capture(xml);
        let mut verdict = Fake::allow_all(&ga);
        verdict.deny(ROOT, denied);
        let result = decide(&ga, &verdict).unwrap();
        let xml = result.to_xml();
        assert!(!xml.contains("dashboard"), "{xml}");
        assert!(xml.contains("name=\"name\""), "{xml}");
    }
}
