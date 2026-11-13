//! stage A: the queue's tables: created
//! idempotently, with `CHECK`s that refuse what the Rust-side checks would also refuse.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_orm::CorePhase;
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

const COLUMNS: &str = "kind, kind_version, max_attempts, run_as_kind, run_as_user, \
                       enqueued_by_kind, enqueued_by_user";

/// Insert one row with `overrides` appended to the minimal valid column list, in a transaction
/// that is rolled back (a refused insert leaves the transaction aborted anyway).
async fn insert(h: &Harness, extra_columns: &str, extra_values: &str) -> erp_fields::Result<u64> {
    let mut tx = h.begin().await.unwrap();
    let sql = format!(
        "INSERT INTO erp_job ({COLUMNS}{}{extra_columns}) VALUES ('test.kind', 1, 3, 'user', 7, 'user', 7{}{extra_values})",
        if extra_columns.is_empty() { "" } else { ", " },
        if extra_values.is_empty() { "" } else { ", " },
    );
    let result = tx.execute(&sql, &[]).await;
    let _ = tx.rollback().await;
    result
}

#[tokio::test]
async fn the_three_tables_exist_and_the_ddl_runs_again_without_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for table in ["erp_job", "erp_job_run", "erp_job_item_failure"] {
        let exists = support::one(
            &mut tx,
            &format!("SELECT (to_regclass('public.{table}') IS NOT NULL)::text"),
        )
        .await;
        assert_eq!(exists.as_deref(), Some("true"), "{table}");
    }
    // the same DDL again, as `sync_schema` does on every start
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn a_minimal_valid_row_is_accepted_with_its_defaults() {
    let h = harness().await;
    assert_eq!(insert(&h, "", "").await.unwrap(), 1);
}

#[tokio::test]
async fn the_checks_refuse_malformed_rows() {
    let h = harness().await;
    let refused = [
        ("state", "'finished'", "erp_job_state_check"),
        ("priority", "10", "erp_job_priority_check"),
        ("priority", "-1", "erp_job_priority_check"),
        ("attempts", "1001", "erp_job_attempts_check"),
        ("failure_rank", "-1", "erp_job_failure_rank_check"),
        ("singleton_key", "''", "erp_job_singleton_key_check"),
        ("dedup_key", "''", "erp_job_dedup_key_check"),
        (
            "run_as_companies",
            "ARRAY(SELECT generate_series(1, 257)::int8)",
            "erp_job_run_as_companies_check",
        ),
        (
            "last_error",
            "repeat('x', 4097)",
            "erp_job_last_error_check",
        ),
        (
            "last_error_code",
            "repeat('x', 65)",
            "erp_job_last_error_code_check",
        ),
        ("items_done", "-1", "erp_job_items_done_check"),
        (
            "payload",
            "to_jsonb(repeat('x', 65536))",
            "erp_job_payload_check",
        ),
        (
            "progress",
            "to_jsonb(repeat('x', 1048576))",
            "erp_job_progress_check",
        ),
        // running without a lease, a lease without running, dead without a time
        ("state", "'running'", "erp_job_lease_iff_running"),
        ("state", "'dead'", "erp_job_dead_iff_dead_at"),
        // a key without an agent or service kind
        ("run_as_key", "5", "erp_job_key_kinds"),
    ];
    for (column, value, constraint) in refused {
        let error = insert(&h, column, value).await.unwrap_err();
        assert_eq!(error.sqlstate(), Some("23514"), "{column} = {value}");
        assert_eq!(error.constraint(), Some(constraint), "{column} = {value}");
    }
    // an agent or service principal needs its key
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .execute(
            "INSERT INTO erp_job (kind, kind_version, max_attempts, run_as_kind, run_as_user, \
             enqueued_by_kind, enqueued_by_user) VALUES ('test.kind', 1, 3, 'agent', 7, 'user', 7)",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(error.constraint(), Some("erp_job_key_kinds"));
    // a kind that is not '<addon>.<name>'
    let mut tx = h.begin().await.unwrap();
    for kind in ["nodot", "Upper.case", "a.B", "1a.b"] {
        let error = tx
            .execute(
                &format!("INSERT INTO erp_job ({COLUMNS}) VALUES ($1, 1, 3, 'user', 7, 'user', 7)"),
                &[&kind],
            )
            .await
            .unwrap_err();
        assert_eq!(error.sqlstate(), Some("23514"), "{kind}");
        assert_eq!(error.constraint(), Some("erp_job_kind_check"), "{kind}");
        tx = h.begin().await.unwrap();
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn running_needs_a_lease_and_dead_needs_a_time() {
    let h = harness().await;
    assert_eq!(
        insert(
            &h,
            "state, lease_owner, lease_until",
            "'running', gen_random_uuid(), clock_timestamp() + interval '1 minute'"
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        insert(&h, "state, dead_at", "'dead', clock_timestamp()")
            .await
            .unwrap(),
        1
    );
    // a lease while ready
    let error = insert(
        &h,
        "lease_owner, lease_until",
        "gen_random_uuid(), clock_timestamp()",
    )
    .await
    .unwrap_err();
    assert_eq!(error.constraint(), Some("erp_job_lease_iff_running"));
}

#[tokio::test]
async fn a_dedup_key_is_unique_among_live_rows_of_a_kind_but_a_dead_row_frees_it() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let live = format!(
        "INSERT INTO erp_job ({COLUMNS}, dedup_key) VALUES ('test.kind', 1, 3, 'user', 7, 'user', 7, 'k')"
    );
    tx.execute(&live, &[]).await.unwrap();
    let error = tx.execute(&live, &[]).await.unwrap_err();
    assert_eq!(error.sqlstate(), Some("23505"));
    assert_eq!(error.constraint(), Some("erp_job_dedup"));
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.execute(
        &format!(
            "INSERT INTO erp_job ({COLUMNS}, dedup_key, state, dead_at) VALUES \
             ('test.kind', 1, 3, 'user', 7, 'user', 7, 'k', 'dead', clock_timestamp())"
        ),
        &[],
    )
    .await
    .unwrap();
    tx.execute(&live, &[]).await.unwrap();
    // another kind may reuse the key
    tx.execute(
        &format!(
            "INSERT INTO erp_job ({COLUMNS}, dedup_key) VALUES ('test.other', 1, 3, 'user', 7, 'user', 7, 'k')"
        ),
        &[],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
}

const OLD_RUN_TABLE: &str = "DROP TABLE erp_job_run CASCADE; \
    CREATE TABLE erp_job_run (job_uuid uuid NOT NULL, kind text NOT NULL, kind_version int2 NOT NULL, \
    outcome text NOT NULL, attempts int4 NOT NULL, enqueued_at timestamptz NOT NULL, started_at timestamptz, \
    finished_at timestamptz NOT NULL DEFAULT clock_timestamp(), items_done int8 NOT NULL, \
    items_failed int8 NOT NULL, run_as_kind text NOT NULL, run_as_user int8 NOT NULL, \
    enqueued_by_user int8 NOT NULL, correlation_id uuid, last_error_code text, \
    PRIMARY KEY (job_uuid, finished_at)); \
    INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, items_done, \
    items_failed, run_as_kind, run_as_user, enqueued_by_user) \
    SELECT gen_random_uuid(), 'test.kind', 1, 'done', 1, now(), 1, 0, 'user', 7, 7 FROM generate_series(1, 2)";

/// What the run table's keys and the two admin views look like, for a before/after comparison.
async fn run_shape(tx: &mut erp_orm::Transaction) -> String {
    support::one(
        tx,
        "SELECT concat_ws(' | ', \
           (SELECT string_agg(pg_get_constraintdef(oid), ',' ORDER BY conname) FROM pg_constraint \
             WHERE conrelid = 'erp_job_run'::regclass AND contype IN ('p', 'u')), \
           (SELECT string_agg(indexdef, ',' ORDER BY indexname) FROM pg_indexes WHERE tablename = 'erp_job_run'), \
           (SELECT count(*) FROM erp_job_run), (SELECT count(DISTINCT id) FROM erp_job_run), \
           'ir_job_run'::regclass::oid, 'ir_job'::regclass::oid)",
    )
    .await
    .unwrap()
}

/// same key as a fresh database, rows kept, and a second run changes nothing)
#[tokio::test]
async fn an_older_run_table_gets_the_same_key_as_a_fresh_one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let fresh_key = support::one(
        &mut tx,
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conrelid = 'erp_job_run'::regclass AND contype = 'p'",
    )
    .await;
    assert_eq!(fresh_key.as_deref(), Some("PRIMARY KEY (id)"));
    let fresh_indexes = support::one(
        &mut tx,
        "SELECT string_agg(indexname, ',' ORDER BY indexname) FROM pg_indexes WHERE tablename = 'erp_job_run'",
    )
    .await;

    tx.batch(OLD_RUN_TABLE).await.unwrap();
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    let key = support::one(
        &mut tx,
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conrelid = 'erp_job_run'::regclass AND contype = 'p'",
    )
    .await;
    assert_eq!(key, fresh_key);
    let indexes = support::one(
        &mut tx,
        "SELECT string_agg(indexname, ',' ORDER BY indexname) FROM pg_indexes WHERE tablename = 'erp_job_run'",
    )
    .await;
    assert_eq!(indexes, fresh_indexes);
    let rows = support::one(
        &mut tx,
        "SELECT count(*) || '/' || count(DISTINCT id) FROM erp_job_run",
    )
    .await;
    assert_eq!(rows.as_deref(), Some("2/2"));

    let once = run_shape(&mut tx).await;
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    assert_eq!(run_shape(&mut tx).await, once);
    tx.rollback().await.unwrap();
}

/// `security_invoker`, and are left alone by a second run)
#[tokio::test]
async fn a_stale_admin_view_is_recreated_and_a_current_one_is_left_alone() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // a view of another shape under the old name: CREATE OR REPLACE VIEW would refuse this
    tx.batch("DROP VIEW ir_job; CREATE VIEW ir_job AS SELECT 'x'::text AS id")
        .await
        .unwrap();
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    let shape = "SELECT concat_ws(' ', (SELECT count(*) FROM pg_attribute WHERE attrelid = 'ir_job'::regclass \
                       AND attname = 'kind' AND NOT attisdropped), \
                       (SELECT reloptions::text FROM pg_class WHERE oid = 'ir_job'::regclass), \
                       'ir_job'::regclass::oid)";
    let once = support::one(&mut tx, shape).await.unwrap();
    assert!(once.starts_with("1 {security_invoker=true} "), "{once}");
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    assert_eq!(support::one(&mut tx, shape).await.unwrap(), once);
    tx.rollback().await.unwrap();
}
