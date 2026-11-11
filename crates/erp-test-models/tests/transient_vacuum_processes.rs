//! The transient vacuum with two actual operating-system processes on one tenant database
//! (package C).
//!
//! Package B's tests use several `Orm` objects inside one process; this target runs the public
//! `Orm::vacuum_transient` in a child process: the test binary started again with
//! `--ignored --exact vacuum_child_worker`, which builds its own `Orm` on the parent's database
//! and answers line commands on stdin/stdout. Lines that start with `@@ ` are the protocol (the
//! test harness prints its own lines on stdout too). A child announces `ready`, then runs one
//! command per line and answers each with one line; the parent bounds every wait and kills the
//! child when a test ends or fails (`kill_on_drop`, and an explicit `finish`).
//!
//! Pausing a child inside the cleanup uses advisory locks, which every session sees: the fixture
//! hooks of `test_vacuum` (`park`, `lockwait`, `hang`) and two test-only seams of
//! `VacuumOptions` (feature `test-faults`): pause between batches or after a batch's snapshot until
//! the parent releases the gate lock, and a commit fault. Nothing here measures time: a lease
//! expires because the fixture moves its expiry into the past, and every wait polls a condition
//! with an upper bound.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use erp_orm::{CommitFault, Orm, PrincipalHint, StaticPolicy, VacuumOptions, VacuumOutcome};
use erp_registry::RegistryBuilder;
use erp_test_models::test_vacuum::{PARK_GATE_KEY, PARK_READY_KEY};
use erp_test_models::{base_users, test_vacuum};
use erp_testkit::Harness;
use erp_testkit::deadpool_postgres::Object;
use erp_testkit::deadpool_postgres::tokio_postgres::types::ToSql;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const ENV_HOST: &str = "ERP_VACUUM_CHILD_HOST";
const ENV_PORT: &str = "ERP_VACUUM_CHILD_PORT";
const ENV_USER: &str = "ERP_VACUUM_CHILD_USER";
const ENV_PASSWORD: &str = "ERP_VACUUM_CHILD_PASSWORD";
const ENV_DB: &str = "ERP_VACUUM_CHILD_DB";
/// Prefix of the protocol lines.
const MARK: &str = "@@ ";
/// The longest the parent waits for one answer of a child before it fails the test.
const ANSWER_LIMIT: Duration = Duration::from_secs(120);

// ----- the child ------------------------------------------------------------------------------

fn child_config() -> erp_testkit::tokio_postgres::Config {
    let need = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| {
            panic!("{name} is not set: this test is the child of the process tests, run those")
        })
    };
    let mut config = erp_testkit::tokio_postgres::Config::new();
    let host = need(ENV_HOST);
    if host.starts_with('/') {
        config.host_path(&host);
    } else {
        config.host(&host);
    }
    config.port(need(ENV_PORT).parse().expect("port"));
    config.user(need(ENV_USER));
    config.dbname(need(ENV_DB));
    if let Ok(password) = std::env::var(ENV_PASSWORD) {
        config.password(&password);
    }
    config
}

async fn child_orm() -> Orm {
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_vacuum::ADDON])
            .linked(true)
            .build()
            .expect("registry"),
    );
    let pool = erp_orm::pool::create_pool(child_config(), 4).expect("pool");
    Orm::builder(registry, pool)
        .policy(StaticPolicy::AllowAll)
        .trust_caller_resolutions()
        .build()
        .expect("orm")
}

fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

fn parse_options(words: &[&str]) -> VacuumOptions {
    let mut options = VacuumOptions::default();
    for word in words {
        let (key, value) = word.split_once('=').expect("key=value option");
        options = match (key, value) {
            ("rows", n) => options
                .max_batch_rows(n.parse().expect("rows"))
                .expect("rows in range"),
            ("fault", "lost") => options.with_commit_fault(CommitFault::AnswerLostAfterCommit),
            ("fault", "closed") => {
                options.with_commit_fault(CommitFault::ConnectionClosedBeforeCommit)
            }
            ("pause", "between") => options.pause_between_batches(PARK_GATE_KEY),
            ("pause", "snapshot") => options.pause_after_snapshot(PARK_GATE_KEY, PARK_READY_KEY),
            other => panic!("unknown child option {other:?}"),
        };
    }
    options
}

async fn run_command(orm: &Orm, line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["hello"] => {
            let mut tx = orm.begin(PrincipalHint::system()).await.expect("begin");
            let rows = tx.query("SELECT pg_backend_pid()", &[]).await.expect("pid");
            let backend: i32 = rows[0].get(0);
            tx.rollback().await.expect("rollback");
            format!("hello os_pid={} backend_pid={backend}", std::process::id())
        }
        ["vacuum", model, rest @ ..] => {
            match orm.vacuum_transient(model, parse_options(rest)).await {
                Ok(r) => format!(
                    "report outcome={:?} deleted={} batches={} epoch={} cycle_rows={} \
                     cycle_batches={} retries={}",
                    r.outcome,
                    r.deleted_rows,
                    r.batches,
                    r.epoch.map_or_else(|| "none".to_owned(), |e| e.to_string()),
                    r.cycle_rows,
                    r.cycle_batches,
                    erp_orm::vacuum_batch_retries()
                ),
                Err(e) => format!(
                    "error kind={:?} sqlstate={} msg={}",
                    e.kind(),
                    e.sqlstate().unwrap_or("none"),
                    one_line(e.message())
                ),
            }
        }
        other => panic!("unknown child command {other:?}"),
    }
}

/// The child process. Never runs by itself (`#[ignore]`): the process tests start it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "the child process of the vacuum process tests, started by them"]
async fn vacuum_child_worker() {
    let orm = child_orm().await;
    println!("{MARK}ready");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.expect("stdin") {
        if line.trim() == "quit" {
            return;
        }
        let answer = run_command(&orm, &line).await;
        println!("{MARK}{answer}");
    }
}

// ----- the parent: a handle on a child --------------------------------------------------------

/// One child process.
struct Peer {
    child: Child,
    stdin: ChildStdin,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
}

/// One answer line of a child.
struct Answer(String);

impl Answer {
    /// The value of `key=value` among the words before `msg=`.
    fn word(&self, key: &str) -> &str {
        let head = self.0.split(" msg=").next().unwrap_or("");
        head.split_whitespace()
            .find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
            .unwrap_or_else(|| panic!("no '{key}' in the answer: {}", self.0))
    }

    fn number(&self, key: &str) -> i64 {
        self.word(key)
            .parse()
            .unwrap_or_else(|_| panic!("'{key}' is not a number in: {}", self.0))
    }

    /// Assert it is a report with this outcome.
    fn expect_report(&self, outcome: VacuumOutcome, deleted: i64) {
        assert!(self.0.starts_with("report "), "not a report: {}", self.0);
        assert_eq!(self.word("outcome"), format!("{outcome:?}"), "{}", self.0);
        assert_eq!(self.number("deleted"), deleted, "{}", self.0);
    }

    /// Assert it is an error and give its message.
    fn expect_error(&self) -> &str {
        assert!(self.0.starts_with("error "), "not an error: {}", self.0);
        self.0.split_once(" msg=").map_or("", |(_, m)| m)
    }
}

impl Peer {
    /// Start a child on the database of `h` and wait for its `ready` line.
    async fn start(h: &Harness) -> Self {
        use erp_testkit::tokio_postgres::config::Host;
        let config = h.db().config();
        let host = match config.get_hosts().first().expect("a host") {
            Host::Tcp(name) => name.clone(),
            Host::Unix(path) => path.to_string_lossy().into_owned(),
        };
        // Limitation: the child gets host, port, user, password and database, not the whole
        // connection string; options that change how it connects are refused, never dropped.
        assert!(
            config.get_options().is_none() && config.get_application_name().is_none(),
            "extra connection options would be lost in the child"
        );
        let port = *config.get_ports().first().expect("a port");
        let mut command = Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "--ignored",
                "--exact",
                "vacuum_child_worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ENV_HOST, host)
            .env(ENV_PORT, port.to_string())
            .env(ENV_USER, config.get_user().expect("a user"))
            .env(ENV_DB, config.get_dbname().expect("a database"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        match config.get_password() {
            Some(pw) => command.env(ENV_PASSWORD, String::from_utf8_lossy(pw).into_owned()),
            None => command.env_remove(ENV_PASSWORD),
        };
        let mut child = command.spawn().expect("start the child process");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let mut peer = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        };
        let ready = peer.answer().await;
        assert_eq!(ready.0, "ready", "the child did not come up");
        peer
    }

    async fn send(&mut self, command: &str) {
        if let Some(status) = self.child.try_wait().expect("child state") {
            panic!("cannot send '{command}': the child already exited with {status}");
        }
        self.stdin
            .write_all(format!("{command}\n").as_bytes())
            .await
            .expect("write to the child");
        self.stdin.flush().await.expect("flush to the child");
    }

    /// The next protocol line, within [`ANSWER_LIMIT`]; a child that exited is a failure.
    async fn answer(&mut self) -> Answer {
        let read = async {
            loop {
                match self.lines.next_line().await.expect("child stdout") {
                    Some(line) => {
                        if let Some((_, rest)) = line.split_once(MARK) {
                            // libtest prints "test <name> ... " before the child's output
                            return Answer(rest.to_owned());
                        }
                    }
                    None => panic!("the child closed its output before it answered"),
                }
            }
        };
        tokio::time::timeout(ANSWER_LIMIT, read)
            .await
            .expect("the child did not answer in time")
    }

    async fn call(&mut self, command: &str) -> Answer {
        self.send(command).await;
        self.answer().await
    }

    /// Signal the child while it is not reaped (`kill -STOP` / `-CONT`): never a group, and never
    /// a pid after the child was waited for.
    fn signal(&mut self, name: &str) {
        assert!(
            self.child.try_wait().expect("child state").is_none(),
            "the child already exited"
        );
        let pid = self.child.id().expect("a live child has a pid");
        let status = std::process::Command::new("kill")
            .arg(format!("-{name}"))
            .arg(pid.to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{name} {pid} failed");
    }

    /// End the child: ask it to quit, then kill it when it does not within the bound.
    async fn finish(mut self) {
        self.send("quit").await;
        drop(self.stdin);
        match tokio::time::timeout(Duration::from_secs(20), self.child.wait()).await {
            Ok(status) => {
                let status = status.expect("wait for the child");
                assert!(status.success(), "the child exited with {status}");
            }
            Err(_) => {
                self.child.kill().await.expect("kill the child");
                panic!("the child did not exit within 20 s of quit");
            }
        }
    }

    /// Kill the child with SIGKILL and reap it.
    async fn kill(mut self) {
        self.child.kill().await.expect("kill the child");
    }
}

// ----- database helpers -----------------------------------------------------------------------

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_vacuum::ADDON])
        .build()
        .await
        .expect("harness")
}

/// Run `sql` and return its single cell as text.
async fn q(h: &Harness, sql: &str) -> String {
    let client = h.db().client().await.unwrap();
    let row = client
        .query_one(&format!("SELECT ({sql})::text"), &[])
        .await
        .unwrap();
    row.get::<_, Option<String>>(0).unwrap_or_default()
}

async fn exec(h: &Harness, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    let client = h.db().client().await.unwrap();
    client.execute(sql, params).await.unwrap();
}

/// A row of `test_vac_default` written two hours ago.
async fn old_row(h: &Harness, name: &str) {
    exec(
        h,
        "INSERT INTO test_vac_default (name, create_date, write_date) VALUES ($1, \
         (now() AT TIME ZONE 'UTC') - interval '2 hours', (now() AT TIME ZONE 'UTC') - interval '2 hours')",
        &[&name],
    )
    .await;
}

async fn old_rows(h: &Harness, n: i64) {
    exec(
        h,
        "INSERT INTO test_vac_default (name, create_date, write_date) SELECT 'r' || i, \
         (now() AT TIME ZONE 'UTC') - interval '2 hours', (now() AT TIME ZONE 'UTC') - interval '2 hours' \
         FROM generate_series(1, $1::int8) AS i",
        &[&n],
    )
    .await;
}

async fn table_rows(h: &Harness) -> i64 {
    q(h, "SELECT count(*) FROM test_vac_default")
        .await
        .parse()
        .unwrap()
}

/// `(entries, distinct entries)` of the deletion log: every deleted record is logged by the
/// deleting transaction, so a record deleted twice shows as fewer distinct entries.
async fn log_counts(h: &Harness) -> (i64, i64) {
    let total: i64 = q(h, "SELECT count(*) FROM test_vac_log")
        .await
        .parse()
        .unwrap();
    let distinct: i64 = q(h, "SELECT count(DISTINCT entry) FROM test_vac_log")
        .await
        .parse()
        .unwrap();
    (total, distinct)
}

/// `phase|latched|rows_total|batches_total|epoch|owner-is-null` of the model's state row.
async fn state(h: &Harness) -> String {
    q(
        h,
        "SELECT phase || '|' || count_latched || '|' || rows_total || '|' || batches_total \
         || '|' || epoch || '|' || (owner IS NULL) FROM erp_transient_vacuum_state \
         WHERE model = 'test_vac.default'",
    )
    .await
}

/// Poll `sql` (a boolean) every 50 ms until it is true, at most `limit`; a bound, not a result.
async fn wait_until(h: &Harness, what: &str, sql: &str, limit: Duration) {
    let polls = limit.as_millis() / 50;
    for _ in 0..polls {
        if q(h, sql).await == "true" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gave up waiting for: {what}");
}

const SHORT: Duration = Duration::from_secs(60);

fn advisory_held(key: i64) -> String {
    format!(
        "EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND granted \
         AND objid::text = '{key}' AND database = \
         (SELECT oid FROM pg_database WHERE datname = current_database()))"
    )
}

/// Wait until a `park` hook of another session is inside its batch.
async fn wait_parked(h: &Harness) {
    let sql = format!("SELECT {}", advisory_held(PARK_READY_KEY));
    wait_until(h, "a batch parked in its hook", &sql, SHORT).await;
}

/// The parent's hold on the gate lock (a session advisory lock on its own connection).
struct Gate(Object);

impl Gate {
    async fn close(h: &Harness) -> Self {
        let client = h.db().client().await.unwrap();
        client
            .query_one("SELECT pg_advisory_lock($1)", &[&PARK_GATE_KEY])
            .await
            .unwrap();
        Self(client)
    }

    async fn open(self) {
        let ok: bool = self
            .0
            .query_one("SELECT pg_advisory_unlock($1)", &[&PARK_GATE_KEY])
            .await
            .unwrap()
            .get(0);
        assert!(ok, "the gate was not held");
    }
}

/// Move the lease of the model's state row into the past, as time passing would.
async fn expire_lease(h: &Harness) {
    exec(
        h,
        "UPDATE erp_transient_vacuum_state SET lease_expires_at = clock_timestamp() - interval '1 hour' \
         WHERE model = 'test_vac.default' AND owner IS NOT NULL",
        &[],
    )
    .await;
}

async fn vacuum(h: &Harness) -> erp_orm::VacuumReport {
    h.orm()
        .vacuum_transient("test_vac.default", VacuumOptions::default())
        .await
        .unwrap()
}

// ----- the tests ------------------------------------------------------------------------------

/// cycle runs once, the model is then not due)
#[tokio::test(flavor = "multi_thread")]
async fn two_processes_contend_for_one_cycle() {
    let h = harness().await;
    for name in ["a", "b", "c", "park"] {
        old_row(&h, name).await;
    }
    let gate = Gate::close(&h).await;
    let mut a = Peer::start(&h).await;
    let mut b = Peer::start(&h).await;
    // two processes, two database sessions: not two Orm objects of this process
    let hello_a = a.call("hello").await;
    let hello_b = b.call("hello").await;
    let parent_session: i32 = q(&h, "SELECT pg_backend_pid()").await.parse().unwrap();
    let pids = [
        i64::from(std::process::id()),
        hello_a.number("os_pid"),
        hello_b.number("os_pid"),
    ];
    assert!(
        pids[0] != pids[1] && pids[0] != pids[2] && pids[1] != pids[2],
        "{pids:?}"
    );
    assert!(hello_a.number("backend_pid") != hello_b.number("backend_pid"));
    assert!(hello_a.number("backend_pid") != i64::from(parent_session));

    // A claims and parks inside its first batch
    a.send("vacuum test_vac.default").await;
    wait_parked(&h).await;
    // the batch holds the state row: the parent (this process) and B (another process) are Busy
    let r = vacuum(&h).await;
    assert_eq!(r.outcome, VacuumOutcome::Busy);
    assert_eq!((r.deleted_rows, r.epoch), (0, None));
    b.call("vacuum test_vac.default")
        .await
        .expect_report(VacuumOutcome::Busy, 0);
    assert_eq!(table_rows(&h).await, 4, "nothing was removed meanwhile");
    assert_eq!(log_counts(&h).await, (0, 0));

    gate.open().await;
    let done = a.answer().await;
    done.expect_report(VacuumOutcome::Completed, 4);
    assert_eq!(done.number("epoch"), 1);
    assert_eq!(table_rows(&h).await, 0);
    // every record was removed once, by the one owner
    assert_eq!(log_counts(&h).await, (4, 4));
    // the cycle ran; the next one is a cadence away, for every process
    assert_eq!(vacuum(&h).await.outcome, VacuumOutcome::NotDue);
    b.call("vacuum test_vac.default")
        .await
        .expect_report(VacuumOutcome::NotDue, 0);
    assert_eq!(state(&h).await, "idle|false|4|1|1|true");
    a.finish().await;
    b.finish().await;
    h.cleanup().await.unwrap();
}

/// and the former owner resuming is refused with LostLease, committing nothing)
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_owner_resumes_into_lost_lease() {
    let h = harness().await;
    old_rows(&h, 25).await;
    let gate = Gate::close(&h).await;
    let mut a = Peer::start(&h).await;
    // A removes ten rows (one committed batch), then stalls before its next batch
    a.send("vacuum test_vac.default rows=10 pause=between")
        .await;
    wait_until(
        &h,
        "A's first batch committed",
        "SELECT batches_total = 1 AND owner IS NOT NULL FROM erp_transient_vacuum_state \
         WHERE model = 'test_vac.default'",
        SHORT,
    )
    .await;
    assert_eq!(table_rows(&h).await, 15);
    // its lease is live: the parent cannot start a second cycle
    assert_eq!(vacuum(&h).await.outcome, VacuumOutcome::Busy);
    // the lease runs out (fixture state, no sleeping); the parent takes the cycle over
    expire_lease(&h).await;
    let r = vacuum(&h).await;
    assert_eq!(r.outcome, VacuumOutcome::Completed);
    assert_eq!((r.deleted_rows, r.epoch), (15, Some(2)));
    // the takeover inherited the cycle's counters instead of starting from zero
    assert_eq!((r.cycle_rows, r.cycle_batches), (25, 2));
    assert_eq!(table_rows(&h).await, 0);
    let before = state(&h).await;
    assert_eq!(before, "idle|false|25|2|2|true");

    // A wakes up: its next batch finds another epoch and refuses
    gate.open().await;
    let resumed = a.answer().await;
    resumed.expect_report(VacuumOutcome::LostLease, 10);
    assert_eq!(resumed.number("epoch"), 1);
    assert_eq!(state(&h).await, before, "the stale owner wrote nothing");
    assert_eq!(log_counts(&h).await, (25, 25), "no record removed twice");
    a.finish().await;
    h.cleanup().await.unwrap();
}

/// back batch is redone by the process that takes the lease over after the lease expired)
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_owner_is_recovered_by_takeover() {
    let h = harness().await;
    for name in ["a", "b", "c", "d", "park"] {
        old_row(&h, name).await;
    }
    let gate = Gate::close(&h).await;
    let mut a = Peer::start(&h).await;
    a.send("vacuum test_vac.default").await;
    wait_parked(&h).await;
    a.kill().await;
    // the server ends the dead process's session: the batch rolls back, its locks go
    wait_until(
        &h,
        "the killed owner's batch released its locks",
        &format!("SELECT NOT {}", advisory_held(PARK_READY_KEY)),
        SHORT,
    )
    .await;
    assert_eq!(table_rows(&h).await, 5);
    assert_eq!(log_counts(&h).await, (0, 0), "the batch left no trace");
    // the dead owner's lease has not expired: nobody may start a second owner yet
    assert_eq!(vacuum(&h).await.outcome, VacuumOutcome::Busy);
    expire_lease(&h).await;
    gate.open().await;
    let r = vacuum(&h).await;
    assert_eq!(r.outcome, VacuumOutcome::Completed);
    assert_eq!((r.deleted_rows, r.epoch), (5, Some(2)));
    assert_eq!(table_rows(&h).await, 0);
    assert_eq!(log_counts(&h).await, (5, 5));
    h.cleanup().await.unwrap();
}

/// skip it; the row is then recent and survives the continued cycle)
#[tokio::test(flavor = "multi_thread")]
async fn a_row_being_written_is_skipped_and_then_protected() {
    let h = harness().await;
    for name in ["old-1", "old-2", "racer"] {
        old_row(&h, name).await;
    }
    let writer = h.db().client().await.unwrap();
    writer.batch_execute("BEGIN").await.unwrap();
    writer
        .execute(
            "UPDATE test_vac_default SET write_date = now() AT TIME ZONE 'UTC' WHERE name = 'racer'",
            &[],
        )
        .await
        .unwrap();
    let mut a = Peer::start(&h).await;
    // the other rows go; the locked one is skipped, which proves nothing is left to remove
    // only after it is read again, so the cycle stays open
    let first = a.call("vacuum test_vac.default").await;
    first.expect_report(VacuumOutcome::Pending, 2);
    assert_eq!(
        q(&h, "SELECT string_agg(name, ',') FROM test_vac_default").await,
        "racer"
    );
    writer.batch_execute("COMMIT").await.unwrap();
    // the committed write made the row recent: the continued cycle ends without removing it
    let second = a.call("vacuum test_vac.default").await;
    second.expect_report(VacuumOutcome::Completed, 0);
    assert_eq!(second.number("cycle_rows"), 2);
    assert_eq!(
        q(&h, "SELECT string_agg(name, ',') FROM test_vac_default").await,
        "racer"
    );
    assert_eq!(log_counts(&h).await, (2, 2));
    a.finish().await;
    h.cleanup().await.unwrap();
}

/// serialization failure; the batch reselects from a fresh snapshot and the row survives)
#[tokio::test(flavor = "multi_thread")]
async fn a_write_after_the_snapshot_protects_the_row() {
    let h = harness().await;
    old_row(&h, "old-1").await;
    old_row(&h, "racer").await;
    let gate = Gate::close(&h).await;
    let mut a = Peer::start(&h).await;
    a.send("vacuum test_vac.default pause=snapshot").await;
    // the batch has its snapshot and the state row locked, and holds the ready lock at the gate
    wait_parked(&h).await;
    exec(
        &h,
        "UPDATE test_vac_default SET write_date = now() AT TIME ZONE 'UTC' WHERE name = 'racer'",
        &[],
    )
    .await;
    gate.open().await;
    let done = a.answer().await;
    // the first attempt failed to serialize and was redone: only the unchanged row was removed
    done.expect_report(VacuumOutcome::Completed, 1);
    assert_eq!(
        done.number("retries"),
        1,
        "exactly one serialization retry: {}",
        done.0
    );
    assert_eq!(
        q(&h, "SELECT string_agg(name, ',') FROM test_vac_default").await,
        "racer"
    );
    assert_eq!(log_counts(&h).await, (1, 1));
    a.finish().await;
    h.cleanup().await.unwrap();
}

/// rolls back, the lease is handed back, and the model can be cleaned afterwards)
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_stuck_on_a_lock_is_cancelled_by_the_lock_timeout() {
    let h = harness().await;
    old_row(&h, "old").await;
    old_row(&h, "lockwait").await;
    let gate = Gate::close(&h).await;
    let mut a = Peer::start(&h).await;
    let failed = a.call("vacuum test_vac.default").await;
    failed.expect_error();
    assert_eq!(failed.word("kind"), "Lock", "{}", failed.0);
    assert_eq!(table_rows(&h).await, 2);
    assert_eq!(log_counts(&h).await, (0, 0));
    // the failed owner handed the lease back and left the cycle open
    assert_eq!(state(&h).await, "age|false|0|0|1|true");
    gate.open().await;
    a.call("vacuum test_vac.default")
        .await
        .expect_report(VacuumOutcome::Completed, 2);
    assert_eq!(table_rows(&h).await, 0);
    a.finish().await;
    h.cleanup().await.unwrap();
}

/// timeout and rolls back)
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_stuck_in_a_statement_is_cancelled_by_the_statement_timeout() {
    let h = harness().await;
    old_row(&h, "old").await;
    old_row(&h, "hang").await;
    let mut a = Peer::start(&h).await;
    let failed = a.call("vacuum test_vac.default").await;
    failed.expect_error();
    assert_eq!(failed.word("sqlstate"), "57014", "{}", failed.0);
    assert_eq!(table_rows(&h).await, 2);
    assert_eq!(log_counts(&h).await, (0, 0));
    assert_eq!(state(&h).await, "age|false|0|0|1|true");
    exec(&h, "UPDATE test_vac_default SET name = 'fine'", &[]).await;
    a.call("vacuum test_vac.default")
        .await
        .expect_report(VacuumOutcome::Completed, 2);
    a.finish().await;
    h.cleanup().await.unwrap();
}

/// backstop; its lock goes, a takeover completes the cycle, and the owner waking up commits
/// nothing)
#[tokio::test(flavor = "multi_thread")]
async fn a_frozen_owner_is_ended_by_the_transaction_backstop() {
    let h = harness().await;
    for name in ["a", "b", "park"] {
        old_row(&h, name).await;
    }
    let gate = Gate::close(&h).await;
    let mut a = Peer::start(&h).await;
    a.send("vacuum test_vac.default").await;
    wait_parked(&h).await;
    // freeze the whole process: no future polls, as a stuck native hook would
    a.signal("STOP");
    wait_until(
        &h,
        "the server ended the frozen batch's session (transaction_timeout)",
        &format!("SELECT NOT {}", advisory_held(PARK_READY_KEY)),
        Duration::from_secs(120),
    )
    .await;
    assert_eq!(table_rows(&h).await, 3);
    assert_eq!(log_counts(&h).await, (0, 0));
    gate.open().await;
    expire_lease(&h).await;
    let r = vacuum(&h).await;
    assert_eq!(r.outcome, VacuumOutcome::Completed);
    assert_eq!((r.deleted_rows, r.epoch), (3, Some(2)));
    let after_takeover = state(&h).await;
    // the frozen process wakes up: its connection is gone, so its late work fails and commits
    // nothing; the lease it holds no longer exists
    a.signal("CONT");
    let late = a.answer().await;
    late.expect_error();
    assert!(
        late.0.contains("transaction_timeout"),
        "the late error names the transaction timeout: {}",
        late.0
    );
    assert_eq!(state(&h).await, after_takeover);
    assert_eq!(log_counts(&h).await, (3, 3));
    a.finish().await;
    h.cleanup().await.unwrap();
}

/// outcome; the lease is kept, and the takeover reads the stored state and does not redo the
/// batch that did commit)
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_lost_after_commit_is_not_replayed() {
    let h = harness().await;
    old_rows(&h, 5).await;
    let mut a = Peer::start(&h).await;
    let lost = a.call("vacuum test_vac.default rows=3 fault=lost").await;
    let message = lost.expect_error();
    assert_eq!(lost.word("kind"), "Database", "{}", lost.0);
    assert!(
        message.contains("outcome of a commit is not known"),
        "{message}"
    );
    assert_eq!(table_rows(&h).await, 2, "the batch did commit");
    // the lease was not handed back: the owner cannot know what the database holds
    assert_eq!(state(&h).await, "age|false|3|1|1|false");
    assert_eq!(vacuum(&h).await.outcome, VacuumOutcome::Busy);
    expire_lease(&h).await;
    let r = vacuum(&h).await;
    assert_eq!(r.outcome, VacuumOutcome::Completed);
    assert_eq!((r.deleted_rows, r.epoch), (2, Some(2)));
    assert_eq!((r.cycle_rows, r.cycle_batches), (5, 2));
    assert_eq!(log_counts(&h).await, (5, 5), "no record was removed twice");
    a.finish().await;
    h.cleanup().await.unwrap();
}

/// outcome; nothing was committed, and the takeover redoes the batch)
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_lost_before_commit_is_redone_by_the_takeover() {
    let h = harness().await;
    old_rows(&h, 5).await;
    let mut a = Peer::start(&h).await;
    let lost = a.call("vacuum test_vac.default rows=3 fault=closed").await;
    let message = lost.expect_error();
    assert_eq!(lost.word("kind"), "Database", "{}", lost.0);
    assert!(
        message.contains("outcome of a commit is not known"),
        "{message}"
    );
    // the server rolls the batch back when the session closes
    wait_until(
        &h,
        "the closed session's batch rolled back",
        "SELECT NOT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() \
         AND pid <> pg_backend_pid() AND state = 'idle in transaction')",
        SHORT,
    )
    .await;
    assert_eq!(table_rows(&h).await, 5);
    assert_eq!(state(&h).await, "age|false|0|0|1|false");
    expire_lease(&h).await;
    let r = vacuum(&h).await;
    assert_eq!(r.outcome, VacuumOutcome::Completed);
    assert_eq!((r.deleted_rows, r.epoch), (5, Some(2)));
    assert_eq!(log_counts(&h).await, (5, 5));
    a.finish().await;
    h.cleanup().await.unwrap();
}

/// one does not make the other Busy, and each state and each deletion stays in its own database)
#[tokio::test(flavor = "multi_thread")]
async fn two_tenant_databases_are_independent() {
    let one = harness().await;
    let two = harness().await;
    assert_ne!(one.db().name(), two.db().name());
    // both databases come from the same cluster: one host and port, so the two children differ
    // only in the database they open
    assert_eq!(one.db().config().get_hosts(), two.db().config().get_hosts());
    assert_eq!(one.db().config().get_ports(), two.db().config().get_ports());
    for name in ["a", "b", "park"] {
        old_row(&one, name).await;
    }
    for name in ["x", "y", "z"] {
        old_row(&two, name).await;
    }
    let gate = Gate::close(&one).await;
    let mut in_one = Peer::start(&one).await;
    let mut in_two = Peer::start(&two).await;
    in_one.send("vacuum test_vac.default").await;
    wait_parked(&one).await;
    // tenant one holds a lease and a locked state row; tenant two is untouched by that
    in_two
        .call("vacuum test_vac.default")
        .await
        .expect_report(VacuumOutcome::Completed, 3);
    assert_eq!(table_rows(&two).await, 0);
    assert_eq!(table_rows(&one).await, 3, "tenant one lost nothing");
    assert_eq!(state(&two).await, "idle|false|3|1|1|true");
    assert_eq!(vacuum(&two).await.outcome, VacuumOutcome::NotDue);
    assert_eq!(vacuum(&one).await.outcome, VacuumOutcome::Busy);
    gate.open().await;
    in_one
        .answer()
        .await
        .expect_report(VacuumOutcome::Completed, 3);
    assert_eq!(table_rows(&one).await, 0);
    assert_eq!(log_counts(&one).await, (3, 3));
    assert_eq!(log_counts(&two).await, (3, 3));
    assert_eq!(state(&one).await, "idle|false|3|1|1|true");
    in_one.finish().await;
    in_two.finish().await;
    one.cleanup().await.unwrap();
    two.cleanup().await.unwrap();
}
