//! The runner: claims jobs, reaps expired leases and runs what it claimed (4, D4,
//! D6; design §5.2, §5.3).
//!
//! One [`JobRunner`] serves one database of one process. Its slots are bounded three ways: the
//! process-wide `jobs.workers`, this database's `jobs.workers_per_db`, and the share of
//! `jobs.max_running_per_db` the live worker count leaves it (4).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use erp_fields::{Error, Result, Uuid};
use tokio::sync::Semaphore;

use super::backoff::Backoff;
use super::claim::{self, ClaimedJob};
use super::{JobKindDecl, JobPrincipalResolver, JobSettings};
use crate::Orm;
use crate::node::{NodeRole, NodeView};

/// Counters a runner keeps (read them with [`JobRunner::metrics`]); every value only grows.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub claimed: AtomicU64,
    pub completed: AtomicU64,
    pub retried: AtomicU64,
    pub dead: AtomicU64,
    pub reaped: AtomicU64,
    pub version_skips: AtomicU64,
    pub lease_lost: AtomicU64,
    pub cancelled: AtomicU64,
    pub panics: AtomicU64,
    pub notifies_sent: AtomicU64,
    pub claim_statements: AtomicU64,
}

/// A snapshot of a runner's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobMetrics {
    /// Jobs this runner claimed.
    pub claimed: u64,
    /// Jobs it finished with `Done`.
    pub completed: u64,
    /// Failed runs it sent back to `ready` with a backoff.
    pub retried: u64,
    /// Jobs it dead-lettered.
    pub dead: u64,
    /// Expired leases it returned (or dead-lettered) as the reaper.
    pub reaped: u64,
    /// Jobs it released because their kind version is outside what this binary accepts.
    pub version_skips: u64,
    /// Runs that stopped because the lease or the epoch moved on.
    pub lease_lost: u64,
    /// Runs that stopped because an operator cancelled the job.
    pub cancelled: u64,
    /// Handler panics it caught and treated as transient failures.
    pub panics: u64,
    /// `erp_jobs` notifications this process sent.
    pub notifies_sent: u64,
    /// Claim transactions it sent (idle polls included).
    pub claim_statements: u64,
}

/// What a runner needs to know of the node registry: how many job-running processes are live,
/// whether any other node is, and whether this node may work at all. A closure returning the
/// membership's [`NodeView`] is one; tests give their own.
pub trait LiveNodes: Send + Sync + 'static {
    /// Live `worker` nodes of the database, this one included when it counts (at least 1).
    fn live_workers(&self) -> u32;
    /// Whether any other node is fresh in the registry.
    fn has_peers(&self) -> bool;
    /// Whether this node is ready to work: a node that is not claims nothing.
    fn ready(&self) -> bool;
}

impl<F> LiveNodes for F
where
    F: Fn() -> NodeView + Send + Sync + 'static,
{
    fn live_workers(&self) -> u32 {
        self().divisor(NodeRole::Worker).get()
    }

    fn has_peers(&self) -> bool {
        !self().fresh_peers().is_empty()
    }

    fn ready(&self) -> bool {
        self().readiness().is_ready()
    }
}

pub(crate) type NodeViewFn = Arc<dyn LiveNodes>;

#[derive(Default)]
enum KindSelection {
    #[default]
    All,
    Maintenance(Vec<&'static JobKindDecl>),
}
impl KindSelection {
    fn admits(&self, decl: &JobKindDecl) -> bool {
        match self {
            Self::All => true,
            Self::Maintenance(kinds) => kinds.iter().any(|k| std::ptr::eq(*k, decl)),
        }
    }
    fn validate(&self) -> Result<()> {
        let kinds = match self {
            Self::All => return Ok(()),
            Self::Maintenance(kinds) => kinds,
        };
        if kinds.is_empty() {
            return Err(Error::programming(
                "A maintenance runner requires at least one admitted kind.",
            ));
        }
        let linked = super::linked_kinds();
        for decl in kinds {
            let registered = linked.iter().any(|k| std::ptr::eq(*k, *decl));
            if !registered
                || !matches!(decl.handler, super::HandlerRef::Native(_))
                || decl.principal != super::PrincipalPolicy::System
                || decl.timeouts != super::TimeoutClass::Maintenance
                || decl.isolation != super::KindIsolation::ReadCommitted
            {
                return Err(Error::programming(format!(
                    "Maintenance runner kind '{}' must be an exact registered native System declaration with Maintenance timeouts and ReadCommitted isolation.",
                    decl.name
                )));
            }
        }
        Ok(())
    }
}

pub(crate) struct Inner {
    pub orm: Orm,
    selection: KindSelection,
    excluded: Vec<&'static str>,
    pub resolver: Arc<dyn JobPrincipalResolver>,
    pub owner: Uuid,
    pub node: Option<NodeViewFn>,
    pub db_slots: Arc<Semaphore>,
    pub process_slots: Arc<Semaphore>,
    pub running: Mutex<HashMap<&'static str, u32>>,
    pub last_reap: Mutex<Option<Instant>>,
    pub counters: Counters,
    pub wake: tokio::sync::Notify,
    /// The wake-up of the `erp_jobs` channel through the signal listener, when the ORM has one.
    pub listener_wake: Option<Arc<tokio::sync::Notify>>,
    #[cfg(feature = "test-faults")]
    pub hooks: Mutex<Option<Arc<dyn RunnerHooks>>>,
}

/// Seams for tests (feature `test-faults` only): the runner awaits them at the named points.
#[cfg(feature = "test-faults")]
pub trait RunnerHooks: Send + Sync + 'static {
    /// Between statement 1 and statement 2 of a singleton claim.
    fn between_singleton_statements(&self) -> crate::BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Whether the runner must act as if its next settling statement failed: the transition is
    /// not sent, so the lease is left to expire.
    fn fail_settle(&self) -> bool {
        false
    }
}

/// Builds a [`JobRunner`].
#[must_use]
pub struct JobRunnerBuilder {
    orm: Orm,
    resolver: Arc<dyn JobPrincipalResolver>,
    owner: Option<Uuid>,
    node: Option<NodeViewFn>,
    process_slots: Option<Arc<Semaphore>>,
    database_slots: Option<Arc<Semaphore>>,
    selection: KindSelection,
    excluded: Vec<&'static str>,
    exclude_elevated: bool,
}

/// Claims and runs the jobs of one database.
///
/// The runner refuses to exist without a [`JobPrincipalResolver`]: a job never runs under a
/// principal nobody resolved.
#[derive(Clone)]
pub struct JobRunner {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for JobRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRunner")
            .field("owner", &self.inner.owner.to_string())
            .finish_non_exhaustive()
    }
}

impl JobRunner {
    /// A runner for the database of `orm`, resolving the principals of its jobs with `resolver`.
    pub fn builder(orm: Orm, resolver: impl JobPrincipalResolver) -> JobRunnerBuilder {
        JobRunnerBuilder {
            orm,
            resolver: Arc::new(resolver),
            owner: None,
            node: None,
            process_slots: None,
            database_slots: None,
            selection: KindSelection::All,
            excluded: Vec::new(),
            exclude_elevated: false,
        }
    }

    /// The settings the runner works under.
    pub fn settings(&self) -> &JobSettings {
        self.inner.orm.job_settings()
    }

    /// The identity written to `lease_owner` by this runner's claims.
    pub fn owner(&self) -> Uuid {
        self.inner.owner
    }

    /// A snapshot of the counters.
    pub fn metrics(&self) -> JobMetrics {
        let c = &self.inner.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        JobMetrics {
            claimed: get(&c.claimed),
            completed: get(&c.completed),
            retried: get(&c.retried),
            dead: get(&c.dead),
            reaped: get(&c.reaped),
            version_skips: get(&c.version_skips),
            lease_lost: get(&c.lease_lost),
            cancelled: get(&c.cancelled),
            panics: get(&c.panics),
            notifies_sent: get(&c.notifies_sent),
            claim_statements: get(&c.claim_statements),
        }
    }

    /// Job-running processes the node registry shows live for this database, this one included
    /// (`1` without a registry view).
    pub(crate) fn live_workers(&self) -> u32 {
        match &self.inner.node {
            Some(nodes) => nodes.live_workers().max(1),
            None => 1,
        }
    }

    /// Jobs this process may run at once for this database: `jobs.workers_per_db`, and the
    /// share of `jobs.max_running_per_db` the live workers leave it.
    pub fn slot_limit(&self) -> u32 {
        let settings = self.settings();
        settings
            .workers_per_db()
            .min(settings.per_process_share(self.live_workers()))
    }

    /// The kinds this binary can run against this database: every registered kind, except step
    /// kinds whose model the registry does not have (an older or partial binary never claims a
    /// kind it cannot run).
    pub(crate) fn claimable_kinds(&self) -> Vec<&'static JobKindDecl> {
        let registry = self.inner.orm.registry();
        super::linked_kinds()
            .into_iter()
            .filter(|decl| self.admits_kind(decl))
            .filter(|decl| match decl.handler {
                super::HandlerRef::Step { model, .. } => registry.model(model).is_some(),
                super::HandlerRef::Native(_) => true,
            })
            .filter(|decl| !self.at_kind_cap(decl))
            .collect()
    }

    pub(crate) fn admits_kind(&self, decl: &JobKindDecl) -> bool {
        self.inner.selection.admits(decl) && !self.inner.excluded.contains(&decl.name)
    }

    /// Whether this process already runs its share of a kind's `max_concurrency`.
    fn at_kind_cap(&self, decl: &JobKindDecl) -> bool {
        let Some(cap) = decl.max_concurrency else {
            return false;
        };
        let share = cap.get().div_ceil(self.live_workers().max(1)).max(1);
        let running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(decl.name)
            .copied()
            .unwrap_or(0);
        running >= share
    }

    /// The lease each kind of `kinds` is granted, in seconds.
    fn leases(&self, kinds: &[&'static JobKindDecl]) -> Vec<f64> {
        kinds
            .iter()
            .map(|k| self.settings().lease_for(k.timeouts).as_secs_f64())
            .collect()
    }

    /// Claim up to `limit` due jobs of the kinds this runner can run: jobs without a singleton
    /// key in one statement, then, when the batch is not full, jobs with one in a transaction of
    /// two statements (1). `limit` is bounded by `jobs.claim_batch`.
    ///
    /// # Errors
    ///
    /// The database's errors; a claim that fails never leaves a job half claimed (it is one
    /// short transaction).
    #[doc(hidden)]
    pub async fn claim(&self, limit: u32) -> Result<Vec<ClaimedJob>> {
        let limit = limit.min(self.settings().claim_batch());
        if limit == 0 {
            return Ok(Vec::new());
        }
        let kinds = self.claimable_kinds();
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let leases = self.leases(&kinds);
        self.inner
            .counters
            .claim_statements
            .fetch_add(1, Ordering::Relaxed);
        let mut claimed = claim::claim_plain(
            &self.inner.orm,
            &kinds,
            &leases,
            i64::from(limit),
            self.inner.owner,
        )
        .await?;
        let wanted = usize::try_from(limit).unwrap_or(usize::MAX);
        if claimed.len() < wanted {
            let rest = i64::try_from(wanted - claimed.len()).unwrap_or(i64::MAX);
            claimed.extend(self.claim_singletons(&kinds, &leases, rest).await?);
        }
        self.inner
            .counters
            .claimed
            .fetch_add(claimed.len() as u64, Ordering::Relaxed);
        Ok(claimed)
    }

    /// The singleton path: candidates with their key lock, then the claim in a fresh snapshot.
    async fn claim_singletons(
        &self,
        kinds: &[&'static JobKindDecl],
        leases: &[f64],
        limit: i64,
    ) -> Result<Vec<ClaimedJob>> {
        let names: Vec<&str> = kinds.iter().map(|k| k.name).collect();
        let mut tx = self.inner.orm.begin_opts(claim::claim_options()?).await?;
        let outcome = async {
            let rows = tx
                .query(claim::SINGLETON_CANDIDATES_SQL, &[&names, &limit])
                .await?;
            let mut candidates = Vec::with_capacity(rows.len());
            for row in &rows {
                let decode = |e: tokio_postgres::Error| {
                    Error::internal("an erp_job candidate did not decode").with_source(e)
                };
                candidates.push((
                    row.try_get::<_, i64>(0).map_err(decode)?,
                    row.try_get::<_, String>(1).map_err(decode)?,
                    row.try_get::<_, bool>(2).map_err(decode)?,
                ));
            }
            #[cfg(feature = "test-faults")]
            {
                let hooks = self
                    .inner
                    .hooks
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                if let Some(hooks) = hooks {
                    hooks.between_singleton_statements().await;
                }
            }
            let ids = claim::pick_per_key(&candidates);
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let rows = tx
                .query(
                    claim::SINGLETON_CLAIM_SQL,
                    &[&ids, &self.inner.owner, &names, &leases.to_vec()],
                )
                .await?;
            rows.iter()
                .map(|row| {
                    let kind: String = row.try_get(2).map_err(|e| {
                        Error::internal("erp_job kind did not decode").with_source(e)
                    })?;
                    let lease = kinds
                        .iter()
                        .zip(leases)
                        .find(|(k, _)| k.name == kind)
                        .map(|(_, secs)| Duration::from_secs_f64(*secs))
                        .ok_or_else(|| {
                            Error::internal("a claimed kind is not in the claim list")
                        })?;
                    claim::decode(row, lease)
                })
                .collect::<Result<Vec<_>>>()
        }
        .await;
        match outcome {
            Ok(claimed) => {
                tx.commit().await?;
                Ok(claimed)
            }
            Err(error) => Err(tx.discard_after(error).await),
        }
    }

    /// Milliseconds until the earliest ready job of the kinds this runner can run is due
    /// (`0` when one is due now, `None` when nothing is queued).
    ///
    /// # Errors
    ///
    /// The database's errors.
    #[doc(hidden)]
    pub async fn next_due(&self) -> Result<Option<Duration>> {
        let kinds = self.claimable_kinds();
        let names: Vec<&str> = kinds.iter().map(|k| k.name).collect();
        let mut tx = self
            .inner
            .orm
            .begin_opts(claim::claim_options()?.read_only("jobs.next_due"))
            .await?;
        let rows = match tx.query(claim::NEXT_DUE_SQL, &[&names]).await {
            Ok(rows) => rows,
            Err(error) => return Err(tx.discard_after(error).await),
        };
        tx.rollback().await?;
        let millis: Option<f64> = match rows.first() {
            Some(row) => row
                .try_get(0)
                .map_err(|e| Error::internal("next due did not decode").with_source(e))?,
            None => None,
        };
        Ok(millis.map(|ms| Duration::from_secs_f64(ms.max(0.0) / 1000.0)))
    }

    /// Return the jobs whose lease ran out: `ready` with the kind's backoff, or `dead` when no
    /// attempt is left. A job whose chunk still holds its row is skipped (`SKIP LOCKED`), so a
    /// run in progress is never taken away. At most `batch` jobs per call.
    ///
    /// # Errors
    ///
    /// The database's errors.
    #[doc(hidden)]
    pub async fn reap(&self, batch: u32) -> Result<ReapReport> {
        let settings = self.settings();
        let (only, excluded): (Option<Vec<&str>>, Vec<&str>) = match &self.inner.selection {
            KindSelection::All => (None, self.inner.excluded.clone()),
            KindSelection::Maintenance(kinds) => (
                Some(kinds.iter().map(|k| k.name).collect()),
                self.inner.excluded.clone(),
            ),
        };
        let limit = i64::from(batch.clamp(1, 1_000));
        let mut tx = self.inner.orm.begin_opts(claim::claim_options()?).await?;
        let outcome = async {
            let expired = tx.query(REAP_SELECT_SQL, &[&limit, &only, &excluded]).await?;
            if expired.is_empty() {
                return Ok(ReapReport::default());
            }
            let mut ids = Vec::with_capacity(expired.len());
            let mut dead = Vec::with_capacity(expired.len());
            let mut delays = Vec::with_capacity(expired.len());
            let mut messages = Vec::with_capacity(expired.len());
            for row in &expired {
                let decode = |e: tokio_postgres::Error| {
                    Error::internal("an expired erp_job row did not decode").with_source(e)
                };
                let id: i64 = row.try_get(0).map_err(decode)?;
                let kind: String = row.try_get(1).map_err(decode)?;
                let attempts: i32 = row.try_get(2).map_err(decode)?;
                let max_attempts: i32 = row.try_get(3).map_err(decode)?;
                let exhausted = attempts >= max_attempts;
                let backoff = match super::enqueue::kind_map().get(kind.as_str()) {
                    Some(decl) => decl.backoff.unwrap_or_else(|| default_backoff(settings)),
                    None => default_backoff(settings),
                };
                let delay = backoff.delay_random(u32::try_from(attempts).unwrap_or(u32::MAX))?;
                ids.push(id);
                dead.push(exhausted);
                delays.push(delay.as_secs_f64());
                messages.push(
                    super::error::bounded(if exhausted {
                        format!(
                            "the lease expired during attempt {attempts} of {max_attempts}; no attempt is left"
                        )
                    } else {
                        format!("the lease expired during attempt {attempts} of {max_attempts}")
                    })
                    .into_string(),
                );
            }
            let rows = tx
                .query(REAP_UPDATE_SQL, &[&ids, &dead, &delays, &messages])
                .await?;
            let mut report = ReapReport::default();
            for row in &rows {
                let now_dead: bool = row
                    .try_get(0)
                    .map_err(|e| Error::internal("reaped state did not decode").with_source(e))?;
                if now_dead {
                    report.dead += 1;
                } else {
                    report.requeued += 1;
                }
            }
            Ok(report)
        }
        .await;
        match outcome {
            Ok(report) => {
                tx.commit().await?;
                self.inner.counters.reaped.fetch_add(
                    u64::from(report.requeued) + u64::from(report.dead),
                    Ordering::Relaxed,
                );
                self.inner
                    .counters
                    .dead
                    .fetch_add(u64::from(report.dead), Ordering::Relaxed);
                Ok(report)
            }
            Err(error) => Err(tx.discard_after(error).await),
        }
    }

    /// Set how many jobs of `kind` this runner counts as running (feature `test-faults`).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn pretend_running_for_tests(&self, kind: &'static str, count: u32) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(kind, count);
    }

    /// Install test hooks (feature `test-faults`).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn set_hooks(&self, hooks: Arc<dyn RunnerHooks>) {
        *self
            .inner
            .hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hooks);
    }
}

/// What one call of the reaper did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReapReport {
    /// Jobs sent back to `ready` with a backoff.
    pub requeued: u32,
    /// Jobs that had no attempt left and became dead letters.
    pub dead: u32,
}

/// The backoff of a kind that declares none: `jobs.backoff_base_secs` and `jobs.backoff_cap_secs`.
pub(crate) fn default_backoff(settings: &JobSettings) -> Backoff {
    Backoff::Exponential {
        base: settings.backoff_base(),
        cap: settings.backoff_cap(),
    }
}

const REAP_SELECT_SQL: &str = "\
SELECT id, kind, attempts, max_attempts FROM erp_job
 WHERE state = 'running' AND lease_until < clock_timestamp()
   AND ($2::text[] IS NULL OR kind=ANY($2)) AND NOT(kind=ANY($3::text[]))
 ORDER BY lease_until
 FOR UPDATE SKIP LOCKED
 LIMIT $1";

const REAP_UPDATE_SQL: &str = "\
UPDATE erp_job j
   SET state = CASE WHEN x.dead THEN 'dead' ELSE 'ready' END,
       lease_owner = NULL, lease_until = NULL,
       run_after = CASE WHEN x.dead THEN j.run_after
                        ELSE clock_timestamp() + make_interval(secs => x.delay) END,
       failure_rank = CASE WHEN x.dead THEN j.failure_rank ELSE LEAST(j.attempts, 32767)::int2 END,
       last_error_code = 'lease_expired', last_error = x.message, last_error_at = clock_timestamp(),
       dead_at = CASE WHEN x.dead THEN clock_timestamp() END
  FROM unnest($1::int8[], $2::bool[], $3::float8[], $4::text[]) AS x(id, dead, delay, message)
 WHERE j.id = x.id AND j.state = 'running' AND j.lease_until < clock_timestamp()
RETURNING (j.state = 'dead')";

impl JobRunnerBuilder {
    /// The identity this runner writes to `lease_owner`: the node id of its membership. Without
    /// it the runner draws a random one.
    pub fn owner(mut self, owner: Uuid) -> Self {
        self.owner = Some(owner);
        self
    }

    /// The node registry's view: live worker counts divide the per-database cap, and a node
    /// that is not ready claims nothing.
    pub fn node_view(mut self, view: impl Fn() -> NodeView + Send + Sync + 'static) -> Self {
        self.node = Some(Arc::new(view));
        self
    }

    /// As [`node_view`](Self::node_view), from any source of the registry's counts.
    pub fn live_nodes(mut self, nodes: impl LiveNodes) -> Self {
        self.node = Some(Arc::new(nodes));
        self
    }

    /// Share one process-wide slot pool (`jobs.workers`) between the runners of several
    /// databases.
    pub fn process_slots(mut self, slots: Arc<Semaphore>) -> Self {
        self.process_slots = Some(slots);
        self
    }

    /// Share the same database slot pool between ordinary and maintenance runners of one
    /// database. The composition root supplies one pool sized to `jobs.workers_per_db` and
    /// also shares `process_slots`; otherwise each runner has its own database pool.
    pub fn database_slots(mut self, slots: Arc<Semaphore>) -> Self {
        self.database_slots = Some(slots);
        self
    }

    /// Admit only these exact registered native System maintenance declarations. Validation
    /// happens at build; an empty list, unregistered copy, Step, caller-principal, default
    /// timeout or non-ReadCommitted declaration is refused. Selection applies to claiming,
    /// next-due, reaping and execution. The trusted composition root must supply only its fixed
    /// built-in set; payloads and addon data must never choose this set.
    pub fn maintenance_kinds(mut self, kinds: &[&'static JobKindDecl]) -> Self {
        self.selection = KindSelection::Maintenance(kinds.to_vec());
        self
    }

    /// Exclude registered names from claiming, execution and reaping. Calls are additive.
    /// Unregistered names are refused at build, preserving the existing public contract.
    pub fn exclude_kinds(mut self, kinds: impl IntoIterator<Item = &'static str>) -> Self {
        self.excluded.extend(kinds);
        self
    }

    /// Exclude every registered elevated kind from this ordinary runner.
    pub fn exclude_elevated(mut self) -> Self {
        self.exclude_elevated = true;
        self
    }

    /// Check the settings against the registered kinds and build the runner.
    ///
    /// # Errors
    ///
    /// `Internal` when no entropy is available for the owner identity; `Programming` when
    /// selected kind declarations do not satisfy the execution boundary.
    pub fn build(mut self) -> Result<JobRunner> {
        self.selection.validate()?;
        let registered = super::enqueue::kind_map();
        if let Some(unknown) = self
            .excluded
            .iter()
            .find(|kind| !registered.contains_key(**kind))
        {
            return Err(Error::programming(format!(
                "JobRunner::exclude_kinds names the job kind '{unknown}', which is not registered in this binary"
            )));
        }
        if self.exclude_elevated {
            for decl in super::JOB_KINDS.iter().filter(|decl| decl.elevated) {
                if !self.excluded.contains(&decl.name) {
                    self.excluded.push(decl.name);
                }
            }
        }
        if let KindSelection::Maintenance(kinds) = &self.selection
            && kinds.iter().any(|decl| self.excluded.contains(&decl.name))
        {
            return Err(Error::programming(
                "Maintenance runner inclusion conflicts with excluded kinds.",
            ));
        }
        let owner = match self.owner {
            Some(owner) => owner,
            None => {
                let id = crate::CorrelationId::generate()?;
                Uuid::parse(id.as_str())?
            }
        };
        let settings = self.orm.job_settings();
        let process_slots = self
            .process_slots
            .unwrap_or_else(|| Arc::new(Semaphore::new(settings.workers() as usize)));
        let db_slots = self
            .database_slots
            .unwrap_or_else(|| Arc::new(Semaphore::new(settings.workers_per_db() as usize)));
        let listener_wake = match self.orm.signal() {
            Some(signal) => Some(signal.subscribe(&crate::signal::NotifyChannel::new(
                super::wake::JOBS_CHANNEL,
            )?)?),
            None => None,
        };
        let orm_id = self.orm.id();
        let inner = Arc::new(Inner {
            orm: self.orm,
            selection: self.selection,
            excluded: self.excluded,
            resolver: self.resolver,
            owner,
            node: self.node,
            db_slots,
            process_slots,
            running: Mutex::new(HashMap::new()),
            last_reap: Mutex::new(None),
            counters: Counters::default(),
            wake: tokio::sync::Notify::new(),
            listener_wake,
            #[cfg(feature = "test-faults")]
            hooks: Mutex::new(None),
        });
        super::wake::register(orm_id, &inner);
        Ok(JobRunner { inner })
    }
}
