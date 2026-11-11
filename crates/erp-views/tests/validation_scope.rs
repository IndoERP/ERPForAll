//! Regression coverage for CV-1/DR-1 and CV-2/DS-3.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::collections::HashSet;

use erp_views::record::widens_validation;
use erp_views::validate::{ArchIssue, ArchWarning, RegistryLookup, Scope, validate_combined_in};
use erp_views::{ViewCatalogue, ViewDescriptor};

struct Fields;

impl RegistryLookup for Fields {
    fn has_model(&self, model: &str) -> bool {
        model == "m"
    }

    fn has_field(&self, model: &str, field: &str) -> bool {
        model == "m" && matches!(field, "name" | "lines")
    }

    fn has_group(&self, _xmlid: &str) -> bool {
        true
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

fn unknown_field_in_created_subtree(views: Vec<ViewDescriptor>, expected_origin: i64) {
    let catalogue = ViewCatalogue::new(views).unwrap();
    let combined = catalogue.combine(1).unwrap();
    let created = HashSet::from([2]);
    let error = validate_combined_in(
        combined.arch(),
        Some("form"),
        Some("m"),
        &Fields,
        Scope::Views(&created),
    )
    .expect_err("descendants of a created element must be validated");
    assert!(
        matches!(&error, ArchIssue::UnknownField { field, model, .. }
        if field == "gone" && model == "m")
    );
    assert_eq!(error.origin().view_id(), expected_origin);
    assert_eq!(
        error.to_string(),
        "Field \"gone\" does not exist in model \"m\""
    );
}

#[test]
fn direct_move_into_created_group_inherits_validation_scope() {
    unknown_field_in_created_subtree(
        vec![
            ViewDescriptor::new(1, "base", "m", "<form><field name=\"name\"/><field name=\"gone\"/></form>"),
            ViewDescriptor::new(2, "new", "m", "<data><form position=\"inside\"><group name=\"inserted\"/></form><group name=\"inserted\" position=\"inside\"><field name=\"gone\" position=\"move\"/></group></data>").extension_of(1),
        ],
        1,
    );
}

#[test]
fn later_sibling_insertion_inherits_validation_scope() {
    unknown_field_in_created_subtree(
        vec![
            ViewDescriptor::new(1, "base", "m", "<form><field name=\"name\"/></form>"),
            ViewDescriptor::new(
                2,
                "new",
                "m",
                "<field name=\"name\" position=\"after\"><group name=\"inserted\"/></field>",
            )
            .extension_of(1),
            ViewDescriptor::new(
                3,
                "sibling",
                "m",
                "<group name=\"inserted\" position=\"inside\"><field name=\"gone\"/></group>",
            )
            .extension_of(1)
            .with_priority(20),
        ],
        3,
    );
}

#[test]
fn scope_stops_at_subviews_and_resets_after_created_subtrees() {
    let catalogue = ViewCatalogue::new(vec![
        ViewDescriptor::new(1, "base", "m", "<form><field name=\"name\"/><field name=\"gone\"/></form>"),
        ViewDescriptor::new(2, "new", "m", "<field name=\"name\" position=\"after\"><group name=\"inserted\"><field name=\"lines\"><list><field name=\"comodel_only\"/></list></field></group></field>").extension_of(1),
        ViewDescriptor::new(3, "sibling", "m", "<group name=\"inserted\" position=\"inside\"><field name=\"name\"/></group>").extension_of(1).with_priority(20),
    ]).unwrap();
    let combined = catalogue.combine(1).unwrap();
    let created = HashSet::from([2]);
    let warnings = validate_combined_in(
        combined.arch(),
        Some("form"),
        Some("m"),
        &Fields,
        Scope::Views(&created),
    )
    .unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(
        matches!(&warnings.as_slice()[0], ArchWarning::SubViewNotValidated { tag, field, .. }
        if tag == "list" && field == "lines")
    );
}

#[test]
fn literal_and_encoded_replace_positions_both_widen_validation() {
    for position in ["replace", "&#114;eplace", "&#x72;eplace"] {
        let extension =
            format!("<field name=\"name\" position=\"{position}\"><field name=\"name\"/></field>");
        let catalogue = ViewCatalogue::new(vec![
            ViewDescriptor::new(1, "base", "m", "<form><field name=\"name\"/></form>"),
            ViewDescriptor::new(2, "new", "m", extension).extension_of(1),
        ])
        .unwrap();
        catalogue.combine(1).unwrap();
        assert!(
            widens_validation(&catalogue, 2).unwrap(),
            "position={position}"
        );
    }
}

#[test]
fn literal_and_encoded_attributes_positions_both_widen_validation() {
    for position in ["attributes", "&#97;ttributes", "&#x61;ttributes"] {
        let extension = format!(
            "<field name=\"name\" position=\"{position}\"><attribute name=\"string\">Name</attribute></field>"
        );
        let catalogue = ViewCatalogue::new(vec![
            ViewDescriptor::new(1, "base", "m", "<form><field name=\"name\"/></form>"),
            ViewDescriptor::new(2, "new", "m", extension).extension_of(1),
        ])
        .unwrap();
        catalogue.combine(1).unwrap();
        assert!(
            widens_validation(&catalogue, 2).unwrap(),
            "position={position}"
        );
    }
}

#[test]
fn encoded_non_widening_positions_do_not_expand_scope() {
    let catalogue = ViewCatalogue::new(vec![
        ViewDescriptor::new(1, "base", "m", "<form><field name=\"name\"/></form>"),
        ViewDescriptor::new(2, "new", "m", "<field name=\"name\" position=\"&#97;fter\"><label string=\"&#114;eplace &#97;ttributes\"/></field>").extension_of(1),
    ]).unwrap();
    catalogue.combine(1).unwrap();
    assert!(!widens_validation(&catalogue, 2).unwrap());
}

fn check_groups_of(value: &str) -> Result<Vec<ArchWarning>, ArchIssue> {
    let xml = format!("<form><field name=\"name\" groups=\"{value}\"/></form>");
    let combined = ViewCatalogue::new(vec![ViewDescriptor::new(1, "v", "m", &xml)])
        .unwrap()
        .combine(1)
        .unwrap();
    validate_combined_in(
        combined.arch(),
        Some("form"),
        Some("m"),
        &Fields,
        Scope::Whole,
    )
    .map(erp_views::validate::ArchWarnings::into_vec)
}

#[test]
fn install_refuses_what_capture_refuses() {
    for value in ["group_foo", "a.x,group_foo", "a.b.c", "m-d.name", "a.x,."] {
        let error = check_groups_of(value).expect_err(value);
        assert!(
            matches!(&error, ArchIssue::InvalidGroups { .. }),
            "{value}: {error:?}"
        );
        let term = value.rsplit(',').next().unwrap();
        assert!(
            error.to_string().contains(&format!("the term '{term}'")),
            "{value}: {error}"
        );
    }
    for value in ["", "  ", ",", "!"] {
        let error = check_groups_of(value).expect_err(value);
        assert!(
            matches!(error, ArchIssue::InvalidGroups { .. }),
            "{value:?}"
        );
    }
    for value in [
        "a.x,",
        "a.x,,b.y",
        ",a.x",
        "a.x,!",
        "!a.x",
        ".",
        "a.x , !b.y",
    ] {
        check_groups_of(value).unwrap_or_else(|e| panic!("{value:?} refused: {e}"));
    }
}

/// A well-formed but unknown group stays a warning at install.
#[test]
fn unknown_well_formed_group_stays_a_warning() {
    struct NoGroups;
    impl RegistryLookup for NoGroups {
        fn has_model(&self, _: &str) -> bool {
            true
        }
        fn has_field(&self, _: &str, _: &str) -> bool {
            true
        }
        fn has_group(&self, _: &str) -> bool {
            false
        }
        fn field_groups(&self, _: &str, _: &str) -> Option<&str> {
            None
        }
        fn comodel(&self, _: &str, _: &str) -> Option<&str> {
            None
        }
        fn implies(&self, _: &str, _: &str) -> bool {
            false
        }
    }
    let combined = ViewCatalogue::new(vec![ViewDescriptor::new(
        1,
        "v",
        "m",
        "<form><field name=\"name\" groups=\"zz.missing\"/></form>",
    )])
    .unwrap()
    .combine(1)
    .unwrap();
    let warnings = validate_combined_in(
        combined.arch(),
        Some("form"),
        Some("m"),
        &NoGroups,
        Scope::Whole,
    )
    .unwrap();
    assert!(
        warnings
            .as_slice()
            .iter()
            .any(|w| matches!(w, ArchWarning::UnknownGroup { xmlid, .. } if xmlid == "zz.missing"))
    );
}
