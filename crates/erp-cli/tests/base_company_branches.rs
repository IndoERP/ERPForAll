//! Installed company branch acceptance. Native RPC uses authenticated principals and shipped policy.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use erp_fields::{Cell, RecId};
use erp_modules::{AddonIndex, InitOptions, ModuleSystem};
use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcService};
use erp_security::auth::{Auth, ClientAddr, CredentialOptions, HashPool, LoginRequest};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

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
async fn installed(db: &TestDb) -> (ModuleSystem, Orm) {
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
        // Full catalogue discovery validates the native UoM uninstall hook too.
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .build()
        .unwrap();
    let loaded = system.initialize(InitOptions::new()).await.unwrap();
    // a database that serves audited calls carries the audit table; the
    // publication step installs it in a real deployment, this fixture does it directly.
    {
        let mut audit_tx = loaded
            .orm
            .begin_opts(
                erp_orm::BeginOptions::new(SUPERUSER_ID)
                    .isolation(erp_orm::Isolation::ReadCommitted)
                    .on_primary(),
            )
            .await
            .unwrap();
        audit_tx.install_audit_schema().await.unwrap();
        audit_tx.commit().await.unwrap();
    }
    assert!(loaded.post_commit.is_clean());
    let source = PolicySource::build(Arc::clone(loaded.orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let published = loaded
        .orm
        .env(SUPERUSER_ID, erp_orm::Context::new())
        .generation_attachment::<erp_views::PublishedViewTypes>()
        .unwrap();
    let orm = Orm::builder(Arc::clone(loaded.orm.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .resolver(Resolver::for_registry(loaded.orm.registry()).unwrap())
        .policy(provider)
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(loaded.orm.registry()),
            published,
        ))
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    (system, orm)
}
async fn xid(db: &TestDb, name: &str) -> i64 {
    db.client()
        .await
        .unwrap()
        .query_one(
            "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name=$1",
            &[&name],
        )
        .await
        .unwrap()
        .get(0)
}
async fn principal(auth: &Auth, login: &str) -> erp_orm::Principal {
    let session = auth
        .authenticate(LoginRequest::new(
            login,
            "company-branch-fixture-password",
            ClientAddr::InProcess("company-branch-test"),
        ))
        .await
        .unwrap();
    let mut begun = auth
        .begin_session(session.session().secret(), CredentialOptions::new())
        .await
        .unwrap();
    let principal = begun.tx.env().principal().clone();
    begun.tx.rollback().await.unwrap();
    principal
}
#[test]
fn installed_branch_action_defaults_permissions_and_membership() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(acceptance());
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn acceptance() {
    let db = TestDb::create().await.unwrap();
    let (_system, orm) = installed(&db).await;
    let main = xid(&db, "main_company").await;
    let partner = xid(&db, "main_partner").await;
    let admin_group = xid(&db, "group_system").await;
    let user_group = xid(&db, "group_user").await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let main_company = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([main])
        .unwrap();
    assert_eq!(
        main_company.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );
    assert_eq!(
        main_company.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Null
    );
    assert_eq!(
        main_company.cell(&mut tx, "partner_id").await.unwrap(),
        Cell::Id(partner)
    );
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM res_company_lifecycle_fence WHERE id=1",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
    let branch = tx
        .env()
        .records("res.company")
        .unwrap()
        .with_context([("default_parent_id", erp_sdk::Value::Int(main))])
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Installed Branch".into()))],
        )
        .await
        .unwrap();
    let branch_id = branch.ids()[0];
    let currency = main_company.cell(&mut tx, "currency_id").await.unwrap();
    assert_eq!(branch.cell(&mut tx, "currency_id").await.unwrap(), currency);
    let users = tx.env().records("res.users").unwrap();
    let mut ordinary = 0;
    for (login, group) in [
        ("branches-admin", admin_group),
        ("branches-user", user_group),
    ] {
        let user = users
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(login.into()))
                        .set_cell("login", Cell::Str(login.into()))
                        .set_cell(
                            "password",
                            Cell::Str("company-branch-fixture-password".into()),
                        )
                        .set_cell("company_id", Cell::Id(main))
                        .set_commands(
                            "company_ids",
                            [Command::Set(vec![
                                RecId::from_real(main).unwrap(),
                                RecId::from_real(branch_id).unwrap(),
                            ])],
                        )
                        .set_commands(
                            "group_ids",
                            [Command::Set(vec![RecId::from_real(group).unwrap()])],
                        ),
                ],
            )
            .await
            .unwrap();
        if login == "branches-user" {
            ordinary = user.ids()[0];
        }
    }
    tx.commit().await.unwrap();
    drop(tx);
    let auth = Auth::new(
        orm.clone(),
        erp_security::auth::AuthConfig::DEFAULT,
        HashPool::global(),
    )
    .await
    .unwrap();
    let admin = principal(&auth, "branches-admin").await;
    let user = principal(&auth, "branches-user").await;
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    let call = async |who: &erp_orm::Principal, path: &str, body: Value| {
        let response = service
            .call(RpcCall::json2(
                path,
                serde_json::to_vec(&body).unwrap(),
                Caller::Principal(who.clone()),
            ))
            .await;
        let json: Value = serde_json::from_slice(response.body()).unwrap();
        (response.status(), json)
    };
    for who in [&admin, &user] {
        let (status, action) = call(
            who,
            "/json/2/res.company/action_all_company_branches",
            json!({"ids":[main]}),
        )
        .await;
        assert_eq!(status, 200, "{action}");
        assert_eq!(
            action["context"],
            json!({"default_parent_id":main,"active_test":false})
        );
        assert_eq!(action["domain"], json!([["parent_id", "=", main]]));
        let (status, result) = call(
            who,
            "/json/2/res.company/erp_normalize_action_result",
            json!({"wire_major":1,"result":action}),
        )
        .await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["result"]["kind"], "current_window");
        assert_eq!(result["result"]["context"]["active_test"], false);
    }
    let (status, error) = call(
        &user,
        "/json/2/res.company/write",
        json!({"ids":[branch_id],"vals":{"parent_id":main}}),
    )
    .await;
    assert_eq!(status, 403, "{error}");
    assert_eq!(error["name"], "erp.exceptions.AccessError");
    let mut denial_tx = orm
        .begin(erp_orm::UserId::new(ordinary).unwrap())
        .await
        .unwrap();
    let typed_denial = denial_tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([branch_id])
        .unwrap()
        .write(
            &mut denial_tx,
            RawVals::new().set_cell("parent_id", Cell::Id(main)),
        )
        .await
        .unwrap_err();
    assert_eq!(typed_denial.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(error["message"], typed_denial.message());
    denial_tx.rollback().await.unwrap();
    drop(denial_tx);
    let (status, error) = call(
        &admin,
        "/json/2/res.company/write",
        json!({"ids":[branch_id],"vals":{"parent_id":main}}),
    )
    .await;
    assert_eq!(status, 422, "{error}");
    assert_eq!(error["name"], "erp.exceptions.UserError");
    assert_eq!(error["message"], "The company hierarchy cannot be changed.");
    // A separate node resolves memberships from PostgreSQL, before and after an archive commit.
    let source = PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let other = Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .resolver(Resolver::for_registry(orm.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .build()
        .unwrap();
    let mut before = other
        .begin(erp_orm::UserId::new(ordinary).unwrap())
        .await
        .unwrap();
    assert!(
        before
            .env()
            .companies()
            .unwrap()
            .iter()
            .any(|id| id.get() == branch_id)
    );
    before.rollback().await.unwrap();
    drop(before);
    // The writing transaction refreshes membership, while rollback must not publish it.
    let mut rolled_back = orm
        .begin(erp_orm::UserId::new(ordinary).unwrap())
        .await
        .unwrap();
    let actor = rolled_back.env().sudo(true);
    actor
        .records("res.company")
        .unwrap()
        .browse([branch_id])
        .unwrap()
        .action_archive(&mut rolled_back)
        .await
        .unwrap();
    let refreshed = rolled_back
        .env()
        .with_context_replaced(erp_orm::Context::new());
    let revoked = rolled_back.companies(&refreshed).await.unwrap_err();
    assert_eq!(revoked.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        revoked.message(),
        "One of the selected companies does not exist or is not one of yours."
    );
    let selected = refreshed.with_context([(
        "allowed_company_ids",
        erp_sdk::Value::list([erp_sdk::Value::Int(main)]),
    )]);
    assert_eq!(
        rolled_back
            .companies(&selected)
            .await
            .unwrap()
            .iter()
            .map(|id| id.get())
            .collect::<Vec<_>>(),
        vec![main]
    );
    rolled_back.rollback().await.unwrap();
    drop(rolled_back);
    let mut retained = other
        .begin(erp_orm::UserId::new(ordinary).unwrap())
        .await
        .unwrap();
    assert!(
        retained
            .env()
            .companies()
            .unwrap()
            .iter()
            .any(|id| id.get() == branch_id)
    );
    retained.rollback().await.unwrap();
    drop(retained);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("res.company")
        .unwrap()
        .browse([branch_id])
        .unwrap()
        .action_archive(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut after = other
        .begin(erp_orm::UserId::new(ordinary).unwrap())
        .await
        .unwrap();
    assert!(
        !after
            .env()
            .companies()
            .unwrap()
            .iter()
            .any(|id| id.get() == branch_id)
    );
    after.rollback().await.unwrap();
    drop(service);
    drop(auth);
    drop(other);
    drop(orm);
    drop(_system);
    db.cleanup().await.unwrap();
}

#[test]
fn current_schema_upgrade_validates_legacy_currency_and_rolls_back() {
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
                    let (system, orm) = installed(&db).await;
                    let main = xid(&db, "main_company").await;
                    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
                    let branch = tx
                        .env()
                        .records("res.company")
                        .unwrap()
                        .create(
                            &mut tx,
                            vec![
                                RawVals::new()
                                    .set_cell("name", Cell::Str("Legacy Branch".into()))
                                    .set_cell("parent_id", Cell::Id(main)),
                            ],
                        )
                        .await
                        .unwrap()
                        .ids()[0];
                    tx.commit().await.unwrap();
                    let client = db.client().await.unwrap();
                    // Explicitly a CURRENT-schema fixture: use the already-merged base 1.4 marker and
                    // inject damage to prove later company migration admission, not an old-schema install.
                    client
                        .execute(
                            "UPDATE ir_module_module SET latest_version='1.4.0' WHERE name='base'",
                            &[],
                        )
                        .await
                        .unwrap();
                    let previous_currency: i64 = client
                        .query_one(
                            "SELECT currency_id FROM res_company WHERE id=$1",
                            &[&branch],
                        )
                        .await
                        .unwrap()
                        .get(0);
                    let other_currency: i64 = client
                        .query_one(
                            "SELECT id FROM res_currency WHERE id<>$1 ORDER BY id LIMIT 1",
                            &[&previous_currency],
                        )
                        .await
                        .unwrap()
                        .get(0);
                    client
                        .execute(
                            "UPDATE res_company SET currency_id=$1, active=NULL WHERE id=$2",
                            &[&other_currency, &branch],
                        )
                        .await
                        .unwrap();
                    let env = system.system_env(erp_sdk::Context::new());
                    let base = erp_modules::manifest::ModuleName::new("base").unwrap();
                    let error = system
                        .button_immediate_upgrade(&env, std::slice::from_ref(&base))
                        .await
                        .unwrap_err();
                    fn migration_source(error: erp_modules::Error) -> erp_fields::Error {
                        match error {
                            erp_modules::Error::StepFailed { source, .. } => migration_source(*source),
                            erp_modules::Error::MigrationFailed { source, .. } => *source,
                            erp_modules::Error::Orm(source) => source,
                            other => panic!("unexpected migration wrapper: {other:?}"),
                        }
                    }
                    let inner = migration_source(error);
                    assert_eq!(inner.kind(), erp_fields::ErrorKind::Validation);
                    assert_eq!(inner.message(), format!("base 1.5 company hierarchy/currency validation failed for company {branch}; repair parent paths and branch currencies before upgrading."));
                    let row = client
                        .query_one(
                            "SELECT currency_id, active FROM res_company WHERE id=$1",
                            &[&branch],
                        )
                        .await
                        .unwrap();
                    assert_eq!(row.get::<_, i64>(0), other_currency);
                    assert_eq!(row.get::<_, Option<bool>>(1), None);
                    client
                        .execute(
                            "UPDATE res_company SET currency_id=$1 WHERE id=$2",
                            &[&previous_currency, &branch],
                        )
                        .await
                        .unwrap();
                    let result = system
                        .button_immediate_upgrade(&env, std::slice::from_ref(&base))
                        .await
                        .unwrap();
                    assert!(result.post_commit.is_clean());
                    assert!(client.query_one("SELECT active FROM res_company WHERE id=$1", &[&branch]).await.unwrap().get::<_, bool>(0));
                    assert!(
                        system
                            .button_immediate_upgrade(&env, std::slice::from_ref(&base))
                            .await
                            .unwrap()
                            .post_commit
                            .is_clean()
                    );
                    drop(client);
                    drop(orm);
                    drop(system);
                    db.cleanup().await.unwrap();
                });
        })
        .unwrap()
        .join()
        .unwrap();
}
