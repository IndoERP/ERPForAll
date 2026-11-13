//! Helpers of the authentication tests: a database with `base` installed far
//! enough to log in (`ir.config_parameter` defaults, the `password` column, the user type groups
//! and an administrator), an [`Auth`] service on its own hashing pool, and users with passwords.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers"
)]

#[path = "../users_support/mod.rs"]
mod users_support;

use std::net::IpAddr;
use std::str::FromStr;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub use users_support::*;

use erp_orm::model_hooks::call_init;
use erp_orm::{BeginOptions, Isolation, Orm, SUPERUSER_ID, Signal, StaticPolicy};
use erp_sdk::{Error, Transaction, Value};
use erp_security::auth::password::limits;
use erp_security::auth::{
    Auth, AuthConfig, ClientAddr, CredentialOptions, HashPool, HashPoolConfig, Login, LoginError,
    LoginRequest, SessionSecret,
};
use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use erp_testkit::tokio_postgres::{Config, NoTls};
use erp_testkit::{Harness, HarnessBuilder, database_url_from_env};

/// A database ready for logins, its service and pool.
pub struct World {
    pub h: Harness,
    pub auth: Auth,
    pub pool: &'static HashPool,
    pub admin: i64,
}

/// A hashing pool of its own (the dummy-verify and job counters of one test).
pub fn own_pool(config: HashPoolConfig) -> &'static HashPool {
    Box::leak(Box::new(HashPool::new(config)))
}

/// `base` and `ir.model.data` (the harness creates the core tables, the authentication ones
/// among them).
pub fn builder() -> HarnessBuilder {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .require_core_tables(erp_security::auth::core_tables())
        // key issuance and revocation write audited events: fixtures that open
        // plain transactions get an operation identity, as the RPC layer would attach one
        .identify_transactions()
}

/// Run the `init()` hooks the module system runs for `base`, and create the user type groups and
/// an administrator.
pub async fn prepare(h: &Harness) -> i64 {
    let mut tx = h.begin().await.unwrap();
    call_init(&mut tx, "ir.config_parameter").await.unwrap();
    call_init(&mut tx, "res.users").await.unwrap();
    let t = user_types_and_admin(&mut tx).await;
    tx.commit().await.unwrap();
    t.admin
}

/// The world with `config` and a pool made with `pool`.
pub async fn world_with(h: Harness, config: AuthConfig, pool: HashPoolConfig) -> World {
    let admin = prepare(&h).await;
    let pool = own_pool(pool);
    let auth = Auth::new(h.orm().clone(), config, pool).await.unwrap();
    World {
        h,
        auth,
        pool,
        admin,
    }
}

/// The default world.
pub async fn world() -> World {
    world_with(
        builder().build().await.expect("harness"),
        AuthConfig::DEFAULT,
        HashPoolConfig::DEFAULT,
    )
    .await
}

/// A user `login` with `password` (created by the system), returns its id.
pub async fn user_with_password(h: &Harness, login: &str, password: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id = create(
        &mut tx,
        "res.users",
        &[
            ("name", s(login)),
            ("login", s(login)),
            ("password", s(password)),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    id
}

/// Set an `ir.config_parameter` through the typed API, as the system.
pub async fn set_param(h: &Harness, key: &str, value: &str) {
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("ir.config_parameter").unwrap();
    let _: String = records
        .call(&mut tx, "set_str", (key.to_owned(), Some(value.to_owned())))
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Make the settings cache of `h`'s ORM stale: a write of a key the service does not read moves
/// the `config_parameter` generation, so the next caller reloads (`Auth::new` caches what it
/// read, rust review round 3 RD-7).
pub async fn make_settings_stale(h: &Harness) {
    set_param(h, "erp_test.settings_stale", "1").await;
}

/// The stored `password` column of user `id`.
pub async fn stored_hash(h: &Harness, id: i64) -> Option<String> {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query("SELECT password FROM res_users WHERE id = $1", &[&id])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

/// `SELECT count(*) FROM <from>`.
pub async fn count(h: &Harness, from: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(&format!("SELECT count(*) FROM {from}"), &[])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

/// The client address of the tests' in-process logins.
pub const IN_PROCESS: ClientAddr = ClientAddr::InProcess("test");

/// Log in `login` / `password` in process (no client address).
pub async fn login(auth: &Auth, login: &str, password: &str) -> Result<Login, LoginError> {
    auth.authenticate(LoginRequest::new(login, password, IN_PROCESS))
        .await
}

/// Log in from `addr`.
pub async fn login_from(
    auth: &Auth,
    login: &str,
    password: &str,
    addr: &str,
) -> Result<Login, LoginError> {
    let addr: IpAddr = addr.parse().unwrap();
    auth.authenticate(LoginRequest::new(login, password, ClientAddr::Known(addr)))
        .await
}

/// A session check with the default options, ended at once; the user.
pub async fn check_user(auth: &Auth, secret: &SessionSecret) -> Result<i64, erp_sdk::Error> {
    let begun = auth.begin_session(secret, CredentialOptions::new()).await?;
    let mut tx = begun.tx;
    tx.rollback().await?;
    Ok(begun.session.user().get())
}

/// A pool of `size` connections to `h`'s database.
pub fn pool_of(h: &Harness, size: usize) -> Pool {
    pool_with_config(h, size, |_| {})
}

/// A pool of `size` connections to `h`'s database, each started with the libpq `options`
/// (e.g. `-c deadlock_timeout=5s`; a superuser-only setting needs a superuser role).
pub fn pool_with_options(h: &Harness, size: usize, options: &str) -> Pool {
    pool_with_config(h, size, |config| {
        config.options(options);
    })
}

fn pool_with_config(h: &Harness, size: usize, adjust: impl FnOnce(&mut Config)) -> Pool {
    let mut config = Config::from_str(&database_url_from_env().unwrap()).unwrap();
    config.dbname(h.db().name());
    adjust(&mut config);
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

/// An ORM with signals on `pool` over `h`'s registry (the signal table must be installed).
pub async fn orm_on(h: &Harness, pool: Pool) -> Orm {
    let client = h.db().client().await.unwrap();
    let baseline = Signal::read(&client).await.unwrap();
    Orm::builder(Arc::clone(h.registry()), pool)
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(baseline))
        .trust_caller_resolutions()
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap()
}

/// A node on `h`'s database: its own ORM with signals on a pool of `connections`, and an `Auth`
/// hashing on `pool` (the signal table must be installed).
pub async fn node_with(h: &Harness, connections: usize, pool: &'static HashPool) -> Auth {
    let orm = orm_on(h, pool_of(h, connections)).await;
    Auth::new(orm, AuthConfig::DEFAULT, pool).await.unwrap()
}

/// A pool with `permits` permits and a 30 s queue wait (tests that hold its permits).
pub fn waiting_pool(permits: u32) -> &'static HashPool {
    own_pool(HashPoolConfig::new(permits, 30, limits::ARGON2_MEMORY_KIB_MIN).unwrap())
}

/// Longest a helper waits for a permit holder to start: past the longest queue wait a pool may
/// have (60 s), so a holder that never gets its permit is reported by its own error first.
const HOLDER_START_LIMIT: Duration = Duration::from_secs(90);

/// Every permit of `pool` held by a job on a blocking thread until the returned sender is used
/// (or dropped): `permits` jobs, started before this returns.
///
/// # Panics
///
/// When a holder's `run` returns without running its job (its queue wait expired: the pool
/// dropped the job, so it can never start) or none started within [`HOLDER_START_LIMIT`]. Before
/// this, the helper polled forever for a start that could not come: a test that queued a holder
/// behind a verify longer than the pool's queue wait hung for hours (round 2).
pub async fn hold_permits(pool: &'static HashPool, permits: u32) -> mpsc::Sender<()> {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    for _ in 0..permits {
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::clone(&release_rx);
        let holder = tokio::spawn(pool.run(limits::ARGON2_MEMORY_KIB_MIN, move || {
            started_tx.send(()).unwrap();
            // a dropped sender ends the wait too
            release.lock().unwrap().recv().ok();
        }));
        let waited = Instant::now();
        loop {
            match started_rx.try_recv() {
                Ok(()) => break,
                // the job was dropped unrun: `run` returned an error
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!(
                        "a permit holder never started: {:?} ({:?})",
                        holder.await,
                        pool.stats()
                    )
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(
                waited.elapsed() < HOLDER_START_LIMIT,
                "a permit holder did not start within {HOLDER_START_LIMIT:?}: {:?}",
                pool.stats()
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    release_tx
}

/// Wait (yielding to the runtime) until `pool` has `n` callers waiting for a permit.
pub async fn until_waiting(pool: &HashPool, n: u64) {
    let started = Instant::now();
    while pool.stats().waiting < n {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{n} waiters never came: {:?}",
            pool.stats()
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Wait until some backend of `h`'s database waits for a lock.
pub async fn until_lock_wait(h: &Harness) {
    let client = h.db().client().await.unwrap();
    let started = Instant::now();
    loop {
        let row = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                  WHERE datname = current_database() AND wait_event_type = 'Lock'",
                &[],
            )
            .await
            .unwrap();
        if row.get::<_, i64>(0) > 0 {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no backend waited for a lock"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// The secret of a login's cookie, as a client would send it back.
pub fn cookie(login: &Login) -> SessionSecret {
    SessionSecret::from_cookie(&login.session().secret().to_cookie()).unwrap()
}

/// Write `vals` on records `ids` of `model` in `tx`.
pub async fn write(
    tx: &mut Transaction,
    model: &str,
    ids: &[i64],
    pairs: &[(&str, Value)],
) -> Result<(), erp_sdk::Error> {
    records(tx, model, ids).write_values(tx, &map(pairs)).await
}

/// The `warn` lines of target `erp::auth` written on this thread while a [`WarnCapture`] lives
/// (`tracing` has no capture of its own and the workspace takes no subscriber crate for it). A
/// `#[tokio::test]` runs on one thread, its spawned tasks included.
pub mod warn_capture {
    use std::fmt;
    use std::sync::{Arc, Mutex, PoisonError};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

    /// The lines kept so far; dropping it ends the capture.
    pub struct WarnCapture {
        lines: Arc<Mutex<Vec<String>>>,
        _guard: tracing::subscriber::DefaultGuard,
    }

    impl WarnCapture {
        /// The lines kept since the last call.
        pub fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.lines.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    struct Capture(Arc<Mutex<Vec<String>>>);

    struct Message(Option<String>);

    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }

    impl Subscriber for Capture {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target() == "erp::auth" && *metadata.level() == Level::WARN
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut message = Message(None);
            event.record(&mut message);
            if let Some(line) = message.0 {
                self.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(line);
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    /// Start keeping this thread's `erp::auth` `warn` lines.
    pub fn start() -> WarnCapture {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let guard = tracing::subscriber::set_default(Capture(Arc::clone(&lines)));
        WarnCapture {
            lines,
            _guard: guard,
        }
    }
}

/// The initial pool `deadlock_timeout` used by the login side, ms (round 6
/// C5-1): the pool of [`deadlock_world`] starts each connection with it. Below the ORM's default
/// `lock_timeout` (10 s), so the login's deadlock check runs before its lock limit can fire.
pub const DEADLOCK_TIMEOUT_MS: f64 = 5000.0;

/// The world of a forced deadlock (round 6 C5-1): an `Auth` and the other side's ORM on one pool
/// whose every connection starts with `deadlock_timeout` = [`DEADLOCK_TIMEOUT_MS`], so the login's
/// backend and the other side's run with the same raised timeout (the test role may set it, as a
/// superuser; checked on erpforall-pg 2026-10-23, round 6).
/// The helper locally overrides its detector afterward; login connections keep this setting.
pub async fn deadlock_world() -> (Harness, Arc<Auth>, Orm) {
    let w = world().await;
    let pool = pool_with_options(&w.h, 4, "-c deadlock_timeout=5s");
    let orm = orm_on(&w.h, pool).await;
    let auth = Auth::new(
        orm.clone(),
        AuthConfig::DEFAULT,
        own_pool(HashPoolConfig::DEFAULT),
    )
    .await
    .unwrap();
    (w.h, Arc::new(auth), orm)
}

/// The helper's detector runs after the login's 5 s detector. Equal detectors can choose
/// the helper as the victim even though both sessions reached the arranged cycle; a failed
/// helper alone does not measure a client scheduling stall. Its lock timeout leaves room for
/// its detector, without changing the login's production retry or timeout behavior.
pub const OTHER_DEADLOCK_TIMEOUT_MS: f64 = 30_000.0;

/// A READ COMMITTED transaction on `orm`, as the system: the other side of a forced deadlock.
pub async fn other_side(orm: &Orm) -> erp_sdk::Transaction {
    let mut tx = orm
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let initial: f64 = tx
        .query(
            "SELECT setting::float8 FROM pg_settings WHERE name = 'deadlock_timeout'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        initial, DEADLOCK_TIMEOUT_MS,
        "the login pool's detector timeout"
    );
    tx.execute("SET LOCAL deadlock_timeout = '30s'", &[])
        .await
        .unwrap();
    tx.execute("SET LOCAL lock_timeout = '45s'", &[])
        .await
        .unwrap();
    let lock_timeout: f64 = tx
        .query(
            "SELECT setting::float8 FROM pg_settings WHERE name = 'lock_timeout'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        lock_timeout, 45_000.0,
        "helper lock timeout follows its detector"
    );
    tx
}

/// The result of the other side's blocking request (round 6 C5-2): its error means the other
/// side, not the login, was refused by the deadlock check, so the panic says so instead of
/// unwrapping a bare 40P01.
pub fn the_other_side_won<T>(result: Result<T, Error>) -> T {
    result.unwrap_or_else(|e| {
        panic!(
            "the other side was the deadlock victim (sqlstate {:?}): the arranged login-victim \
             ordering was not reached: {e}",
            e.sqlstate()
        )
    })
}

/// How long the backend that waits for a lock this transaction holds has waited, ms
/// (`pg_locks` `waitstart`, server clock), and this transaction's own `deadlock_timeout`, ms;
/// `None` while none waits or its wait start is not set yet. `pg_locks` and `pg_blocking_pids`
/// are read live at each statement (`pg_stat_activity` is not: a transaction keeps its first
/// snapshot of it).
pub const LOCK_WAIT_SQL: &str = "\
SELECT (extract(epoch FROM clock_timestamp() - min(l.waitstart)) * 1000)::float8,
       (SELECT setting::float8 FROM pg_settings WHERE name = 'deadlock_timeout')
  FROM pg_locks l
 WHERE NOT l.granted AND l.waitstart IS NOT NULL
   AND pg_backend_pid() = ANY (pg_blocking_pids(l.pid))";

/// Round 4 C4-3, round 6 C5-1 and C5-3: the gate of a forced deadlock, run by the other side
/// right before its blocking request. It waits until the login waits for a lock the other side
/// holds (`pg_locks`, its wait start set), then checks that the login has waited less than half
/// of the login backend's `deadlock_timeout`.
///
/// `pg_settings` here is the helper's session: assert its 30 s detector override. The
/// login remains on the pool's 5 s setting, checked before the helper's local override.
/// The wait admission below still uses half of the login's 5 s detector window.
///
/// Why half: the login's deadlock check runs once, its own `deadlock_timeout` (dL) after it
/// started waiting, and on a cycle the backend that runs the check is the one refused. The login
/// is the victim when the other side's request reaches its lock wait before that check. A request
/// that arrives later makes the other side the victim whatever the other side's own timeout is
/// (its check then finds the cycle first), so the admission window remains dL alone.
/// The helper's later detector removes the equal-timer victim race. The residual timing
/// assumption: the other side's next statement reaches its lock wait within dL - waited (at least
/// dL/2 = 2.5 s, one round trip on the same connection). A wrong victim still fails loudly;
/// that outcome alone does not measure the size or cause of a scheduling delay.
pub async fn until_the_login_waits(other: &mut erp_sdk::Transaction) {
    let started = std::time::Instant::now();
    loop {
        let rows = other.query(LOCK_WAIT_SQL, &[]).await.unwrap();
        let waited: Option<f64> = rows[0].get(0);
        let timeout: f64 = rows[0].get(1);
        assert_eq!(
            timeout, OTHER_DEADLOCK_TIMEOUT_MS,
            "the helper detector must run after the login detector"
        );
        if let Some(waited) = waited {
            assert!(
                waited < DEADLOCK_TIMEOUT_MS / 2.0,
                "the login waited {waited} ms before the other side's request: \
                 beyond half of its 5000 ms deadlock_timeout"
            );
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the login never waited for a lock"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

// ---- race gates: every gate polls with a deadline that panics ----

/// The advisory-lock key of the success pause ([`install_success_pause`]).
pub const PAUSE_KEY: i64 = 0x4c52_0001;

/// Wait until some backend of `h`'s database waits for a relation lock of `mode` (a
/// `pg_locks.mode` name such as `ShareRowExclusiveLock`) on `relation`; `n` waiters at least.
pub async fn until_relation_waiter(h: &Harness, relation: &str, mode: &str, n: i64) {
    let client = h.db().client().await.unwrap();
    let started = Instant::now();
    loop {
        let row = client
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE NOT granted AND locktype = 'relation' \
                    AND relation = $1::text::regclass AND mode = $2",
                &[&relation, &mode],
            )
            .await
            .unwrap();
        if row.get::<_, i64>(0) >= n {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{n} backend(s) never waited for {mode} on {relation}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A transaction that holds the advisory lock `key` until it is committed or rolled back.
pub async fn advisory_gate(h: &Harness, key: i64) -> Transaction {
    let mut tx = h.begin().await.unwrap();
    tx.query("SELECT pg_advisory_xact_lock($1)", &[&key])
        .await
        .unwrap();
    tx
}

/// Wait until some backend waits for the advisory lock `key`.
pub async fn until_advisory_waiter(h: &Harness, key: i64) {
    let client = h.db().client().await.unwrap();
    let started = Instant::now();
    loop {
        let row = client
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE NOT granted AND locktype = 'advisory' \
                    AND ((classid::bigint << 32) | objid::bigint) = $1",
                &[&key],
            )
            .await
            .unwrap();
        if row.get::<_, i64>(0) > 0 {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no backend waited for the advisory lock {key}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Commit a trigger that makes every insert into `erp_session` wait for the advisory lock
/// [`PAUSE_KEY`]: with an [`advisory_gate`] held, a login's success statement pauses inside its
/// transaction (after the selection guard, before the session row).
pub async fn install_success_pause(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "CREATE FUNCTION pause_session_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         PERFORM pg_advisory_xact_lock({PAUSE_KEY}); RETURN NEW; END $$; \
         CREATE TRIGGER pause_session_insert BEFORE INSERT ON erp_session \
         FOR EACH ROW EXECUTE FUNCTION pause_session_insert()"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Wait until no other backend of `h`'s database is in a transaction or running a statement, and
/// no lock request is pending: nothing a cancelled login left behind.
pub async fn until_database_quiet(h: &Harness) {
    let client = h.db().client().await.unwrap();
    let started = Instant::now();
    loop {
        let busy: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
                    AND pid <> pg_backend_pid() AND state IN ('active', 'idle in transaction', \
                    'idle in transaction (aborted)')",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        let waiting: i64 = client
            .query_one("SELECT count(*) FROM pg_locks WHERE NOT granted", &[])
            .await
            .unwrap()
            .get(0);
        if busy == 0 && waiting == 0 {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the database never went quiet: {busy} busy backend(s), {waiting} lock waiter(s)"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Wait until `from` (a `FROM ... WHERE ...` tail as [`count`] takes) counts `expected` rows.
pub async fn until_count(h: &Harness, from: &str, expected: i64) {
    let started = Instant::now();
    loop {
        let found = count(h, from).await;
        if found == expected {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{from} counted {found}, expected {expected}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// What an installed `base` has and these databases lack, for the company-scoped password reset
/// (Amendment 1): the generated company restriction row of `res.users` (the
/// session's compiled policy reports a missing row as drift and refuses the model) and one active
/// company, which every user the system creates afterwards gets as its default and only company.
/// Call it before creating the users of the test. Returns the company.
pub async fn company_scope(h: &Harness) -> i64 {
    let mut tx = h.begin().await.unwrap();
    erp_orm::reflection::reflect_models(&mut tx).await.unwrap();
    let all = erp_security::company_access::generate(h.registry()).unwrap();
    let users = all
        .iter()
        .find(|g| g.model_name() == "res.users")
        .expect("res.users declares a generated company restriction");
    let present: i64 = tx
        .query(
            "SELECT count(*) FROM ir_access a JOIN ir_model m ON m.id = a.model_id \
              WHERE m.model = 'res.users' AND a.generated IS TRUE",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    if present == 0 {
        tx.execute(
            "INSERT INTO ir_access (name, model_id, active, operation, domain, generated) \
             SELECT $1, id, true, 'crud', $2, true FROM ir_model WHERE model = 'res.users'",
            &[&users.name(), &users.text()],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    add_company(h, "Main company").await
}

/// A new active company `name` (with a currency of its own).
pub async fn add_company(h: &Harness, name: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let currency: i64 = tx
        .query(
            "INSERT INTO res_currency (name, symbol, rounding, active) \
             VALUES (left(md5($1), 3), 'T', 0.01, true) RETURNING id",
            &[&name],
        )
        .await
        .unwrap()[0]
        .get(0);
    let id = tx
        .env()
        .records("res.company")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[("name", s(name)), ("currency_id", int(currency))])],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    id
}

/// Put `user` in `companies` alone, the first as its default company (setup by SQL in one
/// transaction that also updates the `res_users` row, as a form save that moves a user does).
pub async fn set_companies(h: &Harness, user: i64, companies: &[i64]) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE res_users SET company_id = $2 WHERE id = $1",
        &[&user, &companies[0]],
    )
    .await
    .unwrap();
    tx.execute(
        "DELETE FROM res_company_users_rel WHERE user_id = $1",
        &[&user],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO res_company_users_rel (cid, user_id) SELECT unnest($2::int8[]), $1",
        &[&user, &companies],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// The id of the group bound to `base.<name>`.
pub async fn base_group(h: &Harness, name: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id: i64 = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = $1",
            &[&name],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    id
}

/// A user `login` with `password` and the direct groups `groups` (created by the system).
pub async fn user_in_groups(h: &Harness, login: &str, password: &str, groups: &[i64]) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id = create(
        &mut tx,
        "res.users",
        &[
            ("name", s(login)),
            ("login", s(login)),
            ("password", s(password)),
            ("group_ids", set_cmd(groups)),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    id
}
