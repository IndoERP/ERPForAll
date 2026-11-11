//! The outbox event table in the dedicated `erp_event` schema (A2-D5, plan
//! S3): idempotent install, refusal of an existing object with the wrong shape, daily UTC partitions
//! independent of the session time zone, a missing partition failing the insert, and generic schema
//! sync / vacuum leaving the event namespace alone. Grants are S4, capture is S5 and later.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::ErrorKind;
use erp_orm::Transaction;
use erp_registry::{FieldDecl, ModelDecl, PartitionDecl, PartitionInterval, PartitionRetain};
use erp_test_models::base_users;
use erp_testkit::Harness;

const TABLE: &str = "\"erp_event\".\"ir_event_outbox\"";

async fn harness() -> Harness {
    // these tests start from an empty event namespace: the audit table has its own tests
    Harness::builder([&base_users::ADDON])
        .without_audit_store()
        .build()
        .await
        .unwrap()
}

/// The UTC dates of the window (today and the 7 days after), as `YYYYMMDD`.
async fn window(tx: &mut Transaction) -> Vec<String> {
    tx.query(
        "SELECT to_char((now() AT TIME ZONE 'UTC')::date + g, 'YYYYMMDD') \
         FROM generate_series(0, 7) g ORDER BY g",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<_, String>(0))
    .collect()
}

/// The attached partitions of the outbox table, by name.
async fn partitions(tx: &mut Transaction) -> Vec<String> {
    tx.query(
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE i.inhparent = 'erp_event.ir_event_outbox'::regclass ORDER BY 1",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<_, String>(0))
    .collect()
}

/// Insert the columns a writer supplies, leaving the generated ones to the database.
const INSERT: &str = "INSERT INTO \"erp_event\".\"ir_event_outbox\" (schema_version, kind, model, \
    record_ids, field_names, cascade_models, actor_id, company_ids, principal_kind, actor_kind, operation_id, tx_ordinal, subject) \
    VALUES (1, 'create', 'res.partner', '[1]', '{}', '{}', 1, '{1}', 'user', 'user', pg_catalog.uuidv7(), 1, '1')";

#[track_caller]
fn refused(error: &erp_fields::Error, code: &str) {
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert!(
        error.message().contains(&format!("({code})")),
        "expected refusal ({code}), got: {}",
        error.message()
    );
}

/// A fresh database gets the schema, a range-partitioned table by `create_date`, today's UTC
/// partition and the seven after it, and no default partition.
#[tokio::test]
async fn install_creates_schema_table_and_the_premade_window() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let report = tx.install_event_schema().await.unwrap();
    assert!(report.created_table);
    let expected: Vec<String> = window(&mut tx)
        .await
        .iter()
        .map(|d| format!("ir_event_outbox_p{d}"))
        .collect();
    assert_eq!(report.created_partitions, expected);
    assert!(report.existing_partitions.is_empty());
    assert_eq!(partitions(&mut tx).await, expected);
    let rows = tx
        .query(
            "SELECT relkind::text, pg_get_partkeydef(oid), pg_get_userbyid(relowner)::text = \
             current_user::text FROM pg_class WHERE oid = 'erp_event.ir_event_outbox'::regclass",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, String>(0), "p");
    assert_eq!(rows[0].get::<_, String>(1), "RANGE (create_date)");
    assert!(rows[0].get::<_, bool>(2));
    let default_partitions: i64 = tx
        .query(
            "SELECT count(*) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'erp_event.ir_event_outbox'::regclass \
             AND pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(default_partitions, 0);
    h.cleanup().await.unwrap();
}

/// A second install changes nothing, from the same transaction and from a later one.
#[tokio::test]
async fn install_is_idempotent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = tx.install_event_schema().await.unwrap();
    let again = tx.install_event_schema().await.unwrap();
    assert!(!again.created_table);
    assert!(again.created_partitions.is_empty());
    assert_eq!(again.existing_partitions, first.created_partitions);
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let later = tx.install_event_schema().await.unwrap();
    assert!(!later.created_table);
    assert!(later.created_partitions.is_empty());
    assert_eq!(later.existing_partitions, first.created_partitions);
    assert_eq!(
        tx.ensure_event_partitions().await.unwrap(),
        Vec::<String>::new()
    );
    h.cleanup().await.unwrap();
}

/// `id`, `event_id`, `xid` and `create_date` come from database defaults: an insert that supplies
/// none of them gets a sequence value, a version 7 uuid, the transaction's xid8 and its start time.
#[tokio::test]
async fn generated_columns_are_database_defaults() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.batch(INSERT).await.unwrap();
    tx.batch(INSERT).await.unwrap();
    let rows = tx
        .query(
            &format!(
                "SELECT id, substr(event_id::text, 15, 1), \
                 xid::text = pg_current_xact_id()::text, create_date = transaction_timestamp() \
                 FROM {TABLE} ORDER BY id"
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].get::<_, i64>(0) < rows[1].get::<_, i64>(0));
    for row in &rows {
        assert_eq!(row.get::<_, String>(1), "7");
        assert!(row.get::<_, bool>(2));
        assert!(row.get::<_, bool>(3));
    }
    h.cleanup().await.unwrap();
}

/// The partition an instant lands in and the names the install makes do not depend on the session
/// time zone: UTC days in every zone, including ones a day ahead of or behind UTC.
#[tokio::test]
async fn partition_assignment_is_independent_of_the_session_time_zone() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut baseline = None;
    for zone in [
        "UTC",
        "Pacific/Kiritimati",
        "America/Los_Angeles",
        "Asia/Kolkata",
    ] {
        tx.query("SELECT set_config('TimeZone', $1, true)", &[&zone])
            .await
            .unwrap();
        let report = tx.install_event_schema().await.unwrap();
        let names = if baseline.is_none() {
            report.created_partitions
        } else {
            assert!(report.created_partitions.is_empty(), "{zone}");
            report.existing_partitions
        };
        assert_eq!(*baseline.get_or_insert(names.clone()), names, "{zone}");
        // the same UTC instants, written in this zone's offset, land in the same partitions
        let rows = tx
            .query(
                "WITH d AS (SELECT (now() AT TIME ZONE 'UTC')::date + 2 AS day) SELECT \
                 to_char(day, 'YYYYMMDD'), \
                 to_char(day + 1, 'YYYYMMDD') FROM d",
                &[],
            )
            .await
            .unwrap();
        let (day, next): (String, String) = (rows[0].get(0), rows[0].get(1));
        for (instant, expected) in [
            (format!("{day} 00:00:00+00"), &day),
            (format!("{day} 23:59:59.999999+00"), &day),
            (format!("{next} 00:00:00+00"), &next),
            // 23:30 at UTC-11 is 10:30 UTC of the next day
            (format!("{day} 23:30:00-11"), &next),
            // 01:00 at UTC+14 is 11:00 UTC of the previous day
            (format!("{next} 01:00:00+14"), &day),
        ] {
            let sql = format!(
                "INSERT INTO {TABLE} (create_date, schema_version, kind, model, record_ids, \
                 field_names, cascade_models, actor_id, company_ids, principal_kind, actor_kind, operation_id, tx_ordinal, subject) VALUES ('{instant}'::timestamptz, 1, \
                 'create', 'res.partner', '[1]', '{{}}', '{{}}', 1, '{{1}}', 'user', 'user', pg_catalog.uuidv7(), 1, '1') \
                 RETURNING tableoid::regclass::text"
            );
            let row = tx.query(&sql, &[]).await.unwrap();
            assert_eq!(
                row[0].get::<_, String>(0),
                format!("erp_event.ir_event_outbox_p{expected}"),
                "{instant} in {zone}"
            );
        }
    }
    h.cleanup().await.unwrap();
}

/// With today's partition gone there is no default partition to catch the row: the insert fails.
#[tokio::test]
async fn missing_partition_fails_the_insert() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    let today = format!("ir_event_outbox_p{}", window(&mut tx).await[0]);
    tx.batch(&format!("DROP TABLE \"erp_event\".\"{today}\""))
        .await
        .unwrap();
    let error = tx.batch(INSERT).await.expect_err("no partition for today");
    assert!(
        error.message().contains("no partition of relation"),
        "{error:?}"
    );
    h.cleanup().await.unwrap();
}

/// The upkeep call recreates a lost partition of the window and nothing else.
#[tokio::test]
async fn upkeep_recreates_a_missing_partition() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    let days = window(&mut tx).await;
    let lost = format!("ir_event_outbox_p{}", days[3]);
    tx.batch(&format!("DROP TABLE \"erp_event\".\"{lost}\""))
        .await
        .unwrap();
    assert_eq!(tx.ensure_event_partitions().await.unwrap(), vec![lost]);
    assert_eq!(partitions(&mut tx).await.len(), 8);
    h.cleanup().await.unwrap();
}

/// Run `forge` on a fresh install, then install again: it must be refused with `code`.
async fn forged_after_install(forge: &str, code: &str) {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.batch(forge)
        .await
        .unwrap_or_else(|e| panic!("{forge}: {e:?}"));
    let error = tx
        .install_event_schema()
        .await
        .expect_err("a forged object must be refused");
    refused(&error, code);
    h.cleanup().await.unwrap();
}

/// An existing object of the wrong shape is refused by name, whatever was forged: a default that
/// calls another function (here a lookalike `uuidv7` earlier on the search path), a changed type,
/// an extra or missing column, a lost primary key, a default partition, a partition with another range.
#[tokio::test]
async fn forged_shapes_are_refused() {
    forged_after_install(
        "CREATE FUNCTION public.uuidv7() RETURNS uuid LANGUAGE sql AS 'SELECT gen_random_uuid()'; \
         ALTER TABLE erp_event.ir_event_outbox ALTER COLUMN event_id SET DEFAULT public.uuidv7()",
        "forged-default",
    )
    .await;
    forged_after_install(
        "ALTER TABLE erp_event.ir_event_outbox ALTER COLUMN create_date DROP DEFAULT",
        "forged-default",
    )
    .await;
    forged_after_install(
        "ALTER TABLE erp_event.ir_event_outbox ALTER COLUMN xid DROP DEFAULT, \
         ALTER COLUMN xid TYPE bigint USING 0",
        "forged-type",
    )
    .await;
    forged_after_install(
        "ALTER TABLE erp_event.ir_event_outbox ALTER COLUMN kind DROP NOT NULL",
        "forged-nullability",
    )
    .await;
    forged_after_install(
        "ALTER TABLE erp_event.ir_event_outbox ADD COLUMN extra text",
        "forged-column",
    )
    .await;
    forged_after_install(
        "ALTER TABLE erp_event.ir_event_outbox DROP COLUMN subject",
        "forged-column",
    )
    .await;
    forged_after_install(
        "ALTER TABLE erp_event.ir_event_outbox ADD CONSTRAINT forged CHECK (schema_version > 0)",
        "forged-constraint",
    )
    .await;
    forged_after_install(
        "CREATE TABLE erp_event.ir_event_outbox_pdefault PARTITION OF erp_event.ir_event_outbox DEFAULT",
        "default-partition",
    )
    .await;
    forged_after_install(
        "CREATE TABLE erp_event.ir_event_outbox_p20200101 PARTITION OF erp_event.ir_event_outbox \
         FOR VALUES FROM ('2020-01-01 00:00:00+00') TO ('2020-01-03 00:00:00+00')",
        "forged-partition",
    )
    .await;
    forged_after_install(
        "CREATE TABLE erp_event.stranger PARTITION OF erp_event.ir_event_outbox \
         FOR VALUES FROM ('2020-01-01 00:00:00+00') TO ('2020-01-02 00:00:00+00')",
        "forged-partition",
    )
    .await;
    forged_after_install(
        "DROP INDEX erp_event.ir_event_outbox_cursor_idx",
        "forged-index",
    )
    .await;
    forged_after_install(
        "ALTER SEQUENCE erp_event.ir_event_outbox_id_seq CYCLE",
        "forged-sequence",
    )
    .await;
    forged_after_install(
        "ALTER TABLE erp_event.ir_event_outbox ALTER COLUMN id SET DEFAULT 7",
        "forged-default",
    )
    .await;
}

/// A view, a plain table or a foreign object sitting where the event table belongs is refused, and
/// so is a view where a partition belongs; nothing is created around it.
#[tokio::test]
async fn forged_relations_are_refused() {
    for (setup, code) in [
        (
            "CREATE SCHEMA erp_event; CREATE VIEW erp_event.ir_event_outbox AS SELECT 1 AS id",
            "forged-relation",
        ),
        (
            "CREATE SCHEMA erp_event; CREATE TABLE erp_event.ir_event_outbox (id bigint, \
             event_id text)",
            "forged-relation",
        ),
        (
            "CREATE SCHEMA erp_event; CREATE TABLE erp_event.ir_event_outbox_id_seq (id bigint)",
            "forged-relation",
        ),
    ] {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        tx.batch(setup).await.unwrap();
        let error = tx
            .install_event_schema()
            .await
            .expect_err("a forged relation must be refused");
        refused(&error, code);
        h.cleanup().await.unwrap();
    }
}

/// A table, the schema or a partition owned by another role is refused (the install runs as the
/// owner role, which must own every object).
#[tokio::test]
async fn forged_owner_is_refused() {
    let h = harness().await;
    // a login role distinct from the owner; dropped with the database by `cleanup`
    h.read_only_role().await.unwrap();
    let role = format!("\"ro_{}\"", h.db().name());
    tx_grant_set(&h, &role).await;
    for forge in [
        format!("ALTER TABLE erp_event.ir_event_outbox OWNER TO {role}"),
        format!("ALTER SCHEMA erp_event OWNER TO {role}"),
        format!(
            "ALTER TABLE erp_event.ir_event_outbox_p{} OWNER TO {role}",
            "00000000"
        ),
    ] {
        let mut tx = h.begin().await.unwrap();
        tx.install_event_schema().await.unwrap();
        let day = window(&mut tx).await[0].clone();
        let forge = forge.replace("p00000000", &format!("p{day}"));
        tx.batch(&forge)
            .await
            .unwrap_or_else(|e| panic!("{forge}: {e:?}"));
        let error = tx
            .install_event_schema()
            .await
            .expect_err("a foreign owner must be refused");
        refused(&error, "forged-owner");
    }
    h.cleanup().await.unwrap();
}

/// Let the owner role switch to `role` (needed to hand objects to it in the owner tests).
async fn tx_grant_set(h: &Harness, role: &str) {
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!("GRANT {role} TO CURRENT_USER WITH SET TRUE"))
        .await
        .unwrap();
}

static EVENT_ADDON: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_event_schema",
    depends: &["base"],
    exact_only: false,
};

fn event_decl() -> ModelDecl {
    ModelDecl::new("test_event_schema", "test_event_schema.event")
        .log_access(false)
        .fields(
            const {
                &[
                    FieldDecl::timestamptz("recorded_at")
                        .required(true)
                        .server_set(true),
                    FieldDecl::char("name"),
                ]
            },
        )
        .partition(
            PartitionDecl::new(PartitionInterval::Day, 7, PartitionRetain::Intervals(30))
                .by("recorded_at"),
        )
}

/// Schema sync of a registry with an event-namespace model creates no table for it, in `public`
/// or anywhere else, and the ordinary model next to it is created.
#[tokio::test]
async fn schema_sync_never_creates_the_event_table_in_public() {
    let h = Harness::builder([&base_users::ADDON, &EVENT_ADDON])
        .without_audit_store()
        .with_core_event_decls([event_decl()])
        .with_decls([
            ModelDecl::new("test_event_schema", "test_event_schema.plain")
                .fields(const { &[FieldDecl::char("name")] }),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let event = h.registry().model("test_event_schema.event").unwrap();
    let plain = h.registry().model("test_event_schema.plain").unwrap();
    // every explicit sync entry point, not only the build's own
    tx.init_models(&[event.id, plain.id]).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    let found = tx
        .query(
            "SELECT n.nspname::text, c.relname::text FROM pg_class c JOIN pg_namespace n ON \
             n.oid = c.relnamespace WHERE c.relname LIKE 'test\\_event\\_schema%' \
             AND c.relkind IN ('r', 'p', 'v', 'S') ORDER BY 2",
            &[],
        )
        .await
        .unwrap();
    let tables: Vec<String> = found
        .iter()
        .map(|r| format!("{}.{}", r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect();
    assert!(
        tables.iter().all(|t| !t.contains(event.table.as_str())),
        "the event model got a table: {tables:?}"
    );
    assert!(
        tables.contains(&format!("public.{}", plain.table)),
        "{tables:?}"
    );
    let in_event_schema: i64 = tx
        .query(
            "SELECT count(*) FROM pg_namespace WHERE nspname = 'erp_event'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        in_event_schema, 0,
        "sync must not create the event schema either"
    );
    h.cleanup().await.unwrap();
}

/// The vacuum refuses an event-namespace model by name before it claims a cycle or sends a statement.
#[tokio::test]
async fn vacuum_refuses_an_event_namespace_model() {
    let h = Harness::builder([&base_users::ADDON, &EVENT_ADDON])
        .without_audit_store()
        .with_core_event_decls([event_decl()])
        .build()
        .await
        .unwrap();
    let error = h
        .orm()
        .vacuum_transient("test_event_schema.event", erp_orm::VacuumOptions::default())
        .await
        .expect_err("the vacuum must refuse the event namespace");
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(
        error.message().contains("closed event namespace"),
        "{}",
        error.message()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn malformed_partition_date_preserves_original_database_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    // Outside the premade window, so upkeep reaches catalog validation instead
    // of attempting to recreate a renamed current partition with overlapping bounds.
    tx.batch("CREATE TABLE erp_event.ir_event_outbox_p20261399 PARTITION OF erp_event.ir_event_outbox FOR VALUES FROM ('2040-01-01') TO ('2040-01-02')").await.unwrap();
    tx.capture_statements(true);
    let error = tx.ensure_event_partitions().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("22008"));
    // PostgreSQL may append server-version-specific detail; SQLSTATE and the
    // primary diagnostic must survive without an aborted-transaction wrapper.
    assert!(
        error
            .message()
            .contains("date/time field value out of range"),
        "{error}"
    );
    assert!(
        !error.message().contains("current transaction is aborted"),
        "{error:?}"
    );
    // Earlier successful catalog checks restore settings normally. The failing
    // partition query must be the last SQL, with no later restoration query.
    assert!(
        tx.captured()
            .last()
            .is_some_and(|sql| sql.contains("FROM pg_inherits i JOIN pg_class")),
        "{:?}",
        tx.captured()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn catalog_settings_restore_after_success_and_shape_refusal() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.query("SELECT set_config('search_path','public, pg_catalog',true),set_config('TimeZone','Asia/Jakarta',true)",&[]).await.unwrap();
    tx.install_event_schema().await.unwrap();
    for forged in [false, true] {
        if forged {
            tx.batch("ALTER TABLE erp_event.ir_event_outbox ADD COLUMN unexpected text")
                .await
                .unwrap();
        }
        if forged {
            let error = tx.ensure_event_partitions().await.unwrap_err();
            refused(&error, "forged-column");
        } else {
            tx.ensure_event_partitions().await.unwrap();
        }
        let settings = tx
            .query(
                "SELECT current_setting('search_path'),current_setting('TimeZone')",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(settings[0].get::<_, String>(0), "public, pg_catalog");
        assert_eq!(settings[0].get::<_, String>(1), "Asia/Jakarta");
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
