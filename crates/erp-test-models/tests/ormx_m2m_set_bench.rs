//! should m2m `set` write its relation rows with one statement instead of two?
//! (rows 5 and 43.)
//!
//! ```text
//! cargo nextest run --release -p erp-test-models --test ormx_m2m_set_bench --run-ignored only \
//!   --no-capture
//! ```
//!
//! `ORMX_M2M_SIZES` (default `200000,10000000`) sets the relation table sizes, `ORMX_M2M_RUNS`
//! (default 30) the runs per cell. Results:.
//!
//! The candidates replace only the writes; the ORM keeps its pre-read of the old pairs (cache,
//! archived and inaccessible targets need them), so the pre-read is not measured:
//!
//! - **A** (today, `erp_sql::relational::{insert_pairs, delete_pairs}`): `INSERT … ON CONFLICT DO
//!   NOTHING` of the added pairs, then `DELETE` of the removed pairs: 2 statements, 2 round trips;
//! - **B**: the same two halves in one statement, the `DELETE` in a writable CTE (row 43);
//! - **C**: `MERGE` with the batch's complete new pair set as source, `WHEN NOT MATCHED BY TARGET
//!   THEN INSERT`, `WHEN NOT MATCHED BY SOURCE AND c1 = ANY(parents) THEN DELETE` (row 5): a FULL
//!   join of the relation table with the source.
//!
//! Workload (design §8): relation tables shaped like the ORM's (`int8` pair, primary key
//! `(c1, c2)`, index `(c2, c1)`), 20 targets per parent; per call 1, 100 or 1,000 random parents,
//! each losing 5 targets and gaining 5. Every timed call is its own transaction and is undone
//! afterwards by an untimed A-call, so every candidate sees the same table. Candidates are
//! interleaved run by run (rotating order) so drift on the shared host hits them alike; 3 warm-up
//! calls per candidate and cell are not counted. The wall time of the statements is measured on
//! the client (BEGIN and COMMIT excluded); `EXPLAIN (ANALYZE, BUFFERS)` of one call per
//! candidate and cell, rolled back, gives the shared buffers and the plan.
//!
//! Decision rule: B or C replaces A only if its p50 is at least 10% better than A's
//! at every batch size on both table sizes, no p95 is more than 5% worse, and C's plan at 10M rows
//! scans the relation table through an index only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "measurement tool"
)]

use std::time::Instant;

use erp_sql::relational::{Relation, delete_pairs, insert_pairs};
use erp_testkit::{TestDb, database_url_from_env};

const REL: Relation<'static> = Relation {
    table: "bench_m2m_rel",
    column1: "c1",
    column2: "c2",
};
const TARGETS_PER_PARENT: i64 = 20;
const CHANGED_PER_PARENT: i64 = 5;
/// Target ids are drawn below this prime, so `j ↦ (p·7919 + j·104729) mod PRIME` is injective in
/// `j` for every parent `p` and the base and added targets of a parent never collide.
const PRIME: i64 = 1_000_003;
const BATCHES: [usize; 3] = [1, 100, 1_000];
/// From this many rows a table is "large": candidate C runs fewer times there.
const LARGE: i64 = 1_000_000;

fn env_list(name: &str, default: &str) -> Vec<i64> {
    std::env::var(name)
        .unwrap_or_else(|_| default.to_owned())
        .split(',')
        .map(|s| s.trim().parse().expect(name))
        .collect()
}

/// The `j`-th target of parent `p`: `j` in `0..20` is the table's state X, `5..25` state Y.
fn target(p: i64, j: i64) -> i64 {
    (p * 7919 + j * 104_729) % PRIME + 1
}

/// Xorshift64: a deterministic parent sampler without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// `n` distinct parents in `1..=parents`, ascending.
    fn parents(&mut self, n: usize, parents: i64) -> Vec<i64> {
        let mut out = std::collections::BTreeSet::new();
        while out.len() < n {
            out.insert((self.next() % parents as u64) as i64 + 1);
        }
        out.into_iter().collect()
    }
}

/// Pairs of `parents` for the targets `js`, sorted (the ORM sorts before it writes).
fn pairs(parents: &[i64], js: std::ops::Range<i64>) -> (Vec<i64>, Vec<i64>) {
    let mut v: Vec<(i64, i64)> = parents
        .iter()
        .flat_map(|p| js.clone().map(move |j| (*p, target(*p, j))))
        .collect();
    v.sort_unstable();
    v.into_iter().unzip()
}

/// B: the `DELETE` half in a writable CTE, the `INSERT` half as the main statement. `$1`/`$2`
/// the added pairs, `$3`/`$4` the removed ones.
fn candidate_b(rel: Relation<'_>) -> String {
    let (t, c1, c2) = (
        erp_sql::quote(rel.table),
        erp_sql::quote(rel.column1),
        erp_sql::quote(rel.column2),
    );
    format!(
        "WITH \"__del\" AS (DELETE FROM {t} WHERE ({c1}, {c2}) IN (SELECT * FROM \
         unnest($3::int8[], $4::int8[]) ORDER BY 1, 2)) INSERT INTO {t} ({c1}, {c2}) SELECT * \
         FROM unnest($1::int8[], $2::int8[]) ORDER BY 1, 2 ON CONFLICT DO NOTHING"
    )
}

/// C: `MERGE` of the batch's complete new pair set (`$1`/`$2`) for the parents `$3`.
fn candidate_c(rel: Relation<'_>) -> String {
    let (t, c1, c2) = (
        erp_sql::quote(rel.table),
        erp_sql::quote(rel.column1),
        erp_sql::quote(rel.column2),
    );
    format!(
        "MERGE INTO {t} AS \"r\" USING (SELECT * FROM unnest($1::int8[], $2::int8[]) AS \
         \"s\"(\"c1\", \"c2\")) AS \"s\" ON \"r\".{c1} = \"s\".\"c1\" AND \"r\".{c2} = \
         \"s\".\"c2\" WHEN NOT MATCHED BY TARGET THEN INSERT ({c1}, {c2}) VALUES (\"s\".\"c1\", \
         \"s\".\"c2\") WHEN NOT MATCHED BY SOURCE AND \"r\".{c1} = ANY($3::int8[]) THEN DELETE"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Candidate {
    A,
    B,
    C,
}

/// The statements and parameters that move `parents` from state X to state Y with `candidate`.
struct Call {
    added: (Vec<i64>, Vec<i64>),
    removed: (Vec<i64>, Vec<i64>),
    new_set: (Vec<i64>, Vec<i64>),
    parents: Vec<i64>,
}

impl Call {
    fn new(parents: Vec<i64>) -> Self {
        Self {
            added: pairs(
                &parents,
                TARGETS_PER_PARENT..TARGETS_PER_PARENT + CHANGED_PER_PARENT,
            ),
            removed: pairs(&parents, 0..CHANGED_PER_PARENT),
            new_set: pairs(
                &parents,
                CHANGED_PER_PARENT..TARGETS_PER_PARENT + CHANGED_PER_PARENT,
            ),
            parents,
        }
    }
}

/// The candidates' statements, prepared once per table (as the ORM's statement cache does), so
/// every timed execution is one round trip per statement.
struct Statements {
    insert: Statement,
    delete: Statement,
    b: Statement,
    c: Statement,
}

impl Statements {
    async fn prepare(client: &Client) -> Self {
        Self {
            insert: client.prepare(&insert_pairs(REL)).await.unwrap(),
            delete: client.prepare(&delete_pairs(REL)).await.unwrap(),
            b: client.prepare(&candidate_b(REL)).await.unwrap(),
            c: client.prepare(&candidate_c(REL)).await.unwrap(),
        }
    }
}

type Client = erp_testkit::deadpool_postgres::Object;
type Statement = erp_testkit::tokio_postgres::Statement;
type Param<'a> = &'a (dyn erp_testkit::tokio_postgres::types::ToSql + Sync);

fn param<T: erp_testkit::tokio_postgres::types::ToSql + Sync>(value: &T) -> Param<'_> {
    value
}

/// Run `candidate` for `call` (state X → Y), returning the elapsed wall time in microseconds.
async fn apply(client: &Client, sql: &Statements, candidate: Candidate, call: &Call) -> u64 {
    let start = Instant::now();
    match candidate {
        Candidate::A => {
            let n = client
                .execute(&sql.insert, &[&call.added.0, &call.added.1])
                .await
                .unwrap();
            let m = client
                .execute(&sql.delete, &[&call.removed.0, &call.removed.1])
                .await
                .unwrap();
            assert_eq!(
                (n, m),
                (call.added.0.len() as u64, call.removed.0.len() as u64)
            );
        }
        Candidate::B => {
            let n = client
                .execute(
                    &sql.b,
                    &[
                        &call.added.0,
                        &call.added.1,
                        &call.removed.0,
                        &call.removed.1,
                    ],
                )
                .await
                .unwrap();
            assert_eq!(n, call.added.0.len() as u64);
        }
        Candidate::C => {
            let n = client
                .execute(&sql.c, &[&call.new_set.0, &call.new_set.1, &call.parents])
                .await
                .unwrap();
            assert_eq!(n, (call.added.0.len() + call.removed.0.len()) as u64);
        }
    }
    u64::try_from(start.elapsed().as_micros()).unwrap()
}

/// Undo `call` (state Y → X) with A, untimed.
async fn revert(client: &Client, sql: &Statements, call: &Call) {
    client.batch_execute("BEGIN").await.unwrap();
    let n = client
        .execute(&sql.insert, &[&call.removed.0, &call.removed.1])
        .await
        .unwrap();
    let m = client
        .execute(&sql.delete, &[&call.added.0, &call.added.1])
        .await
        .unwrap();
    assert_eq!(
        (n, m),
        (call.removed.0.len() as u64, call.added.0.len() as u64)
    );
    client.batch_execute("COMMIT").await.unwrap();
}

/// One timed call in its own transaction, then undone.
async fn timed(client: &Client, sql: &Statements, candidate: Candidate, call: &Call) -> u64 {
    client.batch_execute("BEGIN").await.unwrap();
    let micros = apply(client, sql, candidate, call).await;
    client.batch_execute("COMMIT").await.unwrap();
    revert(client, sql, call).await;
    micros
}

/// `EXPLAIN (ANALYZE, BUFFERS)` of the candidate's statements for `call`, rolled back: the plan
/// texts and the shared buffers (hit + read) of their top nodes.
async fn explain(client: &Client, candidate: Candidate, call: &Call) -> (Vec<String>, u64) {
    let statements: Vec<(String, Vec<Param<'_>>)> = match candidate {
        Candidate::A => vec![
            (
                insert_pairs(REL),
                vec![param(&call.added.0), param(&call.added.1)],
            ),
            (
                delete_pairs(REL),
                vec![param(&call.removed.0), param(&call.removed.1)],
            ),
        ],
        Candidate::B => vec![(
            candidate_b(REL),
            vec![
                param(&call.added.0),
                param(&call.added.1),
                param(&call.removed.0),
                param(&call.removed.1),
            ],
        )],
        Candidate::C => vec![(
            candidate_c(REL),
            vec![
                param(&call.new_set.0),
                param(&call.new_set.1),
                param(&call.parents),
            ],
        )],
    };
    client.batch_execute("BEGIN").await.unwrap();
    let mut plans = Vec::new();
    let mut buffers = 0;
    for (sql, params) in statements {
        let rows = client
            .query(
                &format!("EXPLAIN (ANALYZE, BUFFERS, COSTS OFF) {sql}"),
                &params,
            )
            .await
            .unwrap();
        let text: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
        // the first `Buffers:` line is the top node's, which includes its children; a CTE
        // that the main query does not read (B's `DELETE`) runs after it and has its own top
        // node, so the first `Buffers:` line after each `CTE` line counts as well
        let mut want = true;
        for line in &text {
            let line = line.trim_start();
            if line.starts_with("CTE ") {
                want = true;
            } else if want && line.starts_with("Buffers: shared") {
                want = false;
                for part in line.split_whitespace() {
                    for key in ["hit=", "read="] {
                        if let Some(n) = part.strip_prefix(key) {
                            buffers += n.parse::<u64>().unwrap();
                        }
                    }
                }
            }
        }
        plans.push(text.join("\n"));
    }
    client.batch_execute("ROLLBACK").await.unwrap();
    (plans, buffers)
}

/// Nearest-rank percentile of sorted samples.
fn percentile(sorted: &[u64], q: f64) -> u64 {
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn ms(micros: u64) -> String {
    format!("{:.3}", micros as f64 / 1000.0)
}

/// The host's load average (the client and the database server share the host).
fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|e| format!("unreadable ({e})"))
}

/// The CPU model and count of the host.
fn cpu() -> String {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let model = info
        .lines()
        .find_map(|l| l.strip_prefix("model name"))
        .map(|l| l.trim_start_matches([' ', '\t', ':']).to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let count = info.lines().filter(|l| l.starts_with("processor")).count();
    format!("{model}, {count} logical CPUs")
}

#[tokio::test]
#[ignore = "measurement: run on a build host, see the module documentation"]
async fn bench_m2m_set_candidates() {
    let sizes = env_list("ORMX_M2M_SIZES", "200000,10000000");
    let runs = env_list("ORMX_M2M_RUNS", "30")[0] as usize;
    let c_runs_large = env_list("ORMX_M2M_C_RUNS_LARGE", "5")[0] as usize;
    let db = TestDb::builder()
        .database_url(database_url_from_env().unwrap())
        .create()
        .await
        .unwrap();
    let client = db.client().await.unwrap();
    let version: String = client
        .query_one("SELECT version()", &[])
        .await
        .unwrap()
        .get(0);
    let settings: Vec<String> = client
        .query(
            "SELECT name || '=' || current_setting(name) FROM pg_settings WHERE name IN \
             ('shared_buffers', 'work_mem', 'effective_cache_size', 'random_page_cost', \
             'jit', 'max_parallel_workers_per_gather') ORDER BY name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    eprintln!("server: {version}");
    eprintln!("settings: {}", settings.join(", "));
    eprintln!("host: {}", cpu());
    eprintln!("load average at start: {}", load_average());
    let mut report = vec![
        "| rows | parents/call | candidate | statements | runs | p50 ms | p95 ms | p50 vs A | p95 vs A \
         | shared buffers | relation scan |"
            .to_owned(),
        "|---|---|---|---|---|---|---|---|---|---|---|".to_owned(),
    ];
    let mut verdicts: Vec<String> = Vec::new();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for &rows in &sizes {
        let parents = rows / TARGETS_PER_PARENT;
        // the ORM's relation table shape (`create_relation_table`), keys built after the load
        let t = erp_sql::quote(REL.table);
        client
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS {t}; \
                 CREATE TABLE {t} (\"c1\" int8 NOT NULL, \"c2\" int8 NOT NULL); \
                 INSERT INTO {t} SELECT p, (p * 7919 + j * 104729) % {PRIME} + 1 \
                   FROM generate_series(1::int8, {parents}) AS p, generate_series(0, {}) AS j; \
                 ALTER TABLE {t} ADD PRIMARY KEY (\"c1\", \"c2\"); \
                 CREATE INDEX {} ON {t} (\"c2\", \"c1\")",
                TARGETS_PER_PARENT - 1,
                erp_sql::quote(&erp_sql::relational::relation_index_name(REL)),
            ))
            .await
            .unwrap();
        client
            .batch_execute(&format!("VACUUM (ANALYZE) {t}"))
            .await
            .unwrap();
        let count: i64 = client
            .query_one(&format!("SELECT count(*) FROM {t}"), &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, rows);
        let sql = Statements::prepare(&client).await;
        for &batch in &BATCHES {
            let order = [Candidate::A, Candidate::B, Candidate::C];
            // C reads the whole relation table on every call (a FULL join, see the plans): on the
            // large table it gets `ORMX_M2M_C_RUNS_LARGE` runs (default 5) and one warm-up
            let plan_of = |candidate: Candidate| -> (usize, usize) {
                if candidate == Candidate::C && rows >= LARGE {
                    (c_runs_large, 1)
                } else {
                    (runs, 3)
                }
            };
            // warm-up: caches and the prepared statements' plans
            for round in 0..3 {
                for candidate in order {
                    if round < plan_of(candidate).1 {
                        let call = Call::new(rng.parents(batch, parents));
                        timed(&client, &sql, candidate, &call).await;
                    }
                }
            }
            let mut samples: [Vec<u64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            for run in 0..runs {
                for k in 0..3 {
                    let i = (run + k) % 3;
                    if samples[i].len() < plan_of(order[i]).0 {
                        let call = Call::new(rng.parents(batch, parents));
                        samples[i].push(timed(&client, &sql, order[i], &call).await);
                    }
                }
            }
            let mut stats = Vec::new();
            for (i, candidate) in order.iter().enumerate() {
                let mut s = samples[i].clone();
                s.sort_unstable();
                let (p50, p95) = (percentile(&s, 0.5), percentile(&s, 0.95));
                let call = Call::new(rng.parents(batch, parents));
                let (plans, buffers) = explain(&client, *candidate, &call).await;
                let relation_scan = if plans
                    .iter()
                    .any(|p| p.contains(&format!("Seq Scan on {}", REL.table)))
                {
                    "seq scan"
                } else {
                    "index"
                };
                if batch == 100 || relation_scan == "seq scan" {
                    eprintln!(
                        "--- plan {candidate:?}, {rows} rows, {batch} parents:\n{}",
                        plans.join("\n---\n")
                    );
                }
                stats.push((*candidate, p50, p95, buffers, relation_scan));
            }
            let (_, a50, a95, _, _) = stats[0];
            for (candidate, p50, p95, buffers, scan) in &stats {
                let rel =
                    |x: u64, base: u64| format!("{:+.1}%", (x as f64 / base as f64 - 1.0) * 100.0);
                let statements = if *candidate == Candidate::A { 2 } else { 1 };
                let n = samples[order.iter().position(|c| c == candidate).unwrap()].len();
                report.push(format!(
                    "| {rows} | {batch} | {candidate:?} | {statements} | {n} | {} | {} | {} | {} | {buffers} | {scan} |",
                    ms(*p50),
                    ms(*p95),
                    rel(*p50, a50),
                    rel(*p95, a95),
                ));
                if *candidate != Candidate::A {
                    let wins = (*p50 as f64) <= a50 as f64 * 0.9;
                    let p95_ok = (*p95 as f64) <= a95 as f64 * 1.05;
                    let scan_ok = *scan == "index";
                    verdicts.push(format!(
                        "{candidate:?} at {rows} rows, {batch} parents: p50 ≥10% better: {wins}, \
                         p95 within +5%: {p95_ok}, index-only access: {scan_ok}"
                    ));
                }
            }
            eprintln!("done: {rows} rows, {batch} parents/call");
        }
        client
            .batch_execute(&format!("DROP TABLE {t}"))
            .await
            .unwrap();
    }
    eprintln!("load average at end: {}", load_average());
    eprintln!("\n{}\n", report.join("\n"));
    eprintln!("{}", verdicts.join("\n"));
    drop(client);
    db.cleanup().await.unwrap();
}
