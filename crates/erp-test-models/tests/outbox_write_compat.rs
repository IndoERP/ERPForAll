//! S7 compatibility acceptance for existing write hooks, versions, translations and delegation.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::{Arc, Mutex};

use erp_fields::{Cell, ErrorKind, OnDelete, Result, Value};
use erp_orm::{
    BeginOptions, BoxFuture, OperationIdentity, RawVals, SUPERUSER_ID, Transaction, WriteBatch,
    WriteHook,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_outbox_compat",
    depends: &["base"],
    exact_only: false,
};
const VERSION: &str = "test.outbox.compat.version";
const TRANSLATED: &str = "test.outbox.compat.translated";
const PARENT: &str = "test.outbox.compat.parent";
const CHILD: &str = "test.outbox.compat.child";
const EVENTS: &str = "\"erp_event\".\"ir_event_outbox\"";
static HOOKED: Mutex<Vec<WriteBatch>> = Mutex::new(Vec::new());

fn record_hook<'a>(_tx: &'a mut Transaction, batch: &'a WriteBatch) -> BoxFuture<'a, Result<()>> {
    let batch = batch.clone();
    Box::pin(async move {
        HOOKED
            .lock()
            .map_err(|_| erp_fields::Error::programming("outbox compatibility hook lock poisoned"))?
            .push(batch);
        Ok(())
    })
}

async fn begin(h: &Harness) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}

// A separate public execution identity makes substitution by the flusher's identity observable.
async fn writer_env(h: &Harness) -> erp_orm::Env {
    let mut origin = begin(h).await;
    let writer = origin.env().with_user(7).sudo(true);
    origin.rollback().await.unwrap();
    writer
}

async fn prepare(tx: &mut Transaction) {
    tx.install_event_schema().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES (7, 'compatibility writer', 'compatibility-writer', true)",
        &[],
    )
    .await
    .unwrap();
}

async fn write_events(h: &Harness) -> Vec<tokio_postgres::Row> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            &format!(
                "SELECT model, record_ids, field_names, subject, actor_kind, actor_id, delegating_user_id, \
                 operation_id::text FROM {EVENTS} WHERE kind = 'write' ORDER BY id"
            ),
            &[],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows
}

fn final_insert_count(tx: &Transaction) -> usize {
    tx.captured()
        .iter()
        .filter(|sql| sql.starts_with("INSERT INTO \"erp_event\".\"ir_event_outbox\""))
        .count()
}

async fn quantities(h: &Harness) -> Vec<(i64, i64, i64)> {
    let mut tx = h.begin().await.unwrap();
    let values = tx
        .query(
            "SELECT id, quantity::bigint, row_version FROM test_outbox_compat_version ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    tx.rollback().await.unwrap();
    values
}

#[tokio::test]
async fn hooks_versions_and_change_flags_share_the_update_and_stale_batch_rolls_back() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new(ADDON.name, VERSION)
            .table("test_outbox_compat_version")
            .description("Outbox compatibility version")
            .eventful(true)
            .row_version(true)
            .fields(const { &[FieldDecl::integer("quantity"), FieldDecl::char("note")] })])
        .write_hook(WriteHook::new(VERSION, &["quantity"], record_hook))
        .build()
        .await
        .unwrap();
    let mut seed = begin(&h).await;
    prepare(&mut seed).await;
    let created = seed
        .env()
        .records(VERSION)
        .unwrap()
        .create(
            &mut seed,
            vec![
                RawVals::new()
                    .set_cell("quantity", Cell::Int(10))
                    .set_cell("note", Cell::Str("unchanged".into()));
                2
            ],
        )
        .await
        .unwrap();
    let ids = created.rec_ids().to_vec();
    seed.commit().await.unwrap();
    drop(seed);
    assert_eq!(
        quantities(&h).await,
        vec![(ids[0].get(), 10, 1), (ids[1].get(), 10, 1)]
    );
    HOOKED.lock().unwrap().clear();

    let mut tx = begin(&h).await;
    let writer = writer_env(&h).await;
    assert_ne!(
        writer.operation_identity().unwrap().operation(),
        tx.env().operation_identity().unwrap().operation()
    );
    let operation = writer.operation_identity().unwrap().operation().to_string();
    let subject = writer.uid().to_string();
    let actor = tx.env().uid();
    let record = writer
        .records(VERSION)
        .unwrap()
        .browse([ids[0].get()])
        .unwrap();
    record
        .expect_versions(&mut tx, [(ids[0], 1)])
        .await
        .unwrap();
    tx.capture_statements(true);
    record
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("quantity", Cell::Int(20))
                .set_cell("note", Cell::Str("unchanged".into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(final_insert_count(&tx), 1);
    assert_eq!(
        tx.captured()
            .iter()
            .filter(|sql| sql.contains("RETURNING WITH")
                && sql.contains("__expected")
                && sql.contains("IS DISTINCT FROM"))
            .count(),
        1,
        "{:?}",
        tx.captured()
    );
    drop(tx);
    let batches = HOOKED.lock().unwrap().clone();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].model(), VERSION);
    assert_eq!(batches[0].fields(), &["quantity"]);
    assert_eq!(batches[0].rows().len(), 1);
    assert_eq!(batches[0].rows()[0].id(), ids[0]);
    assert_eq!(
        batches[0].rows()[0].values(),
        &[(Cell::Int(10), Cell::Int(20))]
    );
    let events = write_events(&h).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].get::<_, String>("model"), VERSION);
    assert_eq!(
        events[0].get::<_, serde_json::Value>("record_ids"),
        serde_json::json!([ids[0].get()])
    );
    assert_eq!(events[0].get::<_, Vec<String>>("field_names"), ["quantity"]);
    assert_eq!(events[0].get::<_, String>("subject"), subject);
    assert_eq!(events[0].get::<_, String>("actor_kind"), "system");
    assert_eq!(events[0].get::<_, i64>("actor_id"), actor);
    assert_eq!(
        events[0].get::<_, Option<i64>>("delegating_user_id"),
        Some(actor)
    );
    assert_eq!(events[0].get::<_, String>("operation_id"), operation);
    let before_stale = quantities(&h).await;
    assert_eq!(
        before_stale,
        vec![(ids[0].get(), 20, 2), (ids[1].get(), 10, 1)]
    );

    // A is stale while B still matches: the same UPDATE really writes B before the ORM detects
    // A's missing result. Catching that Conflict must not allow B or a write event to commit.
    let mut stale = begin(&h).await;
    let both = stale
        .env()
        .records(VERSION)
        .unwrap()
        .browse(ids.iter().map(|id| id.get()))
        .unwrap();
    both.expect_versions(&mut stale, ids.iter().map(|id| (*id, 1)))
        .await
        .unwrap();
    both.write(
        &mut stale,
        RawVals::new().set_cell("quantity", Cell::Int(30)),
    )
    .await
    .unwrap();
    let error = stale.flush().await.unwrap_err();
    let expected = format!(
        "Record 'Outbox compatibility version' ({VERSION}) {} was changed by someone else since you read it (version 1, now 2). Reload it and apply your change again.",
        ids[0]
    );
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(error.message(), expected);
    let pending_b: i64 = stale
        .query(
            "SELECT quantity::bigint FROM test_outbox_compat_version WHERE id = $1",
            &[&ids[1].get()],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        pending_b, 30,
        "the successful half of the UPDATE must really exist before rollback"
    );
    let commit = stale.commit().await.unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::Conflict);
    assert_eq!(commit.message(), expected);
    assert_eq!(
        quantities(&h).await,
        before_stale,
        "another connection sees no partial update"
    );
    stale.rollback().await.unwrap();
    drop(stale);
    assert_eq!(quantities(&h).await, before_stale);
    assert_eq!(write_events(&h).await.len(), 1);
    assert_eq!(
        HOOKED.lock().unwrap().len(),
        1,
        "incomplete UPDATE results must not dispatch a partial hook batch"
    );
    h.cleanup().await.unwrap();
}

async fn setup_languages(tx: &mut Transaction) {
    for (name, code, url) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        if tx
            .query("SELECT 1 FROM res_lang WHERE code = $1", &[&code])
            .await
            .unwrap()
            .is_empty()
        {
            let vals = RawVals::new()
                .set_cell("name", Cell::Str(name.into()))
                .set_cell("code", Cell::Str(code.into()))
                .set_cell("url_code", Cell::Str(url.into()))
                .set_cell("active", Cell::Bool(true))
                .set_cell("direction", Cell::Str("ltr".into()))
                .set_cell("date_format", Cell::Str("%m/%d/%Y".into()))
                .set_cell("time_format", Cell::Str("%H:%M:%S".into()))
                .set_cell("week_start", Cell::Str("7".into()))
                .set_cell("grouping", Cell::Str("[3,0]".into()))
                .set_cell("decimal_point", Cell::Str(".".into()))
                .set_cell("thousands_sep", Cell::Str(",".into()));
            tx.env()
                .records("res.lang")
                .unwrap()
                .create(tx, vec![vals])
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn translated_helper_merges_database_json_and_preserves_writer_for_actual_change_only() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .with_decls([ModelDecl::new(ADDON.name, TRANSLATED)
            .table("test_outbox_compat_translated")
            .eventful(true)
            .fields(const { &[FieldDecl::char("name").translate(true)] })])
        .build()
        .await
        .unwrap();
    let mut seed = begin(&h).await;
    prepare(&mut seed).await;
    setup_languages(&mut seed).await;
    let id = seed
        .env()
        .records(TRANSLATED)
        .unwrap()
        .create(
            &mut seed,
            vec![RawVals::new().set_cell(
                "name",
                Cell::Json(Arc::new(
                    serde_json::json!({"en_US": "Original", "fr_FR": "Ancien"}),
                )),
            )],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    seed.commit().await.unwrap();
    drop(seed);

    let mut tx = begin(&h).await;
    let writer = writer_env(&h).await;
    assert_ne!(
        writer.operation_identity().unwrap().operation(),
        tx.env().operation_identity().unwrap().operation()
    );
    let operation = writer.operation_identity().unwrap().operation().to_string();
    let subject = writer.uid().to_string();
    let actor = tx.env().uid();
    let record = writer
        .records(TRANSLATED)
        .unwrap()
        .browse([id.get()])
        .unwrap();
    tx.capture_statements(true);
    assert!(
        record
            .update_field_translations(&mut tx, "name", [("fr_FR", Value::str("Nouveau"))])
            .await
            .unwrap()
    );
    // Deliberately inject an untouched-language database change after cache staging, within
    // this transaction. This is a merge oracle, not a concurrency or raw-SQL capture claim.
    tx.execute("UPDATE test_outbox_compat_translated SET name = jsonb_set(name, '{en_US}', $1::jsonb) WHERE id = $2", &[&serde_json::json!("Database English"), &id.get()]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(final_insert_count(&tx), 1);
    assert!(
        tx.captured()
            .iter()
            .any(|sql| sql.contains("jsonb_strip_nulls(COALESCE(")
                && sql.contains("IS DISTINCT FROM")),
        "{:?}",
        tx.captured()
    );
    drop(tx);
    let mut check = h.begin().await.unwrap();
    let actual: serde_json::Value = check
        .query(
            "SELECT name FROM test_outbox_compat_translated WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        actual,
        serde_json::json!({"en_US": "Database English", "fr_FR": "Nouveau"})
    );
    check.rollback().await.unwrap();
    drop(check);
    let events = write_events(&h).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].get::<_, String>("model"), TRANSLATED);
    assert_eq!(
        events[0].get::<_, serde_json::Value>("record_ids"),
        serde_json::json!([id.get()])
    );
    assert_eq!(events[0].get::<_, Vec<String>>("field_names"), ["name"]);
    assert_eq!(events[0].get::<_, String>("subject"), subject);
    assert_eq!(events[0].get::<_, String>("actor_kind"), "system");
    assert_eq!(events[0].get::<_, i64>("actor_id"), actor);
    assert_eq!(events[0].get::<_, String>("operation_id"), operation);

    let mut noop = begin(&h).await;
    let record = noop
        .env()
        .with_user(7)
        .sudo(true)
        .records(TRANSLATED)
        .unwrap()
        .browse([id.get()])
        .unwrap();
    noop.capture_statements(true);
    assert!(
        !record
            .update_field_translations(&mut noop, "name", [("fr_FR", Value::str("Nouveau"))])
            .await
            .unwrap()
    );
    noop.commit().await.unwrap();
    assert_eq!(final_insert_count(&noop), 0);
    drop(noop);
    assert_eq!(write_events(&h).await.len(), 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn delegated_child_write_captures_parent_business_columns_only() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new(ADDON.name, PARENT)
                .table("test_outbox_compat_parent")
                .eventful(true)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new(ADDON.name, CHILD)
                .table("test_outbox_compat_child")
                .eventful(true)
                .inherits(&[(PARENT, "parent_id")])
                .fields(
                    const {
                        &[
                            FieldDecl::many2one("parent_id", PARENT)
                                .required(true)
                                .ondelete(OnDelete::Cascade),
                            FieldDecl::char("note"),
                        ]
                    },
                ),
        ])
        .build()
        .await
        .unwrap();
    let mut seed = begin(&h).await;
    prepare(&mut seed).await;
    let parent_id = seed
        .env()
        .records(PARENT)
        .unwrap()
        .create(
            &mut seed,
            vec![RawVals::new().set_cell("name", Cell::Str("Parent initial".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    let child_id = seed
        .env()
        .records(CHILD)
        .unwrap()
        .create(
            &mut seed,
            vec![
                RawVals::new()
                    .set_cell("parent_id", Cell::Id(parent_id.get()))
                    .set_cell("note", Cell::Str("Child unchanged".into())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    seed.commit().await.unwrap();
    drop(seed);
    assert!(write_events(&h).await.is_empty());

    let mut tx = begin(&h).await;
    let writer = writer_env(&h).await;
    assert_ne!(
        writer.operation_identity().unwrap().operation(),
        tx.env().operation_identity().unwrap().operation()
    );
    let operation = writer.operation_identity().unwrap().operation().to_string();
    let subject = writer.uid().to_string();
    let actor = tx.env().uid();
    tx.capture_statements(true);
    writer
        .records(CHILD)
        .unwrap()
        .browse([child_id.get()])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Parent updated".into())),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(final_insert_count(&tx), 1);
    drop(tx);
    let mut check = h.begin().await.unwrap();
    let rows = check.query("SELECT p.name, c.note, c.parent_id FROM test_outbox_compat_child c JOIN test_outbox_compat_parent p ON p.id = c.parent_id WHERE c.id = $1", &[&child_id.get()]).await.unwrap();
    assert_eq!(rows[0].get::<_, String>(0), "Parent updated");
    assert_eq!(rows[0].get::<_, String>(1), "Child unchanged");
    assert_eq!(rows[0].get::<_, i64>(2), parent_id.get());
    check.rollback().await.unwrap();
    drop(check);
    let events = write_events(&h).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].get::<_, String>("model"), PARENT);
    assert_eq!(
        events[0].get::<_, serde_json::Value>("record_ids"),
        serde_json::json!([parent_id.get()])
    );
    assert_eq!(events[0].get::<_, Vec<String>>("field_names"), ["name"]);
    assert_eq!(events[0].get::<_, String>("subject"), subject);
    assert_eq!(events[0].get::<_, String>("actor_kind"), "system");
    assert_eq!(events[0].get::<_, i64>("actor_id"), actor);
    assert_eq!(events[0].get::<_, String>("operation_id"), operation);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn denied_translation_after_dirty_write_keeps_transaction_usable() {
    for denial in 0..3 {
        let builder = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
            .with_decls([ModelDecl::new(ADDON.name, TRANSLATED)
                .table("test_outbox_compat_translated")
                .eventful(true)
                .fields(
                    const {
                        &[
                            FieldDecl::char("name").translate(true),
                            FieldDecl::integer("number"),
                        ]
                    },
                )]);
        let builder = match denial {
            0 => builder.policy(erp_testkit::FieldPolicy::new().deny(
                TRANSLATED,
                "name",
                erp_orm::FieldOp::Write,
            )),
            1 => builder.policy(erp_testkit::FieldPolicy::new().deny_model(TRANSLATED)),
            _ => builder.rules(TranslationRecordDenial),
        };
        let h = builder.build().await.unwrap();
        let mut seed = begin(&h).await;
        prepare(&mut seed).await;
        setup_languages(&mut seed).await;
        let id = seed
            .env()
            .records(TRANSLATED)
            .unwrap()
            .create(
                &mut seed,
                vec![RawVals::new().set_cell("name", Cell::Str("original".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        seed.commit().await.unwrap();
        drop(seed);
        let mut tx = begin(&h).await;
        let denied = tx
            .env()
            .with_user(7)
            .records(TRANSLATED)
            .unwrap()
            .browse([id])
            .unwrap();
        let baseline = denied
            .update_field_translations(&mut tx, "name", [("fr_FR", Value::str("refused"))])
            .await
            .unwrap_err();
        assert_eq!(baseline.kind(), ErrorKind::Access);
        assert!(!baseline.message().is_empty());
        tx.env()
            .records(TRANSLATED)
            .unwrap()
            .browse([id])
            .unwrap()
            .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(9)))
            .await
            .unwrap();
        tx.capture_statements(true);
        let error = denied
            .update_field_translations(&mut tx, "name", [("fr_FR", Value::str("refused"))])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), baseline.kind());
        assert_eq!(error.message(), baseline.message());
        assert!(
            !tx.captured().iter().any(|s| s.starts_with("UPDATE")),
            "{:?}",
            tx.captured()
        );
        tx.commit().await.unwrap();
        drop(tx);
        let mut check = h.begin().await.unwrap();
        let row=check.query("SELECT number,name->>'en_US',name->>'fr_FR' FROM test_outbox_compat_translated WHERE id=$1",&[&id]).await.unwrap();
        assert_eq!(row[0].get::<_, i32>(0), 9);
        assert_eq!(row[0].get::<_, String>(1), "original");
        assert_eq!(row[0].get::<_, Option<String>>(2), None);
        check.rollback().await.unwrap();
        drop(check);
        h.cleanup().await.unwrap();
    }
}

struct TranslationRecordDenial;
impl erp_testkit::TestRules for TranslationRecordDenial {
    fn check_model(
        &self,
        _: &erp_orm::PrincipalBinding<'_>,
        _: &erp_registry::ModelSpec,
        _: erp_orm::Operation,
    ) -> Result<()> {
        Ok(())
    }
    fn access_domain(
        &self,
        _: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        operation: erp_orm::Operation,
    ) -> Result<erp_domain::Domain> {
        Ok(
            if model.name == TRANSLATED && operation == erp_orm::Operation::Write {
                erp_domain::Domain::FALSE
            } else {
                erp_domain::Domain::TRUE
            },
        )
    }
}
