//! Recurring jobs an addon declares on its own step kind (`JobKindDecl::schedule`): the module
//! system keeps one row per kind, it runs as the technical user the schedule names, once per due
//! time whatever the number of runners, and reschedules itself.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::time::Duration;

use erp_orm::StaticPolicy;
use erp_orm::jobs::{EnqueueRequest, HandlerRef, JobKindDecl, Schedule, sync_addon_schedules};
use erp_test_models::base_users;
use erp_test_models::test_jobs_fixtures::test_jobs;
use erp_testkit::Harness;
use support::jobs::{cell, exec, runner};

const KIND: &str = "test_jobs.hourly";
const HOUR_MICROS: i64 = 3_600_000_000;

erp_orm::register_job_kind!(
    HOURLY,
    JobKindDecl::new(
        "test_jobs",
        KIND,
        HandlerRef::Step {
            model: "test_jobs.recount",
            method: "run_scheduled_step"
        }
    )
    .schedule(Schedule::every(
        Duration::from_secs(3_600),
        "test_jobs.user_scheduler"
    ))
);

/// `test_jobs` with two internal users (7 is the scheduler, 8 another) and the external id the
/// schedule names; the harness has neither `ir_model_data` nor `share`, so they are made here.
async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_jobs::ADDON])
        .policy(StaticPolicy::AllowAll)
        .build()
        .await
        .expect("harness");
    exec(
        &h,
        "ALTER TABLE res_users ADD COLUMN IF NOT EXISTS share boolean NOT NULL DEFAULT false;
         CREATE TABLE IF NOT EXISTS ir_model_data (
             id int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY, module text NOT NULL,
             name text NOT NULL, model text NOT NULL, res_id int8, noupdate boolean DEFAULT false);
         INSERT INTO res_users (id, name, login, active) OVERRIDING SYSTEM VALUE
             VALUES (7, 'scheduler', 'scheduler', true), (8, 'other', 'other', true);
         INSERT INTO ir_model_data (module, name, model, res_id)
             VALUES ('test_jobs', 'user_scheduler', 'res.users', 7);",
    )
    .await;
    h
}

/// Run the module system's sync for `kept` and `removed` as the system, committing.
async fn sync(h: &Harness, kept: &[&str], removed: &[&str]) -> erp_fields::Result<()> {
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    match sync_addon_schedules(&mut tx, &env, kept, removed).await {
        Ok(()) => tx.commit().await,
        Err(error) => {
            tx.rollback().await.unwrap();
            Err(error)
        }
    }
}

async fn rows(h: &Harness) -> String {
    cell(
        h,
        "SELECT coalesce(string_agg(name, ',' ORDER BY id), '') FROM test_jobs_recount",
    )
    .await
}

async fn job(h: &Harness) -> String {
    cell(
        h,
        &format!(
            "SELECT coalesce(string_agg(concat_ws('|', state, run_as_kind, run_as_user, \
             singleton_key), ','), '') FROM erp_job WHERE kind = '{KIND}'"
        ),
    )
    .await
}

/// `micros` as the step sees it (whole seconds, UTC, `Z`).
async fn rfc3339(h: &Harness, micros: i64) -> String {
    cell(
        h,
        &format!(
            "SELECT to_char(to_timestamp({micros}::float8 / 1000000) AT TIME ZONE 'UTC', \
             'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        ),
    )
    .await
}

async fn now_micros(h: &Harness) -> i64 {
    cell(
        h,
        "SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::int8::text",
    )
    .await
    .parse()
    .unwrap()
}

async fn progress_micros(h: &Harness, key: &str) -> i64 {
    cell(
        h,
        &format!("SELECT progress->>'{key}' FROM erp_job WHERE kind = '{KIND}'"),
    )
    .await
    .parse()
    .unwrap()
}

#[tokio::test]
async fn install_keeps_one_row_due_at_the_next_aligned_instant_and_upgrade_keeps_it() {
    let h = harness().await;
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    assert_eq!(job(&h).await, format!("ready|user|7|{KIND}"));
    // due at the top of the next UTC hour, in the future (concat_ws prints booleans as t/f)
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', run_after > clock_timestamp(), \
                 run_after <= clock_timestamp() + interval '1 hour', \
                 run_after = date_trunc('hour', run_after)) FROM erp_job WHERE kind = '{KIND}'"
            )
        )
        .await,
        "t|t|t"
    );
    // an operator's hold survives an upgrade
    exec(&h, "UPDATE erp_job SET state = 'held'").await;
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    assert_eq!(job(&h).await, format!("held|user|7|{KIND}"));
    // another addon's change leaves it alone
    sync(&h, &["base_users"], &["other_addon"]).await.unwrap();
    assert_eq!(job(&h).await, format!("held|user|7|{KIND}"));
}

#[tokio::test]
async fn a_due_occurrence_runs_once_across_two_runners_and_reschedules_itself() {
    let h = harness().await;
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    exec(&h, "UPDATE erp_job SET run_after = clock_timestamp()").await;
    let (first, second) = (runner(&h), runner(&h));
    let (a, b) = tokio::join!(first.tick(), second.tick());
    assert_eq!(a.unwrap().claimed + b.unwrap().claimed, 1);
    // a later tick finds nothing due
    assert_eq!(first.tick().await.unwrap().claimed, 0);
    let occurrence = progress_micros(&h, "previous").await;
    assert_eq!(occurrence % HOUR_MICROS, 0);
    assert_eq!(progress_micros(&h, "next").await, occurrence + HOUR_MICROS);
    // both chunks of the occurrence saw its instant and no previous occurrence
    let at = rfc3339(&h, occurrence).await;
    assert_eq!(
        rows(&h).await,
        format!("at:{at}|prev:none|n:0,at:{at}|prev:none|n:1")
    );
    // the row stays, ready at the next instant, attempts reset; one history row per occurrence
    assert_eq!(job(&h).await, format!("ready|user|7|{KIND}"));
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', attempts, \
                 (extract(epoch FROM run_after) * 1000000)::int8 = {}) FROM erp_job",
                occurrence + HOUR_MICROS
            )
        )
        .await,
        "0|t"
    );
    assert_eq!(
        cell(
            &h,
            "SELECT string_agg(concat_ws('|', kind, outcome, items_done, run_as_user), ',') \
             FROM erp_job_run"
        )
        .await,
        format!("{KIND}|done|2|7")
    );
}

#[tokio::test]
async fn missed_occurrences_collapse_into_one_that_names_the_last_completed() {
    let h = harness().await;
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    let now = now_micros(&h).await;
    let previous = (now / HOUR_MICROS - 6) * HOUR_MICROS;
    let next = previous + HOUR_MICROS;
    exec(
        &h,
        &format!(
            "UPDATE erp_job SET run_after = clock_timestamp(), progress = jsonb_build_object( \
             'current', NULL, 'previous', {previous}::int8, 'next', {next}::int8, 'items', 0, \
             'step', NULL)"
        ),
    )
    .await;
    assert_eq!(runner(&h).tick().await.unwrap().claimed, 1);
    let occurrence = progress_micros(&h, "previous").await;
    // one run, for the latest missed instant, not one per missed hour
    assert!(occurrence >= next + 4 * HOUR_MICROS, "{occurrence}");
    let (at, prev) = (rfc3339(&h, occurrence).await, rfc3339(&h, previous).await);
    assert_eq!(
        rows(&h).await,
        format!("at:{at}|prev:{prev}|n:0,at:{at}|prev:{prev}|n:1")
    );
    assert_eq!(
        cell(&h, "SELECT count(*)::text FROM erp_job_run").await,
        "1"
    );
}

#[tokio::test]
async fn a_row_claimed_before_its_stored_next_instant_waits_for_it() {
    // the finishing chunk stored the next instant but the reschedule was lost (a crash)
    let h = harness().await;
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    let next = (now_micros(&h).await / HOUR_MICROS + 2) * HOUR_MICROS;
    exec(
        &h,
        &format!(
            "UPDATE erp_job SET run_after = clock_timestamp(), progress = jsonb_build_object( \
             'current', NULL, 'previous', NULL, 'next', {next}::int8, 'items', 0, 'step', NULL)"
        ),
    )
    .await;
    assert_eq!(runner(&h).tick().await.unwrap().claimed, 1);
    assert_eq!(rows(&h).await, "");
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT ((extract(epoch FROM run_after) * 1000000)::int8 = {next})::text \
                 FROM erp_job"
            )
        )
        .await,
        "true"
    );
}

#[tokio::test]
async fn a_scheduled_kind_cannot_be_enqueued_by_code() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let error = tx
        .enqueue(&env, EnqueueRequest::new(KIND))
        .await
        .unwrap_err();
    assert_eq!(
        error.message(),
        "job kind 'test_jobs.hourly' is a scheduled kind: its one job row is kept by the module \
         system from its schedule and it cannot be enqueued"
    );
}

#[tokio::test]
async fn a_changed_scheduler_user_replaces_the_row_and_a_bad_one_is_refused() {
    let h = harness().await;
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    exec(&h, "UPDATE ir_model_data SET res_id = 8").await;
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    assert_eq!(job(&h).await, format!("ready|user|8|{KIND}"));

    exec(&h, "UPDATE res_users SET share = true WHERE id = 8").await;
    let error = sync(&h, &["test_jobs"], &[]).await.unwrap_err();
    assert_eq!(
        error.message(),
        "job kind 'test_jobs.hourly' runs as 'test_jobs.user_scheduler' (user 8), which must be \
         an active internal user (active and not share)"
    );
    exec(
        &h,
        "UPDATE res_users SET share = false, active = false WHERE id = 8",
    )
    .await;
    assert!(
        sync(&h, &["test_jobs"], &[])
            .await
            .unwrap_err()
            .message()
            .contains("must be an active internal user")
    );
    exec(&h, "DELETE FROM ir_model_data").await;
    assert_eq!(
        sync(&h, &["test_jobs"], &[]).await.unwrap_err().message(),
        "job kind 'test_jobs.hourly' runs as 'test_jobs.user_scheduler', but no res.users record \
         has that external id; ship the technical user in the addon's data"
    );
    // nothing of a refused sync was kept
    assert_eq!(job(&h).await, format!("ready|user|8|{KIND}"));
}

#[tokio::test]
async fn uninstall_removes_the_row_but_not_while_it_runs() {
    let h = harness().await;
    sync(&h, &["test_jobs"], &[]).await.unwrap();
    exec(
        &h,
        "UPDATE erp_job SET state = 'running', lease_owner = gen_random_uuid(), \
         lease_until = clock_timestamp() + interval '1 hour'",
    )
    .await;
    assert_eq!(
        sync(&h, &[], &["test_jobs"]).await.unwrap_err().message(),
        "the scheduled job 'test_jobs.hourly' of addon 'test_jobs' is running; uninstall the \
         addon again when it has finished"
    );
    // an expired lease is not a run: the row goes, dead letters too
    exec(
        &h,
        "UPDATE erp_job SET lease_until = clock_timestamp() - interval '1 minute'",
    )
    .await;
    sync(&h, &[], &["test_jobs"]).await.unwrap();
    assert_eq!(job(&h).await, "");
}

#[tokio::test]
async fn only_the_system_keeps_schedules() {
    let h = harness().await;
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let error = sync_addon_schedules(&mut tx, &env, &["test_jobs"], &[])
        .await
        .unwrap_err();
    assert_eq!(
        error.message(),
        "the schedules of addon job kinds are kept by the system principal only"
    );
}
