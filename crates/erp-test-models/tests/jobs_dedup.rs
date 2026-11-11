//! stage B: dedup keys coalesce among live jobs of a kind (4).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use erp_orm::jobs::{
    EnqueueOutcome, EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKey,
    JobKindDecl, OnDuplicate, RunOutcome, When,
};
use erp_orm::{BoxFuture, Context, UserId};
use erp_test_models::base_users;
use erp_testkit::Harness;

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
    DEDUPED,
    JobKindDecl::new("test_jobs", "test_jobs.deduped", HandlerRef::Native(&NOP))
);
erp_orm::register_job_kind!(
    OTHER,
    JobKindDecl::new("test_jobs", "test_jobs.other", HandlerRef::Native(&NOP))
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

async fn cell(h: &Harness, sql: &str) -> String {
    let mut tx = h.begin().await.unwrap();
    support::one(&mut tx, sql).await.unwrap_or_default()
}

fn request(kind: &'static str, key: &str, mode: OnDuplicate, secs: u64) -> EnqueueRequest {
    EnqueueRequest::new(kind)
        .at(When::After(Duration::from_secs(secs)))
        .dedup(JobKey::new(key).unwrap(), mode)
}

async fn run_after_secs(h: &Harness, id: i64) -> String {
    cell(
        h,
        &format!(
            "SELECT round(extract(epoch FROM run_after - clock_timestamp()))::text FROM erp_job WHERE id = {id}"
        ),
    )
    .await
}

#[tokio::test]
async fn keep_leaves_the_first_due_time_and_earliest_lowers_it() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let first = tx
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Keep, 600),
        )
        .await
        .unwrap();
    assert!(first.is_new());
    // the same transaction sees its own insert: coalesced, the due time stays
    let kept = tx
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Keep, 60),
        )
        .await
        .unwrap();
    assert_eq!(kept, EnqueueOutcome::Coalesced(first.job()));
    // Earliest lowers it
    let lowered = tx
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Earliest, 120),
        )
        .await
        .unwrap();
    assert_eq!(lowered, EnqueueOutcome::Coalesced(first.job()));
    // a later time never raises it
    let not_raised = tx
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Earliest, 9_000),
        )
        .await
        .unwrap();
    assert_eq!(not_raised, EnqueueOutcome::Coalesced(first.job()));
    tx.commit().await.unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "1");
    let due = run_after_secs(&h, first.job().id()).await;
    assert!(
        ["118", "119", "120"].contains(&due.as_str()),
        "due in {due} s"
    );

    // a new transaction coalesces into the committed job as well
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let again = tx
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Earliest, 30),
        )
        .await
        .unwrap();
    assert_eq!(again, EnqueueOutcome::Coalesced(first.job()));
    tx.commit().await.unwrap();
    let due = run_after_secs(&h, first.job().id()).await;
    assert!(["28", "29", "30"].contains(&due.as_str()), "due in {due} s");
}

#[tokio::test]
async fn a_key_belongs_to_its_kind_and_keyless_jobs_never_coalesce() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let a = tx
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Keep, 60),
        )
        .await
        .unwrap();
    let b = tx
        .enqueue(&env, request("test_jobs.other", "k", OnDuplicate::Keep, 60))
        .await
        .unwrap();
    assert!(a.is_new() && b.is_new());
    assert_ne!(a.job().id(), b.job().id());
    let x = tx
        .enqueue(&env, EnqueueRequest::new("test_jobs.deduped"))
        .await
        .unwrap();
    let y = tx
        .enqueue(&env, EnqueueRequest::new("test_jobs.deduped"))
        .await
        .unwrap();
    assert!(x.is_new() && y.is_new());
    tx.commit().await.unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "4");
}

#[tokio::test]
async fn a_batch_with_repeated_keys_inserts_one_and_coalesces_the_rest_in_order() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let outcomes = tx
        .enqueue_many(
            &env,
            vec![
                request("test_jobs.deduped", "a", OnDuplicate::Keep, 60),
                EnqueueRequest::new("test_jobs.deduped"),
                request("test_jobs.deduped", "a", OnDuplicate::Keep, 60),
                request("test_jobs.deduped", "b", OnDuplicate::Keep, 60),
            ],
        )
        .await
        .unwrap();
    assert!(outcomes[0].is_new());
    assert!(outcomes[1].is_new());
    assert_eq!(outcomes[2], EnqueueOutcome::Coalesced(outcomes[0].job()));
    assert!(outcomes[3].is_new());
    tx.commit().await.unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "3");
}

#[tokio::test]
async fn a_dead_job_frees_its_key() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let first = tx
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Keep, 60),
        )
        .await
        .unwrap();
    tx.execute(
        "UPDATE erp_job SET state = 'dead', dead_at = clock_timestamp() WHERE id = $1",
        &[&first.job().id()],
    )
    .await
    .unwrap();
    let second = tx
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Keep, 60),
        )
        .await
        .unwrap();
    assert!(second.is_new());
    assert_ne!(second.job(), first.job());
    tx.commit().await.unwrap();
}

/// Poll until a backend is waiting on a lock (the second enqueuer waiting for the first one's
/// uncommitted insert), at most five seconds.
async fn wait_for_lock_waiter(h: &Harness) {
    for _ in 0..250 {
        let waiting = cell(
            h,
            "SELECT count(*)::text FROM pg_stat_activity WHERE datname = current_database() \
             AND wait_event_type = 'Lock'",
        )
        .await;
        if waiting != "0" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nobody waited for the lock");
}

#[tokio::test]
async fn two_concurrent_enqueuers_of_one_key_end_with_one_job() {
    let h = harness().await;
    // the first enqueuer inserts and holds its transaction open
    let mut first = h.begin_as(7).await.unwrap();
    let env = first.env();
    let first_outcome = first
        .enqueue(
            &env,
            request("test_jobs.deduped", "k", OnDuplicate::Keep, 60),
        )
        .await
        .unwrap();
    assert!(first_outcome.is_new());

    // the second runs through the unit of work (its serialization failures are retried), starts
    // while the first is open and has to wait for it
    let attempts = AtomicU32::new(0);
    let second = h
        .orm()
        .run(UserId::new(8).unwrap(), Context::new(), async |tx| {
            attempts.fetch_add(1, Ordering::SeqCst);
            let env = tx.env();
            tx.enqueue(
                &env,
                request("test_jobs.deduped", "k", OnDuplicate::Keep, 60),
            )
            .await
        });
    let release = async {
        wait_for_lock_waiter(&h).await;
        first.commit().await.unwrap();
    };
    let (second_outcome, ()) = tokio::join!(second, release);
    let second_outcome = second_outcome.unwrap();
    assert_eq!(
        second_outcome,
        EnqueueOutcome::Coalesced(first_outcome.job())
    );
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "1");
    // Recorded, not asserted as a design choice: under REPEATABLE READ the skipped insert of a
    // key committed after the snapshot is a serialization failure (the whole-call retry makes the
    // second attempt see the first job); the run count says whether PostgreSQL raised it here.
    let ran = attempts.load(Ordering::SeqCst);
    eprintln!("jobs_dedup: the second enqueuer ran {ran} time(s) (2 = DO NOTHING raised 40001)");
    assert!(ran == 1 || ran == 2, "ran {ran} times");
}
