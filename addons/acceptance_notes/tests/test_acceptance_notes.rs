//! Native addon declaration contracts; runtime install/authorization is verified by HTTP acceptance.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

#[test]
fn declared_note_requires_name_company_and_owner() {
    let registry =
        erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON, &addon_acceptance_notes::ADDON])
            .build()
            .expect("real linked registry");
    let note = registry
        .model("acceptance_notes.note")
        .expect("note model is linked");
    for field in ["name", "company_id", "owner_id"] {
        assert!(
            note.field(field).expect("declared field").required,
            "{field} must be required"
        );
    }
    assert_eq!(
        note.field("internal_memo").unwrap().groups,
        Some("base.group_system")
    );
    let rich_body = note.field("rich_body").expect("rich_body declared field");
    assert_eq!(rich_body.ty.type_name(), "html");
    assert!(!rich_body.required);
}
