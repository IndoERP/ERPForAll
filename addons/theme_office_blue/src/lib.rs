//! Office Blue: an optional, database-wide appearance owned by an installed addon.
//! No models or example business records are added. Settings use the existing durable,
//! audited global settings service; the manifest/registry owns selection availability.

erp_sdk::addon!(name = "theme_office_blue", depends = ["base", "web"]);

fn settings() -> erp_sdk::Result<Vec<erp_sdk::settings::SettingDecl>> {
    use erp_sdk::settings::{ScopeKind, SettingDecl};
    Ok(vec![
        SettingDecl::selection(
            "theme_office_blue.appearance",
            "Database appearance (refresh after saving)",
            ScopeKind::Global,
            "default",
            &[("default", "Default"), ("office-blue", "Office Blue")],
        )?
        .access(&["base.group_user"], &["base.group_system"])?,
    ])
}
erp_sdk::setting_declarations!(settings);

#[cfg(test)]
mod tests {
    #[test]
    fn appearance_is_database_global_internal_read_and_admin_write() {
        use erp_sdk::settings::{ScopeKind, SettingKind};
        let declarations = super::settings().unwrap();
        assert_eq!(declarations.len(), 1);
        let setting = &declarations[0];
        assert_eq!(setting.key(), "theme_office_blue.appearance");
        assert_eq!(setting.scope(), ScopeKind::Global);
        assert_eq!(setting.read_groups(), &["base.group_user"]);
        assert_eq!(setting.write_groups(), &["base.group_system"]);
        assert_eq!(setting.default_value(), &erp_sdk::Value::str("default"));
        let SettingKind::Selection { choices } = setting.kind() else {
            panic!("selection required")
        };
        assert_eq!(
            choices,
            &[
                ("default".into(), "Default".into()),
                ("office-blue".into(), "Office Blue".into())
            ]
        );
    }
}
