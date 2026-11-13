//! The job runner inside the server process (4): `jobs_run_in_app = true` starts a
//! runner that ensures the core schedules and runs due jobs as the stored principal; the default
//! starts none. Uses the actual binary and a nonowner runtime role.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration fixture assertions and shared helpers"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::*;
use std::time::{Duration, Instant};

/// schedules and runs a due job under the runtime role)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_with_jobs_run_in_app_runs_due_jobs_and_keeps_the_core_schedules() {
    let fixture = NativeFixture::start_with_config("jobs_run_in_app=true").await;
    let count = |sql: &'static str| {
        let client = &fixture.client;
        async move { client.query_one(sql, &[]).await.unwrap().get::<_, i64>(0) }
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while count("SELECT count(*) FROM erp_job WHERE kind LIKE 'core.%'").await < 2 {
        assert!(
            Instant::now() < deadline,
            "the runner did not ensure the core schedules: {}",
            std::fs::read_to_string(&fixture.stderr_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // a schedule that is due runs in this process and goes back to waiting for its next turn; the
    // attachment schedules are made due at the same moment, and this runner must leave them
    fixture
        .client
        .execute(
            "UPDATE erp_job SET run_after = clock_timestamp() \
             WHERE kind = 'core.jobs_history_retention' OR kind LIKE 'base.attachment\\_%'",
            &[],
        )
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while count(
        "SELECT count(*) FROM erp_job WHERE kind = 'core.jobs_history_retention' \
         AND started_at IS NOT NULL AND state = 'ready' \
         AND run_after > clock_timestamp() + interval '30 minutes'",
    )
    .await
        < 1
    {
        assert!(
            Instant::now() < deadline,
            "the due job did not run: {}",
            std::fs::read_to_string(&fixture.stderr_path).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // the in-app runner excludes the elevated attachment kinds; their due rows were
    // never claimed
    assert_attachment_rows_untouched(&fixture).await;
    fixture.cleanup().await;
}

/// The attachment schedules: one row per kind, every one `ready`, never started, attempts 0.
async fn assert_attachment_rows_untouched(fixture: &NativeFixture) {
    let rows: Vec<(String, String, i32, bool)> = fixture
        .client
        .query(
            "SELECT kind, state, attempts, started_at IS NOT NULL FROM erp_job \
             WHERE kind LIKE 'base.attachment\\_%' ORDER BY kind",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect();
    let expected: Vec<(String, String, i32, bool)> = [
        "base.attachment_clean_staging",
        "base.attachment_gc",
        "base.attachment_owner_heartbeat",
        "base.attachment_reconcile",
        "base.attachment_usage_compact",
    ]
    .into_iter()
    .map(|kind| (kind.to_owned(), "ready".to_owned(), 0, false))
    .collect();
    assert_eq!(rows, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_without_the_setting_starts_no_runner() {
    let fixture = NativeFixture::start().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    // the attachment schedules exist on every database (base install and every start); without
    // a runner there is no other row, and none of them is claimed
    let rows: i64 = fixture
        .client
        .query_one(
            "SELECT count(*) FROM erp_job WHERE kind NOT IN \
             ('base.attachment_gc', 'base.attachment_clean_staging', 'base.attachment_reconcile', \
             'base.attachment_owner_heartbeat','base.attachment_usage_compact')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
    assert_attachment_rows_untouched(&fixture).await;
    fixture.cleanup().await;
}
