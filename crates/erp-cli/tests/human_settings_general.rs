// SPDX-License-Identifier: MIT OR Apache-2.0
//! Base's General section and the information and link row mechanism through the real base
//! settings service on a real database, with a superuser, a Settings administrator and an internal user.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    reason = "test assertions in one persisted walk"
)]
use erp_addon_base::ADDON;
use erp_addon_base::ir_config_setting::{describe, read_values, resolve_link, write_values};
use erp_fields::{Cell, ErrorKind, RecId};
use erp_modules::{InitOptions, ModuleName};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID, Transaction};
use erp_sdk::settings::{
    LinkTarget, PanelItemDecl, PanelItemKind, PanelItemValue, ScopeKind, SettingsScope,
};
use erp_sdk::views::wire::{
    SettingValue, SettingsDescription, SettingsItem, SettingsLinkTarget, WireId,
};
use erp_sdk::{Env, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const BASE_KEYS: [&str; 8] = [
    "base.active_users",
    "base.companies",
    "base.company_info",
    "base.company_update",
    "base.languages",
    "base.manage_companies",
    "base.manage_languages",
    "base.manage_users",
];
const UNAVAILABLE: &str = "Setting item references an unavailable action or menu.";
const DENIED: &str = "Setting is unavailable or access is denied.";

// Test-only items declared for `base` by this binary: a probe whose resolver behaviour the walk switches,
// and an item restricted to the company scope.
static PROBE_MODE: AtomicU8 = AtomicU8::new(0);
const PROBE_REPORTS_CALLER: u8 = 0;
const PROBE_REFUSED_AFTER_FAILED_STATEMENT: u8 = 1;
const PROBE_FAILS: u8 = 2;
const PROBE_WRONG_KIND: u8 = 3;
const PROBE_RECORD_LINK_WRONG_KIND: u8 = 4;
fn probe_items() -> erp_sdk::Result<Vec<PanelItemDecl>> {
    Ok(vec![
        PanelItemDecl::info("base.test_probe", "Probe")?
            .block("Probe")?
            .access(&["base.group_system"])?,
        PanelItemDecl::action_link(
            "base.test_record_link",
            "Record link",
            "base.settings_companies_action",
            true,
        )?
        .block("Probe")?
        .access(&["base.group_system"])?,
        PanelItemDecl::info("base.test_company_only", "Company only")?
            .scope(ScopeKind::Company)
            .access(&["base.group_system"])?,
    ])
}
fn probe<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    _scope: &'a SettingsScope,
    key: &'a str,
) -> erp_sdk::BoxFuture<'a, erp_sdk::Result<PanelItemValue>> {
    Box::pin(async move {
        match (key, PROBE_MODE.load(Ordering::SeqCst)) {
            ("base.test_company_only", _) => PanelItemValue::info(Value::Bool(true)),
            ("base.test_record_link", PROBE_RECORD_LINK_WRONG_KIND) => {
                PanelItemValue::info(Value::Int(1))
            }
            ("base.test_record_link", mode) if mode != u8::MAX => {
                PanelItemValue::record(RecId::from_real(1).unwrap())
            }
            ("base.test_probe", PROBE_REPORTS_CALLER | PROBE_RECORD_LINK_WRONG_KIND) => {
                PanelItemValue::info(Value::str(env.uid().to_string()))?
                    .detail(&format!("su={}", env.su()))
            }
            ("base.test_probe", PROBE_REFUSED_AFTER_FAILED_STATEMENT) => {
                // A failed statement aborts the transaction unless the service isolated the call.
                let failed = tx.execute("SELECT 1/0", &[]).await.unwrap_err();
                assert_eq!(failed.sqlstate(), Some("22012"));
                Err(erp_sdk::Error::access("Probe refused."))
            }
            ("base.test_probe", PROBE_FAILS) => Err(erp_sdk::Error::validation("Probe failed.")),
            ("base.test_probe", PROBE_WRONG_KIND) => {
                PanelItemValue::record(RecId::from_real(1).unwrap())
            }
            (other, mode) => panic!("probe called for {other} in mode {mode}"),
        }
    })
}
erp_sdk::settings_panel_items!(probe_items, probe);

fn target_of(item: &PanelItemDecl) -> LinkTarget {
    match item.kind() {
        PanelItemKind::Link(target) => target.clone(),
        other => panic!("not a link: {other:?}"),
    }
}
fn action(xmlid: &str) -> LinkTarget {
    target_of(&PanelItemDecl::action_link("base.test_link", "Link", xmlid, false).unwrap())
}
fn menu(xmlid: &str) -> LinkTarget {
    target_of(&PanelItemDecl::menu_link("base.test_link", "Link", xmlid).unwrap())
}
fn item<'a>(description: &'a SettingsDescription, key: &str) -> Option<&'a SettingsItem> {
    description.items.iter().find(|item| match item {
        SettingsItem::Info { key: k, .. } | SettingsItem::Link { key: k, .. } => k == key,
        other => panic!("unknown item {other:?}"),
    })
}
fn keys(description: &SettingsDescription) -> Vec<&str> {
    description
        .items
        .iter()
        .map(|item| match item {
            SettingsItem::Info { key, .. } | SettingsItem::Link { key, .. } => key.as_str(),
            other => panic!("unknown item {other:?}"),
        })
        // This suite inventories the General overview and its private probes; jobs policy has its own suite.
        .filter(|key| *key != "base.jobs_configuration_status")
        .collect()
}
/// `(label, value, detail, block, sequence)` of an information row.
fn info(
    description: &SettingsDescription,
    key: &str,
) -> (String, SettingValue, Option<String>, String, u32) {
    match item(description, key) {
        Some(SettingsItem::Info {
            label,
            value,
            detail,
            block,
            sequence,
            app,
            ..
        }) => {
            assert_eq!(app, "base");
            (
                label.clone(),
                value.clone(),
                detail.clone(),
                block.clone(),
                *sequence,
            )
        }
        other => panic!("{key} is not an information row: {other:?}"),
    }
}
/// `(label, target, block)` of a link row.
fn link(description: &SettingsDescription, key: &str) -> (String, SettingsLinkTarget, String) {
    match item(description, key) {
        Some(SettingsItem::Link {
            label,
            target,
            block,
            app,
            ..
        }) => {
            assert_eq!(app, "base");
            (label.clone(), target.clone(), block.clone())
        }
        other => panic!("{key} is not a link row: {other:?}"),
    }
}
fn integer(value: &SettingValue) -> i64 {
    match value {
        SettingValue::Integer(value) => *value,
        other => panic!("not an integer: {other:?}"),
    }
}
fn text(value: &SettingValue) -> String {
    match value {
        SettingValue::String(value) => value.clone(),
        other => panic!("not text: {other:?}"),
    }
}
fn wire(id: i64) -> WireId {
    WireId::try_from(u64::try_from(id).unwrap()).unwrap()
}
fn target_ids(target: &SettingsLinkTarget) -> (i64, Option<i64>) {
    match target {
        SettingsLinkTarget::Action { id, res_id } => (
            i64::try_from(u64::from(*id)).unwrap(),
            res_id.map(|id| i64::try_from(u64::from(id)).unwrap()),
        ),
        other => panic!("not an action target: {other:?}"),
    }
}
async fn count(tx: &mut Transaction, sql: &str) -> i64 {
    tx.query(sql, &[]).await.unwrap()[0].get(0)
}
async fn xmlid(tx: &mut Transaction, name: &str, model: &str) -> i64 {
    let rows = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name=$1 AND model=$2",
            &[&name, &model],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "{name}");
    rows[0].get(0)
}
/// The company detail text, built here from an independent SQL read.
async fn expected_company(tx: &mut Transaction, company: i64) -> (String, Option<String>) {
    let row = &tx
        .query(
            "SELECT c.name, p.street, p.street2, p.zip, p.city, p.vat FROM res_company c \
             JOIN res_partner p ON p.id = c.partner_id WHERE c.id = $1",
            &[&company],
        )
        .await
        .unwrap()[0];
    let part = |index: usize| {
        row.get::<_, Option<String>>(index)
            .filter(|value| !value.is_empty())
    };
    let mut lines: Vec<String> = Vec::new();
    lines.extend(part(1));
    lines.extend(part(2));
    match (part(3), part(4)) {
        (Some(zip), Some(city)) => lines.push(format!("{zip} - {city}")),
        (Some(one), None) | (None, Some(one)) => lines.push(one),
        (None, None) => {}
    }
    if let Some(vat) = part(5) {
        lines.push(format!("VAT: {vat}"));
    }
    let detail = (!lines.is_empty()).then(|| lines.join("\n"));
    (row.get(0), detail)
}

#[test]
fn general_section_items_follow_caller_rights_scope_and_resolver_rules() {
    // Same native install/RPC stack requirement as human_settings.rs.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(general_section());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn general_section() {
    let db = erp_testkit::TestDb::create().await.unwrap();
    let mut index = erp_modules::AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    // Explicit provenance for this binary's private native fixture bytes.
    let native_artifact_identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                std::path::Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = erp_modules::system::ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(native_artifact_identities)
        .code([&ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let initialized = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    // `uom` depends on base, so its action is outside base's dependency closure.
    let installed = system
        .button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("uom").unwrap()],
        )
        .await
        .unwrap();
    let orm = &installed.orm;
    const INTERNAL_USERS: &str = "SELECT count(*)::int8 FROM res_users WHERE active AND NOT share";
    let mut labels_seen = Vec::new();

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let users_action = xmlid(&mut tx, "settings_users_action", "ir.actions.act_window").await;
    let languages_action = xmlid(
        &mut tx,
        "settings_languages_action",
        "ir.actions.act_window",
    )
    .await;
    let companies_action = xmlid(
        &mut tx,
        "settings_companies_action",
        "ir.actions.act_window",
    )
    .await;
    let users_menu = xmlid(&mut tx, "settings_users_menu", "ir.ui.menu").await;
    assert_eq!(
        resolve_link(&env, &mut tx, "base", &action("base.settings_users_action"))
            .await
            .unwrap(),
        users_action
    );
    assert_eq!(
        resolve_link(&env, &mut tx, "base", &menu("base.settings_users_menu"))
            .await
            .unwrap(),
        users_menu
    );
    let uom_action: i64 = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='uom' AND name='uom_uom_action_list' \
             AND model='ir.actions.act_window'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        resolve_link(&env, &mut tx, "uom", &action("uom.uom_uom_action_list"))
            .await
            .unwrap(),
        uom_action
    );
    for (owner, target) in [
        ("base", menu("base.settings_users_action")),
        ("base", action("base.settings_users_menu")),
        ("base", action("base.no_such_action")),
        ("base", menu("base.no_such_menu")),
        ("base", action("uom.uom_uom_action_list")),
    ] {
        let error = resolve_link(&env, &mut tx, owner, &target)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{target:?}");
        assert_eq!(error.message(), UNAVAILABLE, "{target:?}");
    }

    // A superuser has no current company: the company rows are left out in the global scope
    // while every other row is described, counts singular or plural by the database's own numbers.
    assert!(tx.companies(&env).await.unwrap().is_empty());
    let initial = describe(&env, &mut tx, SettingsScope::Global {})
        .await
        .unwrap();
    let mut expected_keys: Vec<&str> = BASE_KEYS
        .iter()
        .copied()
        .filter(|key| !matches!(*key, "base.company_info" | "base.company_update"))
        .chain(["base.test_probe", "base.test_record_link"])
        .collect();
    expected_keys.sort_unstable();
    assert_eq!(keys(&initial), expected_keys);
    // A fresh database: no active internal user (only the inactive `__system__`), one language, one company.
    for (key, sql, singular, plural, fresh) in [
        (
            "base.active_users",
            INTERNAL_USERS,
            "Active User",
            "Active Users",
            0,
        ),
        (
            "base.languages",
            "SELECT count(*)::int8 FROM res_lang WHERE active",
            "Language",
            "Languages",
            1,
        ),
        (
            "base.companies",
            "SELECT count(*)::int8 FROM res_company WHERE active",
            "Company",
            "Companies",
            1,
        ),
    ] {
        let (label, value, detail, _, _) = info(&initial, key);
        let number = count(&mut tx, sql).await;
        assert_eq!(number, fresh, "{key}");
        assert_eq!(integer(&value), number, "{key}");
        assert_eq!(label, if number <= 1 { singular } else { plural }, "{key}");
        assert_eq!(detail, None);
        labels_seen.push(label);
    }
    let probe_su = info(&initial, "base.test_probe");
    assert_eq!(text(&probe_su.1), env.uid().to_string());
    assert_eq!(probe_su.2.as_deref(), Some("su=true"));
    // Items are never declarations.
    assert!(
        initial
            .declarations
            .iter()
            .all(|declaration| !keys(&initial).contains(&declaration.key.as_str()))
    );

    // Data for the second half: a second, addressed company, a second language, a Settings administrator
    // in both companies and an internal user.
    let main_company: i64 = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='main_company'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let currency = env
        .records("res.company")
        .unwrap()
        .browse([main_company])
        .unwrap()
        .cell(&mut tx, "currency_id")
        .await
        .unwrap();
    let second_company = env
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Second Settings Company".into()))
                    .set_cell("currency_id", currency)
                    .set_cell("street", Cell::Str("12 Harbour Road".into()))
                    .set_cell("street2", Cell::Str("Unit 4".into()))
                    .set_cell("zip", Cell::Str("40115".into()))
                    .set_cell("city", Cell::Str("Bandung".into()))
                    .set_cell("vat", Cell::Str("ID0123".into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let inactive_language: i64 = tx
        .query(
            "SELECT id::int8 FROM res_lang WHERE NOT active ORDER BY code LIMIT 1",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    env.records("res.lang")
        .unwrap()
        .browse([inactive_language])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
        .await
        .unwrap();
    let system_group = xmlid(&mut tx, "group_system", "res.groups").await;
    let user_group = xmlid(&mut tx, "group_user", "res.groups").await;
    let mut user = async |login: &str, group: i64, companies: Vec<i64>| -> i64 {
        tx.env()
            .records("res.users")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(login.into()))
                        .set_cell("login", Cell::Str(login.into()))
                        .set_cell("company_id", Cell::Id(main_company))
                        .set_cell("lang", Cell::Str("en_US".into()))
                        .set_cell("tz", Cell::Str("UTC".into()))
                        .set_commands(
                            "company_ids",
                            [Command::set(
                                companies.iter().map(|id| RecId::from_real(*id).unwrap()),
                            )],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::set([RecId::from_real(group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap()
            .ids()[0]
    };
    let admin_uid = user(
        "general-settings-admin",
        system_group,
        vec![main_company, second_company],
    )
    .await;
    let reader_uid = user("general-settings-reader", user_group, vec![main_company]).await;
    tx.commit().await.unwrap();
    drop(tx);

    // Zero languages, zero companies and one active internal user, in a transaction that is rolled back:
    // every label is singular.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    tx.execute("UPDATE res_lang SET active = false", &[])
        .await
        .unwrap();
    tx.execute("UPDATE res_company SET active = false", &[])
        .await
        .unwrap();
    // Exactly one active internal user: the administrator.
    tx.execute(
        "UPDATE res_users SET active = false WHERE NOT share AND id <> $1",
        &[&admin_uid],
    )
    .await
    .unwrap();
    let empty = describe(&env, &mut tx, SettingsScope::Global {})
        .await
        .unwrap();
    let (label, value, ..) = info(&empty, "base.active_users");
    assert_eq!(count(&mut tx, INTERNAL_USERS).await, 1);
    assert_eq!(integer(&value), 1);
    assert_eq!(label, "Active User");
    for (key, sql, singular) in [
        (
            "base.languages",
            "SELECT count(*)::int8 FROM res_lang WHERE active",
            "Language",
        ),
        (
            "base.companies",
            "SELECT count(*)::int8 FROM res_company WHERE active",
            "Company",
        ),
    ] {
        let (label, value, ..) = info(&empty, key);
        assert_eq!(count(&mut tx, sql).await, 0, "{key}");
        assert_eq!(integer(&value), 0, "{key}");
        assert_eq!(label, singular, "{key}");
    }
    tx.rollback().await.unwrap();
    drop(tx);

    // A policy-enforcing ORM so the administrator and the reader act with their own rights.
    let policy =
        erp_security::source::PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
            .await
            .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(policy);
    let users_orm = Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .resolver(erp_security::resolver::Resolver::for_registry(orm.registry()).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap();

    // The administrator in the global scope: every row, counts equal to independent SQL counts, plural labels.
    let mut tx = users_orm
        .begin(erp_orm::UserId::new(admin_uid).unwrap())
        .await
        .unwrap();
    let admin = tx.env();
    let global = describe(&admin, &mut tx, SettingsScope::Global {})
        .await
        .unwrap();
    let mut expected_keys: Vec<&str> = BASE_KEYS
        .iter()
        .copied()
        .chain(["base.test_probe", "base.test_record_link"])
        .collect();
    expected_keys.sort_unstable();
    assert_eq!(keys(&global), expected_keys);
    // A record link carries the record its resolver named.
    assert_eq!(
        target_ids(&link(&global, "base.test_record_link").1),
        (companies_action, Some(1))
    );
    for (key, sql, singular, plural, block, sequence) in [
        (
            "base.active_users",
            INTERNAL_USERS,
            "Active User",
            "Active Users",
            "Users",
            10,
        ),
        (
            "base.languages",
            "SELECT count(*)::int8 FROM res_lang WHERE active",
            "Language",
            "Languages",
            "Languages",
            30,
        ),
        (
            "base.companies",
            "SELECT count(*)::int8 FROM res_company WHERE active",
            "Company",
            "Companies",
            "Companies",
            70,
        ),
    ] {
        let (label, value, detail, item_block, item_sequence) = info(&global, key);
        let number = count(&mut tx, sql).await;
        assert!(number > 1, "{key}: the second half needs a plural count");
        assert_eq!(integer(&value), number, "{key}");
        assert_eq!(label, if number <= 1 { singular } else { plural }, "{key}");
        assert_eq!(detail, None);
        assert_eq!((item_block.as_str(), item_sequence), (block, sequence));
        labels_seen.push(label);
    }
    // Both label forms were produced for each count across the two halves.
    for (singular, plural) in [
        ("Active User", "Active Users"),
        ("Language", "Languages"),
        ("Company", "Companies"),
    ] {
        assert!(
            labels_seen.iter().any(|label| label == singular)
                && labels_seen.iter().any(|label| label == plural),
            "{singular}/{plural} seen: {labels_seen:?}"
        );
    }
    for (key, label, block, action_id) in [
        ("base.manage_users", "Manage Users", "Users", users_action),
        (
            "base.manage_languages",
            "Manage Languages",
            "Languages",
            languages_action,
        ),
        (
            "base.manage_companies",
            "Manage Companies",
            "Companies",
            companies_action,
        ),
    ] {
        let (item_label, target, item_block) = link(&global, key);
        assert_eq!(item_label, label);
        assert_eq!(item_block, block);
        assert_eq!(target_ids(&target), (action_id, None), "{key}");
    }
    // The current company in the global scope, its address text.
    let current = tx.companies(&admin).await.unwrap()[0].get();
    assert_eq!(current, main_company);
    let (name, detail) = expected_company(&mut tx, main_company).await;
    let (label, value, item_detail, block, sequence) = info(&global, "base.company_info");
    assert_eq!(
        (label.as_str(), block.as_str(), sequence),
        ("Company", "Companies", 50)
    );
    assert_eq!(text(&value), name);
    assert_eq!(item_detail, detail);
    let (label, target, _) = link(&global, "base.company_update");
    assert_eq!(label, "Update Info");
    assert_eq!(target_ids(&target), (companies_action, Some(main_company)));
    let probe_admin = info(&global, "base.test_probe");
    assert_eq!(text(&probe_admin.1), admin_uid.to_string());
    assert_eq!(probe_admin.2.as_deref(), Some("su=false"));
    // The wire id helper agrees with the ids the service put on the wire.
    assert!(matches!(
        link(&global, "base.manage_users").1,
        SettingsLinkTarget::Action { id, res_id: None } if id == wire(users_action)
    ));

    // The company scope of the second company: its name, its address, its record, and the company-only row.
    let second_env = admin.with_company(second_company).unwrap();
    let company_scope = SettingsScope::Company {
        company_id: wire(second_company),
    };
    let scoped = describe(&second_env, &mut tx, company_scope.clone())
        .await
        .unwrap();
    let mut expected_keys: Vec<&str> = BASE_KEYS
        .iter()
        .copied()
        .chain([
            "base.test_company_only",
            "base.test_probe",
            "base.test_record_link",
        ])
        .collect();
    expected_keys.sort_unstable();
    assert_eq!(keys(&scoped), expected_keys);
    let (name, detail) = expected_company(&mut tx, second_company).await;
    assert_eq!(name, "Second Settings Company");
    assert_eq!(
        detail.as_deref(),
        Some("12 Harbour Road\nUnit 4\n40115 - Bandung\nVAT: ID0123")
    );
    let (_, value, item_detail, _, _) = info(&scoped, "base.company_info");
    assert_eq!(text(&value), name);
    assert_eq!(item_detail, detail);
    assert_eq!(
        target_ids(&link(&scoped, "base.company_update").1),
        (companies_action, Some(second_company))
    );

    // Item keys are refused by the values RPCs exactly like an unknown key.
    for item_key in ["base.active_users", "base.manage_users"] {
        let unknown = read_values(
            &admin,
            &mut tx,
            SettingsScope::Global {},
            vec!["base.no_such_setting".into()],
        )
        .await
        .unwrap_err();
        let refused = read_values(
            &admin,
            &mut tx,
            SettingsScope::Global {},
            vec![item_key.into()],
        )
        .await
        .unwrap_err();
        assert_eq!(
            (refused.kind(), refused.message()),
            (unknown.kind(), unknown.message())
        );
        assert_eq!(
            (refused.kind(), refused.message()),
            (ErrorKind::Access, DENIED)
        );
        let unknown = write_values(
            &admin,
            &mut tx,
            SettingsScope::Global {},
            "0".into(),
            vec![("base.no_such_setting".into(), Value::Int(1))],
        )
        .await
        .unwrap_err();
        let refused = write_values(
            &admin,
            &mut tx,
            SettingsScope::Global {},
            "0".into(),
            vec![(item_key.into(), Value::Int(1))],
        )
        .await
        .unwrap_err();
        assert_eq!(
            (refused.kind(), refused.message()),
            (unknown.kind(), unknown.message())
        );
        assert_eq!(
            (refused.kind(), refused.message()),
            (ErrorKind::Access, DENIED)
        );
    }

    // An access error leaves only that item out, even after the resolver's own failed statement.
    PROBE_MODE.store(PROBE_REFUSED_AFTER_FAILED_STATEMENT, Ordering::SeqCst);
    let refused = describe(&admin, &mut tx, SettingsScope::Global {})
        .await
        .unwrap();
    let mut expected_keys = BASE_KEYS.to_vec();
    expected_keys.push("base.test_record_link");
    expected_keys.sort_unstable();
    assert_eq!(keys(&refused), expected_keys);
    assert_eq!(
        integer(&info(&refused, "base.languages").1),
        integer(&info(&global, "base.languages").1)
    );
    // Any other resolver error fails the description with that error.
    PROBE_MODE.store(PROBE_FAILS, Ordering::SeqCst);
    let failed = describe(&admin, &mut tx, SettingsScope::Global {})
        .await
        .unwrap_err();
    assert_eq!(failed.kind(), ErrorKind::Validation);
    assert_eq!(failed.message(), "Probe failed.");
    // A value of the wrong kind for its item is a programming error.
    PROBE_MODE.store(PROBE_WRONG_KIND, Ordering::SeqCst);
    let wrong = describe(&admin, &mut tx, SettingsScope::Global {})
        .await
        .unwrap_err();
    assert_eq!(wrong.kind(), ErrorKind::Programming);
    assert_eq!(
        wrong.message(),
        "Setting item base.test_probe resolved to a value of the wrong kind."
    );
    // The same for a record link whose resolver returns an information value.
    PROBE_MODE.store(PROBE_RECORD_LINK_WRONG_KIND, Ordering::SeqCst);
    let wrong = describe(&admin, &mut tx, SettingsScope::Global {})
        .await
        .unwrap_err();
    assert_eq!(wrong.kind(), ErrorKind::Programming);
    assert_eq!(
        wrong.message(),
        "Setting item base.test_record_link resolved to a value of the wrong kind."
    );
    PROBE_MODE.store(PROBE_REPORTS_CALLER, Ordering::SeqCst);
    tx.rollback().await.unwrap();
    drop(tx);

    // probe would panic in an unknown mode).
    PROBE_MODE.store(u8::MAX, Ordering::SeqCst);
    let mut tx = users_orm
        .begin(erp_orm::UserId::new(reader_uid).unwrap())
        .await
        .unwrap();
    let reader = tx.env();
    let plain = describe(&reader, &mut tx, SettingsScope::Global {})
        .await
        .unwrap();
    assert!(plain.items.is_empty(), "{:?}", keys(&plain));
    tx.rollback().await.unwrap();
    drop(tx);
    PROBE_MODE.store(PROBE_REPORTS_CALLER, Ordering::SeqCst);

    // Control characters in company data are shown as one space per run, then trimmed.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE res_partner SET street = E'12 Road\\r\\nUnit 4', street2 = E'\\tBlock\\t\\tB ' \
         WHERE id = (SELECT partner_id FROM res_company WHERE id = $1)",
        &[&second_company],
    )
    .await
    .unwrap();
    let stored: (String, String) = {
        let row = &tx
            .query(
                "SELECT p.street, p.street2 FROM res_partner p JOIN res_company c \
                 ON c.partner_id = p.id WHERE c.id = $1",
                &[&second_company],
            )
            .await
            .unwrap()[0];
        (row.get(0), row.get(1))
    };
    assert_eq!(
        stored,
        ("12 Road\r\nUnit 4".to_owned(), "\tBlock\t\tB ".to_owned())
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = users_orm
        .begin(erp_orm::UserId::new(admin_uid).unwrap())
        .await
        .unwrap();
    let second_env = tx.env().with_company(second_company).unwrap();
    let pasted = describe(&second_env, &mut tx, company_scope.clone())
        .await
        .unwrap();
    assert_eq!(
        info(&pasted, "base.company_info").2.as_deref(),
        Some("12 Road Unit 4\nBlock B\n40115 - Bandung\nVAT: ID0123")
    );
    // A company context the caller may not use fails the request instead of leaving rows out. This refusal
    // comes from the scope check (`authorize_scope` -> `check_access`, whose model check reads
    // `env.companies()` first); the extra `tx.companies` call in `describe` is not what this proves.
    let foreign = tx.env().with_company(9_007_199_254_740_991).unwrap();
    let refused = describe(&foreign, &mut tx, SettingsScope::Global {})
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Access);
    assert_eq!(
        refused.message(),
        "Access to unauthorized or invalid companies."
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // A company name of only spaces and control characters is refused with the company's id; the name is
    // restored afterwards.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let saved_name: String = tx
        .query(
            "SELECT name FROM res_company WHERE id = $1",
            &[&second_company],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.execute(
        "UPDATE res_company SET name = E' \\t ' WHERE id = $1",
        &[&second_company],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = users_orm
        .begin(erp_orm::UserId::new(admin_uid).unwrap())
        .await
        .unwrap();
    let second_env = tx.env().with_company(second_company).unwrap();
    let nameless = describe(&second_env, &mut tx, company_scope.clone())
        .await
        .unwrap_err();
    assert_eq!(nameless.kind(), ErrorKind::Validation);
    assert_eq!(
        nameless.message(),
        format!("Company {second_company} has no name to show.")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE res_company SET name = $2 WHERE id = $1",
        &[&second_company, &saved_name],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // An address text over 1024 bytes fails the description loudly instead of being cut.
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE res_partner SET street = repeat('x', 1100) \
         WHERE id = (SELECT partner_id FROM res_company WHERE id = $1)",
        &[&second_company],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = users_orm
        .begin(erp_orm::UserId::new(admin_uid).unwrap())
        .await
        .unwrap();
    let second_env = tx.env().with_company(second_company).unwrap();
    let long = describe(&second_env, &mut tx, company_scope)
        .await
        .unwrap_err();
    assert_eq!(long.kind(), ErrorKind::Validation);
    assert_eq!(
        long.message(),
        format!(
            "The address of company {second_company} is longer than the 1024 bytes the Settings \
             page shows."
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);

    drop(users_orm);
    drop(installed);
    drop(initialized);
    drop(system);
    db.cleanup().await.unwrap();
}
