//! measurement at many databases (open question 1).
//!
//! ```text
//! SIGNAL_SCALE_DBS=100 SIGNAL_SCALE_WRITERS=80 SIGNAL_SCALE_SECS=8 \
//!   cargo nextest run -p erp-test-models --test signal_scale --run-ignored only --no-capture
//! ```
//!
//! It creates `SIGNAL_SCALE_DBS` throwaway databases with the signal table and, per phase, runs
//! closed-loop and paced writer transactions in `SIGNAL_SCALE_WRITERS` of them with and without the
//! one NOTIFY per transaction, with 0 and with one real listener per database. It also measures
//! the wake-up latency (commit to applied) and the cost of the generation read that rides `BEGIN`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "measurement tool"
)]

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use erp_fields::Result;
use erp_orm::{Generations, ListenerOptions, Orm, Signal, SignalKey, StaticPolicy, Transaction};
use erp_test_models::base_users;
use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use erp_testkit::tokio_postgres::{Config, NoTls};
use erp_testkit::{Harness, TestDb, database_url_from_env};

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn config_for(db: &str) -> Config {
    let mut config = Config::from_str(&database_url_from_env().unwrap()).unwrap();
    config.dbname(db);
    config
}

fn pool_for(config: Config, size: usize) -> Pool {
    Pool::builder(Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    ))
    .max_size(size)
    .runtime(Runtime::Tokio1)
    .build()
    .unwrap()
}

struct Stat {
    n: usize,
    p50: u64,
    p95: u64,
    p99: u64,
}

fn stat(mut micros: Vec<u64>) -> Stat {
    micros.sort_unstable();
    let at = |q: f64| {
        if micros.is_empty() {
            0
        } else {
            micros[(((micros.len() - 1) as f64) * q) as usize]
        }
    };
    Stat {
        n: micros.len(),
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
    }
}

struct Writers {
    commit_us: Arc<Mutex<Vec<u64>>>,
    tx_us: Arc<Mutex<Vec<u64>>>,
}

/// Run one writer per pool for `secs`, each looping `BEGIN; INSERT [; pg_notify]; COMMIT`.
async fn run_writers(
    pools: &[Pool],
    notify: bool,
    pace: Option<Duration>,
    secs: u64,
) -> (Writers, f64) {
    let stop = Arc::new(AtomicBool::new(false));
    let out = Writers {
        commit_us: Arc::new(Mutex::new(Vec::new())),
        tx_us: Arc::new(Mutex::new(Vec::new())),
    };
    let mut tasks = Vec::new();
    let count = u32::try_from(pools.len()).unwrap().max(1);
    for (i, pool) in pools.iter().enumerate() {
        let client = pool.get().await.unwrap();
        // spread the paced writers over the period: the same start would make them commit together
        let offset = pace.map_or(Duration::ZERO, |p| p * u32::try_from(i).unwrap() / count);
        let stop = Arc::clone(&stop);
        let commit_us = Arc::clone(&out.commit_us);
        let tx_us = Arc::clone(&out.tx_us);
        tasks.push(tokio::spawn(async move {
            let insert = client
                .prepare("INSERT INTO erp_signal_generation (key) VALUES ('cache:bench')")
                .await
                .unwrap();
            let ping = client
                .prepare("SELECT pg_notify('erp_signal', '')")
                .await
                .unwrap();
            let mut local_commit = Vec::new();
            let mut local_tx = Vec::new();
            tokio::time::sleep(offset).await;
            let mut tick = pace.map(|p| {
                let mut i = tokio::time::interval(p);
                i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                i
            });
            while !stop.load(Ordering::Relaxed) {
                if let Some(tick) = &mut tick {
                    tick.tick().await;
                }
                let t0 = Instant::now();
                client.batch_execute("BEGIN").await.unwrap();
                client.execute(&insert, &[]).await.unwrap();
                if notify {
                    client.execute(&ping, &[]).await.unwrap();
                }
                let t1 = Instant::now();
                client.batch_execute("COMMIT").await.unwrap();
                let t2 = Instant::now();
                local_commit.push((t2 - t1).as_micros() as u64);
                local_tx.push((t2 - t0).as_micros() as u64);
            }
            commit_us.lock().unwrap().extend(local_commit);
            tx_us.lock().unwrap().extend(local_tx);
        }));
    }
    let started = Instant::now();
    tokio::time::sleep(Duration::from_secs(secs)).await;
    stop.store(true, Ordering::Relaxed);
    for t in tasks {
        t.await.unwrap();
    }
    let elapsed = started.elapsed().as_secs_f64();
    (out, elapsed)
}

fn cpu_sample() -> String {
    std::process::Command::new("docker")
        .args([
            "stats",
            "--no-stream",
            "--format",
            "{{.CPUPerc}}",
            "erpforall-pg",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "n/a (docker stats unavailable)".to_owned(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        )
}

fn report(label: &str, writers: &Writers, elapsed: f64, cpu: &str) {
    let commit = stat(writers.commit_us.lock().unwrap().clone());
    let total = stat(writers.tx_us.lock().unwrap().clone());
    println!(
        "| {label} | {:.0} | {} / {} / {} | {} / {} / {} | {cpu} |",
        commit.n as f64 / elapsed,
        commit.p50,
        commit.p95,
        commit.p99,
        total.p50,
        total.p95,
        total.p99,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "measurement: run on a build host, see the module documentation"]
async fn measure_many_databases() {
    let dbs = env_usize("SIGNAL_SCALE_DBS", 100);
    let writers = env_usize("SIGNAL_SCALE_WRITERS", 80).min(dbs);
    let secs = env_usize("SIGNAL_SCALE_SECS", 8) as u64;
    let registry = Harness::builder([&base_users::ADDON])
        .registry_only()
        .unwrap();

    // create the databases and install the table
    let setup = Instant::now();
    let mut created = Vec::new();
    for _ in 0..dbs {
        created.push(TestDb::builder().pool_size(2).create().await.unwrap());
    }
    let mut writer_pools = Vec::new();
    let mut orms = Vec::new();
    for db in &created {
        let plain = Orm::builder(Arc::clone(&registry), db.pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap();
        let mut tx = plain.begin(erp_orm::SUPERUSER_ID).await.unwrap();
        tx.install_signal().await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        writer_pools.push(pool_for(config_for(db.name()), 1));
        let signal = Signal::new(Generations::new());
        orms.push(
            Orm::builder(Arc::clone(&registry), db.pool().clone())
                .policy(StaticPolicy::AllowAll)
                .signal(signal)
                .build()
                .unwrap(),
        );
    }
    let writer_pools = &writer_pools[..writers];
    println!(
        "setup of {dbs} databases took {:.1} s",
        setup.elapsed().as_secs_f64()
    );
    {
        let (client, connection) = config_for(&database_name_from_env())
            .connect(NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        for setting in [
            "synchronous_commit",
            "fsync",
            "max_connections",
            "wal_level",
            "shared_buffers",
        ] {
            let row = client
                .query_one(&format!("SHOW {setting}"), &[])
                .await
                .unwrap();
            println!("server setting {setting} = {}", row.get::<_, String>(0));
        }
    }

    println!("\nM3.SIGNAL measurement: {dbs} databases, writers in {writers}, {secs} s per phase");
    println!("| phase | commits/s | commit us p50/p95/p99 | tx us p50/p95/p99 | pg cpu |");
    println!("|---|---|---|---|---|");
    for (pace_label, pace) in [
        ("closed loop", None),
        ("paced 2/s per db", Some(Duration::from_millis(500))),
    ] {
        for listeners in [false, true] {
            let mut handles = Vec::new();
            if listeners {
                for (orm, db) in orms.iter().zip(&created) {
                    let options =
                        ListenerOptions::default().poll_interval(Duration::from_secs(600));
                    handles.push(
                        orm.spawn_signal_listener(config_for(db.name()), NoTls, options)
                            .unwrap(),
                    );
                }
                for orm in &orms {
                    while !orm.signal().unwrap().stats().listener_up {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
            for notify in [false, true] {
                let sampler = tokio::spawn({
                    let delay = Duration::from_secs(secs / 2);
                    async move {
                        tokio::time::sleep(delay).await;
                        tokio::task::spawn_blocking(cpu_sample).await.unwrap()
                    }
                });
                let (w, elapsed) = run_writers(writer_pools, notify, pace, secs).await;
                let cpu = sampler.await.unwrap();
                let label = format!(
                    "{pace_label}, {} listeners, {}",
                    if listeners { dbs } else { 0 },
                    if notify { "NOTIFY" } else { "no NOTIFY" }
                );
                report(&label, &w, elapsed, &cpu);
            }
            for h in handles {
                h.shutdown().await;
            }
        }
    }

    wake_latency(&orms, &created, &writer_pools[..writers.min(dbs)]).await;
    connections().await;

    for db in created {
        db.cleanup().await.unwrap();
    }
}

/// Commit to applied on database 0 while the other writers run paced with NOTIFY.
async fn wake_latency(orms: &[Orm], created: &[TestDb], pools: &[Pool]) {
    let options = ListenerOptions::default().poll_interval(Duration::from_secs(600));
    let mut handles = Vec::new();
    for (orm, db) in orms.iter().zip(created) {
        handles.push(
            orm.spawn_signal_listener(config_for(db.name()), NoTls, options)
                .unwrap(),
        );
    }
    for orm in orms {
        while !orm.signal().unwrap().stats().listener_up {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let background = tokio::spawn({
        let pools: Vec<Pool> = pools.iter().skip(1).cloned().collect();
        async move {
            run_writers(&pools, true, Some(Duration::from_millis(500)), 12)
                .await
                .1
        }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = SignalKey::cache("bench").unwrap();
    let signal = orms[0].signal().unwrap();
    let client = pools[0].get().await.unwrap();
    let mut samples = Vec::new();
    for _ in 0..200 {
        let target = signal.applied().get(&key) + 1;
        let t0 = Instant::now();
        client.batch_execute("BEGIN").await.unwrap();
        client
            .batch_execute("INSERT INTO erp_signal_generation (key) VALUES ('cache:bench')")
            .await
            .unwrap();
        client
            .batch_execute("SELECT pg_notify('erp_signal', '')")
            .await
            .unwrap();
        client.batch_execute("COMMIT").await.unwrap();
        while signal.applied().get(&key) < target {
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
        samples.push(t0.elapsed().as_micros() as u64);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = stat(samples);
    println!(
        "\nwake-up latency, commit start to applied in the listening process (n={}): p50 {} us, p95 {} us, p99 {} us",
        s.n, s.p50, s.p95, s.p99
    );
    background.await.unwrap();
    for h in handles {
        h.shutdown().await;
    }
}

/// What the generation read costs inside `BEGIN`, by the number of rows it aggregates: the
/// minimum over alternating rounds of plain and signalled transactions (the host is shared, so
/// single runs are noisy).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement: run on a build host, see the module documentation"]
async fn read_cost_by_rows() {
    let registry = Harness::builder([&base_users::ADDON])
        .registry_only()
        .unwrap();
    let db = TestDb::builder().pool_size(2).create().await.unwrap();
    let plain = Orm::builder(Arc::clone(&registry), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = plain.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let client = db.client().await.unwrap();
    println!("\ngeneration read inside BEGIN, min of 7 alternating rounds of 1000 transactions:");
    println!("| rows of the key | plain us/tx | with read us/tx | overhead us |");
    println!("|---|---|---|---|");
    for target_rows in [1i64, 100, 1_000, 10_000] {
        let have: i64 = client
            .query_one(
                "SELECT count(*) FROM erp_signal_generation WHERE key = 'cache:bench'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if target_rows > have {
            client
                .execute(
                    "INSERT INTO erp_signal_generation (key) \
                     SELECT 'cache:bench' FROM generate_series(1, $1::int8)",
                    &[&(target_rows - have)],
                )
                .await
                .unwrap();
        }
        client
            .batch_execute("VACUUM erp_signal_generation")
            .await
            .unwrap();
        let with = Orm::builder(Arc::clone(&registry), db.pool().clone())
            .policy(StaticPolicy::AllowAll)
            .signal(Signal::new(Signal::read(&client).await.unwrap()))
            .build()
            .unwrap();
        let (mut a, mut b) = (f64::MAX, f64::MAX);
        for _ in 0..7 {
            a = a.min(time_transactions(&plain).await);
            b = b.min(time_transactions(&with).await);
        }
        println!("| {target_rows} | {a:.0} | {b:.0} | {:.0} |", b - a);
    }
    drop(client);
    db.cleanup().await.unwrap();
}

async fn time_transactions(orm: &Orm) -> f64 {
    const N: u32 = 1_000;
    // warm up
    one(orm).await.unwrap();
    let t0 = Instant::now();
    for _ in 0..N {
        one(orm).await.unwrap();
    }
    t0.elapsed().as_micros() as f64 / f64::from(N)
}

async fn one(orm: &Orm) -> Result<()> {
    let mut tx: Transaction = orm.begin(erp_orm::SUPERUSER_ID).await?;
    tx.query("SELECT 1", &[]).await?;
    tx.commit().await
}

async fn connections() {
    let (client, connection) = config_for(&database_name_from_env())
        .connect(NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    let row = client
        .query_one(
            "SELECT count(*), current_setting('max_connections') FROM pg_stat_activity",
            &[],
        )
        .await
        .unwrap();
    println!(
        "\nserver backends at the end: {} (max_connections {})",
        row.get::<_, i64>(0),
        row.get::<_, String>(1)
    );
}

fn database_name_from_env() -> String {
    Config::from_str(&database_url_from_env().unwrap())
        .unwrap()
        .get_dbname()
        .unwrap()
        .to_owned()
}

/// Where the cost of the generation read goes: raw driver loops over one connection, serial and
/// pipelined, with the prepared read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement: run on a build host, see the module documentation"]
async fn probe_read_cost_raw() {
    let db = TestDb::builder().pool_size(2).create().await.unwrap();
    let client = db.client().await.unwrap();
    client
        .batch_execute(
            "CREATE TABLE erp_signal_generation (id uuid PRIMARY KEY DEFAULT uuidv7(), \
             key text NOT NULL, weight int8 NOT NULL DEFAULT 1, \
             created_at timestamptz NOT NULL DEFAULT now()); \
             CREATE INDEX ON erp_signal_generation (key) INCLUDE (weight); \
             INSERT INTO erp_signal_generation (key) VALUES ('registry')",
        )
        .await
        .unwrap();
    client
        .batch_execute("VACUUM erp_signal_generation")
        .await
        .unwrap();
    let read = client
        .prepare("SELECT key, sum(weight)::int8 FROM erp_signal_generation GROUP BY key")
        .await
        .unwrap();
    let one = client.prepare("SELECT 1").await.unwrap();
    const N: u32 = 3_000;
    let mut results = Vec::new();
    for round in 0..2 {
        for variant in ["plain", "serial read", "pipelined read"] {
            let t0 = Instant::now();
            for _ in 0..N {
                match variant {
                    "plain" => {
                        client
                            .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
                            .await
                            .unwrap();
                    }
                    "serial read" => {
                        client
                            .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
                            .await
                            .unwrap();
                        client.query(&read, &[]).await.unwrap();
                    }
                    _ => {
                        let (b, r) = tokio::join!(
                            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ"),
                            client.query(&read, &[])
                        );
                        b.unwrap();
                        r.unwrap();
                    }
                }
                client.query(&one, &[]).await.unwrap();
                client.batch_execute("COMMIT").await.unwrap();
            }
            let us = t0.elapsed().as_micros() as f64 / f64::from(N);
            if round == 1 {
                results.push((variant, us));
            }
        }
    }
    println!("\nraw driver, us per transaction (BEGIN [read] SELECT 1 COMMIT):");
    for (variant, us) in results {
        println!("| {variant} | {us:.0} |");
    }
    // the same through the ORM's Transaction
    let registry = Harness::builder([&base_users::ADDON])
        .registry_only()
        .unwrap();
    let plain = Orm::builder(Arc::clone(&registry), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let with = Orm::builder(Arc::clone(&registry), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(Signal::read(&client).await.unwrap()))
        .build()
        .unwrap();
    for _ in 0..2 {
        let a = time_transactions(&plain).await;
        let b = time_transactions(&with).await;
        println!("orm: plain {a:.0} us, with read {b:.0} us");
    }
    drop(client);
    db.cleanup().await.unwrap();
}
