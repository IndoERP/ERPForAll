// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
use erp_registry::{BASE_ADDON, FieldDecl, ModelDecl, RegistryBuilder};
use erp_views::{ViewCatalogue, ViewDescriptor, ViewError, validate_dashboard};

static MODEL: ModelDecl = ModelDecl::new("base", "dashboard.fixture").fields(&[
    FieldDecl::char("name"),
    FieldDecl::integer("amount"),
    FieldDecl::date("day"),
]);
fn admit(xml: &str) -> Result<(), ViewError> {
    let registry = RegistryBuilder::new([&BASE_ADDON])
        .with_decls([&MODEL])
        .build()
        .unwrap();
    let views = ViewCatalogue::new(vec![ViewDescriptor::new(
        1,
        "dashboard",
        "dashboard.fixture",
        xml,
    )])?;
    let combined = views.combine(1)?;
    let node = combined.arch().root().children().next().unwrap();
    validate_dashboard(node, registry.model("dashboard.fixture").unwrap())
}

#[test]
fn dashboard_original_admission_rejects_invalid_declarations() {
    for (body, message) in [
        ("", "Dashboard requires at least one item."),
        (
            "unexpected text<count name=\"x\" string=\"X\" domain=\"[]\"/>",
            "Dashboard content must contain metric elements only.",
        ),
        (
            r#"<count name="x" string="X"/>"#,
            "Dashboard count 'x' requires exactly one domain or named filter.",
        ),
        (
            r#"<kpi name="x" string="X" field="name" date_field="day" aggregate="sum"/>"#,
            "Dashboard KPI 'x' requires a stored scalar field 'dashboard.fixture.name' of the declared kind.",
        ),
        (
            r#"<count name="x" string="X" domain="[]" color="purple"/>"#,
            "Dashboard count 'x' has invalid color 'purple'.",
        ),
    ] {
        let error = admit(&format!("<list><dashboard>{body}</dashboard></list>")).unwrap_err();
        assert!(matches!(error, ViewError::InvalidDescription(_)));
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn dashboard_under_kanban_root_names_its_follow_up_task() {
    let error = admit(
        r#"<kanban><dashboard><count name="open" string="Open" filter="open"/></dashboard></kanban>"#,
    )
    .unwrap_err();
    assert!(matches!(error, ViewError::InvalidDescription(_)));
    assert_eq!(
        error.to_string(),
        "A dashboard on a kanban view is not available yet (a later release); \
         place it directly under the list root."
    );
}

#[test]
fn dashboard_original_admission_accepts_count_and_kpi() {
    admit(r#"<list><dashboard><count name="open" string="Open" filter="open"/><kpi name="total" string="Total" field="amount" aggregate="sum" date_field="day"/></dashboard></list>"#).unwrap();
}
