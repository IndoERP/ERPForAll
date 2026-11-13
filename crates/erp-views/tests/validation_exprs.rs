//! Install-time validation of expression attributes (GA-3 WP-C; design
//! ): the names an expression reads must exist, and a
//! visible node may not read a field it does not guard.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::collections::{HashMap, HashSet};

use erp_views::validate::{
    ArchIssue, ArchWarning, RegistryLookup, Scope, reads_fields, validate_combined,
    validate_combined_in,
};
use erp_views::{ViewCatalogue, ViewDescriptor};

/// Models `m` and `line`; `secret` of `m` and `cost` of `line` are restricted; a group implies
/// itself and whatever `implied` lists.
/// A field's `groups` and comodel.
type Definition = (Option<&'static str>, Option<&'static str>);

struct Reg {
    implied: Vec<(&'static str, &'static str)>,
    fields: HashMap<(String, String), Definition>,
}

impl Reg {
    fn new() -> Self {
        let mut reg = Self {
            implied: vec![("x.manager", "base.group_system"), ("a.y", "a.z")],
            fields: HashMap::new(),
        };
        for (model, field, groups, comodel) in [
            ("m", "name", None, None),
            ("m", "amount", None, None),
            ("m", "secret", Some("base.group_system"), None),
            ("m", "fenced", Some("a.x,!a.y"), None),
            ("m", "broken", Some("nodot"), None),
            ("m", "lines", None, Some("line")),
            ("line", "qty", None, None),
            ("line", "cost", Some("base.group_system"), None),
        ] {
            reg.add(model, field, groups, comodel);
        }
        reg
    }

    fn add(
        &mut self,
        model: &str,
        field: &str,
        groups: Option<&'static str>,
        comodel: Option<&'static str>,
    ) {
        self.fields
            .insert((model.to_owned(), field.to_owned()), (groups, comodel));
    }

    fn entry(&self, model: &str, field: &str) -> Option<&Definition> {
        self.fields.get(&(model.to_owned(), field.to_owned()))
    }
}

impl RegistryLookup for Reg {
    fn has_model(&self, model: &str) -> bool {
        matches!(model, "m" | "line")
    }
    fn has_field(&self, model: &str, field: &str) -> bool {
        self.entry(model, field).is_some()
    }
    fn has_group(&self, _xmlid: &str) -> bool {
        true
    }
    fn field_groups(&self, model: &str, field: &str) -> Option<&str> {
        self.entry(model, field)?.0
    }
    fn comodel(&self, model: &str, field: &str) -> Option<&str> {
        self.entry(model, field)?.1
    }
    fn implies(&self, group: &str, implied: &str) -> bool {
        self.implied.contains(&(group, implied))
    }
}

fn combined(xml: &str) -> erp_views::CombinedView {
    ViewCatalogue::new(vec![ViewDescriptor::new(1, "v", "m", xml)])
        .unwrap()
        .combine(1)
        .unwrap()
}

fn check(xml: &str) -> Result<(), ArchIssue> {
    let c = combined(xml);
    validate_combined(c.arch(), Some("form"), Some("m"), &Reg::new()).map(|w| {
        // sub-view warnings are expected in some cases; others must not appear
        assert!(
            w.as_slice()
                .iter()
                .all(|w| matches!(w, ArchWarning::SubViewNotValidated { .. })),
            "{w:?}"
        );
    })
}

fn refused(xml: &str) -> ArchIssue {
    check(xml).expect_err("the arch is refused")
}

#[test]
fn unknown_name_in_an_expression_is_refused() {
    let issue = refused("<form>\n<field name=\"name\" invisible=\"nope\"/></form>");
    assert_eq!(
        issue,
        ArchIssue::UnknownFieldInExpression {
            attribute: "invisible".to_owned(),
            field: "nope".to_owned(),
            model: "m".to_owned(),
            origin: issue.origin(),
        }
    );
    assert_eq!(
        issue.to_string(),
        "Field \"nope\" used in the invisible attribute does not exist in model \"m\""
    );
    assert_eq!((issue.origin().view_id(), issue.origin().line()), (1, 2));
}

#[test]
fn unreadable_expressions_are_refused() {
    let syntax = erp_expr::parse_with_limits("amount >", erp_expr::EvalLimits::default())
        .expect_err("does not parse")
        .to_string();
    let issue = refused("<form>\n<group readonly=\"amount &gt;\"/></form>");
    assert_eq!(
        issue.to_string(),
        format!("Invalid expression in the readonly attribute 'amount >': {syntax}")
    );
    assert_eq!((issue.origin().view_id(), issue.origin().line()), (1, 2));

    let issue = refused("<form><group invisible=\"parent.amount\"/></form>");
    assert_eq!(
        issue.to_string(),
        "Invalid expression in the invisible attribute 'parent.amount': 'parent' is only \
         available inside the sub-view of a relational field"
    );
}

#[test]
fn field_domains_are_checked_against_both_models() {
    check("<form><field name=\"lines\" domain=\"[('qty', '>', amount)]\"/></form>").expect("valid");
    let issue = refused("<form><field name=\"lines\" domain=\"[('nope', '=', 1)]\"/></form>");
    assert_eq!(
        issue.to_string(),
        "Field \"nope\" used in the domain attribute does not exist in model \"line\""
    );
    let issue = refused("<form><field name=\"lines\" domain=\"[('qty', '=', nope)]\"/></form>");
    assert_eq!(
        issue.to_string(),
        "Field \"nope\" used in the domain attribute does not exist in model \"m\""
    );
    let issue = refused("<form><field name=\"name\" domain=\"[('qty', '=', 1)]\"/></form>");
    assert_eq!(
        issue.to_string(),
        "Invalid expression in the domain attribute '[('qty', '=', 1)]': a domain on a field \
         that is not relational makes no sense"
    );
}

#[test]
fn filter_domains_and_default_order_are_checked() {
    check("<form><filter domain=\"[('name', 'ilike', 'x')]\"/><field name=\"name\" filter_domain=\"[('name', 'ilike', self)]\"/></form>")
        .expect("valid");
    let issue = refused("<form><filter domain=\"[('nope', '=', 1)]\"/></form>");
    assert_eq!(
        issue.to_string(),
        "Field \"nope\" used in the domain attribute does not exist in model \"m\""
    );
    // default_order is read on a list, which this root is not: the list is the root here
    let c = ViewCatalogue::new(vec![ViewDescriptor::new(
        1,
        "v",
        "m",
        "<list default_order=\"name, nope desc\"/>",
    )])
    .unwrap()
    .combine(1)
    .unwrap();
    let issue = validate_combined(c.arch(), Some("list"), Some("m"), &Reg::new()).unwrap_err();
    assert_eq!(
        issue.to_string(),
        "Field \"nope\" used in the default_order attribute does not exist in model \"m\""
    );
}

#[test]
fn sub_view_expressions_are_left_to_the_serve_time_check() {
    let xml = "<form><field name=\"lines\"><list><field name=\"qty\" invisible=\"nope\"/></list></field></form>";
    check(xml).expect("the sub-view is skipped");
    let c = combined(xml);
    let warnings =
        validate_combined(c.arch(), Some("form"), Some("m"), &Reg::new()).expect("valid");
    assert_eq!(warnings.len(), 1);
    // the field element that holds the sub-view is checked
    let issue = refused("<form><field name=\"lines\" invisible=\"nope\"><list/></field></form>");
    assert_eq!(
        issue.to_string(),
        "Field \"nope\" used in the invisible attribute does not exist in model \"m\""
    );
}

#[test]
fn expression_checks_follow_the_validation_scope() {
    let views = vec![
        ViewDescriptor::new(
            1,
            "base",
            "m",
            "<form><field name=\"name\" invisible=\"nope\"/><group name=\"g\"/></form>",
        ),
        ViewDescriptor::new(
            2,
            "ext",
            "m",
            "<data><group name=\"g\" position=\"inside\"><field name=\"amount\" invisible=\"name\"/></group></data>",
        )
        .extension_of(1),
    ];
    let c = ViewCatalogue::new(views).unwrap().combine(1).unwrap();
    let created = HashSet::from([2]);
    let warnings = validate_combined_in(
        c.arch(),
        Some("form"),
        Some("m"),
        &Reg::new(),
        Scope::Views(&created),
    )
    .expect("the base view's stale expression is not the created view's");
    assert!(warnings.is_empty());
    let issue = validate_combined(c.arch(), Some("form"), Some("m"), &Reg::new()).unwrap_err();
    assert_eq!(issue.origin().view_id(), 1);
}

fn restricted(attribute: &str, field: &str, model: &str, groups: &str, issue: &ArchIssue) {
    assert_eq!(
        issue,
        &ArchIssue::RestrictedFieldInExpression {
            attribute: attribute.to_owned(),
            field: field.to_owned(),
            model: model.to_owned(),
            field_groups: groups.to_owned(),
            origin: issue.origin(),
        }
    );
    assert_eq!(
        issue.to_string(),
        format!(
            "The {attribute} attribute reads the field \"{field}\" of model \"{model}\", which is \
             restricted to the groups \"{groups}\"; a visible node may not read a field it does \
             not guard: repeat the field's groups on the node"
        )
    );
}

#[test]
fn an_unguarded_node_reading_a_restricted_field_is_refused() {
    let issue = refused("<form>\n<div invisible=\"secret == 1\"/></form>");
    restricted("invisible", "secret", "m", "base.group_system", &issue);
    assert_eq!((issue.origin().view_id(), issue.origin().line()), (1, 2));
    // a node guarded by an unrelated group is not enough
    let issue = refused("<form><div groups=\"base.group_user\" invisible=\"secret\"/></form>");
    restricted("invisible", "secret", "m", "base.group_system", &issue);
}

#[test]
fn guards_that_cover_the_field_pass() {
    for xml in [
        "<form><div groups=\"base.group_system\" invisible=\"secret\"/></form>",
        "<form><group groups=\"base.group_system\"><div readonly=\"secret\"/></group></form>",
        "<form><div groups=\"x.manager\" invisible=\"secret\"/></form>",
        "<form><div groups=\"x.manager,base.group_system\" invisible=\"secret\"/></form>",
        "<form><div groups=\".\" invisible=\"secret\"/></form>",
        // the field's own definition guards its element
        "<form><field name=\"secret\" invisible=\"secret\"/></form>",
        // an expression that only mentions the field in a string reads nothing
        "<form><div invisible=\"name == 'secret'\"/></form>",
        // two clauses (enclosing and own): one of them covers the field
        "<form><group groups=\"b.p\"><div groups=\"base.group_system\" invisible=\"secret\"/></group></form>",
    ] {
        check(xml).unwrap_or_else(|e| panic!("{xml} refused: {e}"));
    }
}

#[test]
fn the_implied_group_does_not_cover_the_implying_one() {
    // x.manager implies base.group_system (checked above), not the other way round
    let mut reg = Reg::new();
    reg.add("m", "managed", Some("x.manager"), None);
    let c = combined("<form><div groups=\"base.group_system\" invisible=\"managed\"/></form>");
    let issue = validate_combined(c.arch(), Some("form"), Some("m"), &reg).unwrap_err();
    restricted("invisible", "managed", "m", "x.manager", &issue);
}

#[test]
fn negated_field_groups_need_a_negation_on_the_node() {
    // fenced: a.x,!a.y
    let issue = refused("<form><div groups=\"a.x\" invisible=\"fenced\"/></form>");
    restricted("invisible", "fenced", "m", "a.x,!a.y", &issue);
    check("<form><div groups=\"a.x,!a.y\" invisible=\"fenced\"/></form>").expect("same spec");
    // a.y implies a.z, so excluding a.z excludes a.y as well
    check("<form><div groups=\"a.x,!a.z\" invisible=\"fenced\"/></form>").expect("implied");
    // a node whose only terms are negative does not cover a field that has positives
    let issue = refused("<form><div groups=\"!a.y\" invisible=\"fenced\"/></form>");
    restricted("invisible", "fenced", "m", "a.x,!a.y", &issue);
}

#[test]
fn restricted_fields_in_domains_are_refused() {
    let issue = refused("<form><field name=\"lines\" domain=\"[('cost', '>', 1)]\"/></form>");
    restricted("domain", "cost", "line", "base.group_system", &issue);
    check(
        "<form><field name=\"lines\" groups=\"base.group_system\" domain=\"[('cost', '>', 1)]\"/></form>",
    )
    .expect("guarded");
    let issue = refused("<form><filter domain=\"[('lines.cost', '>', secret)]\"/></form>");
    restricted("domain", "cost", "line", "base.group_system", &issue);
}

#[test]
fn a_malformed_field_definition_is_refused() {
    let issue = refused("<form>\n<div invisible=\"broken\"/></form>");
    assert!(
        matches!(&issue, ArchIssue::InvalidGroups { value, .. } if value == "nodot"),
        "{issue:?}"
    );
    assert_eq!(
        issue.to_string(),
        "Invalid groups attribute 'nodot': field 'm.broken' definition: the term 'nodot' is not a \
         group external id (module.name): it has no module part"
    );
}

#[test]
fn reads_fields_tells_which_arches_hold_expressions() {
    let arch = |xml: &str| combined(xml);
    assert!(reads_fields(
        arch("<form><div invisible=\"name\"/></form>").arch()
    ));
    assert!(reads_fields(
        arch("<form><field name=\"lines\" domain=\"[('qty', '=', 1)]\"/></form>").arch()
    ));
    assert!(!reads_fields(
        arch("<form><div invisible=\"1\" readonly=\"True\" t-if=\"name\"/></form>").arch()
    ));
    assert!(!reads_fields(
        arch("<form><field name=\"name\"/></form>").arch()
    ));
}
