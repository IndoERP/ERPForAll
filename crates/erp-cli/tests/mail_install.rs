//! Installing `mail` through the module system with its real data files: the default subtypes
//! exist after install, default subtype resolution over them, and the protection of the master
//! subtypes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::path::{Path, PathBuf};

use erp_addon_mail::DefaultSubtypes;
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::{AddonIndex, ModuleName};
use erp_sdk::prelude::*;
use erp_sdk::{Context, ErrorKind, Orm, SUPERUSER_ID};
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

/// The id of external id `mail.<name>`.
async fn xmlid(tx: &mut Transaction, name: &str) -> i64 {
    let rows = tx
        .query(
            "SELECT res_id::int8 FROM ir_model_data WHERE module = 'mail' AND name = $1::text \
             AND model = 'mail.message.subtype'",
            &[&name],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "external id mail.{name}");
    rows[0].get(0)
}

/// is its partner when it has one; the company scope of contacts and users treats the system user as
/// internal)
#[tokio::test]
async fn install_leaves_the_system_user_internal() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT u.share, p.partner_share FROM res_users u \
               LEFT JOIN res_partner p ON p.id = u.partner_id WHERE u.id = $1",
            &[&SUPERUSER_ID.get()],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the system user has a row");
    assert!(
        !rows[0].get::<_, bool>(0),
        "res_users.share of the system user"
    );
    // a partner the install gave it (when it has one) is internal too
    if let Some(partner_share) = rows[0].get::<_, Option<bool>>(1) {
        assert!(!partner_share, "res_partner.partner_share of its partner");
    }
    tx.rollback().await.unwrap();
    drop((system, orm));
    db.cleanup().await.unwrap();
}

/// The installed subtype rows by external id: (name, default, internal, sequence,
/// track_recipients, res_model).
type SubtypeRow = (String, bool, bool, i32, bool, Option<String>);

#[tokio::test]
async fn install_creates_the_default_subtypes() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT d.name::text, s.name->>'en_US', COALESCE(s.\"default\", false), \
             COALESCE(s.internal, false), s.sequence, COALESCE(s.track_recipients, false), \
             s.res_model::text, d.noupdate \
             FROM ir_model_data d JOIN mail_message_subtype s ON s.id = d.res_id \
             WHERE d.module = 'mail' AND d.model = 'mail.message.subtype' ORDER BY d.name",
            &[],
        )
        .await
        .unwrap();
    let found: Vec<(String, SubtypeRow)> = rows
        .iter()
        .map(|row| {
            assert!(row.get::<_, bool>(7), "seed is noupdate");
            (
                row.get(0),
                (
                    row.get(1),
                    row.get(2),
                    row.get(3),
                    row.get(4),
                    row.get(5),
                    row.get(6),
                ),
            )
        })
        .collect();
    let expected: Vec<(String, SubtypeRow)> = [
        ("mt_activities", ("Activities", false, true, 100, false)),
        ("mt_comment", ("Messages", true, false, 0, true)),
        (
            "mt_important_notification",
            ("Important Notification", false, true, 1, false),
        ),
        ("mt_note", ("Notes", false, true, 90, true)),
    ]
    .into_iter()
    .map(|(xmlid, (name, default, internal, sequence, track))| {
        (
            xmlid.to_owned(),
            (name.to_owned(), default, internal, sequence, track, None),
        )
    })
    .collect();
    assert_eq!(found, expected);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

/// The rows are checked as stored after a real install.
#[tokio::test]
async fn subtype_access_rows_limit_non_internal_users_to_external_subtypes() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT g.name, a.operation, COALESCE(a.domain, '') FROM ir_access a \
             JOIN ir_model_data g ON g.model = 'res.groups' AND g.res_id = a.group_id \
             JOIN ir_model m ON m.id = a.model_id \
             WHERE m.model = 'mail.message.subtype' AND g.module = 'base' \
             ORDER BY g.name",
            &[],
        )
        .await
        .unwrap();
    let found: Vec<(String, String, String)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    let external = "[('internal', '=', False)]";
    let expected: Vec<(String, String, String)> = [
        ("group_portal", "r", external),
        ("group_public", "r", external),
        ("group_system", "crud", ""),
        ("group_user", "r", ""),
    ]
    .into_iter()
    .map(|(g, o, d)| (g.to_owned(), o.to_owned(), d.to_owned()))
    .collect();
    assert_eq!(found, expected);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn subtypes_are_ordered_by_sequence_then_id() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let subtypes = tx.env().records("mail.message.subtype").unwrap();
    let mut created = Vec::new();
    for (name, sequence) in [("Late", 95), ("Tie A", 90), ("Early", 1), ("Tie B", 90)] {
        let record = subtypes
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("sequence", Cell::Int(sequence))
                        .set_cell("res_model", Cell::Str("test.order.model".into())),
                ],
            )
            .await
            .unwrap();
        created.push(record.ids()[0]);
    }
    let note = xmlid(&mut tx, "mt_note").await;
    let only_ours = Domain::condition("res_model", Operator::Eq, "test.order.model").unwrap()
        | Domain::condition("id", Operator::Eq, note).unwrap();
    let ordered = subtypes.search(&mut tx, &only_ours).await.unwrap().ids();
    // Early (1), then the two at 90 by id: mt_note (seeded first), Tie A, Tie B; then Late (95)
    assert_eq!(
        ordered,
        vec![created[2], note, created[1], created[3], created[0]]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

/// `follow` picks that list for a share partner, tested in `addons/mail/tests/followers.rs`)
///
/// Over the installed subtypes plus a public and an internal default subtype of `sale.order` and
/// one of another model.
#[tokio::test]
async fn default_subtypes_split_generic_and_model_subtypes() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let subtypes = tx.env().records("mail.message.subtype").unwrap();
    let make =
        async |tx: &mut Transaction, name: &str, model: &str, internal: bool, sequence: i32| {
            subtypes
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("res_model", Cell::Str(model.into()))
                            .set_cell("internal", Cell::Bool(internal))
                            .set_cell("sequence", Cell::Int(sequence)),
                    ],
                )
                .await
                .unwrap()
                .ids()[0]
        };
    let confirmed = make(&mut tx, "Order Confirmed", "sale.order", false, 5).await;
    let margin = make(&mut tx, "Margin Changed", "sale.order", true, 6).await;
    let elsewhere = make(&mut tx, "Ticket Closed", "helpdesk.ticket", false, 2).await;
    let comment = xmlid(&mut tx, "mt_comment").await;
    let note = xmlid(&mut tx, "mt_note").await;
    let defaults: DefaultSubtypes = subtypes
        .call(&mut tx, "default_subtypes", ("sale.order".to_owned(),))
        .await
        .unwrap();
    assert_eq!(defaults.all(), [comment, confirmed, margin]);
    assert_eq!(defaults.internal(), [margin]);
    assert_eq!(defaults.external(), [comment, confirmed]);
    assert!(!defaults.external().contains(&note));
    assert!(!defaults.external().contains(&margin));
    assert!(!defaults.all().contains(&elsewhere));
    let error = subtypes
        .call::<_, DefaultSubtypes>(&mut tx, "default_subtypes", (String::new(),))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "A model name of 1 to 256 bytes is required to resolve default subtypes."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn master_subtypes_cannot_be_deleted() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let subtypes = tx.env().records("mail.message.subtype").unwrap();
    let comment = xmlid(&mut tx, "mt_comment").await;
    let error = tx
        .savepoint(async |tx| subtypes.browse([comment])?.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Subtypes Messages cannot be deleted: other apps rely on them."
    );
    let note = xmlid(&mut tx, "mt_note").await;
    let activities = xmlid(&mut tx, "mt_activities").await;
    let error = tx
        .savepoint(async |tx| subtypes.browse([activities, note])?.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Subtypes Notes, Activities cannot be deleted: other apps rely on them."
    );
    assert_eq!(
        subtypes
            .browse([comment, note, activities])
            .unwrap()
            .exists(&mut tx)
            .await
            .unwrap()
            .ids()
            .len(),
        3
    );
    // a non-master subtype, the seeded mt_important_notification included, can be deleted
    let important = xmlid(&mut tx, "mt_important_notification").await;
    subtypes
        .browse([important])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert!(
        subtypes
            .browse([important])
            .unwrap()
            .exists(&mut tx)
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn master_subtypes_keep_their_configuration() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let subtypes = tx.env().records("mail.message.subtype").unwrap();
    let comment = xmlid(&mut tx, "mt_comment").await;
    let note = xmlid(&mut tx, "mt_note").await;
    for (id, vals, name) in [
        (
            comment,
            RawVals::new().set_cell("res_model", Cell::Str("res.partner".into())),
            "Messages",
        ),
        (
            comment,
            RawVals::new().set_cell("internal", Cell::Bool(true)),
            "Messages",
        ),
        (
            note,
            RawVals::new().set_cell("internal", Cell::Bool(false)),
            "Notes",
        ),
    ] {
        let error = tx
            .savepoint(async |tx| {
                subtypes.browse([id])?.write(tx, vals).await?;
                tx.flush().await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            format!(
                "Subtypes {name} cannot be changed this way: other apps rely on their configuration."
            )
        );
    }
    // the values they must keep, and other fields, are accepted
    subtypes
        .browse([comment, note])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("res_model", Cell::Null)
                .set_cell("sequence", Cell::Int(3)),
        )
        .await
        .unwrap();
    subtypes
        .browse([note])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("internal", Cell::Bool(true)),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        subtypes
            .browse([comment])
            .unwrap()
            .cell(&mut tx, "res_model")
            .await
            .unwrap(),
        Cell::Null
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

/// the shipped subtypes resolve, a post on a contact without subtype is a note by the superuser's
/// partner; the other post cases run on the stand-in in `addons/mail/tests/post.rs`)
#[tokio::test]
async fn a_post_after_a_real_install_is_a_note() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let note = xmlid(&mut tx, "mt_note").await;
    let partners = tx.env().records("res.partner").unwrap();
    let contact = partners
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Contact".into()))],
        )
        .await
        .unwrap();
    // the installed superuser row has no partner (found by this test: the post refuses to create
    // an authorless message), so it gets one for the post
    let refused = contact
        .call::<_, erp_addon_mail::Posted>(
            &mut tx,
            "message_post",
            (erp_addon_mail::PostRequest::new(
                erp_addon_mail::Body::Text("x".to_owned()),
            ),),
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.message(),
        "User 1 has no partner (no res.users row, or its partner_id is empty): a message needs an \
         author partner. The superuser's partner link comes with a later release."
    );
    let author = partners
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Author".into()))],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE res_users SET partner_id = $1::int8 WHERE id = 1",
        &[&author],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let posted: erp_addon_mail::Posted = contact
        .call(
            &mut tx,
            "message_post",
            (erp_addon_mail::PostRequest::new(
                erp_addon_mail::Body::Text("Hello <b>".to_owned()),
            ),),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let rows = tx
        .query(
            "SELECT subtype_id::int8, body, author_id::int8, model, res_id::int8 \
             FROM mail_message WHERE id = $1::int8",
            &[&posted.message_id],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i64>(0), note);
    assert_eq!(rows[0].get::<_, String>(1), "<p>Hello &lt;b&gt;</p>");
    assert_eq!(rows[0].get::<_, Option<i64>>(2), Some(author));
    assert_eq!(rows[0].get::<_, String>(3), "res.partner");
    assert_eq!(rows[0].get::<_, i64>(4), contact.ids()[0]);
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    drop(system);
    db.cleanup().await.unwrap();
}

/// `crates/erp-test-models/tests/mail_scheduled.rs`).
#[tokio::test]
async fn install_ships_no_scheduled_message_cron_or_user_access_row() {
    let (db, system, orm) = Box::pin(installed()).await;
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows: i64 = tx
        .query("SELECT count(*)::int8 FROM mail_scheduled_message", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(rows, 0);
    let crons: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM ir_cron c \
             JOIN ir_act_server s ON s.id = c.ir_actions_server_id \
             JOIN ir_model m ON m.id = s.model_id \
             WHERE m.model = 'mail.scheduled.message' OR s.code LIKE '%_post_messages_cron%'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(crons, 0);
    let access = tx
        .query(
            "SELECT g.module || '.' || g.name, a.operation::text \
             FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
             JOIN ir_model_data g ON g.res_id = a.group_id AND g.model = 'res.groups' \
             WHERE m.model = 'mail.scheduled.message' ORDER BY 1",
            &[],
        )
        .await
        .unwrap();
    let access: Vec<(String, String)> = access.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        access,
        vec![("base.group_system".to_owned(), "crud".to_owned())]
    );
    tx.rollback().await.unwrap();
    drop((db, system));
}
