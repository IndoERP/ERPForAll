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
    // a schedule that is due runs in this process and goes back to waiting for its next turn
    fixture
        .client
        .execute(
            "UPDATE erp_job SET run_after = clock_timestamp() \
             WHERE kind = 'core.jobs_history_retention'",
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
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_without_the_setting_starts_no_runner() {
    let fixture = NativeFixture::start().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let rows: i64 = fixture
        .client
        .query_one("SELECT count(*) FROM erp_job", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
    fixture.cleanup().await;
}
