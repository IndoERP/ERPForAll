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
