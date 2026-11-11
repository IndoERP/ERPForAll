//! stage B: enqueue inside the business transaction (1, D7.4, design §5.1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::time::Duration;

use erp_fields::{ErrorKind, TimestampTz};
use erp_orm::jobs::{
    EnqueueOutcome, EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKey,
    JobKindDecl, MAX_ENQUEUE_BATCH, PrincipalPolicy, Priority, RunAs, RunOutcome, When,
};
use erp_orm::{BeginOptions, BoxFuture, CorrelationId, OperationIdentity, UserId};
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
    PLAIN,
    JobKindDecl::new("test_jobs", "test_jobs.plain", HandlerRef::Native(&NOP))
        .version(3)
        .accepts(1..=3)
        .max_attempts(4)
        .priority(Priority::DEFAULT)
);
erp_orm::register_job_kind!(
    SYSTEM_KIND,
    JobKindDecl::new("base", "base.test_system", HandlerRef::Native(&NOP))
        .principal(PrincipalPolicy::System)
);

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

async fn count(h: &Harness) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let n = support::one(&mut tx, "SELECT count(*)::text FROM erp_job")
        .await
        .unwrap();
    n.parse().unwrap()
}

async fn cell(h: &Harness, sql: &str) -> String {
    let mut tx = h.begin().await.unwrap();
    support::one(&mut tx, sql).await.unwrap_or_default()
}

#[tokio::test]
async fn enqueue_inserts_one_row_with_the_callers_principal_correlation_and_kind_version() {
    let h = harness().await;
    let identity = OperationIdentity::new(CorrelationId::generate().unwrap()).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(UserId::new(7).unwrap()).operation_identity(identity.clone()))
        .await
        .unwrap();
    let env = tx.env();
    tx.capture_statements(true);
    let outcome = tx
        .enqueue(
            &env,
            EnqueueRequest::new("test_jobs.plain")
                .payload(&serde_json::json!({"warehouse": 3}))
                .unwrap(),
        )
        .await
        .unwrap();
    let statements = tx.captured().to_vec();
    // one INSERT, and the coalesced wake-up NOTIFY a due job sends (at most one per transaction)
    assert_eq!(statements.len(), 2, "{statements:?}");
    assert!(statements[1].contains("pg_notify"), "{statements:?}");
    assert!(
        statements[0].starts_with("INSERT INTO erp_job"),
        "{statements:?}"
    );
    let EnqueueOutcome::Inserted(job) = outcome else {
        panic!("a new job: {outcome:?}");
    };
    tx.commit().await.unwrap();
    // a rollback leaves no row
    let mut rolled = h.begin_as(7).await.unwrap();
    let renv = rolled.env();
    rolled
        .enqueue(&renv, EnqueueRequest::new("test_jobs.plain"))
        .await
        .unwrap();
    rolled.rollback().await.unwrap();
    let row = cell(
        &h,
        &format!(
            "SELECT concat_ws('|', id, job_uuid, kind, kind_version, state, priority, max_attempts, \
             run_as_kind, run_as_user, run_as_companies, enqueued_by_kind, enqueued_by_user, \
             correlation_id, causation_operation, payload, attempts, lease_epoch) \
             FROM erp_job WHERE id = {}",
            job.id()
        ),
    )
    .await;
    assert_eq!(
        row,
        format!(
            "{}|{}|test_jobs.plain|3|ready|5|4|user|7|{{}}|user|7|{}|{}|{{\"warehouse\": 3}}|0|0",
            job.id(),
            job.uuid(),
            identity.correlation(),
            identity.operation()
        )
    );
    assert_eq!(count(&h).await, 1);
}

/// this one)
#[tokio::test]
async fn a_rollback_leaves_no_job() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    tx.enqueue(&env, EnqueueRequest::new("test_jobs.plain"))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(count(&h).await, 0);
}

#[tokio::test]
async fn an_unknown_kind_is_refused() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let error = tx
        .enqueue(&env, EnqueueRequest::new("test_jobs.nope"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "job kind 'test_jobs.nope' is not registered in this binary (register_job_kind!)"
    );
    tx.rollback().await.unwrap();
    assert_eq!(count(&h).await, 0);
}

#[tokio::test]
async fn an_oversized_payload_and_an_invalid_key_are_refused_with_their_sizes() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let big = serde_json::json!({ "p": "x".repeat(70_000) });
    let error = tx
        .enqueue(
            &env,
            EnqueueRequest::new("test_jobs.plain")
                .payload(&big)
                .unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "the payload of a 'test_jobs.plain' job is 70009 bytes; the limit is 65536 bytes \
         (jobs.payload_max_bytes)"
    );
    // exactly at the limit passes (a payload of 65536 bytes as `jsonb` prints it)
    let fits = serde_json::json!({ "p": "x".repeat(65_536 - 9) });
    tx.enqueue(
        &env,
        EnqueueRequest::new("test_jobs.plain")
            .payload(&fits)
            .unwrap(),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();

    for (key, message) in [
        (
            String::new(),
            "invalid job key: it is empty (a key is 1..=200 bytes without NUL or control characters)",
        ),
        (
            "k".repeat(201),
            "invalid job key: it is 201 bytes (a key is 1..=200 bytes without NUL or control characters)",
        ),
        (
            "a\0b".to_owned(),
            "invalid job key: it contains a control character (a key is 1..=200 bytes without NUL or control characters)",
        ),
    ] {
        let error = JobKey::new(&key).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), message);
    }
    assert!(JobKey::new(&"k".repeat(200)).is_ok());
}

/// built yet, their refusals are unit-tested against the constructors' own errors)
#[tokio::test]
async fn run_as_another_principal_needs_the_audited_elevation_except_for_the_system() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let error = tx
        .enqueue(
            &env,
            EnqueueRequest::new("test_jobs.plain").run_as(RunAs::User(UserId::new(9).unwrap())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "running a job as another principal needs the 'jobs.enqueue_as' elevation, which is \
         recorded by the audit trail of a later release; that hook is not available yet, so the \
         enqueue is refused"
    );
    tx.rollback().await.unwrap();
    assert_eq!(count(&h).await, 0);

    // the system principal may name the user
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    tx.enqueue(
        &env,
        EnqueueRequest::new("test_jobs.plain").run_as(RunAs::User(UserId::new(9).unwrap())),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            "SELECT concat_ws('|', run_as_kind, run_as_user, enqueued_by_kind, enqueued_by_user) FROM erp_job"
        )
        .await,
        "user|9|system|1"
    );
}

#[tokio::test]
async fn a_system_kind_stores_the_system_principal_whoever_enqueues_and_refuses_run_as() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    tx.enqueue(&env, EnqueueRequest::new("base.test_system"))
        .await
        .unwrap();
    let error = tx
        .enqueue(
            &env,
            EnqueueRequest::new("base.test_system").run_as(RunAs::User(UserId::new(9).unwrap())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "job kind 'base.test_system' runs as the system principal; run_as does not apply to it"
    );
    tx.rollback().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    tx.enqueue(&env, EnqueueRequest::new("base.test_system"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            "SELECT concat_ws('|', run_as_kind, run_as_user, enqueued_by_kind, enqueued_by_user) FROM erp_job"
        )
        .await,
        "system|1|user|7"
    );
}

#[tokio::test]
async fn the_batch_and_attempt_bounds_are_checked_before_anything_is_sent() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let batch: Vec<EnqueueRequest> = (0..=MAX_ENQUEUE_BATCH)
        .map(|_| EnqueueRequest::new("test_jobs.plain"))
        .collect();
    let error = tx.enqueue_many(&env, batch).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "enqueue_many takes at most 1024 jobs per call (got 1025); split the batch"
    );
    assert!(tx.enqueue_many(&env, Vec::new()).await.unwrap().is_empty());
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn enqueue_many_is_one_insert_for_a_hundred_jobs_with_outcomes_in_order() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    tx.capture_statements(true);
    let batch: Vec<EnqueueRequest> = (0..100)
        .map(|i| {
            EnqueueRequest::new("test_jobs.plain")
                .payload(&serde_json::json!({ "n": i }))
                .unwrap()
        })
        .collect();
    let outcomes = tx.enqueue_many(&env, batch).await.unwrap();
    // one INSERT for the hundred jobs, plus the one wake-up NOTIFY of the transaction
    let inserts = tx
        .captured()
        .iter()
        .filter(|s| s.starts_with("INSERT INTO erp_job"))
        .count();
    assert_eq!(
        (inserts, tx.captured().len()),
        (1, 2),
        "{:?}",
        tx.captured()
    );
    assert_eq!(outcomes.len(), 100);
    tx.commit().await.unwrap();
    for (i, outcome) in outcomes.iter().enumerate() {
        assert!(outcome.is_new());
        let n = cell(
            &h,
            &format!(
                "SELECT payload->>'n' FROM erp_job WHERE id = {}",
                outcome.job().id()
            ),
        )
        .await;
        assert_eq!(n, i.to_string());
    }
}

#[tokio::test]
async fn due_times_follow_now_after_and_at() {
    let h = harness().await;
    let at = TimestampTz::from_unix_micros(4_102_444_800_000_000).unwrap(); // 2100-01-01
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let now = tx
        .enqueue(&env, EnqueueRequest::new("test_jobs.plain"))
        .await
        .unwrap()
        .job();
    let later = tx
        .enqueue(
            &env,
            EnqueueRequest::new("test_jobs.plain").at(When::After(Duration::from_secs(600))),
        )
        .await
        .unwrap()
        .job();
    let fixed = tx
        .enqueue(
            &env,
            EnqueueRequest::new("test_jobs.plain").at(When::At(at)),
        )
        .await
        .unwrap()
        .job();
    tx.commit().await.unwrap();
    let secs = |id: i64| {
        format!(
            "SELECT round(extract(epoch FROM run_after - enqueued_at))::text FROM erp_job WHERE id = {id}"
        )
    };
    assert_eq!(cell(&h, &secs(now.id())).await, "0");
    assert_eq!(cell(&h, &secs(later.id())).await, "600");
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT (run_after = '2100-01-01 00:00:00+00')::text FROM erp_job WHERE id = {}",
                fixed.id()
            )
        )
        .await,
        "true"
    );
}

#[tokio::test]
async fn priority_and_attempts_can_override_the_kinds_defaults() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let job = tx
        .enqueue(
            &env,
            EnqueueRequest::new("test_jobs.plain")
                .priority(Priority::new(0).unwrap())
                .max_attempts(9)
                .unwrap(),
        )
        .await
        .unwrap()
        .job();
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', priority, max_attempts) FROM erp_job WHERE id = {}",
                job.id()
            )
        )
        .await,
        "0|9"
    );
}
