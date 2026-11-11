//! Tests for `GuardedArch::capture` (GA-2 WP-A).
//! The capture is pure: a `StaticLookup` stands in for the registry.

use std::collections::HashMap;

use crate::arch::NodeIdx;
use crate::guard::{GuardLookup, GuardedArch};
use crate::validate::ArchIssue;
use crate::{CombinedView, LimitKind, ViewCatalogue, ViewDescriptor, ViewError, ViewLimits};

#[derive(Default)]
pub(super) struct StaticLookup {
    generation: u64,
    fields: HashMap<(String, String), (Option<String>, Option<String>)>,
}

impl StaticLookup {
    pub(super) fn new(generation: u64) -> Self {
        Self {
            generation,
            fields: HashMap::new(),
        }
    }

    pub(super) fn field(
        mut self,
        model: &str,
        name: &str,
        groups: Option<&str>,
        comodel: Option<&str>,
    ) -> Self {
        self.fields.insert(
            (model.to_owned(), name.to_owned()),
            (groups.map(str::to_owned), comodel.map(str::to_owned)),
        );
        self
    }

    fn entry(&self, model: &str, field: &str) -> Option<&(Option<String>, Option<String>)> {
        self.fields.get(&(model.to_owned(), field.to_owned()))
    }
}

impl GuardLookup for StaticLookup {
    fn registry_generation(&self) -> u64 {
        self.generation
    }
    fn field_groups(&self, model: &str, field: &str) -> Option<&str> {
        self.entry(model, field)?.0.as_deref()
    }
    fn comodel(&self, model: &str, field: &str) -> Option<&str> {
        self.entry(model, field)?.1.as_deref()
    }
    fn has_field(&self, model: &str, field: &str) -> bool {
        self.entry(model, field).is_some()
    }
}

/// A record guard as `capture` takes it: view id and group ids.
type RecordInput<'a> = (i64, &'a [i64]);

pub(super) const ROOT: &str = "res.partner";

pub(super) fn lookup() -> StaticLookup {
    StaticLookup::new(7)
        .field(ROOT, "name", None, None)
        .field(ROOT, "state", None, None)
        .field(ROOT, "amount", None, None)
        .field(ROOT, "secret", Some("base.group_system"), None)
        .field(ROOT, "line_ids", None, Some("res.line"))
        .field("res.line", "product_id", None, None)
        .field("res.line", "qty", None, None)
        .field("res.line", "cost", Some("base.group_system"), None)
}

pub(super) fn combined(xml: &str) -> CombinedView {
    ViewCatalogue::new(vec![ViewDescriptor::new(1, "v", ROOT, xml)])
        .expect("catalogue")
        .combine(1)
        .expect("combines")
}

pub(super) fn capture_with(xml: &str, lookup: &StaticLookup) -> Result<GuardedArch, ViewError> {
    GuardedArch::capture(combined(xml), ROOT, &[], lookup, &ViewLimits::default())
}

pub(super) fn capture(xml: &str) -> GuardedArch {
    capture_with(xml, &lookup()).expect("captures")
}

fn capture_err(xml: &str) -> ViewError {
    capture_with(xml, &lookup()).expect_err("capture is refused")
}

/// `tag` or `tag#name` of a node, so entries are read as text.
fn label(ga: &GuardedArch, node: NodeIdx) -> String {
    let arch = ga.arch();
    let name = arch
        .attrs_of(node)
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.as_str());
    match name {
        Some(n) => format!("{}#{n}", arch.tag_of(node)),
        None => arch.tag_of(node).to_owned(),
    }
}

fn labelled(ga: &GuardedArch) -> Vec<(String, usize)> {
    let mut v: Vec<_> = ga
        .node_guard_entries()
        .into_iter()
        .map(|(n, g)| (label(ga, n), g))
        .collect();
    v.sort();
    v
}

fn no_groups_left(ga: &GuardedArch) -> bool {
    !ga.arch().to_xml().contains("groups")
}

fn guard_error(e: &ViewError) -> &ArchIssue {
    match e {
        ViewError::GuardCapture(issue) => issue,
        other => panic!("expected GuardCapture, found {other:?}"),
    }
}

#[test]
fn capture_strips_every_groups_attribute() {
    let ga = capture(
        "<form groups=\"base.group_user\"><group groups=\"a.x,!b.y\">\
         <field name=\"name\" groups=\"c.z\"/></group><field name=\"line_ids\">\
         <list><field name=\"product_id\" groups=\"d.w\"/></list></field></form>",
    );
    assert!(no_groups_left(&ga), "{}", ga.arch().to_xml());
    assert_eq!(
        ga.arch().to_xml(),
        "<form><group><field name=\"name\"/></group><field name=\"line_ids\">\
         <list><field name=\"product_id\"/></list></field></form>"
    );
}

#[test]
fn capture_without_groups_has_no_guards() {
    let ga = capture("<form><field name=\"name\"/></form>");
    assert_eq!(ga.node_guard_count(), 0);
    assert!(ga.guard_texts().is_empty());
    assert!(ga.node_guard_entries().is_empty());
}

#[test]
fn effective_guard_intersects_parent_own_and_field_definition() {
    let ga = capture(
        "<form><group groups=\"a.x\"><span/><div groups=\"b.y\">\
         <field name=\"secret\"/></div></group></form>",
    );
    assert_eq!(
        ga.guard_texts(),
        ["a.x", "a.x & b.y", "a.x & b.y & base.group_system"]
    );
    assert_eq!(
        labelled(&ga),
        [
            ("div".to_owned(), 1),
            ("field#secret".to_owned(), 2),
            ("group".to_owned(), 0),
        ]
    );
    assert_eq!(ga.node_guard_count(), 3);
}

#[test]
fn field_definition_groups_guard_the_node() {
    let ga = capture("<form><field name=\"name\"/><field name=\"secret\"/></form>");
    assert_eq!(ga.guard_texts(), ["base.group_system"]);
    assert_eq!(labelled(&ga), [("field#secret".to_owned(), 0)]);
}

#[test]
fn equal_guards_share_an_index() {
    let ga = capture(
        "<form><a groups=\"a.x,b.y\"/><b groups=\"b.y,a.x\"/><c groups=\"a.x,a.x,b.y\"/>\
         <d groups=\"a.x\"/></form>",
    );
    assert_eq!(ga.guard_texts(), ["a.x,b.y", "a.x"]);
    assert_eq!(
        labelled(&ga),
        [
            ("a".to_owned(), 0),
            ("b".to_owned(), 0),
            ("c".to_owned(), 0),
            ("d".to_owned(), 1),
        ]
    );
}

#[test]
fn guard_text_is_canonical() {
    let ga = capture(
        "<form><a groups=\"b.y,a.x\"/><b groups=\"!x.n\"/><c groups=\"a.x, !b.y\"/>\
         <d groups=\".\"/><e groups=\"!b.y,a.x,!b.y\"/></form>",
    );
    assert_eq!(ga.guard_texts(), ["a.x,b.y", "!x.n", "a.x,!b.y", "."]);
    // `a.x,!b.y` and `!b.y,a.x,!b.y` are the same guard.
    assert_eq!(labelled(&ga)[4], ("e".to_owned(), 2));
}

#[test]
fn never_absorbs_nested_guards() {
    let ga = capture("<form><group groups=\".\"><span groups=\"a.x\"/></group></form>");
    assert_eq!(ga.guard_texts(), ["."]);
    assert_eq!(
        labelled(&ga),
        [("group".to_owned(), 0), ("span".to_owned(), 0)]
    );
}

#[test]
fn capture_is_deterministic() {
    let xml = "<form><group groups=\"a.x\"><field name=\"secret\"/></group></form>";
    assert_eq!(capture(xml), capture(xml));
    let other = "<form><group groups=\"a.y\"><field name=\"secret\"/></group></form>";
    assert_ne!(capture(xml), capture(other));
}

#[test]
fn malformed_reference_is_refused_with_origin() {
    for (value, problem) in [
        ("nodot", "has no module part"),
        (".name", "has an empty module part"),
        ("mod.", "has an empty name part"),
        ("a.b.c", "has more than one dot"),
        (
            "m-d.name",
            "has a module part that is not ASCII letters, digits and '_'",
        ),
    ] {
        let xml = format!("<form>\n<a groups=\"{value}\"/></form>");
        let e = capture_err(&xml);
        assert_eq!(
            e.to_string(),
            format!(
                "Invalid groups attribute '{value}': the term '{value}' is not a group external \
                 id (module.name): it {problem} (view 1, line 2)"
            ),
            "{value}"
        );
        assert!(matches!(guard_error(&e), ArchIssue::InvalidGroups { .. }));
    }
}

#[test]
fn open_guards_are_refused_and_empty_terms_are_ignored() {
    let open = "the group spec has no group reference; an open guard is not allowed";
    for value in ["", "   ", ",", "!", " , ! , "] {
        let e = capture_err(&format!("<form>\n<a groups=\"{value}\"/></form>"));
        assert_eq!(
            e.to_string(),
            format!("Invalid groups attribute '{value}': {open} (view 1, line 2)"),
            "{value:?}"
        );
    }
    for (with_empty, plain) in [
        ("a.x,,b.y", "a.x,b.y"),
        ("a.x,", "a.x"),
        ("a.x,!", "a.x"),
        (",a.x", "a.x"),
    ] {
        let tail = "<field name=\"secret\"/></a></form>";
        assert_eq!(
            capture(&format!("<form><a groups=\"{with_empty}\">{tail}")),
            capture(&format!("<form><a groups=\"{plain}\">{tail}")),
            "{with_empty:?}"
        );
    }
    // `.` only stands alone.
    let e = capture_err("<form><a groups=\"a.x,.\"/></form>");
    assert!(
        matches!(guard_error(&e), ArchIssue::InvalidGroups { .. }),
        "{e}"
    );
}

#[test]
fn malformed_field_definition_groups_are_refused() {
    let bad = lookup().field(ROOT, "broken", Some("nodot"), None);
    let e = capture_with("<form>\n<field name=\"broken\"/></form>", &bad).expect_err("refused");
    assert_eq!(
        e.to_string(),
        "Invalid groups attribute 'nodot': field 'res.partner.broken' definition: the term \
         'nodot' is not a group external id (module.name): it has no module part \
         (view 1, line 2)"
    );
}

fn refs(n: usize) -> String {
    (0..n)
        .map(|i| format!("m.g{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn sixty_five_references_in_one_attribute_are_refused() {
    let ok = capture(&format!("<form><a groups=\"{}\"/></form>", refs(64)));
    assert_eq!(ok.node_guard_count(), 1);
    let e = capture_err(&format!("<form>\n<a groups=\"{}\"/></form>", refs(65)));
    assert_eq!(
        e.to_string(),
        "The groups attribute lists 65 groups, more than the 64 allowed (view 1, line 2)"
    );
    assert!(matches!(
        guard_error(&e),
        ArchIssue::TooManyGroups {
            count: 65,
            max: 64,
            ..
        }
    ));
}

#[test]
fn oversized_effective_guard_is_refused() {
    let first: String = (0..40)
        .map(|i| format!("m.a{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let second: String = (0..40)
        .map(|i| format!("m.b{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let e = capture_err(&format!(
        "<form groups=\"{first}\">\n<a groups=\"{second}\"/></form>"
    ));
    assert_eq!(
        e.to_string(),
        "The groups attribute lists 80 groups, more than the 64 allowed (view 1, line 2)"
    );
}

fn many_guards(n: usize) -> String {
    let mut xml = String::from("<form>");
    for i in 0..n {
        xml.push_str(&format!("<a groups=\"m.g{i}\"/>"));
    }
    xml.push_str("</form>");
    xml
}

#[test]
fn distinct_guard_limit_is_enforced() {
    let ok = capture(&many_guards(4096));
    assert_eq!(ok.guard_texts().len(), 4096);
    let e = capture_err(&many_guards(4097));
    assert_eq!(
        e,
        ViewError::LimitExceeded {
            limit: LimitKind::CheckGroupRefs,
            max: 4096,
            actual: Some(4097),
        }
    );
    assert_eq!(
        e.to_string(),
        "group references in one view check limit exceeded: maximum 4096, reached 4097"
    );
}

#[test]
fn lowered_guard_limit_applies() {
    let limits = ViewLimits::default()
        .lowered(LimitKind::CheckGroupRefs, 2)
        .expect("lowered");
    let xml = many_guards(3);
    let e =
        GuardedArch::capture(combined(&xml), ROOT, &[], &lookup(), &limits).expect_err("refused");
    assert_eq!(
        e,
        ViewError::LimitExceeded {
            limit: LimitKind::CheckGroupRefs,
            max: 2,
            actual: Some(3),
        }
    );
    // Repeating a guard does not count twice.
    let repeated = "<form><a groups=\"m.g\"/><b groups=\"m.g\"/><c groups=\"m.h\"/></form>";
    assert!(GuardedArch::capture(combined(repeated), ROOT, &[], &lookup(), &limits).is_ok());
}

#[test]
fn fields_record_root_and_comodel_names() {
    let ga = capture(
        "<form><field name=\"name\"/><field name=\"line_ids\"><list>\
         <field name=\"product_id\"/><field name=\"cost\"/></list></field></form>",
    );
    let entries: Vec<_> = ga
        .field_entries()
        .into_iter()
        .map(|(_, m, f)| format!("{m}.{f}"))
        .collect();
    assert_eq!(
        entries,
        [
            "res.partner.name",
            "res.partner.line_ids",
            "res.line.product_id",
            "res.line.cost"
        ]
    );
    // The comodel's definition groups guard the sub-view field.
    assert_eq!(ga.guard_texts(), ["base.group_system"]);
    assert_eq!(labelled(&ga), [("field#cost".to_owned(), 0)]);
}

#[test]
fn unknown_field_is_refused_naming_model_and_field() {
    let e = capture_err("<form>\n<field name=\"nope\"/></form>");
    assert_eq!(
        e.to_string(),
        "Field \"nope\" does not exist in model \"res.partner\" (view 1, line 2)"
    );
    assert!(matches!(guard_error(&e), ArchIssue::UnknownField { .. }));
}

#[test]
fn sub_view_field_is_checked_against_the_comodel() {
    // `name` exists on the root model but not on `res.line`.
    let e = capture_err(
        "<form><field name=\"line_ids\"><list>\n<field name=\"name\"/></list></field></form>",
    );
    assert_eq!(
        e.to_string(),
        "Field \"name\" does not exist in model \"res.line\" (view 1, line 2)"
    );
}

#[test]
fn field_without_name_is_refused() {
    let e = capture_err("<form>\n<field/></form>");
    assert_eq!(
        e.to_string(),
        "Field tag must have a \"name\" attribute defined (view 1, line 2)"
    );
}

#[test]
fn sub_view_under_a_scalar_field_is_refused() {
    let e = capture_err("<form>\n<field name=\"name\"><list/></field></form>");
    assert_eq!(
        e.to_string(),
        "Invalid groups attribute '': field 'res.partner.name' holds a <list> sub-view but is not \
         a relational field (view 1, line 2)"
    );
}

#[test]
fn error_names_the_extension_view_and_line() {
    let catalogue = ViewCatalogue::new(vec![
        ViewDescriptor::new(1, "base", ROOT, "<form><field name=\"name\"/></form>"),
        ViewDescriptor::new(
            2,
            "ext",
            ROOT,
            "<field name=\"name\" position=\"after\">\n<b groups=\"bad\"/></field>",
        )
        .extension_of(1),
    ])
    .expect("catalogue");
    let combined = catalogue.combine(1).expect("combines");
    assert_eq!(combined.applied_view_ids(), [1, 2]);
    let e = GuardedArch::capture(combined, ROOT, &[], &lookup(), &ViewLimits::default())
        .expect_err("refused");
    assert!(e.to_string().ends_with("(view 2, line 2)"), "{e}");
}

#[test]
fn record_guards_are_canonical() {
    let combined = combined("<form/>");
    let ga = GuardedArch::capture(
        combined,
        ROOT,
        &[(5, &[3, 1, 3]), (2, &[9])],
        &lookup(),
        &ViewLimits::default(),
    )
    .expect("captures");
    assert_eq!(ga.record_guard_entries(), [(2, vec![9]), (5, vec![1, 3])]);
}

fn record_err(guards: &[RecordInput]) -> ViewError {
    GuardedArch::capture(
        combined("<form/>"),
        ROOT,
        guards,
        &lookup(),
        &ViewLimits::default(),
    )
    .expect_err("refused")
}

#[test]
fn bad_record_guards_are_refused() {
    let cases: [(&[RecordInput], &str); 5] = [
        (
            &[(0, &[1])],
            "invalid record guards: view id 0 is not positive",
        ),
        (
            &[(-3, &[1])],
            "invalid record guards: view id -3 is not positive",
        ),
        (
            &[(4, &[1, 0])],
            "invalid record guards: view 4 has the non-positive group id 0",
        ),
        (
            &[(4, &[])],
            "invalid record guards: view 4 has no groups; leave a view without groups out",
        ),
        (
            &[(4, &[1]), (4, &[2])],
            "invalid record guards: view 4 is listed twice",
        ),
    ];
    for (input, message) in cases {
        let e = record_err(input);
        assert!(matches!(e, ViewError::InvalidRecordGuards(_)));
        assert_eq!(e.to_string(), message);
    }
    let too_many: Vec<RecordInput> = (1..=129).map(|v| (v, &[1_i64][..])).collect();
    assert_eq!(
        record_err(&too_many).to_string(),
        "invalid record guards: 129 record guards, more than the 128 allowed"
    );
}

#[test]
fn accessors_report_the_capture_inputs() {
    let catalogue = ViewCatalogue::new(vec![
        ViewDescriptor::new(1, "base", ROOT, "<form><field name=\"name\"/></form>"),
        ViewDescriptor::new(
            2,
            "ext",
            ROOT,
            "<field name=\"name\" position=\"after\"><span groups=\"a.x\"/></field>",
        )
        .extension_of(1),
    ])
    .expect("catalogue");
    let ga = GuardedArch::capture(
        catalogue.combine(1).expect("combines"),
        ROOT,
        &[],
        &lookup(),
        &ViewLimits::default(),
    )
    .expect("captures");
    assert_eq!(ga.root_model(), ROOT);
    assert_eq!(ga.applied_view_ids(), [1, 2]);
    assert_eq!(ga.registry_generation(), 7);
    assert_eq!(ga.node_guard_count(), 1);
    assert!(ga.retained_bytes() > ga.arch().to_xml().len());
}

#[test]
fn capture_keeps_text_and_tail() {
    let ga = capture(
        "<form><span>(<field name=\"name\" groups=\"a.x\"/>) tail</span><t groups=\"b.y\">x</t> end</form>",
    );
    assert_eq!(
        ga.arch().to_xml(),
        "<form><span>(<field name=\"name\"/>) tail</span><t>x</t> end</form>"
    );
}

/// `(model, path, attribute)` of the expression references, in table order.
fn expression_refs(ga: &GuardedArch) -> Vec<(String, String, String)> {
    ga.expression_entries()
        .into_iter()
        .map(|(_, m, p, a)| (m, p, a))
        .collect()
}

fn triple(model: &str, path: &str, attribute: &str) -> (String, String, String) {
    (model.to_owned(), path.to_owned(), attribute.to_owned())
}

#[test]
fn expression_attributes_record_their_fields() {
    let ga = capture(
        "<form><group invisible=\"state == 'draft'\"><field name=\"name\" \
         readonly=\"amount &gt; 1\" decoration-danger=\"state\"/></group></form>",
    );
    assert_eq!(
        expression_refs(&ga),
        [
            triple(ROOT, "state", "invisible"),
            triple(ROOT, "amount", "readonly"),
            triple(ROOT, "state", "decoration-danger"),
        ]
    );
    let nodes: Vec<_> = ga
        .expression_entries()
        .into_iter()
        .map(|(n, ..)| ga.arch().tag_of(n).to_owned())
        .collect();
    assert_eq!(nodes, ["group", "field", "field"]);
    // the node-level field reference is separate
    assert_eq!(ga.field_entries().len(), 1);
}

#[test]
fn sub_view_expressions_use_the_comodel_and_parent() {
    let ga = capture(
        "<form><field name=\"line_ids\" invisible=\"state\"><list decoration-info=\"qty &gt; 5\">\
         <field name=\"product_id\" column_invisible=\"parent.state == 'x'\" \
         required=\"qty\"/></list></field></form>",
    );
    assert_eq!(
        expression_refs(&ga),
        [
            // the <field> carrying the sub-view belongs to the enclosing model
            triple(ROOT, "state", "invisible"),
            // the <list> and its fields belong to the comodel
            triple("res.line", "qty", "decoration-info"),
            triple(ROOT, "state", "column_invisible"),
            triple("res.line", "qty", "required"),
        ]
    );
}

#[test]
fn field_domain_records_comodel_and_node_model_fields() {
    let ga = capture(
        "<form><field name=\"line_ids\" domain=\"[('qty', '&gt;', amount)]\" \
         context=\"{'default_name': name}\"/></form>",
    );
    assert_eq!(
        expression_refs(&ga),
        [
            triple("res.line", "qty", "domain"),
            triple(ROOT, "amount", "domain"),
            triple(ROOT, "name", "context"),
        ]
    );
}

#[test]
fn unread_attributes_add_no_references() {
    let ga = capture(
        "<form><div t-if=\"nope\" t-att-class=\"nope\"/><field name=\"name\" invisible=\"1\" \
         readonly=\"True\" options=\"{'x': nope}\" widget=\"nope\" \
         context=\"{'a': context.get('b')}\"/></form>",
    );
    assert_eq!(expression_refs(&ga), []);
}

#[test]
fn unknown_field_in_an_expression_is_refused() {
    let e = capture_err("<form>\n<field name=\"name\" invisible=\"nope\"/></form>");
    assert_eq!(
        e.to_string(),
        "Field \"nope\" used in the invisible attribute does not exist in model \"res.partner\" \
         (view 1, line 2)"
    );
    assert!(matches!(
        guard_error(&e),
        ArchIssue::UnknownFieldInExpression { .. }
    ));
    // inside a sub-view the comodel is the model that must have it
    let e = capture_err(
        "<form><field name=\"line_ids\"><list>\n<field name=\"qty\" readonly=\"state\"/></list></field></form>",
    );
    assert_eq!(
        e.to_string(),
        "Field \"state\" used in the readonly attribute does not exist in model \"res.line\" \
         (view 1, line 2)"
    );
}

#[test]
fn unreadable_expressions_are_refused_with_view_and_line() {
    let e = capture_err("<form>\n<field name=\"name\" invisible=\"parent.state\"/></form>");
    assert_eq!(
        e.to_string(),
        "Invalid expression in the invisible attribute 'parent.state': 'parent' is only \
         available inside the sub-view of a relational field (view 1, line 2)"
    );
    assert!(matches!(
        guard_error(&e),
        ArchIssue::InvalidExpression { .. }
    ));

    let e = capture_err("<form>\n<field name=\"name\" domain=\"[('a', '=', 1)]\"/></form>");
    assert_eq!(
        e.to_string(),
        "Invalid expression in the domain attribute '[('a', '=', 1)]': a domain on a field that \
         is not relational makes no sense (view 1, line 2)"
    );

    let syntax = erp_expr::parse_with_limits("state ==", erp_expr::EvalLimits::default())
        .expect_err("does not parse")
        .to_string();
    let e = capture_err("<form>\n<group invisible=\"state ==\"/></form>");
    assert_eq!(
        e.to_string(),
        format!(
            "Invalid expression in the invisible attribute 'state ==': {syntax} (view 1, line 2)"
        )
    );
}

#[test]
fn names_the_plan_cannot_hold_are_refused_at_capture() {
    let e = capture_err("<form><filter domain=\"[('\u{e9}', '=', 1)]\"/></form>");
    assert_eq!(
        e.to_string(),
        "Invalid expression in the domain attribute '[('\u{e9}', '=', 1)]': '\u{e9}' is not a \
         field name (ASCII letters, digits and '_', at most 128 bytes) (view 1, line 1)"
    );
}

#[test]
fn retained_bytes_count_expression_references() {
    let with = capture("<form><group invisible=\"name\"/></form>");
    let without = capture("<form><group invisible=\"1==1\"/></form>");
    assert_eq!(expression_refs(&without), []);
    assert!(
        with.retained_bytes() > without.retained_bytes(),
        "{} against {}",
        with.retained_bytes(),
        without.retained_bytes()
    );
}

#[test]
fn panel_whitespace_domain_is_not_an_absent_domain() {
    let error = capture_err(
        "<search><searchpanel><field name='line_ids' select='multi' domain='  '/></searchpanel></search>",
    );
    assert_eq!(
        error,
        ViewError::InvalidDescription("Search panel section 1 has an invalid domain.".into())
    );
    assert_eq!(
        error.to_string(),
        "Search panel section 1 has an invalid domain."
    );
    for source in ["", "[]", " [] "] {
        capture_with(&format!("<search><searchpanel><field name='line_ids' select='multi' domain='{source}'/></searchpanel></search>"), &lookup()).unwrap();
    }
}
