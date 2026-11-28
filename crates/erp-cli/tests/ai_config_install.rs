// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fresh installs, access boundaries, and synthetic loopback provider exchanges.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
use erp_modules::{
    AddonIndex, ModuleName,
    system::{InitOptions, ModuleSystem},
};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID, UserId};
use erp_sdk::{Cell, Domain, ErrorKind, RecId};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

fn addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../addons")
        .canonicalize()
        .unwrap()
}
#[path = "ai_config_install/aggregate_turn.rs"]
mod aggregate_turn;
#[path = "ai_config_install/conversation.rs"]
mod conversation;
#[path = "ai_config_install/correction_turn.rs"]
mod correction_turn;
#[path = "ai_config_install/find_models.rs"]
mod find_models;
#[path = "ai_config_install/history_turn.rs"]
mod history_turn;
#[path = "ai_config_install/implementation.rs"]
mod implementation;
#[path = "ai_config_install/insights.rs"]
mod insights;
#[allow(
    clippy::panic,
    reason = "the shared test HTTP harness asserts native process failures"
)]
#[path = "support/native_http.rs"]
mod native_http;
#[path = "ai_config_install/retail.rs"]
mod retail;
#[path = "ai_config_install/search_records.rs"]
mod search_records;
#[path = "ai_config_install/text_turn.rs"]
mod text_turn;

async fn install(db: &TestDb) -> Orm {
    install_modules(
        db,
        &["ai_base"],
        &[
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_ai_base::ADDON,
        ],
    )
    .await
}

async fn install_modules(
    db: &TestDb,
    modules: &[&str],
    code: &[&'static erp_registry::AddonDescriptor],
) -> Orm {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons());
    let identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let identity = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, identity.digest().to_owned())
        })
        .collect();
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .code(code.iter().copied())
        .require_sql_objects(erp_cli::native_addons::SQL_MANIFESTS.iter().copied())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .build()
        .unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    assert!(
        initialized.post_commit.is_clean(),
        "{:?}",
        initialized.post_commit
    );
    let installed = system
        .button_immediate_install(
            &system.system_env(Context::new()),
            &modules
                .iter()
                .map(|name| ModuleName::new(*name).unwrap())
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    installed.orm
}
async fn xid(orm: &Orm, name: &str) -> i64 {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let id = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module='ai_base' AND name=$1",
            &[&name],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    id
}
fn big(body: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(body);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn fresh_install_seeds_disabled_provider_and_company_route() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        for name in [
            "provider_list",
            "provider_form",
            "provider_action",
            "provider_menu",
            "route_list",
            "route_form",
            "route_action",
            "route_menu",
            "budget_list",
            "budget_form",
            "budget_action",
            "budget_menu",
            "usage_list",
            "usage_form",
            "usage_action",
            "usage_menu",
        ] {
            assert!(xid(&orm, name).await > 0);
        }
        let provider = xid(&orm, "provider_local").await;
        let route = xid(&orm, "route_assistant_main").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            tx.env()
                .records("ai.provider")
                .unwrap()
                .browse([provider])
                .unwrap()
                .cell(&mut tx, "active")
                .await
                .unwrap(),
            Cell::Bool(false)
        );
        assert_eq!(
            tx.env()
                .records("ai.route")
                .unwrap()
                .browse([route])
                .unwrap()
                .cell(&mut tx, "provider_id")
                .await
                .unwrap(),
            Cell::Id(provider)
        );
        let company = tx
            .env()
            .records("ai.route")
            .unwrap()
            .browse([route])
            .unwrap()
            .cell(&mut tx, "company_id")
            .await
            .unwrap();
        let Cell::Id(company) = company else {
            panic!("route company missing")
        };
        let env = tx.env().with_company(company).unwrap();
        let error = erp_addon_ai_base::routing::resolve(
            &env,
            &mut tx,
            &erp_addon_ai_base::config::Credentials::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "erp.ai.provider_unconfigured: provider 'local'"
        );
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("fixture.key");
        std::fs::write(&key, "synthetic-ai-fixture").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config: toml::Value =
            toml::from_str("[provider.local]\nkey_file='fixture.key'\n").unwrap();
        let credentials =
            erp_addon_ai_base::config::Credentials::load(Some(&config), dir.path()).unwrap();
        let provider_row = tx
            .env()
            .records("ai.provider")
            .unwrap()
            .browse([provider])
            .unwrap();
        provider_row
            .write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
            .await
            .unwrap();
        let resolved = erp_addon_ai_base::routing::resolve(&env, &mut tx, &credentials)
            .await
            .unwrap();
        assert_eq!(resolved.provider(), "local");
        assert_eq!(resolved.model(), "local-model");
        assert_eq!(resolved.family(), "openai_compatible");
        assert_eq!(resolved.base_url(), "http://127.0.0.1:11434/v1");
        let denied = resolved.transport(&credentials).err().unwrap();
        assert!(matches!(
            denied,
            erp_sdk::ai::ProviderError::InvalidRequest { .. }
        ));
        assert_eq!(
            denied.to_string(),
            "provider 'local' invalid request: provider transport requires an unprivileged interactive user"
        );
        // An operator can change provider policy; an old route must then fail at resolution.
        provider_row
            .write(
                &mut tx,
                RawVals::new().set_cell(
                    "allowed_models",
                    Cell::Json(Arc::new(serde_json::json!(["replacement-model"]))),
                ),
            )
            .await
            .unwrap();
        let error = erp_addon_ai_base::routing::resolve(&env, &mut tx, &credentials)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "ai.route.model must be in ai.provider.allowed_models"
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn routes_refuse_models_outside_provider_allowlist_and_invalid_provider_fields() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let provider = xid(&orm, "provider_local").await;
        let route = xid(&orm, "route_assistant_main").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let error = tx
            .env()
            .records("ai.route")
            .unwrap()
            .browse([route])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("model", Cell::Str("not-allowed".into())),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "ai.route.model must be in ai.provider.allowed_models"
        );
        tx.rollback().await.unwrap();
        drop(tx);
        for (field, value, message) in [
            (
                "name",
                "invalid/provider",
                "ai.provider.name must be 1..=63 ASCII letters, digits, underscores or hyphens",
            ),
            (
                "family",
                "unknown",
                "Wrong value for ai.provider.family: 'unknown'",
            ),
            (
                "base_url",
                "https://example.test/v1?key=synthetic",
                "ai.provider.base_url must be HTTP(S) with a host and without credentials, query or fragment",
            ),
            (
                "base_url",
                "file:///tmp/model",
                "ai.provider.base_url must be HTTP(S) with a host and without credentials, query or fragment",
            ),
            (
                "base_url",
                "https://user:password@example.test/v1",
                "ai.provider.base_url must be HTTP(S) with a host and without credentials, query or fragment",
            ),
        ] {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let error = tx
                .env()
                .records("ai.provider")
                .unwrap()
                .browse([provider])
                .unwrap()
                .write(
                    &mut tx,
                    RawVals::new().set_cell(field, Cell::Str(value.into())),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(error.message(), message);
            tx.rollback().await.unwrap();
            drop(tx);
        }
        db.cleanup().await.unwrap();
    });
}

#[test]
fn operator_writes_and_company_visibility_follow_real_access_policy() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let installed = install(&db).await;
        let provider = xid(&installed, "provider_local").await;
        let route = xid(&installed, "route_assistant_main").await;
        let mut seed = installed.begin(SUPERUSER_ID).await.unwrap();
        let main: i64 = seed
            .query(
                "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let groups=seed.query("SELECT name,res_id FROM ir_model_data WHERE module='base' AND name IN ('group_user','group_system')",&[]).await.unwrap();
        let group = |name: &str| {
            groups
                .iter()
                .find(|r| r.get::<_, String>(0) == name)
                .unwrap()
                .get::<_, i64>(1)
        };
        let currency = seed
            .env()
            .records("res.company")
            .unwrap()
            .browse([main])
            .unwrap()
            .cell(&mut seed, "currency_id")
            .await
            .unwrap();
        let other = seed
            .env()
            .records("res.company")
            .unwrap()
            .create(
                &mut seed,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("AI second company".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        let mut users = vec![];
        for (name, group, company) in [
            // Base requires an active administrator before creating ordinary users.
            ("ai-operator", group("group_system"), main),
            ("ai-reader", group("group_user"), main),
            ("ai-foreign-operator", group("group_system"), other),
        ] {
            let id = seed
                .env()
                .records("res.users")
                .unwrap()
                .create(
                    &mut seed,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("login", Cell::Str(name.into()))
                            .set_cell("company_id", Cell::Id(company))
                            .set_commands(
                                "company_ids",
                                [Command::Set(vec![RecId::from_real(company).unwrap()])],
                            )
                            .set_commands(
                                "group_ids",
                                [Command::Set(vec![RecId::from_real(group).unwrap()])],
                            ),
                    ],
                )
                .await
                .unwrap()
                .ids()[0];
            users.push(id);
        }
        seed.commit().await.unwrap();
        drop(seed);
        let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
            .await
            .unwrap();
        let orm = Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .resolver(Resolver::for_registry(installed.registry()).unwrap())
            .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
            .impls(erp_modules::base_models::IMPLS.iter().copied())
            .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .unwrap();
        let mut tx = orm.begin(UserId::new(users[1]).unwrap()).await.unwrap();
        assert_eq!(
            tx.env()
                .records("ai.route")
                .unwrap()
                .search(&mut tx, &Domain::TRUE)
                .await
                .unwrap()
                .ids(),
            &[route]
        );
        let error = tx
            .env()
            .records("ai.provider")
            .unwrap()
            .browse([provider])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("region", Cell::Str("changed".into())),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Your access rights do not let you modify 'AI Provider' (ai.provider) records.\n\nGroups that may do this:\n\t- Settings Administration\n\nAsk an administrator if you need this access."
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(UserId::new(users[0]).unwrap()).await.unwrap();
        tx.env()
            .records("ai.provider")
            .unwrap()
            .browse([provider])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("region", Cell::Str("operator region".into())),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        // The same real policy permits an operator's complete configuration lifecycle.
        let mut tx = orm.begin(UserId::new(users[0]).unwrap()).await.unwrap();
        let old_route = tx
            .env()
            .records("ai.route")
            .unwrap()
            .browse([route])
            .unwrap();
        old_route.unlink(&mut tx).await.unwrap();
        let created_provider = tx
            .env()
            .records("ai.provider")
            .unwrap()
            .create(&mut tx, vec![provider_values("operator-created")])
            .await
            .unwrap();
        let created_route = tx
            .env()
            .records("ai.route")
            .unwrap()
            .create(&mut tx, vec![route_values(main, created_provider.ids()[0])])
            .await
            .unwrap();
        created_route
            .write(
                &mut tx,
                RawVals::new().set_cell("name", Cell::Str("Operator route".into())),
            )
            .await
            .unwrap();
        assert_eq!(
            created_route.cell(&mut tx, "name").await.unwrap(),
            Cell::Str("Operator route".into())
        );
        created_route.unlink(&mut tx).await.unwrap();
        created_provider.unlink(&mut tx).await.unwrap();
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(UserId::new(users[2]).unwrap()).await.unwrap();
        assert!(
            tx.env()
                .records("ai.route")
                .unwrap()
                .search(&mut tx, &Domain::TRUE)
                .await
                .unwrap()
                .is_empty()
        );
        tx.rollback().await.unwrap();
        drop(tx);
        for op in ["read", "write", "unlink"] {
            let mut tx = orm.begin(UserId::new(users[2]).unwrap()).await.unwrap();
            let row = tx
                .env()
                .records("ai.route")
                .unwrap()
                .browse([route])
                .unwrap();
            let error = match op {
                "read" => row.cell(&mut tx, "name").await.unwrap_err(),
                "write" => row
                    .write(
                        &mut tx,
                        RawVals::new().set_cell("name", Cell::Str("refused".into())),
                    )
                    .await
                    .unwrap_err(),
                "unlink" => row.unlink(&mut tx).await.unwrap_err(),
                _ => unreachable!(),
            };
            assert_company_denial(&error, "ai-foreign-operator", users[2], op);
            tx.rollback().await.unwrap();
            drop(tx);
        }
        let mut tx = orm.begin(UserId::new(users[0]).unwrap()).await.unwrap();
        let error = tx
            .env()
            .records("ai.route")
            .unwrap()
            .browse([route])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("company_id", Cell::Id(other)),
            )
            .await
            .unwrap_err();
        assert_company_denial(&error, "ai-operator", users[0], "write");
        tx.rollback().await.unwrap();
        drop(tx);
        for (model, label, id) in [
            ("ai.provider", "AI Provider", provider),
            ("ai.route", "AI Route", route),
        ] {
            for (op, verb) in [
                ("create", "create"),
                ("write", "modify"),
                ("unlink", "delete"),
            ] {
                let mut tx = orm.begin(UserId::new(users[1]).unwrap()).await.unwrap();
                let records = tx.env().records(model).unwrap();
                let error = match op {
                    "create" => {
                        let vals = if model == "ai.provider" {
                            provider_values("reader-denied")
                        } else {
                            route_values(main, provider)
                        };
                        records.create(&mut tx, vec![vals]).await.unwrap_err()
                    }
                    "write" => records
                        .browse([id])
                        .unwrap()
                        .write(
                            &mut tx,
                            RawVals::new().set_cell("name", Cell::Str("reader-denied".into())),
                        )
                        .await
                        .unwrap_err(),
                    "unlink" => records
                        .browse([id])
                        .unwrap()
                        .unlink(&mut tx)
                        .await
                        .unwrap_err(),
                    _ => unreachable!(),
                };
                assert_eq!(error.kind(), ErrorKind::Access);
                assert_eq!(
                    error.message(),
                    format!(
                        "Your access rights do not let you {verb} '{label}' ({model}) records.\n\nGroups that may do this:\n\t- Settings Administration\n\nAsk an administrator if you need this access."
                    )
                );
                tx.rollback().await.unwrap();
                drop(tx);
            }
        }
        db.cleanup().await.unwrap();
    });
}

fn provider_values(name: &str) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("base_url", Cell::Str("https://example.test/v1".into()))
        .set_cell(
            "allowed_models",
            Cell::Json(Arc::new(serde_json::json!(["local-model"]))),
        )
}

fn route_values(company: i64, provider: i64) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str("Fixture route".into()))
        .set_cell("company_id", Cell::Id(company))
        .set_cell("provider_id", Cell::Id(provider))
        .set_cell("model", Cell::Str("local-model".into()))
}

fn assert_company_denial(error: &erp_sdk::Error, user: &str, uid: i64, op: &str) {
    let op = if op == "unlink" { "delete" } else { op };
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        format!(
            "Access rules protect some of these records.\n\n{user} (id={uid}) has no '{op}' access to:\n- AI Route (ai.route)\n\nAsk an administrator if you need access to these records.\n\nThe record belongs to a company outside your allowed companies."
        )
    );
}

#[test]
fn provider_model_limits_and_identifier_lists_are_validated() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let provider = xid(&orm, "provider_local").await;
        // These superuser transactions isolate model constraints from access policy.
        for limit in [1, 1024] {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let row = tx
                .env()
                .records("ai.provider")
                .unwrap()
                .browse([provider])
                .unwrap();
            let mut models: Vec<String> = (0..limit).map(|n| format!("model-{n}")).collect();
            models[0] = "m".repeat(1024);
            row.write(
                &mut tx,
                RawVals::new()
                    .set_cell("model_limit", Cell::Int(limit))
                    .set_cell(
                        "allowed_models",
                        Cell::Json(Arc::new(serde_json::json!(models))),
                    ),
            )
            .await
            .unwrap();
            assert_eq!(
                row.cell(&mut tx, "model_limit").await.unwrap(),
                Cell::Int(limit)
            );
            assert_eq!(
                row.cell(&mut tx, "allowed_models").await.unwrap(),
                Cell::Json(Arc::new(serde_json::json!(models)))
            );
            tx.rollback().await.unwrap();
            drop(tx);
        }
        // The addon constraint refuses an empty list before database flush.
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let error = tx
            .env()
            .records("ai.provider")
            .unwrap()
            .browse([provider])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell(
                    "allowed_models",
                    Cell::Json(Arc::new(serde_json::json!([]))),
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "ai.provider.allowed_models must contain 1..=model_limit unique nonempty ASCII model identifiers of at most 1024 bytes"
        );
        tx.rollback().await.unwrap();
        drop(tx);
        for limit in [-1, 0, 1025] {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let error = tx
                .env()
                .records("ai.provider")
                .unwrap()
                .browse([provider])
                .unwrap()
                .write(
                    &mut tx,
                    RawVals::new().set_cell("model_limit", Cell::Int(limit)),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "ai.provider.model_limit must be in 1..=1024"
            );
            tx.rollback().await.unwrap();
            drop(tx);
        }
        for models in [
            serde_json::json!(["duplicate", "duplicate"]),
            serde_json::json!([""]),
            serde_json::json!(["model with space"]),
            serde_json::json!(["mödèl"]),
            serde_json::json!(["x".repeat(1025)]),
            serde_json::json!([42]),
            serde_json::json!({"model": "local-model"}),
            serde_json::json!(["one", "two", "three"]),
        ] {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let error = tx
                .env()
                .records("ai.provider")
                .unwrap()
                .browse([provider])
                .unwrap()
                .write(
                    &mut tx,
                    RawVals::new()
                        .set_cell("model_limit", Cell::Int(2))
                        .set_cell("allowed_models", Cell::Json(Arc::new(models))),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "ai.provider.allowed_models must contain 1..=model_limit unique nonempty ASCII model identifiers of at most 1024 bytes"
            );
            tx.rollback().await.unwrap();
            drop(tx);
        }
        db.cleanup().await.unwrap();
    });
}

#[test]
fn provider_names_and_company_task_routes_are_unique() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let provider = xid(&orm, "provider_local").await;
        let route = xid(&orm, "route_assistant_main").await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let Cell::Id(company) = tx
            .env()
            .records("ai.route")
            .unwrap()
            .browse([route])
            .unwrap()
            .cell(&mut tx, "company_id")
            .await
            .unwrap()
        else {
            panic!("route company missing")
        };
        tx.rollback().await.unwrap();
        drop(tx);
        for (model, vals, message) in [
            (
                "ai.provider",
                provider_values("local"),
                "The database refused the change: AI provider names must be unique.",
            ),
            (
                "ai.route",
                route_values(company, provider),
                "The database refused the change: An AI task has exactly one route per company.",
            ),
        ] {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let error = tx
                .env()
                .records(model)
                .unwrap()
                .create(&mut tx, vec![vals])
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(error.message(), message);
            tx.rollback().await.unwrap();
            drop(tx);
        }
        db.cleanup().await.unwrap();
    });
}

#[test]
fn routing_refuses_missing_or_unconfigured_selection_without_fallback() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let provider = xid(&orm, "provider_local").await;
        let route = xid(&orm, "route_assistant_main").await;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("alternative.key");
        std::fs::write(&key, "synthetic-alternative-credential").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config: toml::Value =
            toml::from_str("[provider.alternative]\nkey_file='alternative.key'\n").unwrap();
        let credentials =
            erp_addon_ai_base::config::Credentials::load(Some(&config), dir.path()).unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let alternative = tx
            .env()
            .records("ai.provider")
            .unwrap()
            .create(
                &mut tx,
                vec![provider_values("alternative").set_cell("active", Cell::Bool(true))],
            )
            .await
            .unwrap()
            .ids()[0];
        let row = tx
            .env()
            .records("ai.route")
            .unwrap()
            .browse([route])
            .unwrap();
        let Cell::Id(company) = row.cell(&mut tx, "company_id").await.unwrap() else {
            panic!("route company missing")
        };
        let env = tx.env().with_company(company).unwrap();
        // First prove the alternative really is usable, then restore the explicit local choice.
        row.write(
            &mut tx,
            RawVals::new().set_cell("provider_id", Cell::Id(alternative)),
        )
        .await
        .unwrap();
        let selected = erp_addon_ai_base::routing::resolve(&env, &mut tx, &credentials)
            .await
            .unwrap();
        assert_eq!(selected.provider(), "alternative");
        row.write(
            &mut tx,
            RawVals::new().set_cell("provider_id", Cell::Id(provider)),
        )
        .await
        .unwrap();
        for active in [false, true] {
            tx.env()
                .records("ai.provider")
                .unwrap()
                .browse([provider])
                .unwrap()
                .write(
                    &mut tx,
                    RawVals::new().set_cell("active", Cell::Bool(active)),
                )
                .await
                .unwrap();
            let error = erp_addon_ai_base::routing::resolve(&env, &mut tx, &credentials)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "erp.ai.provider_unconfigured: provider 'local'"
            );
        }
        let currency = tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([company])
            .unwrap()
            .cell(&mut tx, "currency_id")
            .await
            .unwrap();
        let other = tx
            .env()
            .records("res.company")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Routing isolation company".into()))
                        .set_cell("currency_id", currency),
                ],
            )
            .await
            .unwrap()
            .ids()[0];
        tx.env()
            .records("ai.route")
            .unwrap()
            .create(&mut tx, vec![route_values(other, alternative)])
            .await
            .unwrap();
        let other_env = tx.env().with_company(other).unwrap();
        let selected = erp_addon_ai_base::routing::resolve(&other_env, &mut tx, &credentials)
            .await
            .unwrap();
        assert_eq!(selected.provider(), "alternative");
        let error = erp_addon_ai_base::routing::resolve(&env, &mut tx, &credentials)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "erp.ai.provider_unconfigured: provider 'local'"
        );
        row.unlink(&mut tx).await.unwrap();
        let error = erp_addon_ai_base::routing::resolve(&env, &mut tx, &credentials)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            format!("erp.ai.route_missing: task 'assistant' company {company}")
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn route_links_are_required_and_routed_providers_cannot_be_deleted() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let orm = install(&db).await;
        let route = xid(&orm, "route_assistant_main").await;
        let provider = xid(&orm, "provider_local").await;
        let spec = orm.registry().model("ai.route").unwrap();
        for field in ["company_id", "provider_id"] {
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            let result = tx
                .env()
                .records("ai.route")
                .unwrap()
                .browse([route])
                .unwrap()
                .write(&mut tx, RawVals::new().set_cell(field, Cell::Null))
                .await;
            let error = match result {
                Ok(()) => tx.flush().await.unwrap_err(),
                Err(error) => error,
            };
            assert_eq!(error.kind(), ErrorKind::Validation);
            let label = &spec.field(field).unwrap().string;
            assert_eq!(
                error.message(),
                if field == "provider_id" {
                    "ai.route.provider_id must reference a record".to_owned()
                } else {
                    format!(
                        "The database refused the change: Missing required field '{label}' ({field}) for model 'AI Route' (ai.route)"
                    )
                }
            );
            tx.rollback().await.unwrap();
            drop(tx);
        }
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let error = tx
            .env()
            .records("ai.provider")
            .unwrap()
            .browse([provider])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        let label = &spec.field("provider_id").unwrap().string;
        assert_eq!(
            error.message(),
            format!(
                "The database refused the change: Other records still point to the record you want to delete.\n\nReferring model: 'AI Route' (ai.route)\nConstraint: '{label}' (provider_id)\nArchive the record instead of deleting it."
            )
        );
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn workspace_loads_the_current_company_and_permitted_retail_sources() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let retail = retail::install(&db).await;
        let action = xid(&retail.installed, "workspace_action").await;
        let mut tx = retail.orm.begin(retail.users[0]).await.unwrap();
        let answer = erp_addon_web::metadata_test_support::test_load_client_action(
            &tx.env(),
            &mut tx,
            action,
        )
        .await
        .unwrap();
        let wire = serde_json::to_value(answer).unwrap();
        assert_eq!(wire["result"]["tag"], "ai_base.workspace");
        let params = &wire["result"]["params"];
        assert_eq!(params["company_id"], retail.company);
        assert_eq!(
            params["history"],
            erp_addon_ai_base::ledger::tab_history(&mut tx)
                .await
                .unwrap()
        );
        let sources = params["sources"].as_array().unwrap();
        assert!(sources.iter().any(|item| item == "purchase.order"));
        assert!(sources.iter().all(|item| item != "sale.order"));
        let limit =
            erp_sdk::params::get_int(&mut tx, &erp_addon_ai_base::ADDON, "ai_base.search_rows")
                .await
                .unwrap();
        assert_eq!(params["limit"], limit);
        tx.rollback().await.unwrap();
    });
}
