#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
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

#[test]
fn setting_decl_app_block_sequence_builders_and_defaults() {
    let base = SettingDecl::boolean(
        "sale_stock.allow_returns",
        "Allow Returns",
        ScopeKind::Global,
        false,
    )
    .unwrap();
    assert_eq!(base.target_app(), "sale_stock");
    assert_eq!(base.block_title(), "General");
    assert_eq!(base.display_sequence(), 100);

    let customized = base
        .app("stock")
        .unwrap()
        .block("Operations")
        .unwrap()
        .sequence(20);
    assert_eq!(customized.target_app(), "stock");
    assert_eq!(customized.block_title(), "Operations");
    assert_eq!(customized.display_sequence(), 20);

    // Bad app names
    for bad_app in ["", "Stock", "stock.sub", "stock-core", "stock\n"] {
        assert!(
            SettingDecl::boolean("sale_stock.test", "Test", ScopeKind::Global, false)
                .unwrap()
                .app(bad_app)
                .is_err()
        );
    }

    // Bad block names
    for bad_block in ["", "Control\nBlock", "\tTabs"] {
        assert!(
            SettingDecl::boolean("sale_stock.test", "Test", ScopeKind::Global, false)
                .unwrap()
                .block(bad_block)
                .is_err()
        );
    }
    let long_block = "a".repeat(257);
    assert!(
        SettingDecl::boolean("sale_stock.test", "Test", ScopeKind::Global, false)
            .unwrap()
            .block(&long_block)
            .is_err()
    );
}

#[test]
fn setting_registry_validates_key_ownership_and_installed_target_app() {
    use erp_registry::settings::SettingsRegistry;
    use std::collections::BTreeSet;

    let contrib = SettingDecl::boolean(
        "sale_stock.allow_returns",
        "Allow Returns",
        ScopeKind::Global,
        false,
    )
    .unwrap()
    .app("stock")
    .unwrap()
    .block("Operations")
    .unwrap()
    .access(&["base.group_user"], &["base.group_system"])
    .unwrap();

    // Valid: declaring addon is sale_stock, target app is stock, both installed
    let installed = BTreeSet::from(["sale_stock".to_owned(), "stock".to_owned()]);
    let registry = SettingsRegistry::build(
        vec![("sale_stock".into(), vec![contrib.clone()])],
        &installed,
        |_| true,
    )
    .unwrap();
    assert_eq!(
        registry
            .get("sale_stock.allow_returns")
            .unwrap()
            .target_app(),
        "stock"
    );
    assert_eq!(
        registry
            .get("sale_stock.allow_returns")
            .unwrap()
            .block_title(),
        "Operations"
    );
    assert_eq!(
        registry
            .get("sale_stock.allow_returns")
            .unwrap()
            .display_sequence(),
        100
    );

    // Refusal: target app 'stock' is NOT in installed addons set
    let only_sale_stock = BTreeSet::from(["sale_stock".to_owned()]);
    assert!(
        SettingsRegistry::build(
            vec![("sale_stock".into(), vec![contrib.clone()])],
            &only_sale_stock,
            |_| true,
        )
        .is_err()
    );

    // Refusal: key prefix does NOT match declaring addon owner (owner is stock, key is sale_stock.allow_returns)
    assert!(
        SettingsRegistry::build(
            vec![("stock".into(), vec![contrib.clone()])],
            &installed,
            |_| true,
        )
        .is_err()
    );
}

#[test]
fn non_setting_content_is_excluded_from_scalar_setting_decl() {
    // SettingDecl only accepts valid scalar values; it does not represent button actions or links
    let decl = SettingDecl::boolean("base.flag", "Flag", ScopeKind::Global, true).unwrap();
    assert!(decl.validate(&Value::Bool(true)).is_ok());
    // Action mappings or non-scalar types are rejected
    assert!(decl.validate(&Value::list([Value::str("button")])).is_err());
}

fn invalid_declaration_message(error: &erp_fields::Error) {
    assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(error.message(), "invalid declared setting or value");
}

#[test]
fn panel_items_have_typed_builders_defaults_and_refuse_bad_shapes() {
    use erp_registry::settings::{LinkTarget, PanelItemDecl, PanelItemKind, PanelItemValue};
    let info = PanelItemDecl::info("base.active_users", "Active Users").unwrap();
    assert_eq!(info.key(), "base.active_users");
    assert_eq!(info.label(), "Active Users");
    assert!(matches!(info.kind(), PanelItemKind::Info));
    assert_eq!(info.target_app(), "base");
    assert_eq!(info.block_title(), "General");
    assert_eq!(info.display_sequence(), 100);
    assert_eq!(info.scope_restriction(), None);
    assert!(info.shows_in(ScopeKind::Global));
    assert!(info.shows_in(ScopeKind::Company));
    assert!(info.read_groups().is_empty());

    let placed = info
        .clone()
        .app("stock")
        .unwrap()
        .block("Users")
        .unwrap()
        .sequence(10)
        .scope(ScopeKind::Company)
        .access(&["base.group_system"])
        .unwrap();
    assert_eq!(placed.target_app(), "stock");
    assert_eq!(placed.block_title(), "Users");
    assert_eq!(placed.display_sequence(), 10);
    assert_eq!(placed.scope_restriction(), Some(ScopeKind::Company));
    assert!(!placed.shows_in(ScopeKind::Global));
    assert!(placed.shows_in(ScopeKind::Company));
    assert_eq!(placed.read_groups(), ["base.group_system".to_owned()]);

    let link = PanelItemDecl::action_link(
        "base.company_update",
        "Update Info",
        "base.settings_companies_action",
        true,
    )
    .unwrap();
    match link.kind() {
        PanelItemKind::Link(LinkTarget::Action { xmlid, record, .. }) => {
            assert_eq!(xmlid, "base.settings_companies_action");
            assert!(*record);
        }
        other => panic!("unexpected kind {other:?}"),
    }
    let menu =
        PanelItemDecl::menu_link("base.users_menu", "Users", "base.settings_users_menu").unwrap();
    match menu.kind() {
        PanelItemKind::Link(target @ LinkTarget::Menu { .. }) => {
            assert_eq!(target.xmlid(), "base.settings_users_menu");
        }
        other => panic!("unexpected kind {other:?}"),
    }

    // Refused declaration shapes: key, label, target identity, placement and policy.
    let long_key = format!("base.{}", "k".repeat(124));
    for bad_key in [
        "",
        "unqualified",
        "Base.key",
        "base.Key",
        "base..key",
        "base.key\n",
        long_key.as_str(),
    ] {
        invalid_declaration_message(&PanelItemDecl::info(bad_key, "Label").unwrap_err());
    }
    let long_label = "l".repeat(257);
    for bad_label in ["", "Tab\tlabel", long_label.as_str()] {
        invalid_declaration_message(&PanelItemDecl::info("base.key", bad_label).unwrap_err());
    }
    let long_target = format!("base.{}", "a".repeat(252));
    for bad_target in [
        "",
        "unqualified",
        "Base.action",
        "base.",
        "base.bad-name",
        "base.a.b",
        long_target.as_str(),
    ] {
        invalid_declaration_message(
            &PanelItemDecl::action_link("base.key", "Label", bad_target, false).unwrap_err(),
        );
        invalid_declaration_message(
            &PanelItemDecl::menu_link("base.key", "Label", bad_target).unwrap_err(),
        );
    }
    // The longest accepted identity is 256 bytes; uppercase letters and digits are allowed after the module.
    let longest = format!("base.{}", "a".repeat(251));
    assert!(PanelItemDecl::menu_link("base.key", "Label", &longest).is_ok());
    assert!(PanelItemDecl::menu_link("base.key", "Label", "base.Menu_2").is_ok());
    let item = || PanelItemDecl::info("base.key", "Label").unwrap();
    invalid_declaration_message(&item().app("Stock").unwrap_err());
    invalid_declaration_message(&item().block("").unwrap_err());
    invalid_declaration_message(&item().block("Line\nbreak").unwrap_err());
    invalid_declaration_message(&item().access(&[]).unwrap_err());
    invalid_declaration_message(
        &item()
            .access(&["base.group_user", "base.group_user"])
            .unwrap_err(),
    );
    invalid_declaration_message(&item().access(&["group_user"]).unwrap_err());
    let groups: Vec<String> = (0..65).map(|i| format!("base.group_{i}")).collect();
    let groups: Vec<&str> = groups.iter().map(String::as_str).collect();
    invalid_declaration_message(&item().access(&groups).unwrap_err());
    assert!(item().access(&groups[..64]).is_ok());

    // Resolver values: scalars with optional label and detail, or a stored record.
    let count = PanelItemValue::info(Value::Int(2))
        .unwrap()
        .label("Active Users")
        .unwrap()
        .detail("first line\nsecond line")
        .unwrap();
    match &count {
        PanelItemValue::Info {
            value,
            label,
            detail,
            ..
        } => {
            assert_eq!(value, &Value::Int(2));
            assert_eq!(label.as_deref(), Some("Active Users"));
            assert_eq!(detail.as_deref(), Some("first line\nsecond line"));
        }
        other => panic!("unexpected value {other:?}"),
    }
    for scalar in [Value::Bool(true), Value::str("Name"), Value::Float(1.5)] {
        assert!(PanelItemValue::info(scalar).is_ok());
    }
    let record = PanelItemValue::record(erp_fields::RecId::from_real(7).unwrap()).unwrap();
    assert!(matches!(record, PanelItemValue::Record { id, .. } if id.get() == 7));

    let programming = |error: erp_fields::Error, message: &str| {
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(error.message(), message);
    };
    const SCALAR: &str = "A setting item value must be a boolean, text, integer or finite float.";
    const LABEL: &str = "A setting item label must be 1 to 256 bytes without control characters.";
    const DETAIL: &str = "A setting item detail must be at most 1024 bytes; a line feed is its only allowed control character.";
    for bad in [
        Value::False,
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::list([Value::Int(1)]),
    ] {
        programming(PanelItemValue::info(bad).unwrap_err(), SCALAR);
    }
    let base_value = || PanelItemValue::info(Value::Int(1)).unwrap();
    programming(base_value().label("").unwrap_err(), LABEL);
    programming(base_value().label("bad\nlabel").unwrap_err(), LABEL);
    programming(base_value().label(&"l".repeat(257)).unwrap_err(), LABEL);
    assert!(base_value().label(&"é".repeat(128)).is_ok());
    programming(base_value().detail("tab\there").unwrap_err(), DETAIL);
    programming(
        base_value().detail("carriage\r\nreturn").unwrap_err(),
        DETAIL,
    );
    programming(base_value().detail(&"d".repeat(1025)).unwrap_err(), DETAIL);
    // The message is built from the exported limit; the literal above is the independent oracle.
    assert!(DETAIL.contains(&erp_registry::settings::MAX_ITEM_DETAIL_BYTES.to_string()));
    assert!(
        base_value()
            .detail(&"d".repeat(erp_registry::settings::MAX_ITEM_DETAIL_BYTES))
            .is_ok()
    );
    assert!(base_value().detail(&"d".repeat(1024)).is_ok());
    assert!(base_value().detail("").is_ok());
    programming(
        record.clone().label("Label").unwrap_err(),
        "Only an information value carries a label or detail.",
    );
    programming(
        record.detail("Detail").unwrap_err(),
        "Only an information value carries a label or detail.",
    );
    programming(
        PanelItemValue::record(erp_fields::RecId::new_unique(1).unwrap()).unwrap_err(),
        "A setting item record must be a stored record.",
    );
}

#[test]
fn settings_registry_publishes_items_with_unique_keys_and_checked_references() {
    use erp_registry::settings::{LinkTarget, PanelItemDecl, SettingsRegistry};
    use std::collections::BTreeSet;
    let setting = || {
        SettingDecl::boolean("addon.flag", "Flag", ScopeKind::Global, false)
            .unwrap()
            .access(&["base.group_user"], &["base.group_system"])
            .unwrap()
    };
    let info = |key: &str| {
        PanelItemDecl::info(key, "Info")
            .unwrap()
            .access(&["base.group_system"])
            .unwrap()
    };
    let link = || {
        PanelItemDecl::action_link("addon.manage", "Manage", "base.some_action", false)
            .unwrap()
            .access(&["base.group_system"])
            .unwrap()
    };
    let installed = BTreeSet::from(["addon".to_owned(), "base".to_owned()]);
    let settings = || {
        SettingsRegistry::build(vec![("addon".into(), vec![setting()])], &installed, |_| {
            true
        })
        .unwrap()
    };
    // The callback receives the contributing addon with the target, so closure checks are per contributor.
    let all_targets =
        |owner: &str, target: &LinkTarget| owner == "addon" && target.xmlid() == "base.some_action";

    let registry = settings()
        .with_items(
            vec![(
                "addon".into(),
                vec![info("addon.zeta"), link(), info("addon.alpha")],
            )],
            &installed,
            |_| true,
            all_targets,
        )
        .unwrap();
    // Settings stay settings; items are their own sorted collection.
    assert_eq!(
        registry.iter().map(SettingDecl::key).collect::<Vec<_>>(),
        ["addon.flag"]
    );
    assert!(registry.get("addon.zeta").is_none());
    assert_eq!(
        registry.items().map(PanelItemDecl::key).collect::<Vec<_>>(),
        ["addon.alpha", "addon.manage", "addon.zeta"]
    );
    assert_eq!(registry.item("addon.manage").unwrap().label(), "Manage");
    assert!(registry.item("addon.flag").is_none());
    // A registry built without items has none.
    assert_eq!(settings().items().count(), 0);

    let refused = |contributions: Vec<(String, Vec<PanelItemDecl>)>,
                   installed: &BTreeSet<String>,
                   groups: bool,
                   targets: bool| {
        settings()
            .with_items(
                contributions,
                installed,
                |_| groups,
                |_: &str, _: &LinkTarget| targets,
            )
            .unwrap_err()
    };
    // Duplicate across settings and items, and within items.
    invalid_declaration_message(&refused(
        vec![("addon".into(), vec![info("addon.flag")])],
        &installed,
        true,
        true,
    ));
    invalid_declaration_message(&refused(
        vec![(
            "addon".into(),
            vec![info("addon.alpha"), info("addon.alpha")],
        )],
        &installed,
        true,
        true,
    ));
    // Key owned by another addon; contributing addon not installed.
    invalid_declaration_message(&refused(
        vec![("base".into(), vec![info("addon.alpha")])],
        &installed,
        true,
        true,
    ));
    invalid_declaration_message(&refused(
        vec![("addon".into(), vec![info("addon.alpha")])],
        &BTreeSet::from(["base".to_owned()]),
        true,
        true,
    ));
    // Empty read groups (never given `.access`) and a group that does not exist.
    invalid_declaration_message(&refused(
        vec![(
            "addon".into(),
            vec![PanelItemDecl::info("addon.open", "Open").unwrap()],
        )],
        &installed,
        true,
        true,
    ));
    invalid_declaration_message(&refused(
        vec![("addon".into(), vec![info("addon.alpha")])],
        &installed,
        false,
        true,
    ));
    // Target app not installed.
    invalid_declaration_message(&refused(
        vec![(
            "addon".into(),
            vec![info("addon.alpha").app("stock").unwrap()],
        )],
        &installed,
        true,
        true,
    ));
    // A link whose target does not exist has its own message; information rows need no target.
    let missing = refused(
        vec![("addon".into(), vec![link()])],
        &installed,
        true,
        false,
    );
    assert_eq!(missing.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        missing.message(),
        "Setting item references an unavailable action or menu."
    );
    assert!(
        settings()
            .with_items(
                vec![("addon".into(), vec![info("addon.alpha")])],
                &installed,
                |_| true,
                |_: &str, _: &LinkTarget| false
            )
            .is_ok()
    );
}
