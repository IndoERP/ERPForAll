//! The cost of the row-version trigger and of
//! the upsert statement's second branch (§3.3): measurements, not tests. Method and results.
//! Run them by hand, in release mode, against one cluster that
//! no benchmark and no other work shares (never the shared dev cluster `erpforall-pg`):
//!
//! ```text
//! ERP_TEST_DATABASE_URLS=<admin URL of that cluster> ERP_MEASURE_CLUSTER=<its host:port/database> \
//!   cargo nextest run --release -p erp-test-models -E 'binary(ormx_row_version_cost)' \
//!   --run-ignored only --no-capture
//! ```
//!
//! Both measurements refuse a debug build, a missing `ERP_MEASURE_CLUSTER`, and a cluster list
//! that names anything but that one cluster. `--no-capture` also makes nextest run them one after
//! the other.
//!
//! # Row-version trigger (§4.5)
//!
//! Four tables of 1,000,000 rows and 12 columns, one per cell:
//!
//! | Cell | Table | Statement |
//! |---|---|---|
//! | A | no trigger | the ORM's flush `UPDATE`, which sets `row_version = row_version + 1` |
//! | B | the trigger with its `WHEN` clause (the design) | the same statement |
//! | C | the trigger without `WHEN` (its function runs for every row) | the same statement |
//! | D | the trigger with `WHEN` | a raw `UPDATE` that does not set `row_version` |
//!
//! Workloads per run: 50 flush statements of 1,000 rows each (distinct blocks of ids), and 1,000
//! single-row statements; every statement is its own transaction. The four cells' runs of one
//! workload are interleaved statement by statement: statement `s` of every cell runs before
//! statement `s + 1` of any, in a cell order that rotates with `s` and the round, so a moment of
//! host noise (a checkpoint, a slower core for the cluster's VM) lands on all four cells alike. A
//! cell's run time is the sum over its statements. Each run is timed in two ways, on different
//! rows:
//!
//! - **server**: every statement runs under `EXPLAIN (ANALYZE, TIMING OFF, BUFFERS OFF)` and the
//!   run's time is the sum of the executor times it reports: the part of a statement a row trigger
//!   adds to, without the network, parsing, planning and commit around it;
//! - **client**: the wall time of the plain prepared statements, as the ORM sends them.
//!
//! One untimed warm-up round, then up to [`ROUNDS`] rounds, in whole groups of [`GROUP`] while
//! [`BUDGET`] lasts. A round runs the flush workload, then the single-row one, each once per clock;
//! the server and client runs swap who goes first every four rounds. After every round the tables
//! are vacuumed (autovacuum is off on them, so no background vacuum or analyze lands inside a timed
//! run) and a `CHECKPOINT` follows, so every round starts from a fresh checkpoint. The flush writes
//! megabytes of full-page images per statement, so WAL volume still starts checkpoints inside the
//! rounds; the checkpointer's counters before and after the rounds report how many, and the
//! statement-level interleaving spreads them over all cells. Reported: p50 and p95 of the run times
//! per cell, their ratio to A, the median of the per-round ratios to A with a 95% bootstrap
//! interval, the other client connections seen on the cluster after every round, the raw run times,
//! and `EXPLAIN (ANALYZE, BUFFERS, WAL)` of one statement of each kind per cell.
//!
//! # Upsert second branch (§3.3)
//!
//! A 1,000,000-row table with a three-column business key (one part nullable); statements of
//! 1,000 rows whose every row is inserted, updated, or left unchanged (those come back through
//! the second branch), interleaved statement by statement (inserted, updated, then unchanged on the
//! keys just updated) and timed in the same two ways; `VACUUM` and `CHECKPOINT` after every round
//! as above.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "measurement"
)]

use std::fmt;
use std::time::{Duration, Instant};

use erp_sdk::{ColumnType, DateTime};
use erp_testkit::TestDb;
use erp_testkit::deadpool_postgres::Object;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Row, SimpleQueryMessage, Statement};

const ROWS: i64 = 1_000_000;
const BATCH: i64 = 1_000;
const BATCH_STATEMENTS: i64 = 50;
const SINGLE_STATEMENTS: i64 = 1_000;
/// Rounds at most.
const ROUNDS: usize = 80;
/// Rounds run in whole groups: as many with the server runs first as with the client runs first.
const GROUP: usize = 8;
/// Time budget of the timed rounds: no group starts that would end after it.
const BUDGET: Duration = Duration::from_secs(40 * 60);
/// Resamples of the bootstrap interval of the median per-round ratio.
const BOOTSTRAP: usize = 10_000;
/// The cluster (`host:port/database`) a measurement may run on.
const MEASURE_CLUSTER_ENV: &str = "ERP_MEASURE_CLUSTER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    A,
    B,
    C,
    D,
}

impl Cell {
    fn table(self) -> &'static str {
        match self {
            Self::A => "rv_a",
            Self::B => "rv_b",
            Self::C => "rv_c",
            Self::D => "rv_d",
        }
    }

    /// The ORM's statement sets `row_version + 1`; D's raw statement does not.
    fn bumps(self) -> bool {
        self != Self::D
    }

    /// The trigger function runs for every updated row: C has no `WHEN`, and D's statement leaves
    /// `row_version` alone so the `WHEN` holds. A has no trigger; B's `WHEN` is false.
    fn calls_function(self) -> bool {
        matches!(self, Self::C | Self::D)
    }
}

const CELLS: [Cell; 4] = [Cell::A, Cell::B, Cell::C, Cell::D];

/// How a run is timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clock {
    /// The sum of the executor times `EXPLAIN (ANALYZE, TIMING OFF, BUFFERS OFF)` reports.
    Server,
    /// Wall time of the plain prepared statements.
    Client,
}

impl Clock {
    fn index(self) -> usize {
        match self {
            Self::Server => 0,
            Self::Client => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }

    /// Server runs update other rows than the client runs of the same round: half a table away.
    fn offset(self) -> i64 {
        match self {
            Self::Server => ROWS / 2,
            Self::Client => 0,
        }
    }

    /// Server first in rounds 0–3 of every group, client first in rounds 4–7.
    fn order(round: usize) -> [Self; 2] {
        if (round / 4).is_multiple_of(2) {
            [Self::Server, Self::Client]
        } else {
            [Self::Client, Self::Server]
        }
    }
}

const CLOCKS: [Clock; 2] = [Clock::Server, Clock::Client];

/// Create the measurement database on the cluster [`MEASURE_CLUSTER_ENV`] names, which must be
/// the only one configured, and refuse a debug build: a timing from a debug build or from a
/// cluster other work shares is not a measurement (the lead's ruling, 2026-10-16).
async fn measurement_db() -> TestDb {
    // a runtime check, not a const assertion: debug builds (CI) must still compile the file
    if cfg!(debug_assertions) {
        panic!("a measurement needs a release build: run it with `cargo nextest run --release`");
    }
    let Ok(expected) = std::env::var(MEASURE_CLUSTER_ENV) else {
        panic!(
            "set {MEASURE_CLUSTER_ENV} to the host:port/database of the cluster to measure on, \
             and ERP_TEST_DATABASE_URLS to its admin URL only"
        );
    };
    let configured: Vec<String> = erp_testkit::clusters_from_env()
        .unwrap()
        .iter()
        .map(|c| c.label().to_owned())
        .collect();
    assert_eq!(
        configured,
        std::slice::from_ref(&expected),
        "the configured test clusters must be exactly the measurement cluster"
    );
    let db = TestDb::builder().pool_size(2).create().await.unwrap();
    assert_eq!(db.cluster().label(), expected);
    db
}

/// Print where the measurement runs and the server settings a reader needs to judge it.
async fn print_server(client: &Object, db: &TestDb) {
    let row = client
        .query_one(
            "SELECT version(), current_setting('shared_buffers'), \
             current_setting('synchronous_commit'), current_setting('jit'), \
             current_setting('max_connections'), current_setting('plan_cache_mode')",
            &[],
        )
        .await
        .unwrap();
    let setting = |i: usize| -> String { row.get(i) };
    println!(
        "database {} on cluster {}\nserver: {}\nshared_buffers {}, synchronous_commit {}, jit {}, \
         max_connections {}, plan_cache_mode {}\n{}",
        db.name(),
        db.cluster().label(),
        setting(0),
        setting(1),
        setting(2),
        setting(3),
        setting(4),
        setting(5),
        others(client).await
    );
}

/// Client connections to other databases on the cluster at one moment.
#[derive(Debug, Clone, Copy)]
struct Others {
    connected: i64,
    active: i64,
    test_databases: i64,
}

impl fmt::Display for Others {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "other client connections {} ({} running a statement), other test databases {}",
            self.connected, self.active, self.test_databases
        )
    }
}

async fn others(client: &Object) -> Others {
    let row = client
        .query_one(
            "SELECT count(*), count(*) FILTER (WHERE state = 'active'), \
             (SELECT count(*) FROM pg_database WHERE starts_with(datname, 'erpforall_test_') \
              AND datname <> current_database()) \
             FROM pg_stat_activity WHERE backend_type = 'client backend' \
             AND datname IS DISTINCT FROM current_database()",
            &[],
        )
        .await
        .unwrap();
    Others {
        connected: row.get(0),
        active: row.get(1),
        test_databases: row.get(2),
    }
}

/// Vacuum `tables`, then `CHECKPOINT`, so the next round starts from a fresh checkpoint (module
/// documentation). Returns how long the checkpoint took.
async fn settle(client: &Object, tables: &[&str]) -> Duration {
    for t in tables {
        client.batch_execute(&format!("VACUUM {t}")).await.unwrap();
    }
    let started = Instant::now();
    client.batch_execute("CHECKPOINT").await.unwrap();
    started.elapsed()
}

/// The checkpointer's counters: started on schedule, requested (a `CHECKPOINT` command or WAL
/// volume), completed.
#[derive(Debug, Clone, Copy)]
struct Checkpoints {
    timed: i64,
    requested: i64,
    done: i64,
}

async fn checkpoints(client: &Object) -> Checkpoints {
    let row = client
        .query_one(
            "SELECT num_timed, num_requested, num_done FROM pg_stat_checkpointer",
            &[],
        )
        .await
        .unwrap();
    Checkpoints {
        timed: row.get(0),
        requested: row.get(1),
        done: row.get(2),
    }
}

/// `EXPLAIN` options of a timed server run: the executor time with the least instrumentation.
fn timed(sql: &str) -> String {
    format!("EXPLAIN (ANALYZE, TIMING OFF, BUFFERS OFF) {sql}")
}

/// What `EXPLAIN ANALYZE` reports for one statement.
#[derive(Debug)]
struct Explained {
    /// `Execution Time`: the executor, without parsing, planning, network and commit.
    execution: Duration,
    /// Calls of row-trigger functions (`Trigger …: calls=N`; EXPLAIN leaves out a trigger whose
    /// function never ran).
    trigger_calls: u64,
    /// `Tuples Inserted: N` of an `INSERT … ON CONFLICT`.
    tuples_inserted: Option<u64>,
    /// `Rows Removed by Conflict Filter: N` (EXPLAIN leaves the line out when N is 0).
    removed_by_conflict_filter: u64,
}

fn explained(rows: &[Row]) -> Explained {
    let lines: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    let count = |line: &str, value: &str| -> u64 {
        value
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("unexpected EXPLAIN line {line:?}: {e}"))
    };
    let mut execution = None;
    let mut out = Explained {
        execution: Duration::ZERO,
        trigger_calls: 0,
        tuples_inserted: None,
        removed_by_conflict_filter: 0,
    };
    for line in lines.iter().map(|l| l.trim()) {
        if let Some(rest) = line.strip_prefix("Execution Time:") {
            let Some(ms) = rest.trim().strip_suffix(" ms") else {
                panic!("unexpected EXPLAIN line {line:?}");
            };
            let ms: f64 = ms.parse().unwrap();
            execution = Some(Duration::from_secs_f64(ms / 1000.0));
        } else if line.starts_with("Trigger ") {
            let Some((_, calls)) = line.rsplit_once("calls=") else {
                panic!("unexpected EXPLAIN line {line:?}");
            };
            out.trigger_calls += count(line, calls);
        } else if let Some(n) = line.strip_prefix("Tuples Inserted:") {
            out.tuples_inserted = Some(count(line, n));
        } else if let Some(n) = line.strip_prefix("Rows Removed by Conflict Filter:") {
            out.removed_by_conflict_filter = count(line, n);
        }
    }
    let Some(execution) = execution else {
        panic!("EXPLAIN ANALYZE without Execution Time: {lines:#?}");
    };
    out.execution = execution;
    out
}

/// Run `sql` with `params` (bound like the timed statements) and print every line it returns.
async fn print_plan(client: &Object, title: &str, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    println!("\n{title}:");
    for row in client.query(sql, params).await.unwrap() {
        let line: String = row.get(0);
        println!("    {line}");
    }
}

/// Print the generic plan of `sql` (sent unbound, as a prepared statement's sixth execution may
/// plan it).
async fn print_generic_plan(client: &Object, title: &str, sql: &str) {
    println!("\n{title}:");
    for message in client
        .simple_query(&format!("EXPLAIN (GENERIC_PLAN) {sql}"))
        .await
        .unwrap()
    {
        if let SimpleQueryMessage::Row(row) = message {
            println!("    {}", row.get(0).unwrap());
        }
    }
}

// ---- the row-version trigger (design note §4.5) ----------------------------------------------

async fn setup(client: &Object) {
    let function = erp_sql::version::bump_function();
    client.batch_execute(&function.join(";\n")).await.unwrap();
    for cell in CELLS {
        let t = cell.table();
        client
            .batch_execute(&format!(
                "CREATE TABLE {t} (id int8 PRIMARY KEY, row_version int8 NOT NULL, \
                 product_id int8, location_id int8, quantity float8, reserved float8, \
                 amount numeric, name varchar, note text, in_date date, write_date timestamp, \
                 active bool) WITH (autovacuum_enabled = false);
                 INSERT INTO {t} SELECT g, 1, g % 5000, g % 50, g * 0.5, 0, g * 1.25, 'name ' || g, \
                 'note ' || g, DATE '2026-01-01' + (g % 365)::int, now(), true \
                 FROM generate_series(1, {ROWS}) AS g"
            ))
            .await
            .unwrap();
        // alone: a multi-statement batch is one transaction block, which VACUUM refuses
        client
            .batch_execute(&format!("VACUUM ANALYZE {t}"))
            .await
            .unwrap();
        let trigger = match cell {
            Cell::A => None,
            Cell::B | Cell::D => Some(erp_sql::version::bump_trigger(t, false)),
            Cell::C => Some(vec![format!(
                "CREATE TRIGGER \"{t}__row_version\" BEFORE UPDATE ON {t} FOR EACH ROW EXECUTE \
                 FUNCTION erp_row_version_bump()"
            )]),
        };
        if let Some(ddl) = trigger {
            client.batch_execute(&ddl.join(";\n")).await.unwrap();
        }
    }
}

/// The flush statement of one cell: the ORM's builder for A–C, the same without the bump for D.
fn flush_sql(cell: Cell) -> String {
    let columns = [
        ("quantity", ColumnType::Float8),
        ("write_date", ColumnType::Timestamp),
    ];
    let with = erp_sql::version::VersionedUpdate::new().bump(cell.bumps());
    erp_sql::version::update_from_arrays(cell.table(), &columns, &[], &with)
}

fn single_sql(cell: Cell) -> String {
    let bump = if cell.bumps() {
        ", row_version = row_version + 1"
    } else {
        ""
    };
    format!(
        "UPDATE {} SET quantity = quantity + 1, write_date = now()::timestamp{bump} WHERE id = $1",
        cell.table()
    )
}

/// The parameters of one flush statement: ids `block * BATCH + 1 ..= (block + 1) * BATCH`.
type FlushParams = (Vec<i64>, Vec<Option<f64>>, Vec<Option<DateTime>>);

fn flush_params(block: i64) -> FlushParams {
    let stamp = DateTime::parse_odoo("2026-10-16 00:00:00").unwrap();
    let ids: Vec<i64> = (1..=BATCH).map(|i| block * BATCH + i).collect();
    let qty = ids.iter().map(|i| Some(*i as f64)).collect();
    let now = ids.iter().map(|_| Some(stamp)).collect();
    (ids, qty, now)
}

/// Prepare `sql` for `clock`: under `EXPLAIN` for the server clock.
async fn prepare(client: &Object, sql: &str, clock: Clock) -> Statement {
    match clock {
        Clock::Server => client.prepare(&timed(sql)).await.unwrap(),
        Clock::Client => client.prepare(sql).await.unwrap(),
    }
}

/// Execute one statement of `cell` that must update `rows` rows; its time by `clock`.
async fn execute_timed(
    client: &Object,
    stmt: &Statement,
    params: &[&(dyn ToSql + Sync)],
    clock: Clock,
    cell: Cell,
    rows: u64,
) -> Duration {
    match clock {
        Clock::Client => {
            let started = Instant::now();
            let updated = client.execute(stmt, params).await.unwrap();
            let elapsed = started.elapsed();
            assert_eq!(updated, rows, "{cell:?}");
            elapsed
        }
        Clock::Server => {
            let plan = explained(&client.query(stmt, params).await.unwrap());
            let calls = if cell.calls_function() { rows } else { 0 };
            assert_eq!(plan.trigger_calls, calls, "{cell:?}: {plan:?}");
            plan.execution
        }
    }
}

/// The flush runs of all cells in one round for one clock, interleaved statement by statement:
/// `BATCH_STATEMENTS` statements of `BATCH` rows per cell, a distinct block of ids each. Returns
/// every cell's run time, in `CELLS` order.
async fn flush_phase(client: &Object, round: usize, clock: Clock) -> [Duration; 4] {
    let mut stmts = Vec::with_capacity(CELLS.len());
    for cell in CELLS {
        stmts.push(prepare(client, &flush_sql(cell), clock).await);
    }
    let first = i64::try_from(round).unwrap() * BATCH_STATEMENTS + clock.offset() / BATCH;
    let params: Vec<FlushParams> = (0..BATCH_STATEMENTS)
        .map(|s| flush_params((first + s) % (ROWS / BATCH)))
        .collect();
    let rows = u64::try_from(BATCH).unwrap();
    let mut totals = [Duration::ZERO; 4];
    for (s, (ids, qty, now)) in params.iter().enumerate() {
        for k in 0..CELLS.len() {
            let i = (round + s + k) % CELLS.len();
            totals[i] +=
                execute_timed(client, &stmts[i], &[ids, qty, now], clock, CELLS[i], rows).await;
        }
    }
    totals
}

/// Id of single-row statement `s` of a round: spread over the table.
fn single_id(round: usize, s: i64, clock: Clock) -> i64 {
    ((i64::try_from(round).unwrap() * SINGLE_STATEMENTS + s) * 7919 + clock.offset()) % ROWS + 1
}

/// The single-row runs of all cells in one round for one clock, interleaved statement by
/// statement: `SINGLE_STATEMENTS` statements of one row per cell. Returns every cell's run time,
/// in `CELLS` order.
async fn single_phase(client: &Object, round: usize, clock: Clock) -> [Duration; 4] {
    let mut stmts = Vec::with_capacity(CELLS.len());
    for cell in CELLS {
        stmts.push(prepare(client, &single_sql(cell), clock).await);
    }
    let ids: Vec<i64> = (0..SINGLE_STATEMENTS)
        .map(|s| single_id(round, s, clock))
        .collect();
    let mut totals = [Duration::ZERO; 4];
    for (s, id) in ids.iter().enumerate() {
        for k in 0..CELLS.len() {
            let i = (round + s + k) % CELLS.len();
            totals[i] += execute_timed(client, &stmts[i], &[id], clock, CELLS[i], 1).await;
        }
    }
    totals
}

/// Run times of one workload, by clock and cell, in round order.
type Runs = [[Vec<Duration>; 4]; 2];

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn sorted_ms(runs: &[Duration]) -> Vec<f64> {
    let mut out: Vec<f64> = runs.iter().map(|d| ms(*d)).collect();
    out.sort_by(f64::total_cmp);
    out
}

/// Linear interpolation between the closest ranks of a sorted sample.
fn quantile(sorted: &[f64], p: f64) -> f64 {
    let pos = (sorted.len() - 1) as f64 * p;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    quantile(values, 0.5)
}

fn pct(ratio: f64) -> f64 {
    (ratio - 1.0) * 100.0
}

/// xorshift64* with a fixed seed: the interval is reproducible from the raw run times.
struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: usize) -> usize {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64) as usize
    }
}

/// Median of the per-round ratios `cell / base` and its 95% percentile-bootstrap interval.
struct RatioInterval {
    median: f64,
    low: f64,
    high: f64,
}

fn ratio_interval(cell: &[Duration], base: &[Duration]) -> RatioInterval {
    assert_eq!(cell.len(), base.len());
    let ratios: Vec<f64> = cell
        .iter()
        .zip(base)
        .map(|(c, b)| c.as_secs_f64() / b.as_secs_f64())
        .collect();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut medians: Vec<f64> = (0..BOOTSTRAP)
        .map(|_| {
            let mut sample: Vec<f64> = (0..ratios.len())
                .map(|_| ratios[rng.below(ratios.len())])
                .collect();
            median(&mut sample)
        })
        .collect();
    medians.sort_by(f64::total_cmp);
    RatioInterval {
        median: median(&mut ratios.clone()),
        low: quantile(&medians, 0.025),
        high: quantile(&medians, 0.975),
    }
}

/// One table per workload and clock: p50, p95, the ratios to A and the difference per row.
fn report(title: &str, rows_per_run: i64, runs: &[Vec<Duration>; 4]) {
    println!(
        "\n{title}\n\n| Cell | p50 (ms) | p95 (ms) | p50 vs A | p95 vs A | per-round ratio to A: \
         median [95% interval] | p50 over A per row (µs) |\n|---|---|---|---|---|---|---|"
    );
    let a = sorted_ms(&runs[0]);
    let (a50, a95) = (quantile(&a, 0.5), quantile(&a, 0.95));
    for (i, cell) in CELLS.iter().enumerate() {
        let s = sorted_ms(&runs[i]);
        let (p50, p95) = (quantile(&s, 0.5), quantile(&s, 0.95));
        let paired = if i == 0 {
            "–".to_owned()
        } else {
            let r = ratio_interval(&runs[i], &runs[0]);
            format!(
                "{:+.1}% [{:+.1}%, {:+.1}%]",
                pct(r.median),
                pct(r.low),
                pct(r.high)
            )
        };
        println!(
            "| {cell:?} | {p50:.2} | {p95:.2} | {:+.1}% | {:+.1}% | {paired} | {:+.3} |",
            pct(p50 / a50),
            pct(p95 / a95),
            (p50 - a50) * 1000.0 / rows_per_run as f64
        );
    }
}

fn print_raw(workload: &str, runs: &Runs) {
    for clock in CLOCKS {
        for (i, cell) in CELLS.iter().enumerate() {
            let values: Vec<String> = runs[clock.index()][i]
                .iter()
                .map(|d| format!("{:.3}", ms(*d)))
                .collect();
            println!(
                "raw,{workload},{},{cell:?},{}",
                clock.label(),
                values.join(",")
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement of design note §4.5: run by hand, see the module documentation"]
async fn row_version_trigger_cost() {
    let db = measurement_db().await;
    let client = db.client().await.unwrap();
    print_server(&client, &db).await;
    let setup_started = Instant::now();
    setup(&client).await;
    println!("setup: {} s", setup_started.elapsed().as_secs_f64().round());
    // the trigger fires for every row: verify the cells before timing them
    for cell in CELLS {
        let stmt = client.prepare(&flush_sql(cell)).await.unwrap();
        let ids: Vec<i64> = vec![1];
        client
            .execute(&stmt, &[&ids, &vec![Some(0.0f64)], &vec![None::<DateTime>]])
            .await
            .unwrap();
        let version: i64 = client
            .query_one(
                &format!("SELECT row_version FROM {} WHERE id = 1", cell.table()),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(version, 2, "{cell:?}: one UPDATE, one bump");
    }
    let tables = CELLS.map(Cell::table);
    // warm-up: one untimed round with a round index of its own
    for clock in CLOCKS {
        flush_phase(&client, ROUNDS, clock).await;
        single_phase(&client, ROUNDS, clock).await;
    }
    let warm = settle(&client, &tables).await;
    println!("warm-up round done, checkpoint {} ms", warm.as_millis());
    let before = checkpoints(&client).await;
    let mut flush: Runs = Default::default();
    let mut single: Runs = Default::default();
    let mut checkpoint_times = Vec::new();
    let started = Instant::now();
    let mut rounds = 0;
    for round in 0..ROUNDS {
        if round > 0 && round.is_multiple_of(GROUP) {
            let per_round = started.elapsed() / u32::try_from(round).unwrap();
            if started.elapsed() + per_round * u32::try_from(GROUP).unwrap() > BUDGET {
                println!("time budget: stopping after {round} rounds");
                break;
            }
        }
        for clock in Clock::order(round) {
            let totals = flush_phase(&client, round, clock).await;
            for (runs, total) in flush[clock.index()].iter_mut().zip(totals) {
                runs.push(total);
            }
        }
        for clock in Clock::order(round) {
            let totals = single_phase(&client, round, clock).await;
            for (runs, total) in single[clock.index()].iter_mut().zip(totals) {
                runs.push(total);
            }
        }
        let checkpoint = settle(&client, &tables).await;
        checkpoint_times.push(checkpoint);
        rounds = round + 1;
        println!(
            "round {rounds} of {ROUNDS} done at {:.0} s, checkpoint {} ms; {}",
            started.elapsed().as_secs_f64(),
            checkpoint.as_millis(),
            others(&client).await
        );
    }
    let after = checkpoints(&client).await;
    println!(
        "\n{rounds} rounds in {:.0} s; checkpoints during the rounds: {} timed, {} requested ({} by \
         this measurement), {} completed; checkpoint p50 {:.0} ms",
        started.elapsed().as_secs_f64(),
        after.timed - before.timed,
        after.requested - before.requested,
        rounds,
        after.done - before.done,
        quantile(&sorted_ms(&checkpoint_times), 0.5)
    );
    let flush_rows = BATCH_STATEMENTS * BATCH;
    for clock in CLOCKS {
        report(
            &format!(
                "flush, {BATCH_STATEMENTS} statements x {BATCH} rows per run, {}",
                clock.label()
            ),
            flush_rows,
            &flush[clock.index()],
        );
        report(
            &format!(
                "single row, {SINGLE_STATEMENTS} statements per run, {}",
                clock.label()
            ),
            SINGLE_STATEMENTS,
            &single[clock.index()],
        );
    }
    println!("\nraw run times (ms), in round order:");
    print_raw("flush", &flush);
    print_raw("single", &single);
    // one statement of each kind per cell with every counter, after the last checkpoint, on rows
    // the last rounds and the warm-up (round index ROUNDS) did not touch
    let next = rounds + 1;
    let block = (i64::try_from(next).unwrap() * BATCH_STATEMENTS) % (ROWS / BATCH);
    let (ids, qty, now) = flush_params(block);
    let id = single_id(next, 0, Clock::Client);
    for cell in CELLS {
        print_plan(
            &client,
            &format!("EXPLAIN {cell:?}, flush of {BATCH} rows"),
            &format!("EXPLAIN (ANALYZE, BUFFERS, WAL) {}", flush_sql(cell)),
            &[&ids, &qty, &now],
        )
        .await;
        print_plan(
            &client,
            &format!("EXPLAIN {cell:?}, single row"),
            &format!("EXPLAIN (ANALYZE, BUFFERS, WAL) {}", single_sql(cell)),
            &[&id],
        )
        .await;
    }
    print_generic_plan(&client, "generic plan, A flush", &flush_sql(Cell::A)).await;
    // every cell applied the same updates, each bumping by one (statement or trigger)
    let mut sums = Vec::new();
    for cell in CELLS {
        let sum: i64 = client
            .query_one(
                &format!("SELECT sum(row_version)::int8 FROM {}", cell.table()),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        sums.push(sum);
    }
    println!("\nsum(row_version) per cell: {sums:?}");
    assert!(sums.iter().all(|s| *s == sums[0]), "{sums:?}");
    drop(client);
    db.cleanup().await.unwrap();
}

// ---- the upsert statement's second branch (design note §3.3: "the slice measures it") --------

const UPSERT_ROWS: i64 = 1_000;
const UPSERT_STATEMENTS: i64 = 20;
const UPSERT_RUNS: i64 = 20;

/// The upsert statement over `uq` (key `product_id, location_id, lot_id`, `lot_id` nullable),
/// accumulating `quantity`, built by the ORM's own builder.
fn upsert_sql() -> String {
    use erp_sql::upsert::{ConflictAction, KeyColumn, SetAction, Upsert};
    let columns = [
        ("product_id", ColumnType::Int8),
        ("location_id", ColumnType::Int8),
        ("lot_id", ColumnType::Int8),
        ("quantity", ColumnType::Float8),
    ];
    let key = [
        KeyColumn {
            name: "product_id",
            nullable: false,
        },
        KeyColumn {
            name: "location_id",
            nullable: false,
        },
        KeyColumn {
            name: "lot_id",
            nullable: true,
        },
    ];
    let sets = [("quantity", SetAction::Accumulate)];
    erp_sql::upsert::upsert(
        &Upsert::new(
            "uq",
            "uq_key",
            &columns,
            &key,
            ConflictAction::Update {
                sets: &sets,
                audit: &[],
            },
        )
        .insert_exprs(&[("row_version", "1")])
        .row_version(true)
        .returning(&[("quantity", ColumnType::Float8)]),
    )
}

/// What happens to every row of a statement, in the order a round runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Inserted,
    Updated,
    Unchanged,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged (second branch)",
        }
    }

    /// First product of a run: new keys above the table's for inserts; a block of existing keys
    /// of its own per round and clock for updates, which the no-op run of the same round and
    /// clock then reuses.
    fn first(self, round: i64, clock: Clock) -> i64 {
        let span = UPSERT_ROWS * UPSERT_STATEMENTS;
        let slot = match clock {
            Clock::Client => round,
            Clock::Server => UPSERT_RUNS + round,
        };
        match self {
            Self::Inserted => ROWS + 1 + slot * span,
            Self::Updated | Self::Unchanged => 1 + slot * span,
        }
    }

    /// The accumulated quantity: 0 leaves the row unchanged (the conflict `WHERE` is false).
    fn delta(self) -> f64 {
        match self {
            Self::Inserted | Self::Updated => 1.0,
            Self::Unchanged => 0.0,
        }
    }
}

const OUTCOMES: [Outcome; 3] = [Outcome::Inserted, Outcome::Updated, Outcome::Unchanged];

type UpsertParams = (Vec<i64>, Vec<i64>, Vec<Option<i64>>, Vec<f64>);

/// `UPSERT_ROWS` keys from product `base`, every second key without a lot.
fn upsert_params(base: i64, delta: f64) -> UpsertParams {
    let products: Vec<i64> = (base..base + UPSERT_ROWS).collect();
    let locations = products.iter().map(|p| p % 50).collect();
    let lots = products
        .iter()
        .map(|p| (p % 2 == 0).then_some(*p))
        .collect();
    let quantity = products.iter().map(|_| delta).collect();
    (products, locations, lots, quantity)
}

/// Execute one upsert statement whose every row must have `outcome`; its time by `clock`.
async fn upsert_execute(
    client: &Object,
    stmt: &Statement,
    params: &[&(dyn ToSql + Sync)],
    clock: Clock,
    outcome: Outcome,
) -> Duration {
    let rows = u64::try_from(UPSERT_ROWS).unwrap();
    match clock {
        Clock::Client => {
            let started = Instant::now();
            let result = client.query(stmt, params).await.unwrap();
            let elapsed = started.elapsed();
            assert_eq!(result.len(), usize::try_from(UPSERT_ROWS).unwrap());
            for row in &result {
                let (inserted, written): (bool, bool) = (row.get(2), row.get(3));
                assert_eq!(inserted, outcome == Outcome::Inserted, "{outcome:?}");
                assert_eq!(written, outcome != Outcome::Unchanged, "{outcome:?}");
            }
            elapsed
        }
        Clock::Server => {
            let plan = explained(&client.query(stmt, params).await.unwrap());
            let (inserted, filtered) = match outcome {
                Outcome::Inserted => (rows, 0),
                Outcome::Updated => (0, 0),
                Outcome::Unchanged => (0, rows),
            };
            assert_eq!(
                plan.tuples_inserted,
                Some(inserted),
                "{outcome:?}: {plan:?}"
            );
            assert_eq!(
                plan.removed_by_conflict_filter, filtered,
                "{outcome:?}: {plan:?}"
            );
            plan.execution
        }
    }
}

/// One round of the upsert measurement for one clock, interleaved statement by statement:
/// statement `s` of every outcome runs before statement `s + 1` of any (inserted, updated, then
/// unchanged on the keys just updated). Returns every statement's time, by outcome.
async fn upsert_phase(client: &Object, round: i64, clock: Clock) -> [Vec<Duration>; 3] {
    let stmt = prepare(client, &upsert_sql(), clock).await;
    let mut out: [Vec<Duration>; 3] = Default::default();
    for s in 0..UPSERT_STATEMENTS {
        for (times, outcome) in out.iter_mut().zip(OUTCOMES) {
            let base = outcome.first(round, clock) + s * UPSERT_ROWS;
            let (products, locations, lots, quantity) = upsert_params(base, outcome.delta());
            let params: [&(dyn ToSql + Sync); 4] = [&products, &locations, &lots, &quantity];
            times.push(upsert_execute(client, &stmt, &params, clock, outcome).await);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement of design note §3.3: run by hand, see the module documentation"]
async fn upsert_statement_cost() {
    let db = measurement_db().await;
    let client = db.client().await.unwrap();
    print_server(&client, &db).await;
    client
        .batch_execute(&format!(
            "CREATE TABLE uq (id int8 GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, \
             row_version int8 NOT NULL, product_id int8 NOT NULL, location_id int8 NOT NULL, \
             lot_id int8, quantity float8, name varchar, note text) \
             WITH (autovacuum_enabled = false);
             ALTER TABLE uq ADD CONSTRAINT uq_key {};
             INSERT INTO uq (row_version, product_id, location_id, lot_id, quantity, name, note)
             SELECT 1, g, g % 50, CASE WHEN g % 2 = 0 THEN g END, 0, 'name ' || g, 'note ' || g
             FROM generate_series(1, {ROWS}) AS g",
            erp_sql::upsert::key_definition(&["product_id", "location_id", "lot_id"], false)
        ))
        .await
        .unwrap();
    client.batch_execute("VACUUM ANALYZE uq").await.unwrap();
    client.batch_execute("CHECKPOINT").await.unwrap();
    let before = checkpoints(&client).await;
    // per clock and outcome, every statement's time
    let mut times: [[Vec<Duration>; 3]; 2] = Default::default();
    let started = Instant::now();
    for round in 0..UPSERT_RUNS {
        // server and client take turns going first
        let order = if round % 2 == 0 {
            [Clock::Server, Clock::Client]
        } else {
            [Clock::Client, Clock::Server]
        };
        for clock in order {
            let phase = upsert_phase(&client, round, clock).await;
            for (all, these) in times[clock.index()].iter_mut().zip(phase) {
                all.extend(these);
            }
        }
        let checkpoint = settle(&client, &["uq"]).await;
        println!(
            "upsert round {} of {UPSERT_RUNS} done at {:.0} s, checkpoint {} ms; {}",
            round + 1,
            started.elapsed().as_secs_f64(),
            checkpoint.as_millis(),
            others(&client).await
        );
    }
    let after = checkpoints(&client).await;
    println!(
        "checkpoints during the rounds: {} timed, {} requested ({UPSERT_RUNS} by this \
         measurement), {} completed",
        after.timed - before.timed,
        after.requested - before.requested,
        after.done - before.done
    );
    println!(
        "\n| Clock | Rows per statement | Outcome of every row | statements | p50 (ms) | p95 (ms) | \
         p50 vs updated |\n|---|---|---|---|---|---|---|"
    );
    for clock in CLOCKS {
        let updated = quantile(&sorted_ms(&times[clock.index()][1]), 0.5);
        for (o, outcome) in OUTCOMES.iter().enumerate() {
            let sorted = sorted_ms(&times[clock.index()][o]);
            let p50 = quantile(&sorted, 0.5);
            println!(
                "| {} | {UPSERT_ROWS} | {} | {} | {p50:.2} | {:.2} | {:+.1}% |",
                clock.label(),
                outcome.label(),
                sorted.len(),
                quantile(&sorted, 0.95),
                pct(p50 / updated)
            );
        }
    }
    // every outcome once more with every counter, on keys no run touched
    let span = UPSERT_ROWS * UPSERT_STATEMENTS;
    let sql = format!("EXPLAIN (ANALYZE, BUFFERS, WAL) {}", upsert_sql());
    for outcome in OUTCOMES {
        let base = match outcome {
            Outcome::Inserted => ROWS + 1 + 2 * UPSERT_RUNS * span,
            Outcome::Updated | Outcome::Unchanged => 1 + 2 * UPSERT_RUNS * span,
        };
        let (products, locations, lots, quantity) = upsert_params(base, outcome.delta());
        print_plan(
            &client,
            &format!(
                "EXPLAIN upsert, {UPSERT_ROWS} rows, every row {}",
                outcome.label()
            ),
            &sql,
            &[&products, &locations, &lots, &quantity],
        )
        .await;
    }
    drop(client);
    db.cleanup().await.unwrap();
}
