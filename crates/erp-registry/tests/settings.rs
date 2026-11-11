#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::Value;
use erp_registry::settings::{ScopeKind, SettingDecl};
#[test]
fn boolean_declarations_require_real_types_and_explicit_policy() {
    let setting = SettingDecl::boolean(
        "acceptance_notes.show_archived",
        "Show archived notes",
        ScopeKind::Global,
        false,
    )
    .unwrap()
    .access(&["base.group_user"], &["base.group_system"])
    .unwrap();
    assert!(setting.validate(&Value::Bool(true)).is_ok());
    assert!(setting.validate(&Value::Bool(false)).is_ok());
    for bad in [Value::Int(1), Value::str("true"), Value::False] {
        assert!(setting.validate(&bad).is_err());
    }
    for bad in ["", "unqualified", "addon..key", "addon.secret\n"] {
        assert!(SettingDecl::boolean(bad, "Label", ScopeKind::Global, false).is_err());
    }
    assert!(SettingDecl::boolean("addon.key", "", ScopeKind::Global, false).is_err());
    assert!(
        SettingDecl::boolean("addon.key", "Label", ScopeKind::Global, false)
            .unwrap()
            .access(&[], &["base.group_system"])
            .is_err()
    );
    assert!(
        SettingDecl::boolean("addon.key", "Label", ScopeKind::Global, false)
            .unwrap()
            .access(
                &["base.group_user", "base.group_user"],
                &["base.group_system"]
            )
            .is_err()
    );
}

#[test]
fn scalar_bounds_and_publication_refuse_invalid_declarations() {
    use erp_registry::settings::SettingsRegistry;
    use std::collections::BTreeSet;
    let text = SettingDecl::string("addon.text", "Text", ScopeKind::Company, "ok", 4).unwrap();
    assert!(text.validate(&Value::str("éé")).is_ok());
    assert!(text.validate(&Value::str("ééa")).is_err());
    let integer =
        SettingDecl::integer("addon.count", "Count", ScopeKind::Global, 2, (0, 10)).unwrap();
    assert!(integer.validate(&Value::Int(11)).is_err());
    assert!(integer.validate(&Value::Float(2.0)).is_err());
    assert!(
        SettingDecl::integer("addon.count", "Count", ScopeKind::Global, 0, (0, i64::MAX)).is_err()
    );
    let float =
        SettingDecl::float("addon.rate", "Rate", ScopeKind::Global, 1.5, (0.0, 2.0)).unwrap();
    assert!(float.validate(&Value::Float(f64::NAN)).is_err());
    assert!(float.validate(&Value::Int(1)).is_err());
    assert!(
        SettingDecl::float(
            "addon.rate",
            "Rate",
            ScopeKind::Global,
            1.5,
            (0.0, f64::INFINITY)
        )
        .is_err()
    );
    let selection = SettingDecl::selection(
        "addon.style",
        "Style",
        ScopeKind::Global,
        "a",
        &[("a", "A"), ("b", "B")],
    )
    .unwrap();
    assert!(selection.validate(&Value::str("b")).is_ok());
    assert!(selection.validate(&Value::str("c")).is_err());
    assert!(
        SettingDecl::selection(
            "addon.style",
            "Style",
            ScopeKind::Global,
            "a",
            &[("a", "A"), ("a", "Another")]
        )
        .is_err()
    );
    let declaration = || {
        SettingDecl::boolean("addon.flag", "Flag", ScopeKind::Global, false)
            .unwrap()
            .access(&["base.group_user"], &["base.group_system"])
            .unwrap()
    };
    let installed = BTreeSet::from(["addon".to_owned()]);
    let registry = SettingsRegistry::build(
        vec![("addon".into(), vec![declaration()])],
        &installed,
        |_| true,
    )
    .unwrap();
    assert_eq!(registry.get("addon.flag").unwrap().key(), "addon.flag");
    assert!(registry.get("database.secret").is_none());
    assert!(
        SettingsRegistry::build(
            vec![("addon".into(), vec![declaration(), declaration()])],
            &installed,
            |_| true
        )
        .is_err()
    );
    assert!(
        SettingsRegistry::build(
            vec![("other".into(), vec![declaration()])],
            &installed,
            |_| true
        )
        .is_err()
    );
    assert!(
        SettingsRegistry::build(
            vec![("addon".into(), vec![declaration()])],
            &BTreeSet::new(),
            |_| true
        )
        .is_err()
    );
    assert!(
        SettingsRegistry::build(
            vec![("addon".into(), vec![declaration()])],
            &installed,
            |_| false
        )
        .is_err()
    );
    assert!(
        SettingsRegistry::build(vec![("addon".into(), vec![text])], &installed, |_| true).is_err()
    );
}
