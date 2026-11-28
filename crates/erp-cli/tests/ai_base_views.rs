// SPDX-License-Identifier: MIT OR Apache-2.0
//! Every `ai_base` window action must compile and load cleanly via `erp_load_web_action_views`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::Path;
use std::sync::Arc;

use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_orm::{Orm, UserId};
use erp_sdk::{Cell, Command, Context, RawVals, RecId, SUPERUSER_ID};
use erp_testkit::TestDb;

fn system(db: &TestDb) -> ModuleSystem {
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
    ModuleSystem::builder(db.pool().clone(), index)
        .view_types(erp_addon_web::view_type_contributions())
        .native_artifact_identities(identities)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_ai_base::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap()
}

async fn policy_orm(db: &TestDb, installed: &Orm) -> Orm {
    let source = erp_security::source::PolicySource::build(
        Arc::clone(installed.registry()),
        db.pool().clone(),
    )
    .await
    .unwrap();
    Orm::builder(Arc::clone(installed.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(installed.registry()),
            installed
                .env(SUPERUSER_ID, Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .resolver(erp_security::resolver::Resolver::for_registry(installed.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}

#[test]
fn ai_base_window_actions_compile_and_open_cleanly() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let db = TestDb::create().await.unwrap();
                    let sys = system(&db);
                    let init = Box::pin(
                        sys.initialize(InitOptions::new().with_skip_auto_install(true)),
                    )
                    .await
                    .unwrap();
                    assert!(init.data_notices.is_empty(), "{:?}", init.data_notices);
                    let env = sys.system_env(Context::new());
                    let installed = Box::pin(sys.button_immediate_install(
                        &env,
                        &[
                            ModuleName::new("web").unwrap(),
                            ModuleName::new("ai_base").unwrap(),
                        ],
                    ))
                    .await
                    .unwrap();
                    assert!(installed.post_commit.is_clean(), "{:?}", installed.post_commit);
                    assert!(installed.data_notices.is_empty(), "{:?}", installed.data_notices);

                    let mut setup = installed.orm.begin(SUPERUSER_ID).await.unwrap();
                    let xid = |row: &erp_orm::Row| -> i64 { row.get(0) };
                    let company = xid(
                        &setup
                            .query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='main_company'", &[])
                            .await
                            .unwrap()[0],
                    );
                    let g_sys = xid(
                        &setup
                            .query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_system'", &[])
                            .await
                            .unwrap()[0],
                    );
                    let g_user = xid(
                        &setup
                            .query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='group_user'", &[])
                            .await
                            .unwrap()[0],
                    );
                    let g_impl = xid(
                        &setup
                            .query("SELECT res_id::int8 FROM ir_model_data WHERE module='ai_base' AND name='group_implementation'", &[])
                            .await
                            .unwrap()[0],
                    );
                    let uid = setup
                        .env()
                        .records("res.users")
                        .unwrap()
                        .create(
                            &mut setup,
                            vec![
                                RawVals::new()
                                    .set_cell("name", Cell::Str("AI Views Admin".into()))
                                    .set_cell("login", Cell::Str("ai-views-admin".into()))
                                    .set_cell("company_id", Cell::Id(company))
                                    .set_commands(
                                        "company_ids",
                                        [Command::set([RecId::from_real(company).unwrap()])],
                                    )
                                    .set_commands(
                                        "group_ids",
                                        [Command::set([
                                            RecId::from_real(g_sys).unwrap(),
                                            RecId::from_real(g_user).unwrap(),
                                            RecId::from_real(g_impl).unwrap(),
                                        ])],
                                    ),
                            ],
                        )
                        .await
                        .unwrap()
                        .ids()[0];
                    setup.commit().await.unwrap();

                    let p_orm = policy_orm(&db, &installed.orm).await;
                    let mut p_tx = p_orm
                        .begin_with(
                            UserId::new(uid).unwrap(),
                            Context::new().with_company_id(company),
                        )
                        .await
                        .unwrap();
                    let actions = p_tx
                        .query(
                            "SELECT module || '.' || name, res_id::int8 FROM ir_model_data \
                             WHERE module = 'ai_base' AND model = 'ir.actions.act_window' \
                             ORDER BY name",
                            &[],
                        )
                        .await
                        .unwrap();
                    assert!(actions.len() >= 3, "expected ai_base window actions");
                    let act_window = p_tx.env().records("ir.actions.act_window").unwrap();
                    for row in actions {
                        let xmlid: String = row.get(0);
                        let id: i64 = row.get(1);
                        let _: erp_sdk::Value = act_window
                            .browse([id])
                            .unwrap()
                            .call(
                                &mut p_tx,
                                "erp_load_web_action_views",
                                (
                                    1u64,
                                    Option::<erp_sdk::ValueMap>::None,
                                    Option::<bool>::None,
                                ),
                            )
                            .await
                            .unwrap_or_else(|e| panic!("ai_base window action {xmlid} failed to open: {e}"));
                    }
                    p_tx.rollback().await.unwrap();
                    drop(p_tx);
                    drop(p_orm);
                    drop(installed);
                    drop(init);
                    drop(sys);
                    db.cleanup().await.unwrap();
                });
        })
        .unwrap()
        .join()
        .unwrap();
}
