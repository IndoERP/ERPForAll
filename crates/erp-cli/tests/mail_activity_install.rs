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

fn system(db: &TestDb) -> ModuleSystem {
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
    ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        // Complete discovery validates named hooks even for uninstalled addons.
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_uom::ADDON,
        ])
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap()
}

/// A database with `base` initialized and `mail` installed; the ORM of the installed registry.
async fn installed() -> (TestDb, ModuleSystem, Orm) {
    let db = TestDb::create().await.unwrap();
    let system = system(&db);
    let init = Box::pin(system.initialize(InitOptions::new().with_skip_auto_install(true)))
        .await
        .unwrap();
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
