// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real barcode installation, defaults, model constraints, authorization and served views.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use erp_fields::Cell;
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Context, Orm, RawVals, SUPERUSER_ID};
use erp_testkit::TestDb;
use std::path::Path;

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../addons")
            .canonicalize()
            .unwrap(),
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
    ModuleSystem::builder(db.pool().clone(), index)
        .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
        .native_artifact_identities(identities)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_barcodes::ADDON,
        ])
        .build()
        .unwrap()
}
fn run(body: impl std::future::Future<Output = ()> + Send + 'static) {
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
async fn install(db: &TestDb) -> (ModuleSystem, Orm) {
    let system = system(db);
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    assert!(initialized.post_commit.is_clean());
    let installed = system
        .button_immediate_install(
            &system.system_env(Context::new()),
            &[ModuleName::new("barcodes").unwrap()],
        )
        .await
        .unwrap();
    assert!(
        installed.post_commit.is_clean(),
        "{:?}",
        installed.post_commit
    );
    (system, installed.orm)
}
#[test]
fn barcode_install_defaults_constraints_and_company_selection() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let (_system, orm) = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let row=tx.query("SELECT n.id,n.upc_ean_conv,r.pattern,r.sequence FROM barcode_nomenclature n JOIN barcode_rule r ON r.barcode_nomenclature_id=n.id",&[]).await.unwrap();
        assert_eq!(row.len(), 1);
        let default_id: i64 = row[0].get(0);
        assert_eq!(row[0].get::<_, String>(1), "always");
        assert_eq!(row[0].get::<_, String>(2), ".*");
        assert_eq!(row[0].get::<_, i32>(3), 90);
        let count: i64 = tx
            .query(
                "SELECT count(*) FROM res_company WHERE nomenclature_id IS DISTINCT FROM $1",
                &[&default_id],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(count, 0);
        let companies = tx.env().records("res.company").unwrap();
        let currency: i64 = tx
            .query(
                "SELECT currency_id FROM res_company ORDER BY id LIMIT 1",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let new = companies
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Barcode company".into()))
                        .set_cell("currency_id", Cell::Id(currency)),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            new.cell(&mut tx, "nomenclature_id").await.unwrap(),
            Cell::Id(default_id)
        );
        let rules = tx.env().records("barcode.rule").unwrap();
        let error = tx
            .savepoint(async |tx| {
                rules
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("name", Cell::Str("Bad pattern".into()))
                                .set_cell("pattern", Cell::Str("{}".into())),
                        ],
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Numeric pattern must contain N digits followed by D digits."
        );
        let error = tx
            .savepoint(async |tx| {
                rules
                    .create(
                        tx,
                        (0..257)
                            .map(|index| {
                                RawVals::new()
                                    .set_cell(
                                        "name",
                                        Cell::Str(format!("Bounded rule {index}").into()),
                                    )
                                    .set_cell("pattern", Cell::Str(".*".into()))
                            })
                            .collect(),
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Rule validation batch exceeds 256 records."
        );
        let rule = rules
            .browse([
                tx.query("SELECT id FROM barcode_rule", &[]).await.unwrap()[0].get::<_, i64>(0),
            ])
            .unwrap();
        let error = tx
            .savepoint(async |tx| {
                rule.write(
                    tx,
                    RawVals::new().set_cell("pattern", Cell::Str("[".into())),
                )
                .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Pattern is not supported by the bounded regex engine."
        );
        assert_eq!(
            rule.cell(&mut tx, "pattern").await.unwrap(),
            Cell::Str(".*".into())
        );
        let nomenclatures = tx.env().records("barcode.nomenclature").unwrap();
        let extra = nomenclatures
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Other".into()))],
            )
            .await
            .unwrap();
        let mixed = nomenclatures.browse([default_id, extra.ids()[0]]).unwrap();
        let error = tx
            .savepoint(async |tx| mixed.unlink(tx).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "Cannot delete the default barcode nomenclature."
        );
        new.write(
            &mut tx,
            RawVals::new().set_cell("nomenclature_id", Cell::Id(extra.ids()[0])),
        )
        .await
        .unwrap();
        extra.unlink(&mut tx).await.unwrap();
        assert_eq!(
            new.cell(&mut tx, "nomenclature_id").await.unwrap(),
            Cell::Null
        );
        let output: String = nomenclatures
            .browse([default_id])
            .unwrap()
            .call(&mut tx, "parse_barcode", ("ABC-42".to_owned(),))
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["items"][0]["code"], "ABC-42");
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn stock_adds_default_rule_types_without_transaction_schema() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let mut index = AddonIndex::new(true);
        index.add_dev_path(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../addons")
                .canonicalize()
                .unwrap(),
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
            .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
            .native_artifact_identities(identities)
            .view_types(erp_addon_web::view_type_contributions())
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .code([
                &erp_addon_base::ADDON,
                &erp_addon_web::ADDON,
                &erp_addon_barcodes::ADDON,
                &erp_addon_barcodes_gs1_nomenclature::ADDON,
                &erp_addon_uom::ADDON,
                &erp_addon_product::ADDON,
                &erp_addon_stock::ADDON,
            ])
            .build()
            .unwrap();
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        assert!(initialized.post_commit.is_clean());
        let outcome = system
            .button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap();
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
        let rows = tx
            .query("SELECT type FROM barcode_rule ORDER BY sequence,id", &[])
            .await
            .unwrap();
        let types: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        assert_eq!(types, ["weight", "location", "package", "lot", "product"]);
        let id: i64 = tx
            .query("SELECT id FROM barcode_nomenclature", &[])
            .await
            .unwrap()[0]
            .get(0);
        let n = tx
            .env()
            .records("barcode.nomenclature")
            .unwrap()
            .browse([id])
            .unwrap();
        let output: String = n
            .call(&mut tx, "parse_barcode", ("2100000012503".to_owned(),))
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["items"][0]["type"], "weight");
        assert_eq!(output["items"][0]["value"], "1.25");
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

struct Host;
impl erp_rpc::RpcHost for Host {
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
#[test]
fn barcode_permissions_and_every_view_served_to_manager() {
    run(async {
        use erp_fields::RecId;
        use erp_orm::{Command, UserId};
        use erp_rpc::{Caller, RpcCall, RpcConfig, RpcService};
        use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
        use erp_security::{resolver::Resolver, source::PolicySource};
        use std::sync::Arc;
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let (_system, installed) = install(&db).await;
        let source = PolicySource::build(Arc::clone(installed.registry()), db.pool().clone())
            .await
            .unwrap();
        let orm = Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
            .generation_attachment(erp_orm::GenerationAttachment::new(
                Arc::clone(installed.registry()),
                installed
                    .env(SUPERUSER_ID, Context::new())
                    .generation_attachment::<erp_views::PublishedViewTypes>()
                    .unwrap(),
            ))
            .require_core_tables(erp_security::auth::core_tables())
            .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
            .resolver(Resolver::for_registry(installed.registry()).unwrap())
            .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
            .impls(erp_modules::base_models::IMPLS.iter().copied())
            .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
            .unlink_hook(erp_data::model_data::UNLINK_HOOK)
            .build()
            .unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let company: i64 = tx
            .query(
                "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let users = tx.env().records("res.users").unwrap();
        let mut identities = Vec::new();
        for (login, group) in [
            ("barcode-admin", "group_system"),
            ("barcode-manager", "group_erp_manager"),
            ("barcode-user", "group_user"),
            ("barcode-portal", "group_portal"),
        ] {
            let group: i64 = tx
                .query(
                    "SELECT res_id FROM ir_model_data WHERE module='base' AND name=$1",
                    &[&group],
                )
                .await
                .unwrap()[0]
                .get(0);
            let user = users
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(login.into()))
                            .set_cell("login", Cell::Str(login.into()))
                            .set_cell(
                                "password",
                                Cell::Str(format!("{login}-fixture-password").into()),
                            )
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
                .unwrap();
            identities.push((login, user.ids()[0]));
        }
        let views=tx.query("SELECT v.id,v.model,v.type FROM ir_ui_view v JOIN ir_model_data d ON d.res_id=v.id AND d.model='ir.ui.view' WHERE d.module='barcodes' AND v.inherit_id IS NULL",&[]).await.unwrap();
        assert_eq!(views.len(), 4);
        let company_form: i64 = tx.query("SELECT res_id FROM ir_model_data WHERE module='base' AND name='settings_companies_form'", &[]).await.unwrap()[0].get(0);
        let nomenclature: i64 = tx
            .query("SELECT id FROM barcode_nomenclature", &[])
            .await
            .unwrap()[0]
            .get(0);
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        for (login, id) in identities
            .iter()
            .filter(|(login, _)| matches!(*login, "barcode-user" | "barcode-portal"))
        {
            let mut tx = orm.begin(UserId::new(*id).unwrap()).await.unwrap();
            let model = tx.env().records("barcode.nomenclature").unwrap();
            if *login == "barcode-user" {
                assert_eq!(
                    model
                        .search_count(&mut tx, &erp_sdk::Domain::constant(true), None)
                        .await
                        .unwrap(),
                    1
                );
                let rules = tx.env().records("barcode.rule").unwrap();
                assert_eq!(
                    rules
                        .search_count(&mut tx, &erp_sdk::Domain::constant(true), None)
                        .await
                        .unwrap(),
                    1
                );
                let parsed: String = model
                    .browse([nomenclature])
                    .unwrap()
                    .call(&mut tx, "parse_barcode", ("USER-42".to_owned(),))
                    .await
                    .unwrap();
                let parsed: serde_json::Value = serde_json::from_str(&parsed).unwrap();
                assert_eq!(parsed["items"][0]["code"], "USER-42");
                let company = tx
                    .env()
                    .records("res.company")
                    .unwrap()
                    .browse([company])
                    .unwrap();
                let error = company
                    .write(
                        &mut tx,
                        RawVals::new().set_cell("nomenclature_id", Cell::Id(nomenclature)),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
                assert_eq!(
                    error.message(),
                    "Your access rights do not let you modify 'Company' (res.company) records.\n\nGroups that may do this:\n\t- Settings Administration\n\nAsk an administrator if you need this access."
                );
            } else {
                let error = model
                    .browse([nomenclature])
                    .unwrap()
                    .call::<_, String>(&mut tx, "parse_barcode", ("PORTAL-42".to_owned(),))
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
                assert_eq!(
                    error.message(),
                    "Your access rights do not let you access 'Barcode Nomenclature' (barcode.nomenclature) records.\n\nGroups that may do this:\n\t- Access Administration\n\t- Internal User\n\t- Regular User\n\t- Settings Administration\n\nAsk an administrator if you need this access."
                );
            }
            let error = model
                .create(
                    &mut tx,
                    vec![RawVals::new().set_cell("name", Cell::Str("Denied".into()))],
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
            assert_eq!(
                error.message(),
                "Your access rights do not let you create 'Barcode Nomenclature' (barcode.nomenclature) records.\n\nGroups that may do this:\n\t- Access Administration\n\t- Settings Administration\n\nAsk an administrator if you need this access."
            );
            tx.rollback().await.unwrap();
        }
        let auth = Auth::new(
            orm.clone(),
            erp_security::auth::AuthConfig::DEFAULT,
            HashPool::global(),
        )
        .await
        .unwrap();
        let administrator = auth
            .authenticate(LoginRequest::new(
                "barcode-admin",
                "barcode-admin-fixture-password",
                ClientAddr::InProcess("barcode-company-test"),
            ))
            .await
            .unwrap();
        let mut admin = auth
            .begin_session(administrator.session().secret(), CredentialOptions::new())
            .await
            .unwrap();
        let company_model = admin.tx.env().records("res.company").unwrap();
        let defaults: erp_sdk::ValueMap = company_model
            .call(
                &mut admin.tx,
                "default_get",
                (vec!["nomenclature_id".to_owned()],),
            )
            .await
            .unwrap();
        assert_eq!(
            defaults.get("nomenclature_id"),
            Some(&erp_sdk::Value::Int(nomenclature))
        );
        let chosen = admin
            .tx
            .env()
            .records("barcode.nomenclature")
            .unwrap()
            .create(
                &mut admin.tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Administrator choice".into()))],
            )
            .await
            .unwrap();
        let selected_company = company_model.browse([company]).unwrap();
        selected_company
            .write(
                &mut admin.tx,
                RawVals::new().set_cell("nomenclature_id", Cell::Id(chosen.ids()[0])),
            )
            .await
            .unwrap();
        assert_eq!(
            selected_company
                .cell(&mut admin.tx, "nomenclature_id")
                .await
                .unwrap(),
            Cell::Id(chosen.ids()[0])
        );
        admin.tx.rollback().await.unwrap();
        let logged = auth
            .authenticate(LoginRequest::new(
                "barcode-manager",
                "barcode-manager-fixture-password",
                ClientAddr::InProcess("barcode-test"),
            ))
            .await
            .unwrap();
        let mut begun = auth
            .begin_session(logged.session().secret(), CredentialOptions::new())
            .await
            .unwrap();
        let principal = begun.tx.env().principal().clone();
        let configured = begun
            .tx
            .env()
            .records("barcode.nomenclature")
            .unwrap()
            .create(
                &mut begun.tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Manager-created".into()))],
            )
            .await
            .unwrap();
        let configured_rule = begun
            .tx
            .env()
            .records("barcode.rule")
            .unwrap()
            .create(
                &mut begun.tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Manager rule".into()))
                        .set_cell("barcode_nomenclature_id", Cell::Id(configured.ids()[0]))
                        .set_cell("pattern", Cell::Str("MANAGER".into())),
                ],
            )
            .await
            .unwrap();
        configured_rule
            .write(
                &mut begun.tx,
                RawVals::new().set_cell("sequence", Cell::Int(10)),
            )
            .await
            .unwrap();
        assert_eq!(
            configured_rule
                .cell(&mut begun.tx, "sequence")
                .await
                .unwrap(),
            Cell::Int(10)
        );
        configured_rule.unlink(&mut begun.tx).await.unwrap();
        configured.unlink(&mut begun.tx).await.unwrap();
        let protected = begun
            .tx
            .env()
            .records("barcode.nomenclature")
            .unwrap()
            .with_context([("force_delete", erp_sdk::Value::Bool(true))])
            .browse([nomenclature])
            .unwrap();
        let error = begun
            .tx
            .savepoint(async |tx| protected.unlink(tx).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "Cannot delete the default barcode nomenclature."
        );
        begun.tx.rollback().await.unwrap();
        let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
        let public = service
            .call(RpcCall::json2(
                "/json/2/barcode.nomenclature/parse_barcode",
                serde_json::to_vec(&serde_json::json!({"ids":[nomenclature],"code":"PUBLIC-42"}))
                    .unwrap(),
                Caller::Public,
            ))
            .await;
        let denied: serde_json::Value = serde_json::from_slice(public.body()).unwrap();
        assert_eq!(denied["name"], "erp.exceptions.AccessDenied");
        assert_eq!(
            denied["message"],
            "public callers are not accepted until a later release"
        );
        for row in views {
            let id: i64 = row.get(0);
            let model: String = row.get(1);
            let kind: String = row.get(2);
            for mobile in [false, true] {
                let response=service.call(RpcCall::json2(&format!("/json/2/{model}/get_views"),serde_json::to_vec(&serde_json::json!({"views":[[id,kind]],"wire_major":1,"options":{"mobile":mobile}})).unwrap(),Caller::Principal(principal.clone()))).await;
                assert_eq!(
                    response.status(),
                    200,
                    "{}",
                    String::from_utf8_lossy(response.body())
                );
                let served = String::from_utf8_lossy(response.body());
                assert!(served.contains("name"));
                assert!(served.contains(if model == "barcode.rule" {
                    "pattern"
                } else {
                    "upc_ean_conv"
                }));
            }
        }
        for mobile in [false, true] {
            let response = service.call(RpcCall::json2("/json/2/res.company/get_views", serde_json::to_vec(&serde_json::json!({"views":[[company_form,"form"]],"wire_major":1,"options":{"mobile":mobile}})).unwrap(), Caller::Principal(principal.clone()))).await;
            assert_eq!(
                response.status(),
                200,
                "{}",
                String::from_utf8_lossy(response.body())
            );
            assert!(String::from_utf8_lossy(response.body()).contains("nomenclature_id"));
        }
        for model in ["barcode.nomenclature", "barcode.rule"] {
            let response = service
                .call(RpcCall::json2(
                    &format!("/json/2/{model}/default_get"),
                    serde_json::to_vec(&serde_json::json!({"fields":["name"]})).unwrap(),
                    Caller::Principal(principal.clone()),
                ))
                .await;
            assert_eq!(
                response.status(),
                200,
                "{}",
                String::from_utf8_lossy(response.body())
            );
        }
        for (model, fields_spec) in [
            (
                "barcode.nomenclature",
                serde_json::json!({"name":{},"upc_ean_conv":{},"rule_ids":{"fields":{"display_name":{}}}}),
            ),
            (
                "barcode.rule",
                serde_json::json!({"name":{},"sequence":{},"encoding":{},"type":{},"pattern":{},"alias":{}}),
            ),
        ] {
            let response = service.call(RpcCall::json2(&format!("/json/2/{model}/onchange"), serde_json::to_vec(&serde_json::json!({"ids":[],"values":{},"field_names":[],"fields_spec":fields_spec})).unwrap(), Caller::Principal(principal.clone()))).await;
            assert_eq!(
                response.status(),
                200,
                "{}",
                String::from_utf8_lossy(response.body())
            );
        }
        let response = service
            .call(RpcCall::json2(
                "/json/2/barcode.nomenclature/parse_barcode",
                serde_json::to_vec(&serde_json::json!({"ids":[nomenclature],"code":"ITEM-42"}))
                    .unwrap(),
                Caller::Principal(principal),
            ))
            .await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        db.cleanup().await.unwrap();
    });
}

#[test]
fn committed_configuration_survives_reader_process_restart() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let (_system, orm) = install(&db).await;
        let read = || {
            use erp_testkit::tokio_postgres::config::Host;
            let cfg = db.config();
            let mut command = std::process::Command::new("psql");
            match &cfg.get_hosts()[0] {
                Host::Tcp(host) => {
                    command.env("PGHOST", host);
                }
                #[cfg(unix)]
                Host::Unix(path) => {
                    command.env("PGHOST", path);
                }
            }
            command
                .env("PGPORT", cfg.get_ports()[0].to_string())
                .env("PGDATABASE", db.name())
                .env("PGUSER", cfg.get_user().unwrap());
            if let Some(password) = cfg.get_password() {
                command.env("PGPASSWORD", std::str::from_utf8(password).unwrap());
            }
            let output=command.args(["-X","-A","-t","-v","ON_ERROR_STOP=1","-c","SELECT DISTINCT n.upc_ean_conv FROM res_company c JOIN barcode_nomenclature n ON n.id=c.nomenclature_id ORDER BY 1"]).output().unwrap();
            assert!(
                output.status.success(),
                "independent configuration reader failed"
            );
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(read(), "always\n");
        assert_eq!(read(), "always\n");
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.execute("UPDATE barcode_nomenclature SET upc_ean_conv='none'", &[])
            .await
            .unwrap();
        assert_eq!(read(), "always\n");
        tx.rollback().await.unwrap();
        drop(tx);
        assert_eq!(read(), "always\n");
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.execute("UPDATE barcode_nomenclature SET upc_ean_conv='none'", &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        assert_eq!(read(), "none\n");
        assert_eq!(read(), "none\n");
        db.cleanup().await.unwrap();
    });
}

#[test]
fn barcode_upgrade_preserves_customized_defaults_and_explicit_company_choice() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let (system, orm) = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let configurations = tx.env().records("barcode.nomenclature").unwrap();
        let other = configurations
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str("Company-specific".into()))],
            )
            .await
            .unwrap();
        let id = other.ids()[0];
        tx.execute("UPDATE res_company SET nomenclature_id=$1", &[&id])
            .await
            .unwrap();
        tx.execute("UPDATE barcode_rule SET pattern='CUSTOM.*'", &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let upgraded = system
            .button_immediate_upgrade(
                &system.system_env(Context::new()),
                &[ModuleName::new("barcodes").unwrap()],
            )
            .await
            .unwrap();
        let mut tx = upgraded.orm.begin(SUPERUSER_ID).await.unwrap();
        let wrong: i64 = tx
            .query(
                "SELECT count(*) FROM res_company WHERE nomenclature_id IS DISTINCT FROM $1",
                &[&id],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(wrong, 0);
        let pattern: String = tx
            .query("SELECT pattern FROM barcode_rule", &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(pattern, "CUSTOM.*");
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn barcode_install_initializes_archived_company() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        let mut tx = initialized.orm.begin(SUPERUSER_ID).await.unwrap();
        let currency: i64 = tx
            .query(
                "SELECT currency_id FROM res_company ORDER BY id LIMIT 1",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let old = tx
            .env()
            .records("res.company")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Archived before barcode install".into()))
                        .set_cell("active", Cell::Bool(false))
                        .set_cell("currency_id", Cell::Id(currency)),
                ],
            )
            .await
            .unwrap();
        let id = old.ids()[0];
        tx.commit().await.unwrap();
        drop(tx);
        let installed = system
            .button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("barcodes").unwrap()],
            )
            .await
            .unwrap();
        let mut tx = installed.orm.begin(SUPERUSER_ID).await.unwrap();
        let count:i64=tx.query("SELECT count(*) FROM res_company c JOIN ir_model_data d ON d.res_id=c.nomenclature_id AND d.module='barcodes' AND d.name='default_barcode_nomenclature' WHERE c.id=$1 AND NOT c.active",&[&id]).await.unwrap()[0].get(0);
        assert_eq!(count, 1);
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}

#[test]
fn barcode_uninstall_removes_defaults_and_company_extension() {
    run(async {
        let db = TestDb::create().await.unwrap();
        let (system, orm) = install(&db).await;
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let companies: i64 = tx
            .query("SELECT count(*) FROM res_company", &[])
            .await
            .unwrap()[0]
            .get(0);
        tx.rollback().await.unwrap();
        drop(tx);
        let outcome = system
            .button_immediate_uninstall(
                &system.system_env(Context::new()),
                &[ModuleName::new("barcodes").unwrap()],
            )
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean());
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await.unwrap();
        let row = tx.query("SELECT to_regclass('barcode_nomenclature') IS NULL, to_regclass('barcode_rule') IS NULL", &[]).await.unwrap();
        assert!(row[0].get::<_, bool>(0));
        assert!(row[0].get::<_, bool>(1));
        let columns: i64 = tx.query("SELECT count(*) FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='res_company' AND column_name='nomenclature_id'", &[]).await.unwrap()[0].get(0);
        assert_eq!(columns, 0);
        let retained: i64 = tx
            .query("SELECT count(*) FROM res_company", &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(retained, companies);
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}
