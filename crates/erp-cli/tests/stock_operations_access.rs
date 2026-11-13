// SPDX-License-Identifier: MIT OR Apache-2.0
//! The stock operations tables (`stock.reference`, `stock.picking`, `stock.move`)
//! and the move line tables (`stock.move.line`, `stock.move.line.serial`,
//! `stock.move.line.correction`) ship no access rows yet (adds them): on a real `stock`
//! install with the compiled policy, an ordinary internal user is refused on each of them, for reading
//! and for creating.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use std::path::Path;
use std::sync::Arc;

use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Context, Orm, RawVals, SUPERUSER_ID, UserId};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

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
        .unwrap()
}

/// Run `body` on a thread with a large stack: a full install chain exceeds the default one.
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

fn refusal(verb: &str, description: &str, model: &str) -> String {
    format!(
        "You are not allowed to {verb} '{description}' ({model}) records.\n\nNo group currently \
         allows this operation.\n\nContact your administrator to request access if necessary."
    )
}

#[test]
fn internal_user_is_refused_on_operations_tables_until_access_rows_ship() {
    run(async {
        let db = TestDb::builder().pool_size(8).create().await.unwrap();
        let system = system(&db);
        let initialized = system.initialize(InitOptions::new()).await.unwrap();
        assert!(initialized.post_commit.is_clean());
        let installed = system
            .button_immediate_install(
                &system.system_env(Context::new()),
                &[ModuleName::new("stock").unwrap()],
            )
            .await
            .unwrap();
        assert!(
            installed.post_commit.is_clean(),
            "{:?}",
            installed.post_commit
        );
        let installed = installed.orm;
        // the compiled policy of the installed access data, as the server builds it
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
        let mut ids = Vec::new();
        for name in ["main_company", "group_system", "group_user"] {
            let rows = tx
                .query(
                    "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = $1",
                    &[&name],
                )
                .await
                .unwrap();
            ids.push(rows[0].get::<_, i64>(0));
        }
        let company = ids[0];
        // the base rules want an administrator before an ordinary user is created
        let mut users = Vec::new();
        for (login, group) in [("stock-admin", ids[1]), ("stock-clerk", ids[2])] {
            let created = tx
                .env()
                .records("res.users")
                .unwrap()
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
            users.push(created.ids()[0]);
        }
        let user = users[1];
        tx.install_audit_schema().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let mut tx = orm.begin(UserId::new(user).unwrap()).await.unwrap();
        for model in [
            "stock.reference",
            "stock.picking",
            "stock.move",
            "stock.move.line",
            "stock.move.line.serial",
            "stock.move.line.correction",
        ] {
            // the refusal names the model's description, read from the installed registry
            let description = installed
                .registry()
                .model(model)
                .unwrap()
                .description
                .clone();
            let description = description.as_str();
            let records = tx.env().records(model).unwrap();
            let error = records
                .search_count(&mut tx, &erp_sdk::Domain::constant(true), None)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Access, "{model}");
            assert_eq!(error.message(), refusal("access", description, model));
            let error = records
                .create(&mut tx, vec![RawVals::new()])
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Access, "{model}");
            assert_eq!(error.message(), refusal("create", description, model));
        }
        tx.rollback().await.unwrap();
        drop(tx);
        db.cleanup().await.unwrap();
    });
}
