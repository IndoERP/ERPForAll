//! leaf partitioning against PostgreSQL:
//! the server-set key, schema sync (create, adopt, refuse), the upkeep calls and their report, the
//! missing-partition error and the generation rules (025 is a registry case,
//! `erp-registry/tests/partition.rs`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use erp_data::ImpexExt;
use erp_fields::{Cell, DateTime, ErrorKind, Value};
use erp_orm::{
    BoxFuture, Changes, Context, Horizon, KeepAfter, Orm, OrmConfig, PartitionAction,
    PartitionStep, RawVals, Refresh, Refreshed, Signal, StaticPolicy, UpsertCall,
};
use erp_registry::{
    FieldDecl, ModelDecl, PARTITION_ADR, PartitionInterval, Registry, RegistryBuilder,
    partition_name,
};
use erp_test_models::{base_users, test_partition};
use erp_testkit::deadpool_postgres::Pool;
use erp_testkit::{Harness, assert_query_count};

// ---------------------------------------------------------------------------------------------
// helpers

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_partition::ADDON])
        .build()
        .await
        .expect("harness")
}

/// A second pool on the harness database (another process's view).
fn pool(h: &Harness, size: usize) -> Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

/// An ORM on `registry` and `pool` with the given config.
fn orm_on(registry: Arc<Registry>, pool: Pool, config: OrmConfig) -> Orm {
    Orm::builder(registry, pool)
        .policy(StaticPolicy::AllowAll)
        .config(config)
        .build()
        .unwrap()
}

/// Run raw SQL on a pooled connection of its own (a hand, another process).
async fn exec(h: &Harness, sql: &str) {
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// The database's current time (UTC), as the ORM's server-set fields take it.
async fn db_now(h: &Harness) -> DateTime {
    let client = h.db().client().await.unwrap();
    client
        .query_one("SELECT (now() AT TIME ZONE 'UTC')", &[])
        .await
        .unwrap()
        .get(0)
}

/// Partitions attached to `table` (pending detaches included), by name.
async fn attached(h: &Harness, table: &str) -> Vec<String> {
    let client = h.db().client().await.unwrap();
    client
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

/// The name of the partition of `table` holding `t`, `days` days later (our naming).
fn day_partition(table: &str, t: DateTime, days: i64) -> (String, DateTime, DateTime) {
    let start = PartitionInterval::Day
        .shift(PartitionInterval::Day.start_of(t).unwrap(), days)
        .unwrap();
    let end = PartitionInterval::Day.next(start).unwrap();
    (
        partition_name(table, PartitionInterval::Day, start).unwrap(),
        start,
        end,
    )
}

/// The name of the partition of `table` holding `t`, `months` months later.
fn month_partition(table: &str, t: DateTime, months: i64) -> String {
    let start = PartitionInterval::Month
        .shift(PartitionInterval::Month.start_of(t).unwrap(), months)
        .unwrap();
    partition_name(table, PartitionInterval::Month, start).unwrap()
}

/// Create and attach a daily partition of `scale_event` by hand (as an older upkeep would have),
/// with one row in it.
async fn old_partition(h: &Harness, now: DateTime, days: i64) -> String {
    let (name, from, to) = day_partition("scale_event", now, days);
    exec(
        h,
        &format!(
            "CREATE TABLE \"{name}\" (LIKE scale_event INCLUDING DEFAULTS INCLUDING CONSTRAINTS); \
             ALTER TABLE scale_event ATTACH PARTITION \"{name}\" FOR VALUES FROM ('{from}') TO \
             ('{to}'); INSERT INTO scale_event (name, create_date) VALUES ('old', '{from}')"
        ),
    )
    .await;
    name
}

/// Detach and drop a partition by hand.
async fn drop_partition(h: &Harness, table: &str, name: &str) {
    exec(
        h,
        &format!("ALTER TABLE \"{table}\" DETACH PARTITION \"{name}\"; DROP TABLE \"{name}\""),
    )
    .await;
}

fn vals(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// Commit and drop, so the connection goes back to the pool (a committed `Transaction` keeps it).
async fn finish(mut tx: erp_orm::Transaction) {
    tx.commit().await.unwrap();
}

/// `entry` is a failed step with this reason and, when given, this SQLSTATE and error kind (none:
/// the upkeep's own text, no database error behind it).
#[track_caller]
fn assert_failed(
    entry: &erp_orm::PartitionEntry,
    step: PartitionStep,
    reason: &str,
    cause: Option<(&str, ErrorKind)>,
) {
    assert_eq!(entry.step(), Some(step), "{entry:?}");
    assert_eq!(entry.reason(), Some(reason), "{entry:?}");
    assert_eq!(entry.sqlstate(), cause.map(|c| c.0), "{entry:?}");
    assert_eq!(entry.error_kind(), cause.map(|c| c.1), "{entry:?}");
}

#[track_caller]
fn assert_error(error: &erp_fields::Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error:?}");
    assert_eq!(error.message(), message);
}

/// create_date with 1 INSERT, a row of the next day lands in the next partition and both read
/// back by id in 1 statement; server_set fields: the transaction timestamp at insert, caller
/// values refused from create, write and a data file, readonly in fields_get, primary key (id,
/// recorded_at). Two ORM transactions on two days need a clock fixture the ORM does not have; the
/// bad-key registry error is in erp-registry/tests/partition.rs)
#[tokio::test]
async fn spec_ormx_026_the_partition_key_is_server_time() {
    let h = Harness::builder([
        &base_users::ADDON,
        &test_partition::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let events = tx.env().records("scale.event").unwrap();
    let created = assert_query_count!(tx, >= 1, <= 1, {
        events
            .create_values(
                &mut tx,
                &[
                    vals(&[("name", Value::str("a"))]),
                    vals(&[("name", Value::str("b"))]),
                ],
            )
            .await
            .unwrap()
    });
    let now = tx
        .now()
        .expect("the INSERT returned the transaction timestamp");
    assert_eq!(
        created.values(&mut tx, "create_date").await.unwrap(),
        vec![Value::DateTime(now), Value::DateTime(now)]
    );
    // a row of the next day (a transaction on the next day would set it so)
    let (today, _, _) = day_partition("scale_event", now, 0);
    let (tomorrow, from, _) = day_partition("scale_event", now, 1);
    let next = tx
        .query(
            "INSERT INTO scale_event (name, create_date) VALUES ('c', $1) RETURNING id",
            &[&from],
        )
        .await
        .unwrap();
    let next_id: i64 = next[0].get(0);
    let mut ids = created.ids();
    ids.push(next_id);
    let rows = tx
        .query(
            "SELECT tableoid::regclass::text FROM scale_event WHERE id = ANY($1) ORDER BY id",
            &[&ids],
        )
        .await
        .unwrap();
    let homes: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(homes, vec![today.clone(), today, tomorrow]);
    // read by id across partitions: one statement, as on an unpartitioned model
    tx.invalidate_all().await.unwrap();
    let all = events.browse(ids.iter().copied()).unwrap();
    let names = assert_query_count!(tx, >= 1, <= 1, {
        all.values(&mut tx, "name").await.unwrap()
    });
    assert_eq!(
        names,
        vec![Value::str("a"), Value::str("b"), Value::str("c")]
    );

    // a declared server-set key: the transaction timestamp, never a caller's value
    let audits = tx.env().records("scale.audit").unwrap();
    let refused = "'scale.audit.recorded_at' is set by the server and cannot be written";
    let error = audits
        .create_values(
            &mut tx,
            &[vals(&[
                ("what", Value::str("x")),
                ("recorded_at", Value::str("2020-01-01 00:00:00")),
            ])],
        )
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Programming, refused);
    let audit = audits
        .create_values(&mut tx, &[vals(&[("what", Value::str("ok"))])])
        .await
        .unwrap();
    assert_eq!(
        audit.values(&mut tx, "recorded_at").await.unwrap(),
        vec![Value::DateTime(now)]
    );
    let home: String = tx
        .query(
            "SELECT tableoid::regclass::text FROM scale_audit WHERE id = ANY($1)",
            &[&audit.ids()],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(home, month_partition("scale_audit", now, 0));
    let error = audit
        .write_values(
            &mut tx,
            &vals(&[("recorded_at", Value::str("2020-01-01 00:00:00"))]),
        )
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Programming, refused);
    // a data file (the import path data files take) setting it
    let error = audits
        .load(
            &mut tx,
            &["what", "recorded_at"],
            &[vec!["y".to_owned(), "2020-01-01 00:00:00".to_owned()]],
        )
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Programming, refused);
    let described = audits
        .fields_get(&mut tx, Some(&["recorded_at"]), Some(&["readonly"]))
        .await
        .unwrap();
    assert_eq!(
        described["recorded_at"]["readonly"],
        Value::Bool(true),
        "{described:?}"
    );
    let pk: String = tx
        .query(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = \
             'scale_audit'::regclass AND contype = 'p'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let key = h
        .registry()
        .model("scale.audit")
        .unwrap()
        .partition
        .unwrap()
        .key_name();
    assert_eq!(pk, format!("PRIMARY KEY (id, {key})"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_026_a_caller_create_date_is_refused_on_partitioned_models() {
    let h = Harness::builder([
        &base_users::ADDON,
        &test_partition::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let stamp = Value::str("2020-01-01 00:00:00");
    for (model, other) in [("scale.event", "name"), ("scale.audit", "what")] {
        let records = tx.env().records(model).unwrap();
        let refused = format!("'{model}.create_date' is set by the server and cannot be written");
        let error = records
            .create_values(
                &mut tx,
                &[vals(&[
                    (other, Value::str("x")),
                    ("create_date", stamp.clone()),
                ])],
            )
            .await
            .unwrap_err();
        assert_error(&error, ErrorKind::Programming, &refused);
        // a refused create left nothing behind
        let counted = tx
            .query(
                &format!("SELECT count(*) FROM {}", model.replace('.', "_")),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(counted[0].get::<_, i64>(0), 0, "{model}");

        let existing = records
            .create_values(&mut tx, &[vals(&[(other, Value::str("ok"))])])
            .await
            .unwrap();
        let error = existing
            .write_values(&mut tx, &vals(&[("create_date", stamp.clone())]))
            .await
            .unwrap_err();
        assert_error(&error, ErrorKind::Programming, &refused);
        let error = records
            .load(
                &mut tx,
                &[other, "create_date"],
                &[vec!["y".to_owned(), "2020-01-01 00:00:00".to_owned()]],
            )
            .await
            .unwrap_err();
        assert_error(&error, ErrorKind::Programming, &refused);
        existing
            .write_values(
                &mut tx,
                &vals(&[(other, Value::str("again")), ("write_date", stamp.clone())]),
            )
            .await
            .unwrap();
        // an upsert needs a business key, and a partitioned model cannot have one
        let key = records.spec().partition.unwrap().key_name();
        let message = format!(
            "upsert on '{model}': rows of a partitioned model are inserted with create, not \
             upserted (a unique constraint of a partitioned table must include the partition key \
             '{key}', which the server sets on every insert)"
        );
        for row in [
            RawVals::new().set_cell(other, Cell::Str("z".into())),
            RawVals::new()
                .set_cell(other, Cell::Str("z".into()))
                .set_cell(
                    "create_date",
                    Cell::DateTime(DateTime::parse_odoo("2020-01-01 00:00:00").unwrap()),
                ),
        ] {
            let error = records
                .upsert(&mut tx, UpsertCall::new("_any").with_rows([row]))
                .await
                .unwrap_err();
            assert_error(&error, ErrorKind::Programming, &message);
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A test writer: inserts events one transaction at a time until stopped, recording the slowest
/// insert and every error.
struct Writer {
    stop: Arc<AtomicBool>,
    inserts: Arc<AtomicUsize>,
    slowest: Arc<Mutex<Duration>>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Writer {
    /// Wait until `n` inserts succeeded; fail with the writer's errors after a minute.
    async fn wait_for(&self, n: usize) {
        let reached = tokio::time::timeout(Duration::from_secs(60), async {
            while self.inserts.load(Ordering::SeqCst) < n {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            reached.is_ok(),
            "the writer did not reach {n} inserts: {:?}",
            self.errors.lock().unwrap()
        );
    }

    fn start(orm: Orm) -> (Self, tokio::task::JoinHandle<()>) {
        let writer = Self {
            stop: Arc::new(AtomicBool::new(false)),
            inserts: Arc::new(AtomicUsize::new(0)),
            slowest: Arc::new(Mutex::new(Duration::ZERO)),
            errors: Arc::new(Mutex::new(Vec::new())),
        };
        let (stop, inserts, slowest, errors) = (
            Arc::clone(&writer.stop),
            Arc::clone(&writer.inserts),
            Arc::clone(&writer.slowest),
            Arc::clone(&writer.errors),
        );
        let task = tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                let started = Instant::now();
                let result = orm
                    .run(erp_orm::SUPERUSER_ID, Context::new(), async |tx| {
                        let events = tx.env().records("scale.event")?;
                        events
                            .create_values(tx, &[vals(&[("name", Value::str("w"))])])
                            .await
                            .map(|_| ())
                    })
                    .await;
                let took = started.elapsed();
                let mut slow = slowest.lock().unwrap();
                *slow = (*slow).max(took);
                drop(slow);
                match result {
                    Ok(()) => {
                        inserts.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) => errors.lock().unwrap().push(format!("{e:?}")),
                }
            }
        });
        (writer, task)
    }
}

/// `ensure_partitions` creates the declared window, from two processes at once each partition once;
/// `retire_partitions` detaches (concurrently, a writer keeps inserting) and drops what is older
/// than the retention; an interrupted detach is listed `finalized` by the next call;
/// `maintain_partitions` is the union for every partitioned model; `retain = "all"` never retires;
/// failures are listed with their reason.
#[tokio::test]
async fn spec_ormx_027_ensure_and_retire_keep_the_window_and_report_every_action() {
    let h = harness().await;
    let now = db_now(&h).await;
    let day = |n: i64| day_partition("scale_event", now, n).0;
    // Given: partitions up to today only
    drop_partition(&h, "scale_event", &day(1)).await;
    drop_partition(&h, "scale_event", &day(2)).await;
    assert_eq!(attached(&h, "scale_event").await, vec![day(0)]);

    // two processes at once
    let (owner_a, owner_b) = (h.db().pool().clone(), pool(&h, 2));
    let other = orm_on(Arc::clone(h.registry()), pool(&h, 2), OrmConfig::default());
    let (a, b) = tokio::join!(
        h.orm()
            .ensure_partitions(&owner_a, "scale.event", Horizon::Declared),
        other.ensure_partitions(&owner_b, "scale.event", Horizon::Declared),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let (first, second) = if a.created().is_empty() {
        (b, a)
    } else {
        (a, b)
    };
    assert_eq!(first.created(), [day(1), day(2)]);
    assert_eq!(first.existed(), [day(0)]);
    assert_eq!(second.created(), Vec::<&str>::new());
    assert_eq!(second.existed(), [day(0), day(1), day(2)]);
    assert!(first.is_complete() && second.is_complete());
    assert_eq!(
        attached(&h, "scale_event").await,
        vec![day(0), day(1), day(2)]
    );

    // retire with a writer running: partitions that ended 3 days ago or earlier go
    for n in [-5, -4, -3] {
        old_partition(&h, now, n).await;
    }
    let (writer, task) = Writer::start(other.clone());
    // let the writer get going
    writer.wait_for(3).await;
    let retired = h
        .orm()
        .retire_partitions(&owner_a, "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    let before = writer.inserts.load(Ordering::SeqCst);
    writer.wait_for(before + 3).await;
    writer.stop.store(true, Ordering::SeqCst);
    task.await.unwrap();
    assert_eq!(*writer.errors.lock().unwrap(), Vec::<String>::new());
    let slowest = *writer.slowest.lock().unwrap();
    assert!(
        slowest < OrmConfig::default().partition_lock_timeout,
        "the writer waited {slowest:?}"
    );
    assert_eq!(retired.detached(), [day(-5), day(-4)]);
    assert_eq!(retired.dropped(), [day(-5), day(-4)]);
    assert!(retired.is_complete(), "{retired:?}");
    assert_eq!(
        attached(&h, "scale_event").await,
        vec![day(-3), day(0), day(1), day(2)]
    );

    // an interrupted detach: an open snapshot keeps the concurrent detach waiting past the
    // upkeep's lock_timeout, which cancels it and leaves the partition pending
    let stale = old_partition(&h, now, -7).await;
    let hasty = orm_on(
        Arc::clone(h.registry()),
        pool(&h, 1),
        OrmConfig {
            partition_lock_timeout: Duration::from_millis(500),
            ..OrmConfig::default()
        },
    );
    let reader = h.db().client().await.unwrap();
    reader
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM scale_event")
        .await
        .unwrap();
    let interrupted = hasty
        .retire_partitions(&owner_a, "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    reader.batch_execute("COMMIT").await.unwrap();
    drop(reader);
    let failures: Vec<_> = interrupted.failures().collect();
    assert_eq!(failures.len(), 1, "{interrupted:?}");
    assert_eq!(failures[0].partition(), stale);
    assert_failed(
        failures[0],
        PartitionStep::Detach,
        "canceling statement due to lock timeout (SQLSTATE 55P03)",
        Some(("55P03", ErrorKind::Lock)),
    );
    assert!(failures[0].is_lock_timeout());
    assert!(interrupted.detached().is_empty());
    let pending: bool = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT inhdetachpending FROM pg_inherits WHERE inhrelid = $1::text::regclass",
            &[&stale],
        )
        .await
        .unwrap()
        .get(0);
    assert!(pending, "the interrupted detach left the partition pending");
    let next = h
        .orm()
        .ensure_partitions(&owner_a, "scale.event", Horizon::Declared)
        .await
        .unwrap();
    assert_eq!(next.finalized(), [stale.as_str()]);
    assert_eq!(next.existed(), [day(0), day(1), day(2)]);

    // maintain: ensure and retire for every partitioned model; the finalized table, marked when
    // its retirement started, is dropped; scale.audit keeps everything
    let maintained = h.orm().maintain_partitions(&owner_a, now).await.unwrap();
    let got: Vec<(&str, &str, String)> = maintained
        .entries()
        .iter()
        .map(|e| (e.model(), e.partition(), e.action().to_string()))
        .collect();
    let month = |n: i64| month_partition("scale_audit", now, n);
    let expected: Vec<(&str, String, &str)> = vec![
        ("scale.event", day(0), "existed"),
        ("scale.event", day(1), "existed"),
        ("scale.event", day(2), "existed"),
        ("scale.event", stale.clone(), "dropped"),
        ("scale.audit", month(0), "existed"),
        ("scale.audit", month(1), "existed"),
        ("scale.audit", month(2), "existed"),
    ];
    let expected: Vec<(&str, &str, String)> = expected
        .iter()
        .map(|(m, p, a)| (*m, p.as_str(), (*a).to_owned()))
        .collect();
    assert_eq!(got, expected);
    assert_eq!(
        attached(&h, "scale_event").await,
        vec![day(-3), day(0), day(1), day(2)]
    );

    // retain = "all": never retired
    let error = h
        .orm()
        .retire_partitions(&owner_a, "scale.audit", KeepAfter::Declared)
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        "retire_partitions on 'scale.audit': the model keeps every partition (retain = \"all\")",
    );
    assert_eq!(
        attached(&h, "scale_audit").await,
        vec![month(0), month(1), month(2)]
    );

    // a failed step is listed with its reason, the others go on: a stray table holds the name of
    // a partition of the window
    let (stray, _, _) = day_partition("scale_event", now, 4);
    exec(&h, &format!("CREATE TABLE \"{stray}\" (x int)")).await;
    let (fifth, _, _) = day_partition("scale_event", now, 5);
    let horizon = PartitionInterval::Day
        .shift(PartitionInterval::Day.start_of(now).unwrap(), 5)
        .unwrap();
    let report = h
        .orm()
        .ensure_partitions(&owner_a, "scale.event", Horizon::Through(horizon))
        .await
        .unwrap();
    assert_eq!(report.created(), [day(3), fifth]);
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].partition(), stray);
    assert_failed(
        failures[0],
        PartitionStep::Create,
        &format!("relation \"{stray}\" already exists (SQLSTATE 42P07)"),
        Some(("42P07", ErrorKind::Database)),
    );
    assert!(!failures[0].is_lock_timeout());
    let error = report.into_result().unwrap_err();
    assert_error(
        &error,
        ErrorKind::Database,
        &format!(
            "partition upkeep failed: scale.event {stray}: create: relation \"{stray}\" already \
             exists (SQLSTATE 42P07)"
        ),
    );
    h.cleanup().await.unwrap();
}

/// The upkeep refuses a model that is not partitioned, a cutoff in the future and a lock timeout
/// out of bounds, before it touches anything.
#[tokio::test]
async fn upkeep_refuses_wrong_calls() {
    let h = harness().await;
    let owner = h.db().pool().clone();
    let error = h
        .orm()
        .ensure_partitions(&owner, "scale.note", Horizon::Declared)
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        "ensure_partitions on 'scale.note': the model is not partitioned",
    );
    let error = h
        .orm()
        .ensure_partitions(&owner, "scale.nope", Horizon::Declared)
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        "Model 'scale.nope' does not exist in registry.",
    );
    let later = PartitionInterval::Day
        .shift(
            PartitionInterval::Day.start_of(db_now(&h).await).unwrap(),
            2,
        )
        .unwrap();
    let error = h
        .orm()
        .retire_partitions(&owner, "scale.event", KeepAfter::Time(later))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    // the current time is the database's, read by the call
    assert!(
        error.message().starts_with(&format!(
            "retire_partitions on 'scale.event': keep_after {later} is later than the current \
             time "
        )) && error
            .message()
            .ends_with("; only partitions that ended in the past are retired"),
        "{}",
        error.message()
    );
    for timeout in [Duration::ZERO, Duration::from_secs(3_601)] {
        let orm = orm_on(
            Arc::clone(h.registry()),
            pool(&h, 1),
            OrmConfig {
                partition_lock_timeout: timeout,
                ..OrmConfig::default()
            },
        );
        let error = orm
            .ensure_partitions(&owner, "scale.event", Horizon::Declared)
            .await
            .unwrap_err();
        assert_error(
            &error,
            ErrorKind::Programming,
            &format!(
                "OrmConfig::partition_lock_timeout is {timeout:?}; it must be from 100ms to \
                 3600s (0 would wait forever)"
            ),
        );
    }
    h.cleanup().await.unwrap();
}

/// An insert beyond the partition window fails with the model, the key's value and the declared
/// window, recognised from SQLSTATE 23514 without a constraint on a partitioned model's table;
/// the health check reports the window short.
#[tokio::test]
async fn spec_ormx_028_an_insert_beyond_the_window_fails_loudly() {
    let h = harness().await;
    let now = db_now(&h).await;
    // the window ended yesterday: upkeep has not run since
    for n in 0..=2 {
        drop_partition(&h, "scale_event", &day_partition("scale_event", now, n).0).await;
    }
    old_partition(&h, now, -1).await;
    let health = h.orm().partition_health(h.db().pool()).await.unwrap();
    let event = health.iter().find(|w| w.model() == "scale.event").unwrap();
    assert_eq!(event.covered(), 0);
    assert!(event.is_short());
    let audit = health.iter().find(|w| w.model() == "scale.audit").unwrap();
    assert_eq!(audit.covered(), 3);
    assert!(!audit.is_short());

    let mut tx = h.begin().await.unwrap();
    // the key the INSERT will get: this transaction's timestamp, as PostgreSQL prints it
    let key: String = tx
        .query("SELECT (now() AT TIME ZONE 'UTC')::text", &[])
        .await
        .unwrap()[0]
        .get(0);
    let events = tx.env().records("scale.event").unwrap();
    let error = events
        .create_values(&mut tx, &[vals(&[("name", Value::str("late"))])])
        .await
        .unwrap_err();
    let partition = h
        .registry()
        .model("scale.event")
        .unwrap()
        .partition
        .unwrap();
    assert_error(
        &error,
        ErrorKind::Database,
        &format!(
            "no partition of 'scale.event' accepts create_date = {key}: partition upkeep has not \
             created it yet (window: {}; see the health check)",
            partition.interval().count(partition.premake())
        ),
    );
    assert_eq!(error.sqlstate(), Some("23514"));
    drop(tx);
    // a CHECK constraint of the same table keeps its own mapping (Validation, the declared name)
    exec(
        &h,
        "ALTER TABLE scale_event ADD CONSTRAINT scale_event_named CHECK (name <> 'bad')",
    )
    .await;
    let mut tx = h.begin().await.unwrap();
    tx.query(
        "INSERT INTO scale_event (name, create_date) VALUES ('ok', (now() AT TIME ZONE 'UTC') - \
         interval '1 day')",
        &[],
    )
    .await
    .unwrap();
    let error = tx
        .query(
            "INSERT INTO scale_event (name, create_date) VALUES ('bad', (now() AT TIME ZONE \
             'UTC') - interval '1 day')",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Run the schema sync of the harness's ORM, capturing its statements.
async fn sync(orm: &Orm) -> erp_fields::Result<Vec<String>> {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await?;
    tx.capture_statements(true);
    let result = tx.init_models(&[]).await;
    let captured = tx.captured().to_vec();
    match result {
        Ok(()) => {
            tx.commit().await?;
            Ok(captured)
        }
        Err(e) => {
            tx.rollback().await?;
            Err(e)
        }
    }
}

/// The hand-written partitioned `scale_event` ('s convention in the design note: range on
/// `create_date`, primary key `(id, create_date)`, `<table>_p<YYYYMMDD>` partitions), with the
/// model's columns and foreign keys, one partition named `partition` for `[from, to)` and one row.
fn hand_written(partition: &str, from: DateTime, to: DateTime) -> String {
    format!(
        "CREATE TABLE scale_event (id int8 GENERATED BY DEFAULT AS IDENTITY, name varchar, \
         create_uid int8, create_date timestamp NOT NULL, write_uid int8, write_date timestamp, \
         PRIMARY KEY (id, create_date)) PARTITION BY RANGE (create_date); \
         ALTER TABLE scale_event ADD CONSTRAINT scale_event_create_uid_fkey FOREIGN KEY \
         (create_uid) REFERENCES res_users(id) ON DELETE SET NULL; \
         ALTER TABLE scale_event ADD CONSTRAINT scale_event_write_uid_fkey FOREIGN KEY \
         (write_uid) REFERENCES res_users(id) ON DELETE SET NULL; \
         CREATE TABLE \"{partition}\" PARTITION OF scale_event FOR VALUES FROM ('{from}') TO \
         ('{to}'); \
         INSERT INTO scale_event (name, create_date) VALUES ('kept', '{from}')"
    )
}

fn has_rows_refusal(table: &str) -> String {
    format!(
        "table '{table}' has rows; changing its partitioning is not supported ({PARTITION_ADR}; \
         business tables: M5 partitioning ADR)"
    )
}

/// A matching hand-written partitioned table is adopted without DDL on it (only the missing
/// partitions are created); a mismatching one with rows is refused listing the difference; a
/// regular table with rows is refused, an empty one is created again partitioned in the install
/// transaction; removing the partitioning of a partitioned table with rows is refused likewise.
#[tokio::test]
async fn spec_ormx_029_adopt_matching_tables_refuse_changes_on_tables_with_rows() {
    let h = harness().await;
    let now = db_now(&h).await;
    let day = |n: i64| day_partition("scale_event", now, n);

    // 1. the hand-written table is adopted
    let (today, from, to) = day(0);
    exec(&h, "DROP TABLE scale_event").await;
    exec(&h, &hand_written(&today, from, to)).await;
    let statements = sync(h.orm()).await.unwrap();
    let on_the_table: Vec<&String> = statements
        .iter()
        .filter(|s| {
            s.split(";\n").any(|part| {
                let part = part.trim_start();
                part.starts_with("CREATE TABLE \"scale_event\" ")
                    || part.starts_with("ALTER TABLE \"scale_event\" ")
                    || part.starts_with("DROP TABLE \"scale_event\"")
            })
        })
        .collect();
    assert_eq!(on_the_table, Vec::<&String>::new(), "{statements:#?}");
    assert_eq!(
        attached(&h, "scale_event").await,
        vec![today.clone(), day(1).0, day(2).0]
    );
    let kept: i64 = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one("SELECT count(*) FROM scale_event WHERE name = 'kept'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, 1, "adopted without a rewrite");

    // 2. a month-named partition under a day interval, with rows: refused, listing the difference
    let month = partition_name("scale_event", PartitionInterval::Month, from).unwrap();
    exec(&h, "DROP TABLE scale_event").await;
    exec(&h, &hand_written(&month, from, to)).await;
    let error = sync(h.orm()).await.unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &format!(
            "table 'scale_event' has rows and does not match the partitioning declared for \
             'scale.event': partition '{month}' does not match interval day; changing its \
             partitioning is not supported ({PARTITION_ADR}; business tables: M5 partitioning \
             ADR)"
        ),
    );

    // 3. a regular table with rows: refused
    exec(&h, "DROP TABLE scale_event").await;
    exec(
        &h,
        "CREATE TABLE scale_event (id int8 GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name \
         varchar); INSERT INTO scale_event (name) VALUES ('row')",
    )
    .await;
    let error = sync(h.orm()).await.unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &has_rows_refusal("scale_event"),
    );

    // 4. an empty regular table: dropped and created again, partitioned, in the same transaction
    exec(&h, "DROP TABLE scale_event").await;
    exec(
        &h,
        "CREATE TABLE scale_event (id int8 GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name \
         varchar)",
    )
    .await;
    let statements = sync(h.orm()).await.unwrap();
    let ddl = statements.join("\n");
    let dropped = ddl.find("DROP TABLE \"scale_event\"").expect(&ddl);
    let created = ddl
        .find("CREATE TABLE \"scale_event\" (")
        .expect("created again");
    assert!(dropped < created, "{ddl}");
    let kind: String = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT relkind::text FROM pg_class WHERE oid = 'scale_event'::regclass",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(kind, "p");
    assert_eq!(
        attached(&h, "scale_event").await,
        vec![today.clone(), day(1).0, day(2).0]
    );

    // 5. the declaration without partitioning, on the partitioned table with rows: refused
    exec(
        &h,
        &format!("INSERT INTO scale_event (name, create_date) VALUES ('row', '{from}')"),
    )
    .await;
    let plain = plain_event_orm(&h);
    let error = plain.sync_schema().await.unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &has_rows_refusal("scale_event"),
    );
    // ... and on an empty partitioned table (never written since TRUNCATE): created again, regular
    exec(&h, "TRUNCATE scale_event").await;
    assert!(plain.sync_schema().await.unwrap().is_empty());
    let kind: String = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT relkind::text FROM pg_class WHERE oid = 'scale_event'::regclass",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(kind, "r");
    h.cleanup().await.unwrap();
}

/// "Empty" means never written: the heap size decides, not the rows this transaction can see, so
/// rows deleted but not vacuumed (or committed by a transaction newer than the sync's snapshot)
/// keep a table from being dropped, regular or partitioned.
#[tokio::test]
async fn a_table_with_invisible_rows_is_not_recreated() {
    let h = harness().await;
    let refusal = format!(
        "table 'scale_event' has rows (none visible to this transaction: deleted rows not yet \
         vacuumed, or rows another transaction committed meanwhile); changing its partitioning \
         is not supported ({PARTITION_ADR}; business tables: M5 partitioning ADR)"
    );
    // a regular table where partitioning is declared
    exec(&h, "DROP TABLE scale_event").await;
    exec(
        &h,
        "CREATE TABLE scale_event (id int8 GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name \
         varchar); INSERT INTO scale_event (name) VALUES ('gone'); DELETE FROM scale_event",
    )
    .await;
    let error = sync(h.orm()).await.unwrap_err();
    assert_error(&error, ErrorKind::Programming, &refusal);
    // a partitioned table where it is not
    exec(&h, "DROP TABLE scale_event").await;
    sync(h.orm()).await.unwrap();
    exec(
        &h,
        "INSERT INTO scale_event (name, create_date) VALUES ('gone', now() AT TIME ZONE 'UTC'); \
         DELETE FROM scale_event",
    )
    .await;
    let error = plain_event_orm(&h).sync_schema().await.unwrap_err();
    assert_error(&error, ErrorKind::Programming, &refusal);
    h.cleanup().await.unwrap();
}

/// `maintain_partitions` goes on past a model whose upkeep cannot start, listing it as failed.
#[tokio::test]
async fn maintain_lists_a_model_that_cannot_start_and_goes_on() {
    let h = harness().await;
    let now = db_now(&h).await;
    let holder = h.db().client().await.unwrap();
    holder
        .query(erp_sql::partition::SESSION_LOCK, &[&"scale_event"])
        .await
        .unwrap();
    let hasty = orm_on(
        Arc::clone(h.registry()),
        pool(&h, 1),
        OrmConfig {
            partition_lock_timeout: Duration::from_millis(200),
            ..OrmConfig::default()
        },
    );
    let report = hasty.maintain_partitions(h.db().pool(), now).await.unwrap();
    let got: Vec<(&str, &str, String)> = report
        .entries()
        .iter()
        .map(|e| (e.model(), e.partition(), e.action().to_string()))
        .collect();
    let month = |n: i64| month_partition("scale_audit", now, n);
    assert_eq!(
        got,
        vec![
            (
                "scale.event",
                "scale_event",
                "failed to prepare: partition upkeep of 'scale.event' could not take the upkeep \
                 lock of table 'scale_event' (another upkeep or a schema synchronization holds \
                 it): canceling statement due to lock timeout (SQLSTATE 55P03)"
                    .to_owned()
            ),
            ("scale.audit", month(0).as_str(), "existed".to_owned()),
            ("scale.audit", month(1).as_str(), "existed".to_owned()),
            ("scale.audit", month(2).as_str(), "existed".to_owned()),
        ]
    );
    assert!(!report.is_complete());
    // the model that could not start is a structured failure, and no unlock was attempted for a
    // lock that was never taken (it would have added an `unlock` entry)
    let prepare: Vec<_> = report.failures().collect();
    assert_eq!(prepare.len(), 1, "{report:?}");
    assert_eq!(prepare[0].step(), Some(PartitionStep::Prepare));
    assert_eq!(prepare[0].sqlstate(), Some("55P03"));
    assert_eq!(prepare[0].error_kind(), Some(ErrorKind::Lock));
    assert!(prepare[0].is_lock_timeout());
    holder
        .query(erp_sql::partition::SESSION_UNLOCK, &[&"scale_event"])
        .await
        .unwrap();
    drop(holder);
    h.cleanup().await.unwrap();
}

/// Leave `name`, a partition of `scale_event`, the way an interrupted retirement leaves it: marked
/// and pending detach (an open snapshot keeps the concurrent detach waiting past its lock_timeout).
async fn interrupt_retirement(h: &Harness, name: &str) {
    let reader = h.db().client().await.unwrap();
    reader
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM scale_event")
        .await
        .unwrap();
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&erp_sql::partition::mark_retired("scale_event", name))
        .await
        .unwrap();
    client
        .batch_execute("SET lock_timeout = '300ms'")
        .await
        .unwrap();
    let error = client
        .batch_execute(&erp_sql::partition::detach_concurrently(
            "scale_event",
            name,
        ))
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        Some(&erp_testkit::tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE),
        "{error:?}"
    );
    client.batch_execute("RESET lock_timeout").await.unwrap();
    reader.batch_execute("COMMIT").await.unwrap();
}

/// `maintain_partitions` finishes a pending detach once, then drops the table it leaves (marked
/// when its retirement started), with every other action of both models listed after it.
#[tokio::test]
async fn maintain_finalizes_once_and_drops_the_leftover() {
    let h = harness().await;
    let now = db_now(&h).await;
    let stale = old_partition(&h, now, -8).await;
    interrupt_retirement(&h, &stale).await;
    let report = h
        .orm()
        .maintain_partitions(h.db().pool(), now)
        .await
        .unwrap();
    let got: Vec<(&str, &str, String)> = report
        .entries()
        .iter()
        .map(|e| (e.model(), e.partition(), e.action().to_string()))
        .collect();
    let day = |n: i64| day_partition("scale_event", now, n).0;
    let month = |n: i64| month_partition("scale_audit", now, n);
    let expected: Vec<(&str, String, &str)> = vec![
        ("scale.event", stale.clone(), "finalized"),
        ("scale.event", day(0), "existed"),
        ("scale.event", day(1), "existed"),
        ("scale.event", day(2), "existed"),
        ("scale.event", stale.clone(), "dropped"),
        ("scale.audit", month(0), "existed"),
        ("scale.audit", month(1), "existed"),
        ("scale.audit", month(2), "existed"),
    ];
    let expected: Vec<(&str, &str, String)> = expected
        .iter()
        .map(|(m, p, a)| (*m, p.as_str(), (*a).to_owned()))
        .collect();
    assert_eq!(got, expected);
    assert!(report.is_complete());
    let left: i64 = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*) FROM pg_class WHERE relname = $1::text",
            &[&stale],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 0, "the leftover table is gone");
    h.cleanup().await.unwrap();
}

static PLAIN: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "plain_event",
    depends: &["base"],
    exact_only: false,
};

/// An ORM on the harness database whose `scale.event` is not partitioned.
fn plain_event_orm(h: &Harness) -> Orm {
    let decl: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("plain_event", "scale.event")
            .description("Scale Event")
            .fields(const { &[FieldDecl::char("name")] }),
    ));
    let registry = RegistryBuilder::new([&PLAIN])
        .linked(false)
        .with_decls([decl])
        .build()
        .unwrap();
    orm_on(Arc::new(registry), pool(h, 2), OrmConfig::default())
}

#[derive(Default)]
struct Hook {
    calls: AtomicUsize,
}

struct Counting(Arc<Hook>);

impl Refresh for Counting {
    fn refresh<'a>(
        &'a self,
        _changes: &'a Changes,
    ) -> BoxFuture<'a, erp_fields::Result<Refreshed>> {
        Box::pin(async move {
            self.0.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Refreshed::InPlace)
        })
    }
}

/// An ORM with signals on the harness database (its own pool of `size`), applied baseline = what
/// the generation table holds now.
async fn signal_orm(h: &Harness, hook: &Arc<Hook>, size: usize) -> Orm {
    let client = h.db().client().await.unwrap();
    let baseline = Signal::read(&client).await.unwrap();
    Orm::builder(Arc::clone(h.registry()), pool(h, size))
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(baseline).with_refresh(Counting(Arc::clone(hook))))
        .build()
        .unwrap()
}

async fn generation_rows(h: &Harness) -> Vec<(String, i64)> {
    let client = h.db().client().await.unwrap();
    client
        .query(
            "SELECT key, count(*) FROM erp_signal_generation GROUP BY key ORDER BY key",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

/// installing process; the upkeep adds no generation row and a second ORM's prepared statement
/// survives an attach and a detach unprepared; a no-op sync adds nothing. Keys, exclusions, BRIN
/// indexes and row_version join the same install when's sub-slices merge)
#[tokio::test]
async fn spec_ormx_030_schema_ddl_bumps_registry_once_upkeep_bumps_nothing() {
    let h = Harness::builder([&base_users::ADDON, &test_partition::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    finish(tx).await;
    let (hook_a, hook_b) = (Arc::new(Hook::default()), Arc::new(Hook::default()));
    let a = signal_orm(&h, &hook_a, 2).await;
    let b = signal_orm(&h, &hook_b, 1).await;

    assert!(a.sync_schema().await.unwrap().is_empty());
    assert_eq!(generation_rows(&h).await, vec![("registry".to_owned(), 1)]);
    // the superuser row the harness inserts after its own sync (audit foreign keys)
    exec(
        &h,
        "INSERT INTO res_users (id, name, login) VALUES (1, '__system__', '__system__')",
    )
    .await;
    let mut tx = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
    finish(tx).await;
    assert_eq!(hook_a.calls.load(Ordering::SeqCst), 0, "its own bump");

    // the second ORM applies the install, then prepares a read of scale_event
    let read = "SELECT id, name FROM scale_event WHERE id = ANY($1) ORDER BY id";
    let prepared =
        "SELECT name, prepare_time::text FROM pg_prepared_statements WHERE statement = $1";
    let mut tx = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let ids: Vec<i64> = tx
        .env()
        .records("scale.event")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("name", Value::str("one"))])])
        .await
        .unwrap()
        .ids();
    assert_eq!(tx.query(read, &[&ids]).await.unwrap().len(), 1);
    let before: Vec<(String, String)> = tx
        .query(prepared, &[&read])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    finish(tx).await;
    assert_eq!(hook_b.calls.load(Ordering::SeqCst), 1);
    assert_eq!(before.len(), 1);

    // upkeep: an attach and a detach
    let now = db_now(&h).await;
    old_partition(&h, now, -10).await;
    let owner = h.db().pool().clone();
    let horizon = PartitionInterval::Day
        .shift(PartitionInterval::Day.start_of(now).unwrap(), 3)
        .unwrap();
    let ensured = a
        .ensure_partitions(&owner, "scale.event", Horizon::Through(horizon))
        .await
        .unwrap();
    assert_eq!(ensured.created(), [day_partition("scale_event", now, 3).0]);
    let retired = a
        .retire_partitions(&owner, "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    assert_eq!(
        retired.dropped(),
        [day_partition("scale_event", now, -10).0]
    );
    assert_eq!(generation_rows(&h).await, vec![("registry".to_owned(), 1)]);

    let mut tx = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let rows = tx.query(read, &[&ids]).await.unwrap();
    assert_eq!(rows.len(), 1, "the statement still works");
    let after: Vec<(String, String)> = tx
        .query(prepared, &[&read])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    finish(tx).await;
    assert_eq!(
        after, before,
        "the same prepared statement, not prepared again"
    );
    assert_eq!(hook_b.calls.load(Ordering::SeqCst), 1, "nothing to apply");

    // a sync that changes nothing bumps nothing
    assert!(a.sync_schema().await.unwrap().is_empty());
    assert_eq!(generation_rows(&h).await, vec![("registry".to_owned(), 1)]);
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// review round 2: MUST 3, LOW a, b, c

/// the drop gives up after `PARTITION_SHORT_LOCK_TIMEOUT` and is reported `drop_deferred` with the
/// table still detached and marked, and the next retirement completes it)
#[tokio::test]
async fn a_retired_partition_drop_gives_way_to_readers_of_the_tables_it_references() {
    let h = harness().await;
    let now = db_now(&h).await;
    let stale = old_partition(&h, now, -7).await;
    // a long reader of res_users: the drop of the detached table needs ACCESS EXCLUSIVE there
    let holder = h.db().client().await.unwrap();
    holder
        .batch_execute("BEGIN; SELECT count(*) FROM res_users")
        .await
        .unwrap();
    // a new reader queues behind the waiting drop, for as long as the drop waits
    let (orm, owner) = (h.orm().clone(), pool(&h, 2));
    let (waited, report) = queue_behind_the_res_users_waiter(
        &h,
        async move {
            orm.retire_partitions(&owner, "scale.event", KeepAfter::Declared)
                .await
        },
        "PERFORM count(*) FROM res_users",
        "the drop never queued behind the reader",
    )
    .await;
    assert!(
        waited < OrmConfig::default().partition_lock_timeout / 2,
        "a reader of res_users waited {waited:?} behind the drop of {stale}"
    );
    let report = report.unwrap();
    assert_eq!(report.detached(), [stale.as_str()]);
    assert_eq!(report.dropped(), Vec::<&str>::new());
    assert_eq!(report.drops_deferred(), [stale.as_str()]);
    assert!(report.is_complete(), "{report:?}");
    let entry = report
        .entries()
        .iter()
        .find(|e| matches!(e.action(), PartitionAction::DropDeferred { .. }))
        .and_then(|e| e.reason())
        .unwrap();
    assert_eq!(
        entry,
        "canceling statement due to lock timeout (SQLSTATE 55P03)"
    );
    // left detached and marked, with its rows
    let client = h.db().client().await.unwrap();
    let left = client
        .query_one(
            "SELECT obj_description($1::text::regclass, 'pg_class'), \
                    (SELECT count(*) FROM pg_inherits WHERE inhrelid = $1::text::regclass)",
            &[&stale],
        )
        .await
        .unwrap();
    assert_eq!(
        left.get::<_, Option<String>>(0),
        Some(erp_sql::partition::retired_marker("scale_event"))
    );
    assert_eq!(left.get::<_, i64>(1), 0);
    // the holder lets go; the next retirement completes the drop
    holder.batch_execute("COMMIT").await.unwrap();
    let next = h
        .orm()
        .retire_partitions(&pool(&h, 2), "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    assert_eq!(next.dropped(), [stale.as_str()]);
    assert!(
        next.drops_deferred().is_empty() && next.is_complete(),
        "{next:?}"
    );
    let gone: Option<String> = client
        .query_one("SELECT to_regclass($1)::text", &[&stale])
        .await
        .unwrap()
        .get(0);
    assert_eq!(gone, None);
    h.cleanup().await.unwrap();
}

/// every retirement instead of being skipped silently)
#[tokio::test]
async fn retire_lists_partitions_with_bounds_it_cannot_read() {
    let h = harness().await;
    exec(
        &h,
        "CREATE TABLE scale_event_pbottom (LIKE scale_event INCLUDING DEFAULTS INCLUDING \
         CONSTRAINTS); ALTER TABLE scale_event ATTACH PARTITION scale_event_pbottom FOR VALUES \
         FROM (MINVALUE) TO ('2020-01-01 00:00:00')",
    )
    .await;
    let report = h
        .orm()
        .retire_partitions(h.db().pool(), "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].partition(), "scale_event_pbottom");
    assert_failed(
        failures[0],
        PartitionStep::Detach,
        "the partition's bounds (FOR VALUES FROM (MINVALUE) TO ('2020-01-01 00:00:00')) are not \
         a range of timestamps, so the upkeep cannot tell whether it is old enough to retire; \
         detach it by hand",
        None,
    );
    assert!(report.detached().is_empty() && report.dropped().is_empty());
    assert!(
        attached(&h, "scale_event")
            .await
            .contains(&"scale_event_pbottom".to_owned())
    );
    h.cleanup().await.unwrap();
}

/// `inhdetachpending` of partition `name`, `None` when it is not a partition.
async fn detach_pending(h: &Harness, name: &str) -> Option<bool> {
    h.db()
        .client()
        .await
        .unwrap()
        .query_opt(
            "SELECT inhdetachpending FROM pg_inherits WHERE inhrelid = $1::text::regclass",
            &[&name],
        )
        .await
        .unwrap()
        .map(|r| r.get::<_, bool>(0))
}

/// counting the partition as absent; the finished table is standalone afterwards)
#[tokio::test]
async fn schema_sync_finishes_a_pending_detach() {
    let h = harness().await;
    let now = db_now(&h).await;
    let stale = old_partition(&h, now, -7).await;
    // an interrupted DETACH … CONCURRENTLY: an open snapshot keeps it waiting past its lock timeout
    let reader = h.db().client().await.unwrap();
    reader
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM scale_event")
        .await
        .unwrap();
    let detacher = h.db().client().await.unwrap();
    detacher
        .batch_execute("SET lock_timeout = '300ms'")
        .await
        .unwrap();
    let error = detacher
        .batch_execute(&format!(
            "ALTER TABLE scale_event DETACH PARTITION \"{stale}\" CONCURRENTLY"
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code().map(|c| c.code()), Some("55P03"));
    reader.batch_execute("COMMIT").await.unwrap();
    assert_eq!(detach_pending(&h, &stale).await, Some(true));
    assert!(h.orm().sync_schema().await.unwrap().is_empty());
    assert_eq!(
        detach_pending(&h, &stale).await,
        None,
        "finished: no longer a partition"
    );
    let standalone: Option<String> = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one("SELECT to_regclass($1)::text", &[&stale])
        .await
        .unwrap()
        .get(0);
    assert_eq!(standalone.as_deref(), Some(stale.as_str()));
    h.cleanup().await.unwrap();
}

/// clock, the caller's `now`; a `now` 2 days behind the database creates the 2 missing partitions
/// of its own window, which the database's clock would not have)
#[tokio::test]
async fn maintain_places_the_window_by_the_callers_clock() {
    let h = harness().await;
    let db_now = db_now(&h).await;
    let behind = PartitionInterval::Day.shift(db_now, -2).unwrap();
    let day = |n: i64| day_partition("scale_event", behind, n).0;
    let report = h
        .orm()
        .maintain_partitions(h.db().pool(), behind)
        .await
        .unwrap();
    let events: Vec<(&str, &str, String)> = report
        .entries()
        .iter()
        .filter(|e| e.model() == "scale.event")
        .map(|e| (e.model(), e.partition(), e.action().to_string()))
        .collect();
    let expected = [
        (day(0), "created"),
        (day(1), "created"),
        (day(2), "existed"),
    ];
    let expected: Vec<(&str, &str, String)> = expected
        .iter()
        .map(|(p, a)| ("scale.event", p.as_str(), (*a).to_owned()))
        .collect();
    assert_eq!(events, expected);
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// review round 3

/// The `Programming` error of a horizon that is before the interval holding the current time, and
/// the limits of one call at their boundaries (messages built from the constants).
///
/// `MAX_PARTITIONS_PER_CALL` partitions are `Programming` errors before anything runs)
#[tokio::test]
async fn ensure_refuses_a_horizon_in_the_past_and_a_window_over_the_limit() {
    let h = harness().await;
    let owner = h.db().pool().clone();
    let now = db_now(&h).await;
    let day = PartitionInterval::Day;
    let current = day.start_of(now).unwrap();
    let before = day.shift(current, -1).unwrap();
    let error = h
        .orm()
        .ensure_partitions(&owner, "scale.event", Horizon::Through(before))
        .await
        .unwrap_err();
    // the database's clock places the window; the message names the interval it starts
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert!(
        error.message().starts_with(&format!(
            "ensure_partitions on 'scale.event': the horizon {before} lies before the day holding \
             the current time "
        )) && error
            .message()
            .ends_with("a horizon is at or after the current interval"),
        "{}",
        error.message()
    );
    // a horizon inside the current interval is the window of one partition: fine
    let report = h
        .orm()
        .ensure_partitions(&owner, "scale.event", Horizon::Through(current))
        .await
        .unwrap();
    assert!(report.is_complete(), "{report:?}");
    // one past the limit: current + MAX_PARTITIONS_PER_CALL intervals holds MAX + 1 partitions
    let max = i64::try_from(erp_orm::MAX_PARTITIONS_PER_CALL).unwrap();
    let too_far = day.shift(current, max).unwrap();
    let error = h
        .orm()
        .ensure_partitions(&owner, "scale.event", Horizon::Through(too_far))
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &format!(
            "the partition window of 'scale.event' through {too_far} holds more than {} \
             partitions; create them in several calls",
            erp_orm::MAX_PARTITIONS_PER_CALL
        ),
    );
    h.cleanup().await.unwrap();
}

/// The error the watching block raises when no backend queued within its 30 s deadline.
const NO_WAITER: &str = "no backend queued for a lock on res_users within 30 s";

/// Runs `op` while another session waits to run `stmt` (one PL/pgSQL statement on `res_users`)
/// as soon as a backend of this database waits for a lock on `res_users`, and returns how long
/// `stmt` took, and `op`'s result. The upkeep waits there for at most
/// `PARTITION_SHORT_LOCK_TIMEOUT`, so the check for the waiter and `stmt` run in one server-side
/// block: no client round trip or test scheduling sits between seeing the waiter and queuing
/// behind it, and the block is running before `op` starts. That narrows the window to the
/// watching backend's own scheduling (1 ms passes); it does not remove it: a backend stalled for
/// the whole short wait still misses it, and the test then fails loudly with `never` after 30 s.
/// The block commits on every pass, so it holds no snapshot an upkeep step waits out
/// (`DETACH … CONCURRENTLY` and `FINALIZE` wait for older snapshots, and would otherwise wait for
/// the watcher instead of `res_users`). Any ungranted `res_users` lock in the test database
/// counts as the waiter: only the holder, the watcher and `op`'s sessions use it, and the callers
/// assert from `op`'s report that its statement was cut by the lock timeout. `stmt` is a literal
/// of the test.
async fn queue_behind_the_res_users_waiter<T: Send + 'static>(
    h: &Harness,
    op: impl std::future::Future<Output = T> + Send + 'static,
    stmt: &str,
    never: &str,
) -> (Duration, T) {
    let watcher = h.db().client().await.unwrap();
    let watcher_pid: i32 = watcher
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let block = format!(
        "DO $erp_test$ \
         DECLARE deadline timestamptz := clock_timestamp() + interval '30 seconds'; \
                 started timestamptz; \
         BEGIN \
           LOOP \
             EXIT WHEN EXISTS (SELECT FROM pg_locks \
               WHERE locktype = 'relation' \
                 AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                 AND relation = 'res_users'::regclass \
                 AND pid <> pg_backend_pid() \
                 AND NOT granted); \
             IF clock_timestamp() > deadline THEN \
               RAISE EXCEPTION '{NO_WAITER}'; \
             END IF; \
             COMMIT; \
             PERFORM pg_sleep(0.001); \
           END LOOP; \
           started := clock_timestamp(); \
           {stmt}; \
           PERFORM set_config('erp_test.queued_us', \
             floor(extract(epoch FROM clock_timestamp() - started) * 1000000)::bigint::text, \
             false); \
         END $erp_test$"
    );
    let watch = tokio::spawn(async move {
        watcher.batch_execute(&block).await?;
        let micros: i64 = watcher
            .query_one("SELECT current_setting('erp_test.queued_us')::bigint", &[])
            .await?
            .get(0);
        Ok::<_, erp_testkit::tokio_postgres::Error>(micros)
    });
    // the block runs before `op` starts (not a timed window: `op` waits for this)
    let probe = h.db().client().await.unwrap();
    let running = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let state: Option<String> = probe
                .query_one(
                    "SELECT state FROM pg_stat_activity WHERE pid = $1",
                    &[&watcher_pid],
                )
                .await
                .unwrap()
                .get(0);
            if state.as_deref() == Some("active") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(running.is_ok(), "the watching block never started");
    let task = tokio::spawn(op);
    let micros = match watch.await.unwrap() {
        Ok(micros) => micros,
        Err(error)
            if error
                .as_db_error()
                .is_some_and(|e| e.message() == NO_WAITER) =>
        {
            panic!("{never}: {NO_WAITER}")
        }
        Err(error) => panic!("the watching block failed: {error:?}"),
    };
    let out = task.await.unwrap();
    (Duration::from_micros(u64::try_from(micros).unwrap()), out)
}

/// A long transaction that has written to `res_users` (a login, a profile change) holds
/// `ROW EXCLUSIVE` on it; `op` is run meanwhile, and a second writer of `res_users` starts as soon
/// as `op` queues for its lock. Returns how long the second writer waited, and `op`'s result.
async fn second_writer_wait<T: Send + 'static>(
    h: &Harness,
    op: impl std::future::Future<Output = T> + Send + 'static,
) -> (Duration, T) {
    let holder = h.db().client().await.unwrap();
    holder
        .batch_execute("BEGIN; UPDATE res_users SET id = id WHERE false")
        .await
        .unwrap();
    let out = queue_behind_the_res_users_waiter(
        h,
        op,
        "UPDATE res_users SET id = id WHERE false",
        "the upkeep never queued behind the writer",
    )
    .await;
    holder.batch_execute("COMMIT").await.unwrap();
    out
}

/// What the writers of the tables a partition's foreign keys point to may wait behind an upkeep
/// statement: about the short lock timeout, never the configured one.
fn writer_wait_bound() -> Duration {
    OrmConfig::default().partition_lock_timeout / 2
}

/// stall the other writers of `res_users` for the configured lock timeout; the detach gives up
/// after `PARTITION_SHORT_LOCK_TIMEOUT` as a failed entry with SQLSTATE 55P03 that leaves the
/// partition attached (marker removed) or pending, and the next retirement completes it)
#[tokio::test]
async fn retiring_a_partition_does_not_stall_the_writers_of_referenced_tables() {
    let h = harness().await;
    let now = db_now(&h).await;
    let stale = old_partition(&h, now, -7).await;
    let (orm, owner) = (h.orm().clone(), pool(&h, 2));
    let (waited, report) = second_writer_wait(&h, async move {
        orm.retire_partitions(&owner, "scale.event", KeepAfter::Declared)
            .await
    })
    .await;
    eprintln!("MEASURED retire: a second res_users writer waited {waited:?}");
    assert!(
        waited < writer_wait_bound(),
        "a writer of res_users waited {waited:?} behind the detach of {stale}"
    );
    let report = report.unwrap();
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].partition(), stale);
    assert_eq!(failures[0].step(), Some(PartitionStep::Detach));
    assert!(failures[0].is_lock_timeout(), "{report:?}");
    assert_eq!(failures[0].error_kind(), Some(ErrorKind::Lock));
    assert!(report.detached().is_empty() && report.dropped().is_empty());
    let pending = detach_pending(&h, &stale).await;
    eprintln!("MEASURED retire: the cut detach left the partition (pending: {pending:?})");
    assert!(pending.is_some(), "attached or pending, never lost");
    // the writer committed: the next retirement completes whatever state the cut left
    let next = h
        .orm()
        .retire_partitions(&pool(&h, 2), "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    assert!(next.is_complete(), "{next:?}");
    assert_eq!(next.dropped(), [stale.as_str()]);
    h.cleanup().await.unwrap();
}

/// the foreign keys point to, gives up after the short lock timeout while a writer of `res_users`
/// is open, without stalling other writers; the cut attach leaves no table behind and the next
/// call creates the partition)
#[tokio::test]
async fn creating_a_partition_does_not_stall_the_writers_of_referenced_tables() {
    let h = harness().await;
    let now = db_now(&h).await;
    let (missing, _, _) = day_partition("scale_event", now, 2);
    drop_partition(&h, "scale_event", &missing).await;
    let (orm, owner) = (h.orm().clone(), pool(&h, 2));
    let (waited, report) = second_writer_wait(&h, async move {
        orm.ensure_partitions(&owner, "scale.event", Horizon::Declared)
            .await
    })
    .await;
    eprintln!("MEASURED attach: a second res_users writer waited {waited:?}");
    assert!(
        waited < writer_wait_bound(),
        "a writer of res_users waited {waited:?} behind the attach of {missing}"
    );
    let report = report.unwrap();
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].partition(), missing);
    assert_eq!(failures[0].step(), Some(PartitionStep::Create));
    assert!(failures[0].is_lock_timeout(), "{report:?}");
    let leftover: Option<String> = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one("SELECT to_regclass($1)::text", &[&missing])
        .await
        .unwrap()
        .get(0);
    assert_eq!(leftover, None, "the cut create left no table");
    let next = h
        .orm()
        .ensure_partitions(&pool(&h, 2), "scale.event", Horizon::Declared)
        .await
        .unwrap();
    assert_eq!(next.created(), [missing.as_str()]);
    assert!(next.is_complete(), "{next:?}");
    h.cleanup().await.unwrap();
}

/// a writer of `res_users` is open, without stalling other writers; the partition stays pending
/// and the window entry that needs its name says the finalize did not complete, and says
/// `finalized now` once it has)
#[tokio::test]
async fn finalizing_a_detach_does_not_stall_the_writers_of_referenced_tables() {
    let h = harness().await;
    let now = db_now(&h).await;
    let (name, _, _) = day_partition("scale_event", now, 1);
    interrupt_retirement(&h, &name).await;
    assert_eq!(detach_pending(&h, &name).await, Some(true));
    let (orm, owner) = (h.orm().clone(), pool(&h, 2));
    let (waited, report) = second_writer_wait(&h, async move {
        orm.ensure_partitions(&owner, "scale.event", Horizon::Declared)
            .await
    })
    .await;
    eprintln!("MEASURED finalize: a second res_users writer waited {waited:?}");
    assert!(
        waited < writer_wait_bound(),
        "a writer of res_users waited {waited:?} behind the finalize of {name}"
    );
    let report = report.unwrap();
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 2, "{report:?}");
    assert_eq!(failures[0].step(), Some(PartitionStep::Finalize));
    assert!(failures[0].is_lock_timeout(), "{report:?}");
    assert_eq!(failures[1].step(), Some(PartitionStep::Create));
    assert_eq!(
        failures[1].reason(),
        Some(
            "the partition is being detached and its finalize did not complete (see its entry); \
             once it has, a standalone table of that name is left and must be removed or \
             attached by hand"
        )
    );
    assert_eq!(detach_pending(&h, &name).await, Some(true));
    // nobody holds anything: the finalize completes
    let next = h
        .orm()
        .ensure_partitions(&pool(&h, 2), "scale.event", Horizon::Declared)
        .await
        .unwrap();
    assert_eq!(next.finalized(), [name.as_str()]);
    let failed: Vec<_> = next.failures().collect();
    assert_eq!(failed.len(), 1, "{next:?}");
    assert_eq!(
        failed[0].reason(),
        Some(
            "the partition was being detached (finalized now); a standalone table of that name \
             is left and must be removed or attached by hand"
        )
    );
    h.cleanup().await.unwrap();
}

/// partitions of its own table: a partition of another partitioned table whose name starts with
/// the same prefix and that carries the same comment is left as it is)
#[tokio::test]
async fn the_marker_of_another_tables_partition_is_left_alone() {
    let h = harness().await;
    let marker = erp_sql::partition::retired_marker("scale_event");
    exec(
        &h,
        &format!(
            "CREATE TABLE scale_event_plus (id int8, create_date timestamp) \
                 PARTITION BY RANGE (create_date); \
             CREATE TABLE scale_event_plus_p1 PARTITION OF scale_event_plus \
                 FOR VALUES FROM ('2020-01-01') TO ('2020-01-02'); \
             COMMENT ON TABLE scale_event_plus_p1 IS '{marker}'"
        ),
    )
    .await;
    let report = h
        .orm()
        .retire_partitions(&pool(&h, 2), "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    assert!(report.failures().next().is_none(), "{report:?}");
    let comment: Option<String> = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT obj_description('scale_event_plus_p1'::regclass, 'pg_class')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(comment.as_deref(), Some(marker.as_str()));
    h.cleanup().await.unwrap();
}

/// `erp:retired` marker back, so a later manual detach never gets the table dropped by the next
/// retirement)
#[tokio::test]
async fn a_failed_detach_takes_the_retired_marker_back() {
    let h = harness().await;
    let now = db_now(&h).await;
    let stale = old_partition(&h, now, -7).await;
    // SHARE conflicts with the detach's SHARE UPDATE EXCLUSIVE on the parent: it waits, then is cut
    let blocker = h.db().client().await.unwrap();
    blocker
        .batch_execute("BEGIN; LOCK TABLE scale_event IN SHARE MODE")
        .await
        .unwrap();
    let report = h
        .orm()
        .retire_partitions(&pool(&h, 2), "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    blocker.batch_execute("COMMIT").await.unwrap();
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].step(), Some(PartitionStep::Detach));
    assert!(failures[0].is_lock_timeout());
    assert_eq!(
        detach_pending(&h, &stale).await,
        Some(false),
        "still attached"
    );
    let comment: Option<String> = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT obj_description($1::text::regclass, 'pg_class')",
            &[&stale],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(comment, None, "the retired marker is gone");
    // someone detaches it by hand; the next retirement must not drop it
    exec(
        &h,
        &format!("ALTER TABLE scale_event DETACH PARTITION \"{stale}\""),
    )
    .await;
    let next = h
        .orm()
        .retire_partitions(&pool(&h, 2), "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    assert!(next.dropped().is_empty(), "{next:?}");
    let kept: Option<String> = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one("SELECT to_regclass($1)::text", &[&stale])
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept.as_deref(), Some(stale.as_str()));
    h.cleanup().await.unwrap();
}

/// detach that is finished already, or a table that is gone, it does nothing, where a plain
/// `FINALIZE` fails; for a pending detach it finishes it)
#[tokio::test]
async fn the_conditional_finalize_is_a_no_op_once_the_detach_is_finished() {
    let h = harness().await;
    let now = db_now(&h).await;
    let stale = old_partition(&h, now, -7).await;
    interrupt_retirement(&h, &stale).await;
    assert_eq!(detach_pending(&h, &stale).await, Some(true));
    let client = h.db().client().await.unwrap();
    let still = |name: String| {
        let client = &client;
        async move {
            let row = client
                .query_one(
                    erp_sql::partition::STILL_PARTITION_QUERY,
                    &[&erp_sql::quote(&name)],
                )
                .await
                .unwrap();
            row.get::<_, bool>(0)
        }
    };
    assert!(still(stale.clone()).await, "pending is still a partition");
    let conditional = erp_sql::partition::detach_finalize_if_pending("scale_event", &stale);
    client.batch_execute(&conditional).await.unwrap();
    assert_eq!(detach_pending(&h, &stale).await, None, "finished");
    assert!(!still(stale.clone()).await);
    // finished already: nothing to do ...
    client.batch_execute(&conditional).await.unwrap();
    // ... where the plain statement fails
    let plain = erp_sql::partition::detach_finalize("scale_event", &stale);
    let error = client.batch_execute(&plain).await.unwrap_err();
    let db = error.as_db_error().expect("a database error");
    assert_eq!(db.code().code(), "42P01", "{db:?}");
    assert!(
        db.message().contains("is not a partition of relation"),
        "{db:?}"
    );
    // a table that is gone altogether
    exec(&h, &format!("DROP TABLE \"{stale}\"")).await;
    assert!(!still(stale.clone()).await);
    client.batch_execute(&conditional).await.unwrap();
    h.cleanup().await.unwrap();
}

/// A hand-made partition with an `infinity` bound: it passes `pg_input_is_valid` as a timestamp but
/// has no number of microseconds, so it must read as an unreadable bound, not break the catalog.
async fn add_infinity_partition(h: &Harness) -> String {
    exec(
        h,
        "CREATE TABLE scale_event_pinf PARTITION OF scale_event FOR VALUES FROM ('-infinity') TO \
         ('2000-01-01 00:00:00')",
    )
    .await;
    "scale_event_pinf".to_owned()
}

/// schema sync refuses the table with rows naming the partition, instead of failing wholesale)
#[tokio::test]
async fn an_infinity_bound_is_a_mismatch_for_schema_sync() {
    let h = harness().await;
    let now = db_now(&h).await;
    old_partition(&h, now, -2).await;
    let name = add_infinity_partition(&h).await;
    let error = sync(h.orm()).await.unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &format!(
            "table 'scale_event' has rows and does not match the partitioning declared for \
             'scale.event': partition '{name}' does not match interval day; changing its \
             partitioning is not supported ({PARTITION_ADR}; business tables: M5 partitioning \
             ADR)"
        ),
    );
    h.cleanup().await.unwrap();
}

/// and maintenance and counted by the health check as an unreadable bound, none of them failing
/// wholesale)
#[tokio::test]
async fn an_infinity_bound_is_listed_not_fatal_for_upkeep_and_health() {
    let h = harness().await;
    let now = db_now(&h).await;
    let name = add_infinity_partition(&h).await;
    let bound: String = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = $1::text",
            &[&name],
        )
        .await
        .unwrap()
        .get(0);
    let expected = format!(
        "the partition's bounds ({bound}) are not a range of timestamps, so the upkeep cannot \
         tell whether it is old enough to retire; detach it by hand"
    );
    let report = h
        .orm()
        .retire_partitions(h.db().pool(), "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].partition(), name);
    assert_eq!(failures[0].reason(), Some(expected.as_str()));
    let report = h
        .orm()
        .maintain_partitions(h.db().pool(), now)
        .await
        .unwrap();
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].reason(), Some(expected.as_str()));
    // the other model was maintained all the same
    assert!(
        report
            .existed()
            .iter()
            .any(|p| p.starts_with("scale_audit_p"))
    );
    let health = h.orm().partition_health(h.db().pool()).await.unwrap();
    let by_model = |model: &str| health.iter().find(|w| w.model() == model).unwrap();
    assert_eq!(by_model("scale.event").unreadable_bounds(), 1);
    assert_eq!(by_model("scale.audit").unreadable_bounds(), 0);
    assert!(!by_model("scale.event").is_short());
    h.cleanup().await.unwrap();
}

/// partitioned is a `Database` error naming the table and the model, not a window of 0)
#[tokio::test]
async fn partition_health_refuses_a_table_that_is_not_partitioned() {
    let h = harness().await;
    let message = "table 'scale_event' of partitioned model 'scale.event' is not a partitioned \
                   table in the database (missing or a plain table); run the schema \
                   synchronization first";
    exec(
        &h,
        "DROP TABLE scale_event; CREATE TABLE scale_event (id int8 PRIMARY KEY)",
    )
    .await;
    let error = h.orm().partition_health(h.db().pool()).await.unwrap_err();
    assert_error(&error, ErrorKind::Database, message);
    exec(&h, "DROP TABLE scale_event").await;
    let error = h.orm().partition_health(h.db().pool()).await.unwrap_err();
    assert_error(&error, ErrorKind::Database, message);
    h.cleanup().await.unwrap();
}

/// partition marked; the next retirement takes the marker off a partition that is attached and not
/// pending, so a later manual detach is never dropped as a retirement)
#[tokio::test]
async fn a_retirement_clears_the_marker_a_crash_left_on_an_attached_partition() {
    let h = harness().await;
    let now = db_now(&h).await;
    let owner = pool(&h, 2);
    let created = h
        .orm()
        .ensure_partitions(&owner, "scale.event", Horizon::Declared)
        .await
        .unwrap();
    assert!(created.is_complete(), "{created:?}");
    let live = day_partition("scale_event", now, 1).0;
    exec(&h, &erp_sql::partition::mark_retired("scale_event", &live)).await;
    let report = h
        .orm()
        .retire_partitions(&owner, "scale.event", KeepAfter::Declared)
        .await
        .unwrap();
    assert!(report.is_complete(), "{report:?}");
    assert!(report.dropped().is_empty(), "{report:?}");
    let comment: Option<String> = h
        .db()
        .client()
        .await
        .unwrap()
        .query_one(
            "SELECT obj_description($1::text::regclass, 'pg_class')",
            &[&live],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(comment, None, "the retired marker is gone");
    assert_eq!(
        detach_pending(&h, &live).await,
        Some(false),
        "still attached"
    );
    h.cleanup().await.unwrap();
}

/// the end of the oldest one's range, 0 and `None` once finished)
#[tokio::test]
async fn partition_health_reports_pending_detaches() {
    let h = harness().await;
    let now = db_now(&h).await;
    let health = h.orm().partition_health(h.db().pool()).await.unwrap();
    let event = health.iter().find(|w| w.model() == "scale.event").unwrap();
    assert_eq!(event.pending_detaches(), 0);
    assert_eq!(event.oldest_pending_end(), None);
    // PostgreSQL allows one pending detach per table
    let older = old_partition(&h, now, -9).await;
    interrupt_retirement(&h, &older).await;
    assert_eq!(detach_pending(&h, &older).await, Some(true));
    let health = h.orm().partition_health(h.db().pool()).await.unwrap();
    let event = health.iter().find(|w| w.model() == "scale.event").unwrap();
    assert_eq!(event.pending_detaches(), 1);
    assert_eq!(
        event.oldest_pending_end(),
        Some(day_partition("scale_event", now, -9).2)
    );
    let audit = health.iter().find(|w| w.model() == "scale.audit").unwrap();
    assert_eq!(audit.pending_detaches(), 0);
    let report = h
        .orm()
        .maintain_partitions(h.db().pool(), now)
        .await
        .unwrap();
    assert!(report.is_complete(), "{report:?}");
    let health = h.orm().partition_health(h.db().pool()).await.unwrap();
    let event = health.iter().find(|w| w.model() == "scale.event").unwrap();
    assert_eq!(event.pending_detaches(), 0);
    assert_eq!(event.oldest_pending_end(), None);
    h.cleanup().await.unwrap();
}
