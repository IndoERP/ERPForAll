//! Installing `mail` through the module system with its real data files: the default activity
//! types exist after install (AGENTS.md "Default data for every model") and the activity access
//! rows are stored as shipped.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_sdk::prelude::{Cell, Command, RawVals, RecId};
use erp_sdk::{Context, Operation, Orm, SUPERUSER_ID};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_testkit::TestDb;

fn addons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}

/// The module system, with the `web` addon (and its view types) when `with_web`.
fn system_with(db: &TestDb, with_web: bool) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons_dir());
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
    let builder = ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        // Complete discovery validates named hooks even for uninstalled addons.
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables());
    if with_web {
        builder
            .view_types(erp_addon_web::view_type_contributions())
            .code([&erp_addon_web::ADDON])
            .build()
            .unwrap()
    } else {
        builder.build().unwrap()
    }
}

/// A database with `base` initialized and `mail` installed; the ORM of the installed registry.
async fn installed() -> (TestDb, ModuleSystem, Orm) {
    installed_with(false).await
}

/// [`install`] on a 16 MiB thread of its own: initializing `base` and installing `mail` loads
/// every data file, a chain that needs more than the 2 MiB test thread in an unoptimized build
/// (as in `contacts_install.rs`). The access and RPC steps each test checks after it stay on the
/// default test thread. The test thread, waiting in `Runtime::block_on`, drives the runtime's IO
/// and timers for the install thread's `Handle::block_on`.
async fn installed_with(with_web: bool) -> (TestDb, ModuleSystem, Orm) {
    let handle = tokio::runtime::Handle::current();
    let (done, outcome) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let installed = handle.block_on(install(with_web));
            assert!(done.send(installed).is_ok(), "the test stopped waiting");
        })
        .unwrap();
    match outcome.await {
        Ok(installed) => installed,
        // the install thread panicked: its own panic is the failure
        Err(_) => std::panic::resume_unwind(thread.join().unwrap_err()),
    }
}

async fn install(with_web: bool) -> (TestDb, ModuleSystem, Orm) {
    let db = TestDb::create().await.unwrap();
    let system = system_with(&db, with_web);
    // `web` installs by itself (auto_install); the other fixture keeps the install minimal
    let options = if with_web {
        InitOptions::new()
    } else {
        InitOptions::new().with_skip_auto_install(true)
    };
    let init = Box::pin(system.initialize(options)).await.unwrap();
    let env = init.orm.env(SUPERUSER_ID, Context::new());
    let outcome =
        Box::pin(system.button_immediate_install(&env, &[ModuleName::new("mail").unwrap()]))
            .await
            .unwrap();
    assert!(outcome.post_commit.is_clean());
    let orm = outcome.orm;
    (db, system, orm)
}

/// The activity types `mail` ships, by external id: (name, summary, icon, delay, sequence,
/// category, active).
type TypeRow = (
    String,
    Option<String>,
    Option<String>,
    i32,
    i32,
    String,
    bool,
);

#[tokio::test]
async fn install_creates_the_default_activity_types() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT d.name::text, t.name->>'en_US', t.summary->>'en_US', t.icon::text, \
             COALESCE(t.delay_count, 0), t.sequence, COALESCE(t.category, 'default'), t.active, \
             d.noupdate FROM ir_model_data d JOIN mail_activity_type t ON t.id = d.res_id \
             WHERE d.module = 'mail' AND d.model = 'mail.activity.type' ORDER BY d.name",
            &[],
        )
        .await
        .unwrap();
    let found: Vec<(String, TypeRow)> = rows
        .iter()
        .map(|row| {
            assert!(row.get::<_, bool>(8), "seed is noupdate");
            (
                row.get(0),
                (
                    row.get(1),
                    row.get(2),
                    row.get(3),
                    row.get(4),
                    row.get(5),
                    row.get(6),
                    row.get(7),
                ),
            )
        })
        .collect();
    let row =
        |name: &str, summary: Option<&str>, icon: &str, delay, sequence, category: &str, active| {
            (
                name.to_owned(),
                summary.map(str::to_owned),
                Some(icon.to_owned()),
                delay,
                sequence,
                category.to_owned(),
                active,
            )
        };
    let expected: Vec<(String, TypeRow)> = vec![
        (
            "mail_activity_data_call".to_owned(),
            row("Call", Some("Call"), "phone", 2, 6, "phonecall", true),
        ),
        (
            "mail_activity_data_email".to_owned(),
            row("Email", Some("Email"), "mail", 0, 3, "default", true),
        ),
        (
            "mail_activity_data_meeting".to_owned(),
            row(
                "Meeting",
                Some("Meeting"),
                "calendar_today",
                0,
                9,
                "default",
                true,
            ),
        ),
        (
            "mail_activity_data_todo".to_owned(),
            row("To-Do", Some("To-Do"), "check", 5, 2, "default", true),
        ),
        (
            "mail_activity_data_upload_document".to_owned(),
            row(
                "Document",
                Some("Document"),
                "upload",
                5,
                25,
                "upload_file",
                true,
            ),
        ),
        (
            "mail_activity_data_warning".to_owned(),
            row("Exception", None, "warning", 0, 99, "default", false),
        ),
    ];
    assert_eq!(found, expected);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn activity_access_rows_are_stored_as_shipped() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT m.model::text, g.name, a.operation, COALESCE(a.domain, '') FROM ir_access a \
             JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
             JOIN ir_model m ON m.id = a.model_id \
             WHERE m.model IN ('mail.activity', 'mail.activity.type') AND g.module = 'base' \
             ORDER BY m.model, g.name, a.operation",
            &[],
        )
        .await
        .unwrap();
    let found: Vec<(String, String, String, String)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    let read = "['|', ('user_id', '=', user.id), ('res_id', 'activity_thread_access', 'read')]";
    let own = "['|', ('user_id', '=', user.id), ('create_uid', '=', user.id)]";
    let doc_write = "[('res_id', 'activity_thread_access', 'write')]";
    let doc_unlink = "[('res_id', 'activity_thread_access', 'write')]";
    let expected: Vec<(String, String, String, String)> = [
        ("mail.activity", "group_system", "crud", ""),
        ("mail.activity", "group_user", "c", ""),
        ("mail.activity", "group_user", "d", doc_unlink),
        ("mail.activity", "group_user", "r", read),
        ("mail.activity", "group_user", "u", doc_write),
        ("mail.activity", "group_user", "ud", own),
        ("mail.activity.type", "group_system", "crud", ""),
        ("mail.activity.type", "group_user", "r", ""),
    ]
    .into_iter()
    .map(|(m, g, o, d)| (m.to_owned(), g.to_owned(), o.to_owned(), d.to_owned()))
    .collect();
    assert_eq!(found, expected);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

async fn xmlid(tx: &mut erp_sdk::Transaction, module: &str, name: &str, model: &str) -> i64 {
    tx.query(
        "SELECT res_id FROM ir_model_data WHERE module = $1 AND name = $2 AND model = $3",
        &[&module, &name, &model],
    )
    .await
    .unwrap()[0]
        .get(0)
}

/// A user (with its partner) in the given groups, through the ORM.
async fn make_user(tx: &mut erp_sdk::Transaction, name: &str, company: i64, groups: &[i64]) -> i64 {
    let partner = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("company_id", Cell::Id(company)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let set = |ids: &[i64]| Command::set(ids.iter().map(|id| RecId::from_real(*id).unwrap()));
    tx.env()
        .records("res.users")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("login", Cell::Str(name.into()))
                    .set_cell("partner_id", Cell::Id(partner))
                    .set_cell("company_id", Cell::Id(company))
                    .set_commands("company_ids", [set(&[company])])
                    .set_commands("group_ids", [set(groups)]),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}

#[tokio::test]
async fn activity_rows_or_assignee_creator_and_record_access() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let user_group = xmlid(&mut tx, "base", "group_user", "res.groups").await;
    let admin_group = xmlid(&mut tx, "base", "group_system", "res.groups").await;
    let company = xmlid(&mut tx, "base", "main_company", "res.company").await;
    // the ORM refuses a database without an administrator user, so one comes first
    make_user(&mut tx, "act_admin", company, &[user_group, admin_group]).await;
    let one = make_user(&mut tx, "act_one", company, &[user_group]).await;
    let two = make_user(&mut tx, "act_two", company, &[user_group]).await;
    // a contact: employees read contacts, they do not write them
    let doc: i64 = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("doc".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.flush().await.unwrap();
    // (assignee, creator) of three activities on the contact
    let mut ids = Vec::new();
    for (assignee, creator) in [(one, two), (two, one), (two, two)] {
        let row = tx
            .query(
                "INSERT INTO mail_activity (res_model, res_id, date_deadline, user_id, create_uid, \
                 active, automated) VALUES ('res.partner', $1, CURRENT_DATE, $2, $3, true, false) \
                 RETURNING id",
                &[&doc, &assignee, &creator],
            )
            .await
            .unwrap();
        ids.push(row[0].get::<_, i64>(0));
    }
    tx.commit().await.unwrap();
    // an ORM whose policy is the shipped rows of the database
    let source = PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    let real = erp_orm::Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(orm.registry()).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap();
    let mut tx = real
        .begin(erp_orm::UserId::new(one).unwrap())
        .await
        .unwrap();
    let employee = tx.env();
    let all = employee
        .records("mail.activity")
        .unwrap()
        .browse(ids.clone())
        .unwrap();
    // readable: the contact is readable, so all three are
    let readable = all
        .filtered_access(&mut tx, Operation::Read)
        .await
        .unwrap()
        .ids();
    assert_eq!(readable, ids);
    // assigned to the employee, created by the employee; not the activity of two strangers
    let writable = all
        .filtered_access(&mut tx, Operation::Write)
        .await
        .unwrap()
        .ids();
    assert_eq!(writable, vec![ids[0], ids[1]]);
    let deletable = all
        .filtered_access(&mut tx, Operation::Unlink)
        .await
        .unwrap()
        .ids();
    assert_eq!(deletable, vec![ids[0], ids[1]]);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(real);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

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

/// An ORM whose policy is the shipped rows of the database.
async fn runtime(db: &TestDb, orm: &Orm) -> Orm {
    let source = PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(orm.registry()).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}

/// As [`runtime`], with the view-type generation the `web` addon's metadata methods read.
async fn runtime_web(db: &TestDb, orm: &Orm) -> Orm {
    let source = PolicySource::build(Arc::clone(orm.registry()), db.pool().clone())
        .await
        .unwrap();
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(orm.registry()), db.pool().clone())
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(orm.registry()),
            orm.env(SUPERUSER_ID, Context::new())
                .generation_attachment::<erp_views::PublishedViewTypes>()
                .unwrap(),
        ))
        .require_core_tables(erp_security::auth::core_tables())
        .resolver(Resolver::for_registry(orm.registry()).unwrap())
        .policy(provider)
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .build()
        .unwrap()
}

async fn principal_of(orm: &Orm, uid: i64) -> erp_orm::Principal {
    let mut tx = orm.begin(erp_orm::UserId::new(uid).unwrap()).await.unwrap();
    let principal = tx.env().principal().clone();
    tx.rollback().await.unwrap();
    principal
}

async fn call_menu(service: &RpcService, principal: erp_orm::Principal) -> erp_rpc::RpcResponse {
    service
        .call(RpcCall::json2(
            "/json/2/mail.activity/erp_activity_groups",
            b"{}".to_vec(),
            Caller::Principal(principal),
        ))
        .await
}

#[tokio::test]
async fn only_an_internal_user_gets_the_activity_menu() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let user_group = xmlid(&mut tx, "base", "group_user", "res.groups").await;
    let admin_group = xmlid(&mut tx, "base", "group_system", "res.groups").await;
    let portal_group = xmlid(&mut tx, "base", "group_portal", "res.groups").await;
    let public_group = xmlid(&mut tx, "base", "group_public", "res.groups").await;
    let company = xmlid(&mut tx, "base", "main_company", "res.company").await;
    make_user(&mut tx, "menu_admin", company, &[user_group, admin_group]).await;
    let portal = make_user(&mut tx, "menu_portal", company, &[portal_group]).await;
    let public = make_user(&mut tx, "menu_public", company, &[public_group]).await;
    // an activity of each, so that a refusal is not an empty answer
    let doc: i64 = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("doc".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.flush().await.unwrap();
    for user in [portal, public] {
        tx.query(
            "INSERT INTO mail_activity (res_model, res_id, date_deadline, user_id, create_uid, \
             active, automated) VALUES ('res.partner', $1, CURRENT_DATE, $2, $2, true, false)",
            &[&doc, &user],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let real = runtime(&db, &orm).await;
    let service = RpcService::new(real.clone(), Host, RpcConfig::default());
    for (label, uid) in [("portal", portal), ("public", public)] {
        let mut tx = real
            .begin(erp_orm::UserId::new(uid).unwrap())
            .await
            .unwrap();
        let env = tx.env();
        let direct = env
            .records("mail.activity")
            .unwrap()
            .check_access(&mut tx, Operation::Read)
            .await
            .unwrap_err();
        assert_eq!(direct.kind(), erp_fields::ErrorKind::Access, "{label}");
        assert!(
            direct
                .message()
                .starts_with("You are not allowed to access 'Activity' (mail.activity) records."),
            "{label}: {}",
            direct.message()
        );
        let typed = erp_addon_mail::activity_groups(&env, &mut tx)
            .await
            .unwrap_err();
        assert_eq!(typed.kind(), erp_fields::ErrorKind::Access, "{label}");
        assert_eq!(typed.message(), direct.message(), "{label}");
        tx.rollback().await.unwrap();
        let response = call_menu(&service, principal_of(&real, uid).await).await;
        assert_eq!(
            response.status(),
            403,
            "{label}: {}",
            String::from_utf8_lossy(response.body())
        );
        let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(body["name"], "erp.exceptions.AccessError", "{label}");
        assert_eq!(body["message"], direct.message(), "{label}");
        assert!(body.get("groups").is_none(), "{label}: no counts");
    }
    drop(service);
    drop(real);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

/// The menu through JSON-2 for an internal user, with the real access rows: the typed answer.
#[tokio::test]
async fn json2_answers_the_menu_for_an_internal_user() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let user_group = xmlid(&mut tx, "base", "group_user", "res.groups").await;
    let admin_group = xmlid(&mut tx, "base", "group_system", "res.groups").await;
    let company = xmlid(&mut tx, "base", "main_company", "res.company").await;
    make_user(&mut tx, "menu_admin", company, &[user_group, admin_group]).await;
    let employee = make_user(&mut tx, "menu_employee", company, &[user_group]).await;
    let doc: i64 = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("doc".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.flush().await.unwrap();
    let activity: i64 = tx
        .query(
            "INSERT INTO mail_activity (res_model, res_id, date_deadline, user_id, create_uid, \
             active, automated) VALUES ('res.partner', $1, CURRENT_DATE + 3650, $2, $2, true, \
             false) RETURNING id",
            &[&doc, &employee],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();
    let real = runtime(&db, &orm).await;
    let service = RpcService::new(real.clone(), Host, RpcConfig::default());
    let response = call_menu(&service, principal_of(&real, employee).await).await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    // the same menu as the typed function gives, field by field
    let mut tx = real
        .begin(erp_orm::UserId::new(employee).unwrap())
        .await
        .unwrap();
    let env = tx.env();
    let menu = erp_addon_mail::activity_groups(&env, &mut tx)
        .await
        .unwrap();
    let label = env
        .registry()
        .model("res.partner")
        .unwrap()
        .description
        .clone();
    tx.rollback().await.unwrap();
    let mut keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["activity_counter", "groups"]);
    assert_eq!(body["activity_counter"], menu.counter());
    let groups = body["groups"].as_array().unwrap();
    assert_eq!(groups.len(), menu.groups().len());
    assert_eq!(groups.len(), 1);
    let (wire, typed) = (&groups[0], &menu.groups()[0]);
    let mut keys: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "activity_ids",
            "addon",
            "domain",
            "model",
            "name",
            "overdue_count",
            "planned_count",
            "today_count",
            "total_count",
        ]
    );
    assert_eq!(wire["model"], "res.partner");
    assert_eq!(wire["name"], label.as_str());
    assert_eq!(wire["addon"], typed.addon().unwrap());
    assert_eq!(wire["overdue_count"], typed.overdue_count());
    assert_eq!(wire["today_count"], typed.today_count());
    assert_eq!(wire["planned_count"], typed.planned_count());
    assert_eq!(wire["total_count"], typed.total_count());
    assert_eq!(wire["activity_ids"], serde_json::json!([activity]));
    assert_eq!(
        wire["domain"],
        serde_json::json!([["active", "in", [true, false]]])
    );
    drop(service);
    drop(real);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

async fn call_open(
    service: &RpcService,
    principal: erp_orm::Principal,
    model: &str,
    state: &str,
) -> erp_rpc::RpcResponse {
    service
        .call(RpcCall::json2(
            "/json/2/mail.activity/erp_activity_group_action",
            serde_json::to_vec(&serde_json::json!({"model": model, "state": state})).unwrap(),
            Caller::Principal(principal),
        ))
        .await
}

/// `addons/mail/tests/activity.rs`)
#[tokio::test]
async fn only_an_internal_user_opens_an_activity_group() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let portal_group = xmlid(&mut tx, "base", "group_portal", "res.groups").await;
    let public_group = xmlid(&mut tx, "base", "group_public", "res.groups").await;
    let company = xmlid(&mut tx, "base", "main_company", "res.company").await;
    let admin_group = xmlid(&mut tx, "base", "group_system", "res.groups").await;
    let user_group_for_admin = xmlid(&mut tx, "base", "group_user", "res.groups").await;
    make_user(
        &mut tx,
        "open_admin",
        company,
        &[user_group_for_admin, admin_group],
    )
    .await;
    let portal = make_user(&mut tx, "open_portal", company, &[portal_group]).await;
    let public = make_user(&mut tx, "open_public", company, &[public_group]).await;
    tx.commit().await.unwrap();
    let real = runtime(&db, &orm).await;
    let service = RpcService::new(real.clone(), Host, RpcConfig::default());
    for (label, uid) in [("portal", portal), ("public", public)] {
        let mut tx = real
            .begin(erp_orm::UserId::new(uid).unwrap())
            .await
            .unwrap();
        let env = tx.env();
        let direct = env
            .records("mail.activity")
            .unwrap()
            .check_access(&mut tx, Operation::Read)
            .await
            .unwrap_err();
        tx.rollback().await.unwrap();
        // an unknown state is refused with Access too: the right is checked before any input
        for state in ["all", "planned", "bogus"] {
            let response = call_open(
                &service,
                principal_of(&real, uid).await,
                "res.partner",
                state,
            )
            .await;
            assert_eq!(
                response.status(),
                403,
                "{label} {state}: {}",
                String::from_utf8_lossy(response.body())
            );
            let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
            assert_eq!(body["name"], "erp.exceptions.AccessError", "{label}");
            assert_eq!(body["message"], direct.message(), "{label}");
        }
    }
    drop(service);
    drop(real);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

/// model with `active`)
#[tokio::test]
async fn json2_answers_the_open_action_for_an_internal_user() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let user_group = xmlid(&mut tx, "base", "group_user", "res.groups").await;
    let company = xmlid(&mut tx, "base", "main_company", "res.company").await;
    let admin_group = xmlid(&mut tx, "base", "group_system", "res.groups").await;
    let user_group_for_admin = xmlid(&mut tx, "base", "group_user", "res.groups").await;
    make_user(
        &mut tx,
        "open_admin",
        company,
        &[user_group_for_admin, admin_group],
    )
    .await;
    let employee = make_user(&mut tx, "open_employee", company, &[user_group]).await;
    let doc: i64 = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("doc".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.flush().await.unwrap();
    let activity: i64 = tx
        .query(
            "INSERT INTO mail_activity (res_model, res_id, date_deadline, user_id, create_uid, \
             active, automated) VALUES ('res.partner', $1, CURRENT_DATE - 2, $2, $2, true, \
             false) RETURNING id",
            &[&doc, &employee],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();
    let real = runtime(&db, &orm).await;
    let service = RpcService::new(real.clone(), Host, RpcConfig::default());
    let response = call_open(
        &service,
        principal_of(&real, employee).await,
        "res.partner",
        "overdue",
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    let mut tx = real
        .begin(erp_orm::UserId::new(employee).unwrap())
        .await
        .unwrap();
    let env = tx.env();
    let menu = erp_addon_mail::activity_groups(&env, &mut tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let typed = &menu.groups()[0];
    assert_eq!(typed.model(), "res.partner");
    assert_eq!(typed.open_ids(erp_addon_mail::OpenState::Overdue), [doc]);
    assert_eq!(typed.activity_ids(), [activity]);
    assert_eq!(body["type"], "ir.actions.act_window");
    assert_eq!(body["name"], typed.name());
    assert_eq!(body["res_model"], "res.partner");
    assert_eq!(body["target"], "current");
    assert_eq!(
        body["views"],
        serde_json::json!([[false, "list"], [false, "form"]])
    );
    assert_eq!(body["context"], serde_json::json!({}));
    assert_eq!(
        body["domain"],
        serde_json::json!([["id", "in", [doc]], ["active", "in", [true, false]]])
    );
    // a state with no record is a count of zero, an empty id list and not an error
    let planned = call_open(
        &service,
        principal_of(&real, employee).await,
        "res.partner",
        "planned",
    )
    .await;
    assert_eq!(planned.status(), 200);
    let planned: serde_json::Value = serde_json::from_slice(planned.body()).unwrap();
    assert_eq!(planned["domain"][0], serde_json::json!(["id", "in", []]));
    drop(service);
    drop(real);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn my_activities_action_ships_with_mail_and_loads_for_an_internal_user() {
    let (db, system, orm) = Box::pin(installed_with(true)).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let user_group = xmlid(&mut tx, "base", "group_user", "res.groups").await;
    let company = xmlid(&mut tx, "base", "main_company", "res.company").await;
    let admin_group = xmlid(&mut tx, "base", "group_system", "res.groups").await;
    let user_group_for_admin = xmlid(&mut tx, "base", "group_user", "res.groups").await;
    make_user(
        &mut tx,
        "open_admin",
        company,
        &[user_group_for_admin, admin_group],
    )
    .await;
    let employee = make_user(&mut tx, "my_activities_user", company, &[user_group]).await;
    let action = xmlid(
        &mut tx,
        "mail",
        "mail_activity_action_my",
        "ir.actions.act_window",
    )
    .await;
    let list_view = xmlid(&mut tx, "mail", "mail_activity_view_list", "ir.ui.view").await;
    let form_view = xmlid(&mut tx, "mail", "mail_activity_view_form", "ir.ui.view").await;
    let stored = tx
        .query(
            "SELECT a.name, w.res_model, w.view_mode, w.domain FROM ir_act_window w \
             JOIN ir_actions a ON a.id = w.id WHERE w.id = $1",
            &[&action],
        )
        .await
        .unwrap();
    let name: String = stored[0].get(0);
    let model: String = stored[0].get(1);
    let view_mode: String = stored[0].get(2);
    let domain: String = stored[0].get(3);
    assert_eq!(
        (name.as_str(), model.as_str(), view_mode.as_str()),
        ("My Activities", "mail.activity", "list,form")
    );
    assert_eq!(domain, "[('user_id', '=', uid)]");
    let rows = tx
        .query(
            "SELECT view_mode, view_id, sequence FROM ir_act_window_view \
             WHERE act_window_id = $1 ORDER BY sequence",
            &[&action],
        )
        .await
        .unwrap();
    let rows: Vec<(String, i64, i32)> = rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    assert_eq!(
        rows,
        [
            ("list".to_owned(), list_view, 10),
            ("form".to_owned(), form_view, 20)
        ]
    );
    tx.commit().await.unwrap();
    let real = runtime_web(&db, &orm).await;
    let service = RpcService::new(real.clone(), Host, RpcConfig::default());
    let response = service
        .call(RpcCall::json2(
            "/json/2/ir.actions.act_window/erp_load_web_action",
            serde_json::to_vec(&serde_json::json!({"ids": [action]})).unwrap(),
            Caller::Principal(principal_of(&real, employee).await),
        ))
        .await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["name"], "My Activities");
    assert_eq!(body["model"], "mail.activity");
    assert_eq!(body["target"], "current");
    assert_eq!(
        body["views"],
        serde_json::json!([
            {"id": list_view, "type": "list"},
            {"id": form_view, "type": "form"}
        ])
    );
    // the loaded domain is the stored source as the bounded expression document the client
    // evaluates with its own `uid`
    let expected = erp_sdk::expr::wire::WireDocument::from_expr(
        &erp_sdk::expr::parse("[('user_id', '=', uid)]").unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["domain"],
        serde_json::to_value(&expected).unwrap(),
        "{body}"
    );
    drop(service);
    drop(real);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}
