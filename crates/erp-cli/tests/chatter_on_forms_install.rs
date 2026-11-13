//! Integration tests for chatter on forms:
//! `res.company`, `res.users`, `product.template`, and `product.product`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use erp_fields::{Cell, RecId, Value};
use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
use erp_orm::rpc::{Exposure, RpcArgs};
use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID, Transaction, UserId};
use erp_sdk::Domain;
use erp_testkit::TestDb;

fn repo_addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

fn system(db: &TestDb) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(repo_addons());
    let native_artifact_identities = index
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
        .native_artifact_identities(native_artifact_identities)
        .code([
            &erp_addon_uom::ADDON,
            &erp_addon_base::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_product_mail::ADDON,
            &erp_addon_web::ADDON,
        ])
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .build()
        .unwrap()
}

fn big_stack(body: impl std::future::Future<Output = ()> + Send + 'static) {
    thread::Builder::new()
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

async fn ensure_admin_user(orm: &Orm) -> i64 {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let existing = tx
        .query(
            "SELECT id FROM res_users WHERE login = 'test_admin' LIMIT 1",
            &[],
        )
        .await
        .unwrap();
    if let Some(row) = existing.first() {
        let id: i64 = row.get(0);
        tx.rollback().await.unwrap();
        return id;
    }

    let admin_group_rows = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name IN ('group_system', 'group_erp_manager') AND model = 'res.groups'",
            &[],
        )
        .await
        .unwrap();
    let group_ids: Vec<RecId> = admin_group_rows
        .iter()
        .map(|row| RecId::from_real(row.get::<_, i64>(0)).unwrap())
        .collect();

    let company_rows = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = 'main_company' AND model = 'res.company'",
            &[],
        )
        .await
        .unwrap();
    let main_company: i64 = company_rows[0].get(0);

    let users = tx.env().records("res.users").unwrap();
    let admin_user = users
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Test Admin".into()))
                    .set_cell("login", Cell::Str("test_admin".into()))
                    .set_cell("company_id", Cell::Id(main_company))
                    .set_commands(
                        "company_ids",
                        [Command::Set(vec![RecId::from_real(main_company).unwrap()])],
                    )
                    .set_commands("group_ids", [Command::Set(group_ids)]),
            ],
        )
        .await
        .unwrap();
    let admin_uid = admin_user.ids()[0];
    tx.commit().await.unwrap();
    admin_uid
}

async fn get_views(orm: &Orm, db: &TestDb, model: &str, types: &[&str]) -> serde_json::Value {
    let admin_uid = ensure_admin_user(orm).await;
    let source =
        erp_security::source::PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
            .await
            .unwrap();
    let published_views = orm
        .env(SUPERUSER_ID, erp_orm::Context::new())
        .generation_attachment::<erp_views::PublishedViewTypes>()
        .unwrap();
    let runtime_orm = Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(orm.registry()),
            published_views,
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(erp_security::resolver::Resolver::for_registry(orm.registry()).unwrap())
        .policy(Arc::new(source) as Arc<dyn erp_orm::PolicyProvider>)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();

    let mut tx = runtime_orm
        .begin(UserId::new(admin_uid).unwrap())
        .await
        .unwrap();
    let env = tx.env().clone();
    let views = types
        .iter()
        .map(|t| Value::list([Value::False, Value::str(*t)]))
        .collect::<Vec<_>>();
    let described = erp_addon_web::metadata_test_support::test_get_views(
        &env,
        &mut tx,
        model,
        views,
        32 * 1024 * 1024,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    serde_json::to_value(described).unwrap()
}

async fn ensure_superuser_author(tx: &mut Transaction) {
    let partner_rows = tx
        .query("SELECT id FROM res_partner LIMIT 1", &[])
        .await
        .unwrap();
    let partner_id = if let Some(row) = partner_rows.first() {
        row.get::<_, i64>(0)
    } else {
        let created = tx
            .query(
                "INSERT INTO res_partner (name, active, partner_share) VALUES ('Superuser', true, false) RETURNING id",
                &[],
            )
            .await
            .unwrap();
        created[0].get::<_, i64>(0)
    };
    tx.execute(
        "UPDATE res_users SET partner_id = $2, active = true, share = false WHERE id = $1",
        &[&1i64, &partner_id],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
}

async fn post_thread(tx: &mut Transaction, model: &str, id: i64, body: &str) -> i64 {
    ensure_superuser_author(tx).await;
    let records = tx.env().records(model).unwrap().browse(vec![id]).unwrap();
    let Exposure::Exposed(method) = tx
        .exposure()
        .get(records.model_id(), "thread_post")
        .unwrap_or_else(|| panic!("thread_post must be exposed on {model}"))
        .clone()
    else {
        panic!("thread_post is exposed on {model}")
    };
    let args = erp_fields::ValueMap::from_iter([(
        "post".into(),
        Value::Map(Arc::new(erp_fields::ValueMap::from_iter([(
            "body".into(),
            Value::str(body),
        )]))),
    )]);
    let value = method
        .call(tx, records, RpcArgs::new(args))
        .await
        .unwrap_or_else(|e| panic!("{model} thread_post failed: {e:?}"));
    match value {
        Value::Map(map) => match map.get("message_id") {
            Some(Value::Int(mid)) => *mid,
            other => panic!("expected message_id in thread_post answer, got {other:?}"),
        },
        other => panic!("expected Map answer from thread_post, got {other:?}"),
    }
}

#[test]
fn mail_install_serves_chatter_and_thread_post_on_company_and_users() {
    big_stack(async move {
        let db = TestDb::create().await.unwrap();

        // 1. Initialize with base only (skip auto-install so mail is not yet installed)
        let system_before = system(&db);
        let init = system_before
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap();
        let orm_before = init.orm;

        // Forms before mail must not carry chatter node
        for model in ["res.company", "res.users"] {
            let before = get_views(&orm_before, &db, model, &["form"]).await;
            assert!(
                !before["views"][0]["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|node| node["kind"] == "chatter"),
                "{model} form before mail must not carry chatter node"
            );
        }

        // 2. Install mail
        let system = system(&db);
        let env = system.system_env(erp_orm::Context::new());
        let outcome = system
            .button_immediate_install(&env, &[ModuleName::new("mail").unwrap()])
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean());
        let orm_after = outcome.orm;

        // 3. Confirm forms served to client carry chatter node
        for model in ["res.company", "res.users"] {
            let after = get_views(&orm_after, &db, model, &["form"]).await;
            let views_after = after["views"].as_array().unwrap();
            assert_eq!(views_after.len(), 1);
            let form_after = &views_after[0];
            let chatter = form_after["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["kind"] == "chatter")
                .unwrap_or_else(|| {
                    panic!("{model} form arch served to client must carry chatter node")
                });
            assert_eq!(chatter["kind"], "chatter");
        }

        // 4. Confirm models inherit mail.thread and thread_post works
        let mut tx = orm_after.begin(SUPERUSER_ID).await.unwrap();

        let company_records = tx.env().records("res.company").unwrap();
        assert!(
            company_records.spec().inherits("mail.thread"),
            "res.company must inherit mail.thread after mail install"
        );
        let comp_id = company_records
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .rec_ids()[0]
            .get();
        let comp_msg_id = post_thread(&mut tx, "res.company", comp_id, "Note on company").await;
        assert!(comp_msg_id > 0, "thread_post on res.company must succeed");

        let user_records = tx.env().records("res.users").unwrap();
        assert!(
            user_records.spec().inherits("mail.thread"),
            "res.users must inherit mail.thread after mail install"
        );
        let user_id = user_records
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .rec_ids()[0]
            .get();
        let user_msg_id = post_thread(&mut tx, "res.users", user_id, "Note on user").await;
        assert!(user_msg_id > 0, "thread_post on res.users must succeed");

        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

#[test]
fn mail_install_serves_chatter_and_thread_post_on_product_forms() {
    big_stack(async move {
        let db = TestDb::create().await.unwrap();

        // 1. Initialize system and install product (without mail)
        let system_init = system(&db);
        let _init = system_init
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap();
        let env_init = system_init.system_env(erp_orm::Context::new());
        let outcome_prod = system_init
            .button_immediate_install(&env_init, &[ModuleName::new("product").unwrap()])
            .await
            .unwrap();
        assert!(outcome_prod.post_commit.is_clean());
        let orm_prod = outcome_prod.orm;

        // Forms before mail must not carry chatter node
        for model in ["product.template", "product.product"] {
            let before = get_views(&orm_prod, &db, model, &["form"]).await;
            assert!(
                !before["views"][0]["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|node| node["kind"] == "chatter"),
                "{model} form before mail must not carry chatter node"
            );
        }

        // 2. Install mail (which triggers auto_install of product_mail)
        let system_mail = system(&db);
        let env_mail = system_mail.system_env(erp_orm::Context::new());
        let outcome_mail = system_mail
            .button_immediate_install(&env_mail, &[ModuleName::new("mail").unwrap()])
            .await
            .unwrap();
        assert!(outcome_mail.post_commit.is_clean());
        let orm_after = outcome_mail.orm;

        // 3. Confirm product forms served to client carry chatter node
        for model in ["product.template", "product.product"] {
            let after = get_views(&orm_after, &db, model, &["form"]).await;
            let views_after = after["views"].as_array().unwrap();
            assert_eq!(views_after.len(), 1);
            let form_after = &views_after[0];
            let chatter = form_after["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["kind"] == "chatter")
                .unwrap_or_else(|| {
                    panic!("{model} form arch served to client must carry chatter node")
                });
            assert_eq!(chatter["kind"], "chatter");
        }

        // 4. Confirm product models inherit mail.thread and thread_post works
        let mut tx = orm_after.begin(SUPERUSER_ID).await.unwrap();

        let tmpl_records = tx.env().records("product.template").unwrap();
        assert!(
            tmpl_records.spec().inherits("mail.thread"),
            "product.template must inherit mail.thread after product_mail install"
        );
        let tmpl = tmpl_records
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Test Template".into()))
                        .set_cell("type", Cell::Str("consu".into())),
                ],
            )
            .await
            .unwrap();
        let tmpl_id = tmpl.rec_ids()[0].get();
        let tmpl_msg_id =
            post_thread(&mut tx, "product.template", tmpl_id, "Template message").await;
        assert!(
            tmpl_msg_id > 0,
            "thread_post on product.template must succeed"
        );

        let prod_records = tx.env().records("product.product").unwrap();
        assert!(
            prod_records.spec().inherits("mail.thread"),
            "product.product must inherit mail.thread after product_mail install"
        );
        let prod_id = prod_records
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .rec_ids()[0]
            .get();
        let prod_msg_id = post_thread(&mut tx, "product.product", prod_id, "Variant message").await;
        assert!(
            prod_msg_id > 0,
            "thread_post on product.product must succeed"
        );

        tx.rollback().await.unwrap();
        db.cleanup().await.unwrap();
    });
}

/// (every thread model of the installed registry is a model of the store
/// schema, so the web client's `hold` of a chatter record is accepted; a new thread model that its
/// addon forgets to declare in a store schema part fails here)
#[test]
fn every_thread_model_is_a_model_of_the_store_schema() {
    big_stack(async move {
        let db = TestDb::create().await.unwrap();
        let system = system(&db);
        let _init = system
            .initialize(InitOptions::new().with_skip_auto_install(true))
            .await
            .unwrap();
        let env = system.system_env(erp_orm::Context::new());
        let outcome = system
            .button_immediate_install(
                &env,
                &[
                    ModuleName::new("product").unwrap(),
                    ModuleName::new("mail").unwrap(),
                ],
            )
            .await
            .unwrap();
        assert!(outcome.post_commit.is_clean());
        let schema = erp_addon_mail::store::schema().unwrap();
        let threads: Vec<&str> = outcome
            .orm
            .registry()
            .models()
            .iter()
            .filter(|m| m.inherits("mail.thread") && !m.is_abstract())
            .map(|m| m.name)
            .collect();
        for expected in [
            "res.users",
            "res.company",
            "res.partner",
            "product.template",
            "product.product",
        ] {
            assert!(threads.contains(&expected), "{expected} is not a thread");
        }
        for model in threads {
            assert!(
                schema.model(model).is_some(),
                "{model} inherits mail.thread but is not a model of the store schema: add it to thread_schema in addons/mail/src/store.rs"
            );
        }
        db.cleanup().await.unwrap();
    });
}
