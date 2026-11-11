//! stage C: singleton keys.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_orm::BoxFuture;
use erp_orm::jobs::{
    EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKey, JobKindDecl, RunOutcome,
    RunnerHooks,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{cell, enqueue, runner};
use tokio::sync::Barrier;

struct Nop;

impl JobHandler for Nop {
    fn run<'a>(
        &'a self,
        _ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async { Ok(RunOutcome::Done) })
    }
}

static NOP: Nop = Nop;

erp_orm::register_job_kind!(
    KEYED,
    JobKindDecl::new("test_jobs", "test_jobs.keyed", HandlerRef::Native(&NOP))
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

fn keyed(key: &str) -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.keyed").singleton(JobKey::new(key).unwrap())
}

/// Both claimers sit between the two statements of the singleton claim until the other arrives.
struct Rendezvous(Barrier);

impl RunnerHooks for Rendezvous {
    fn between_singleton_statements(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            // a claimer that never arrives fails the test instead of hanging it
            tokio::time::timeout(std::time::Duration::from_secs(10), self.0.wait())
                .await
                .expect("both claimers reach the gap between the statements");
        })
    }
}

#[tokio::test]
async fn two_jobs_with_one_key_never_run_together_even_when_two_claimers_race() {
    let h = harness().await;
    enqueue(&h, vec![keyed("k"), keyed("k")]).await;
    let (a, b) = (runner(&h), runner(&h));
    let barrier = Arc::new(Rendezvous(Barrier::new(2)));
    a.set_hooks(barrier.clone());
    b.set_hooks(barrier);
    let (first, second) = tokio::join!(a.claim(8), b.claim(8));
    let claimed = first.unwrap().len() + second.unwrap().len();
    assert_eq!(claimed, 1, "one holder of the key");
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE state = 'running'"
        )
        .await,
        "1"
    );
    // a later claim, after the holder committed `running`, finds the key taken
    assert!(runner(&h).claim(8).await.unwrap().is_empty());
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE state = 'ready'"
        )
        .await,
        "1"
    );
}

/// path leaves it alone)
#[tokio::test]
async fn different_keys_run_in_parallel_and_the_plain_path_never_takes_a_keyed_job() {
    let h = harness().await;
    enqueue(
        &h,
        vec![
            keyed("a"),
            keyed("b"),
            keyed("c"),
            EnqueueRequest::new("test_jobs.keyed"),
        ],
    )
    .await;
    let claimed = runner(&h).claim(8).await.unwrap();
    assert_eq!(claimed.len(), 4);
    let mut keys: Vec<Option<&str>> = claimed.iter().map(|c| c.singleton_key()).collect();
    keys.sort();
    assert_eq!(keys, [None, Some("a"), Some("b"), Some("c")]);
}

#[tokio::test]
async fn a_holder_whose_lease_expired_no_longer_blocks_its_key() {
    let h = harness().await;
    enqueue(&h, vec![keyed("k"), keyed("k")]).await;
    let first = runner(&h).claim(8).await.unwrap();
    assert_eq!(first.len(), 1);
    assert!(runner(&h).claim(8).await.unwrap().is_empty());
    support::jobs::exec(
        &h,
        "UPDATE erp_job SET lease_until = clock_timestamp() - interval '1 second' WHERE state = 'running'",
    )
    .await;
    // the second job may now start: a dead holder cannot be waited for forever
    assert_eq!(runner(&h).claim(8).await.unwrap().len(), 1);
}
