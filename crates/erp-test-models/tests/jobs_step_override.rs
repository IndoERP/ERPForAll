//! stage D: a step kind is a model method, so another addon changes it with super
//! (2).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::StaticPolicy;
use erp_orm::jobs::{EnqueueRequest, HandlerRef, JobKindDecl};
use erp_test_models::base_users;
use erp_test_models::test_jobs_fixtures::{test_jobs, test_jobs_ext};
use erp_testkit::Harness;
use support::jobs::{cell, enqueue, exec, runner};

erp_orm::register_job_kind!(
    RECOUNT,
    JobKindDecl::new(
        "test_jobs",
        "test_jobs.recount",
        HandlerRef::Step {
            model: "test_jobs.recount",
            method: "run_step"
        }
    )
);

/// Rows written by user 7 reference that user.
async fn with_user(h: Harness) -> Harness {
    exec(
        &h,
        "INSERT INTO res_users (id, name, login, active) OVERRIDING SYSTEM VALUE VALUES (7, 'u7', 'u7', true)",
    )
    .await;
    h
}

async fn without_extension() -> Harness {
    with_user(
        Harness::builder([&base_users::ADDON, &test_jobs::ADDON])
            .policy(StaticPolicy::AllowAll)
            .build()
            .await
            .expect("harness"),
    )
    .await
}

async fn with_extension() -> Harness {
    with_user(
        Harness::builder([&base_users::ADDON, &test_jobs::ADDON, &test_jobs_ext::ADDON])
            .policy(StaticPolicy::AllowAll)
            .build()
            .await
            .expect("harness"),
    )
    .await
}

fn recount(total: u64) -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.recount")
        .payload(&serde_json::json!({ "total": total }))
        .unwrap()
}

async fn rows(h: &Harness) -> String {
    cell(
        h,
        "SELECT coalesce(string_agg(name, ',' ORDER BY id), '') FROM test_jobs_recount",
    )
    .await
}

#[tokio::test]
async fn a_step_kind_runs_its_model_method_chunk_by_chunk() {
    let h = without_extension().await;
    let jobs = enqueue(&h, vec![recount(3)]).await;
    let runner = runner(&h);
    runner.tick().await.unwrap();
    // the budget lets one run finish three one-row chunks; the job completed in its last chunk
    assert_eq!(rows(&h).await, "base:0,base:1,base:2");
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(
        cell(
            &h,
            "SELECT concat_ws('|', outcome, items_done) FROM erp_job_run"
        )
        .await,
        "done|3"
    );
    let _ = jobs;
}

#[tokio::test]
async fn an_addon_overrides_the_step_with_super_and_both_effects_commit_in_the_chunk() {
    let h = with_extension().await;
    enqueue(&h, vec![recount(2)]).await;
    runner(&h).tick().await.unwrap();
    // each chunk: the owner's row first (super), then the extension's, in one transaction
    assert_eq!(rows(&h).await, "base:0,ext:0,base:1,ext:1");
    // the extension changed the progress the next chunk reads and the history shows the job done
    assert_eq!(cell(&h, "SELECT outcome FROM erp_job_run").await, "done");
}

#[tokio::test]
async fn a_step_kind_is_not_claimed_by_a_binary_whose_registry_lacks_its_model() {
    let h = Harness::builder([&base_users::ADDON])
        .policy(StaticPolicy::AllowAll)
        .build()
        .await
        .unwrap();
    enqueue(&h, vec![recount(1)]).await;
    let runner = runner(&h);
    assert_eq!(runner.tick().await.unwrap().claimed, 0);
    assert_eq!(cell(&h, "SELECT state FROM erp_job").await, "ready");
}
