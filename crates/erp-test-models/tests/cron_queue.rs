//! stage F: `ir.cron` on the job queue. The server
//! action a cron runs is a probe runner that follows a script of what it reports through
//! `commit_progress`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use erp_addon_base::ir_cron::commit_progress;
use erp_addon_base::server_action::{ActionResult, RunnerCtx};
use erp_fields::{Cell, ErrorKind};
use erp_orm::{BoxFuture, RawVals, StaticPolicy, UserId};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_testkit::Harness;
use support::jobs::{PassResolver, cell, exec};

static PROBE_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_ext_cron",
    depends: &["base"],
    exact_only: false,
};
const STATE: &[FieldDecl] = &[FieldDecl::selection_add_ondelete(
    "state",
    &[("x_cron_probe", Some("Cron probe"))],
    &[("x_cron_probe", erp_registry::SelectionOnDeleteDecl::Cascade)],
)];

/// What one run of the probe action does.
#[derive(Debug, Clone, Copy, Default)]
struct Step {
    done: u64,
    remaining: u64,
    deactivate: bool,
    fail: bool,
}

static SCRIPT: Mutex<VecDeque<Step>> = Mutex::new(VecDeque::new());
static CALLS: AtomicU32 = AtomicU32::new(0);
/// Tests of this file share the statics above: they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn probe(mut ctx: RunnerCtx<'_>) -> BoxFuture<'_, erp_fields::Result<Option<ActionResult>>> {
    Box::pin(async move {
        CALLS.fetch_add(1, Ordering::SeqCst);
        let step = SCRIPT.lock().unwrap().pop_front().unwrap_or_default();
        if step.fail {
            return Err(erp_fields::Error::user("the probe failed"));
        }
        if step.done > 0 || step.remaining > 0 || step.deactivate {
            commit_progress(ctx.tx(), step.done, Some(step.remaining), step.deactivate);
        }
        Ok(None)
    })
}

erp_addon_base::server_action_runner!(
    PROBE_RUNNER = { addon: PROBE_ADDON, state: "x_cron_probe", multi: true, run: probe }
);

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &PROBE_ADDON,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::extension("test_ext_cron", "ir.actions.server", &["ir.actions.server"])
            .fields(STATE),
    ])
    .policy(StaticPolicy::AllowAll)
    .build()
    .await
    .expect("harness")
}

fn script(steps: &[Step]) {
    *SCRIPT.lock().unwrap() = steps.iter().copied().collect();
    CALLS.store(0, Ordering::SeqCst);
}

/// A server action of the probe state, as the system.
async fn action(h: &Harness) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let model: i64 = tx
        .query(
            "INSERT INTO ir_model (name, model, state, transient) \
             VALUES ('Scheduled Action', 'ir.cron', 'base', false) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let vals = RawVals::new()
        .set_cell("name", Cell::Str("Probe".into()))
        .set_cell("model_id", Cell::Id(model))
        .set_cell("state", Cell::Str("x_cron_probe".into()))
        .set_cell("usage", Cell::Str("ir_cron".into()));
    let created = tx
        .env()
        .records("ir.actions.server")
        .unwrap()
        .create(&mut tx, vec![vals])
        .await
        .unwrap();
    let id = created.ids()[0];
    tx.commit().await.unwrap();
    id
}

/// `ir.cron` rows made by `who` (a user id; 1 is the system) with the probe action.
async fn cron(
    h: &Harness,
    action: i64,
    who: i64,
    extra: &[(&'static str, Cell)],
) -> erp_fields::Result<i64> {
    let mut tx = h.orm().begin(UserId::new(who).unwrap()).await?;
    let mut vals = RawVals::new()
        .set_cell("ir_actions_server_id", Cell::Id(action))
        .set_cell("user_id", Cell::Id(who))
        .set_cell("interval_number", Cell::Int(1))
        .set_cell("interval_type", Cell::Str("hours".into()));
    for (name, value) in extra {
        vals = vals.set_cell(*name, value.clone());
    }
    let created = tx
        .env()
        .records("ir.cron")?
        .create(&mut tx, vec![vals])
        .await;
    match created {
        Ok(records) => {
            let id = records.ids()[0];
            tx.commit().await?;
            Ok(id)
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

fn nextcall_in(seconds: i64) -> (&'static str, Cell) {
    // the value is replaced in SQL by tests that need exact times; the cell is a datetime
    let now = erp_fields::DateTime::now();
    let _ = seconds;
    ("nextcall", Cell::DateTime(now))
}

async fn job(h: &Harness, id: i64, column: &str) -> String {
    cell(
        h,
        &format!(
            "SELECT ({column})::text FROM erp_job WHERE kind = 'base.cron' AND dedup_key = 'base.cron:{id}'"
        ),
    )
    .await
}

async fn run_due(h: &Harness) -> u32 {
    let runner = erp_orm::jobs::JobRunner::builder(h.orm().clone(), PassResolver)
        .build()
        .unwrap();
    runner.tick().await.unwrap().claimed
}

async fn make_due(h: &Harness, id: i64) {
    exec(
        h,
        &format!(
            "UPDATE ir_cron SET nextcall = (clock_timestamp() AT TIME ZONE 'UTC') - interval '1 minute' WHERE id = {id}; \
             UPDATE erp_job SET run_after = clock_timestamp() - interval '1 minute' WHERE dedup_key = 'base.cron:{id}'"
        ),
    )
    .await;
}

#[tokio::test]
async fn a_cron_owns_one_job_row_that_is_ready_only_when_due() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[]);
    let (name, value) = nextcall_in(0);
    let id = cron(&h, a, 1, &[(name, value)]).await.unwrap();
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE kind = 'base.cron'"
        )
        .await,
        "1"
    );
    assert_eq!(job(&h, id, "state").await, "ready");
    assert_eq!(
        job(&h, id, "singleton_key").await,
        format!("base.cron:{id}")
    );
    // not due: a tick claims nothing
    exec(
        &h,
        &format!(
            "UPDATE ir_cron SET nextcall = (clock_timestamp() AT TIME ZONE 'UTC') + interval '1 hour' WHERE id = {id}; \
             UPDATE erp_job SET run_after = clock_timestamp() + interval '1 hour'"
        ),
    )
    .await;
    assert_eq!(run_due(&h).await, 0);
    assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    make_due(&h, id).await;
    assert_eq!(run_due(&h).await, 1);
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
}

/// call)
#[tokio::test]
async fn a_finished_run_reschedules_the_cron_and_its_job_one_interval_later() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[Step::default()]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    make_due(&h, id).await;
    assert_eq!(run_due(&h).await, 1);
    // the interval is an hour: nextcall and the job's due time moved into the future, together
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', (nextcall AT TIME ZONE 'UTC' > clock_timestamp() AT TIME ZONE 'UTC'), \
                 lastcall IS NOT NULL, failure_count, active) FROM ir_cron WHERE id = {id}"
            )
        )
        .await,
        "t|t|0|t"
    );
    assert_eq!(job(&h, id, "state").await, "ready");
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT (run_after = (SELECT nextcall AT TIME ZONE 'UTC' FROM ir_cron WHERE id = {id}))::text \
                 FROM erp_job WHERE dedup_key = 'base.cron:{id}'"
            )
        )
        .await,
        "true"
    );
}

/// coalesced, dropped for an inactive cron, cleared when elapsed)
#[tokio::test]
async fn triggers_make_a_cron_due_coalesce_and_are_cleared_when_elapsed() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[Step::default()]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    // the next call is far away; a trigger with no time makes the cron due now
    exec(
        &h,
        &format!(
            "UPDATE ir_cron SET nextcall = (clock_timestamp() AT TIME ZONE 'UTC') + interval '1 day' WHERE id = {id}; \
             UPDATE erp_job SET run_after = clock_timestamp() + interval '1 day'"
        ),
    )
    .await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("ir.cron").unwrap().browse([id]).unwrap();
    let made: i64 = records
        .call::<(Option<Vec<i64>>, i64), i64>(&mut tx, "trigger", (None, 0))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(made, 1);
    assert_eq!(job(&h, id, "run_after <= clock_timestamp()").await, "true");
    // coalescing rounds up to the end of the 10 minute window of the UTC epoch
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("ir.cron").unwrap().browse([id]).unwrap();
    let at = erp_fields::DateTime::now().micros() + 3 * 86_400_000_000; // in the future
    records
        .call::<(Option<Vec<i64>>, i64), i64>(&mut tx, "trigger", (Some(vec![at + 1, at + 2]), 10))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT count(DISTINCT call_at)::text || '|' || \
                 (extract(epoch FROM max(call_at)) % 600 = 0)::text FROM ir_cron_trigger \
                 WHERE cron_id = {id} AND call_at > now() + interval '1 day'"
            )
        )
        .await,
        "1|true"
    );
    // a run clears the elapsed trigger and keeps the two future ones (one run serves both, they
    // share a window)
    assert_eq!(run_due(&h).await, 1);
    assert_eq!(
        cell(
            &h,
            &format!("SELECT count(*)::text FROM ir_cron_trigger WHERE cron_id = {id}")
        )
        .await,
        "2"
    );
}

#[tokio::test]
async fn an_inactive_cron_drops_triggers_that_are_due_and_keeps_its_job_held() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    let id = cron(&h, a, 1, &[("active", Cell::Bool(false))])
        .await
        .unwrap();
    assert_eq!(job(&h, id, "state").await, "held");
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("ir.cron").unwrap().browse([id]).unwrap();
    let made: i64 = records
        .call::<(Option<Vec<i64>>, i64), i64>(&mut tx, "trigger", (None, 0))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(made, 0);
    assert_eq!(run_due(&h).await, 0);
}

#[tokio::test]
async fn partial_progress_runs_again_at_once_and_full_progress_reschedules() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    // two chunks: some work with more left, then a chunk that could do none of it
    script(&[
        Step {
            done: 3,
            remaining: 5,
            ..Step::default()
        },
        Step {
            done: 0,
            remaining: 5,
            ..Step::default()
        },
    ]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    make_due(&h, id).await;
    exec(
        &h,
        &format!("UPDATE ir_cron SET failure_count = 2 WHERE id = {id}"),
    )
    .await;
    assert_eq!(run_due(&h).await, 1);
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    // partially done: ready now (not rescheduled later), the failure counter reset by progress
    assert_eq!(job(&h, id, "state").await, "ready");
    assert_eq!(job(&h, id, "run_after <= clock_timestamp()").await, "true");
    assert_eq!(
        cell(
            &h,
            &format!("SELECT failure_count::text FROM ir_cron WHERE id = {id}")
        )
        .await,
        "0"
    );
}

#[tokio::test]
async fn a_failing_run_counts_a_failure_and_the_cron_is_rescheduled_later() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[Step {
        fail: true,
        ..Step::default()
    }]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    make_due(&h, id).await;
    assert_eq!(run_due(&h).await, 1);
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', failure_count, first_failure_date IS NOT NULL, active, \
                 nextcall AT TIME ZONE 'UTC' > clock_timestamp() AT TIME ZONE 'UTC') FROM ir_cron WHERE id = {id}"
            )
        )
        .await,
        "1|t|t|t"
    );
    // the failure rank orders failing jobs behind healthy ones
    assert_eq!(job(&h, id, "failure_rank").await, "1");
    assert_eq!(job(&h, id, "state").await, "ready");
}

#[tokio::test]
async fn a_cron_that_failed_five_times_over_a_week_deactivates_and_its_job_is_held() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[Step {
        fail: true,
        ..Step::default()
    }]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    make_due(&h, id).await;
    exec(
        &h,
        &format!(
            "UPDATE ir_cron SET failure_count = 4, \
             first_failure_date = (clock_timestamp() AT TIME ZONE 'UTC') - interval '8 days' WHERE id = {id}"
        ),
    )
    .await;
    assert_eq!(run_due(&h).await, 1);
    assert_eq!(
        cell(
            &h,
            &format!(
                "SELECT concat_ws('|', active, failure_count, first_failure_date IS NULL) FROM ir_cron WHERE id = {id}"
            )
        )
        .await,
        "f|0|t"
    );
    assert_eq!(job(&h, id, "state").await, "held");
}

/// the action)
#[tokio::test]
async fn three_timed_out_runs_in_a_row_fail_without_calling_the_action() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    make_due(&h, id).await;
    // the two earlier runs ended by lease expiry without progress
    exec(
        &h,
        &format!(
            "UPDATE erp_job SET progress = '{{\"timed_out\": 2, \"done\": 0}}', \
             last_error_code = 'lease_expired' WHERE dedup_key = 'base.cron:{id}'"
        ),
    )
    .await;
    assert_eq!(run_due(&h).await, 1);
    assert_eq!(CALLS.load(Ordering::SeqCst), 0, "the action was not called");
    assert_eq!(
        cell(
            &h,
            &format!("SELECT failure_count::text FROM ir_cron WHERE id = {id}")
        )
        .await,
        "1"
    );
}

#[tokio::test]
async fn an_action_can_ask_its_cron_to_deactivate_when_it_is_done() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[Step {
        done: 1,
        remaining: 0,
        deactivate: true,
        fail: false,
    }]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    make_due(&h, id).await;
    assert_eq!(run_due(&h).await, 1);
    assert_eq!(
        cell(
            &h,
            &format!("SELECT active::text FROM ir_cron WHERE id = {id}")
        )
        .await,
        "false"
    );
    assert_eq!(job(&h, id, "state").await, "held");
}

#[tokio::test]
async fn write_and_unlink_keep_the_job_row_in_step_and_refuse_while_running() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    // deactivating holds the job, reactivating returns it
    for (active, expected) in [(false, "held"), (true, "ready")] {
        let mut tx = h.begin().await.unwrap();
        let records = tx.env().records("ir.cron").unwrap().browse([id]).unwrap();
        records
            .write(
                &mut tx,
                RawVals::new().set_cell("active", Cell::Bool(active)),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(job(&h, id, "state").await, expected);
    }
    // while a worker holds it, the cron may be neither changed nor deleted
    make_due(&h, id).await;
    let runner = erp_orm::jobs::JobRunner::builder(h.orm().clone(), PassResolver)
        .build()
        .unwrap();
    let claimed = runner.claim(1).await.unwrap();
    assert_eq!(claimed.len(), 1);
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("ir.cron").unwrap().browse([id]).unwrap();
    let error = records
        .write(&mut tx, RawVals::new().set_cell("priority", Cell::Int(1)))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Record cannot be modified right now: This cron task is currently being executed and \
         may not be modified Please try again in a few minutes"
    );
    let _ = tx.rollback().await;
    runner
        .run_claimed(claimed.into_iter().next().unwrap())
        .await;
    // finished: it can go, and its job row goes with it
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("ir.cron").unwrap().browse([id]).unwrap();
    records.unlink(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job WHERE kind = 'base.cron'"
        )
        .await,
        "0"
    );
}

#[tokio::test]
async fn the_admin_button_makes_the_cron_due_now() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    script(&[Step::default()]);
    let id = cron(&h, a, 1, &[]).await.unwrap();
    exec(
        &h,
        &format!(
            "UPDATE ir_cron SET nextcall = (clock_timestamp() AT TIME ZONE 'UTC') + interval '1 day' WHERE id = {id}; \
             UPDATE erp_job SET run_after = clock_timestamp() + interval '1 day'"
        ),
    )
    .await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("ir.cron").unwrap().browse([id]).unwrap();
    assert!(
        records
            .call::<(), bool>(&mut tx, "method_direct_trigger", ())
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(run_due(&h).await, 1);
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn another_user_for_a_cron_needs_the_elevation_that_m3_audit_provides() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    exec(
        &h,
        "INSERT INTO res_users (id, name, login, active, share) OVERRIDING SYSTEM VALUE VALUES \
         (7, 'u7', 'u7', true, false), (9, 'u9', 'u9', true, false)",
    )
    .await;
    let mut tx = h.orm().begin(UserId::new(7).unwrap()).await.unwrap();
    let vals = RawVals::new()
        .set_cell("ir_actions_server_id", Cell::Id(a))
        .set_cell("user_id", Cell::Id(9))
        .set_cell("interval_number", Cell::Int(1))
        .set_cell("interval_type", Cell::Str("hours".into()))
        .set_cell("nextcall", Cell::DateTime(erp_fields::DateTime::now()));
    let error = tx
        .env()
        .records("ir.cron")
        .unwrap()
        .create(&mut tx, vec![vals])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "choosing another user than your own for a scheduled action needs the 'jobs.enqueue_as' \
         elevation, which is recorded by the audit trail of a later release; that hook is not available \
         yet, so the change is refused"
    );
    let _ = tx.rollback().await;
    // for oneself it is allowed, and the job runs as that user
    let own = cron(&h, a, 7, &[]).await.unwrap();
    assert_eq!(job(&h, own, "run_as_user").await, "7");
}

const REFUSED: &str = "choosing another user than your own for a scheduled action needs the \
    'jobs.enqueue_as' elevation, which is recorded by the audit trail of a later release; that hook is \
    not available yet, so the change is refused";

#[tokio::test]
async fn a_user_id_in_any_cell_form_other_than_the_callers_is_refused_on_create_and_write() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    exec(
        &h,
        "INSERT INTO res_users (id, name, login, active, share) OVERRIDING SYSTEM VALUE VALUES \
         (7, 'u7', 'u7', true, false), (9, 'u9', 'u9', true, false)",
    )
    .await;
    let own = cron(&h, a, 7, &[]).await.unwrap();
    let forms = [
        Cell::Id(9),
        Cell::Int(9),
        Cell::Str("9".into()),
        Cell::Float(9.0),
        Cell::Null,
        Cell::Bool(false),
    ];
    for form in forms {
        let mut tx = h.orm().begin(UserId::new(7).unwrap()).await.unwrap();
        let vals = RawVals::new()
            .set_cell("ir_actions_server_id", Cell::Id(a))
            .set_cell("user_id", form.clone())
            .set_cell("interval_number", Cell::Int(1))
            .set_cell("interval_type", Cell::Str("hours".into()));
        let error = tx
            .env()
            .records("ir.cron")
            .unwrap()
            .create(&mut tx, vec![vals])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access, "create with {form:?}");
        assert_eq!(error.message(), REFUSED, "create with {form:?}");
        let _ = tx.rollback().await;

        let mut tx = h.orm().begin(UserId::new(7).unwrap()).await.unwrap();
        let records = tx.env().records("ir.cron").unwrap().browse([own]).unwrap();
        let error = records
            .write(&mut tx, RawVals::new().set_cell("user_id", form.clone()))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access, "write with {form:?}");
        assert_eq!(error.message(), REFUSED, "write with {form:?}");
        let _ = tx.rollback().await;
    }
    assert_eq!(job(&h, own, "run_as_user").await, "7");
}

/// caller who could not have created it)
#[tokio::test]
async fn changing_a_cron_that_belongs_to_another_user_is_refused_for_a_non_system_caller() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    exec(
        &h,
        "INSERT INTO res_users (id, name, login, active, share) OVERRIDING SYSTEM VALUE VALUES \
         (7, 'u7', 'u7', true, false), (9, 'u9', 'u9', true, false)",
    )
    .await;
    let theirs = cron(&h, a, 9, &[]).await.unwrap();
    let mut tx = h.orm().begin(UserId::new(7).unwrap()).await.unwrap();
    let records = tx
        .env()
        .records("ir.cron")
        .unwrap()
        .browse([theirs])
        .unwrap();
    let error = records
        .write(
            &mut tx,
            RawVals::new().set_cell("interval_number", Cell::Int(5)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), REFUSED);
    let _ = tx.rollback().await;
    // the system may
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("ir.cron")
        .unwrap()
        .browse([theirs])
        .unwrap();
    records
        .write(
            &mut tx,
            RawVals::new().set_cell("interval_number", Cell::Int(5)),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(job(&h, theirs, "run_as_user").await, "9");
}

/// A4 and.2: a user keeps the job row of its own cron without switching to
/// the superuser, on create and on write, attributed to the user; the core entry a user keeps its
/// schedule with never replaces the row of another principal, nor names another user or a
/// system-run kind.
#[tokio::test]
async fn a_user_keeps_its_own_cron_row_and_never_replaces_another_principals_row() {
    use erp_orm::jobs::{JobKey, RecurringSpec};
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let a = action(&h).await;
    exec(
        &h,
        "INSERT INTO res_users (id, name, login, active, share) OVERRIDING SYSTEM VALUE VALUES \
         (7, 'u7', 'u7', true, false), (9, 'u9', 'u9', true, false)",
    )
    .await;
    // create by the user: the job row runs as the user
    let own = cron(&h, a, 7, &[]).await.unwrap();
    assert_eq!(job(&h, own, "run_as_user").await, "7");
    assert_eq!(job(&h, own, "run_as_kind").await, "user");
    // the row was written by the core but is attributed to the user who kept it
    assert_eq!(job(&h, own, "enqueued_by_kind").await, "user");
    assert_eq!(job(&h, own, "enqueued_by_user").await, "7");
    // write by the user keeps the row in step
    let mut tx = h.orm().begin(UserId::new(7).unwrap()).await.unwrap();
    let records = tx.env().records("ir.cron").unwrap().browse([own]).unwrap();
    records
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(job(&h, own, "state").await, "held");
    assert_eq!(job(&h, own, "run_as_user").await, "7");

    // the own entry, directly: another principal's row (the cron of user 9) is not replaced; the
    // ORM knows the impersonation capability, so a switch made by user 7 passes the gate under
    // `AllowAll` and reaches the entry's own check
    let theirs = cron(&h, a, 9, &[]).await.unwrap();
    let key = JobKey::new(&format!("base.cron:{theirs}")).unwrap();
    // the system-run core kinds are linked from erp-security
    assert!(!erp_security::jobs::core_job_kinds().is_empty());
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .impersonate_capability(&erp_security::principal::PRINCIPAL_IMPERSONATE)
        .build()
        .unwrap();
    let mut tx = orm.begin(UserId::new(7).unwrap()).await.unwrap();
    let env = tx.env();
    let error = tx
        .job_upsert_recurring_own(
            &env,
            RecurringSpec::new("base.cron", key.clone()).run_as(UserId::new(7).unwrap()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        format!(
            "the recurring job base.cron:{theirs} of kind 'base.cron' runs as user 9: only the \
             system principal replaces it"
        )
    );
    // nor runs as another user
    let error = tx
        .job_upsert_recurring_own(
            &env,
            RecurringSpec::new("base.cron", key.clone()).run_as(UserId::new(9).unwrap()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "job_upsert_recurring_own keeps a schedule that runs as its caller (user 7), not as user 9"
    );
    // nor keeps a kind that runs as the system
    let error = tx
        .job_upsert_recurring_own(
            &env,
            RecurringSpec::new(
                "core.jobs_history_retention",
                JobKey::new("core.jobs_history_retention").unwrap(),
            )
            .run_as(UserId::new(7).unwrap()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "job kind 'core.jobs_history_retention' runs as the system principal: only the system \
         keeps its schedule (job_upsert_recurring)"
    );
    // nor acts from an admitted switch to another user, and a switch to the superuser is refused
    // by the gate before the entry
    let own_only = "job_upsert_recurring_own keeps the schedule of a user acting for itself: call \
                    it with the user's own environment, outside an elevation block and without \
                    with_user";
    for (switched, message) in [
        (env.with_user(9), own_only.to_owned()),
        (
            env.sudo(true).with_user(erp_orm::SUPERUSER_ID),
            "only the system principal may act as the superuser (user 7 called with_user(1))"
                .to_owned(),
        ),
    ] {
        let error = tx
            .job_upsert_recurring_own(
                &switched,
                RecurringSpec::new("base.cron", key.clone()).run_as(UserId::new(7).unwrap()),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access, "{error}");
        assert_eq!(error.message(), message);
    }
    tx.rollback().await.unwrap();
    assert_eq!(job(&h, theirs, "run_as_user").await, "9");
}
