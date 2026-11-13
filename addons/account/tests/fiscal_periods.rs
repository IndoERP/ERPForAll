// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fiscal periods on real PostgreSQL: `ensure_periods` (with an explicit
//! calendar; the settings path is checked by `crates/erp-cli/tests/account_install.rs`, which loads
//! the groups the settings name), the period-row status machine, the write admission and the
//! database constraints. Functional tests, not performance tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::time::{Duration, Instant};

use erp_addon_account::fiscal::calendar::{FiscalYearEnd, add_months};
use erp_addon_account::fiscal::settings::{FiscalCalendar, HORIZON_SETTING, LAST_DAY_SETTING};
use erp_addon_account::fiscal::status::{
    LockOutcome, LockRun, MANAGER_GROUP, PeriodSet, hard_lock_begin, hard_lock_complete,
    hard_lock_reset, stock_lock_begin, stock_lock_complete, stock_lock_reset, sync_lock_dates,
};
use erp_addon_account::fiscal::{
    PERIOD_LOCK_CLASS, PeriodVerdict, WriteClass, ensure_periods_with, period_admission,
    require_open_periods,
};
use erp_sdk::prelude::*;
use erp_sdk::{BeginOptions, Context, ErrorKind, SUPERUSER_ID, UserId};
use erp_testkit::{FieldPolicy, Harness, HarnessBuilder};

fn d(y: i32, m: u32, day: u32) -> Date {
    Date::from_ymd(y, m, day).unwrap()
}

/// The caller of `ensure_periods_with` in the tests that do not test its company scope.
fn system() -> BeginOptions {
    BeginOptions::new(SUPERUSER_ID)
}

fn calendar(day: i64, month: i64) -> FiscalCalendar {
    FiscalCalendar::new(FiscalYearEnd::new(day, month).unwrap(), 12).unwrap()
}

/// The pool holds four connections and a finished transaction keeps its own until dropped, so the
/// tests drop each one after its commit or rollback.
async fn harness() -> Harness {
    builder().build().await.unwrap()
}

fn builder() -> HarnessBuilder {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
}

/// A committed company row, a root unless `parent` is given.
async fn company(h: &Harness, name: &str, parent: Option<i64>) -> i64 {
    let client = h.db().client().await.unwrap();
    let existing = client
        .query("SELECT id::int8 FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap();
    let currency: i64 = match existing.first() {
        Some(row) => row.get(0),
        None => erp_testkit::fixtures::seed_currency(&client).await.unwrap(),
    };
    client
        .query_one(
            "WITH n AS (SELECT nextval(pg_get_serial_sequence('res_company', 'id')) AS id) \
             INSERT INTO res_company (id, name, active, currency_id, parent_id, parent_path) \
             SELECT id, $1, true, $2, $3::int8, \
                    COALESCE((SELECT parent_path FROM res_company WHERE id = $3::int8), '') || id::text || '/' \
             FROM n RETURNING id::int8",
            &[&name, &currency, &parent],
        )
        .await
        .unwrap()
        .get(0)
}

async fn set_dates(
    h: &Harness,
    company: i64,
    fiscal: Option<Date>,
    hard: Option<Date>,
    stock: Option<Date>,
) {
    h.db()
        .client()
        .await
        .unwrap()
        .execute(
            "UPDATE res_company SET fiscalyear_lock_date = $2, hard_lock_date = $3, \
             stock_lock_date = $4 WHERE id = $1",
            &[&company, &fiscal, &hard, &stock],
        )
        .await
        .unwrap();
}

/// `(name, date_from, date_to, status, stock_lock_status)` of a company's periods, by date.
async fn periods(h: &Harness, company: i64) -> Vec<(String, Date, Date, String, String)> {
    h.db()
        .client()
        .await
        .unwrap()
        .query(
            "SELECT name, date_from, date_to, status, stock_lock_status FROM account_fiscal_period \
             WHERE company_id = $1 ORDER BY date_from",
            &[&company],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        .collect()
}

fn statuses(rows: &[(String, Date, Date, String, String)]) -> Vec<(&str, &str, &str)> {
    rows.iter()
        .map(|(name, _, _, s, k)| (name.as_str(), s.as_str(), k.as_str()))
        .collect()
}

async fn period_id(h: &Harness, company: i64, date: Date) -> i64 {
    h.db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT id::int8 FROM account_fiscal_period WHERE company_id = $1 \
             AND kind = 'regular' AND $2 BETWEEN date_from AND date_to",
            &[&company, &date],
        )
        .await
        .unwrap()
        .get(0)
}

fn assert_refused(error: &Error, kind: ErrorKind, needle: &str) {
    assert_eq!(error.kind(), kind, "{error:?}");
    assert!(
        error.message().contains(needle),
        "expected {needle:?} in {:?}",
        error.message()
    );
}

#[tokio::test]
async fn ensure_creates_the_missing_months_of_the_whole_tree_once() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    let branch = company(&h, "Branch", Some(root)).await;
    let cal = calendar(31, 12);

    let created = ensure_periods_with(
        h.orm(),
        system(),
        &cal,
        branch,
        d(2024, 1, 15),
        d(2024, 3, 10),
    )
    .await
    .unwrap();
    assert_eq!(created, 6, "three months for the root and for the branch");
    for c in [root, branch] {
        let rows = periods(&h, c).await;
        let ranges: Vec<(&str, Date, Date)> =
            rows.iter().map(|r| (r.0.as_str(), r.1, r.2)).collect();
        assert_eq!(
            ranges,
            [
                ("2024/01", d(2024, 1, 1), d(2024, 1, 31)),
                ("2024/02", d(2024, 2, 1), d(2024, 2, 29)),
                ("2024/03", d(2024, 3, 1), d(2024, 3, 31)),
            ]
        );
    }
    // idempotent, from either company of the tree
    for c in [root, branch] {
        let again = ensure_periods_with(h.orm(), system(), &cal, c, d(2024, 2, 1), d(2024, 3, 31))
            .await
            .unwrap();
        assert_eq!(again, 0);
    }
    // a fiscal year ending in March: the periods straddle two calendar years
    let other = company(&h, "March Co", None).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 3),
        other,
        d(2024, 3, 15),
        d(2024, 4, 2),
    )
    .await
    .unwrap();
    let names: Vec<String> = periods(&h, other).await.into_iter().map(|r| r.0).collect();
    assert_eq!(names, ["2023-24/12", "2024-25/01"]);

    // beyond the horizon: refused naming the setting
    let late = add_months(Date::today(), 13).unwrap();
    let error = ensure_periods_with(h.orm(), system(), &cal, root, late, late)
        .await
        .unwrap_err();
    assert_refused(&error, ErrorKind::Validation, HORIZON_SETTING);
    // a fiscal year end that no longer matches the periods: refused, nothing created
    let error = ensure_periods_with(
        h.orm(),
        system(),
        &calendar(15, 1),
        root,
        d(2024, 1, 20),
        d(2024, 1, 20),
    )
    .await
    .unwrap_err();
    assert_refused(&error, ErrorKind::Validation, LAST_DAY_SETTING);
    assert!(error.message().contains("2024/01"), "{}", error.message());
    assert_eq!(periods(&h, root).await.len(), 3);
}

/// Wait until some session of the test database waits for a lock.
async fn until_a_session_waits(h: &Harness) {
    let client = h.db().client().await.unwrap();
    let started = Instant::now();
    loop {
        let waiting: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no session waited for a lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn two_concurrent_ensures_for_the_same_new_period_both_succeed() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    let cal = calendar(31, 12);
    // an in-flight period creation holds the root company row
    let mut holder = h.begin().await.unwrap();
    holder
        .execute(
            "UPDATE res_company SET fiscal_period_serial = 41 WHERE id = $1",
            &[&root],
        )
        .await
        .unwrap();
    let first = ensure_periods_with(h.orm(), system(), &cal, root, d(2025, 5, 2), d(2025, 5, 3));
    let second = ensure_periods_with(
        h.orm(),
        system(),
        &cal,
        root,
        d(2025, 5, 20),
        d(2025, 5, 20),
    );
    let release = async {
        until_a_session_waits(&h).await;
        holder.commit().await.unwrap();
    };
    let (first, second, ()) = tokio::join!(first, second, release);
    assert_eq!(
        first.unwrap() + second.unwrap(),
        1,
        "the period exists once"
    );
    assert_eq!(periods(&h, root).await.len(), 1);

    // a lock-date synchronisation whose snapshot predates a committed creation is retried, never
    // left with a period derived from the old lock date
    let mut setter = h.begin().await.unwrap();
    setter
        .query("SELECT 1 FROM res_company LIMIT 1", &[])
        .await
        .unwrap();
    ensure_periods_with(h.orm(), system(), &cal, root, d(2025, 6, 1), d(2025, 6, 1))
        .await
        .unwrap();
    let error = sync_lock_dates(&mut setter, root).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency, "{error:?}");
    setter.rollback().await.unwrap();
    drop(setter);
}

#[tokio::test]
async fn created_periods_take_their_statuses_from_the_lock_dates() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    let branch = company(&h, "Branch", Some(root)).await;
    set_dates(
        &h,
        root,
        Some(d(2024, 2, 29)),
        Some(d(2024, 1, 31)),
        Some(d(2024, 3, 31)),
    )
    .await;
    // the branch's own fiscal-year lock is later than its parent's
    set_dates(&h, branch, Some(d(2024, 3, 31)), None, None).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 4, 30),
    )
    .await
    .unwrap();
    assert_eq!(
        statuses(&periods(&h, root).await),
        [
            ("2024/01", "hard_locked", "stock_locked"),
            ("2024/02", "closed", "stock_locked"),
            ("2024/03", "open", "stock_locked"),
            ("2024/04", "open", "open"),
        ]
    );
    assert_eq!(
        statuses(&periods(&h, branch).await),
        [
            ("2024/01", "hard_locked", "stock_locked"),
            ("2024/02", "closed", "stock_locked"),
            ("2024/03", "closed", "stock_locked"),
            ("2024/04", "open", "open"),
        ]
    );
}

#[tokio::test]
async fn lock_dates_close_and_reopen_periods_with_the_refusals_of_the_date_model() {
    // uid 2 is no accounting manager; the policy knows the group so `has_group` answers
    let h = builder()
        .policy(FieldPolicy::new().member(3, [MANAGER_GROUP]))
        .build()
        .await
        .unwrap();
    let root = company(&h, "Root", None).await;
    let branch = company(&h, "Branch", Some(root)).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 6, 30),
    )
    .await
    .unwrap();

    // raising the fiscal-year lock closes, lowering reopens (one direction: later periods)
    set_dates(&h, root, Some(d(2024, 3, 31)), None, None).await;
    let mut tx = h.begin().await.unwrap();
    let synced = sync_lock_dates(&mut tx, root).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        synced.closed, 6,
        "three months of the root and of the branch"
    );
    set_dates(&h, root, Some(d(2024, 1, 31)), None, None).await;
    let mut tx = h.begin().await.unwrap();
    let synced = sync_lock_dates(&mut tx, root).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!((synced.closed, synced.reopened), (0, 4));
    assert_eq!(
        statuses(&periods(&h, branch).await)[..3],
        [
            ("2024/01", "closed", "open"),
            ("2024/02", "open", "open"),
            ("2024/03", "open", "open"),
        ]
    );

    // stock-lock January, then the fiscal-year lock cannot reopen it: lower the stock lock first
    let jan = period_id(&h, root, d(2024, 1, 10)).await;
    let mut tx = h.begin().await.unwrap();
    let set = PeriodSet::of(&mut tx, jan).await.unwrap();
    assert_eq!(
        set.ids().len(),
        2,
        "the branch's January moves with the root's"
    );
    let run = stock_lock_begin(&mut tx, &set).await.unwrap();
    assert_eq!(
        stock_lock_complete(&mut tx, &run).await.unwrap(),
        LockOutcome::Applied
    );
    tx.commit().await.unwrap();
    drop(tx);
    set_dates(&h, root, None, None, Some(d(2024, 1, 31))).await;
    let mut tx = h.begin().await.unwrap();
    let error = sync_lock_dates(&mut tx, root).await.unwrap_err();
    assert_refused(
        &error,
        ErrorKind::Validation,
        "lower the stock lock date first",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // the stock lock date lowered first reopens the stock status
    set_dates(&h, root, None, None, None).await;
    let mut tx = h.begin().await.unwrap();
    let synced = sync_lock_dates(&mut tx, root).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!((synced.reopened, synced.stock_reopened), (2, 2));

    // hard-lock January (closed first), then nothing lowers below it
    set_dates(&h, root, Some(d(2024, 1, 31)), None, None).await;
    let mut tx = h.begin().await.unwrap();
    sync_lock_dates(&mut tx, root).await.unwrap();
    let run = hard_lock_begin(&mut tx, &set).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        hard_lock_complete(&mut tx, &run).await.unwrap(),
        LockOutcome::Applied
    );
    tx.commit().await.unwrap();
    drop(tx);
    let hard: Option<Date> = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT hard_lock_date FROM res_company WHERE id = $1",
            &[&root],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(hard, Some(d(2024, 1, 31)), "T2 wrote the company date");
    assert_eq!(
        statuses(&periods(&h, branch).await)[0],
        ("2024/01", "hard_locked", "stock_locked"),
        "the hard lock implies the stock lock"
    );
    set_dates(&h, root, Some(d(2023, 12, 31)), Some(d(2024, 1, 31)), None).await;
    let mut tx = h.begin().await.unwrap();
    let error = sync_lock_dates(&mut tx, root).await.unwrap_err();
    assert_refused(&error, ErrorKind::Validation, "hard-locked period 2024/01");
    tx.rollback().await.unwrap();
    drop(tx);
    // a hard lock date written past the hard-locked periods is refused: only T2 moves it
    set_dates(&h, root, Some(d(2024, 2, 29)), Some(d(2024, 2, 29)), None).await;
    let mut tx = h.begin().await.unwrap();
    let error = sync_lock_dates(&mut tx, root).await.unwrap_err();
    assert_refused(
        &error,
        ErrorKind::Validation,
        "set by the hard lock operation",
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // a user without the accounting manager group changes nothing
    let mut tx = h.begin_as(2).await.unwrap();
    let error = sync_lock_dates(&mut tx, root).await.unwrap_err();
    assert_refused(&error, ErrorKind::Access, "Only accounting administrators");
}

#[tokio::test]
async fn lock_runs_are_two_step_idempotent_and_superseded_by_generation() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 2, 29),
    )
    .await
    .unwrap();
    let jan = period_id(&h, root, d(2024, 1, 1)).await;
    let feb = period_id(&h, root, d(2024, 2, 1)).await;
    let mut tx = h.begin().await.unwrap();
    let jan_set = PeriodSet::of(&mut tx, jan).await.unwrap();
    let feb_set = PeriodSet::of(&mut tx, feb).await.unwrap();

    // in order only
    let error = stock_lock_begin(&mut tx, &feb_set).await.unwrap_err();
    assert_refused(&error, ErrorKind::Validation, "2024/01 is not stock-locked");
    // the hard lock needs a closed period
    let error = hard_lock_begin(&mut tx, &jan_set).await.unwrap_err();
    assert_refused(&error, ErrorKind::Validation, "2024/01 is open");

    // T1 twice keeps the first run; a cancel with a stale generation does nothing
    let first = stock_lock_begin(&mut tx, &jan_set).await.unwrap();
    let started: (Option<i64>, String) = {
        let r = tx
            .query(
                "SELECT stock_lock_started_by::int8, stock_lock_started_at::text \
                 FROM account_fiscal_period WHERE id = $1",
                &[&jan],
            )
            .await
            .unwrap();
        (r[0].get(0), r[0].get(1))
    };
    let again = stock_lock_begin(&mut tx, &jan_set).await.unwrap();
    assert_eq!(first.generations(), again.generations());
    let stale = LockRun::resume(jan_set.clone(), vec![first.generations()[0] - 1]).unwrap();
    let LockOutcome::NotApplied(states) = stock_lock_reset(&mut tx, &stale).await.unwrap() else {
        panic!("a stale generation must not apply");
    };
    assert_eq!(states[0].stock_lock_status, "stock_locking");
    let kept: (Option<i64>, String) = {
        let r = tx
            .query(
                "SELECT stock_lock_started_by::int8, stock_lock_started_at::text \
                 FROM account_fiscal_period WHERE id = $1",
                &[&jan],
            )
            .await
            .unwrap();
        (r[0].get(0), r[0].get(1))
    };
    assert_eq!(kept, started, "the re-run keeps the first starter");

    // cancel, re-run: the old run's T2 is superseded
    assert_eq!(
        stock_lock_reset(&mut tx, &first).await.unwrap(),
        LockOutcome::Applied
    );
    let rerun = stock_lock_begin(&mut tx, &jan_set).await.unwrap();
    assert_eq!(rerun.generations()[0], first.generations()[0] + 1);
    let LockOutcome::NotApplied(states) = stock_lock_complete(&mut tx, &first).await.unwrap()
    else {
        panic!("the superseded run must not complete");
    };
    assert_eq!(states[0].stock_lock_generation, rerun.generations()[0]);
    assert_eq!(
        stock_lock_complete(&mut tx, &rerun).await.unwrap(),
        LockOutcome::Applied
    );
    let error = stock_lock_begin(&mut tx, &jan_set).await.unwrap_err();
    assert_refused(&error, ErrorKind::Validation, "already stock_locked");
    tx.commit().await.unwrap();
    drop(tx);

    // a starter field outside its in-flight status is refused by the database
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .savepoint(async |tx| {
            tx.execute(
                "UPDATE account_fiscal_period SET stock_lock_status = 'stock_locking' WHERE id = $1",
                &[&feb],
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), Some("23514"), "{error:?}");
    // a branch without the matching period cannot be part of a lock set
    tx.rollback().await.unwrap();
    drop(tx);
    company(&h, "New branch", Some(root)).await;
    let mut tx = h.begin().await.unwrap();
    let error = PeriodSet::of(&mut tx, jan).await.unwrap_err();
    assert_refused(&error, ErrorKind::Missing, "ensure_periods");
}

#[tokio::test]
async fn writers_ask_the_period_row_by_class() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    set_dates(
        &h,
        root,
        Some(d(2024, 2, 29)),
        Some(d(2024, 1, 31)),
        Some(d(2024, 2, 29)),
    )
    .await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 4, 30),
    )
    .await
    .unwrap();
    let mar = period_id(&h, root, d(2024, 3, 1)).await;
    let apr = period_id(&h, root, d(2024, 4, 1)).await;
    let mut tx = h.begin().await.unwrap();
    let set = PeriodSet::of(&mut tx, mar).await.unwrap();
    stock_lock_begin(&mut tx, &set).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let rows = [
        (root, d(2024, 1, 5)),
        (root, d(2024, 2, 5)),
        (root, d(2024, 3, 5)),
        (root, d(2024, 4, 5)),
        (root, d(2030, 1, 1)),
    ];
    let mut tx = h.begin().await.unwrap();
    let kinds = |verdicts: Vec<PeriodVerdict>| -> Vec<&'static str> {
        verdicts
            .iter()
            .map(|v| match v {
                PeriodVerdict::Open { .. } => "open",
                PeriodVerdict::NoPeriod { .. } => "none",
                PeriodVerdict::FiscalLock { .. } => "fiscal",
                PeriodVerdict::HardLock { .. } => "hard",
                PeriodVerdict::StockLock { .. } => "stock",
                _ => "other",
            })
            .collect()
    };
    let accounting = period_admission(&mut tx, WriteClass::Accounting, &rows)
        .await
        .unwrap();
    assert_eq!(
        kinds(accounting),
        ["hard", "fiscal", "open", "open", "none"]
    );
    let stock = period_admission(&mut tx, WriteClass::Stock, &rows)
        .await
        .unwrap();
    assert_eq!(kinds(stock), ["hard", "fiscal", "stock", "open", "none"]);
    assert_eq!(
        require_open_periods(&mut tx, WriteClass::Stock, &rows[3..4])
            .await
            .unwrap(),
        [apr]
    );
    let error = require_open_periods(&mut tx, WriteClass::Stock, &rows[2..3])
        .await
        .unwrap_err();
    assert_refused(&error, ErrorKind::Validation, "stock_locking");
    let error = require_open_periods(&mut tx, WriteClass::Accounting, &rows[4..])
        .await
        .unwrap_err();
    assert_refused(&error, ErrorKind::Missing, "ensure_periods");
    tx.rollback().await.unwrap();
    drop(tx);

    // a writer waits for a lock operation that holds the period's exclusive advisory lock
    let mut operation = h.begin().await.unwrap();
    operation
        .query(
            "SELECT pg_advisory_xact_lock($1::int4, $2::int4)",
            &[&PERIOD_LOCK_CLASS, &i32::try_from(apr).unwrap()],
        )
        .await
        .unwrap();
    let mut writer = h.begin().await.unwrap();
    let ask = period_admission(&mut writer, WriteClass::Accounting, &rows[3..4]);
    let release = async {
        until_a_session_waits(&h).await;
        operation.commit().await.unwrap();
    };
    let (asked, ()) = tokio::join!(ask, release);
    assert!(asked.unwrap()[0].is_open());
}

#[tokio::test]
async fn periods_change_only_through_their_operations_and_never_overlap() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 1, 31),
    )
    .await
    .unwrap();
    let jan = period_id(&h, root, d(2024, 1, 1)).await;
    let mut tx = h.begin().await.unwrap();
    let model = tx
        .env()
        .sudo(true)
        .records("account.fiscal.period")
        .unwrap();
    let record = model.browse([jan]).unwrap();
    let refused = "they cannot be created, edited or deleted directly";
    let error = record
        .write(
            &mut tx,
            RawVals::new().set_cell("status", Cell::Str("closed".into())),
        )
        .await
        .unwrap_err();
    assert_refused(&error, ErrorKind::Validation, refused);
    let error = record.unlink(&mut tx).await.unwrap_err();
    assert_refused(&error, ErrorKind::Validation, refused);
    let error = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("x".into()))],
        )
        .await
        .unwrap_err();
    assert_refused(&error, ErrorKind::Validation, refused);
    tx.rollback().await.unwrap();
    drop(tx);

    // the database refuses an overlapping regular period and a duplicate number; an adjustment
    // period on a day of a regular one is allowed
    let mut tx = h.begin().await.unwrap();
    let insert = "INSERT INTO account_fiscal_period (company_id, name, fiscal_year, kind, number, \
                  date_from, date_to, status, stock_lock_status, stock_lock_generation, \
                  hard_lock_generation) VALUES ($1, 'x', $2, $3, $4, $5, $6, 'open', 'open', 0, 0)";
    for (year, kind, number, from, to) in [
        (2024, "regular", 2, d(2024, 1, 20), d(2024, 2, 19)),
        (2024, "regular", 1, d(2025, 1, 1), d(2025, 1, 31)),
    ] {
        let error = tx
            .savepoint(async |tx| {
                tx.execute(insert, &[&root, &year, &kind, &number, &from, &to])
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.sqlstate().map(|s| &s[..2]), Some("23"), "{error:?}");
    }
    tx.execute(
        insert,
        &[
            &root,
            &2024_i32,
            &"adjustment",
            &13_i32,
            &d(2024, 1, 31),
            &d(2024, 1, 31),
        ],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
}

/// The refusal of a company the caller does not hold (the same for one that does not exist).
const NOT_ALLOWED: &str = "Access to unauthorized or invalid companies.";

/// A transaction of user 3 (the accounting manager of `scoped`) whose active companies are
/// `companies`.
async fn manager_in(h: &Harness, companies: &[i64]) -> Transaction {
    h.orm().begin_opts(manager(companies)).await.unwrap()
}

/// The begin options of user 3 working in `companies`.
fn manager(companies: &[i64]) -> BeginOptions {
    let allowed = Value::list(companies.iter().map(|c| Value::Int(*c)));
    BeginOptions::new(UserId::new(3).unwrap())
        .context(Context::from_pairs([("allowed_company_ids", allowed)]))
}

/// A harness whose policy makes user 3 an accounting manager; its user row exists, as the lock
/// runs record who started them.
async fn scoped() -> Harness {
    let h = builder()
        .policy(FieldPolicy::new().member(3, [MANAGER_GROUP]))
        .build()
        .await
        .unwrap();
    h.db()
        .client()
        .await
        .unwrap()
        .execute(
            "INSERT INTO res_users (id, name, login, active, share) \
             VALUES (3, 'Manager', 'manager', true, false)",
            &[],
        )
        .await
        .unwrap();
    h
}

#[tokio::test]
async fn period_operations_act_only_on_the_callers_companies() {
    let h = scoped().await;
    let mine = company(&h, "Mine", None).await;
    let branch = company(&h, "Mine branch", Some(mine)).await;
    let theirs = company(&h, "Theirs", None).await;
    for root in [mine, theirs] {
        ensure_periods_with(
            h.orm(),
            system(),
            &calendar(31, 12),
            root,
            d(2024, 1, 1),
            d(2024, 3, 31),
        )
        .await
        .unwrap();
    }
    // their January and February are closed; a stock run on January and a hard run on February
    // are in flight
    set_dates(&h, theirs, Some(d(2024, 2, 29)), None, None).await;
    let their_jan = period_id(&h, theirs, d(2024, 1, 10)).await;
    let their_feb = period_id(&h, theirs, d(2024, 2, 10)).await;
    let mut tx = h.begin().await.unwrap();
    sync_lock_dates(&mut tx, theirs).await.unwrap();
    let jan_set = PeriodSet::of(&mut tx, their_jan).await.unwrap();
    let feb_set = PeriodSet::of(&mut tx, their_feb).await.unwrap();
    let stock_run = stock_lock_begin(&mut tx, &jan_set).await.unwrap();
    let hard_run = hard_lock_begin(&mut tx, &feb_set).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    set_dates(&h, theirs, None, None, None).await;
    let before = periods(&h, theirs).await;

    let mut tx = manager_in(&h, &[mine]).await;
    let refused = [
        sync_lock_dates(&mut tx, theirs).await.map(|_| ()),
        sync_lock_dates(&mut tx, 999_999).await.map(|_| ()),
        PeriodSet::of(&mut tx, their_jan).await.map(|_| ()),
        PeriodSet::of(&mut tx, 999_999).await.map(|_| ()),
        stock_lock_begin(&mut tx, &jan_set).await.map(|_| ()),
        hard_lock_begin(&mut tx, &feb_set).await.map(|_| ()),
        stock_lock_complete(&mut tx, &stock_run).await.map(|_| ()),
        stock_lock_reset(&mut tx, &stock_run).await.map(|_| ()),
        hard_lock_complete(&mut tx, &hard_run).await.map(|_| ()),
        hard_lock_reset(&mut tx, &hard_run).await.map(|_| ()),
        period_admission(&mut tx, WriteClass::Accounting, &[(theirs, d(2024, 3, 1))])
            .await
            .map(|_| ()),
    ];
    for (n, result) in refused.into_iter().enumerate() {
        let Err(error) = result else {
            panic!("call {n} on the other company was not refused");
        };
        assert_refused(&error, ErrorKind::Access, NOT_ALLOWED);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        periods(&h, theirs).await,
        before,
        "nothing of theirs changed"
    );
    // nor are periods created for them, nor does an unknown company tell it does not exist
    for company in [theirs, 999_999] {
        let error = ensure_periods_with(
            h.orm(),
            manager(&[mine]),
            &calendar(31, 12),
            company,
            d(2024, 4, 1),
            d(2024, 4, 1),
        )
        .await
        .unwrap_err();
        assert_refused(&error, ErrorKind::Access, NOT_ALLOWED);
    }
    assert_eq!(periods(&h, theirs).await, before);
    for company in [mine, branch] {
        ensure_periods_with(
            h.orm(),
            manager(&[mine]),
            &calendar(31, 12),
            company,
            d(2024, 4, 1),
            d(2024, 4, 1),
        )
        .await
        .unwrap();
    }

    // the same calls on the manager's own company and its branch work
    set_dates(&h, mine, Some(d(2024, 1, 31)), None, None).await;
    let my_jan = period_id(&h, mine, d(2024, 1, 10)).await;
    let branch_feb = period_id(&h, branch, d(2024, 2, 10)).await;
    let mut tx = manager_in(&h, &[mine]).await;
    assert_eq!(sync_lock_dates(&mut tx, mine).await.unwrap().closed, 2);
    let set = PeriodSet::of(&mut tx, my_jan).await.unwrap();
    assert_eq!(set.ids().len(), 2, "the branch's January moves with mine");
    let run = stock_lock_begin(&mut tx, &set).await.unwrap();
    assert_eq!(
        stock_lock_reset(&mut tx, &run).await.unwrap(),
        LockOutcome::Applied
    );
    let run = hard_lock_begin(&mut tx, &set).await.unwrap();
    assert_eq!(
        hard_lock_complete(&mut tx, &run).await.unwrap(),
        LockOutcome::Applied
    );
    assert!(
        period_admission(&mut tx, WriteClass::Accounting, &[(mine, d(2024, 3, 1))])
            .await
            .unwrap()[0]
            .is_open()
    );
    // a branch follows the company the manager holds
    sync_lock_dates(&mut tx, branch).await.unwrap();
    assert_eq!(
        PeriodSet::of(&mut tx, branch_feb)
            .await
            .unwrap()
            .ids()
            .len(),
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        statuses(&periods(&h, branch).await)[0],
        ("2024/01", "hard_locked", "stock_locked")
    );
}

#[tokio::test]
async fn the_lock_date_sync_waits_for_writers_in_flight() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 2, 29),
    )
    .await
    .unwrap();
    let rows = [(root, d(2024, 1, 15))];
    let jan = period_id(&h, root, d(2024, 1, 15)).await;
    // T1: a writer admitted for the open January that has not committed
    let mut writer = h.begin().await.unwrap();
    assert_eq!(
        require_open_periods(&mut writer, WriteClass::Accounting, &rows)
            .await
            .unwrap(),
        [jan]
    );
    // a writer that has begun and not asked yet
    let mut late = h.begin().await.unwrap();
    // T2: the fiscal-year lock date now covers January; the sync waits for T1
    set_dates(&h, root, Some(d(2024, 1, 31)), None, None).await;
    let mut setter = h.begin().await.unwrap();
    let sync = sync_lock_dates(&mut setter, root);
    let release = async {
        until_a_session_waits(&h).await;
        writer.commit().await.unwrap();
    };
    let (synced, ()) = tokio::join!(sync, release);
    drop(writer);
    assert_eq!(synced.unwrap().closed, 1);
    setter.commit().await.unwrap();
    drop(setter);
    assert_eq!(statuses(&periods(&h, root).await)[0].1, "closed");
    // after T2 committed, the writer that had not asked yet and a new one are refused
    for tx in [&mut late, &mut h.begin().await.unwrap()] {
        let error = require_open_periods(tx, WriteClass::Accounting, &rows)
            .await
            .unwrap_err();
        assert_refused(&error, ErrorKind::Validation, "2024/01");
    }
    late.rollback().await.unwrap();
}

#[tokio::test]
async fn the_database_refuses_unknown_period_statuses() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 1, 31),
    )
    .await
    .unwrap();
    let jan = period_id(&h, root, d(2024, 1, 1)).await;
    let client = h.db().client().await.unwrap();
    for sql in [
        "UPDATE account_fiscal_period SET status = 'frozen', closed_at = now() WHERE id = $1",
        "UPDATE account_fiscal_period SET stock_lock_status = 'melted' WHERE id = $1",
    ] {
        let error = client.execute(sql, &[&jan]).await.unwrap_err();
        assert_eq!(error.code().map(|c| c.code()), Some("23514"), "{error:?}");
    }
}

#[tokio::test]
async fn a_company_without_a_valid_parent_path_is_refused_and_is_no_ancestor() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    let client = h.db().client().await.unwrap();
    let currency: i64 = client
        .query_one("SELECT id::int8 FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()
        .get(0);
    // a company whose parent store was never filled and one with an empty path, both hard-locked
    let mut broken = Vec::new();
    for (name, path) in [("No path", None), ("Empty path", Some(""))] {
        let id: i64 = client
            .query_one(
                "INSERT INTO res_company (name, active, currency_id, parent_path, hard_lock_date) \
                 VALUES ($1, true, $2, $3, '2030-12-31') RETURNING id::int8",
                &[&name, &currency, &path],
            )
            .await
            .unwrap()
            .get(0);
        broken.push(id);
    }
    for id in &broken {
        let error = ensure_periods_with(
            h.orm(),
            system(),
            &calendar(31, 12),
            *id,
            d(2024, 1, 1),
            d(2024, 1, 31),
        )
        .await
        .unwrap_err();
        assert!(error.message().contains("parent_path"), "{error:?}");
        let mut tx = h.begin().await.unwrap();
        let error = sync_lock_dates(&mut tx, *id).await.unwrap_err();
        assert!(error.message().contains("parent_path"), "{error:?}");
        tx.rollback().await.unwrap();
        drop(tx);
    }
    let created: i64 = client
        .query_one(
            "SELECT count(*)::int8 FROM account_fiscal_period WHERE company_id <> $1",
            &[&root],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(created, 0, "no period for any other company");
    // the empty path is no ancestor of the root: its hard lock date does not lock the root's periods
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 1, 31),
    )
    .await
    .unwrap();
    assert_eq!(
        statuses(&periods(&h, root).await),
        [("2024/01", "open", "open")]
    );
}

#[tokio::test]
async fn an_ensure_that_creates_nothing_writes_nothing() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    let cal = calendar(31, 12);
    let created = ensure_periods_with(h.orm(), system(), &cal, root, d(2024, 1, 1), d(2024, 2, 29))
        .await
        .unwrap();
    assert_eq!(created, 2);
    let serial = async || -> Option<i32> {
        h.db()
            .client()
            .await
            .unwrap()
            .query_one(
                "SELECT fiscal_period_serial FROM res_company WHERE id = $1",
                &[&root],
            )
            .await
            .unwrap()
            .get(0)
    };
    let before = serial().await;
    // another transaction holds the root company row; a call with nothing to create does not wait
    let mut holder = h.begin().await.unwrap();
    holder
        .execute("UPDATE res_company SET name = name WHERE id = $1", &[&root])
        .await
        .unwrap();
    let again = tokio::time::timeout(
        Duration::from_secs(5),
        ensure_periods_with(
            h.orm(),
            system(),
            &cal,
            root,
            d(2024, 1, 10),
            d(2024, 2, 10),
        ),
    )
    .await
    .expect("the call waited for the root company row")
    .unwrap();
    assert_eq!(again, 0);
    holder.rollback().await.unwrap();
    drop(holder);
    assert_eq!(
        serial().await,
        before,
        "the root company row was not written"
    );
}

#[tokio::test]
async fn a_writer_with_an_older_snapshot_keeps_its_snapshots_verdict() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 1, 31),
    )
    .await
    .unwrap();
    let rows = [(root, d(2024, 1, 15))];
    // the writer's snapshot is taken before the close
    let mut stale = h.begin().await.unwrap();
    stale
        .query("SELECT 1 FROM res_company LIMIT 1", &[])
        .await
        .unwrap();
    set_dates(&h, root, Some(d(2024, 1, 31)), None, None).await;
    let mut setter = h.begin().await.unwrap();
    assert_eq!(sync_lock_dates(&mut setter, root).await.unwrap().closed, 1);
    setter.commit().await.unwrap();
    drop(setter);
    assert!(
        period_admission(&mut stale, WriteClass::Accounting, &rows)
            .await
            .unwrap()[0]
            .is_open(),
        "under REPEATABLE READ the verdict is the snapshot's"
    );
    stale.rollback().await.unwrap();
    drop(stale);
    let mut fresh = h.begin().await.unwrap();
    let error = require_open_periods(&mut fresh, WriteClass::Accounting, &rows)
        .await
        .unwrap_err();
    assert_refused(&error, ErrorKind::Validation, "2024/01");
}

#[tokio::test]
async fn a_period_that_collides_on_its_key_names_the_existing_period() {
    let h = harness().await;
    let root = company(&h, "Root", None).await;
    ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 12),
        root,
        d(2024, 1, 1),
        d(2024, 1, 31),
    )
    .await
    .unwrap();
    // with a fiscal year ending on 31 January, February 2023 is period 1 of fiscal year 2024 as
    // well, without overlapping January 2024
    let error = ensure_periods_with(
        h.orm(),
        system(),
        &calendar(31, 1),
        root,
        d(2023, 2, 1),
        d(2023, 2, 1),
    )
    .await
    .unwrap_err();
    assert_refused(&error, ErrorKind::Validation, "2024/01");
    assert!(!error.message().contains("none"), "{error:?}");
}
