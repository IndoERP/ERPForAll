// SPDX-License-Identifier: MIT OR Apache-2.0
//! The document viewer's bounds, set in Settings (database-wide). A PDF states its own page count,
//! so a small file can claim any number of pages: the web client reads these two values when it
//! opens a document, refuses to lay out more pages than the first and to print more than the
//! second, and says so by name with the download offered (`document-viewer.ts`).

/// Setting key of the most pages of one document the viewer lays out.
pub(crate) const MAX_PAGES: &str = "web.document_viewer_max_pages";
/// Setting key of the most pages of one document the viewer prepares for printing.
pub(crate) const PRINT_MAX_PAGES: &str = "web.document_viewer_print_max_pages";

fn declarations() -> erp_sdk::Result<Vec<erp_sdk::settings::SettingDecl>> {
    use erp_sdk::settings::{ScopeKind, SettingDecl};
    Ok(vec![
        SettingDecl::integer(
            MAX_PAGES,
            "Document viewer: most pages of one document shown",
            ScopeKind::Global,
            2_000,
            (1, 10_000),
        )?
        .access(&["base.group_user"], &["base.group_system"])?,
        SettingDecl::integer(
            PRINT_MAX_PAGES,
            "Document viewer: most pages of one document printed from the viewer",
            ScopeKind::Global,
            100,
            (1, 1_000),
        )?
        .access(&["base.group_user"], &["base.group_system"])?,
    ])
}
erp_sdk::setting_declarations!(declarations);

#[cfg(test)]
mod tests {
    /// Both bounds are declared settings with a default, a floor and a hard bound, read by
    /// internal users and written by administrators; the refusal itself is the client's.
    #[test]
    fn viewer_bounds_are_declared_with_default_floor_and_hard_bound() {
        use erp_sdk::settings::SettingKind;
        let declarations = super::declarations().unwrap();
        let facts: Vec<_> = declarations
            .iter()
            .map(|setting| {
                let SettingKind::Integer { min, max } = setting.kind() else {
                    panic!("integer required")
                };
                assert_eq!(setting.read_groups(), &["base.group_user"]);
                assert_eq!(setting.write_groups(), &["base.group_system"]);
                (setting.key(), setting.default_value().clone(), *min, *max)
            })
            .collect();
        assert_eq!(
            facts,
            vec![
                (super::MAX_PAGES, erp_sdk::Value::Int(2_000), 1, 10_000),
                (super::PRINT_MAX_PAGES, erp_sdk::Value::Int(100), 1, 1_000),
            ]
        );
    }
}
