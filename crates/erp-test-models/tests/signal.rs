//! generation counters, the opening round trip, staleness handling, payload-free
//! NOTIFY and the listener.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use erp_fields::{Error, ErrorKind, Result, Value};
use erp_orm::{
    BoxFuture, Changes, Context, Generations, ListenerOptions, NotifyChannel, Orm, OrmCacheKey,
    Refresh, Refreshed, SelfApplied, Signal, SignalKey, StaticPolicy, Transaction,
};
use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use erp_testkit::tokio_postgres::{AsyncMessage, Config, NoTls};
use erp_testkit::{Harness, database_url_from_env};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------------------------
// helpers

fn registry_key() -> SignalKey {
    SignalKey::Registry
}

/// The connection config of the harness database.
fn db_config(h: &Harness) -> Config {
    let mut config = Config::from_str(&database_url_from_env().unwrap()).unwrap();
    config.dbname(h.db().name());
    config
}

fn pool_for(config: Config, size: usize) -> Pool {
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(size)
        .runtime(Runtime::Tokio1)
        .build()
        .unwrap()
}

async fn install(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
}

/// Commit and drop, so the connection goes back to the pool (a committed `Transaction` keeps it).
async fn finish(mut tx: Transaction) {
    tx.commit().await.unwrap();
}

async fn current(h: &Harness) -> Generations {
    let client = h.db().client().await.unwrap();
    Signal::read(&client).await.unwrap()
}

/// What another process does: bump a key in its own committed transaction (a plain insert).
async fn foreign_bump(h: &Harness, key: &SignalKey) {
    let client = h.db().client().await.unwrap();
    client
        .execute(
            "INSERT INTO erp_signal_generation (key) VALUES ($1)",
            &[&key.to_key_string()],
        )
        .await
        .unwrap();
}

async fn rows(h: &Harness) -> i64 {
    let client = h.db().client().await.unwrap();
    client
        .query_one("SELECT count(*) FROM erp_signal_generation", &[])
        .await
        .unwrap()
        .get(0)
}

/// A refresh hook that records its calls and plays back scripted outcomes.
#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
    outcomes: Mutex<VecDeque<Result<Refreshed>>>,
    seen: Mutex<Vec<Changes>>,
    delay: Duration,
}

impl Fake {
    fn with_delay(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            delay,
            ..Self::default()
        })
    }

    fn script(&self, outcome: Result<Refreshed>) {
        self.outcomes.lock().unwrap().push_back(outcome);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

struct Hook(Arc<Fake>);

impl Refresh for Hook {
    fn refresh<'a>(&'a self, changes: &'a Changes) -> BoxFuture<'a, Result<Refreshed>> {
        Box::pin(async move {
            self.0.calls.fetch_add(1, Ordering::SeqCst);
            self.0.seen.lock().unwrap().push(changes.clone());
            tokio::time::sleep(self.0.delay).await;
            self.0
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Refreshed::InPlace))
        })
    }
}

/// An ORM with signals on the harness database, applied baseline = what the table holds now.
async fn signal_orm(h: &Harness, hook: Option<&Arc<Fake>>, pool_size: usize) -> Orm {
    let baseline = current(h).await;
    let mut signal = Signal::new(baseline);
    if let Some(hook) = hook {
        signal = signal.with_refresh(Hook(Arc::clone(hook)));
    }
    Orm::builder(Arc::clone(h.registry()), pool_for(db_config(h), pool_size))
        .policy(StaticPolicy::AllowAll)
        .signal(signal)
        .build()
        .unwrap()
}

async fn first_statement(orm: &Orm) -> Result<Transaction> {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await?;
    tx.query("SELECT 1", &[]).await?;
    Ok(tx)
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..300 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// One protocol message seen by the relay, in arrival order across both directions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    /// Sent by the server (otherwise by the client).
    server: bool,
    kind: u8,
    text: String,
}

type EventLog = Arc<Mutex<Vec<Event>>>;

/// A TCP relay that logs the protocol messages of both directions by type, in one ordered list.
struct Proxy {
    port: u16,
    log: EventLog,
}

impl Proxy {
    async fn start(config: &Config) -> Self {
        let Some(erp_testkit::tokio_postgres::config::Host::Tcp(host)) = config.get_hosts().first()
        else {
            panic!("the test needs a TCP host in DATABASE_URL");
        };
        let target = (
            host.clone(),
            config.get_ports().first().copied().unwrap_or(5432),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: EventLog = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&log);
        tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    return;
                };
                let Ok(server) = TcpStream::connect(target.clone()).await else {
                    return;
                };
                tokio::spawn(relay(client, server, Arc::clone(&shared)));
            }
        });
        Self { port, log }
    }

    fn clear(&self) {
        self.log.lock().unwrap().clear();
    }

    fn events(&self) -> Vec<Event> {
        self.log.lock().unwrap().clone()
    }

    fn config(&self, base: &Config) -> Config {
        let mut config = Config::new();
        config
            .host("127.0.0.1")
            .port(self.port)
            .user(base.get_user().unwrap())
            .dbname(base.get_dbname().unwrap());
        if let Some(password) = base.get_password() {
            config.password(password);
        }
        config
    }
}

/// Split `pending` into typed messages (`type`, int32 length, body) and log them.
fn drain_messages(pending: &mut Vec<u8>, server: bool, log: &EventLog) {
    loop {
        if pending.len() < 5 {
            return;
        }
        let len = u32::from_be_bytes(pending[1..5].try_into().unwrap()) as usize;
        if pending.len() < 1 + len {
            return;
        }
        let text = String::from_utf8_lossy(&pending[5..1 + len])
            .trim_end_matches('\0')
            .to_owned();
        log.lock().unwrap().push(Event {
            server,
            kind: pending[0],
            text,
        });
        pending.drain(..1 + len);
    }
}

async fn relay(client: TcpStream, server: TcpStream, log: EventLog) {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    let down_log = Arc::clone(&log);
    let down = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        let mut pending = Vec::new();
        loop {
            match server_read.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if client_write.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                    pending.extend_from_slice(&buf[..n]);
                    drain_messages(&mut pending, true, &down_log);
                }
            }
        }
    });
    let mut pending: Vec<u8> = Vec::new();
    let mut started = false;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match client_read.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if server_write.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                pending.extend_from_slice(&buf[..n]);
                if !started {
                    // the startup packet has no type byte
                    if pending.len() < 4 {
                        continue;
                    }
                    let len = u32::from_be_bytes(pending[..4].try_into().unwrap()) as usize;
                    if pending.len() < len {
                        continue;
                    }
                    pending.drain(..len);
                    started = true;
                }
                drain_messages(&mut pending, false, &log);
            }
        }
    }
    down.abort();
}

// ---------------------------------------------------------------------------------------------
// counters

/// a bump adds one to the key's generation; two bumps of a key in one transaction add
/// one; a rolled-back transaction adds nothing; keys are independent.
#[tokio::test]
async fn bump_counts_once_per_transaction_and_rolls_back() {
    let h = support::orm().await;
    install(&h).await;
    assert_eq!(current(&h).await, Generations::new());

    let mut tx = h.begin().await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    tx.signal_bump(&SignalKey::Policy, SelfApplied::No)
        .await
        .unwrap();
    assert_eq!(rows_in(&mut tx).await, 2);
    finish(tx).await;
    let after = current(&h).await;
    assert_eq!(after.get(&registry_key()), 1);
    assert_eq!(after.get(&SignalKey::Policy), 1);
    assert_eq!(after.get(&SignalKey::cache("x").unwrap()), 0);

    let mut tx = h.begin().await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(current(&h).await.get(&registry_key()), 1);

    let mut tx = h.begin().await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    finish(tx).await;
    assert_eq!(current(&h).await.get(&registry_key()), 2);
    h.cleanup().await.unwrap();
}

async fn rows_in(tx: &mut Transaction) -> i64 {
    tx.query("SELECT count(*) FROM erp_signal_generation", &[])
        .await
        .unwrap()[0]
        .get(0)
}

/// the case `max(id)` loses. A and B bump; B (the later insert) commits first, then
/// A. After B the reader sees 1, after A it sees 2: no commit order hides a change.
#[tokio::test]
async fn out_of_order_commits_are_both_seen() {
    let h = support::orm().await;
    install(&h).await;
    let mut a = h.begin().await.unwrap();
    let mut b = h.begin().await.unwrap();
    a.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    b.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    b.commit().await.unwrap();
    assert_eq!(current(&h).await.get(&registry_key()), 1);
    a.commit().await.unwrap();
    assert_eq!(current(&h).await.get(&registry_key()), 2);
    h.cleanup().await.unwrap();
}

/// concurrent bumpers of one key under REPEATABLE READ neither conflict nor lose a
/// bump (a `max + 1` design fails here with unique violations).
#[tokio::test]
async fn concurrent_bumps_are_exact() {
    let h = support::orm().await;
    install(&h).await;
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let orm = h.orm().clone();
        tasks.push(tokio::spawn(async move {
            orm.run(erp_orm::SUPERUSER_ID, Context::new(), async |tx| {
                tx.signal_bump(&SignalKey::Registry, SelfApplied::No).await
            })
            .await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(current(&h).await.get(&registry_key()), 16);
    h.cleanup().await.unwrap();
}

/// compaction replaces old rows by one summary row, keeps every generation, leaves a
/// row inserted after its snapshot alone, and does nothing for fewer than two rows.
#[tokio::test]
async fn compaction_keeps_generations() {
    let h = support::orm().await;
    install(&h).await;
    let policy = SignalKey::Policy;
    for _ in 0..3 {
        let mut tx = h.begin().await.unwrap();
        tx.signal_bump(&registry_key(), SelfApplied::No)
            .await
            .unwrap();
        finish(tx).await;
    }
    let mut tx = h.begin().await.unwrap();
    tx.signal_bump(&policy, SelfApplied::No).await.unwrap();
    finish(tx).await;

    // a single qualifying row is left alone
    let mut tx = h.begin().await.unwrap();
    assert_eq!(tx.signal_compact(&policy, Duration::ZERO).await.unwrap(), 0);
    finish(tx).await;
    assert_eq!(rows(&h).await, 4);

    // compaction starts (snapshot), a bump commits meanwhile, compaction finishes
    let mut compactor = h.begin().await.unwrap();
    compactor.signal_generations().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    finish(tx).await;
    let removed = compactor
        .signal_compact(&registry_key(), Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(removed, 3);
    finish(compactor).await;
    let after = current(&h).await;
    assert_eq!(after.get(&registry_key()), 4);
    assert_eq!(after.get(&policy), 1);
    // summary row + the late bump + the policy row
    assert_eq!(rows(&h).await, 3);
    h.cleanup().await.unwrap();
}

/// the table refuses keys that are not `registry`, `policy` or `cache:<name>`.
#[tokio::test]
async fn table_rejects_junk_keys() {
    let h = support::orm().await;
    install(&h).await;
    let client = h.db().client().await.unwrap();
    for bad in ["Registry", "cache:", "cache:UP", "other"] {
        let err = client
            .execute(
                "INSERT INTO erp_signal_generation (key) VALUES ($1)",
                &[&bad],
            )
            .await
            .unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("23514"), "{bad:?}");
    }
    drop(client);
    h.cleanup().await.unwrap();
}

/// a missing table is reported with the way to fix it, not as a raw 42P01.
#[tokio::test]
async fn missing_table_names_the_fix() {
    let h = support::orm().await;
    let orm = signal_orm_without_table(&h).await;
    let err = first_statement(&orm).await.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the signal table erp_signal_generation is missing: run Transaction::install_signal \
         (Orm::sync_schema does it when the Orm has a Signal) before serving"
    );
    // sync_schema installs it
    assert!(orm.sync_schema().await.unwrap().is_empty());
    first_statement(&orm).await.unwrap();
    h.cleanup().await.unwrap();
}

async fn signal_orm_without_table(h: &Harness) -> Orm {
    Orm::builder(Arc::clone(h.registry()), pool_for(db_config(h), 2))
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(Generations::new()))
        .build()
        .unwrap()
}

// ---------------------------------------------------------------------------------------------
// the opening round trip

/// with a signal the first statement opens the transaction by sending `BEGIN` and the
/// prepared generation read back to back: no server answer is awaited between them and no Parse is
/// sent (the read was prepared on the connection by an earlier transaction). Without a signal the
/// next message waits for the `BEGIN` answer. Neither the read nor the BEGIN counts in
/// `query_count`.
#[tokio::test]
async fn begin_and_read_are_pipelined() {
    let h = support::orm().await;
    install(&h).await;
    let base = db_config(&h);
    let proxy = Proxy::start(&base).await;
    let pool = pool_for(proxy.config(&base), 1);

    let orm = Orm::builder(Arc::clone(h.registry()), pool.clone())
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(current(&h).await))
        .build()
        .unwrap();
    // the first transaction on the connection prepares the read; it is not the one measured
    let mut warm = first_statement(&orm).await.unwrap();
    warm.rollback().await.unwrap();
    drop(warm);

    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    proxy.clear();
    tx.query("SELECT 1", &[]).await.unwrap();
    let events = proxy.events();
    let begin = events
        .iter()
        .position(|e| !e.server && e.kind == b'Q')
        .unwrap_or_else(|| panic!("no BEGIN sent: {events:?}"));
    assert!(
        events[begin]
            .text
            .starts_with("BEGIN ISOLATION LEVEL REPEATABLE READ; SET LOCAL lock_timeout = "),
        "BEGIN carries the limits: {}",
        events[begin].text
    );
    let first_ready = events
        .iter()
        .position(|e| e.server && e.kind == b'Z')
        .unwrap_or_else(|| panic!("no answer seen: {events:?}"));
    let sync = events[begin..]
        .iter()
        .position(|e| !e.server && e.kind == b'S')
        .map(|i| i + begin)
        .unwrap_or_else(|| panic!("the read was not sent: {events:?}"));
    assert!(
        sync < first_ready,
        "BEGIN and the read must both be on the wire before the first answer: {events:?}"
    );
    let read_messages: Vec<u8> = events[begin + 1..=sync]
        .iter()
        .filter(|e| !e.server)
        .map(|e| e.kind)
        .collect();
    // an unnamed typed statement: Parse travels in the same flight, there is no prepare
    // round trip before BEGIN
    assert_eq!(read_messages.first(), Some(&b'P'), "{read_messages:?}");
    assert_eq!(read_messages.last(), Some(&b'S'), "{read_messages:?}");
    assert_eq!(read_messages.iter().filter(|k| **k == b'S').count(), 1);
    assert_eq!(tx.query_count(), 1, "only the user's statement is counted");
    tx.rollback().await.unwrap();
    drop(tx);

    // without a signal the next message waits for the BEGIN answer
    let plain = Orm::builder(Arc::clone(h.registry()), pool)
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = plain.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    proxy.clear();
    tx.query("SELECT 1", &[]).await.unwrap();
    let events = proxy.events();
    let begin = events
        .iter()
        .position(|e| !e.server && e.kind == b'Q')
        .unwrap();
    assert!(
        events[begin]
            .text
            .starts_with("BEGIN ISOLATION LEVEL REPEATABLE READ; SET LOCAL lock_timeout = "),
        "BEGIN carries the limits: {}",
        events[begin].text
    );
    let ready = events[begin..]
        .iter()
        .position(|e| e.server && e.kind == b'Z')
        .map(|i| i + begin)
        .unwrap();
    let next_frontend = events[begin + 1..]
        .iter()
        .position(|e| !e.server)
        .map(|i| i + begin + 1)
        .unwrap();
    assert!(ready < next_frontend, "{events:?}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// staleness

/// a foreign registry bump makes the next transactions refresh; N concurrent first
/// statements cause ONE refresh and all of them proceed.
#[tokio::test]
async fn concurrent_requests_share_one_refresh() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::from_millis(150));
    let orm = signal_orm(&h, Some(&fake), 4).await;
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 0, "nothing changed yet");

    foreign_bump(&h, &registry_key()).await;
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let orm = orm.clone();
        tasks.push(tokio::spawn(async move {
            let mut tx = first_statement(&orm).await?;
            tx.rollback().await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(fake.calls(), 1);
    {
        let seen = fake.seen.lock().unwrap();
        assert!(seen[0].registry && !seen[0].policy && seen[0].caches.is_empty());
        assert_eq!(seen[0].observed.get(&registry_key()), 1);
    }
    let stats = orm.signal().unwrap().stats();
    assert_eq!((stats.refreshes, stats.applies), (1, 1));
    h.cleanup().await.unwrap();
}

/// (2b): a registry change in another process clears this process's
/// statement caches before the next transaction runs.
#[tokio::test]
async fn registry_bump_clears_statement_caches() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 1).await;
    // SELECT 1 only: the generation read is an unnamed statement
    let mut tx = first_statement(&orm).await.unwrap();
    assert_eq!(tx.prepared_statements(), 1);
    tx.rollback().await.unwrap();
    drop(tx);

    // no change: the pooled connection keeps its statements
    let mut tx = first_statement(&orm).await.unwrap();
    assert_eq!(tx.prepared_statements(), 1);
    tx.rollback().await.unwrap();
    drop(tx);

    foreign_bump(&h, &registry_key()).await;
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.query("SELECT 2", &[]).await.unwrap();
    assert_eq!(
        tx.prepared_statements(),
        1,
        "only SELECT 2: the cache was emptied before the transaction ran"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// a failing refresh fails the request that triggered it, leaves the generation
/// unapplied, and the next request tries again.
#[tokio::test]
async fn failed_refresh_is_retried_by_the_next_request() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    fake.script(Err(Error::new(ErrorKind::Database, "rebuild failed")));
    let orm = signal_orm(&h, Some(&fake), 2).await;
    foreign_bump(&h, &registry_key()).await;

    let err = first_statement(&orm).await.err().unwrap();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Database, "rebuild failed")
    );
    assert_eq!(orm.signal().unwrap().applied().get(&registry_key()), 0);
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 2);
    assert_eq!(orm.signal().unwrap().applied().get(&registry_key()), 1);
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 2, "applied: no third refresh");
    h.cleanup().await.unwrap();
}

/// a registry change with no refresh hook is refused, never served stale.
#[tokio::test]
async fn registry_change_without_hook_is_refused() {
    let h = support::orm().await;
    install(&h).await;
    let orm = signal_orm(&h, None, 2).await;
    first_statement(&orm).await.unwrap();
    foreign_bump(&h, &registry_key()).await;
    let err = first_statement(&orm).await.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the registry generation moved but no signal refresh hook is installed \
         (Signal::with_refresh); refusing to serve on stale state"
    );
    h.cleanup().await.unwrap();
}

/// a refresh that replaces the Orm ends the transaction with the documented marker,
/// and every later transaction of the superseded Orm ends the same way.
#[tokio::test]
async fn superseded_orm_refuses_transactions() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    fake.script(Ok(Refreshed::Superseded));
    let orm = signal_orm(&h, Some(&fake), 2).await;
    foreign_bump(&h, &registry_key()).await;
    for _ in 0..2 {
        let err = first_statement(&orm).await.err().unwrap();
        assert!(err.is_superseded());
        assert_eq!(err.kind(), ErrorKind::Concurrency);
        assert_eq!(err.sqlstate(), Some(erp_orm::signal::SUPERSEDED_SQLSTATE));
        assert_eq!(
            err.message(),
            "the registry changed in another process and was reloaded: this Orm is superseded; \
             retry on the current one"
        );
    }
    assert!(orm.signal().unwrap().is_superseded());
    assert_eq!(fake.calls(), 1);

    // ruling (b): a database function raising the same SQLSTATE is not "superseded"
    let mut tx = h.begin().await.unwrap();
    let raised = tx
        .query(
            "DO $$ BEGIN RAISE EXCEPTION 'raised' USING ERRCODE = 'ERP01'; END $$",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(raised.sqlstate(), Some("ERP01"));
    assert!(!raised.is_superseded());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// a policy change calls the hook with `policy` set and nothing else.
#[tokio::test]
async fn policy_bump_calls_the_hook() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 2).await;
    foreign_bump(&h, &SignalKey::Policy).await;
    first_statement(&orm).await.unwrap();
    {
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].policy && !seen[0].registry && seen[0].caches.is_empty());
    }
    h.cleanup().await.unwrap();
}

/// log texts are not part)
///
/// A commit that invalidated a named cache bumps `cache:<name>`; another process clears exactly
/// that cache before its next transaction, and the bumping process does not refresh for its own
/// bump.
#[tokio::test]
async fn named_cache_invalidation_crosses_processes() {
    let h = support::orm().await;
    install(&h).await;
    let a = signal_orm(&h, None, 2).await;
    let b = signal_orm(&h, None, 2).await;
    let stable = OrmCacheKey::new("test.model", "stable", vec![]);
    let other = OrmCacheKey::new("test.model", "other", vec![]);

    let mut tx = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.ormcache_add("stable", stable.clone(), Value::Int(1))
        .await
        .unwrap();
    tx.ormcache_add("other", other.clone(), Value::Int(2))
        .await
        .unwrap();
    finish(tx).await;
    let mut tx = first_statement(&a).await.unwrap();
    assert_eq!(
        tx.ormcache_get("stable", &stable).await.unwrap(),
        Some(Value::Int(1))
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // B invalidates "stable" and commits
    let mut tx = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.invalidate_ormcache("stable");
    finish(tx).await;
    assert_eq!(
        current(&h).await.get(&SignalKey::cache("stable").unwrap()),
        1
    );
    assert_eq!(
        current(&h).await.get(&SignalKey::cache("other").unwrap()),
        0
    );

    // B does not apply its own bump again
    first_statement(&b).await.unwrap();
    assert_eq!(b.signal().unwrap().stats().applies, 0);

    // A sees it at its next transaction
    let mut tx = first_statement(&a).await.unwrap();
    assert_eq!(tx.ormcache_get("stable", &stable).await.unwrap(), None);
    assert_eq!(
        tx.ormcache_get("other", &other).await.unwrap(),
        Some(Value::Int(2))
    );
    assert_eq!(a.signal().unwrap().stats().applies, 1);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// (pre-existing defect, found by MN-4): a named-cache lookup that is the first
/// operation of a fresh transaction sees another process's invalidation. The lookup opens the
/// transaction, whose flight reads the generations, before it reads the cache; before the fix
/// it read the cache of a transaction that had sent nothing yet and served the stale value.
/// `ormcache`, `ormcache_get` and `ormcache_add` all open first.
#[tokio::test]
async fn first_lookup_of_a_transaction_sees_a_foreign_invalidation() {
    let h = support::orm().await;
    install(&h).await;
    let a = signal_orm(&h, None, 2).await;
    let b = signal_orm(&h, None, 2).await;
    let key = OrmCacheKey::new("test.model", "stable", vec![]);

    // B caches a value and reads it back
    let mut tx = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.ormcache_add("stable", key.clone(), Value::Int(1))
        .await
        .unwrap();
    finish(tx).await;
    let mut tx = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        tx.ormcache_get("stable", &key).await.unwrap(),
        Some(Value::Int(1))
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // A invalidates it
    let mut tx = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.invalidate_ormcache("stable");
    finish(tx).await;

    // B's next transaction looks the value up before anything else: a miss, through each path
    let mut tx = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(tx.ormcache_get("stable", &key).await.unwrap(), None);
    assert_eq!(b.signal().unwrap().stats().applies, 1);
    tx.rollback().await.unwrap();
    drop(tx);

    let mut tx = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.ormcache_add("stable", key.clone(), Value::Int(2))
        .await
        .unwrap();
    finish(tx).await;
    let mut tx = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.invalidate_ormcache("stable");
    finish(tx).await;
    let mut tx = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let value = tx
        .ormcache("stable", key.clone(), async |_tx: &mut Transaction| {
            Ok(Value::Int(3))
        })
        .await
        .unwrap();
    assert_eq!(value, Value::Int(3), "computed again, not the stale 2");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `SelfApplied::Yes` spares the bumping process a refresh for its own change, but a
/// foreign bump that lands in the same window is still seen; `SelfApplied::No` refreshes.
#[tokio::test]
async fn own_bump_is_not_refreshed_but_foreign_bumps_are() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 2).await;

    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::Yes)
        .await
        .unwrap();
    finish(tx).await;
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 0, "own bump applied in memory");
    assert_eq!(orm.signal().unwrap().applied().get(&registry_key()), 1);

    // a foreign bump commits while our transaction is open
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::Yes)
        .await
        .unwrap();
    foreign_bump(&h, &registry_key()).await;
    finish(tx).await;
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 1, "the foreign bump is still detected");

    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    finish(tx).await;
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 2);
    h.cleanup().await.unwrap();
}

/// a transaction that began before the process caught up (its generation is behind the
/// applied one) must not move the applied generation when it commits its own bump: crediting it
/// would lower what the process applied and repeat a refresh, or skip one.
#[tokio::test]
async fn own_bump_of_an_older_transaction_leaves_applied_alone() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 3).await;

    let mut older = first_statement(&orm).await.unwrap(); // began at generation 0
    foreign_bump(&h, &registry_key()).await;
    foreign_bump(&h, &registry_key()).await;
    let mut newer = first_statement(&orm).await.unwrap(); // refreshes up to generation 2
    newer.rollback().await.unwrap();
    drop(newer);
    assert_eq!(fake.calls(), 1);
    assert_eq!(orm.signal().unwrap().applied().get(&registry_key()), 2);

    older
        .signal_bump(&registry_key(), SelfApplied::Yes)
        .await
        .unwrap();
    finish(older).await;
    assert_eq!(
        orm.signal().unwrap().applied().get(&registry_key()),
        2,
        "not lowered to 1, not credited"
    );
    assert_eq!(current(&h).await.get(&registry_key()), 3);
    // the next transaction refreshes for generation 3
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 2);
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// NOTIFY

/// gate: a notification carries no data, is sent at commit only, and arrives once per
/// transaction however many keys were bumped.
#[tokio::test]
async fn notification_has_empty_payload_and_arrives_once_at_commit() {
    let h = support::orm().await;
    install(&h).await;
    let (client, mut connection) = db_config(&h).connect(NoTls).await.unwrap();
    let (tx_note, mut rx_note) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = std::future::poll_fn(|cx| connection.poll_message(cx)).await {
            if let Ok(AsyncMessage::Notification(n)) = message {
                let _ = tx_note.send((n.channel().to_owned(), n.payload().to_owned()));
            }
        }
    });
    client.batch_execute("LISTEN erp_signal").await.unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rx_note.try_recv().is_err(), "nothing before the commit");
    tx.rollback().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rx_note.try_recv().is_err(), "nothing after a rollback");

    let mut tx = h.begin().await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    tx.signal_bump(&SignalKey::Policy, SelfApplied::No)
        .await
        .unwrap();
    tx.signal_bump(&SignalKey::cache("c").unwrap(), SelfApplied::No)
        .await
        .unwrap();
    finish(tx).await;
    let got = tokio::time::timeout(Duration::from_secs(5), rx_note.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, ("erp_signal".to_owned(), String::new()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        rx_note.try_recv().is_err(),
        "one notification per transaction"
    );
    h.cleanup().await.unwrap();
}

/// at most one NOTIFY statement per channel per transaction; another channel gets its
/// own; a savepoint rollback takes back what it sent, so the next bump sends again.
#[tokio::test]
async fn at_most_one_notify_statement_per_channel() {
    let h = support::orm().await;
    install(&h).await;
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    tx.signal_bump(&SignalKey::Policy, SelfApplied::No)
        .await
        .unwrap();
    assert!(!tx.notify_once(&NotifyChannel::signal()).await.unwrap());
    assert!(
        tx.notify_once(&NotifyChannel::new("erp_other").unwrap())
            .await
            .unwrap()
    );
    assert!(
        !tx.notify_once(&NotifyChannel::new("erp_other").unwrap())
            .await
            .unwrap()
    );
    let notifies = |tx: &Transaction| {
        tx.captured()
            .iter()
            .filter(|s| s.contains("pg_notify"))
            .count()
    };
    assert_eq!(notifies(&tx), 2);

    // a savepoint that bumps `cache:sp` and fails: both its insert and its NOTIFY are gone
    let failed: Result<()> = tx
        .savepoint(async |tx| {
            tx.signal_bump(&SignalKey::cache("sp").unwrap(), SelfApplied::No)
                .await?;
            tx.notify_once(&NotifyChannel::new("erp_third").unwrap())
                .await?;
            Err(Error::user("boom"))
        })
        .await;
    assert_eq!(failed.unwrap_err().message(), "boom");
    let before = notifies(&tx);
    tx.signal_bump(&SignalKey::cache("sp").unwrap(), SelfApplied::No)
        .await
        .unwrap();
    assert!(
        tx.notify_once(&NotifyChannel::new("erp_third").unwrap())
            .await
            .unwrap()
    );
    assert_eq!(notifies(&tx), before + 1, "sent again after the rollback");
    finish(tx).await;
    assert_eq!(current(&h).await.get(&SignalKey::cache("sp").unwrap()), 1);
    h.cleanup().await.unwrap();
}

/// channel names are validated; a payload cannot be given (no parameter exists).
#[tokio::test]
async fn channel_names_are_validated() {
    for bad in ["", "Erp", "erp-signal", "1erp", "erp signal; DROP TABLE x"] {
        let err = NotifyChannel::new(bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{bad:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// listener

fn fast() -> ListenerOptions {
    ListenerOptions::default()
        .poll_interval(Duration::from_millis(200))
        .backoff(Duration::from_millis(50), Duration::from_millis(200))
}

/// the listener applies a committed bump promptly (the poll interval is long, so only
/// the NOTIFY can explain it), reports itself up, and shuts down.
#[tokio::test]
async fn listener_applies_on_notification() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 2).await;
    let options = ListenerOptions::default()
        .poll_interval(Duration::from_secs(600))
        .backoff(Duration::from_millis(50), Duration::from_millis(200));
    let handle = orm
        .spawn_signal_listener(db_config(&h), NoTls, options)
        .unwrap();
    let signal = orm.signal().unwrap();
    eventually("listener up", || signal.stats().listener_up).await;
    assert_eq!(fake.calls(), 0);

    let mut tx = h.begin().await.unwrap();
    tx.signal_bump(&registry_key(), SelfApplied::No)
        .await
        .unwrap();
    finish(tx).await;
    eventually("refresh from the notification", || {
        fake.calls() == 1 && signal.applied().get(&registry_key()) == 1
    })
    .await;
    let stats = signal.stats();
    assert!(stats.notifications >= 1);
    assert_eq!(signal.applied().get(&registry_key()), 1);

    handle.shutdown().await;
    assert!(!signal.stats().listener_up);
    h.cleanup().await.unwrap();
}

/// without any NOTIFY (a plain insert) the poll interval still finds the change.
#[tokio::test]
async fn listener_polls_when_no_notification_comes() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 2).await;
    let handle = orm
        .spawn_signal_listener(db_config(&h), NoTls, fast())
        .unwrap();
    eventually("listener up", || orm.signal().unwrap().stats().listener_up).await;
    foreign_bump(&h, &registry_key()).await;
    eventually("refresh from the poll", || fake.calls() == 1).await;
    assert!(orm.signal().unwrap().stats().polls >= 1);
    handle.shutdown().await;
    h.cleanup().await.unwrap();
}

/// catch-up on reconnect. The listener's backend is terminated; a bump commits while
/// it is down; after it reconnects the change is applied, `listener_up` went false and came back,
/// and a reconnect was counted.
#[tokio::test]
async fn listener_catches_up_after_reconnect() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 2).await;
    let mut config = db_config(&h);
    config.application_name("erp_signal_test_listener");
    let options = ListenerOptions::default()
        .poll_interval(Duration::from_secs(600))
        .backoff(Duration::from_millis(600), Duration::from_millis(600));
    let handle = orm.spawn_signal_listener(config, NoTls, options).unwrap();
    let signal = orm.signal().unwrap();
    eventually("listener up", || signal.stats().listener_up).await;

    let client = h.db().client().await.unwrap();
    client
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = 'erp_signal_test_listener' \
             AND datname = current_database() AND pid <> pg_backend_pid()",
            &[],
        )
        .await
        .unwrap();
    eventually("listener down", || !signal.stats().listener_up).await;
    // let the pool poll of the down phase pass: the bump below is for the catch-up to find
    tokio::time::sleep(Duration::from_millis(150)).await;
    foreign_bump(&h, &registry_key()).await;
    eventually("catch-up refresh", || fake.calls() == 1).await;
    eventually("listener back up", || signal.stats().listener_up).await;
    assert!(signal.stats().reconnects >= 1);
    handle.shutdown().await;
    drop(client);
    h.cleanup().await.unwrap();
}

/// a listener that cannot connect is reported down, keeps the process correct by
/// polling through the pool, and stops on request.
#[tokio::test]
async fn unreachable_listener_polls_through_the_pool() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 2).await;
    let mut dead = Config::new();
    dead.host("127.0.0.1").port(1).user("nobody").dbname("none");
    let handle = orm.spawn_signal_listener(dead, NoTls, fast()).unwrap();
    foreign_bump(&h, &registry_key()).await;
    eventually("refresh through the pool", || fake.calls() == 1).await;
    let stats = orm.signal().unwrap().stats();
    assert!(!stats.listener_up);
    assert!(stats.polls >= 1);
    handle.shutdown().await;
    h.cleanup().await.unwrap();
}

/// a listener needs a Signal, and options must be non-zero.
#[tokio::test]
async fn listener_argument_errors() {
    let h = support::orm().await;
    let err = h
        .orm()
        .spawn_signal_listener(db_config(&h), NoTls, fast())
        .err()
        .unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "spawn_signal_listener: this Orm has no Signal (OrmBuilder::signal)"
    );
    install(&h).await;
    let orm = signal_orm(&h, None, 1).await;
    let err = orm
        .spawn_signal_listener(
            db_config(&h),
            NoTls,
            ListenerOptions::default().poll_interval(Duration::ZERO),
        )
        .err()
        .unwrap();
    assert_eq!(
        err.message(),
        "listener options: poll_interval, backoff_min and read_timeout must be non-zero and \
         backoff_min <= backoff_max (zero never means unlimited or immediate)"
    );
    let err = orm
        .spawn_signal_listener(db_config(&h), NoTls, fast().read_timeout(Duration::ZERO))
        .err()
        .unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    h.cleanup().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// review fixes (lead's review of 8390d04)

/// Review item 1: a transaction whose snapshot predates a foreign invalidation of a named cache
/// must not publish values of that cache, even when it first uses the cache after this process
/// applied the invalidation (the per-process generation is recorded at first use, so it alone
/// misses this). An invalidation it made itself is still published, its values are not.
#[tokio::test]
async fn values_from_a_snapshot_older_than_an_applied_invalidation_are_not_published() {
    let h = support::orm().await;
    install(&h).await;
    let a = signal_orm(&h, None, 2).await;
    let b = signal_orm(&h, None, 3).await;
    let key = OrmCacheKey::new("test.model", "stable", vec![]);

    // B's transaction opens before A's invalidation and has not used the cache yet
    let mut old = first_statement(&b).await.unwrap();
    let mut tx = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.invalidate_ormcache("stable");
    finish(tx).await;
    // another transaction of B applies the invalidation
    let mut tx = first_statement(&b).await.unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(b.signal().unwrap().stats().applies, 1);
    // the old transaction computes a value from its old snapshot and commits
    old.ormcache_add("stable", key.clone(), Value::Int(42))
        .await
        .unwrap();
    finish(old).await;
    let mut tx = first_statement(&b).await.unwrap();
    assert_eq!(tx.ormcache_get("stable", &key).await.unwrap(), None);
    tx.rollback().await.unwrap();
    drop(tx);

    // same with a transaction that also invalidated the cache itself: the invalidation is
    // published (the shared values are dropped), its own value is not
    let mut tx = first_statement(&b).await.unwrap();
    tx.ormcache_add("stable", key.clone(), Value::Int(7))
        .await
        .unwrap();
    finish(tx).await;
    let mut old = first_statement(&b).await.unwrap();
    let mut tx = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.invalidate_ormcache("stable");
    finish(tx).await;
    let mut tx = first_statement(&b).await.unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    old.invalidate_ormcache("stable");
    old.ormcache_add("stable", key.clone(), Value::Int(43))
        .await
        .unwrap();
    finish(old).await;
    let mut tx = first_statement(&b).await.unwrap();
    assert_eq!(tx.ormcache_get("stable", &key).await.unwrap(), None);
    tx.rollback().await.unwrap();
    drop(tx);

    // a transaction that began after the apply publishes as usual
    let mut tx = first_statement(&b).await.unwrap();
    tx.ormcache_add("stable", key.clone(), Value::Int(8))
        .await
        .unwrap();
    finish(tx).await;
    let mut tx = first_statement(&b).await.unwrap();
    assert_eq!(
        tx.ormcache_get("stable", &key).await.unwrap(),
        Some(Value::Int(8))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A refresh hook that blocks until the test lets it go, so a test can act while a refresh runs.
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct GateHook(Arc<Gate>);

impl Refresh for GateHook {
    fn refresh<'a>(&'a self, _changes: &'a Changes) -> BoxFuture<'a, Result<Refreshed>> {
        Box::pin(async move {
            self.0.entered.notify_one();
            self.0.release.notified().await;
            Ok(Refreshed::InPlace)
        })
    }
}

/// An ORM with signals whose refresh hook is `gate`, applied baseline = what the table holds now.
async fn gated_orm(h: &Harness, gate: &Arc<Gate>, pool_size: usize) -> Orm {
    let signal = Signal::new(current(h).await).with_refresh(GateHook(Arc::clone(gate)));
    Orm::builder(Arc::clone(h.registry()), pool_for(db_config(h), pool_size))
        .policy(StaticPolicy::AllowAll)
        .signal(signal)
        .build()
        .unwrap()
}

/// Delta review MUST: while a refresh runs (named caches already cleared, hook not finished), a
/// transaction whose snapshot predates the change and that first uses a cleared cache inside that
/// window must not publish its values: the cache generations are applied before the caches are
/// cleared, so its values read as computed on an older snapshot.
#[tokio::test]
async fn values_computed_while_a_refresh_runs_are_not_published() {
    let h = support::orm().await;
    install(&h).await;
    let gate = Arc::new(Gate::default());
    let orm = gated_orm(&h, &gate, 3).await;
    let key = OrmCacheKey::new("test.model", "stable", vec![]);

    // T opens before the foreign change and has not used the cache yet
    let mut old = first_statement(&orm).await.unwrap();
    foreign_bump(&h, &SignalKey::Policy).await;
    foreign_bump(&h, &SignalKey::cache("stable").unwrap()).await;

    // another request applies the change: clears "stable", then blocks in the hook
    let refreshing = tokio::spawn({
        let orm = orm.clone();
        async move {
            let mut tx = first_statement(&orm).await?;
            tx.rollback().await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
        .await
        .expect("the refresh hook was not entered");

    // inside the window: T computes a value from its old snapshot and commits
    old.ormcache_add("stable", key.clone(), Value::Int(42))
        .await
        .unwrap();
    finish(old).await;

    gate.release.notify_one();
    refreshing.await.unwrap().unwrap();
    let signal = orm.signal().unwrap();
    assert_eq!(signal.applied(), current(&h).await);
    assert_eq!(signal.stats().applies, 1);

    let mut tx = first_statement(&orm).await.unwrap();
    assert_eq!(
        tx.ormcache_get("stable", &key).await.unwrap(),
        None,
        "a value computed on a snapshot older than the applied invalidation was published"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Delta review MUST, regression path: while a full refresh after a regression runs (every named
/// cache already cleared), a transaction that began before it must not publish values into the
/// cleared caches, whatever generations it began at.
#[tokio::test]
async fn values_computed_while_a_regression_refresh_runs_are_not_published() {
    let h = support::orm().await;
    install(&h).await;
    foreign_bump(&h, &SignalKey::cache("stable").unwrap()).await;
    let gate = Arc::new(Gate::default());
    let orm = gated_orm(&h, &gate, 3).await;
    let key = OrmCacheKey::new("test.model", "stable", vec![]);

    let mut old = first_statement(&orm).await.unwrap();
    // "restore": the rows are gone
    let client = h.db().client().await.unwrap();
    client
        .execute("DELETE FROM erp_signal_generation", &[])
        .await
        .unwrap();
    drop(client);

    let refreshing = tokio::spawn({
        let orm = orm.clone();
        async move { first_statement(&orm).await.map(|_| ()) }
    });
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
        .await
        .expect("the refresh hook was not entered");

    old.ormcache_add("stable", key.clone(), Value::Int(42))
        .await
        .unwrap();
    finish(old).await;

    gate.release.notify_one();
    let err = refreshing.await.unwrap().unwrap_err();
    assert!(err.is_signal_regression());
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(
        err.message(),
        "the signal generations went backwards (cache:stable 1 -> 0): the database was restored \
         or signal rows were deleted; this process ran a full refresh; retry the request"
    );
    let signal = orm.signal().unwrap();
    assert_eq!(signal.applied(), current(&h).await);

    let mut tx = first_statement(&orm).await.unwrap();
    assert_eq!(
        tx.ormcache_get("stable", &key).await.unwrap(),
        None,
        "a value computed before the regression refresh was published"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 2: the request role may only insert the `key` column (each of its rows weighs 1)
/// and read; it can neither choose a weight nor delete or update rows. The table bounds the weight
/// for every role. Role names are validated.
#[tokio::test]
async fn request_role_inserts_keys_only_and_weights_are_bounded() {
    let h = support::orm().await;
    install(&h).await;
    let role = format!("erp_signal_req_{}", std::process::id());
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "DROP ROLE IF EXISTS {role}; CREATE ROLE {role} NOLOGIN"
        ))
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.grant_signal_request_role(&role).await.unwrap();
    finish(tx).await;

    // an ORM connected as the request role bumps and reads normally
    let mut config = db_config(&h);
    config.options(format!("-c role={role}"));
    let as_role = Orm::builder(Arc::clone(h.registry()), pool_for(config, 1))
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(current(&h).await))
        .build()
        .unwrap();
    let mut tx = as_role.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.signal_bump(&SignalKey::Policy, SelfApplied::No)
        .await
        .unwrap();
    finish(tx).await;
    assert_eq!(current(&h).await.get(&SignalKey::Policy), 1);

    client
        .batch_execute(&format!("SET ROLE {role}"))
        .await
        .unwrap();
    for sql in [
        "INSERT INTO erp_signal_generation (key, weight) VALUES ('policy', 5)",
        "DELETE FROM erp_signal_generation",
        "UPDATE erp_signal_generation SET weight = 1",
    ] {
        let err = client.execute(sql, &[]).await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("42501"), "{sql}");
    }
    client.batch_execute("RESET ROLE").await.unwrap();

    // the weight bound holds for every role
    let max = erp_orm::signal::MAX_SIGNAL_WEIGHT;
    client
        .execute(
            "INSERT INTO erp_signal_generation (key, weight) VALUES ('cache:w', $1)",
            &[&max],
        )
        .await
        .unwrap();
    for bad in [max + 1, 0, -1] {
        let err = client
            .execute(
                "INSERT INTO erp_signal_generation (key, weight) VALUES ('cache:w', $1)",
                &[&bad],
            )
            .await
            .unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("23514"), "{bad}");
    }

    let mut tx = h.begin().await.unwrap();
    for bad in [String::new(), "r".repeat(64), "a\0b".to_owned()] {
        let err = tx.grant_signal_request_role(&bad).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!("invalid role name {bad:?}: 1 to 63 bytes without NUL")
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    drop(as_role);
    client
        .batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
        .await
        .unwrap();
    drop(client);
    h.cleanup().await.unwrap();
}

/// The privileges `role` holds on the signal table: table-level SELECT/INSERT/UPDATE/DELETE, then
/// INSERT on `key` and on `weight`.
async fn signal_privileges(client: &erp_testkit::tokio_postgres::Client, role: &str) -> Vec<bool> {
    let row = client
        .query_one(
            "SELECT has_table_privilege($1, 'erp_signal_generation', 'SELECT'), \
                    has_table_privilege($1, 'erp_signal_generation', 'INSERT'), \
                    has_table_privilege($1, 'erp_signal_generation', 'UPDATE'), \
                    has_table_privilege($1, 'erp_signal_generation', 'DELETE'), \
                    has_column_privilege($1, 'erp_signal_generation', 'key', 'INSERT'), \
                    has_column_privilege($1, 'erp_signal_generation', 'weight', 'INSERT')",
            &[&role],
        )
        .await
        .unwrap();
    (0..6).map(|i| row.get(i)).collect()
}

/// The table and column ACLs of the signal table, as text.
async fn signal_acls(client: &erp_testkit::tokio_postgres::Client) -> Vec<Option<String>> {
    client
        .query(
            "SELECT relacl::text FROM pg_class WHERE oid = 'erp_signal_generation'::regclass \
             UNION ALL \
             (SELECT attacl::text FROM pg_attribute \
              WHERE attrelid = 'erp_signal_generation'::regclass AND attnum > 0 ORDER BY attnum)",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// Delta review SHOULD 1: granting twice gives the same privileges, a role name with a double
/// quote is quoted, the table owner is refused, and a missing table names the fix.
#[tokio::test]
async fn grant_request_role_is_idempotent_quotes_names_and_refuses_the_owner() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let err = tx.grant_signal_request_role("anyone").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the signal table erp_signal_generation is missing: run Transaction::install_signal \
         before granting the request role"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    install(&h).await;

    let role = format!("erp\"sig_{}", std::process::id());
    let quoted = format!("\"{}\"", role.replace('"', "\"\""));
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "DROP ROLE IF EXISTS {quoted}; CREATE ROLE {quoted} NOLOGIN"
        ))
        .await
        .unwrap();
    let expected = vec![true, false, false, false, true, false];
    let mut tx = h.begin().await.unwrap();
    tx.grant_signal_request_role(&role).await.unwrap();
    finish(tx).await;
    assert_eq!(signal_privileges(&client, &role).await, expected);
    let acls = signal_acls(&client).await;
    let mut tx = h.begin().await.unwrap();
    tx.grant_signal_request_role(&role).await.unwrap();
    finish(tx).await;
    assert_eq!(signal_privileges(&client, &role).await, expected);
    assert_eq!(
        signal_acls(&client).await,
        acls,
        "a second grant changes nothing"
    );

    // reserved names go through the same checks as the heartbeat role's grant
    let mut tx = h.begin().await.unwrap();
    let err = tx.grant_signal_request_role("public").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "role \"public\" is reserved: PUBLIC, NONE and the predefined pg_* roles are refused (a \
         grant to them would reach every role or a whole class of roles)"
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // the owner (the role that installed the table) is refused, and keeps its rights
    let owner: String = client
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    let mut tx = h.begin().await.unwrap();
    let err = tx.grant_signal_request_role(&owner).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "role {owner:?} owns erp_signal_generation: the request role must be another role \
             (revoking from the owner would remove what installation and compaction need)"
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        signal_privileges(&client, &owner).await,
        vec![true, true, true, true, true, true]
    );
    assert_eq!(signal_acls(&client).await, acls);

    client
        .batch_execute(&format!("DROP OWNED BY {quoted}; DROP ROLE {quoted}"))
        .await
        .unwrap();
    drop(client);
    h.cleanup().await.unwrap();
}

/// Ruling (c): generations lower than applied, read after the last apply (rows deleted, a restored
/// database), fail the request with a typed error, are counted, and force a full refresh, after
/// which the process runs on the observed generations.
#[tokio::test]
async fn lower_generations_force_a_full_refresh_and_fail_loudly() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 2).await;
    foreign_bump(&h, &registry_key()).await;
    foreign_bump(&h, &registry_key()).await;
    foreign_bump(&h, &SignalKey::cache("c").unwrap()).await;
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 1);

    // "restore": the rows are gone, and a policy bump lands afterwards
    let client = h.db().client().await.unwrap();
    client
        .execute("DELETE FROM erp_signal_generation", &[])
        .await
        .unwrap();
    drop(client);
    foreign_bump(&h, &SignalKey::Policy).await;

    let err = first_statement(&orm).await.err().unwrap();
    assert!(err.is_signal_regression());
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(
        err.message(),
        "the signal generations went backwards (registry 2 -> 0, cache:c 1 -> 0): the database \
         was restored or signal rows were deleted; this process ran a full refresh; retry the \
         request"
    );
    let signal = orm.signal().unwrap();
    assert_eq!(signal.stats().regressions, 1);
    assert_eq!(fake.calls(), 2);
    {
        let seen = fake.seen.lock().unwrap();
        let full = &seen[1];
        assert!(full.regression && full.registry && full.policy);
        assert_eq!(full.caches, vec![erp_orm::CacheName::new("c").unwrap()]);
    }
    assert_eq!(signal.applied(), current(&h).await);

    // the process runs on the observed generations now: no further error or refresh
    first_statement(&orm).await.unwrap();
    assert_eq!(fake.calls(), 2);
    assert_eq!(signal.stats().regressions, 1);
    h.cleanup().await.unwrap();
}

/// Ruling (c): without a refresh hook nothing can be rebuilt: every request fails with the
/// regression until the process restarts.
#[tokio::test]
async fn lower_generations_without_a_hook_refuse_service() {
    let h = support::orm().await;
    install(&h).await;
    let orm = signal_orm(&h, None, 2).await;
    foreign_bump(&h, &SignalKey::cache("c").unwrap()).await;
    first_statement(&orm).await.unwrap();
    let client = h.db().client().await.unwrap();
    client
        .execute("DELETE FROM erp_signal_generation", &[])
        .await
        .unwrap();
    drop(client);
    for _ in 0..2 {
        let err = first_statement(&orm).await.err().unwrap();
        assert!(err.is_signal_regression());
        assert_eq!(err.kind(), ErrorKind::Database);
        assert_eq!(
            err.message(),
            "the signal generations went backwards (cache:c 1 -> 0): the database was restored \
             or signal rows were deleted; no refresh hook is installed (Signal::with_refresh), so \
             this process refuses to serve until it restarts"
        );
    }
    assert_eq!(orm.signal().unwrap().stats().regressions, 2);
    h.cleanup().await.unwrap();
}

/// Review item 4: a named-cache bump sends no NOTIFY (the BEGIN read and the poll carry it).
#[tokio::test]
async fn cache_bumps_send_no_notification() {
    let h = support::orm().await;
    install(&h).await;
    let (client, mut connection) = db_config(&h).connect(NoTls).await.unwrap();
    let (tx_note, mut rx_note) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = std::future::poll_fn(|cx| connection.poll_message(cx)).await {
            if let Ok(AsyncMessage::Notification(n)) = message
                && tx_note.send(n.channel().to_owned()).is_err()
            {
                return;
            }
        }
    });
    client.batch_execute("LISTEN erp_signal").await.unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.signal_bump(&SignalKey::cache("c").unwrap(), SelfApplied::No)
        .await
        .unwrap();
    assert_eq!(
        tx.captured()
            .iter()
            .filter(|s| s.contains("pg_notify"))
            .count(),
        0
    );
    finish(tx).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rx_note.try_recv().is_err(), "no notification for a cache");
    assert_eq!(current(&h).await.get(&SignalKey::cache("c").unwrap()), 1);
    drop(client);
    h.cleanup().await.unwrap();
}

/// Review item 5: the refresh hook is bounded by the apply timeout; an overrun fails the request
/// and leaves the generation unapplied for the next one.
#[tokio::test]
async fn refresh_hook_is_bounded_by_the_apply_timeout() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::from_millis(800));
    let signal = Signal::new(current(&h).await)
        .with_refresh(Hook(Arc::clone(&fake)))
        .with_apply_timeout(Duration::from_millis(100))
        .unwrap();
    let orm = Orm::builder(Arc::clone(h.registry()), pool_for(db_config(&h), 2))
        .policy(StaticPolicy::AllowAll)
        .signal(signal)
        .build()
        .unwrap();
    foreign_bump(&h, &registry_key()).await;
    let err = first_statement(&orm).await.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(
        err.message(),
        "the signal refresh hook did not finish within 100ms (Signal::with_apply_timeout): it \
         was cancelled and the registry and policy generations stay unapplied, so the next \
         request tries again"
    );
    assert_eq!(orm.signal().unwrap().applied().get(&registry_key()), 0);
    h.cleanup().await.unwrap();
}

/// Review item 7: a listener whose Orm was superseded ends by itself.
#[tokio::test]
async fn superseded_listener_stops_by_itself() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    fake.script(Ok(Refreshed::Superseded));
    let orm = signal_orm(&h, Some(&fake), 2).await;
    let handle = orm
        .spawn_signal_listener(db_config(&h), NoTls, fast())
        .unwrap();
    let signal = orm.signal().unwrap();
    eventually("listener up", || signal.stats().listener_up).await;
    foreign_bump(&h, &registry_key()).await;
    eventually("listener ended", || handle.is_finished()).await;
    assert!(signal.is_superseded());
    assert!(!signal.stats().listener_up);
    handle.shutdown().await;
    h.cleanup().await.unwrap();
}

/// Review item 7: while the listener is down, its pool reads are bounded by the read timeout (a
/// pool with no free connection does not stall it) and give way to a shutdown.
#[tokio::test]
async fn listener_pool_reads_are_bounded_and_stoppable() {
    let h = support::orm().await;
    install(&h).await;
    let fake = Fake::with_delay(Duration::ZERO);
    let orm = signal_orm(&h, Some(&fake), 1).await;
    let held = first_statement(&orm).await.unwrap(); // the only pooled connection
    let mut dead = Config::new();
    dead.host("127.0.0.1").port(1).user("nobody").dbname("none");

    let handle = orm
        .spawn_signal_listener(
            dead.clone(),
            NoTls,
            fast().read_timeout(Duration::from_millis(100)),
        )
        .unwrap();
    let signal = orm.signal().unwrap();
    let polls = signal.stats().polls;
    eventually("several bounded polls", || {
        signal.stats().polls >= polls + 3
    })
    .await;
    handle.shutdown().await;

    // a long read timeout: only the stop watch can end the stalled pool read promptly
    let handle = orm
        .spawn_signal_listener(dead, NoTls, fast().read_timeout(Duration::from_secs(600)))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown while the pool read waits");
    drop(held);
    h.cleanup().await.unwrap();
}

/// Review item 8: a bump made in a released savepoint belongs to the enclosing transaction: a later
/// sibling savepoint that rolls back does not take it back, so a second bump of the key is still
/// skipped (one row, one NOTIFY).
#[tokio::test]
async fn released_savepoint_bumps_survive_a_later_rollback() {
    let h = support::orm().await;
    install(&h).await;
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.savepoint(async |tx| tx.signal_bump(&SignalKey::Registry, SelfApplied::No).await)
        .await
        .unwrap();
    let failed: Result<()> = tx.savepoint(async |_| Err(Error::user("boom"))).await;
    assert_eq!(failed.unwrap_err().message(), "boom");
    tx.signal_bump(&SignalKey::Registry, SelfApplied::No)
        .await
        .unwrap();
    let notifies = tx
        .captured()
        .iter()
        .filter(|s| s.contains("pg_notify"))
        .count();
    assert_eq!(notifies, 1);
    finish(tx).await;
    assert_eq!(current(&h).await.get(&registry_key()), 1);
    h.cleanup().await.unwrap();
}

/// Ruling (d): a schema sync that changes the schema bumps the registry in its own transaction;
/// the syncing process credits its own bump, another process rebuilds; a sync that changes
/// nothing bumps nothing (a restarting worker does not make the others rebuild).
#[tokio::test]
async fn schema_change_bumps_the_registry_itself() {
    let h = support::orm().await;
    install(&h).await;
    let fake_a = Fake::with_delay(Duration::ZERO);
    let fake_b = Fake::with_delay(Duration::ZERO);
    let a = signal_orm(&h, Some(&fake_a), 2).await;
    let b = signal_orm(&h, Some(&fake_b), 2).await;

    assert!(a.sync_schema().await.unwrap().is_empty());
    assert_eq!(current(&h).await.get(&registry_key()), 0, "nothing changed");

    let client = h.db().client().await.unwrap();
    client
        .batch_execute("ALTER TABLE test_orm_country DROP COLUMN code")
        .await
        .unwrap();
    drop(client);
    assert!(a.sync_schema().await.unwrap().is_empty());
    assert_eq!(current(&h).await.get(&registry_key()), 1);
    assert_eq!(a.signal().unwrap().applied().get(&registry_key()), 1);
    first_statement(&a).await.unwrap();
    assert_eq!(fake_a.calls(), 0, "own schema change: no rebuild");
    first_statement(&b).await.unwrap();
    assert_eq!(fake_b.calls(), 1);
    assert!(fake_b.seen.lock().unwrap()[0].registry);
    h.cleanup().await.unwrap();
}
