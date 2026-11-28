// SPDX-License-Identifier: MIT OR Apache-2.0
//! SQL-free checks: the project models link with their real parents, carry the fields the views
//! and the board rely on, and the manifest ships exactly the files of the addon.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn registry() -> erp_sdk::Registry {
    erp_sdk::RegistryBuilder::new([
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_mail::ADDON,
        &erp_addon_project::ADDON,
    ])
    .build()
    .expect("project registry composition")
}

#[test]
fn the_four_models_link_with_their_fields() {
    let registry = registry();
    for (model, fields) in [
        ("project.tags", &["name", "color"][..]),
        (
            "project.task.type",
            &["name", "sequence", "fold", "description", "project_id"][..],
        ),
        (
            "project.project",
            &[
                "name",
                "description",
                "active",
                "sequence",
                "color",
                "partner_id",
                "user_id",
                "tag_ids",
                "date_start",
                "date",
                "company_id",
                "task_ids",
                "type_ids",
                "task_count",
                "open_task_count",
                "message_ids",
                "activity_ids",
            ][..],
        ),
        (
            "project.task",
            &[
                "name",
                "description",
                "active",
                "sequence",
                "color",
                "project_id",
                "stage_id",
                "user_ids",
                "partner_id",
                "tag_ids",
                "priority",
                "state",
                "date_deadline",
                "parent_id",
                "child_ids",
                "company_id",
                "date_last_stage_update",
                "message_ids",
                "activity_ids",
            ][..],
        ),
    ] {
        let decl = registry.model(model).unwrap_or_else(|| panic!("{model}"));
        for field in fields {
            assert!(decl.field(field).is_some(), "missing {model}.{field}");
        }
    }
    let task = registry.model("project.task").unwrap();
    assert!(task.field("name").unwrap().required);
    assert!(task.field("priority").unwrap().required);
    assert!(
        registry
            .model("project.project")
            .unwrap()
            .field("name")
            .unwrap()
            .required
    );
    assert!(
        registry
            .model("project.tags")
            .unwrap()
            .field("name")
            .unwrap()
            .required
    );
}

/// The board scopes its columns by project: the stage needs a stored many2one with the same name
/// and comodel as the task's scope field, and the task's group-by and scope fields are distinct
/// stored many2ones.
#[test]
fn the_stage_project_is_the_stored_scope_of_the_task_board() {
    let registry = registry();
    let stage_project = registry
        .model("project.task.type")
        .unwrap()
        .field("project_id")
        .unwrap();
    assert_eq!(stage_project.ty.type_name(), "many2one");
    assert!(stage_project.store, "stage project_id must be stored");
    assert_eq!(stage_project.ty.comodel(), Some("project.project"));
    let task = registry.model("project.task").unwrap();
    for (field, comodel) in [
        ("project_id", "project.project"),
        ("stage_id", "project.task.type"),
    ] {
        let decl = task.field(field).unwrap();
        assert_eq!(decl.ty.type_name(), "many2one", "{field}");
        assert!(decl.store, "{field} must be stored");
        assert_eq!(decl.ty.comodel(), Some(comodel), "{field}");
    }
    let stages = registry
        .model("project.project")
        .unwrap()
        .field("type_ids")
        .unwrap();
    match &stages.ty {
        erp_sdk::FieldType::One2many { comodel, inverse } => {
            assert_eq!(comodel, "project.task.type");
            assert_eq!(inverse, "project_id");
        }
        other => panic!("type_ids must be a one2many, not {}", other.type_name()),
    }
}

#[test]
fn counts_are_computed_integers_that_are_not_stored() {
    let registry = registry();
    for field in ["task_count", "open_task_count"] {
        let decl = registry
            .model("project.project")
            .unwrap()
            .field(field)
            .unwrap();
        assert_eq!(decl.ty.type_name(), "integer", "{field}");
        assert!(!decl.store, "{field} must not be stored");
    }
}

#[test]
fn history_tracks_the_fields_of_the_spec() {
    let registry = registry();
    let tracked = |model: &str, field: &str| {
        registry
            .model(model)
            .unwrap()
            .field(field)
            .unwrap_or_else(|| panic!("{model}.{field}"))
            .tracking
            .is_some()
    };
    for field in [
        "project_id",
        "stage_id",
        "user_ids",
        "partner_id",
        "priority",
        "state",
        "date_deadline",
    ] {
        assert!(tracked("project.task", field), "project.task.{field}");
    }
    for field in ["partner_id", "user_id"] {
        assert!(tracked("project.project", field), "project.project.{field}");
    }
}

#[test]
fn the_last_stage_update_is_read_only_and_not_copied() {
    let registry = registry();
    let decl = registry
        .model("project.task")
        .unwrap()
        .field("date_last_stage_update")
        .unwrap();
    assert_eq!(decl.ty.type_name(), "datetime");
    assert!(decl.readonly);
    assert!(!decl.copy);
}

#[test]
fn the_task_state_has_exactly_the_five_states() {
    let registry = registry();
    let decl = registry
        .model("project.task")
        .unwrap()
        .field("state")
        .unwrap();
    assert!(decl.required);
    let erp_sdk::FieldType::Selection {
        values: Some(values),
        ..
    } = &decl.ty
    else {
        panic!("state must be a static selection");
    };
    let keys: Vec<&str> = values.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "in_progress",
            "changes_requested",
            "approved",
            "done",
            "cancelled"
        ]
    );
    assert_eq!(erp_addon_project::CLOSED_STATES, &["done", "cancelled"][..]);
}

fn manifest() -> toml::Table {
    include_str!("../manifest.toml")
        .parse()
        .expect("valid manifest")
}

fn strings(table: &toml::Table, key: &str) -> Vec<String> {
    table[key]
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_owned())
        .collect()
}

fn shipped(dirs: &[&str]) -> BTreeSet<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = BTreeSet::new();
    for dir in dirs {
        for entry in fs::read_dir(root.join(dir)).expect("addon directory") {
            let path = entry.expect("entry").path();
            if path.is_file() {
                out.insert(
                    path.strip_prefix(root)
                        .expect("relative")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    out
}

/// The data list is fixed (the views and demo files are written against these names), every
/// listed file exists and no file of the addon is left unlisted.
#[test]
fn manifest_lists_exactly_the_shipped_files() {
    let manifest = manifest();
    let data = strings(&manifest, "data");
    assert_eq!(
        data,
        [
            "security/project_security.xml",
            "security/ir.access.csv",
            "data/project_data.xml",
            "views/project_tags_views.xml",
            "views/project_task_type_views.xml",
            "views/project_task_views.xml",
            "views/project_project_views.xml",
            "views/project_overview_views.xml",
            "views/project_menus.xml",
        ]
    );
    assert_eq!(
        data.iter().cloned().collect::<BTreeSet<_>>(),
        shipped(&["security", "data", "views"])
    );
    let demo = strings(&manifest, "demo");
    assert_eq!(demo, ["demo/project_demo.xml"]);
    assert_eq!(
        demo.into_iter().collect::<BTreeSet<_>>(),
        shipped(&["demo"])
    );
    assert_eq!(manifest["version"].as_str(), Some("1.0.0"));
    assert_eq!(manifest["icon"].as_str(), Some("lucide:folder-kanban"));
}

#[test]
fn manifest_depends_match_the_descriptor() {
    let from_manifest: BTreeSet<String> = strings(&manifest(), "depends").into_iter().collect();
    let from_descriptor: BTreeSet<String> = erp_addon_project::ADDON
        .depends
        .iter()
        .map(|d| (*d).to_owned())
        .collect();
    assert_eq!(from_manifest, from_descriptor);
}

/// The task board shows the selected project's own stages.
#[test]
fn the_task_board_is_scoped_by_project() {
    let views = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("views/project_task_views.xml"),
    )
    .expect("task views");
    assert!(
        views.contains("group_scope=\"project_id\""),
        "the task kanban must declare group_scope=\"project_id\""
    );
}
