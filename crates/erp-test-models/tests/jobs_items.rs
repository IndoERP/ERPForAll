//! stage D: per-item isolation (3).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::BoxFuture;
use erp_orm::jobs::{
    EnqueueRequest, HandlerRef, ItemsChunk, JobContext, JobError, JobHandler, JobKindDecl,
    JobRunner, JobSettings, RunOutcome, Setting,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{PassResolver, cell, enqueue, exec};

/// Writes one effect row per item of the payload's `items`; an item listed in `poison` fails
/// by the way the payload's `fail_as` names: `item` (the handler marks it), `constraint` (the
/// table's CHECK refuses it) or `transient` (a failure of the chunk, not of the item).
struct Batch;

impl JobHandler for Batch {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let payload: serde_json::Value = ctx.payload()?;
            let items: Vec<i64> = serde_json::from_value(payload["items"].clone()).unwrap();
            let poison: Vec<i64> = serde_json::from_value(payload["poison"].clone()).unwrap();
            let fail_as = payload["fail_as"].as_str().unwrap_or("item").to_owned();
            ctx.chunk_items(
                ItemsChunk::new(&items, serde_json::json!({"batch": 1})).finished(),
                move |tx, batch| {
                    let poison = poison.clone();
                    let fail_as = fail_as.clone();
                    Box::pin(async move {
                        for item in batch {
                            if poison.contains(&item) {
                                match fail_as.as_str() {
                                    "constraint" => {
                                        tx.execute(
                                            "INSERT INTO jobs_effect (note) VALUES ('bad')",
                                            &[],
                                        )
                                        .await?;
                                    }
                                    "transient" => {
                                        return Err(JobError::transient("flaky", "lock wait"));
                                    }
                                    _ => {
                                        return Err(JobError::item(
                                            "poison",
                                            format!("item {item} cannot be processed"),
                                        ));
                                    }
                                }
                            }
                            let note = format!("item {item}");
                            tx.execute("INSERT INTO jobs_effect (note) VALUES ($1)", &[&note])
                                .await?;
                        }
                        Ok(())
                    })
                },
            )
            .await?;
            Ok(RunOutcome::Done)
        })
    }
}

static BATCH: Batch = Batch;

erp_orm::register_job_kind!(
    BATCH_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.batch", HandlerRef::Native(&BATCH)).max_attempts(2)
);

async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    exec(
        &h,
        "CREATE TABLE jobs_effect (id bigserial PRIMARY KEY, note text NOT NULL CHECK (note <> 'bad'))",
    )
    .await;
    h
}

fn batch(items: &[i64], poison: &[i64], fail_as: &str) -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.batch")
        .payload(&serde_json::json!({"items": items, "poison": poison, "fail_as": fail_as}))
        .unwrap()
}

async fn notes(h: &Harness) -> String {
    cell(
        h,
        "SELECT coalesce(string_agg(note, ',' ORDER BY id), '') FROM jobs_effect",
    )
    .await
}

#[tokio::test]
async fn one_poison_item_fails_alone_the_others_commit_and_the_failure_is_recorded() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![batch(&[1, 2, 3, 4], &[3], "item")]).await;
    support::jobs::runner(&h).tick().await.unwrap();
    assert_eq!(notes(&h).await, "item 1,item 2,item 4");
    let failures = cell(
        &h,
        &format!(
            "SELECT concat_ws('|', item_key, error_code, error) FROM erp_job_item_failure WHERE job_uuid = '{}'",
            jobs[0].uuid()
        ),
    )
    .await;
    assert_eq!(failures, "3|poison|item 3 cannot be processed");
    // the job finished; its counters and its history row carry the failure, and its failure rows
    // stay for the operator
    assert_eq!(
        cell(
            &h,
            "SELECT concat_ws('|', outcome, items_done, items_failed) FROM erp_job_run"
        )
        .await,
        "done|3|1"
    );
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM erp_job_item_failure").await,
        "1"
    );
}

#[tokio::test]
async fn a_constraint_error_of_the_database_is_attributed_to_its_item() {
    let h = harness().await;
    enqueue(&h, vec![batch(&[1, 2, 3], &[2], "constraint")]).await;
    support::jobs::runner(&h).tick().await.unwrap();
    assert_eq!(notes(&h).await, "item 1,item 3");
    assert_eq!(
        cell(
            &h,
            "SELECT item_key || '|' || error_code FROM erp_job_item_failure"
        )
        .await,
        "2|validation"
    );
}

#[tokio::test]
async fn a_chunk_in_which_every_item_fails_is_a_chunk_failure_not_a_success_with_no_items() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![batch(&[1, 2], &[1, 2], "item")]).await;
    support::jobs::runner(&h).tick().await.unwrap();
    assert_eq!(notes(&h).await, "");
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM erp_job_item_failure").await,
        "0"
    );
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', state, last_error_code, last_error) FROM erp_job WHERE id = {}",
                jobs[0].id()
            )
        )
        .await,
        "dead|poison|item 1 cannot be processed"
    );
}

/// per-item pass)
#[tokio::test]
async fn an_error_that_belongs_to_the_chunk_is_not_isolated_per_item() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![batch(&[1, 2, 3], &[2], "transient")]).await;
    support::jobs::runner(&h).tick().await.unwrap();
    // the chunk rolled back whole (item 1's row too) and the job retries
    assert_eq!(notes(&h).await, "");
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', state, last_error_code) FROM erp_job WHERE id = {}",
                jobs[0].id()
            )
        )
        .await,
        "ready|flaky"
    );
}

#[tokio::test]
async fn more_failures_than_the_cap_dead_letter_the_job_naming_the_limit() {
    let h = harness().await;
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(
            JobSettings::builder()
                .set(Setting::MaxItemFailuresPerJob, 1)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let jobs = enqueue(&h, vec![batch(&[1, 2, 3, 4], &[2, 4], "item")]).await;
    let runner = JobRunner::builder(orm, PassResolver).build().unwrap();
    runner.tick().await.unwrap();
    assert_eq!(notes(&h).await, "");
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM erp_job_item_failure").await,
        "0"
    );
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', state, last_error_code, last_error) FROM erp_job WHERE id = {}",
                jobs[0].id()
            )
        )
        .await,
        "dead|item_failures_exceeded|more than 1 items failed (jobs.max_item_failures_per_job)"
    );
}

#[tokio::test]
async fn an_item_that_fails_twice_in_one_chunk_is_recorded_once() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![batch(&[1, 3, 2, 3, 4], &[3], "item")]).await;
    support::jobs::runner(&h).tick().await.unwrap();
    assert_eq!(notes(&h).await, "item 1,item 2,item 4");
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT count(*)::text FROM erp_job_item_failure WHERE job_uuid = '{}' AND item_key = '3'",
                jobs[0].uuid()
            )
        )
        .await,
        "1"
    );
    assert_eq!(
        cell(
            &h,
            "SELECT concat_ws('|', outcome, items_done, items_failed) FROM erp_job_run"
        )
        .await,
        "done|3|1"
    );
}
