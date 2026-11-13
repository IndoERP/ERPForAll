//! stage D: administration, history and retention (D14).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_fields::ErrorKind;
use erp_orm::jobs::{CancelOutcome, HandlerRef, JobKey, JobKindDecl, JobRef, RecurringSpec, When};
use erp_orm::{Orm, StaticPolicy, UserId};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{SCRIPT, cell, enqueue, exec, runner, script};

erp_orm::register_job_kind!(
    SCRIPT_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.script", HandlerRef::Native(&SCRIPT)).max_attempts(2)
);

async fn harness(policy: StaticPolicy) -> Harness {
    Harness::builder([&base_users::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness")
}

/// An ORM on the harness database that knows which capability administers jobs.
fn orm_with_admin_capability(h: &Harness) -> Orm {
    h.orm()
        .rebuild(h.registry().clone())
        .job_admin_capability(&erp_security::jobs::JOBS_ADMIN)
        .build()
        .unwrap()
}

async fn as_system(h: &Harness) -> (erp_orm::Transaction, erp_orm::Env) {
    let tx = h.begin().await.unwrap();
    let env = tx.env();
    (tx, env)
}

async fn history(h: &Harness) -> String {
    cell(
        h,
        "SELECT coalesce(string_agg(outcome, ',' ORDER BY finished_at, outcome), '') FROM erp_job_run",
    )
    .await
}

/// Kill a job: make it a dead letter the way an exhausted run would.
async fn kill(h: &Harness, job: JobRef) {
    exec(
        h,
        &format!(
            "UPDATE erp_job SET state = 'dead', dead_at = clock_timestamp(), last_error_code = 'broken', \
             last_error = 'cannot be fixed' WHERE id = {}",
            job.id()
        ),
    )
    .await;
}

#[tokio::test]
async fn a_held_job_is_not_claimed_until_it_is_resumed() {
    let h = harness(StaticPolicy::AllowAll).await;
    let jobs = enqueue(&h, vec![script("native_done")]).await;
    let (mut tx, env) = as_system(&h).await;
    tx.job_hold(&env, jobs[0]).await.unwrap();
    tx.commit().await.unwrap();
    let runner = runner(&h);
    assert_eq!(runner.tick().await.unwrap().claimed, 0);
    let (mut tx, env) = as_system(&h).await;
    tx.job_resume(&env, jobs[0]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(runner.tick().await.unwrap().claimed, 1);
    // a job that is not ready cannot be held, and the error names its state
    let (mut tx, env) = as_system(&h).await;
    let error = tx.job_hold(&env, jobs[0]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Missing);
    assert_eq!(
        error.message(),
        format!(
            "job_hold: job {} does not exist (it may have finished)",
            jobs[0].uuid()
        )
    );
    let _ = tx.rollback().await;
}

#[tokio::test]
async fn cancelling_a_waiting_job_deletes_it_and_leaves_a_cancelled_history_row() {
    let h = harness(StaticPolicy::AllowAll).await;
    let jobs = enqueue(&h, vec![script("native_done"), script("native_done")]).await;
    let (mut tx, env) = as_system(&h).await;
    tx.job_hold(&env, jobs[1]).await.unwrap();
    assert_eq!(
        tx.job_cancel(&env, jobs[0]).await.unwrap(),
        CancelOutcome::Deleted
    );
    assert_eq!(
        tx.job_cancel(&env, jobs[1]).await.unwrap(),
        CancelOutcome::Deleted
    );
    tx.commit().await.unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(history(&h).await, "cancelled,cancelled");
}

#[tokio::test]
async fn cancelling_a_running_job_stops_it_at_its_next_fence() {
    let h = harness(StaticPolicy::AllowAll).await;
    let jobs = enqueue(&h, vec![script("ok")]).await;
    let runner = runner(&h);
    let claimed = runner.claim(1).await.unwrap();
    let (mut tx, env) = as_system(&h).await;
    assert_eq!(
        tx.job_cancel(&env, jobs[0]).await.unwrap(),
        CancelOutcome::Requested
    );
    tx.commit().await.unwrap();
    assert_eq!(support::jobs::state(&h, jobs[0]).await, "running");
    runner
        .run_claimed(claimed.into_iter().next().unwrap())
        .await
        .unwrap();
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "0");
    assert_eq!(history(&h).await, "cancelled");
    assert_eq!(runner.metrics().cancelled, 1);
}

#[tokio::test]
async fn a_dead_letter_can_be_requeued_or_discarded() {
    let h = harness(StaticPolicy::AllowAll).await;
    let jobs = enqueue(&h, vec![script("native_done"), script("native_done")]).await;
    kill(&h, jobs[0]).await;
    kill(&h, jobs[1]).await;
    let (mut tx, env) = as_system(&h).await;
    tx.job_requeue(&env, jobs[0]).await.unwrap();
    tx.job_discard(&env, jobs[1]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', state, attempts, failure_rank, dead_at IS NULL) FROM erp_job WHERE id = {}",
                jobs[0].id()
            )
        )
        .await,
        "ready|0|0|t"
    );
    assert_eq!(cell(&h, "SELECT count(*)::text FROM erp_job").await, "1");
    assert_eq!(history(&h).await, "dead_discarded");
    // a job that is not dead is refused with its state
    let (mut tx, env) = as_system(&h).await;
    let error = tx.job_requeue(&env, jobs[0]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        format!("job_requeue: job {} is ready, not dead", jobs[0].uuid())
    );
    let _ = tx.rollback().await;
    // the requeued job runs
    assert_eq!(runner(&h).tick().await.unwrap().claimed, 1);
    assert_eq!(history(&h).await, "dead_discarded,done");
}

#[tokio::test]
async fn a_user_needs_the_capability_to_administer_jobs() {
    let denied = harness(StaticPolicy::DenyAll).await;
    let jobs = enqueue(&denied, vec![script("native_done")]).await;
    // without a configured capability a user's attempt is a coding error, not a silent refusal
    let mut tx = denied.begin_as(7).await.unwrap();
    let env = tx.env();
    let error = tx.job_hold(&env, jobs[0]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "job_hold needs the capability that authorizes job administration, and this ORM was \
         built without one (OrmBuilder::job_admin_capability)"
    );
    let _ = tx.rollback().await;
    // with it, a user who does not hold it is refused ...
    let orm = orm_with_admin_capability(&denied);
    let mut tx = orm.begin(UserId::new(7).unwrap()).await.unwrap();
    let env = tx.env();
    let error = tx.job_hold(&env, jobs[0]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "job_hold needs the 'jobs.admin' capability (the system administrators group)"
    );
    let _ = tx.rollback().await;
    // ... and one who does is let through
    let allowed = harness(StaticPolicy::AllowAll).await;
    let jobs = enqueue(&allowed, vec![script("native_done")]).await;
    let orm = orm_with_admin_capability(&allowed);
    let mut tx = orm.begin(UserId::new(7).unwrap()).await.unwrap();
    let env = tx.env();
    tx.job_hold(&env, jobs[0]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(support::jobs::state(&allowed, jobs[0]).await, "held");
}

#[tokio::test]
async fn dead_letters_past_the_retention_move_to_history_and_history_is_trimmed() {
    let h = harness(StaticPolicy::AllowAll).await;
    let jobs = enqueue(
        &h,
        vec![
            script("native_done"),
            script("native_done"),
            script("native_done"),
        ],
    )
    .await;
    for job in &jobs {
        kill(&h, *job).await;
    }
    // two died long ago, one just now
    exec(
        &h,
        &format!(
            "UPDATE erp_job SET dead_at = clock_timestamp() - interval '31 days' WHERE id IN ({}, {})",
            jobs[0].id(),
            jobs[1].id()
        ),
    )
    .await;
    // history: one row 8 days old, one fresh
    exec(
        &h,
        "INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, \
         finished_at, items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user) \
         VALUES (gen_random_uuid(), 'a.b', 1, 'done', 1, now(), now() - interval '8 days', 0, 0, 'user', 7, 7), \
                (gen_random_uuid(), 'a.b', 1, 'done', 1, now(), now(), 0, 0, 'user', 7, 7)",
    )
    .await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(tx.job_expire_dead(30, 100).await.unwrap(), 2);
    assert_eq!(tx.job_purge_history(7, 100).await.unwrap(), 1);
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE state = 'dead'"
        )
        .await,
        "1"
    );
    // the two expired letters left a dead_expired row each; the old history row is gone
    assert_eq!(history(&h).await, "done,dead_expired,dead_expired");
    // upkeep is for the system principal
    let mut tx = h.begin_as(7).await.unwrap();
    let error = tx.job_purge_history(7, 10).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "job_purge_history is the queue's own upkeep and is for the system principal"
    );
    let _ = tx.rollback().await;
}

#[tokio::test]
async fn retention_removes_at_most_one_batch_per_call() {
    let h = harness(StaticPolicy::AllowAll).await;
    exec(
        &h,
        "INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, \
         finished_at, items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user) \
         SELECT gen_random_uuid(), 'a.b', 1, 'done', 1, now(), now() - interval '30 days', 0, 0, 'user', 7, 7 \
         FROM generate_series(1, 25)",
    )
    .await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(tx.job_purge_history(7, 10).await.unwrap(), 10);
    assert_eq!(tx.job_purge_history(7, 10).await.unwrap(), 10);
    assert_eq!(tx.job_purge_history(7, 10).await.unwrap(), 5);
    assert_eq!(tx.job_purge_history(7, 10).await.unwrap(), 0);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn the_core_schedules_exist_once_and_their_jobs_trim_history() {
    use erp_orm::jobs::{JobRunner, ensure_core_schedules};
    let h = harness(StaticPolicy::AllowAll).await;
    let _ = erp_security::jobs::core_job_kinds();
    ensure_core_schedules(h.orm()).await.unwrap();
    ensure_core_schedules(h.orm()).await.unwrap();
    assert_eq!(
        cell(
            &h,
            "SELECT string_agg(kind || ':' || coalesce(singleton_key, '-') || ':' || run_as_kind, ',' ORDER BY kind) FROM erp_job"
        )
        .await,
        "core.jobs_dead_expiry:core.jobs_dead_expiry:system,\
         core.jobs_history_retention:core.jobs_history_retention:system"
    );
    exec(
        &h,
        "INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, \
         finished_at, items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user) \
         VALUES (gen_random_uuid(), 'a.b', 1, 'done', 1, now(), now() - interval '9 days', 0, 0, 'user', 7, 7)",
    )
    .await;
    exec(&h, "UPDATE erp_job SET run_after = clock_timestamp()").await;
    let runner = JobRunner::builder(
        h.orm().clone(),
        erp_security::jobs::SecurityJobPrincipalResolver,
    )
    .build()
    .unwrap();
    assert_eq!(runner.tick().await.unwrap().claimed, 2);
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM erp_job_run").await,
        "0"
    );
    // each rescheduled itself an hour ahead: still exactly one row each, ready, not due
    assert_eq!(
        cell(
            &h,
            "SELECT string_agg(state || ':' || (run_after > clock_timestamp() + interval '50 minutes')::text, ',') FROM erp_job"
        )
        .await,
        "ready:true,ready:true"
    );
}

#[tokio::test]
async fn a_recurring_row_is_kept_once_and_rewritten_in_place() {
    let h = harness(StaticPolicy::AllowAll).await;
    let key = JobKey::new("script:1").unwrap();
    let user = UserId::new(7).unwrap();
    let spec = || {
        RecurringSpec::new("test_jobs.script", key.clone())
            .run_as(user)
            .first_run(When::After(std::time::Duration::from_secs(3_600)))
    };
    let (mut tx, env) = as_system(&h).await;
    let first = tx.job_upsert_recurring(&env, spec()).await.unwrap();
    let again = tx
        .job_upsert_recurring(&env, spec().held(true))
        .await
        .unwrap();
    assert_eq!(first, again);
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            "SELECT concat_ws('|', count(*), min(state), min(dedup_key), min(singleton_key), min(run_as_user)) FROM erp_job"
        )
        .await,
        "1|held|script:1|script:1|7"
    );
    // a different user replaces the row (the stored principal is never updated in place)
    let (mut tx, env) = as_system(&h).await;
    let other = tx
        .job_upsert_recurring(&env, spec().run_as(UserId::new(9).unwrap()))
        .await
        .unwrap();
    assert_ne!(other, first);
    // moving the due time earlier (a trigger)
    let at = erp_fields::TimestampTz::from_unix_micros(4_102_444_800_000_000).unwrap();
    assert!(
        tx.job_set_run_after(&env, "test_jobs.script", &key, at)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            "SELECT concat_ws('|', count(*), min(run_as_user), (min(run_after) = '2100-01-01 00:00:00+00')::text) FROM erp_job"
        )
        .await,
        "1|9|true"
    );
    // a running schedule is not rewritten
    exec(
        &h,
        "UPDATE erp_job SET state = 'ready', run_after = clock_timestamp()",
    )
    .await;
    let claimed = runner(&h).claim(1).await.unwrap();
    assert_eq!(claimed.len(), 1);
    let (mut tx, env) = as_system(&h).await;
    let error = tx
        .job_upsert_recurring(&env, spec().run_as(UserId::new(9).unwrap()))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "the recurring job script:1 of kind 'test_jobs.script' is running and is not rewritten"
    );
    let _ = tx.rollback().await;
    // and only the system principal keeps schedules
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let error = tx.job_upsert_recurring(&env, spec()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    let _ = tx.rollback().await;
}

/// Two item-failure rows for `job`.
async fn add_failures(h: &Harness, job: JobRef) {
    exec(
        h,
        &format!(
            "INSERT INTO erp_job_item_failure (job_uuid, item_key, error_code, error) \
             VALUES ('{u}', 'a', 'item', 'failed'), ('{u}', 'b', 'item', 'failed')",
            u = job.uuid()
        ),
    )
    .await;
}

async fn failure_rows(h: &Harness) -> String {
    cell(h, "SELECT count(*)::text FROM erp_job_item_failure").await
}

#[tokio::test]
async fn cancelling_discarding_or_expiring_a_job_removes_its_item_failures() {
    let h = harness(StaticPolicy::AllowAll).await;
    let jobs = enqueue(
        &h,
        vec![
            script("native_done"),
            script("native_done"),
            script("native_done"),
        ],
    )
    .await;
    for job in &jobs {
        add_failures(&h, *job).await;
    }
    assert_eq!(failure_rows(&h).await, "6");
    kill(&h, jobs[1]).await;
    kill(&h, jobs[2]).await;
    exec(
        &h,
        &format!(
            "UPDATE erp_job SET dead_at = clock_timestamp() - interval '31 days' WHERE id = {}",
            jobs[2].id()
        ),
    )
    .await;
    let (mut tx, env) = as_system(&h).await;
    tx.job_cancel(&env, jobs[0]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(failure_rows(&h).await, "4", "cancel");
    let (mut tx, env) = as_system(&h).await;
    tx.job_discard(&env, jobs[1]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(failure_rows(&h).await, "2", "discard");
    let mut tx = h.begin().await.unwrap();
    assert_eq!(tx.job_expire_dead(30, 100).await.unwrap(), 1);
    tx.commit().await.unwrap();
    assert_eq!(failure_rows(&h).await, "0", "expiry");
}

#[tokio::test]
async fn purging_history_removes_orphan_item_failures_and_keeps_the_others() {
    let h = harness(StaticPolicy::AllowAll).await;
    let jobs = enqueue(&h, vec![script("native_done")]).await;
    add_failures(&h, jobs[0]).await;
    // a finished job with a fresh history row keeps its failures; one with an old row loses them
    exec(
        &h,
        "INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, \
         finished_at, items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user) \
         VALUES ('00000000-0000-7000-8000-000000000001', 'a.b', 1, 'done', 1, now(), now(), 0, 2, 'user', 7, 7), \
                ('00000000-0000-7000-8000-000000000002', 'a.b', 1, 'done', 1, now(), now() - interval '8 days', 0, 2, 'user', 7, 7)",
    )
    .await;
    exec(
        &h,
        "INSERT INTO erp_job_item_failure (job_uuid, item_key, error_code, error) VALUES \
         ('00000000-0000-7000-8000-000000000001', 'a', 'item', 'x'), \
         ('00000000-0000-7000-8000-000000000002', 'a', 'item', 'x'), \
         ('00000000-0000-7000-8000-000000000003', 'a', 'item', 'x')",
    )
    .await;
    assert_eq!(failure_rows(&h).await, "5");
    let mut tx = h.begin().await.unwrap();
    assert_eq!(tx.job_purge_history(7, 100).await.unwrap(), 1);
    tx.commit().await.unwrap();
    // gone: the old history's failures and the ones with no job and no history; kept: the queued
    // job's two and the fresh history's one
    assert_eq!(failure_rows(&h).await, "3");
    assert_eq!(
        cell(
            &h,
            "SELECT string_agg(job_uuid::text, ',' ORDER BY job_uuid::text) FROM erp_job_item_failure \
             WHERE job_uuid::text LIKE '00000000%'"
        )
        .await,
        "00000000-0000-7000-8000-000000000001"
    );
}
