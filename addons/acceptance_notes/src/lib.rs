//! Addon `acceptance_notes` for ERPForAll.

erp_sdk::addon!(name = "acceptance_notes", depends = ["base"]);

pub mod models;

/// Native settings owned by Notes and exposed only after its real installation.
fn settings() -> erp_sdk::Result<Vec<erp_sdk::settings::SettingDecl>> {
    use erp_sdk::settings::{ScopeKind, SettingDecl};
    Ok(vec![
        SettingDecl::boolean(
            "acceptance_notes.enabled",
            "Enable shared notes",
            ScopeKind::Global,
            true,
        )?
        .access(&["base.group_system"], &["base.group_system"])?,
    ])
}
erp_sdk::setting_declarations!(settings);

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static NOTE_TITLE_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("acceptance_notes.note_title", &["char"]);
