//! The hashing pool: password hashes run on blocking threads, at
//! most `permits` at once per process, so a login flood can neither starve the async runtime nor
//! take more memory than the process reserved for hashing (`permits × memory_kib`).
//!
//! A caller waits for a permit at most `queue_wait`; then it gets
//! [`HashPoolError::Overloaded`] (answers 503 with `Retry-After`). The permit
//! moves into the blocking task and is released when the hash returns, also when the caller's
//! future was dropped meanwhile, so cancelled requests never let more hashes run than permits.
//!
//! The process has one pool ([`HashPool::global`]); the server configures it once at startup
//! ([`HashPool::configure`])'s `admission.hash_*` settings, before the first hash.
//! Every hash of an ORM lineage (one `Orm` and its clones and derived handles) runs on one pool:
//! `Auth::new` registers the pool it is handed for its ORM ([`register_for`]), and code that
//! hashes inside a transaction of that lineage (setting a password, `res.users.init`) finds it
//! with [`for_env`]; a lineage without an `Auth` (data loading before the server starts) hashes on
//! the process pool (rust review Low: the bound of "at most N hashes at once" is never split
//! across two pools). The rule is per lineage, not per database: two separately built ORMs of one
//! database are two lineages, so a server builds one ORM per database (types-delta N-f).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use tokio::sync::Semaphore;

use super::password::limits;

/// The pool's settings, validated (`admission.hash_concurrency`,
/// `admission.hash_queue_wait_secs`; `admission.hash_memory_kib` from §14.5 of the design note).
///
/// ```
/// use erp_security::auth::pool::HashPoolConfig;
/// let c = HashPoolConfig::new(2, 5, 19_456)?;
/// assert_eq!(c.reserved_bytes(), 2 * 19_456 * 1024);
/// assert_eq!(
///     HashPoolConfig::new(0, 5, 19_456).unwrap_err().to_string(),
///     "admission.hash_concurrency = 0 is outside 1..=256"
/// );
/// # Ok::<(), erp_security::auth::pool::HashPoolError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashPoolConfig {
    permits: u32,
    queue_wait: Duration,
    memory_kib: u32,
}

/// Largest permit count accepted (a process has at most this many hashing threads at once).
pub const MAX_PERMITS: u32 = 256;

impl HashPoolConfig {
    /// Defaults and the design note: 2 permits, 5 s wait, 19 MiB per permit.
    pub const DEFAULT: Self = Self {
        permits: 2,
        queue_wait: Duration::from_secs(5),
        memory_kib: limits::ARGON2_MEMORY_KIB_MIN,
    };

    /// `permits` hashes at once (1 – 256; the server also bounds it by the CPU count), a waiter
    /// gives up after `queue_wait_secs` (1 – 60), each hash may use up to `memory_kib`
    /// (19,456 – 1,048,576).
    ///
    /// # Errors
    ///
    /// [`HashPoolError::OutOfRange`] naming the setting.
    pub fn new(permits: u32, queue_wait_secs: u32, memory_kib: u32) -> Result<Self, HashPoolError> {
        range("admission.hash_concurrency", permits, 1, MAX_PERMITS)?;
        range("admission.hash_queue_wait_secs", queue_wait_secs, 1, 60)?;
        range(
            "admission.hash_memory_kib",
            memory_kib,
            limits::ARGON2_MEMORY_KIB_MIN,
            limits::ARGON2_MEMORY_KIB_MAX,
        )?;
        Ok(Self {
            permits,
            queue_wait: Duration::from_secs(u64::from(queue_wait_secs)),
            memory_kib,
        })
    }

    /// Hashes at once.
    pub const fn permits(&self) -> u32 {
        self.permits
    }

    /// Longest wait for a permit.
    pub const fn queue_wait(&self) -> Duration {
        self.queue_wait
    }

    /// Memory one hash may use, KiB.
    pub const fn memory_kib(&self) -> u32 {
        self.memory_kib
    }

    /// What the process reserves for hashing: `permits × memory_kib` bytes (the machine view's
    /// line, checked by the server against a quarter of `process.memory_soft_bytes`).
    pub const fn reserved_bytes(&self) -> u64 {
        self.permits as u64 * self.memory_kib as u64 * 1024
    }
}

fn range(setting: &'static str, value: u32, min: u32, max: u32) -> Result<(), HashPoolError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(HashPoolError::OutOfRange {
            setting,
            value,
            min,
            max,
        })
    }
}

/// Counters of a pool (monotonic since the pool was made).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct HashPoolStats {
    /// Jobs that ran (hashes and verifies, dummy ones included).
    pub jobs: u64,
    /// Verifies of a dummy hash (unknown or inactive logins, empty passwords).
    pub dummy_verifies: u64,
    /// Waits that expired ([`HashPoolError::Overloaded`]).
    pub overloaded: u64,
    /// Callers waiting for a permit right now (a gauge, not monotonic).
    pub waiting: u64,
    /// Denials whose cost-floor dummy verify could not run (the denial stood and was counted).
    pub floor_skipped: u64,
    /// Of [`HashPoolStats::floor_skipped`], those that were not an expired wait (a panic, an
    /// over-reservation, any other error): logged at `error`, the floor is off for that user
    /// until fixed (silent-delta S-1).
    pub floor_failures: u64,
    /// Hashes refused because they need more memory than a permit reserves (a configuration
    /// fault: `password.hashing.argon2_memory_kib` against `admission.hash_memory_kib`).
    pub over_reservation: u64,
    /// Refusals of a stored hash over the reservation whose padding dummy could not run (logged
    /// at `warn` naming `admission.hash_memory_kib`; the attempt is then answered as an unknown
    /// login's whose dummy could not run; security review round 3 R3-a, R3-b).
    pub over_reservation_pad_skipped: u64,
}

#[derive(Debug, Default)]
struct Counters {
    jobs: AtomicU64,
    dummy_verifies: AtomicU64,
    overloaded: AtomicU64,
    waiting: AtomicU64,
    floor_skipped: AtomicU64,
    floor_failures: AtomicU64,
    over_reservation: AtomicU64,
    over_reservation_pad_skipped: AtomicU64,
}

/// Counts one waiter while alive (also when the waiting future is dropped).
struct Waiting<'a>(&'a AtomicU64);

impl<'a> Waiting<'a> {
    fn new(gauge: &'a AtomicU64) -> Self {
        gauge.fetch_add(1, Ordering::Relaxed);
        Self(gauge)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A bounded pool of hashing permits over Tokio's blocking threads.
#[derive(Debug)]
pub struct HashPool {
    config: HashPoolConfig,
    permits: Arc<Semaphore>,
    counters: Counters,
}

static GLOBAL: OnceLock<HashPool> = OnceLock::new();

impl HashPool {
    /// A pool with `config` (tests, and the process pool through [`HashPool::configure`]).
    pub fn new(config: HashPoolConfig) -> Self {
        Self {
            config,
            permits: Arc::new(Semaphore::new(config.permits as usize)),
            counters: Counters::default(),
        }
    }

    /// Set the process pool's settings. Called once by the server at startup, before any hash.
    ///
    /// # Errors
    ///
    /// [`HashPoolError::AlreadyConfigured`] when the process pool exists with other settings
    /// (configured twice, or used before it was configured).
    pub fn configure(config: HashPoolConfig) -> Result<&'static HashPool, HashPoolError> {
        let pool = GLOBAL.get_or_init(|| HashPool::new(config));
        if pool.config == config {
            Ok(pool)
        } else {
            Err(HashPoolError::AlreadyConfigured {
                existing: pool.config,
            })
        }
    }

    /// The process pool; with [`HashPoolConfig::DEFAULT`] when nothing configured it first.
    pub fn global() -> &'static HashPool {
        GLOBAL.get_or_init(|| HashPool::new(HashPoolConfig::DEFAULT))
    }

    /// The settings.
    pub fn config(&self) -> HashPoolConfig {
        self.config
    }

    /// The counters.
    pub fn stats(&self) -> HashPoolStats {
        HashPoolStats {
            jobs: self.counters.jobs.load(Ordering::Relaxed),
            dummy_verifies: self.counters.dummy_verifies.load(Ordering::Relaxed),
            overloaded: self.counters.overloaded.load(Ordering::Relaxed),
            waiting: self.counters.waiting.load(Ordering::Relaxed),
            floor_skipped: self.counters.floor_skipped.load(Ordering::Relaxed),
            floor_failures: self.counters.floor_failures.load(Ordering::Relaxed),
            over_reservation: self.counters.over_reservation.load(Ordering::Relaxed),
            over_reservation_pad_skipped: self
                .counters
                .over_reservation_pad_skipped
                .load(Ordering::Relaxed),
        }
    }

    pub(crate) fn count_dummy_verify(&self) {
        self.counters.dummy_verifies.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_floor_skipped(&self) {
        self.counters.floor_skipped.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_over_reservation_pad_skipped(&self) {
        self.counters
            .over_reservation_pad_skipped
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_floor_failure(&self) {
        self.counters.floor_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Run `job`, which needs up to `memory_kib`, on a blocking thread under a permit.
    ///
    /// # Errors
    ///
    /// [`HashPoolError::MemoryOverReservation`] when `memory_kib` is above the pool's reservation
    /// per permit (nothing runs); [`HashPoolError::Overloaded`] when no permit came within the
    /// queue wait; [`HashPoolError::Closed`] or [`HashPoolError::Panicked`] when the runtime is
    /// shutting down or the job panicked.
    pub async fn run<T, F>(&self, memory_kib: u32, job: F) -> Result<T, HashPoolError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        if memory_kib > self.config.memory_kib {
            self.counters
                .over_reservation
                .fetch_add(1, Ordering::Relaxed);
            return Err(HashPoolError::MemoryOverReservation {
                needed_kib: memory_kib,
                reserved_kib: self.config.memory_kib,
            });
        }
        let acquire = Arc::clone(&self.permits).acquire_owned();
        let waiting = Waiting::new(&self.counters.waiting);
        let acquired = tokio::time::timeout(self.config.queue_wait, acquire).await;
        drop(waiting);
        let permit = match acquired {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(HashPoolError::Closed),
            Err(_) => {
                self.counters.overloaded.fetch_add(1, Ordering::Relaxed);
                return Err(HashPoolError::Overloaded {
                    retry_after_secs: self.config.queue_wait.as_secs().max(1),
                });
            }
        };
        self.counters.jobs.fetch_add(1, Ordering::Relaxed);
        let handle = tokio::task::spawn_blocking(move || {
            // held until the job returns, whatever happens to the caller
            let _permit = permit;
            job()
        });
        handle.await.map_err(|e| {
            if e.is_panic() {
                HashPoolError::Panicked
            } else {
                HashPoolError::Closed
            }
        })
    }
}

struct LineageBinding {
    env: erp_orm::Env,
    pool: &'static HashPool,
    admission: OnceLock<(usize, Arc<Semaphore>)>,
    /// One permit: the API key last-use writes of this lineage, one at a time.
    last_use_gate: Arc<Semaphore>,
}

fn registered() -> &'static std::sync::Mutex<Vec<Weak<LineageBinding>>> {
    static REGISTERED: OnceLock<std::sync::Mutex<Vec<Weak<LineageBinding>>>> = OnceLock::new();
    REGISTERED.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// An owned lease on one ORM lineage's hashing pool. Clone it for work handed to another task.
/// [`Self::run`] retains the lease inside blocking work, even after its awaiting future is
/// cancelled. The last service, queued job or running hash releases the binding. The weak
/// registry retains neither ORM pools nor environments after all leases are gone.
#[derive(Clone)]
#[non_exhaustive]
pub struct PoolBinding {
    inner: Arc<LineageBinding>,
}

impl fmt::Debug for PoolBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolBinding")
            .field("config", &self.config())
            .finish_non_exhaustive()
    }
}

impl PoolBinding {
    pub(crate) fn raw(&self) -> &'static HashPool {
        self.inner.pool
    }
    /// The leased pool's validated configuration.
    pub fn config(&self) -> HashPoolConfig {
        self.inner.pool.config()
    }

    /// Counters of the shared pool, including work outside this one lineage.
    pub fn stats(&self) -> HashPoolStats {
        self.inner.pool.stats()
    }

    /// Run a job with the same bounds and errors as [`HashPool::run`], retaining this lineage
    /// until the actual blocking job finishes, not merely until its caller stops awaiting.
    ///
    /// # Errors
    ///
    /// The reservation, queue-wait and blocking-task errors of [`HashPool::run`].
    pub async fn run<T, F>(&self, memory_kib: u32, job: F) -> Result<T, HashPoolError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let lease = Arc::clone(&self.inner);
        self.inner
            .pool
            .run(memory_kib, move || {
                let _lease = lease;
                job()
            })
            .await
    }

    pub(crate) fn admission(&self, permits: usize) -> erp_fields::Result<Arc<Semaphore>> {
        let (size, gate) = self
            .inner
            .admission
            .get_or_init(|| (permits, Arc::new(Semaphore::new(permits))));
        if *size != permits {
            return Err(erp_fields::Error::programming(
                "authentication admission capacity changed within an ORM lineage",
            ));
        }
        Ok(Arc::clone(gate))
    }

    /// The one-permit gate of the API key last-use writes, shared by every `Auth` of the lineage.
    pub(crate) fn last_use_gate(&self) -> Arc<Semaphore> {
        Arc::clone(&self.inner.last_use_gate)
    }

    pub(crate) fn count_dummy_verify(&self) {
        self.inner.pool.count_dummy_verify();
    }
    pub(crate) fn count_floor_skipped(&self) {
        self.inner.pool.count_floor_skipped();
    }
    pub(crate) fn count_floor_failure(&self) {
        self.inner.pool.count_floor_failure();
    }
    pub(crate) fn count_over_reservation_pad_skipped(&self) {
        self.inner.pool.count_over_reservation_pad_skipped();
    }
}

/// Register `pool` as the hashing pool of `orm`'s lineage (type design T7: one pool per ORM
/// lineage, so "at most N hashes" is one bound; module doc for why lineage, not database).
///
/// # Errors
///
/// `Programming` when the lineage already hashes on another pool (another `Auth` of the same
/// ORM lineage was handed a different one); the same pool again is accepted.
pub(crate) fn register_for(
    orm: &erp_orm::Orm,
    pool: &'static HashPool,
) -> erp_fields::Result<PoolBinding> {
    bind_env(&orm.env(erp_orm::SUPERUSER_ID, Default::default()), pool)
}

pub(crate) fn bind_env(
    env: &erp_orm::Env,
    pool: &'static HashPool,
) -> erp_fields::Result<PoolBinding> {
    let mut all = registered()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    all.retain(|r| r.strong_count() != 0);
    if let Some(inner) = all
        .iter()
        .filter_map(Weak::upgrade)
        .find(|r| env.shares_lineage(&r.env))
    {
        if !std::ptr::eq(inner.pool, pool) {
            return Err(erp_fields::Error::programming(
                "this ORM lineage already hashes on another hashing pool (an Auth of the same ORM \
             was handed a different one): every Auth of an ORM lineage takes the same pool",
            ));
        }
        return Ok(PoolBinding { inner });
    }
    Ok(insert_binding(&mut all, env, pool))
}

fn insert_binding(
    all: &mut Vec<Weak<LineageBinding>>,
    env: &erp_orm::Env,
    pool: &'static HashPool,
) -> PoolBinding {
    let inner = Arc::new(LineageBinding {
        env: env.clone(),
        pool,
        admission: OnceLock::new(),
        last_use_gate: Arc::new(Semaphore::new(1)),
    });
    all.push(Arc::downgrade(&inner));
    PoolBinding { inner }
}

/// Lease the pool of `env`'s ORM lineage, or bind it to the process pool before its first
/// service. Keep the returned handle through queued and running work; use [`PoolBinding::run`]
/// to preserve the lease when its awaiting future is cancelled.
pub fn for_env(env: &erp_orm::Env) -> PoolBinding {
    let mut all = registered()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    all.retain(|r| r.strong_count() != 0);
    if let Some(inner) = all
        .iter()
        .filter_map(Weak::upgrade)
        .find(|r| env.shares_lineage(&r.env))
    {
        return PoolBinding { inner };
    }
    insert_binding(&mut all, env, HashPool::global())
}

/// The hashing pool's error in `e`'s source chain, if any.
pub(crate) fn pool_error(e: &erp_fields::Error) -> Option<&HashPoolError> {
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        if let Some(pool) = s.downcast_ref::<HashPoolError>() {
            return Some(pool);
        }
        source = s.source();
    }
    None
}

/// Why the pool could not run a hash.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HashPoolError {
    /// A setting is outside its bounds.
    OutOfRange {
        /// The setting.
        setting: &'static str,
        /// The value given.
        value: u32,
        /// The floor.
        min: u32,
        /// The hard bound.
        max: u32,
    },
    /// The process pool already exists with other settings.
    AlreadyConfigured {
        /// Its settings.
        existing: HashPoolConfig,
    },
    /// A hash needs more memory than the pool reserves per permit.
    MemoryOverReservation {
        /// What the hash needs, KiB.
        needed_kib: u32,
        /// What the pool reserves per permit, KiB.
        reserved_kib: u32,
    },
    /// No permit within the queue wait.
    Overloaded {
        /// The `Retry-After` the server sends.
        retry_after_secs: u64,
    },
    /// The runtime is shutting down.
    Closed,
    /// The hashing job panicked.
    Panicked,
}

impl fmt::Display for HashPoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange {
                setting,
                value,
                min,
                max,
            } => write!(f, "{setting} = {value} is outside {min}..={max}"),
            Self::AlreadyConfigured { existing } => write!(
                f,
                "the hashing pool is already configured ({} permits, {} s wait, {} KiB per \
                 permit); configure it once, before the first hash",
                existing.permits,
                existing.queue_wait.as_secs(),
                existing.memory_kib
            ),
            Self::MemoryOverReservation {
                needed_kib,
                reserved_kib,
            } => write!(
                f,
                "a password hash needs {needed_kib} KiB, more than the {reserved_kib} KiB the \
                 hashing pool reserves per permit (password.hashing.argon2_memory_kib against \
                 admission.hash_memory_kib)"
            ),
            // one plural rule with `LoginError::Overloaded` (rust review round 2 N-4)
            Self::Overloaded {
                retry_after_secs: 1,
            } => f.write_str("Server busy; retry after 1 second."),
            Self::Overloaded { retry_after_secs } => {
                write!(f, "Server busy; retry after {retry_after_secs} seconds.")
            }
            Self::Closed => f.write_str("the hashing pool is shutting down"),
            Self::Panicked => f.write_str("a password hashing job panicked"),
        }
    }
}

impl std::error::Error for HashPoolError {}

impl From<HashPoolError> for erp_fields::Error {
    fn from(e: HashPoolError) -> Self {
        use erp_fields::{Condition, ErrorKind};
        match &e {
            HashPoolError::Overloaded { .. } => {
                erp_fields::Error::new(ErrorKind::ResourceExhausted, e.to_string())
                    .with_condition(Condition::Overloaded)
                    .with_source(e)
            }
            HashPoolError::MemoryOverReservation { .. } | HashPoolError::OutOfRange { .. } => {
                erp_fields::Error::new(ErrorKind::Validation, e.to_string()).with_source(e)
            }
            HashPoolError::AlreadyConfigured { .. }
            | HashPoolError::Closed
            | HashPoolError::Panicked => {
                erp_fields::Error::new(ErrorKind::Programming, e.to_string()).with_source(e)
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn pool(permits: u32) -> &'static HashPool {
        Box::leak(Box::new(HashPool::new(
            HashPoolConfig::new(permits, 1, limits::ARGON2_MEMORY_KIB_MIN).unwrap(),
        )))
    }

    /// A job that runs until `release` gets a message (or is dropped); tells `started` first.
    fn held(started: mpsc::Sender<()>, release: mpsc::Receiver<()>) -> impl FnOnce() + Send {
        move || {
            started.send(()).unwrap();
            // a dropped sender ends the wait too
            release.recv().ok();
        }
    }

    /// permits + 1 callers: the extra one waits the queue wait, then is refused as overloaded;
    /// once a permit is free, the next caller runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_caller_past_the_permits_waits_then_is_overloaded() {
        let pool = pool(1);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first =
            tokio::spawn(pool.run(limits::ARGON2_MEMORY_KIB_MIN, held(started_tx, release_rx)));
        started_rx.recv().unwrap();
        let waited = std::time::Instant::now();
        let refused = pool
            .run(limits::ARGON2_MEMORY_KIB_MIN, || ())
            .await
            .unwrap_err();
        assert!(
            waited.elapsed() >= Duration::from_secs(1),
            "waited the queue wait"
        );
        assert_eq!(
            refused,
            HashPoolError::Overloaded {
                retry_after_secs: 1
            }
        );
        let error = erp_fields::Error::from(refused);
        assert_eq!(error.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(error.condition(), Some(erp_fields::Condition::Overloaded));
        assert_eq!(error.message(), "Server busy; retry after 1 second.");
        assert_eq!(
            HashPoolError::Overloaded {
                retry_after_secs: 5
            }
            .to_string(),
            "Server busy; retry after 5 seconds."
        );
        release_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        pool.run(limits::ARGON2_MEMORY_KIB_MIN, || ())
            .await
            .unwrap();
        let stats = pool.stats();
        assert_eq!((stats.jobs, stats.overloaded), (2, 1));
    }

    /// The waiting gauge counts a caller while it waits for a permit, and not after (also when
    /// its future is dropped).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_waiting_gauge_follows_the_waiters() {
        let pool = pool(1);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first =
            tokio::spawn(pool.run(limits::ARGON2_MEMORY_KIB_MIN, held(started_tx, release_rx)));
        started_rx.recv().unwrap();
        assert_eq!(pool.stats().waiting, 0);
        let second = tokio::spawn(pool.run(limits::ARGON2_MEMORY_KIB_MIN, || ()));
        while pool.stats().waiting == 0 {
            tokio::task::yield_now().await;
        }
        second.abort();
        assert!(second.await.unwrap_err().is_cancelled());
        assert_eq!(pool.stats().waiting, 0);
        release_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
    }

    /// A caller that gives up (its future dropped) keeps its permit until the job returns: no
    /// more jobs run than permits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_caller_holds_its_permit_until_the_job_returns() {
        let pool = pool(1);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first =
            tokio::spawn(pool.run(limits::ARGON2_MEMORY_KIB_MIN, held(started_tx, release_rx)));
        started_rx.recv().unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let refused = pool
            .run(limits::ARGON2_MEMORY_KIB_MIN, || ())
            .await
            .unwrap_err();
        assert!(matches!(refused, HashPoolError::Overloaded { .. }));
        release_tx.send(()).unwrap();
        // the blocking job returns and gives the permit back
        let mut ran = Err(HashPoolError::Closed);
        for _ in 0..50 {
            ran = pool.run(limits::ARGON2_MEMORY_KIB_MIN, || ()).await;
            if ran.is_ok() {
                break;
            }
        }
        ran.unwrap();
    }

    /// A hash that needs more memory than a permit reserves never runs.
    #[tokio::test]
    async fn a_hash_over_the_reservation_is_refused() {
        let pool = pool(1);
        let e = pool
            .run(limits::ARGON2_MEMORY_KIB_MIN + 1, || panic!("ran"))
            .await
            .unwrap_err();
        assert_eq!(
            e,
            HashPoolError::MemoryOverReservation {
                needed_kib: limits::ARGON2_MEMORY_KIB_MIN + 1,
                reserved_kib: limits::ARGON2_MEMORY_KIB_MIN,
            }
        );
        assert_eq!(pool.stats().jobs, 0);
        assert_eq!(
            erp_fields::Error::from(e).kind(),
            erp_fields::ErrorKind::Validation
        );
    }

    /// A panicking job is an error, and its permit comes back.
    #[tokio::test]
    async fn a_panicking_job_is_an_error_and_frees_its_permit() {
        let pool = pool(1);
        let e = pool
            .run(limits::ARGON2_MEMORY_KIB_MIN, || panic!("job"))
            .await
            .unwrap_err();
        assert_eq!(e, HashPoolError::Panicked);
        pool.run(limits::ARGON2_MEMORY_KIB_MIN, || ())
            .await
            .unwrap();
    }
}
