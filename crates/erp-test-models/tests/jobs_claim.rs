//! stage C: the claim (D6.1; design §5.2).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::time::Duration;

use erp_fields::TimestampTz;
use erp_orm::BoxFuture;
use erp_orm::jobs::{
    EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl, Priority,
    RunOutcome, TimeoutClass, When,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{cell, enqueue, exec, runner};

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
    PLAIN,
    JobKindDecl::new("test_jobs", "test_jobs.plain", HandlerRef::Native(&NOP))
);
erp_orm::register_job_kind!(
    LONG,
    JobKindDecl::new("test_jobs", "test_jobs.long", HandlerRef::Native(&NOP))
        .timeouts(TimeoutClass::Maintenance)
);
erp_orm::register_job_kind!(
    CAPPED,
    JobKindDecl::new("test_jobs", "test_jobs.capped", HandlerRef::Native(&NOP))
        .max_concurrency(NonZeroU32::MIN)
);
erp_orm::register_job_kind!(
    NO_MODEL,
    JobKindDecl::new(
        "test_jobs",
        "test_jobs.no_model",
        HandlerRef::Step {
            model: "test_jobs.no_such_model",
            method: "run_step"
        }
    )
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

fn plain() -> EnqueueRequest {
    EnqueueRequest::new("test_jobs.plain")
}

fn at(micros: i64) -> When {
    When::At(TimestampTz::from_unix_micros(micros).unwrap())
}

/// 2020-01-01 and the days after, in Unix microseconds.
const DAY: i64 = 86_400_000_000;
const Y2020: i64 = 1_577_836_800_000_000;

#[tokio::test]
async fn only_due_jobs_of_runnable_kinds_are_claimed() {
    let h = harness().await;
    let jobs = enqueue(
        &h,
        vec![
            plain(),
            plain().at(When::After(Duration::from_secs(3600))),
            EnqueueRequest::new("test_jobs.no_model"),
        ],
    )
    .await;
    // a job of a kind this binary does not register, a held job and a dead one
    exec(
        &h,
        "INSERT INTO erp_job (kind, kind_version, max_attempts, run_as_kind, run_as_user, \
         enqueued_by_kind, enqueued_by_user) VALUES ('other.unknown', 1, 3, 'user', 7, 'user', 7)",
    )
    .await;
    exec(
        &h,
        "INSERT INTO erp_job (kind, kind_version, max_attempts, run_as_kind, run_as_user, \
         enqueued_by_kind, enqueued_by_user, state) VALUES ('test_jobs.plain', 1, 3, 'user', 7, 'user', 7, 'held')",
    )
    .await;
    exec(
        &h,
        "INSERT INTO erp_job (kind, kind_version, max_attempts, run_as_kind, run_as_user, \
         enqueued_by_kind, enqueued_by_user, state, dead_at) VALUES ('test_jobs.plain', 1, 3, 'user', 7, 'user', 7, 'dead', now())",
    )
    .await;
    let runner = runner(&h);
    let claimed = runner.claim(8).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].job(), jobs[0]);
    assert_eq!(claimed[0].attempt(), 1);
    assert_eq!(claimed[0].epoch(), 1);
    assert_eq!(claimed[0].kind().name, "test_jobs.plain");
    assert!(runner.claim(8).await.unwrap().is_empty());
    // everything else is where it was
    assert_eq!(
        cell(
            &h,
            "SELECT string_agg(kind || ':' || state, ',' ORDER BY id) FROM erp_job"
        )
        .await,
        "test_jobs.plain:running,test_jobs.plain:ready,test_jobs.no_model:ready,\
         other.unknown:ready,test_jobs.plain:held,test_jobs.plain:dead"
    );
}

#[tokio::test]
async fn a_claim_takes_a_lease_an_epoch_and_an_attempt() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![plain(), EnqueueRequest::new("test_jobs.long")]).await;
    let runner = runner(&h);
    let claimed = runner.claim(8).await.unwrap();
    assert_eq!(claimed.len(), 2);
    let row = |id: i64| {
        format!(
            "SELECT concat_ws('|', state, lease_owner, lease_epoch, attempts, \
             round(extract(epoch FROM lease_until - clock_timestamp()) / 10) * 10, \
             (started_at IS NOT NULL)::text) FROM erp_job WHERE id = {id}"
        )
    };
    let owner = runner.owner();
    // a default-class job holds the 90 s lease, a maintenance-class job transaction timeout + 10 s
    assert_eq!(
        cell(&h, &row(jobs[0].id())).await,
        format!("running|{owner}|1|1|90|true")
    );
    assert_eq!(
        cell(&h, &row(jobs[1].id())).await,
        format!("running|{owner}|1|1|610|true")
    );
}

#[tokio::test]
async fn jobs_are_claimed_by_failure_rank_then_priority_then_run_after_then_id() {
    let h = harness().await;
    let p = |n| Priority::new(n).unwrap();
    // ids ascend in this order; the expected claim order is listed by label
    let jobs = enqueue(
        &h,
        vec![
            plain().at(at(Y2020 + 5 * DAY)).priority(p(5)), // a: later run_after, same priority as b
            plain().at(at(Y2020 + DAY)).priority(p(5)), // b: earlier run_after, higher id than a
            plain().at(at(Y2020 + 9 * DAY)).priority(p(1)), // c: better priority
            plain().at(at(Y2020 + DAY)).priority(p(5)), // d: same run_after as b, higher id
            plain().at(at(Y2020)).priority(p(9)),       // e: worst priority, earliest
            plain().at(at(Y2020 + 2 * DAY)).priority(p(0)), // f: failing (rank 3) despite priority 0
        ],
    )
    .await;
    exec(
        &h,
        &format!(
            "UPDATE erp_job SET failure_rank = 3 WHERE id = {}",
            jobs[5].id()
        ),
    )
    .await;
    let runner = runner(&h);
    let mut order = Vec::new();
    for _ in 0..jobs.len() {
        let claimed = runner.claim(1).await.unwrap();
        assert_eq!(claimed.len(), 1);
        let index = jobs.iter().position(|j| *j == claimed[0].job()).unwrap();
        order.push(["a", "b", "c", "d", "e", "f"][index]);
    }
    // healthy jobs first (rank 0): c (priority 1), then priority 5 by run_after (b and d, then a),
    // then e (priority 9); the failing job last although its priority is 0
    assert_eq!(order, ["c", "b", "d", "a", "e", "f"]);
}

#[tokio::test]
async fn a_claim_takes_at_most_the_claim_batch() {
    let h = harness().await;
    enqueue(&h, (0..20).map(|_| plain()).collect()).await;
    let runner = runner(&h);
    assert_eq!(runner.claim(100).await.unwrap().len(), 8);
    assert_eq!(runner.claim(3).await.unwrap().len(), 3);
    assert_eq!(runner.claim(0).await.unwrap().len(), 0);
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE state = 'running'"
        )
        .await,
        "11"
    );
}

#[tokio::test]
async fn two_claimers_never_return_one_job_twice_and_a_locked_job_is_skipped_not_waited_for() {
    let h = harness().await;
    let jobs = enqueue(&h, (0..40).map(|_| plain()).collect()).await;
    // another transaction holds the first job's row (as a completing worker does)
    let mut holder = h.begin().await.unwrap();
    holder
        .execute(
            "SELECT id FROM erp_job WHERE id = $1 FOR UPDATE",
            &[&jobs[0].id()],
        )
        .await
        .unwrap();
    let (a, b) = (runner(&h), runner(&h));
    let (first, second) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(a.claim(8), b.claim(8))
    })
    .await
    .expect("a locked row is skipped, never waited for");
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.len() + second.len(), 16);
    let ids: BTreeSet<i64> = first
        .iter()
        .chain(second.iter())
        .map(|c| c.job().id())
        .collect();
    assert_eq!(ids.len(), 16, "no job claimed twice");
    assert!(!ids.contains(&jobs[0].id()), "the locked job was skipped");
    holder.rollback().await.unwrap();
    // released, it is claimable again
    let again = a.claim(8).await.unwrap();
    assert!(again.iter().any(|c| c.job() == jobs[0]));
}

#[tokio::test]
async fn the_claim_statement_is_a_no_key_update_skip_locked() {
    assert!(erp_orm::jobs::claim_statement_for_tests().contains("FOR NO KEY UPDATE SKIP LOCKED"));
}

#[tokio::test]
async fn a_claim_beside_a_completing_worker_raises_nothing_and_goes_on_to_the_next_job() {
    let h = harness().await;
    let jobs = enqueue(&h, vec![plain(), plain(), plain()]).await;
    let runner = runner(&h);
    // worker one claims the first job and is completing it: its row is locked, not yet deleted
    let first = runner.claim(1).await.unwrap();
    assert_eq!(first[0].job(), jobs[0]);
    let mut completing = h.begin().await.unwrap();
    completing
        .execute("DELETE FROM erp_job WHERE id = $1", &[&jobs[0].id()])
        .await
        .unwrap();
    // worker two claims meanwhile: no serialization failure, it takes the next jobs
    let second = support::jobs::runner(&h).claim(8).await.unwrap();
    assert_eq!(second.len(), 2);
    assert!(second.iter().all(|c| c.job() != jobs[0]));
    completing.commit().await.unwrap();
    // the finished job is gone and nobody gets it again
    assert!(runner.claim(8).await.unwrap().is_empty());
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "2");
}

#[tokio::test]
async fn a_kind_at_its_concurrency_cap_is_not_claimed_again() {
    let h = harness().await;
    enqueue(
        &h,
        vec![
            EnqueueRequest::new("test_jobs.capped"),
            EnqueueRequest::new("test_jobs.capped"),
            plain(),
        ],
    )
    .await;
    let runner = runner(&h);
    // the share of a cap of 1 with one live worker is 1; a running job of the kind uses it
    runner.pretend_running_for_tests("test_jobs.capped", 1);
    let claimed = runner.claim(8).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].kind().name, "test_jobs.plain");
    runner.pretend_running_for_tests("test_jobs.capped", 0);
    assert_eq!(runner.claim(8).await.unwrap().len(), 2);
}

#[tokio::test]
async fn next_due_reports_the_wait_for_the_earliest_ready_job() {
    let h = harness().await;
    let runner = runner(&h);
    assert_eq!(runner.next_due().await.unwrap(), None);
    enqueue(
        &h,
        vec![
            plain().at(When::After(Duration::from_secs(600))),
            plain().at(When::After(Duration::from_secs(300))),
        ],
    )
    .await;
    let wait = runner.next_due().await.unwrap().unwrap();
    assert!(
        wait > Duration::from_secs(290) && wait <= Duration::from_secs(300),
        "{wait:?}"
    );
    enqueue(&h, vec![plain()]).await;
    assert_eq!(runner.next_due().await.unwrap(), Some(Duration::ZERO));
}
