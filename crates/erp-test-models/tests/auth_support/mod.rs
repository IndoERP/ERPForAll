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
use erp_orm::{Orm, Signal, StaticPolicy};
use erp_sdk::{Transaction, Value};
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
