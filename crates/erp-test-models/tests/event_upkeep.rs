// SPDX-License-Identifier: MIT OR Apache-2.0
//! The event-partition upkeep of the outbox and the audit table (slice H1):
//! two processes at once create each missing day once, a table that is not installed or whose
//! parent is locked is a reported outcome, an eventful commit past the window fails with a typed
//! error that names the upkeep, the scheduled tick refreshes the cached audit store state, and the
//! generic partition upkeep never touches the closed event namespace.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use erp_fields::{Cell, DateTime, ErrorKind};
use erp_orm::audit::AuditStoreState;
use erp_orm::{
    BeginOptions, EventTableUpkeep, EventUpkeepReport, OperationIdentity, Orm, OrmConfig,
    PrincipalHint, RawVals, SUPERUSER_ID,
};
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, PartitionDecl, PartitionInterval, PartitionRetain,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls};

const OUTBOX: &str = "erp_event.ir_event_outbox";
const AUDIT: &str = "erp_event.ir_audit_entry";

/// An owner connection outside every ORM.
async fn owner(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}

async fn install_outbox(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.commit().await.unwrap();
}

/// The attached partitions of `table`, by name.
async fn partitions(h: &Harness, table: &str) -> Vec<String> {
    owner(h)
        .await
        .query(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = $1::text::regclass ORDER BY 1",
            &[&table],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

async fn drop_partition(h: &Harness, name: &str) {
    owner(h)
        .await
        .batch_execute(&format!("DROP TABLE erp_event.{name}"))
        .await
        .unwrap();
}

/// A second ORM lineage over its own pool: another process, as far as the database can tell.
fn other_node(h: &Harness, config: OrmConfig) -> Orm {
    let pool = erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    Orm::builder(Arc::clone(h.registry()), pool)
        .config(config)
        .build()
        .unwrap()
}

fn created(outcome: &EventTableUpkeep) -> Vec<String> {
    match outcome {
        EventTableUpkeep::Maintained { created } => created.clone(),
        other => panic!("expected Maintained, got {other:?}"),
    }
}

fn window_of(report: &EventUpkeepReport, table: &str) -> erp_orm::EventPartitionWindow {
    report
        .health
        .iter()
        .find(|w| w.table == table)
        .unwrap_or_else(|| panic!("no health figure for {table}: {:?}", report.health))
        .clone()
}

/// Two ORMs on separate, warmed pools start the upkeep while a third session holds the namespace
/// lock, so both are proven to wait on it at the same time (two advisory waiters in `pg_locks`)
/// before it is released: neither run errors, each re-created name is in exactly one of the two
/// reports, and both end with the full window.
#[tokio::test]
async fn event_upkeep_two_sessions_one_creates() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    install_outbox(&h).await;
    let outbox_days = partitions(&h, OUTBOX).await;
    let audit_days = partitions(&h, AUDIT).await;
    let (lost_outbox, lost_audit) = (outbox_days[3].clone(), audit_days[5].clone());
    drop_partition(&h, &lost_outbox).await;
    drop_partition(&h, &lost_audit).await;
    // a wait far longer than the test, so the forced overlap never turns into Busy
    let config = || OrmConfig {
        partition_lock_timeout: Duration::from_secs(120),
        ..OrmConfig::default()
    };
    let nodes = [other_node(&h, config()), other_node(&h, config())];
    for orm in &nodes {
        // open each pool's connection before the race
        let mut tx = orm.begin(PrincipalHint::system()).await.unwrap();
        tx.query("SELECT 1", &[]).await.unwrap();
        tx.rollback().await.unwrap();
    }
    let holder = owner(&h).await;
    holder
        .batch_execute("SELECT pg_advisory_lock(hashtextextended('erp_event.ir_event_outbox', 0))")
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for orm in nodes {
        tasks.push(tokio::spawn(async move {
            orm.maintain_event_partitions().await
        }));
    }
    let waiting = Instant::now();
    loop {
        let waiters: i64 = holder
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
                 AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if waiters == 2 {
            break;
        }
        assert!(
            waiting.elapsed() < Duration::from_secs(60),
            "the two upkeeps never waited on the namespace lock together ({waiters} waiting)"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    holder
        .batch_execute(
            "SELECT pg_advisory_unlock(hashtextextended('erp_event.ir_event_outbox', 0))",
        )
        .await
        .unwrap();
    let mut reports = Vec::new();
    for task in tasks {
        reports.push(task.await.unwrap().expect("neither upkeep errors"));
    }
    for (lost, pick) in [
        (
            &lost_outbox,
            (|r: &EventUpkeepReport| created(&r.outbox)) as fn(&EventUpkeepReport) -> Vec<String>,
        ),
        (&lost_audit, |r: &EventUpkeepReport| created(&r.audit)),
    ] {
        let listed: Vec<Vec<String>> = reports.iter().map(pick).collect();
        assert_eq!(
            listed.iter().filter(|c| c.contains(lost)).count(),
            1,
            "{lost} must be created by exactly one upkeep: {listed:?}"
        );
        assert!(
            listed
                .iter()
                .all(|c| c.is_empty() || c == &vec![lost.clone()]),
            "{listed:?}"
        );
    }
    for report in &reports {
        let outbox = window_of(report, OUTBOX);
        assert!(outbox.today_covered);
        assert_eq!(outbox.days_left, erp_orm::EVENT_PREMAKE_DAYS);
        let audit = window_of(report, AUDIT);
        assert!(audit.today_covered);
        assert_eq!(audit.days_left, erp_orm::audit::AUDIT_PREMAKE_DAYS);
    }
    assert_eq!(partitions(&h, OUTBOX).await, outbox_days);
    assert_eq!(partitions(&h, AUDIT).await, audit_days);
    h.cleanup().await.unwrap();
}

/// The name of the daily partition of `table` (unqualified) `offset` UTC days from today, by the
/// database's clock.
async fn day_partition(h: &Harness, table: &str, offset: i32) -> String {
    owner(h)
        .await
        .query_one(
            "SELECT $1::text || '_p' || \
             to_char((now() AT TIME ZONE 'UTC')::date + $2::int4, 'YYYYMMDD')",
            &[&table, &offset],
        )
        .await
        .unwrap()
        .get(0)
}

/// A stray plain table under the name of a future outbox day makes the outbox upkeep refuse; the
/// audit table is still maintained (its missing day created and committed) and the error names the
/// outbox problem.
#[tokio::test]
async fn a_broken_outbox_does_not_stop_the_audit_upkeep() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    install_outbox(&h).await;
    let stray = day_partition(&h, "ir_event_outbox", 2).await;
    drop_partition(&h, &stray).await;
    owner(&h)
        .await
        .batch_execute(&format!("CREATE TABLE erp_event.{stray} (n integer)"))
        .await
        .unwrap();
    let lost_audit = day_partition(&h, "ir_audit_entry", 4).await;
    drop_partition(&h, &lost_audit).await;
    let error = h
        .orm()
        .maintain_event_partitions()
        .await
        .expect_err("the stray outbox table is refused");
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "a later release event schema refused (forged-partition): \"erp_event\".\"{stray}\" exists \
             (relkind r) but is not a partition of \"ir_event_outbox\"; nothing was changed for it, \
             fix or drop the object by hand as the owner role (the install never adopts or repairs \
             it)"
        )
    );
    assert!(
        partitions(&h, AUDIT).await.contains(&lost_audit),
        "the audit day was created and committed"
    );
    h.cleanup().await.unwrap();
}

/// The window counts the days after today, as the audit store state does, and says separately
/// whether today is covered: full, today and tomorrow, today only, today missing.
#[tokio::test]
async fn the_partition_window_counts_days_after_today() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let audit = async || window_of_health(&h.orm().event_partition_health().await.unwrap());
    let full = audit().await;
    assert!(full.today_covered);
    assert_eq!(full.days_left, erp_orm::audit::AUDIT_PREMAKE_DAYS);
    assert!(!full.is_short());
    for (dropped_offset, today_covered, days_left) in [(2, true, 1), (1, true, 0), (0, false, 0)] {
        drop_partition(
            &h,
            &day_partition(&h, "ir_audit_entry", dropped_offset).await,
        )
        .await;
        let window = audit().await;
        assert_eq!(
            (window.today_covered, window.days_left, window.is_short()),
            (today_covered, days_left, true),
            "after dropping day +{dropped_offset}"
        );
        assert_eq!(
            h.orm().check_audit_store().await.unwrap(),
            AuditStoreState::PartitionWindowShort { days_left },
            "the store check counts the same days"
        );
    }
    h.cleanup().await.unwrap();
}

fn window_of_health(health: &[erp_orm::EventPartitionWindow]) -> erp_orm::EventPartitionWindow {
    health
        .iter()
        .find(|w| w.table == AUDIT)
        .unwrap_or_else(|| panic!("no health figure for {AUDIT}: {health:?}"))
        .clone()
}

static CAPTURE: AddonDescriptor = AddonDescriptor {
    name: "test_upkeep_capture",
    depends: &["base"],
    exact_only: false,
};
const EVENTFUL: &str = "test_upkeep_capture.thing";

async fn eventful_commit(h: &Harness) -> erp_fields::Result<()> {
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    tx.env()
        .records(EVENTFUL)
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("one".into()))],
        )
        .await
        .unwrap();
    tx.commit().await
}

/// With today's outbox partition gone (the clock cannot be moved), an eventful commit fails with
/// kind `Concurrency` and the message that names the upkeep; once the upkeep ran, the same commit
/// succeeds.
#[tokio::test]
async fn outbox_commit_past_window_fails_typed_then_succeeds_after_upkeep() {
    let h = Harness::builder([&base_users::ADDON, &CAPTURE])
        .with_decls([ModelDecl::new("test_upkeep_capture", EVENTFUL)
            .eventful(true)
            .fields(const { &[FieldDecl::char("name")] })])
        .build()
        .await
        .unwrap();
    install_outbox(&h).await;
    let today = day_partition(&h, "ir_event_outbox", 0).await;
    drop_partition(&h, &today).await;
    let error = eventful_commit(&h)
        .await
        .expect_err("no partition for today");
    assert_eq!(error.kind(), ErrorKind::Concurrency, "{error:?}");
    assert_eq!(
        error.message(),
        "outbox: no partition of erp_event.ir_event_outbox covers the transaction's day; the \
         event partition upkeep has not run (it runs at server start and hourly; `erp audit \
         maintain` refreshes both event tables)"
    );
    assert_eq!(
        error.sqlstate(),
        None,
        "never retried as a serialization failure"
    );
    let report = h.orm().maintain_event_partitions().await.unwrap();
    assert_eq!(created(&report.outbox), vec![today.clone()]);
    eventful_commit(&h).await.unwrap();
    let rows: i64 = owner(&h)
        .await
        .query_one(&format!("SELECT count(*) FROM erp_event.{today}"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 1);
    h.cleanup().await.unwrap();
}

/// Audit installed, outbox not: the outbox is `NotInstalled` with no health window, the audit
/// table gets its missing day and reports its full window.
#[tokio::test]
async fn event_upkeep_reports_not_installed_outbox() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let lost = partitions(&h, AUDIT).await[7].clone();
    drop_partition(&h, &lost).await;
    let report = h.orm().maintain_event_partitions().await.unwrap();
    assert_eq!(report.outbox, EventTableUpkeep::NotInstalled);
    assert_eq!(created(&report.audit), vec![lost]);
    let outbox = window_of(&report, OUTBOX);
    assert!(!outbox.installed);
    assert!(!outbox.today_covered);
    assert_eq!(outbox.days_left, 0);
    assert!(
        !outbox.is_short(),
        "a table that is not installed is not short"
    );
    let audit = window_of(&report, AUDIT);
    assert!(audit.installed);
    assert!(audit.today_covered);
    assert_eq!(audit.days_left, erp_orm::audit::AUDIT_PREMAKE_DAYS);
    assert_eq!(
        h.orm().event_partition_health().await.unwrap(),
        report.health
    );
    h.cleanup().await.unwrap();
}

/// Another session holds `ACCESS EXCLUSIVE` on the outbox while a day is missing: the upkeep gives
/// up after its lock timeout and reports the outbox `Busy`, the audit table is still maintained,
/// and after the lock is gone the next run creates the day.
#[tokio::test]
async fn event_upkeep_busy_under_lock_timeout() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    install_outbox(&h).await;
    let lost = partitions(&h, OUTBOX).await[2].clone();
    drop_partition(&h, &lost).await;
    let timeout = Duration::from_millis(300);
    let node = other_node(
        &h,
        OrmConfig {
            partition_lock_timeout: timeout,
            ..OrmConfig::default()
        },
    );
    let locker = owner(&h).await;
    locker
        .batch_execute("BEGIN; LOCK TABLE erp_event.ir_event_outbox IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let started = Instant::now();
    let report = node.maintain_event_partitions().await.unwrap();
    let waited = started.elapsed();
    assert_eq!(report.outbox, EventTableUpkeep::Busy);
    assert_eq!(created(&report.audit), Vec::<String>::new());
    assert!(
        waited < Duration::from_secs(5),
        "the upkeep waited {waited:?}, its lock timeout is {timeout:?}"
    );
    locker.batch_execute("ROLLBACK").await.unwrap();
    let report = node.maintain_event_partitions().await.unwrap();
    assert_eq!(created(&report.outbox), vec![lost]);
    h.cleanup().await.unwrap();
}

/// A runtime ORM of its own lineage cached `PartitionWindowShort { days_left: 0 }`; one tick body
/// (upkeep on the owner ORM, then the store check on the runtime ORM) makes it `Ready`, without
/// any other call on the runtime ORM.
#[tokio::test]
async fn audit_store_recovers_after_tick() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let runtime = other_node(&h, OrmConfig::default());
    let days = partitions(&h, AUDIT).await;
    drop_partition(&h, &days[1]).await;
    assert_eq!(
        runtime.check_audit_store().await.unwrap(),
        AuditStoreState::PartitionWindowShort { days_left: 0 }
    );
    assert!(!runtime.audit_store_state().allows_writes());
    let tick = h.orm().event_upkeep_tick(&runtime).await.unwrap();
    assert_eq!(created(&tick.report.audit), vec![days[1].clone()]);
    assert_eq!(tick.audit_store, AuditStoreState::Ready);
    assert_eq!(runtime.audit_store_state(), AuditStoreState::Ready);
    h.cleanup().await.unwrap();
}

static EVENT_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_upkeep_event",
    depends: &["base"],
    exact_only: false,
};
const EVENT_LEAF: &str = "test_upkeep_event.event";

fn event_decl() -> ModelDecl {
    ModelDecl::new("test_upkeep_event", EVENT_LEAF)
        .log_access(false)
        .fields(
            const {
                &[
                    FieldDecl::timestamptz("recorded_at")
                        .required(true)
                        .server_set(true),
                    FieldDecl::char("name"),
                ]
            },
        )
        .partition(
            PartitionDecl::new(PartitionInterval::Day, 7, PartitionRetain::Intervals(30))
                .by("recorded_at"),
        )
}

/// A registry with a declared, partitioned event leaf: its table and an old partition exist in
/// `erp_event` (made by hand here; the generic sync never makes them). The generic upkeep of the
/// whole registry has no entry for the leaf and changes none of its partitions (the 2020 one would
/// be retired under `retain = 30 days`, and no day ahead is created); the single-model calls refuse it.
#[tokio::test]
async fn generic_upkeep_leaves_event_leaf_untouched() {
    let h = Harness::builder([&base_users::ADDON, &EVENT_ADDON])
        .without_audit_store()
        .with_core_event_decls([event_decl()])
        .build()
        .await
        .unwrap();
    let table = h.registry().model(EVENT_LEAF).unwrap().table.to_string();
    let qualified = format!("erp_event.{table}");
    owner(&h)
        .await
        .batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS erp_event; \
             CREATE TABLE {qualified} (id bigint NOT NULL, recorded_at timestamptz NOT NULL, \
               name varchar, PRIMARY KEY (id, recorded_at)) PARTITION BY RANGE (recorded_at); \
             CREATE TABLE erp_event.{table}_p20200101 PARTITION OF {qualified} \
               FOR VALUES FROM ('2020-01-01 00:00:00+00') TO ('2020-01-02 00:00:00+00')"
        ))
        .await
        .unwrap();
    let before = partitions(&h, &qualified).await;
    assert_eq!(before, vec![format!("{table}_p20200101")]);
    let now: DateTime = owner(&h)
        .await
        .query_one("SELECT (now() AT TIME ZONE 'UTC')", &[])
        .await
        .unwrap()
        .get(0);
    let report = h
        .orm()
        .maintain_partitions(h.db().pool(), now)
        .await
        .unwrap();
    assert!(
        report.entries().iter().all(|e| e.model() != EVENT_LEAF),
        "{:?}",
        report.entries()
    );
    assert!(report.is_complete(), "{:?}", report.entries());
    assert!(
        h.orm()
            .partition_health(h.db().pool())
            .await
            .unwrap()
            .iter()
            .all(|w| w.model() != EVENT_LEAF)
    );
    assert_eq!(partitions(&h, &qualified).await, before);
    let error = h
        .orm()
        .ensure_partitions(h.db().pool(), EVENT_LEAF, erp_orm::Horizon::Declared)
        .await
        .expect_err("the generic upkeep refuses an event leaf");
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "ensure_partitions on 'test_upkeep_event.event': the model is in the closed event \
         namespace; its partitions are kept by Orm::maintain_event_partitions, never by the \
         generic partition upkeep"
    );
    let error = h
        .orm()
        .retire_partitions(h.db().pool(), EVENT_LEAF, erp_orm::KeepAfter::Declared)
        .await
        .expect_err("the generic upkeep refuses an event leaf");
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "retire_partitions on 'test_upkeep_event.event': the model is in the closed event \
         namespace; its partitions are kept by Orm::maintain_event_partitions, never by the \
         generic partition upkeep"
    );
    assert_eq!(partitions(&h, &qualified).await, before);
    h.cleanup().await.unwrap();
}
