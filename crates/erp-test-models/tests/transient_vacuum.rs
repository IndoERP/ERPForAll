//! The transient vacuum engine on a real database (package B,,
//! explicitly, nothing sleeps. The unlink override of the `test_vacuum` fixtures logs every
//! deleted record in the deleting transaction, so the log shows exactly what the ORM unlink
//! chain removed and a rollback removes the log with it.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_fields::ErrorKind;
use erp_orm::{VacuumOptions, VacuumOutcome, VacuumReport, transient_policy_fingerprint};
use erp_test_models::{base_users, test_vacuum};
use erp_testkit::Harness;
use erp_testkit::deadpool_postgres::tokio_postgres::types::ToSql;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_vacuum::ADDON])
        .build()
        .await
        .expect("harness")
}

/// Run `sql` (committing).
async fn exec(h: &Harness, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(sql, params).await.unwrap();
    tx.commit().await.unwrap();
}

/// One text cell of a query.
async fn cell(h: &Harness, sql: &str) -> String {
    let mut tx = h.begin().await.unwrap();
    support::one(&mut tx, &format!("SELECT ({sql})::text"))
        .await
        .unwrap_or_default()
}

/// A row of `table` written `secs_ago` seconds before the database's now.
async fn row(h: &Harness, table: &str, name: &str, secs_ago: i64) {
    exec(
        h,
        &format!(
            "INSERT INTO {table} (name, create_date, write_date) VALUES ($1, \
             (now() AT TIME ZONE 'UTC') - $2::int8 * interval '1 second', \
             (now() AT TIME ZONE 'UTC') - $2::int8 * interval '1 second')"
        ),
        &[&name, &secs_ago],
    )
    .await;
}

/// `n` rows named `r<i>` written `secs_ago` seconds ago.
async fn rows(h: &Harness, table: &str, n: i64, secs_ago: i64) {
    exec(
        h,
        &format!(
            "INSERT INTO {table} (name, create_date, write_date) SELECT 'r' || i, \
             (now() AT TIME ZONE 'UTC') - $2::int8 * interval '1 second', \
             (now() AT TIME ZONE 'UTC') - $2::int8 * interval '1 second' \
             FROM generate_series(1, $1::int8) AS i"
        ),
        &[&n, &secs_ago],
    )
    .await;
}

async fn count(h: &Harness, table: &str) -> i64 {
    cell(h, &format!("SELECT count(*) FROM {table}"))
        .await
        .parse()
        .unwrap()
}

async fn remaining_names(h: &Harness, table: &str) -> String {
    cell(
        h,
        &format!("SELECT coalesce(string_agg(name, ',' ORDER BY id), '') FROM {table}"),
    )
    .await
}

async fn log_len(h: &Harness) -> i64 {
    count(h, "test_vac_log").await
}

async fn vacuum(h: &Harness, model: &str) -> VacuumReport {
    h.orm()
        .vacuum_transient(model, VacuumOptions::default())
        .await
        .unwrap()
}

/// The state row of `model` as `phase|latched|rows_total|batches_total|epoch|owner-is-null`.
async fn state(h: &Harness, model: &str) -> String {
    cell(
        h,
        &format!(
            "SELECT phase || '|' || count_latched || '|' || rows_total || '|' || batches_total \
             || '|' || epoch || '|' || (owner IS NULL) FROM erp_transient_vacuum_state \
             WHERE model = '{model}'"
        ),
    )
    .await
}

fn fingerprint(h: &Harness, model: &str) -> String {
    let policy = h.registry().model(model).unwrap().transient_policy.unwrap();
    transient_policy_fingerprint(model, &policy)
}

/// Seed a running cycle of `model` held by a lease that expired, with a fixed cutoff.
#[allow(clippy::too_many_arguments)]
async fn seed_running(
    h: &Harness,
    model: &str,
    phase: &str,
    latched: bool,
    cutoff: &str,
    rows_total: i64,
    batches_total: i64,
    epoch: i64,
) {
    let fp = fingerprint(h, model);
    exec(
        h,
        "INSERT INTO erp_transient_vacuum_state (model, epoch, owner, lease_expires_at, \
         next_due_at, phase, cycle_started_at, cycle_cutoff, count_latched, policy_fingerprint, \
         rows_total, batches_total) VALUES ($1, $2, gen_random_uuid(), \
         clock_timestamp() - interval '1 hour', clock_timestamp() + interval '1 hour', $3, \
         $4::text::timestamptz, $4::text::timestamptz, $5, $6, $7, $8)",
        &[
            &model,
            &epoch,
            &phase,
            &cutoff,
            &latched,
            &fp,
            &rows_total,
            &batches_total,
        ],
    )
    .await;
}

#[tokio::test]
async fn spec_model_036_default_and_count_scenarios() {
    let h = harness().await;
    // scenario 1: default limits (1 hour, no count limit); rows written 2 h, 30 min, 2 min ago
    row(&h, "test_vac_default", "two-hours", 7_200).await;
    row(&h, "test_vac_default", "half-hour", 1_800).await;
    row(&h, "test_vac_default", "two-minutes", 120).await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 1);
    assert_eq!(
        remaining_names(&h, "test_vac_default").await,
        "half-hour,two-minutes"
    );
    // the deletion went through the unlink override, under the system principal
    assert_eq!(log_len(&h).await, 1);
    // scenario 2: a count limit of 1; three rows written 10 min ago and one 2 min ago
    row(&h, "test_vac_counted", "old-1", 600).await;
    row(&h, "test_vac_counted", "old-2", 600).await;
    row(&h, "test_vac_counted", "old-3", 600).await;
    row(&h, "test_vac_counted", "recent", 120).await;
    let report = vacuum(&h, "test_vac.counted").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 3);
    assert_eq!(remaining_names(&h, "test_vac_counted").await, "recent");
    assert_eq!(log_len(&h).await, 4);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_empty_model_completes() {
    let h = harness().await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!((report.deleted_rows, report.batches), (0, 0));
    assert!(report.epoch.is_some());
    assert!(
        state(&h, "test_vac.default")
            .await
            .starts_with("idle|false|0|0|1|true")
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn age_boundary_is_exact() {
    let h = harness().await;
    let cutoff = "2030-01-01 00:00:00+00";
    for (name, offset) in [
        ("on-the-line", "0"),
        ("just-older", "-1 microsecond"),
        ("just-newer", "1 microsecond"),
    ] {
        exec(
            &h,
            "INSERT INTO test_vac_default (name, create_date, write_date) VALUES ($1, \
             timestamp '2029-12-31 23:00:00' + $2::text::interval, \
             timestamp '2029-12-31 23:00:00' + $2::text::interval)",
            &[&name, &offset],
        )
        .await;
    }
    seed_running(&h, "test_vac.default", "age", false, cutoff, 0, 0, 7).await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 1);
    assert_eq!(report.epoch, Some(8));
    assert_eq!(
        remaining_names(&h, "test_vac_default").await,
        "on-the-line,just-newer"
    );
    h.cleanup().await.unwrap();
}

/// exact)
#[tokio::test]
async fn recent_write_boundary_is_exact() {
    let h = harness().await;
    let cutoff = "2030-01-01 00:00:00+00";
    for (name, offset) in [
        ("on-the-line", "0"),
        ("just-older", "-1 microsecond"),
        ("just-newer", "1 microsecond"),
    ] {
        exec(
            &h,
            "INSERT INTO test_vac_latch (name, create_date, write_date) VALUES ($1, \
             timestamp '2029-12-31 23:55:00' + $2::text::interval, \
             timestamp '2029-12-31 23:55:00' + $2::text::interval)",
            &[&name, &offset],
        )
        .await;
    }
    seed_running(&h, "test_vac.latch", "count", true, cutoff, 0, 0, 0).await;
    let report = vacuum(&h, "test_vac.latch").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 1);
    assert_eq!(
        remaining_names(&h, "test_vac_latch").await,
        "on-the-line,just-newer"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn recent_writes_are_protected_whatever_the_count() {
    let h = harness().await;
    row(&h, "test_vac_counted", "a", 120).await;
    row(&h, "test_vac_counted", "b", 120).await;
    row(&h, "test_vac_counted", "c", 120).await;
    let report = vacuum(&h, "test_vac.counted").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 0);
    assert_eq!(count(&h, "test_vac_counted").await, 3);
    assert_eq!(log_len(&h).await, 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn many_rows_take_several_batches() {
    let h = harness().await;
    rows(&h, "test_vac_default", 2_500, 7_200).await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 2_500);
    assert_eq!(report.batches, 3);
    assert_eq!(count(&h, "test_vac_default").await, 0);
    assert_eq!(log_len(&h).await, 2_500);
    assert!(
        state(&h, "test_vac.default")
            .await
            .starts_with("idle|false|2500|3|")
    );
    h.cleanup().await.unwrap();
}

/// the error)
#[tokio::test]
async fn a_hook_failure_rolls_the_batch_back() {
    let h = harness().await;
    for (name, secs) in [("a", 7_200), ("b", 7_200), ("boom", 7_200), ("c", 7_200)] {
        row(&h, "test_vac_default", name, secs).await;
    }
    let options = VacuumOptions::default().max_batch_rows(2).unwrap();
    let err = h
        .orm()
        .vacuum_transient("test_vac.default", options)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert!(
        err.message().contains("the unlink hook refused the record"),
        "{err}"
    );
    // the first batch (a, b) committed; the second (boom, c) rolled back with its log rows
    assert_eq!(remaining_names(&h, "test_vac_default").await, "boom,c");
    assert_eq!(log_len(&h).await, 2);
    // progress covers the committed batch only, and the lease was handed back
    assert_eq!(state(&h, "test_vac.default").await, "age|false|2|1|1|true");
    h.cleanup().await.unwrap();
}

/// drains old rows even after the count fell to the limit)
#[tokio::test]
async fn the_count_trigger_stays_latched() {
    let h = harness().await;
    rows(&h, "test_vac_latch", 5, 600).await;
    let one_batch = VacuumOptions::default()
        .max_batch_rows(2)
        .unwrap()
        .max_batches(1)
        .unwrap();
    let report = h
        .orm()
        .vacuum_transient("test_vac.latch", one_batch)
        .await
        .unwrap();
    assert_eq!(report.outcome, VacuumOutcome::Pending);
    assert_eq!(report.deleted_rows, 2);
    // 3 rows are left: not over the limit of 3 any more, but the trigger stays latched
    assert_eq!(count(&h, "test_vac_latch").await, 3);
    assert_eq!(state(&h, "test_vac.latch").await, "count|true|2|1|1|true");
    let report = vacuum(&h, "test_vac.latch").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 3);
    assert_eq!(count(&h, "test_vac_latch").await, 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_count_within_the_limit_deletes_nothing() {
    let h = harness().await;
    rows(&h, "test_vac_latch", 3, 600).await;
    let report = vacuum(&h, "test_vac.latch").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 0);
    assert_eq!(count(&h, "test_vac_latch").await, 3);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn invocation_caps_leave_a_continuation() {
    let h = harness().await;
    rows(&h, "test_vac_default", 10, 7_200).await;
    let options = VacuumOptions::default()
        .max_batch_rows(2)
        .unwrap()
        .max_batches(2)
        .unwrap();
    let report = h
        .orm()
        .vacuum_transient("test_vac.default", options)
        .await
        .unwrap();
    assert_eq!(report.outcome, VacuumOutcome::Pending);
    assert_eq!((report.deleted_rows, report.batches), (4, 2));
    assert_eq!(count(&h, "test_vac_default").await, 6);
    // a row cap of 3 limits the last batch to the remaining row
    let options = VacuumOptions::default()
        .max_batch_rows(2)
        .unwrap()
        .max_rows(3)
        .unwrap();
    let report = h
        .orm()
        .vacuum_transient("test_vac.default", options)
        .await
        .unwrap();
    assert_eq!(report.outcome, VacuumOutcome::Pending);
    assert_eq!(report.deleted_rows, 3);
    // the continuation is not "not due": the open cycle is inherited
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 3);
    assert_eq!(count(&h, "test_vac_default").await, 0);
    assert!(
        state(&h, "test_vac.default")
            .await
            .starts_with("idle|false|10|")
    );
    h.cleanup().await.unwrap();
}

/// exhausted cycle is pushed out a full cadence)
#[tokio::test]
async fn the_cycle_row_budget_survives_a_takeover() {
    let h = harness().await;
    rows(&h, "test_vac_default", 1_200, 7_200).await;
    seed_running(
        &h,
        "test_vac.default",
        "age",
        false,
        "2030-01-01 00:00:00+00",
        99_500,
        1,
        4,
    )
    .await;
    // the seeded cycle's own due time is past, so only the exhaustion push-out can delay the next
    exec(
        &h,
        "UPDATE erp_transient_vacuum_state SET next_due_at = clock_timestamp() - interval '1 hour'",
        &[],
    )
    .await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::BudgetExhausted);
    assert_eq!(report.deleted_rows, 500);
    assert_eq!(report.cycle_rows, 100_000);
    assert_eq!(report.epoch, Some(5));
    assert_eq!(count(&h, "test_vac_default").await, 700);
    assert!(
        state(&h, "test_vac.default")
            .await
            .starts_with("idle|false|100000|2|5|true")
    );
    // the push-out itself: the next cycle is at least a cadence (300 s) from now
    assert_eq!(
        cell(
            &h,
            "SELECT next_due_at >= clock_timestamp() + interval '299 seconds' \
             FROM erp_transient_vacuum_state"
        )
        .await,
        "true"
    );
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::NotDue);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_cycle_batch_budget_is_a_hard_cap() {
    let h = harness().await;
    rows(&h, "test_vac_default", 30, 7_200).await;
    seed_running(
        &h,
        "test_vac.default",
        "age",
        false,
        "2030-01-01 00:00:00+00",
        0,
        99,
        0,
    )
    .await;
    let options = VacuumOptions::default().max_batch_rows(10).unwrap();
    let report = h
        .orm()
        .vacuum_transient("test_vac.default", options)
        .await
        .unwrap();
    assert_eq!(report.outcome, VacuumOutcome::BudgetExhausted);
    assert_eq!(report.deleted_rows, 10);
    assert_eq!(count(&h, "test_vac_default").await, 20);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn options_are_validated() {
    for bad in [
        VacuumOptions::default().max_batch_rows(0),
        VacuumOptions::default().max_batch_rows(1_001),
        VacuumOptions::default().max_batches(0),
        VacuumOptions::default().max_batches(101),
        VacuumOptions::default().max_rows(0),
        VacuumOptions::default().max_rows(100_001),
    ] {
        assert_eq!(bad.unwrap_err().kind(), ErrorKind::Programming);
    }
    assert!(VacuumOptions::default().max_batch_rows(1_000).is_ok());
}

#[tokio::test]
async fn a_live_lease_is_busy() {
    let h = harness().await;
    row(&h, "test_vac_default", "old", 7_200).await;
    seed_running(
        &h,
        "test_vac.default",
        "age",
        false,
        "2030-01-01 00:00:00+00",
        0,
        0,
        3,
    )
    .await;
    exec(
        &h,
        "UPDATE erp_transient_vacuum_state SET lease_expires_at = clock_timestamp() + interval '1 hour'",
        &[],
    )
    .await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::Busy);
    assert_eq!((report.deleted_rows, report.epoch), (0, None));
    assert_eq!(count(&h, "test_vac_default").await, 1);
    assert!(state(&h, "test_vac.default").await.ends_with("|3|false"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_locked_state_row_is_busy() {
    let h = harness().await;
    row(&h, "test_vac_default", "old", 7_200).await;
    exec(
        &h,
        "INSERT INTO erp_transient_vacuum_state (model, next_due_at) VALUES ('test_vac.default', clock_timestamp())",
        &[],
    )
    .await;
    let mut holder = h.begin().await.unwrap();
    holder
        .query(
            "SELECT 1 FROM erp_transient_vacuum_state WHERE model = 'test_vac.default' FOR UPDATE",
            &[],
        )
        .await
        .unwrap();
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::Busy);
    holder.rollback().await.unwrap();
    assert_eq!(count(&h, "test_vac_default").await, 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_cycle_not_due_does_nothing() {
    let h = harness().await;
    let first = vacuum(&h, "test_vac.default").await;
    assert_eq!(first.outcome, VacuumOutcome::Completed);
    row(&h, "test_vac_default", "old", 7_200).await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::NotDue);
    assert_eq!(count(&h, "test_vac_default").await, 1);
    // due again once the database says so
    exec(
        &h,
        "UPDATE erp_transient_vacuum_state SET next_due_at = clock_timestamp()",
        &[],
    )
    .await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::Completed);
    assert_eq!(report.deleted_rows, 1);
    assert_eq!(report.epoch, Some(2));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_lost_lease_commits_nothing() {
    let h = harness().await;
    row(&h, "test_vac_default", "steal", 7_200).await;
    row(&h, "test_vac_default", "other", 7_200).await;
    let report = vacuum(&h, "test_vac.default").await;
    assert_eq!(report.outcome, VacuumOutcome::LostLease);
    assert_eq!((report.deleted_rows, report.batches), (0, 0));
    assert_eq!(count(&h, "test_vac_default").await, 2);
    assert_eq!(log_len(&h).await, 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_changed_policy_aborts_the_old_cycle() {
    let h = harness().await;
    row(&h, "test_vac_latch", "old", 600).await;
    seed_running(
        &h,
        "test_vac.latch",
        "count",
        true,
        "2030-01-01 00:00:00+00",
        5,
        1,
        2,
    )
    .await;
    exec(
        &h,
        "UPDATE erp_transient_vacuum_state SET policy_fingerprint = 'an older policy'",
        &[],
    )
    .await;
    // the abandoned cycle is dropped; the new one is not due yet, so nothing is deleted
    let report = vacuum(&h, "test_vac.latch").await;
    assert_eq!(report.outcome, VacuumOutcome::NotDue);
    assert!(report.aborted_previous_cycle);
    assert_eq!(count(&h, "test_vac_latch").await, 1);
    assert!(
        state(&h, "test_vac.latch")
            .await
            .starts_with("idle|false|0|0|3|true")
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_policy_change_between_batches_stops_the_cycle() {
    let h = harness().await;
    row(&h, "test_vac_default", "drift", 7_200).await;
    row(&h, "test_vac_default", "later", 7_200).await;
    let options = VacuumOptions::default().max_batch_rows(1).unwrap();
    let report = h
        .orm()
        .vacuum_transient("test_vac.default", options)
        .await
        .unwrap();
    assert_eq!(report.outcome, VacuumOutcome::PolicyChanged);
    assert_eq!(report.deleted_rows, 1);
    assert_eq!(remaining_names(&h, "test_vac_default").await, "later");
    assert!(state(&h, "test_vac.default").await.starts_with("idle|"));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn disabled_policy_is_reported() {
    let h = harness().await;
    row(&h, "test_vac_off", "old", 99_999).await;
    let report = vacuum(&h, "test_vac.off").await;
    assert_eq!(report.outcome, VacuumOutcome::Disabled);
    assert_eq!(count(&h, "test_vac_off").await, 1);
    assert_eq!(
        cell(&h, "SELECT count(*) FROM erp_transient_vacuum_state").await,
        "0"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn only_transient_models_are_admitted() {
    let h = harness().await;
    for model in ["test_vac.plain", "test_vac.log", "no.such.model"] {
        let err = h
            .orm()
            .vacuum_transient(model, VacuumOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{model}: {err}");
        assert!(err.message().contains(model), "{err}");
    }
    h.cleanup().await.unwrap();
}

/// The explicit ACL of `table` for `role`: `PRIV,PRIV` sorted, empty when none.
async fn table_privileges(h: &Harness, table: &str, role: &str) -> String {
    let client = h.db().client().await.unwrap();
    let rows = client
        .query(
            "SELECT string_agg(a.privilege_type, ',' ORDER BY a.privilege_type) FROM pg_class c \
             CROSS JOIN LATERAL aclexplode(c.relacl) a WHERE c.relname = $1 AND c.relkind = 'r' \
             AND a.grantee = (SELECT oid FROM pg_roles WHERE rolname = $2)",
            &[&table, &role],
        )
        .await
        .unwrap();
    rows.first()
        .and_then(|r| r.get::<_, Option<String>>(0))
        .unwrap_or_default()
}

/// package D (privilege admission): the maintenance role is granted exactly SELECT,
/// INSERT, UPDATE and DELETE on the state table, twice gives the same result; the request role
/// holding the blanket grant of the schema loses everything on it; the owner is refused.
#[tokio::test]
async fn the_state_table_belongs_to_the_maintenance_role_not_the_request_role() {
    let h = harness().await;
    let suffix: String = h.db().name().chars().take(40).collect();
    let (maintenance, request) = (format!("vm_{suffix}"), format!("vr_{suffix}"));
    for role in [&maintenance, &request] {
        exec(&h, &format!("CREATE ROLE \"{role}\" NOLOGIN"), &[]).await;
    }
    // the blanket grant a runtime publication makes
    exec(
        &h,
        &format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO \"{request}\""
        ),
        &[],
    )
    .await;
    assert_eq!(
        table_privileges(&h, "erp_transient_vacuum_state", &request).await,
        "DELETE,INSERT,SELECT,UPDATE"
    );
    for _ in 0..2 {
        let mut tx = h.begin().await.unwrap();
        tx.grant_vacuum_role(&maintenance).await.unwrap();
        tx.revoke_vacuum_request_role(&request).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            table_privileges(&h, "erp_transient_vacuum_state", &maintenance).await,
            "DELETE,INSERT,SELECT,UPDATE"
        );
        assert_eq!(
            table_privileges(&h, "erp_transient_vacuum_state", &request).await,
            ""
        );
    }
    // the owner cannot be the grantee: revoking from it would remove the schema sync's rights
    let owner = cell(&h, "SELECT current_user").await;
    let mut tx = h.begin().await.unwrap();
    let err = tx.grant_vacuum_role(&owner).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains("owns erp_transient_vacuum_state"),
        "{}",
        err.message()
    );
    tx.rollback().await.unwrap();
    for role in [&maintenance, &request] {
        exec(
            &h,
            &format!("REVOKE ALL ON ALL TABLES IN SCHEMA public FROM \"{role}\""),
            &[],
        )
        .await;
        exec(&h, &format!("DROP ROLE \"{role}\""), &[]).await;
    }
    h.cleanup().await.unwrap();
}

/// package D: the state table is created closed to PUBLIC, also in a database whose
/// default privileges would grant every new table to PUBLIC (the pattern of the post-commit queue
/// test in `erp-modules/tests/security_review.rs`).
#[tokio::test]
async fn the_state_table_is_created_closed_to_public() {
    let h = harness().await;
    let public = |table: &'static str| {
        let h = &h;
        async move {
            cell(
                h,
                &format!(
                    "SELECT count(*) FROM pg_class c, aclexplode(c.relacl) a \
                     WHERE c.oid = '{table}'::regclass AND a.grantee = 0"
                ),
            )
            .await
        }
    };
    exec(&h, "DROP TABLE erp_transient_vacuum_state", &[]).await;
    exec(
        &h,
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON TABLES TO PUBLIC",
        &[],
    )
    .await;
    exec(&h, "CREATE TABLE vac_public_control (id int8)", &[]).await;
    assert_ne!(
        public("vac_public_control").await,
        "0",
        "control: the default privileges reach a new table"
    );
    let _report = h.orm().sync_schema().await.unwrap();
    assert_eq!(public("erp_transient_vacuum_state").await, "0");
    h.cleanup().await.unwrap();
}
