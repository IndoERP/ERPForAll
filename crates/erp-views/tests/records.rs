//! The pure record layer: node origins through
//! combination, write-time validation of a combined arch and its order. The database half of
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::HashSet;
use std::num::NonZeroU32;

use erp_views::record::{ViewMode, ViewRow, widens_validation};
use erp_views::validate::{
    ArchIssue, ArchWarning, ArchWarnings, MAX_GROUP_REFS, RegistryLookup, Scope, group_refs,
    validate_combined, validate_combined_in,
};
use erp_views::{CombinedView, NodeOrigin, ViewCatalogue, ViewDescriptor};

struct Fields {
    fields: HashSet<&'static str>,
    groups: HashSet<&'static str>,
}

impl Fields {
    fn new() -> Self {
        Self {
            fields: ["name", "model", "inherit_id", "inherit_children_ids"].into(),
            groups: ["base.group_user"].into(),
        }
    }
}

impl RegistryLookup for Fields {
    fn has_model(&self, model: &str) -> bool {
        model == "ir.ui.view"
    }
    fn has_field(&self, model: &str, field: &str) -> bool {
        model == "ir.ui.view" && self.fields.contains(field)
    }
    fn has_group(&self, xmlid: &str) -> bool {
        self.groups.contains(xmlid)
    }
    fn field_groups(&self, _model: &str, _field: &str) -> Option<&str> {
        None
    }

    fn comodel(&self, _model: &str, _field: &str) -> Option<&str> {
        None
    }

    fn implies(&self, _group: &str, _implied: &str) -> bool {
        false
    }
}

/// The origin at `line` (1-based) of view `view`.
fn at(view: i64, line: u32) -> NodeOrigin {
    NodeOrigin::new(view, NonZeroU32::new(line).expect("a 1-based line"))
}

fn combined(views: Vec<ViewDescriptor>, id: i64) -> CombinedView {
    ViewCatalogue::new(views).unwrap().combine(id).unwrap()
}

fn single(arch: &str) -> CombinedView {
    combined(vec![ViewDescriptor::new(9, "v", "ir.ui.view", arch)], 9)
}

fn check(arch: &str, view_type: &str) -> Result<Vec<ArchWarning>, ArchIssue> {
    let c = single(arch);
    validate_combined(
        c.arch(),
        Some(view_type),
        Some("ir.ui.view"),
        &Fields::new(),
    )
    .map(ArchWarnings::into_vec)
}

/// Lines are counted per element start tag, CRLF and multi-line tags included; spec content
/// keeps the line of its own view, and a `$0` copy keeps the line of what it copies.
#[test]
fn origins_name_the_view_and_line_that_wrote_each_node() {
    let base = "<form>\r\n  <group\r\n     string=\"a\">\r\n    <field name=\"name\"/>\r\n  </group>\n</form>";
    let ext = "<field name=\"name\" position=\"after\">\n\n  <field name=\"model\"/>\n</field>";
    let wrap = "<xpath expr=\"//group\" position=\"replace\">\n<div>$0</div></xpath>";
    let c = combined(
        vec![
            ViewDescriptor::new(1, "base", "ir.ui.view", base),
            ViewDescriptor::new(2, "ext", "ir.ui.view", ext).extension_of(1),
            ViewDescriptor::new(3, "wrap", "ir.ui.view", wrap)
                .extension_of(1)
                .with_priority(20),
        ],
        1,
    );
    let root = c.arch().root();
    assert_eq!(root.origin(), at(1, 1));
    let div = root.children().next().unwrap();
    assert_eq!((div.tag(), div.origin()), ("div", at(3, 2)));
    let group = div.children().next().unwrap();
    assert_eq!((group.tag(), group.origin()), ("group", at(1, 2)));
    let origins: Vec<(String, NodeOrigin)> = group
        .children()
        .map(|f| (f.attr("name").unwrap().to_owned(), f.origin()))
        .collect();
    assert_eq!(
        origins,
        [
            ("name".to_owned(), at(1, 4)),
            ("model".to_owned(), at(2, 3)),
        ]
    );
}

#[test]
fn validation_reports_in_references_order() {
    assert_eq!(
        check("<data><form/></data>", "form"),
        Err(ArchIssue::RootMismatch {
            view_type: Some("form".into()),
            tag: "data".into(),
            origin: at(9, 1),
        })
    );
    let untyped = single("<form/>");
    let issue =
        validate_combined(untyped.arch(), None, Some("ir.ui.view"), &Fields::new()).unwrap_err();
    assert_eq!(
        issue,
        ArchIssue::RootMismatch {
            view_type: None,
            tag: "form".into(),
            origin: at(9, 1),
        }
    );
    assert_eq!(
        issue.to_string(),
        "The root node of a None view should be a <None>, not a <form>"
    );
    assert_eq!(
        check(
            "<form>\n<field name=\"name\" attrs=\"{}\"/>\n<field name=\"nope\"/>\n<field/></form>",
            "form"
        ),
        Err(ArchIssue::UnknownField {
            field: "nope".into(),
            model: "ir.ui.view".into(),
            origin: at(9, 3),
        })
    );
    assert_eq!(
        check("<form>\n<field name=\"\"/></form>", "form"),
        Err(ArchIssue::FieldWithoutName { origin: at(9, 2) })
    );
    assert_eq!(
        check("<form>\n<div>\n<span states=\"a\"/></div></form>", "form"),
        Err(ArchIssue::ObsoleteAttribute {
            attribute: "states",
            origin: at(9, 3),
        })
    );
    let model = single("<form/>");
    assert_eq!(
        validate_combined(model.arch(), Some("form"), Some("no.such"), &Fields::new()),
        Err(ArchIssue::UnknownModel {
            model: Some("no.such".into()),
            origin: at(9, 1),
        })
    );
    let issue = validate_combined(model.arch(), Some("form"), None, &Fields::new()).unwrap_err();
    assert_eq!(
        issue,
        ArchIssue::UnknownModel {
            model: None,
            origin: at(9, 1),
        }
    );
    assert_eq!(issue.to_string(), "Model not found: False");
    assert_eq!(
        check("<form><field name=\"name\"/></form>", "form"),
        Ok(vec![])
    );
}

/// QWeb archs are combined but not validated.
#[test]
fn qweb_is_not_validated() {
    assert_eq!(check("<div><field/></div>", "qweb"), Ok(vec![]));
}

/// A sub-view inside a field is returned as not validated (its fields are the comodel's),
/// never read against the view's model.
#[test]
fn sub_views_are_reported_not_validated() {
    let warnings = check(
        "<form><field name=\"inherit_children_ids\">\n<list><field name=\"not_here\"/></list>\n\
         <form/></field><field name=\"name\"/></form>",
        "form",
    )
    .unwrap();
    assert_eq!(
        warnings,
        [
            ArchWarning::SubViewNotValidated {
                tag: "list".into(),
                field: "inherit_children_ids".into(),
                origin: at(9, 2),
            },
            ArchWarning::SubViewNotValidated {
                tag: "form".into(),
                field: "inherit_children_ids".into(),
                origin: at(9, 3),
            },
        ]
    );
    assert_eq!(warnings[1].origin(), at(9, 3));
    assert!(
        warnings[0]
            .to_string()
            .ends_with(erp_views::validate::FULL_VALIDATION_TASK)
    );
    // only the sub-views are skipped: another child of the field is checked (review A-S8)
    assert!(matches!(
        check(
            "<form><field name=\"inherit_children_ids\"><list><field name=\"x\"/></list>\
             <div><field name=\"nope\"/></div></field></form>",
            "form"
        ),
        Err(ArchIssue::UnknownField { field, .. }) if field == "nope"
    ));
    // the field after the sub-views is checked again
    assert!(matches!(
        check(
            "<form><field name=\"name\"><list/></field><field name=\"x\"/></form>",
            "form"
        ),
        Err(ArchIssue::UnknownField { field, .. }) if field == "x"
    ));
}

/// Unknown groups are warnings (they never match); duplicates and `!` are folded in
/// `group_refs`; more than 64 references is an issue.
#[test]
fn groups_are_checked() {
    let arch = "<form><field name=\"name\" groups=\"base.group_user, !base.nope\"/>\
                <field name=\"model\" groups=\"base.group_user,,other.nope\"/></form>";
    let c = single(arch);
    assert_eq!(
        group_refs(c.arch()).unwrap(),
        ["base.group_user", "base.nope", "other.nope"]
    );
    assert_eq!(
        validate_combined(c.arch(), Some("form"), Some("ir.ui.view"), &Fields::new())
            .map(ArchWarnings::into_vec),
        Ok(vec![
            ArchWarning::UnknownGroup {
                xmlid: "base.nope".into(),
                origin: at(9, 1),
            },
            ArchWarning::UnknownGroup {
                xmlid: "other.nope".into(),
                origin: at(9, 1),
            },
        ])
    );
    let many = (0..=MAX_GROUP_REFS)
        .map(|i| format!("base.g{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let issue = check(&format!("<form><div groups=\"{many}\"/></form>"), "form").unwrap_err();
    assert_eq!(
        issue,
        ArchIssue::TooManyGroups {
            count: 65,
            max: 64,
            origin: at(9, 1),
        }
    );
    assert_eq!(
        issue.to_string(),
        "The groups attribute lists 65 groups, more than the 64 allowed"
    );
}

/// `group_refs` refuses an attribute over [`MAX_GROUP_REFS`] itself, before any reference is
/// collected or looked up (review R-1/S-1), and folds duplicates across many elements in linear
/// time: 10,000 elements naming the same 64 groups give 64 references.
#[test]
fn group_refs_refuses_oversized_attributes_and_dedupes_in_linear_time() {
    let many = (0..=MAX_GROUP_REFS)
        .map(|i| format!("base.g{i}"))
        .collect::<Vec<_>>()
        .join(",");
    // the oversized attribute comes after a valid one: the refusal names its element
    let c = single(&format!(
        "<form><div groups=\"base.a\"/>\n<div groups=\"{many}\"/></form>"
    ));
    let issue = group_refs(c.arch()).unwrap_err();
    assert_eq!(
        issue,
        ArchIssue::TooManyGroups {
            count: 65,
            max: 64,
            origin: at(9, 2),
        }
    );
    assert_eq!(
        issue.to_string(),
        "The groups attribute lists 65 groups, more than the 64 allowed"
    );
    // duplicates within one attribute count against the limit as written (as `check_groups`)
    let twice = vec!["base.a"; MAX_GROUP_REFS + 1].join(",");
    let c = single(&format!("<form><div groups=\"{twice}\"/></form>"));
    assert!(matches!(
        group_refs(c.arch()),
        Err(ArchIssue::TooManyGroups { count: 65, .. })
    ));
    // 64 per attribute is allowed; the same 64 on 9,000 elements fold to 64
    let full = (0..MAX_GROUP_REFS)
        .map(|i| format!("base.g{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let body = format!("<div groups=\"{full}\"/>").repeat(9_000);
    let c = single(&format!("<form>{body}</form>"));
    let refs = group_refs(c.arch()).unwrap();
    assert_eq!(refs.len(), MAX_GROUP_REFS);
    assert_eq!(refs[0], "base.g0");
    assert_eq!(refs[MAX_GROUP_REFS - 1], "base.g63");
}

#[test]
fn a_scoped_validation_raises_only_the_scoped_views_fields() {
    let views = vec![
        ViewDescriptor::new(
            1,
            "root",
            "ir.ui.view",
            "<form><field name=\"name\"/></form>",
        ),
        ViewDescriptor::new(
            2,
            "stale",
            "ir.ui.view",
            "<field name=\"name\" position=\"after\"><field name=\"gone\"/></field>",
        )
        .extension_of(1),
        ViewDescriptor::new(
            3,
            "new",
            "ir.ui.view",
            "<field name=\"name\" position=\"before\"><field name=\"model\"/></field>",
        )
        .extension_of(1),
    ];
    let c = combined(views.clone(), 1);
    let fields = Fields::new();
    let only_new = HashSet::from([3]);
    let validate =
        |scope| validate_combined_in(c.arch(), Some("form"), Some("ir.ui.view"), &fields, scope);
    assert_eq!(
        validate(Scope::Views(&only_new)).map(ArchWarnings::into_vec),
        Ok(vec![])
    );
    let whole = validate(Scope::Whole).unwrap_err();
    assert_eq!(
        whole,
        ArchIssue::UnknownField {
            field: "gone".into(),
            model: "ir.ui.view".into(),
            origin: at(2, 1),
        }
    );
    assert_eq!(
        validate_combined(c.arch(), Some("form"), Some("ir.ui.view"), &fields),
        Err(whole)
    );
    let stale = HashSet::from([2]);
    assert!(matches!(
        validate(Scope::Views(&stale)),
        Err(ArchIssue::UnknownField { field, .. }) if field == "gone"
    ));
    // the root tag is checked whatever the scope
    assert!(matches!(
        validate_combined_in(
            c.arch(),
            Some("list"),
            Some("ir.ui.view"),
            &fields,
            Scope::Views(&only_new)
        ),
        Err(ArchIssue::RootMismatch { .. })
    ));
}

#[test]
fn replace_and_attributes_specs_widen_the_validation() {
    let row = |id, arch: &str| {
        ViewRow::new(id, ViewMode::Extension)
            .arch(arch)
            .inherit_id(Some(1))
            .into_descriptor()
            .unwrap()
    };
    let catalogue = ViewCatalogue::new(vec![
        ViewRow::new(1, ViewMode::Primary)
            .arch("<form><a/></form>")
            .into_descriptor()
            .unwrap(),
        row(2, "<a position=\"after\"><b/></a>"),
        row(3, "<data><a position=\"replace\"><b/></a></data>"),
        row(
            4,
            "<a position=\"attributes\"><attribute name=\"x\">1</attribute></a>",
        ),
        // the words alone are no spec
        row(
            5,
            "<a position=\"after\"><b string=\"replace attributes\"/></a>",
        ),
    ])
    .unwrap();
    let widens: Vec<bool> = [1, 2, 3, 4, 5]
        .into_iter()
        .map(|id| widens_validation(&catalogue, id).unwrap())
        .collect();
    assert_eq!(widens, [false, false, true, true, false]);
    assert_eq!(
        widens_validation(&catalogue, 9),
        Err(erp_views::ViewError::ViewNotFound(9))
    );
}

#[test]
fn issue_messages_keep_references_wording() {
    let messages: Vec<String> = [
        check("<form><field name=\"not_a_field\"/></form>", "form"),
        check("<form><field/></form>", "form"),
        check("<data><div/><form/></data>", "form"),
        check("<form><field name=\"name\" attrs=\"{}\"/></form>", "form"),
    ]
    .into_iter()
    .map(|r| r.unwrap_err().to_string())
    .collect();
    assert_eq!(
        messages,
        [
            "Field \"not_a_field\" does not exist in model \"ir.ui.view\"",
            "Field tag must have a \"name\" attribute defined",
            "The root node of a form view should be a <form>, not a <data>",
            "Since 17.0, the \"attrs\" and \"states\" attributes are no longer used.",
        ]
    );
}

/// A tree over the view limit is refused when the catalogue is built.
#[test]
fn oversized_trees_are_refused() {
    let views: Vec<ViewDescriptor> = (1..=10_001)
        .map(|id| ViewDescriptor::new(id, "v", "m", "<a/>"))
        .collect();
    assert_eq!(
        ViewCatalogue::new(views).map(|_| ()),
        Err(erp_views::ViewError::LimitExceeded {
            limit: erp_views::LimitKind::CatalogueViews,
            max: 10_000,
            actual: Some(10_001),
        })
    );
}
