// SPDX-License-Identifier: MIT OR Apache-2.0
//! The kanban board's bounds, set in Settings (database-wide): the most columns one board may show and
//! the most cards one column may hold. The web client reads them when it opens a grouped kanban. A
//! grouping with more columns is refused by name instead of drawn in part; a column at its card bound
//! says how many it shows instead of loading more.

/// Setting key of the most columns a kanban board may show.
pub(crate) const MAX_COLUMNS: &str = "web.kanban_max_columns";
/// Setting key of the most cards one kanban column may hold.
pub(crate) const MAX_COLUMN_CARDS: &str = "web.kanban_max_column_cards";

fn declarations() -> erp_sdk::Result<Vec<erp_sdk::settings::SettingDecl>> {
    use erp_sdk::settings::{ScopeKind, SettingDecl};
    Ok(vec![
        SettingDecl::integer(
            MAX_COLUMNS,
            "Kanban view: most columns a board may show",
            ScopeKind::Global,
            100,
            (10, 1_000),
        )?
        .access(&["base.group_user"], &["base.group_system"])?,
        SettingDecl::integer(
            MAX_COLUMN_CARDS,
            "Kanban view: most cards a column may hold",
            ScopeKind::Global,
            400,
            (40, 4_000),
        )?
        .access(&["base.group_user"], &["base.group_system"])?,
    ])
}
erp_sdk::setting_declarations!(declarations);

#[cfg(test)]
mod tests {
    /// SPEC-VIEW-272 (partial: both bounds are declared settings with a default, a floor and a hard bound, read by internal users and written by administrators; the refusal and the column sentence are the client's)
    #[test]
    fn kanban_bounds_are_declared_with_default_floor_and_hard_bound() {
        use erp_sdk::settings::SettingKind;
        let declarations = super::declarations().unwrap();
        let expected = [
            (super::MAX_COLUMNS, 100, 10, 1_000),
            (super::MAX_COLUMN_CARDS, 400, 40, 4_000),
        ];
        assert_eq!(declarations.len(), expected.len());
        for (setting, (key, default, floor, bound)) in declarations.iter().zip(expected) {
            assert_eq!(setting.key(), key);
            assert_eq!(setting.read_groups(), &["base.group_user"]);
            assert_eq!(setting.write_groups(), &["base.group_system"]);
            assert_eq!(setting.default_value(), &erp_sdk::Value::Int(default));
            let SettingKind::Integer { min, max } = setting.kind() else {
                panic!("integer required")
            };
            assert_eq!((*min, *max), (floor, bound));
        }
    }
}
