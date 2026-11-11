//! Technical-menu eligibility against real installed policy, roles and the metadata RPC.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use erp_addon_base::ir_ui_menu::IrUiMenu;
use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID, Transaction};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_sdk::prelude::{RecordSet, methods};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

// Test-only RPC instrumentation proves the trusted request debug flag reaches the method.
#[erp_sdk::extend(IrUiMenu, addon = "web")]
struct H18MenuDebugProbe {}
#[methods(IrUiMenu, addon = "web")]
impl H18MenuDebugProbe {
    #[method]
    #[rpc(effect = read, model)]
    async fn h18_test_principal_debug(
        this: &RecordSet<IrUiMenu>,
        _tx: &mut Transaction,
    ) -> erp_sdk::Result<bool> {
        Ok(this.as_records().env().principal().debug())
    }
}

use std::{path::Path, sync::Arc};

struct Host;
impl RpcHost for Host {
    fn language_active(&self, code: &str) -> erp_fields::Result<bool> {
        Ok(code == "en_US")
    }
    fn has_capability(
        &self,
        _: &erp_orm::Principal,
        _: &erp_orm::Capability,
    ) -> erp_fields::Result<bool> {
        Ok(false)
    }
}
async fn xid(tx: &mut Transaction, name: &str, model: &str) -> i64 {
    tx.query(
        "SELECT res_id FROM ir_model_data WHERE module='base' AND name=$1 AND model=$2",
        &[&name, &model],
    )
    .await
    .unwrap()[0]
        .get(0)
}
async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    tx.env()
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}
fn groups(ids: &[i64]) -> Command {
    Command::set(ids.iter().map(|id| RecId::from_real(*id).unwrap()))
}
async fn user(tx: &mut Transaction, name: &str, company: i64, ids: &[i64]) -> i64 {
    let partner = create(
        tx,
        "res.partner",
        RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("company_id", Cell::Id(company)),
    )
    .await;
    create(
        tx,
        "res.users",
        RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("login", Cell::Str(name.into()))
            .set_cell("partner_id", Cell::Id(partner))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("lang", Cell::Str("en_US".into()))
            .set_cell("tz", Cell::Str("UTC".into()))
            .set_commands("company_ids", [groups(&[company])])
            .set_commands("group_ids", [groups(ids)]),
    )
    .await
}
async fn action(tx: &mut Transaction, name: &str, model: &str, group: Option<i64>) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("res_model", Cell::Str(model.into()))
        .set_cell("view_mode", Cell::Str("form".into()));
    if let Some(group) = group {
        vals = vals.set_commands("group_ids", [groups(&[group])]);
    }
    create(tx, "ir.actions.act_window", vals).await
}
async fn menu(tx: &mut Transaction, name: &str, action: i64, technical: i64) -> i64 {
    create(
        tx,
        "ir.ui.menu",
        RawVals::new()
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("active", Cell::Bool(true))
            .set_cell(
                "action",
                Cell::Str(format!("ir.actions.act_window,{action}").into()),
            )
            .set_commands("group_ids", [groups(&[technical])]),
    )
    .await
}

/// the uncached implementation has no menu-cache identity to exercise)
#[test]
fn technical_menu_requires_debug_and_regular_or_system_role() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(run());
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn run() {
    let db = TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    let identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    let loaded = system.initialize(InitOptions::new()).await.unwrap();
    assert!(loaded.post_commit.is_clean());
    let mut tx = loaded.orm.begin(SUPERUSER_ID).await.unwrap();
    let internal = xid(&mut tx, "group_user", "res.groups").await;
    let regular = xid(&mut tx, "group_user_regular", "res.groups").await;
    let system_group = xid(&mut tx, "group_system", "res.groups").await;
    let company = xid(&mut tx, "main_company", "res.company").await;
    // Create an administrator first, preserving the base last-administrator invariant.
    let bootstrap_admin = user(
        &mut tx,
        "h18-menu-bootstrap",
        company,
        &[internal, system_group],
    )
    .await;
    // Missing from bootstrap defaults: test-owned XMLID must exist before policy compilation.
    let technical = create(
        &mut tx,
        "res.groups",
        RawVals::new().set_cell("name", Cell::Str("H18 technical fixture".into())),
    )
    .await;
    create(
        &mut tx,
        "ir.model.data",
        RawVals::new()
            .set_cell("module", Cell::Str("base".into()))
            .set_cell("name", Cell::Str("group_no_one".into()))
            .set_cell("model", Cell::Str("res.groups".into()))
            .set_cell("res_id", Cell::Id(technical))
            .set_cell("noupdate", Cell::Bool(true)),
    )
    .await;
    // It was created before its light-group XMLID existed; remove the automatic regular
    // implication only after naming it. The normal group override now recognizes it as light.
    tx.env()
        .records("res.groups")
        .unwrap()
        .browse([technical])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_commands("implied_ids", [Command::set(Vec::<RecId>::new())]),
        )
        .await
        .unwrap();
    let system_subject = user(
        &mut tx,
        "h18-menu-system",
        company,
        &[internal, system_group, technical],
    )
    .await;
    tx.env()
        .records("res.users")
        .unwrap()
        .browse([system_subject])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_commands("group_ids", [groups(&[internal, system_group, technical])]),
        )
        .await
        .unwrap();
    let subjects = [
        (
            user(
                &mut tx,
                "h18-menu-regular",
                company,
                &[internal, regular, technical],
            )
            .await,
            "regular_user",
        ),
        (
            user(&mut tx, "h18-menu-light", company, &[internal, technical]).await,
            "light_user",
        ),
        (system_subject, "group_system"),
    ];
    assert!(
        subjects
            .iter()
            .all(|(uid, _)| *uid != SUPERUSER_ID.get() && *uid != bootstrap_admin),
        "every tested subject must be a real User principal, never reserved uid1 or the bootstrap account"
    );
    for (uid, role) in subjects {
        let rows = tx
            .env()
            .records("res.users")
            .unwrap()
            .browse([uid])
            .unwrap()
            .read_with(&mut tx, &["role"], erp_orm::Load::Raw)
            .await
            .unwrap();
        assert_eq!(
            rows[0].get("role"),
            Some(&erp_fields::Value::str(role)),
            "fixture must have actual intended role"
        );
    }
    let denied_group = create(
        &mut tx,
        "res.groups",
        RawVals::new().set_cell("name", Cell::Str("H18 action excluded group".into())),
    )
    .await;
    let readable = action(&mut tx, "H18 readable", "res.partner", None).await;
    let denied_action = action(
        &mut tx,
        "H18 action denied",
        "res.partner",
        Some(denied_group),
    )
    .await;
    let denied_model = action(&mut tx, "H18 model denied", "res.groups", None).await;
    let readable_menu = menu(&mut tx, "H18 readable technical", readable, technical).await;
    let denied_action_menu = menu(
        &mut tx,
        "H18 denied action technical",
        denied_action,
        technical,
    )
    .await;
    let denied_model_menu = menu(
        &mut tx,
        "H18 denied model technical",
        denied_model,
        technical,
    )
    .await;
    tx.commit().await.unwrap();
    let source = PolicySource::build(Arc::clone(loaded.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let table = loaded
        .orm
        .env(SUPERUSER_ID, Context::new())
        .generation_attachment::<erp_sdk::views::PublishedViewTypes>()
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(loaded.orm.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(loaded.orm.registry()).unwrap())
        .policy(provider)
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(loaded.orm.registry()),
            table,
        ))
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let mut producer_tx = orm
        .begin(erp_orm::UserId::new(system_subject).unwrap())
        .await
        .unwrap();
    let producer_env = producer_tx.env();
    producer_env
        .records("res.partner")
        .unwrap()
        .check_access(&mut producer_tx, erp_orm::Operation::Read)
        .await
        .unwrap();
    assert_producer_admission(&mut producer_tx).await;
    producer_tx.rollback().await.unwrap();
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    for (uid, role) in subjects {
        let mut tx = orm.begin(erp_orm::UserId::new(uid).unwrap()).await.unwrap();
        let caller_env = tx.env();
        if role != "group_system" {
            let error = tx
                .env()
                .records("res.groups")
                .unwrap()
                .check_access(&mut tx, erp_orm::Operation::Read)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
            assert_eq!(
                error.message(),
                "You are not allowed to access 'Access Groups' (res.groups) records.\n\nThis operation is allowed for the following groups:\n\t- Settings Administration\n\nContact your administrator to request access if necessary."
            );
        }
        tx.rollback().await.unwrap();
        for debug in [false, true, false] {
            let principal = caller_env.with_debug(debug).principal().clone();
            assert_eq!(principal.debug(), debug);
            let probe = service
                .call(RpcCall::json2(
                    "/json/2/ir.ui.menu/h18_test_principal_debug",
                    br#"{}"#.to_vec(),
                    Caller::Principal(principal.clone()),
                ))
                .await;
            assert_eq!(
                probe.status(),
                200,
                "{}",
                String::from_utf8_lossy(probe.body())
            );
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(probe.body()).unwrap(),
                serde_json::json!(debug),
                "the real RPC method must observe the intended trusted debug flag"
            );
            let response = service
                .call(RpcCall::json2(
                    "/json/2/ir.ui.menu/erp_load_web_menus",
                    br#"{}"#.to_vec(),
                    Caller::Principal(principal.clone()),
                ))
                .await;
            assert_eq!(
                response.status(),
                200,
                "{}",
                String::from_utf8_lossy(response.body())
            );
            let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
            let ids: Vec<_> = body["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|node| node["id"].as_i64().unwrap())
                .collect();
            let mut direct = orm.begin(erp_orm::UserId::new(uid).unwrap()).await.unwrap();
            let env = direct.env().with_debug(debug);
            let base_visible =
                erp_addon_base::ir_ui_menu::IrUiMenu::visible_menu_ids(&env, &mut direct, debug)
                    .await
                    .unwrap();
            assert_eq!(
                base_visible.contains(&readable_menu),
                debug && role != "light_user",
                "base role={role} debug={debug}"
            );
            assert!(
                !base_visible.contains(&denied_action_menu),
                "base debug cannot grant action groups"
            );
            if role != "group_system" {
                assert!(
                    !base_visible.contains(&denied_model_menu),
                    "base debug cannot grant model rights"
                );
            }
            direct.rollback().await.unwrap();
            assert_eq!(
                ids.contains(&readable_menu),
                debug && role != "light_user",
                "role={role} debug={debug}"
            );
            assert!(
                !ids.contains(&denied_action_menu),
                "debug cannot grant action groups: role={role} debug={debug}"
            );
            if role != "group_system" {
                assert!(
                    !ids.contains(&denied_model_menu),
                    "debug cannot grant model Read: role={role} debug={debug}"
                );
            }
        }
    }
    drop(service);
    drop(orm);
    drop(loaded);
    drop(system);
    db.cleanup().await.unwrap();
}

#[allow(clippy::print_stdout, reason = "tool-recorded admission evidence")]
async fn assert_producer_admission(tx: &mut Transaction) {
    let action = action(tx, &"A".repeat(4096), "res.partner", None).await;
    create(
        tx,
        "ir.ui.menu",
        RawVals::new()
            .set_cell("name", Cell::Str("M".repeat(4096).into()))
            .set_cell("active", Cell::Bool(true))
            .set_cell(
                "web_icon",
                Cell::Str(format!("lucide:{}", "i".repeat(512)).into()),
            )
            .set_cell(
                "action",
                Cell::Str(format!("ir.actions.act_window,{action}").into()),
            ),
    )
    .await;
    let env = tx.env();
    // These call the actual private orchestration, without final transport as an accidental gate.
    let (menus, menu_stages) =
        erp_addon_web::metadata_test_support::test_load_menus_probed(&env, tx, 4096)
            .await
            .unwrap();
    let (action_result, action_stages) =
        erp_addon_web::metadata_test_support::test_load_action_probed(&env, tx, action, 4096)
            .await
            .unwrap();
    println!("menu executed checkpoints: {menu_stages:?}");
    println!("action executed checkpoints: {action_stages:?}");
    assert!(
        !menu_stages.contains(&"menus:text:read"),
        "source text read must not start after admission refusal"
    );
    assert!(
        !action_stages.contains(&"action:text:read"),
        "action metadata read must not start after admission refusal"
    );
    println!(
        "producer lowered allowances: menus_refused={} action_refused={}",
        menus.is_err(),
        action_result.is_err()
    );
    if let Err(error) = &menus {
        println!(
            "menu probe error kind={:?} message={}",
            error.kind(),
            error.message()
        );
    }
    for (label, error) in [("action", action_result.err()), ("menu", menus.err())] {
        let error = error
            .unwrap_or_else(|| panic!("{label} must refuse source storage before producing a DTO"));
        assert_build_refusal(&error, 4096);
    }
    let menus = erp_addon_web::metadata_test_support::test_load_menus(&env, tx, 32 * 1024 * 1024)
        .await
        .unwrap();
    assert!(
        serde_json::to_value(menus).unwrap()["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["label"]
                .as_str()
                .is_some_and(|name| name.len() == 4096))
    );
    let returned_action =
        erp_addon_web::metadata_test_support::test_load_action(&env, tx, action, 32 * 1024 * 1024)
            .await
            .unwrap();
    assert_eq!(
        serde_json::to_value(returned_action).unwrap()["name"]
            .as_str()
            .unwrap()
            .len(),
        4096
    );
    assert_view_aggregation(tx, action).await;
    assert_view_referenced_unrenderable_field_fails(tx).await;
}

fn assert_build_refusal(error: &erp_fields::Error, limit: usize) {
    assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
    let prefix = format!(
        "simultaneously retained description bytes limit exceeded: maximum {limit}, reached "
    );
    let actual: usize = error
        .message()
        .strip_prefix(&prefix)
        .expect("precise construction limit diagnostic")
        .parse()
        .unwrap();
    assert!(actual > limit);
    assert_eq!(
        error.message(),
        erp_sdk::views::ViewError::LimitExceeded {
            limit: erp_sdk::views::LimitKind::DescriptionBuildBytes,
            max: limit,
            actual: Some(actual),
        }
        .to_string()
    );
}

async fn assert_view_aggregation(tx: &mut Transaction, action: i64) {
    for (kind, arch) in [
        ("list", r#"<list><field name="name"/></list>"#),
        (
            "form",
            r#"<form><sheet><group><field name="name"/></group></sheet></form>"#,
        ),
        ("search", r#"<search><field name="name"/></search>"#),
    ] {
        create(
            tx,
            "ir.ui.view",
            RawVals::new()
                .set_cell("name", Cell::Str(format!("H18 admission {kind}").into()))
                .set_cell("model", Cell::Str("res.partner".into()))
                .set_cell("type", Cell::Str(kind.into()))
                .set_cell("arch", Cell::Str(arch.into()))
                .set_cell("priority", Cell::Int(0)),
        )
        .await;
    }
    let env = tx.env().with_context([
        ("list_view_ref", erp_fields::Value::str("unqualified-list")),
        ("form_view_ref", erp_fields::Value::str("unqualified-form")),
        (
            "search_view_ref",
            erp_fields::Value::str("unqualified-search"),
        ),
    ]);
    let requests = || {
        ["list", "form", "search"]
            .into_iter()
            .map(|kind| {
                erp_fields::Value::list([erp_fields::Value::False, erp_fields::Value::str(kind)])
            })
            .collect()
    };
    let low = erp_addon_web::metadata_test_support::test_get_views(
        &env,
        tx,
        "res.partner",
        requests(),
        1,
    )
    .await
    .unwrap_err();
    assert_build_refusal(&low, 1);
    let high = erp_addon_web::metadata_test_support::test_get_views(
        &env,
        tx,
        "res.partner",
        requests(),
        32 * 1024 * 1024,
    )
    .await
    .unwrap();
    let value = serde_json::to_value(high).unwrap();
    assert_eq!(value["views"].as_array().unwrap().len(), 3);
    // The Search view adds every readable, renderable model field to the optional catalogue, so
    // the repeated `name` references of the three views stay one entry, and a field type with no
    // human renderer (the dynamic selection `lang`) is left out instead of failing the call.
    let catalogue = value["fields"]["res.partner"].as_object().unwrap();
    assert!(catalogue.contains_key("name"));
    assert!(!catalogue.contains_key("lang"));
    assert_eq!(
        value["notices"].as_array().unwrap().len(),
        3,
        "all selected-view notices survive aggregation"
    );
    let low_bundle =
        erp_addon_web::metadata_test_support::test_load_action_views(&env, tx, action, 4096)
            .await
            .unwrap_err();
    assert_build_refusal(&low_bundle, 4096);
    let bundle = erp_addon_web::metadata_test_support::test_load_action_views(
        &env,
        tx,
        action,
        32 * 1024 * 1024,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(bundle).unwrap()["action"]["name"]
            .as_str()
            .unwrap()
            .len(),
        4096
    );
}

/// `res.partner.lang` is a dynamic selection, which the human client cannot render. A search view
/// that never names it still succeeds and its optional catalogue silently leaves `lang` out. A
/// search view that references it itself, through a filter domain or a field node, fails the call
/// with the located Validation error instead of losing the entry.
async fn assert_view_referenced_unrenderable_field_fails(tx: &mut Transaction) {
    async fn search_view(tx: &mut Transaction, label: &str, arch: &str) -> i64 {
        create(
            tx,
            "ir.ui.view",
            RawVals::new()
                .set_cell(
                    "name",
                    Cell::Str(format!("H18 unrenderable {label}").into()),
                )
                .set_cell("model", Cell::Str("res.partner".into()))
                .set_cell("type", Cell::Str("search".into()))
                .set_cell("arch", Cell::Str(arch.into()))
                .set_cell("priority", Cell::Int(0)),
        )
        .await
    }
    async fn get(tx: &mut Transaction, view: i64) -> erp_fields::Result<serde_json::Value> {
        let env = tx.env().clone();
        let described = erp_addon_web::metadata_test_support::test_get_views(
            &env,
            tx,
            "res.partner",
            vec![erp_fields::Value::list([
                erp_fields::Value::Int(view),
                erp_fields::Value::str("search"),
            ])],
            32 * 1024 * 1024,
        )
        .await?;
        Ok(serde_json::to_value(described).unwrap())
    }
    let clean = search_view(tx, "clean", r#"<search><field name="name"/></search>"#).await;
    let value = get(tx, clean).await.unwrap();
    let catalogue = value["fields"]["res.partner"].as_object().unwrap();
    assert!(catalogue.contains_key("name"));
    assert!(
        !catalogue.contains_key("lang"),
        "the optional catalogue omits a field type with no human renderer"
    );
    for (label, arch) in [
        (
            "filter",
            r#"<search><filter name="by_lang" string="Language" domain="[('lang','=','en_US')]"/></search>"#,
        ),
        ("field", r#"<search><field name="lang"/></search>"#),
    ] {
        let view = search_view(tx, label, arch).await;
        let error = get(tx, view)
            .await
            .expect_err("a view that references the unrenderable field must fail loudly");
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation, "{label}");
        assert!(
            error.message().contains("Dynamic selection field")
                && error.message().contains("lang")
                && error
                    .message()
                    .ends_with("is unsupported by the current human UI contract."),
            "{label}: {}",
            error.message()
        );
    }
}
