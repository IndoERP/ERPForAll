//! The ORM runtime ([`Orm`]) and transactions ([`Transaction`],–5).
//!
//! One task owns a `Transaction` and passes it as `&mut` to every data operation: no locks, `Send`
//! futures. The transaction owns its pooled connection, the record cache and the statement counter.
//!
//! State machine: `Idle` → (`BEGIN ISOLATION LEVEL REPEATABLE READ`, sent lazily) → `Open`. Any
//! server error while open makes it `Aborted` (PostgreSQL ignores further commands until the end of
//! the transaction); `commit()` then rolls back and fails. When the connection's state is unknown —
//! a future dropped during `BEGIN`, a failed `COMMIT` or `ROLLBACK`, a closed connection — the
//! connection is detached from the pool (closing it rolls back on the server) and the transaction
//! becomes `Released`: unusable. A connection inside a transaction never returns to the pool.
//!
//! [`Orm::run`] is the unit-of-work entry point: it retries serialization failures and deadlocks.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use deadpool_postgres::{Object, Pool};
use erp_fields::{Cell, DateTime, Error, ErrorKind, RecId, Result};
use erp_registry::{FieldId, ModelId, ModelSpec, Registry};
use rustc_hash::FxHashMap;
use tokio_postgres::Row;
use tokio_postgres::types::ToSql;

pub(crate) mod batches;
pub(crate) mod cursors;
mod lifecycle_open;
mod pipelined;
pub(crate) mod raw_row;
mod run_options;
pub use run_options::{RetryPolicy, RunFailure, RunOptions, require_top_level_unit};
pub(crate) mod scoped_ids;
mod sql_words;
mod start;
mod stream;
pub(crate) mod write_gens;

pub(crate) use batches::ResponseBatches;
pub(crate) use pipelined::{MAX_FLIGHT_STATEMENTS, PipelinedQuery};
pub(crate) use scoped_ids::ScopedIds;
pub(crate) use stream::{Args, write_fetch};

use crate::access::{
    AccessPolicy, IntoPolicyProvider, POLICY_SOURCE_WRITES, PolicyProvider, PolicySourceWrite,
    PrincipalResolver, StaticPolicy,
};
use crate::cache::Cache;
use crate::compute::{COMPUTE_IMPLS, CONSTRAINT_IMPLS, ComputeImpl, ConstraintImpl};
use crate::context::Context;
use crate::dberror::map_pg_error;
use crate::dispatch::{Dispatch, MethodImpl};
use crate::engine::EngineState;
use crate::env::Env;
use crate::graph::ComputeGraph;
use crate::notice::{MAX_NOTICES_PER_TRANSACTION, Notice};
use crate::ormcache::SharedOrmCache;
use crate::principal::{Principal, PrincipalHint};
use crate::records::Records;

/// Tunables.
#[derive(Debug, Clone)]
pub struct OrmConfig {
    /// Cumulative native selection metadata admission caps.
    pub selection_limits: crate::SelectionLimits,
    /// Most ids per prefetch query (one `int8[]` parameter per chunk).
    pub prefetch_chunk: usize,
    /// Most rows per `INSERT` statement (bounds memory; one array parameter per column).
    pub insert_batch: usize,
    /// Attempts of a unit of work in [`Orm::run`] (5).
    pub max_attempts: u32,
    /// Base of the full-jitter exponential backoff between attempts (50 ms).
    pub retry_base: Duration,
    /// Cap of the backoff (2 s).
    pub retry_cap: Duration,
    /// Most records per compute call (bounded batches, /).
    pub recompute_batch: usize,
    /// Rounds of recompute + write a flush may take before failing (COMP §7 D5).
    pub max_flush_rounds: u32,
    pub unaccent: bool,
    /// Most predicate texts and orders kept per ORM by the search memo, with
    /// two-generation eviction; 0 disables the memo.
    pub search_memo_size: usize,
    /// Nonstored field search domain resource limits.
    pub search_limits: crate::search_method::SearchLimits,
    /// Most prepared statements kept per pooled connection; beyond it the connection's statement
    /// cache is emptied (externally supplied orders and domain shapes must not grow it without
    /// bound).
    pub statement_cache_size: usize,
    /// `lock_timeout` of the partition upkeep calls ([`Orm::ensure_partitions`] and the others,
    /// ): bounds every lock wait of the upkeep, including a concurrent detach waiting
    /// for older transactions (it then gives up and leaves the partition pending). From 100 ms
    /// to 1 h ([`crate::PARTITION_LOCK_TIMEOUT_BOUNDS`]); default 5 s.
    pub partition_lock_timeout: Duration,
    /// Limits every transaction sets with `BEGIN` (`lock_timeout`, `statement_timeout` and the
    /// `transaction_timeout` backstop, "Timeouts"); [`BeginOptions::timeouts`] changes
    /// them for one transaction.
    pub timeouts: crate::timeouts::Timeouts,
    /// Give every transaction opened without an [`OperationIdentity`](crate::OperationIdentity)
    /// a freshly generated one (per attempt in [`Orm::run`]). Meant for test harnesses whose
    /// fixtures write audited lifecycle events outside the RPC layer; servers leave
    /// it off and attach an identity at each entry point, so an untagged audited write still
    /// fails loudly. Default off. Only exists with the test-only cargo feature `test-identities`
    /// (erp-testkit enables it; no shipped build may).
    #[cfg(feature = "test-identities")]
    pub identify_untagged: bool,
}

impl Default for OrmConfig {
    fn default() -> Self {
        Self {
            selection_limits: crate::SelectionLimits::default(),
            prefetch_chunk: 10_000,
            insert_batch: 1_000,
            max_attempts: 5,
            retry_base: Duration::from_millis(50),
            retry_cap: Duration::from_secs(2),
            recompute_batch: 10_000,
            max_flush_rounds: 16,
            unaccent: false,
            search_memo_size: 4_096,
            search_limits: crate::search_method::SearchLimits::default(),
            statement_cache_size: 1_024,
            partition_lock_timeout: Duration::from_secs(5),
            timeouts: crate::timeouts::Timeouts::default(),
            #[cfg(feature = "test-identities")]
            identify_untagged: false,
        }
    }
}

pub(crate) struct Shared {
    pub(crate) generation_attachments: Arc<crate::generation::GenerationAttachments>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) dispatch: Dispatch,
    pub(crate) graph: ComputeGraph,
    /// Relational facts of the registry (inverses, field domains, FK metadata).
    pub(crate) relations: crate::relational::Relations,
    /// The models a write to each table may change (the write ticks of streams).
    pub(crate) tables: write_gens::TableModels,
    /// `#[onchange]` methods per (model, field).
    pub(crate) onchange: crate::onchange::OnchangeMethods,
    /// Compiled exclusion constraints and `WITHOUT OVERLAPS` keys.
    pub(crate) exclusions: crate::exclusion::Exclusions,
    /// Nonstored field search methods.
    pub(crate) search_methods: crate::search_method::FieldSearchChains,
    pub(crate) ormcache: SharedOrmCache,
    /// Resolved guard plans per policy snapshot; shared by every [`Orm::rebuild`]
    /// of a lineage.
    pub(crate) resolved_guards: Arc<crate::access::ResolvedGuardCache>,
    /// Where transactions pin their policy snapshot.
    pub(crate) policy: Arc<dyn PolicyProvider>,
    /// Resolves principals against the database; `None` keeps unresolved
    /// principals (static policies).
    pub(crate) resolver: Option<Arc<dyn PrincipalResolver>>,
    /// The policy-source and membership models of this registry (design note §3.4).
    pub(crate) policy_sources: FxHashMap<ModelId, Vec<&'static PolicySourceWrite>>,
    /// RPC exposure of every (model, method).
    pub(crate) exposure: crate::rpc::ExposureTable,
    pub(crate) config: OrmConfig,
    /// Compiled searches of pure domains, shared by every transaction.
    pub(crate) search_memo: crate::search::SearchMemo,
    /// Identity of the database: created by [`Orm::builder`], shared by every [`Orm::rebuild`]
    /// of it. [`Transaction::switch_orm`] only moves between ORMs of one identity.
    /// The schema epoch lives here, not per `Shared`: DDL run through one ORM of the database
    /// (a stage ORM of a module operation) must invalidate the prepared statements every other ORM
    /// of the database cached on the shared pool.
    database: Arc<DatabaseIdentity>,
    /// Cross-process change signals; `None` keeps the single-process behaviour. One
    /// per database: carried over by [`Orm::rebuild`].
    pub(crate) signal: Option<Arc<crate::signal::Signal>>,
    /// The read pool, when one is configured; `None`: every transaction uses the
    /// primary.
    pub(crate) read: Option<crate::route::ReadRoute>,
    /// Routing decisions since startup.
    pub(crate) routing: crate::route::RoutingCounters,
    /// Write hooks by model (design note §4.4).
    pub(crate) write_hooks: crate::version::WriteHooks,
    /// Unlink hooks, called after every `unlink`.
    pub(crate) unlink_hooks: Vec<crate::version::UnlinkHook>,
    pub(crate) precommit_hooks: Vec<crate::tracking::PrecommitHook>,
    /// Selection ondelete callbacks.
    pub(crate) selection_callbacks: Vec<crate::version::SelectionCallback>,
    /// The job queue's settings.
    pub(crate) job_settings: Arc<crate::jobs::JobSettings>,
    pub(crate) job_settings_source: Option<Arc<dyn crate::jobs::JobSettingsSource>>,
    job_settings_compatibility: Option<crate::jobs::JobSettingsCompatibility>,
    /// The capability that authorizes job administration (`jobs.admin`).
    pub(crate) job_admin: Option<&'static crate::Capability>,
    /// The capability that lets a principal that is not `System` act as another user
    /// (`principal.impersonate`, A4).
    pub(crate) impersonate: Option<&'static crate::Capability>,
    /// Implementations given to the builder at runtime, carried over by [`Orm::rebuild`].
    extras: Extras,
}

/// Source of [`DatabaseIdentity::orm_id`].
static NEXT_ORM_ID: AtomicU64 = AtomicU64::new(1);

/// One exists per
/// [`Orm`] lineage, minted only by [`OrmBuilder::build_with_base_install_grant`] and returned to
/// the build site, which hands it to the module-install executor (the data loader). It is not
/// `Clone`, `Copy` or `Default`, and nothing reachable from a [`Transaction`] or an `Env` returns
/// it, so cron jobs, addon hooks and RPC handlers cannot obtain it. It stays valid on the ORMs
/// [`Orm::rebuild`] makes from its ORM (the stage registries of one module operation),
/// which share the id it is checked against ([`Orm::id`]).
pub struct BaseInstallGrant {
    orm_id: u64,
}

impl BaseInstallGrant {
    /// The id of the [`Orm`] this grant belongs to (see [`Orm::id`]). Exists for the dev-only
    /// testkit's bookkeeping; knowing an id grants nothing.
    #[doc(hidden)]
    pub fn orm_id(&self) -> u64 {
        self.orm_id
    }
}

impl std::fmt::Debug for BaseInstallGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BaseInstallGrant { .. }")
    }
}

/// Runtime implementations of an ORM (beyond the linked ones); each registry keeps those of its
/// addons.
struct Extras {
    impls: Vec<&'static MethodImpl>,
    wasm_methods: Vec<crate::dispatch::WasmMethodImpl>,
    computes: Vec<&'static ComputeImpl>,
    constraints: Vec<&'static ConstraintImpl>,
    search_methods: Vec<&'static crate::search_method::FieldSearchImpl>,
    write_hooks: Vec<crate::version::WriteHook>,
    unlink_hooks: Vec<crate::version::UnlinkHook>,
    precommit_hooks: Vec<crate::tracking::PrecommitHook>,
    selection_callbacks: Vec<crate::version::SelectionCallback>,
    policy_sources: Vec<&'static PolicySourceWrite>,
    rpc_impls: Vec<&'static crate::rpc::RpcImpl>,
    /// The builder's `trust_caller_resolutions` (`trusted-test-principals` builds only).
    #[cfg(feature = "trusted-test-principals")]
    trusted: bool,
    /// The core tables this ORM requires ([`OrmBuilder::require_core_tables`]).
    required_core_tables: Vec<&'static crate::CoreTableRequirement>,
    /// The job kinds this ORM requires ([`OrmBuilder::require_job_kinds`]).
    required_job_kinds: Vec<&'static crate::jobs::JobKindRequirement>,
}

/// Append the items not already present (the same `'static` item given twice is one item).
fn extend_unique<T>(list: &mut Vec<&'static T>, items: impl IntoIterator<Item = &'static T>) {
    for item in items {
        if !list.iter().any(|known| std::ptr::eq(*known, item)) {
            list.push(item);
        }
    }
}

impl Shared {
    /// The cached audit store state of this database's lineage.
    pub(crate) fn audit_store(&self) -> &crate::audit::StoreCell {
        &self.database.audit_store
    }

    /// Clear `conn`'s statement cache when DDL ran since it last checked.
    fn refresh_statements(&self, conn: &Object) {
        self.database.refresh_statements(conn);
    }

    fn schema_changed(&self, conn: &Object) {
        self.database.schema_changed(conn);
    }

    /// Every pooled connection of the database drops its prepared statements before its next use
    /// (the schema changed in another process).
    pub(crate) fn bump_schema_epoch(&self) {
        self.database.bump_schema_epoch();
    }
}

impl Shared {
    pub(crate) async fn job_snapshot(&self) -> Result<Arc<crate::jobs::JobSettingsSnapshot>> {
        match &self.job_settings_source {
            Some(source) => tokio::time::timeout(self.config.timeouts.transaction(), source.read())
                .await.map_err(|_| Error::new(erp_fields::ErrorKind::Database,
                    "Job policy read exceeded the node transaction timeout; new admission stopped."))??
                .on_node(&self.job_settings),
            None => Ok(crate::jobs::JobSettingsSnapshot::static_policy((*self.job_settings).clone())),
        }
    }
}

/// The lineage an [`Env`] carries: the identity of the database of the ORM that made it.
pub(crate) type Lineage = Arc<LineageToken>;

/// What an [`Env`] carries to say which ORM lineage made it: a token without state (so the
/// environment, usable as a hash key, has no interior mutability), compared by address.
#[derive(Debug, Default)]
pub(crate) struct LineageToken;

/// Identity of a database (compared by address) and its schema epoch, shared by every ORM built
/// with [`Orm::rebuild`] from one [`Orm::builder`].
#[derive(Debug)]
pub(crate) struct DatabaseIdentity {
    /// The token the environments of this lineage carry ([`Env::same_lineage`]).
    lineage: Lineage,
    /// Process-unique id of the lineage ([`Orm::id`]): a [`BaseInstallGrant`] names it, so the
    /// grant of an ORM is valid on every [`Orm::rebuild`] of it (lead decision, merge train
    /// 2026-10-18: the stage registries of one module operation are one install).
    orm_id: u64,
    /// Bumped by every DDL batch through any ORM of the database, and again when the batch's
    /// transaction commits: connections that saw an older epoch drop their prepared statements
    /// before use (their plans may describe old table shapes). The second bump covers a
    /// connection that refreshed between the DDL and the commit and prepared statements against
    /// the shape it could still see then.
    schema_epoch: AtomicU64,
    /// Epoch last seen per pooled connection, keyed by its statement cache's address (stable for
    /// the connection's life). The entry keeps a [`std::sync::Weak`] to the cache: once the
    /// connection is gone (closed, dropped by the pool) the entry is dead, is never read as the
    /// state of a new connection that reuses the address, and is removed by the next prune.
    seen_epochs: Mutex<FxHashMap<usize, SeenEpoch>>,
    /// Whether the audit table can take rows: a rebuildable cache, shared by every
    /// ORM of the lineage.
    audit_store: crate::audit::StoreCell,
}

/// The epoch one connection last saw, and whether that connection is still alive.
#[derive(Debug)]
struct SeenEpoch {
    cache: std::sync::Weak<deadpool_postgres::StatementCache>,
    epoch: u64,
}

/// Entries of `seen_epochs` above which a lookup first removes those of closed connections (a
/// pool holds a few dozen connections; the bound only keeps the map from growing without limit
/// when connections come and go).
const SEEN_EPOCHS_PRUNE_AT: usize = 256;

impl DatabaseIdentity {
    /// A new lineage, with a new [`DatabaseIdentity::orm_id`].
    fn new() -> Self {
        Self {
            lineage: Lineage::default(),
            orm_id: NEXT_ORM_ID.fetch_add(1, Ordering::Relaxed),
            schema_epoch: AtomicU64::new(0),
            seen_epochs: Mutex::new(FxHashMap::default()),
            audit_store: crate::audit::StoreCell::new(),
        }
    }

    fn conn_key(conn: &Object) -> usize {
        Arc::as_ptr(&conn.statement_cache) as usize
    }

    fn refresh_statements(&self, conn: &Object) {
        let epoch = self.schema_epoch.load(Ordering::Acquire);
        // A poisoned lock only means another thread panicked while holding it; the map holds
        // plain integers that are always consistent, so keep using it.
        let mut seen = self
            .seen_epochs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Self::prune(&mut seen);
        let key = Self::conn_key(conn);
        // An entry whose connection is gone belongs to an earlier connection at the same address:
        // this one has not been seen.
        let last = seen
            .get(&key)
            .filter(|e| e.cache.strong_count() > 0)
            .map_or(u64::MAX, |e| e.epoch);
        if last != epoch {
            conn.statement_cache.clear();
            Self::record(&mut seen, conn, epoch);
        }
    }

    /// Remove the entries of connections that no longer exist, once the map is large.
    fn prune(seen: &mut FxHashMap<usize, SeenEpoch>) {
        if seen.len() >= SEEN_EPOCHS_PRUNE_AT {
            seen.retain(|_, e| e.cache.strong_count() > 0);
        }
    }

    fn record(seen: &mut FxHashMap<usize, SeenEpoch>, conn: &Object, epoch: u64) {
        seen.insert(
            Self::conn_key(conn),
            SeenEpoch {
                cache: Arc::downgrade(&conn.statement_cache),
                epoch,
            },
        );
    }

    /// Make every pooled connection drop its prepared statements before its next use (the
    /// schema changed in another process).
    pub(crate) fn bump_schema_epoch(&self) {
        self.schema_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn schema_changed(&self, conn: &Object) {
        // Wrapping is harmless: only equality with the last seen value matters.
        let epoch = self
            .schema_epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        // See `refresh_statements` for the poisoned lock.
        let mut seen = self
            .seen_epochs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Self::prune(&mut seen);
        Self::record(&mut seen, conn, epoch);
    }
}

/// The ORM for one database: registry, dispatch tables, access policy, connection pool.
#[derive(Clone)]
pub struct Orm {
    pub(crate) shared: Arc<Shared>,
    pub(crate) pool: Pool,
}

/// Builds an [`Orm`].
pub struct OrmBuilder {
    generation_attachments: crate::generation::GenerationAttachments,
    registry: Arc<Registry>,
    pool: Pool,
    policy: Option<Result<Arc<dyn PolicyProvider>>>,
    resolver: Option<Arc<dyn PrincipalResolver>>,
    #[cfg(feature = "trusted-test-principals")]
    trust_caller_resolutions: bool,
    policy_sources: Vec<&'static PolicySourceWrite>,
    impls: Vec<&'static MethodImpl>,
    wasm_methods: Vec<crate::dispatch::WasmMethodImpl>,
    rpc_impls: Vec<&'static crate::rpc::RpcImpl>,
    computes: Vec<&'static ComputeImpl>,
    constraints: Vec<&'static ConstraintImpl>,
    search_methods: Vec<&'static crate::search_method::FieldSearchImpl>,
    config: OrmConfig,
    database: Arc<DatabaseIdentity>,
    resolved_guards: Option<Arc<crate::access::ResolvedGuardCache>>,
    signal: Option<Arc<crate::signal::Signal>>,
    write_hooks: Vec<crate::version::WriteHook>,
    unlink_hooks: Vec<crate::version::UnlinkHook>,
    precommit_hooks: Vec<crate::tracking::PrecommitHook>,
    selection_callbacks: Vec<crate::version::SelectionCallback>,
    required_core_tables: Vec<&'static crate::CoreTableRequirement>,
    required_job_kinds: Vec<&'static crate::jobs::JobKindRequirement>,
    job_settings: Option<Arc<crate::jobs::JobSettings>>,
    job_settings_source: Option<Arc<dyn crate::jobs::JobSettingsSource>>,
    job_admin: Option<&'static crate::Capability>,
    impersonate: Option<&'static crate::Capability>,
    read_pool: Option<Pool>,
    read_pool_failure: Option<crate::route::ReadPoolFailure>,
    read_pool_max_lag: Option<Duration>,
    /// Made by [`Orm::rebuild`]: the access policy is the carried one and cannot be replaced.
    rebuilt: bool,
    /// The first override a rebuilt builder refused (resolver, trust flag, policy sources, write
    /// hooks, signal); returned by [`OrmBuilder::build`] (S1: a rebuilt ORM enforces the access
    /// configuration of the ORM it came from).
    refused: Option<Error>,
}

impl OrmBuilder {
    /// Attach a mandatory immutable composition product to this exact registry.
    ///
    /// Wrong registry identities and duplicate product types make `build` fail. Rebuilt
    /// builders start empty; callers must validate and attach the target generation's product.
    pub fn generation_attachment<T: std::any::Any + Send + Sync>(
        mut self,
        attachment: crate::GenerationAttachment<T>,
    ) -> Self {
        if let Err(error) = self
            .generation_attachments
            .insert(&self.registry, attachment)
        {
            self.refused.get_or_insert(error);
        }
        self
    }

    /// Compose a required service from owned target-generation inputs and final exposure.
    ///
    /// Factories run before any ORM escapes. Wrong registry identity, duplicate output type
    /// or composition error aborts construction; rebuilds inherit neither factory nor value.
    pub fn generation_attachment_with<
        T: std::any::Any + Send + Sync,
        I: std::any::Any + Send + Sync,
    >(
        mut self,
        factory: crate::GenerationAttachmentFactory<T, I>,
    ) -> Self {
        if let Err(error) = self.generation_attachments.factory(&self.registry, factory) {
            self.refused.get_or_insert(error);
        }
        self
    }

    /// Record that `call` was used on a builder made by [`Orm::rebuild`]: those carry the access
    /// policy, resolver, trust flag, policy sources, write hooks and signal of their ORM, fixed.
    /// Only the first refusal is kept; [`OrmBuilder::build`] returns it.
    fn refuse_override(&mut self, call: &str) {
        if self.rebuilt && self.refused.is_none() {
            self.refused = Some(Error::programming(format!(
                "{call} on a builder made by Orm::rebuild: a rebuilt ORM keeps the access \
                 configuration (policy, resolver, policy sources, write hooks, signal) of the \
                 ORM it came from; build a new ORM with Orm::builder to use another"
            )));
        }
    }

    /// Route `read`-effect transactions to `pool`, a pool of connections to a replica or standby
    /// (or to a read-only role of the primary), when the unit of work has not begun a write
    /// ([`crate::unit_of_work`]). Explicit: without a
    /// read pool every transaction uses the primary ([`Orm::routing_stats`] reports which).
    /// Needs [`OrmBuilder::signal`]: the routed transaction reads the signal generations from
    /// the replica and runs on the primary when the replica is behind what this process applied
    /// (schema, policy, membership and cache changes; row data is not covered, see
    /// [`OrmBuilder::read_pool_max_lag`]). Reads are routed only while this process knows the
    /// changes it would have to apply: its signal listener must be running
    /// ([`Orm::spawn_signal_listener`]) or the replay lag must be bounded
    /// ([`OrmBuilder::read_pool_max_lag`]); otherwise reads use the primary, counted in
    /// [`crate::RoutingStats::listener_down`]. Before the first routed transaction on each
    /// connection of the pool it is verified ([`Orm::check_read_pool`]): it must serve the
    /// primary's database and server and be read-only end to end, that is a standby or a login
    /// whose roles (the roles it can `SET ROLE` to included) have no write privileges on the
    /// ORM's tables (`default_transaction_read_only` is only a session default and does not
    /// count; log in as the read-only role, a `-c role=..` switch can be undone); the `READ ONLY`
    /// the ORM adds to every routed `BEGIN` stays the guard of each transaction. A pool that fails the check follows [`OrmBuilder::read_pool_failure`], except
    /// that a pool serving another database or an unsupported server is always an error.
    #[must_use]
    pub fn read_pool(mut self, pool: Pool) -> Self {
        self.read_pool = Some(pool);
        self
    }

    /// What an unreachable or unusable read pool does to a read transaction (default
    /// [`crate::ReadPoolFailure::Fail`]). Setting it without [`OrmBuilder::read_pool`] is refused
    /// by [`OrmBuilder::build`].
    #[must_use]
    pub fn read_pool_failure(mut self, failure: crate::route::ReadPoolFailure) -> Self {
        self.read_pool_failure = Some(failure);
        self
    }

    /// Bound the replay lag of the read pool: a routed transaction measures it with its `BEGIN`
    /// flight (`pg_last_xact_replay_timestamp()`) and runs on the primary when the lag is over
    /// `lag` or cannot be measured (counted in [`crate::RoutingStats::replica_lagging`]). Without
    /// it only the signal generations are compared, which covers schema, policy, membership and
    /// cache changes but not row data. A server that is not in recovery (a read-only role of the
    /// primary) has lag 0. Caveats: the replay timestamp is the commit time of the last replayed
    /// transaction, so an idle primary looks like a growing lag; and it is compared with the
    /// replica's clock, so the hosts' clocks must agree. Meaningful only with
    /// [`OrmBuilder::read_pool`]; `lag` must be longer than zero ([`OrmBuilder::build`]).
    #[must_use]
    pub fn read_pool_max_lag(mut self, lag: Duration) -> Self {
        self.read_pool_max_lag = Some(lag);
        self
    }

    /// Register a write hook (design note §4.4): the flush `UPDATE`s and upsert updates
    /// of the hooked fields return their old values in the same statement and hand them to the
    /// hook. Checked against the registry by [`OrmBuilder::build`].
    #[must_use]
    pub fn write_hook(mut self, hook: crate::version::WriteHook) -> Self {
        self.refuse_override("OrmBuilder::write_hook");
        self.write_hooks.push(hook);
        self
    }

    /// Register an unlink hook: called after every `unlink` with the model and the
    /// deleted ids, inside the transaction. Hooks run in registration order.
    #[must_use]
    pub fn unlink_hook(mut self, hook: crate::version::UnlinkHook) -> Self {
        self.refuse_override("OrmBuilder::unlink_hook");
        self.unlink_hooks.push(hook);
        self
    }

    /// Register a precommit hook (D7): run at commit, after the flush and
    /// before `COMMIT`, in registration order; whatever it writes is flushed and the hooks run
    /// again while tracked changes are still pending (at most
    /// [`OrmConfig::max_flush_rounds`] rounds). The mail addon drains
    /// [`Transaction::take_tracked_changes`] in its hook.
    #[must_use]
    pub fn precommit_hook(mut self, hook: crate::tracking::PrecommitHook) -> Self {
        self.refuse_override("OrmBuilder::precommit_hook");
        self.precommit_hooks.push(hook);
        self
    }

    /// Register a selection ondelete callback.
    #[must_use]
    pub fn selection_callback(mut self, callback: crate::version::SelectionCallback) -> Self {
        self.refuse_override("OrmBuilder::selection_callback");
        self.selection_callbacks.push(callback);
        self
    }

    /// Require the core tables `tables`: [`OrmBuilder::build`] and [`Orm::sync_schema`]
    /// refuse with a `Programming` error naming the table and its owner crate when no linked
    /// [`core_ddl!`](crate::core_ddl) registration creates one of them. A registration is a
    /// static nothing calls, so the linker drops it from a binary that references nothing else of
    /// its object file, and the schema step would then skip the table silently; the expectation
    /// lives at the build site, which is always linked. The owner crate exports its list (for
    /// authentication: `erp_security::auth::core_tables()`). Calls add up; also allowed on a
    /// builder made by [`Orm::rebuild`] (which carries the list of its ORM).
    pub fn require_core_tables(
        mut self,
        tables: impl IntoIterator<Item = &'static crate::CoreTableRequirement>,
    ) -> Self {
        extend_unique(&mut self.required_core_tables, tables);
        self
    }

    /// Require the job kinds `kinds`: [`OrmBuilder::build`] refuses with a `Programming`
    /// error naming the kind and its owner crate when no linked
    /// [`register_job_kind!`](crate::register_job_kind) registers one of them. A registration is
    /// a static nothing calls, so the linker drops it from a binary that references nothing else
    /// of its object file; the expectation lives at the build site. Build also validates every
    /// linked kind ([`crate::jobs::validate_kinds`]). Calls add up; also allowed on a builder
    /// made by [`Orm::rebuild`].
    pub fn require_job_kinds(
        mut self,
        kinds: impl IntoIterator<Item = &'static crate::jobs::JobKindRequirement>,
    ) -> Self {
        extend_unique(&mut self.required_job_kinds, kinds);
        self
    }

    /// Explicit static job policy for isolated core fixtures, or local capacity in database mode.
    /// Without a source, absent settings use catalogue defaults. With a source, explicit Shared
    /// process overrides are refused. Also allowed on a builder made by [`Orm::rebuild`].
    #[must_use]
    pub fn job_settings(mut self, settings: crate::jobs::JobSettings) -> Self {
        self.job_settings = Some(Arc::new(settings));
        self
    }

    /// Use committed database authority for Shared job settings.
    /// Local capacity comes from `job_settings`; explicit Shared process overrides are refused.
    #[must_use]
    pub fn job_settings_source(mut self, source: Arc<dyn crate::jobs::JobSettingsSource>) -> Self {
        self.job_settings_source = Some(source);
        self
    }

    /// The capability that authorizes job administration (hold, resume, cancel, requeue,
    /// discard). `erp-orm` cannot declare a capability itself, so the binary names
    /// the one `erp-security` declares (`erp_security::jobs::JOBS_ADMIN`). Without it only the
    /// system principal may administer jobs and a user's attempt is a `Programming` error. Also
    /// allowed on a builder made by [`Orm::rebuild`].
    #[must_use]
    pub fn job_admin_capability(mut self, capability: &'static crate::Capability) -> Self {
        self.job_admin = Some(capability);
        self
    }

    /// The capability that lets a principal that is not `System` act as another user with
    /// `with_user` (`principal.impersonate`, A4). `erp-orm` cannot declare
    /// a capability itself, so the binary names the one `erp-security` declares
    /// (`erp_security::principal::PRINCIPAL_IMPERSONATE`). Without it every switch to another
    /// user by a principal that is not `System` is refused with a `Programming` error naming this
    /// call; switches by `System` and to the switcher's own subject need no capability. Also
    /// allowed on a builder made by [`Orm::rebuild`].
    #[must_use]
    pub fn impersonate_capability(mut self, capability: &'static crate::Capability) -> Self {
        self.impersonate = Some(capability);
        self
    }

    /// Enable cross-process change signals: every transaction reads the generations
    /// with its `BEGIN` and applies what changed before its first statement.
    #[must_use]
    pub fn signal(mut self, signal: crate::signal::Signal) -> Self {
        self.refuse_override("OrmBuilder::signal");
        self.signal = Some(Arc::new(signal));
        self
    }

    /// Install the access policy: a [`StaticPolicy`], any [`AccessPolicy`] snapshot (served for
    /// ever), or a [`PolicyProvider`] (`Arc<dyn PolicyProvider>`, `erp-security`'s
    /// `PolicySource`). Without one every model operation is refused (fail closed).
    ///
    /// A builder made by [`Orm::rebuild`] carries the policy of the ORM it came from (every stage
    /// ORM of a database enforces one policy): calling this on it makes [`OrmBuilder::build`]
    /// fail with a `Programming` error instead of replacing the carried policy.
    #[must_use]
    pub fn policy(mut self, policy: impl IntoPolicyProvider) -> Self {
        self.policy = Some(if self.rebuilt {
            Err(Error::programming(
                "OrmBuilder::policy on a builder made by Orm::rebuild: a rebuilt ORM keeps the \
                 access policy of the ORM it came from; build a new ORM with Orm::builder to \
                 use another policy",
            ))
        } else {
            policy.into_policy_provider()
        });
        self
    }

    /// Install the principal resolver (`erp-security`).
    ///
    /// With a read pool ([`OrmBuilder::read_pool`]) the contract of a resolver is: every table it
    /// reads a principal's groups, companies or active flag from belongs to a model registered
    /// as a [`crate::SourceKind::Membership`] source that declares a `membership` signal trigger
    /// (`signal_triggers = [("_membership_bump", "membership")]`, on the relation table for a
    /// many2many). A replica that has not replayed a change to a table without the trigger
    /// cannot be detected, and the stale membership would be served. [`OrmBuilder::build`]
    /// checks the registrations; it cannot see which tables a custom resolver reads.
    #[must_use]
    pub fn resolver(mut self, resolver: impl PrincipalResolver) -> Self {
        self.refuse_override("OrmBuilder::resolver");
        self.resolver = Some(Arc::new(resolver));
        self
    }

    /// Accept a policy that decides on resolved principals without a resolver: the resolutions
    /// callers build ([`crate::Resolution::new`]) are then trusted and never re-read. Tests that
    /// supply the principals themselves (property tests over fabricated groups) only; a service
    /// installs a resolver.
    ///
    /// Only with the `trusted-test-principals` feature (enabled by `erp-testkit`; never in
    /// production builds).
    #[cfg(feature = "trusted-test-principals")]
    #[must_use]
    pub fn trust_caller_resolutions(mut self) -> Self {
        self.refuse_override("OrmBuilder::trust_caller_resolutions");
        self.trust_caller_resolutions = true;
        self
    }

    /// Add policy-source registrations built at runtime (tests); the linked
    /// [`POLICY_SOURCE_WRITES`] always count.
    #[must_use]
    pub fn policy_sources(
        mut self,
        sources: impl IntoIterator<Item = &'static PolicySourceWrite>,
    ) -> Self {
        self.refuse_override("OrmBuilder::policy_sources");
        self.policy_sources.extend(sources);
        self
    }

    /// Add method implementations built at runtime (tests).
    #[must_use]
    pub fn impls(mut self, impls: impl IntoIterator<Item = &'static MethodImpl>) -> Self {
        extend_unique(&mut self.impls, impls);
        self
    }

    /// Add the WebAssembly implementations of some addons: they join the override
    /// chains of their models at the position of their addon, and replace the linked native
    /// implementations of that addon. Carried over by [`Orm::rebuild`]; an implementation given
    /// again for the same addon, model and method replaces the earlier one.
    #[must_use]
    pub fn wasm_methods(
        mut self,
        methods: impl IntoIterator<Item = crate::dispatch::WasmMethodImpl>,
    ) -> Self {
        for method in methods {
            self.wasm_methods.retain(|known| {
                !(known.model == method.model
                    && known.method == method.method
                    && known.invoker.addon() == method.invoker.addon())
            });
            self.wasm_methods.push(method);
        }
        self
    }

    /// Add `#[rpc]` declarations built at runtime (tests).
    #[must_use]
    pub fn rpc_impls(
        mut self,
        impls: impl IntoIterator<Item = &'static crate::rpc::RpcImpl>,
    ) -> Self {
        self.rpc_impls.extend(impls);
        self
    }

    /// Add compute implementations built at runtime (tests).
    #[must_use]
    pub fn computes(mut self, computes: impl IntoIterator<Item = &'static ComputeImpl>) -> Self {
        extend_unique(&mut self.computes, computes);
        self
    }

    /// Add constraint declarations built at runtime (tests).
    #[must_use]
    pub fn constraints(
        mut self,
        constraints: impl IntoIterator<Item = &'static ConstraintImpl>,
    ) -> Self {
        extend_unique(&mut self.constraints, constraints);
        self
    }

    /// Register nonstored search method implementations.
    #[must_use]
    pub fn search_methods(
        mut self,
        search_methods: impl IntoIterator<Item = &'static crate::search_method::FieldSearchImpl>,
    ) -> Self {
        extend_unique(&mut self.search_methods, search_methods);
        self
    }

    /// Use `cache` for resolved guard plans instead of a default-sized one made at
    /// `build`. Create it with [`crate::ResolvedGuardCacheConfig::new`], which rejects a size
    /// outside the configurable range. [`Orm::rebuild`] carries the cache over.
    #[must_use]
    pub fn resolved_guard_cache(mut self, cache: Arc<crate::access::ResolvedGuardCache>) -> Self {
        self.resolved_guards = Some(cache);
        self
    }

    /// Override the tunables.
    #[must_use]
    pub fn config(mut self, config: OrmConfig) -> Self {
        self.config = config;
        self
    }

    /// Build the dispatch tables and the runtime.
    ///
    /// # Errors
    ///
    /// `Programming` when a read pool is configured without a signal
    /// ([`OrmBuilder::read_pool`]), when `read_pool_failure` or `read_pool_max_lag` is set
    /// without a read pool or the lag is zero, when the policy needs resolved principals and no resolver is
    /// installed, when a linked `capability!` has a malformed name or a name declared twice,
    /// when a policy-source registration watches a field its model does
    /// not have, when, with a read pool configured, a model registered as a
    /// [`crate::SourceKind::Membership`] source declares no membership signal trigger, when an
    /// addon's `#[access_domain]` overrides one model twice or overrides a model
    /// the addon does not declare, or when the method, compute, constraint, onchange or RPC
    /// exposure tables cannot be built from the registry (an override narrowing an `#[rpc]`
    /// effect, a receiver mismatch, two `#[rpc]` declarations of one method in one addon; ADR
    /// 0038), or when a translated field (`translate = true`) is also computed or
    /// related. These refusals happen before any connection or schema operation.
    pub fn build(self) -> Result<Orm> {
        self.build_orm()
    }

    /// [`OrmBuilder::build`], and the one [`BaseInstallGrant`] of the ORM, which lets a
    /// transaction be marked as loading the base module's data
    /// ([`Transaction::begin_base_install`]). Only the build site can hold it: a plain
    /// [`OrmBuilder::build`] mints none, and nothing reachable from a [`Transaction`] or an `Env`
    /// returns one. The process's module-install executor (the data loader) takes the
    /// grant from the build site, before cron, hooks or RPC start. On a builder made by
    /// [`Orm::rebuild`] the grant is the original ORM's again (one id per lineage): building
    /// with this method there mints nothing the original's holder did not already have.
    ///
    /// # Errors
    ///
    /// As [`OrmBuilder::build`].
    pub fn build_with_base_install_grant(self) -> Result<(Orm, BaseInstallGrant)> {
        let orm = self.build_orm()?;
        let grant = BaseInstallGrant { orm_id: orm.id() };
        Ok((orm, grant))
    }

    fn build_orm(mut self) -> Result<Orm> {
        self.config.search_limits.validate()?;
        if let Some(refused) = self.refused {
            return Err(refused);
        }
        crate::principal::validate_capabilities(crate::principal::CAPABILITIES.iter())?;
        for model in self.registry.models() {
            for field in &model.fields {
                if field.translate && (field.compute.is_some() || field.related.is_some()) {
                    return Err(Error::programming(format!(
                        "{}.{}: a translated field cannot be computed or related yet \
                         (a follow-up slice, not yet created)",
                        model.name, field.name
                    )));
                }
            }
        }
        if self.read_pool.is_none()
            && (self.read_pool_failure.is_some() || self.read_pool_max_lag.is_some())
        {
            return Err(Error::programming(
                "OrmBuilder::read_pool_failure and OrmBuilder::read_pool_max_lag need \
                 OrmBuilder::read_pool: without a read pool they would be silently ignored",
            ));
        }
        if self.read_pool_max_lag == Some(Duration::ZERO) {
            return Err(Error::programming(
                "OrmBuilder::read_pool_max_lag must be longer than zero",
            ));
        }
        if self.read_pool.is_some() && self.signal.is_none() {
            return Err(Error::programming(
                "OrmBuilder::read_pool needs OrmBuilder::signal: the replica lag guard compares the \
                 generations the read pool returns with the ones this process applied",
            ));
        }
        let policy = match self.policy {
            Some(provider) => provider?,
            None => StaticPolicy::DenyAll.into_policy_provider()?,
        };
        #[cfg(feature = "trusted-test-principals")]
        let trusted = self.trust_caller_resolutions;
        #[cfg(not(feature = "trusted-test-principals"))]
        let trusted = false;
        if policy.current().needs_resolved_principals() && self.resolver.is_none() && !trusted {
            return Err(Error::programming(
                "the installed access policy decides on the groups and companies of the subject, \
                 but no principal resolver is installed (OrmBuilder::resolver): a resolution \
                 built by the caller would be trusted and a membership change never re-read",
            ));
        }
        let mut policy_sources: FxHashMap<ModelId, Vec<&'static PolicySourceWrite>> =
            FxHashMap::default();
        for source in POLICY_SOURCE_WRITES
            .iter()
            .chain(self.policy_sources.iter().copied())
        {
            if let Some(spec) = source.validate(&self.registry)? {
                // a replica that has not replayed a membership change is detected only through
                // the `membership` generation the model's trigger bumps
                if self.read_pool.is_some()
                    && source.kind() == crate::SourceKind::Membership
                    && !spec.table_objects.iter().any(|o| {
                        matches!(
                            o.kind,
                            erp_registry::TableObjectKind::SignalTrigger(d)
                                if d.signal == erp_registry::TriggerSignal::Membership
                        )
                    })
                {
                    return Err(Error::programming(format!(
                        "the model '{}' is registered as a membership source (SourceKind::Membership) \
                         but declares no membership signal trigger (`signal_triggers = \
                         [(\"<name>\", \"membership\")]`): with a read pool configured a replica that \
                         has not replayed a change to it could not be detected",
                        source.model()
                    )));
                }
                policy_sources.entry(spec.id).or_default().push(source);
            }
        }
        // the policy runs the chains while it compiles; a broken override fails here, at
        // startup, whatever policy is installed
        crate::access::AccessDomainChains::build(&self.registry, &[])?;
        crate::core_ddl::validate(&self.registry, &self.required_core_tables)?;
        crate::jobs::validate_kinds(&crate::jobs::linked_kinds(), &self.required_job_kinds)?;
        let dispatch = Dispatch::build_with(&self.registry, &self.impls, &self.wasm_methods)?;
        crate::generated_input::validate_chains(&self.registry, &dispatch)?;
        let exposure =
            crate::rpc::ExposureTable::build(&self.registry, &dispatch, &self.rpc_impls)?;
        let computes: Vec<&'static ComputeImpl> = COMPUTE_IMPLS
            .iter()
            .chain(self.computes.iter().copied())
            .collect();
        let constraints: Vec<&'static ConstraintImpl> = CONSTRAINT_IMPLS
            .iter()
            .chain(self.constraints.iter().copied())
            .collect();
        let graph = ComputeGraph::build(&self.registry, &dispatch, &computes, &constraints)?;
        for model in self.registry.models() {
            for field in &model.fields {
                if let Some(compute) = graph.compute((model.id, field.id)) {
                    let reads_property = compute
                        .readable
                        .iter()
                        .any(|f| model.field_by_id(*f).company_dependent)
                        || compute.input_paths().any(|path| {
                            path.iter().any(|(m, f)| {
                                self.registry
                                    .model_by_id(*m)
                                    .field_by_id(*f)
                                    .company_dependent
                            })
                        })
                        || graph.upstream((model.id, field.id)).iter().any(|(m, f)| {
                            self.registry
                                .model_by_id(*m)
                                .field_by_id(*f)
                                .company_dependent
                        });
                    if reads_property
                        && (compute.stored || !compute.depends_context.contains(&"company"))
                    {
                        return Err(crate::company_dependent_runtime::unsupported(
                            model.name,
                            field.name,
                            "compute dependencies without nonstored company context",
                        ));
                    }
                }
            }
        }
        let relations = crate::relational::Relations::build(&self.registry)?;
        let onchange = crate::onchange::OnchangeMethods::build(&self.registry, &dispatch)?;
        let write_hooks = crate::version::WriteHooks::build(&self.registry, &self.write_hooks)?;
        let exclusions = crate::exclusion::Exclusions::build(&self.registry)?;
        let search_methods =
            crate::search_method::FieldSearchChains::build(&self.registry, &self.search_methods)?;

        let mut seen_callbacks = rustc_hash::FxHashSet::default();
        for cb in &self.selection_callbacks {
            if cb.name().is_empty() {
                return Err(Error::programming(
                    "selection callback name cannot be empty",
                ));
            }
            if !seen_callbacks.insert(cb.name()) {
                return Err(Error::programming(format!(
                    "duplicate selection callback '{}' registered on OrmBuilder",
                    cb.name()
                )));
            }
        }

        // Normal registry/dispatch/exposure checks are complete. These products remain
        // private until every required factory succeeds; no partial ORM is published.
        self.generation_attachments
            .compose(&self.registry, &exposure)?;

        // The ORM sets these three limits with every BEGIN (SET LOCAL), so they replace what a
        // role, a database, a pooler or the pool's options configured; report them once.
        let limits = self.config.timeouts;
        tracing::info!(
            target: "erp::orm",
            lock_timeout_ms = limits.lock().as_millis(),
            statement_timeout_ms = limits.statement().as_millis(),
            transaction_timeout_ms = limits.transaction().as_millis(),
            "transaction limits set by the ORM with every BEGIN (they replace role, database and \
             pool defaults)"
        );
        let job_settings = self.job_settings.take().unwrap_or_default();
        if self.job_settings_source.is_some() {
            job_settings.refuse_shared_overrides()?;
        }
        Ok(Orm {
            shared: Arc::new(Shared {
                generation_attachments: Arc::new(self.generation_attachments),
                tables: write_gens::TableModels::build(&self.registry),
                registry: self.registry,
                dispatch,
                graph,
                relations,
                onchange,
                exclusions,
                search_methods,
                ormcache: SharedOrmCache::default(),
                resolved_guards: self.resolved_guards.unwrap_or_else(|| {
                    Arc::new(crate::access::ResolvedGuardCache::new(
                        crate::access::ResolvedGuardCacheConfig::default(),
                    ))
                }),
                policy,
                resolver: self.resolver,
                policy_sources,
                exposure,
                search_memo: crate::search::SearchMemo::new(self.config.search_memo_size),
                config: self.config,
                database: self.database,
                signal: self.signal,
                read: self.read_pool.map(|pool| crate::route::ReadRoute {
                    pool,
                    on_failure: self.read_pool_failure.unwrap_or_default(),
                    max_lag: self.read_pool_max_lag,
                    verify: crate::route::VerifyCache::default(),
                }),
                routing: crate::route::RoutingCounters::default(),
                write_hooks,
                unlink_hooks: self.unlink_hooks.clone(),
                precommit_hooks: self.precommit_hooks.clone(),
                selection_callbacks: self.selection_callbacks.clone(),
                extras: Extras {
                    impls: self.impls,
                    wasm_methods: self.wasm_methods,
                    computes: self.computes,
                    constraints: self.constraints,
                    search_methods: self.search_methods,
                    write_hooks: self.write_hooks,
                    unlink_hooks: self.unlink_hooks,
                    precommit_hooks: self.precommit_hooks,
                    selection_callbacks: self.selection_callbacks,
                    policy_sources: self.policy_sources,
                    rpc_impls: self.rpc_impls,
                    #[cfg(feature = "trusted-test-principals")]
                    trusted,
                    required_core_tables: self.required_core_tables,
                    required_job_kinds: self.required_job_kinds,
                },
                job_settings,
                job_settings_compatibility: self
                    .job_settings_source
                    .as_ref()
                    .map(|s| s.compatibility()),
                job_settings_source: self.job_settings_source,
                job_admin: self.job_admin,
                impersonate: self.impersonate,
            }),
            pool: self.pool,
        })
    }
}

/// Retried in [`Orm::run`]: serialization failures and deadlocks. `55P03` (lock not
/// available) is not: it answers a caller who asked not to wait.
pub(crate) fn is_retryable(error: &Error) -> bool {
    error.kind() == ErrorKind::Concurrency && matches!(error.sqlstate(), Some("40001" | "40P01"))
}

/// A small xorshift generator for backoff jitter (no cryptographic use).
pub(crate) fn jitter(bound_ms: u64) -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut x = seed ^ STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    if bound_ms == 0 { 0 } else { x % (bound_ms + 1) }
}

impl Orm {
    /// Process-unique id of this ORM's lineage: clones and every [`Orm::rebuild`] of it share it
    /// (a [`BaseInstallGrant`] of the original stays valid on the stage ORMs of a module
    /// operation). Exists for the dev-only testkit; it only identifies the ORM and grants nothing.
    #[doc(hidden)]
    pub fn id(&self) -> u64 {
        self.shared.database.orm_id
    }

    /// The largest number of connections the primary pool opens (its configured size): services
    /// that bound their own concurrency below it (the login admission of `erp_security::auth`)
    /// read it.
    pub fn primary_pool_max_size(&self) -> usize {
        self.pool.status().max_size
    }

    /// Start building the ORM of a database. Each call starts a new database identity: ORMs built
    /// this way never accept each other's transactions in [`Transaction::switch_orm`], even on the
    /// same pool; use [`Orm::rebuild`] for another registry of the same database.
    pub fn builder(registry: Arc<Registry>, pool: Pool) -> OrmBuilder {
        OrmBuilder {
            generation_attachments: crate::generation::GenerationAttachments::default(),
            registry,
            pool,
            policy: None,
            resolver: None,
            #[cfg(feature = "trusted-test-principals")]
            trust_caller_resolutions: false,
            policy_sources: Vec::new(),
            impls: Vec::new(),
            wasm_methods: Vec::new(),
            rpc_impls: Vec::new(),
            computes: Vec::new(),
            constraints: Vec::new(),
            search_methods: Vec::new(),
            config: OrmConfig::default(),
            database: Arc::new(DatabaseIdentity::new()),
            resolved_guards: None,
            signal: None,
            write_hooks: Vec::new(),
            unlink_hooks: Vec::new(),
            precommit_hooks: Vec::new(),
            selection_callbacks: Vec::new(),
            required_core_tables: Vec::new(),
            required_job_kinds: Vec::new(),
            job_settings: None,
            job_settings_source: None,
            job_admin: None,
            impersonate: None,
            read_pool: None,
            read_pool_failure: None,
            read_pool_max_lag: None,
            rebuilt: false,
            refused: None,
        }
    }

    /// Start building an ORM for another registry of the same database: same pool, access policy,
    /// configuration and database identity, so a transaction of this ORM can
    /// [`Transaction::switch_orm`] to the result (–2: the stage registries of a module
    /// operation). Method, compute and constraint implementations given to the original builder
    /// are carried over (each registry uses those of its addons); more can be added, and one
    /// given again is not duplicated. The access policy cannot be replaced: [`OrmBuilder::policy`]
    /// on the returned builder makes `build` fail.
    ///
    /// ```ignore
    /// let stage = orm.rebuild(Arc::new(stage_registry)).build()?;
    /// tx.switch_orm(&stage).await?;
    /// ```
    pub fn rebuild(&self, registry: Arc<Registry>) -> OrmBuilder {
        OrmBuilder {
            generation_attachments: crate::generation::GenerationAttachments::default(),
            registry,
            pool: self.pool.clone(),
            policy: Some(Ok(Arc::clone(&self.shared.policy))),
            resolver: self.shared.resolver.clone(),
            #[cfg(feature = "trusted-test-principals")]
            trust_caller_resolutions: self.shared.extras.trusted,
            policy_sources: self.shared.extras.policy_sources.clone(),
            impls: self.shared.extras.impls.clone(),
            wasm_methods: self.shared.extras.wasm_methods.clone(),
            rpc_impls: self.shared.extras.rpc_impls.clone(),
            computes: self.shared.extras.computes.clone(),
            constraints: self.shared.extras.constraints.clone(),
            search_methods: self.shared.extras.search_methods.clone(),
            config: self.shared.config.clone(),
            database: Arc::clone(&self.shared.database),
            resolved_guards: Some(Arc::clone(&self.shared.resolved_guards)),
            signal: self.shared.signal.clone(),
            write_hooks: self.shared.extras.write_hooks.clone(),
            unlink_hooks: self.shared.extras.unlink_hooks.clone(),
            precommit_hooks: self.shared.extras.precommit_hooks.clone(),
            selection_callbacks: self.shared.extras.selection_callbacks.clone(),
            required_core_tables: self.shared.extras.required_core_tables.clone(),
            required_job_kinds: self.shared.extras.required_job_kinds.clone(),
            job_settings: Some(Arc::clone(&self.shared.job_settings)),
            job_settings_source: self.shared.job_settings_source.clone(),
            job_admin: self.shared.job_admin,
            impersonate: self.shared.impersonate,
            // the read routing of the ORM it came from: a stage registry of a module operation
            // keeps routing reads to the replica (the verification cache starts empty)
            read_pool: self.shared.read.as_ref().map(|read| read.pool.clone()),
            read_pool_failure: self.shared.read.as_ref().map(|read| read.on_failure),
            read_pool_max_lag: self.shared.read.as_ref().and_then(|read| read.max_lag),
            rebuilt: true,
            refused: None,
        }
    }

    /// True when `other` belongs to the same lineage as this ORM: both were built, directly or
    /// through other [`Orm::rebuild`] calls, from one [`Orm::builder`]. Only ORMs of one lineage
    /// accept each other's transactions in [`Transaction::switch_orm`]. It says nothing about the
    /// connection pool: two lineages can be built on the same pool (and so the same database).
    pub fn same_lineage(&self, other: &Orm) -> bool {
        Arc::ptr_eq(&self.shared.database, &other.shared.database)
    }

    /// Routing decisions since startup, and whether a read pool is configured. With a read pool,
    /// a process that is not running its signal listener (and has no
    /// [`OrmBuilder::read_pool_max_lag`]) reads from the primary, counted in
    /// [`crate::RoutingStats::listener_down`]: start the listener or bound the lag.
    pub fn routing_stats(&self) -> crate::route::RoutingStats {
        self.shared.routing.snapshot(self.shared.read.is_some())
    }

    /// The counters and size of the resolved guard plan cache.
    pub fn resolved_guard_cache_stats(&self) -> crate::access::ResolvedGuardCacheStats {
        self.shared.resolved_guards.stats()
    }

    /// The cross-process signal state, when enabled.
    pub fn signal(&self) -> Option<&crate::signal::Signal> {
        self.shared.signal.as_deref()
    }

    /// The compute implementations given to this ORM's builder ([`OrmBuilder::computes`]), for
    /// another ORM over the same registry: a computed field of the registry cannot be built
    /// without its compute.
    pub fn runtime_computes(&self) -> &[&'static ComputeImpl] {
        &self.shared.extras.computes
    }

    /// The core tables this ORM requires ([`OrmBuilder::require_core_tables`]).
    pub fn required_core_tables(&self) -> &[&'static crate::CoreTableRequirement] {
        &self.shared.extras.required_core_tables
    }

    /// Local job capacity and static-source settings. Shared values here are not live policy.
    pub fn job_node_settings(&self) -> &crate::jobs::JobSettings {
        &self.shared.job_settings
    }

    /// Read the current committed job policy for a new admission.
    /// # Errors
    /// Source errors refuse admission; database mode never falls back to static defaults.
    pub async fn job_settings_snapshot(&self) -> Result<Arc<crate::jobs::JobSettingsSnapshot>> {
        self.shared.job_snapshot().await
    }

    /// Native composition can share authority with a separately pooled minimal owner ORM.
    pub fn job_settings_source(&self) -> Option<Arc<dyn crate::jobs::JobSettingsSource>> {
        self.shared.job_settings_source.clone()
    }

    /// Node identity checks provider compatibility in database mode, or static values otherwise.
    pub fn job_settings_identity(&self) -> &dyn crate::node::SharedSettings {
        match &self.shared.job_settings_compatibility {
            Some(identity) => identity,
            None => self.shared.job_settings.as_ref(),
        }
    }

    /// The job kinds this ORM was built to require
    /// ([`OrmBuilder::require_job_kinds`]).
    pub fn required_job_kinds(&self) -> &[&'static crate::jobs::JobKindRequirement] {
        &self.shared.extras.required_job_kinds
    }

    /// The registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.shared.registry
    }

    /// The dispatch tables.
    pub fn dispatch(&self) -> &Dispatch {
        &self.shared.dispatch
    }

    /// The RPC exposure table of this ORM's registry.
    pub fn exposure(&self) -> &crate::rpc::ExposureTable {
        &self.shared.exposure
    }

    /// Hits, misses and size of the compiled-search memo.
    pub fn search_memo_stats(&self) -> crate::search::MemoStats {
        self.shared.search_memo.stats()
    }

    /// An environment on this ORM's registry for `principal` (a [`PrincipalHint`] or a
    /// [`crate::UserId`]; [`crate::SUPERUSER_ID`] is the `System` principal).
    pub fn env(&self, principal: impl Into<PrincipalHint>, context: Context) -> Env {
        Env::new(
            Arc::clone(&self.shared.registry),
            principal.into().principal(),
            context,
        )
        .with_lineage(Arc::clone(&self.shared.database.lineage))
        .with_generation_attachments(Arc::clone(&self.shared.generation_attachments))
    }

    /// [`Orm::env`] for a caller (A4): an environment of the `User`
    /// principal of `subject`, never `System`. The entry addon code uses to build an environment
    /// for a user it acts for; the hint-taking [`Orm::env`] also builds the `System` one.
    ///
    /// # Errors
    ///
    /// `Access` ("the superuser cannot be opened as a caller: core code opens the system
    /// principal itself") for [`crate::SUPERUSER_ID`].
    pub fn env_caller(&self, subject: crate::UserId, context: Context) -> Result<Env> {
        Ok(self.env(PrincipalHint::caller(subject)?, context))
    }

    /// Whether this ORM's lineage is `lineage` (by identity).
    pub(crate) fn has_lineage(&self, lineage: &Lineage) -> bool {
        Arc::ptr_eq(&self.shared.database.lineage, lineage)
    }

    /// The connection pool.
    pub(crate) fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Open a transaction for `principal` (no statement is sent yet): a [`PrincipalHint`] or a
    /// [`crate::UserId`] (`orm.begin(SUPERUSER_ID)` opens one as `System`).
    ///
    /// Until resolves principals against the database, a user principal is unresolved: no
    /// groups and no companies, as under today's static policies.
    pub async fn begin(&self, principal: impl Into<PrincipalHint>) -> Result<Transaction> {
        self.begin_with(principal, Context::new()).await
    }

    /// Open a transaction for a caller (A4): the `User` principal of
    /// `subject`, resolved as [`Orm::begin`] resolves it, never `System`. The entry addon code
    /// uses to open a transaction for a user it acts for; the hint-taking entries
    /// ([`Orm::begin`], [`Orm::begin_with`], [`Orm::run`], [`BeginOptions::new`]) also open the
    /// `System` one.
    ///
    /// ```no_run
    /// # async fn demo(orm: &erp_orm::Orm, user: erp_orm::UserId) -> erp_fields::Result<()> {
    /// let tx = orm.begin_caller(user).await?;
    /// assert_eq!(tx.env().principal().kind(), erp_orm::PrincipalKind::User);
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// `Access` ("the superuser cannot be opened as a caller: core code opens the system
    /// principal itself") for [`crate::SUPERUSER_ID`], before any statement; then as
    /// [`Orm::begin_opts`].
    pub async fn begin_caller(&self, subject: crate::UserId) -> Result<Transaction> {
        self.begin_opts(BeginOptions::caller(subject)?).await
    }

    /// Open a transaction with a context.
    pub async fn begin_with(
        &self,
        principal: impl Into<PrincipalHint>,
        context: Context,
    ) -> Result<Transaction> {
        self.begin_opts(BeginOptions::new(principal).context(context))
            .await
    }

    /// Open a transaction as described by `options`: a resolved [`Principal`], a context, and
    /// whether it is read-only ([`TxIntent::Read`]). No statement is sent
    /// yet; a read-only transaction starts with `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`.
    ///
    /// ```no_run
    /// # async fn example(orm: &erp_orm::Orm) -> erp_fields::Result<()> {
    /// use erp_orm::{BeginOptions, SUPERUSER_ID};
    /// let mut tx = orm
    ///     .begin_opts(BeginOptions::new(SUPERUSER_ID).read_only("res.partner.search_read"))
    ///     .await?;
    /// assert!(tx.intent().is_read());
    /// # Ok(()) }
    /// ```
    pub async fn begin_opts(&self, options: BeginOptions) -> Result<Transaction> {
        if options.credential {
            return Err(Error::programming(
                "BeginOptions::credential() opens only through Orm::begin_credential, which \
                 checks the credential in the opening flight",
            ));
        }
        let (mut tx, hint) = self.open_unresolved(options).await?;
        if let Some(hint) = hint {
            tx.resolve_begin(hint).await?;
        }
        Ok(tx)
    }

    /// Open a transaction whose principal comes from `check` (the credential variant
    /// of the principal resolver): on the primary, the check's
    /// statement leaves in the opening flight with `BEGIN`, the limits and the signal read (one
    /// round trip), and [`CredentialCheck::finish`](crate::CredentialCheck::finish) decides.
    /// Accepted: the transaction acts as the principal it returned, with that subject's
    /// resolution recorded as for [`Orm::begin_opts`]. Refused: the transaction is rolled back and
    /// the refusal returned.
    ///
    /// # Errors
    ///
    /// `Programming` when `finish` accepts a `System` principal or one with full elevation; the
    /// check's errors; the opening flight's errors (the transaction is discarded).
    pub async fn begin_credential<C: crate::CredentialCheck>(
        &self,
        options: CredentialOptions,
        check: &C,
    ) -> Result<crate::CredentialBegin<C::Accepted, C::Refused>> {
        // made only by `CredentialOptions::new` (credential, primary, REPEATABLE READ): the type
        // carries what the runtime checks used to (type design T4)
        let options = options.0;
        let statement = check.statement()?;
        let (tx, _) = self.open_unresolved(options).await?;
        tx.begin_credential_flight(&statement, check).await
    }

    /// One transaction of `last` alone, on the primary: `BEGIN` with the isolation and the limits,
    /// `last` and `COMMIT` leave together and are answered in one round trip; `last`'s rows. The
    /// flight reads no cross-process generation and hands out no [`Transaction`], so no
    /// record path can run in it: for raw, set-based statements over core tables that depend on no
    /// cache of this process (the login's read and write and the session updates of
    /// `erp_security::auth`).
    ///
    /// # Errors
    ///
    /// [`CommitError::RolledBack`] with `Programming` for options whose principal needs a
    /// statement to resolve (only the system or a resolved principal opens without one), for
    /// credential options, or when a read-only `last` may write; with
    /// the server's error when `BEGIN`, the limits, `last` or `COMMIT` was refused (nothing
    /// committed). [`CommitError::Unknown`] when an answer is missing. `options` are routed to
    /// the primary whatever they say (types-delta N-b).
    pub async fn one_flight(
        &self,
        options: BeginOptions,
        last: FinalStatement<'_>,
    ) -> std::result::Result<Vec<Row>, CommitError> {
        let mut tx = self
            .begin_opts(options.on_primary())
            .await
            .map_err(CommitError::RolledBack)?;
        if tx.route != crate::route::Route::Primary {
            return Err(CommitError::RolledBack(Error::programming(
                "Orm::one_flight opened a transaction that is not on the primary (internal error)",
            )));
        }
        if tx.state != State::Idle {
            return Err(CommitError::RolledBack(Error::programming(
                "Orm::one_flight needs a transaction that has sent nothing: the system or a \
                 resolved principal (a principal hint is resolved by a statement of its own)",
            )));
        }
        let rows = Box::pin(tx.start_committing(last)).await?;
        tx.committed();
        Ok(rows)
    }

    /// The transaction of `options` before any statement, with the hint to resolve.
    async fn open_unresolved(
        &self,
        options: BeginOptions,
    ) -> Result<(Transaction, Option<PrincipalHint>)> {
        let BeginOptions {
            principal,
            hint,
            context,
            intent,
            read_audit_owner,
            isolation,
            timeouts,
            on_primary,
            credential: _,
            operation_identity,
            #[cfg(feature = "test-faults")]
            import_snapshot,
            #[cfg(feature = "test-faults")]
            break_before_reroute,
            #[cfg(feature = "test-faults")]
            lag_read_sql,
            #[cfg(feature = "test-faults")]
            signal_read_sql,
        } = options;
        // checked before a connection is taken: nothing has been sent
        refuse_switched_principal(&principal)?;
        #[cfg(feature = "test-faults")]
        if isolation == Isolation::ReadCommitted && import_snapshot.is_some() {
            return Err(Error::programming(
                "BeginOptions::import_snapshot needs REPEATABLE READ: PostgreSQL imports a \
                 snapshot only into a REPEATABLE READ or SERIALIZABLE transaction",
            ));
        }
        #[cfg(feature = "test-identities")]
        let operation_identity = match operation_identity {
            Some(identity) => Some(identity),
            None if self.shared.config.identify_untagged => {
                Some(Arc::new(crate::OperationIdentity::generate()?))
            }
            None => None,
        };
        let unit = crate::route::current_unit();
        // an owned read-only transaction keeps its elevation evidence across a rollback inside a
        // method body (the owner's attempt row needs it after the body returns)
        let retain_elevations = read_audit_owner.is_some();
        let crate::route_orm::Checkout {
            conn,
            route,
            primary,
            fallback,
        } = self.checkout(&intent, unit.as_ref(), on_primary).await?;
        // the seam only exists for the read route: on the primary it would silently run the
        // primary against an older snapshot
        #[cfg(feature = "test-faults")]
        if import_snapshot.is_some() && route != crate::route::Route::ReadPool {
            return Err(Error::programming(
                "BeginOptions::import_snapshot needs a transaction routed to the read pool",
            ));
        }
        self.shared.refresh_statements(&conn);
        let tx = Transaction {
            search_state: crate::search_method::SearchState::default(),
            env: Env::new(Arc::clone(&self.shared.registry), principal, context)
                .with_lineage(Arc::clone(&self.shared.database.lineage))
                .with_operation_identity(operation_identity)
                .with_generation_attachments(Arc::clone(&self.shared.generation_attachments)),
            policy: self.shared.policy.current(),
            access: crate::access::AccessState::default(),
            refresh: crate::access::RefreshState::default(),
            private_texts: Mutex::new(FxHashMap::default()),
            intent,
            read_audit_owner,
            isolation,
            raw_statements_only: false,
            method_effects: None,
            hook_scope: None,
            effect_scratch: FxHashMap::default(),
            timeouts: timeouts.unwrap_or(self.shared.config.timeouts),
            watch: crate::timeouts::LimitWatch::default(),
            route,
            primary,
            unit,
            fallback,
            #[cfg(feature = "test-faults")]
            import_snapshot,
            #[cfg(feature = "test-faults")]
            break_before_reroute,
            #[cfg(feature = "test-faults")]
            lag_read_sql,
            #[cfg(feature = "test-faults")]
            signal_read_sql,
            failed_sql: None,
            #[cfg(feature = "test-faults")]
            commit_fault: None,
            shared: Arc::clone(&self.shared),
            conn: Some(conn),
            state: State::Idle,
            cache: Cache::default(),
            row_ranges: raw_row::RowRanges::default(),
            queries: 0,
            mutations: 0,
            default_gets: 0,
            extensions: FxHashMap::default(),
            capture: None,
            #[cfg(feature = "test-faults")]
            company_chunk: None,
            now: None,
            after_commit: Vec::new(),
            engine: EngineState::default(),
            poison: None,
            savepoints: Vec::new(),
            cursors: cursors::Cursors::default(),
            write_gens: write_gens::WriteGens::default(),
            isolated: Vec::new(),
            isolation_seq: 0,
            switched_from: Vec::new(),
            schema_changed: false,
            signal_state: crate::signal::TxSignal::default(),
            outbox: crate::outbox::Buffer::new(crate::outbox::BufferLimits::default())?,
            audit: if retain_elevations {
                crate::audit::AuditBuffer::retaining_elevations()
            } else {
                crate::audit::AuditBuffer::new()
            },
            blocks: Arc::default(),
            pending_capture: crate::outbox::Pending::default(),
            signal_read: true,
            versions: crate::version::Expectations::default(),
            tracking: crate::tracking::TrackBuffer::default(),
            base_installing: false,
            connection_tainted: false,
            initial_locks: None,
            strings_unsure: false,
            operation: None,
            orm_id: self.id(),
            copy_tracker: None,
            #[cfg(feature = "test-faults")]
            copy_limits_for_tests: None,
            scoped_ids: FxHashMap::default(),
            html_reports: crate::html::Reports::default(),
            html_mode: erp_fields::html::SanitizeMode::Clean,
            notices: Vec::new(),
            savepoint_notices: Vec::new(),
            notices_overflowed: 0,
            active_langs: None,
            langs_dirty: false,
            translated_baselines: FxHashMap::default(),
            candidate_stage: None,
        };
        Ok((tx, hint))
    }

    /// Run a unit of work in a transaction and commit it. On a serialization failure
    /// or deadlock — from a statement or from the commit — the transaction is rolled back, its cache
    /// discarded, and the work runs again in a new transaction after a full-jitter exponential
    /// backoff, up to `max_attempts` attempts in total. Any other error rolls back and is
    /// returned. After-commit hooks registered by the work run only after the final successful
    /// commit.
    pub async fn run<T, F>(
        &self,
        principal: impl Into<PrincipalHint>,
        context: Context,
        mut work: F,
    ) -> Result<T>
    where
        F: AsyncFnMut(&mut Transaction) -> Result<T>,
    {
        let options = RunOptions::legacy(
            BeginOptions::new(principal).context(context),
            RetryPolicy::legacy(&self.shared.config),
        );
        crate::route::unit_of_work(self.run_attempts(options, &mut work))
            .await
            .map_err(RunFailure::into_error)
    }

    /// Run a repeatable native unit with explicit transaction and retry options.
    /// Each retry gets a fresh transaction, cache and snapshot; the supplied operation identity
    /// remains stable. External effects and manual commits do not belong in the callback.
    /// A failed attempt's after-commit hooks are discarded. A connection loss with an uncertain
    /// commit outcome is not retried.
    /// # Errors
    /// [`RunFailure`] retains the original error and actual attempt/exhaustion metadata.
    /// Invalid options or a refused top-level boundary fail before opening an attempt.
    pub async fn run_with_options<T, F>(
        &self,
        options: RunOptions,
        mut work: F,
    ) -> std::result::Result<T, RunFailure>
    where
        F: AsyncFnMut(&mut Transaction) -> Result<T>,
    {
        let options = options
            .prepare(self.shared.config.timeouts)
            .map_err(|error| RunFailure::new(error, 0, false))?;
        crate::route::unit_of_work(self.run_attempts(options, &mut work)).await
    }

    async fn run_attempts<T, F>(
        &self,
        options: RunOptions,
        work: &mut F,
    ) -> std::result::Result<T, RunFailure>
    where
        F: AsyncFnMut(&mut Transaction) -> Result<T>,
    {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let mut tx = self
                .begin_opts(options.begin.clone())
                .await
                .map_err(|error| RunFailure::new(error, attempt, false))?;
            let outcome = match work(&mut tx).await {
                Ok(value) => {
                    let committed = if options.classified_commit {
                        tx.commit_pipelined(None)
                            .await
                            .map_err(|failure| match failure {
                                CommitError::RolledBack(error) => (error, true),
                                // An uncertain commit must never re-enter the body, even if a
                                // later driver happens to attach a contention-looking cause.
                                CommitError::Unknown(error) => (error, false),
                            })
                    } else {
                        tx.commit().await.map_err(|error| (error, true))
                    };
                    match committed {
                        Ok(()) => Ok(value),
                        // A refused commit can leave it open: roll it back.
                        Err((error, retry_allowed)) => {
                            Err((tx.discard_after(error).await, retry_allowed))
                        }
                    }
                }
                // A failed rollback detached the connection and is added to the work's error,
                // which stays the primary one.
                Err(error) => Err((tx.discard_after(error).await, true)),
            };
            // A completed transaction still owns its pool slot. Release it before sleeping or
            // opening another attempt; a one-connection pool must be sufficient.
            drop(tx);
            match outcome {
                Err((error, true))
                    if options.accepts(&error) && attempt < options.retry.attempts =>
                {
                    tokio::time::sleep(options.retry.delay(attempt)).await;
                }
                Err((error, retry_allowed)) => {
                    let exhausted = retry_allowed && options.accepts(&error);
                    return Err(RunFailure::new(error, attempt, exhausted));
                }
                Ok(value) => return Ok(value),
            }
        }
    }
}

/// Where the transaction stands with respect to its connection.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// No transaction open on the connection.
    Idle,
    /// `BEGIN` was sent and not yet acknowledged; if the future is dropped here, the connection's
    /// state is unknown.
    Beginning,
    /// Inside a transaction.
    Open,
    /// A statement failed: PostgreSQL rejects everything until the transaction ends.
    Aborted {
        sqlstate: Option<String>,
        message: String,
    },
    /// The connection was detached; the transaction cannot be used any more.
    Released,
}

/// What a transaction is opened: fixed at `BEGIN`, never changed.
///
/// A [`TxIntent::Read`] transaction starts with `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`
/// and refuses every write path before sending anything: flush of pending values, `create`,
/// `write`, `unlink`, row locks and raw statements that may change data, with a `Programming`
/// error naming the entry method. The database backs the guard (SQLSTATE `25006`, mapped to the
/// same error). routes on this marker.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TxIntent {
    /// May write (the default).
    Write,
    /// A `read`-effect entry call.
    Read {
        /// The entry method, `<model>.<method>`, named in the guard's error.
        entry: Arc<str>,
    },
}

impl TxIntent {
    /// True for [`TxIntent::Read`].
    pub fn is_read(&self) -> bool {
        matches!(self, Self::Read { .. })
    }

    /// The `BEGIN` statement of this intent at `isolation`.
    pub(crate) fn begin_sql(&self, isolation: Isolation) -> &'static str {
        match (isolation, self) {
            (Isolation::RepeatableRead, Self::Write) => "BEGIN ISOLATION LEVEL REPEATABLE READ",
            (Isolation::RepeatableRead, Self::Read { .. }) => {
                "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY"
            }
            (Isolation::ReadCommitted, Self::Write) => "BEGIN ISOLATION LEVEL READ COMMITTED",
            (Isolation::ReadCommitted, Self::Read { .. }) => {
                "BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY"
            }
        }
    }
}

/// The isolation level a transaction opens with ([`BeginOptions::isolation`]).
///
/// The ORM's guarantees (one snapshot per transaction for the record cache, the access memo and
/// the write-generation checks; serialization failures retried by [`Orm::run`]) are stated for
/// [`Isolation::RepeatableRead`], the default. [`Isolation::ReadCommitted`] is for
/// raw, set-based statements that must not hold one snapshot for their whole length and must not
/// fail on concurrent updates of the rows they touch or skip: the batched garbage collection of
/// the session and throttle tables, and the authentication service's short transactions (the
/// login's two phases, session rotation and touch through [`Orm::one_flight`], and the session
/// ends with their hooks, round 1 decisions 9 and 12, rust review
/// round 2 N-5). Each statement then reads a new snapshot, so such a transaction refuses the
/// record API (record reads and writes, searches, method calls, access checks) with a
/// `Programming` error; only raw `query`, `execute` and `batch` run in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Isolation {
    /// `READ COMMITTED`: a new snapshot per statement.
    ReadCommitted,
    /// `REPEATABLE READ`: one snapshot for the whole transaction (the default).
    #[default]
    RepeatableRead,
}

/// A write a [`TxIntent::Read`] transaction refuses ([`Transaction::require_writable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WriteOp {
    /// `create`.
    Create,
    /// `write`.
    Write,
    /// `upsert` on a business key.
    Upsert,
    /// `unlink`.
    Unlink,
    /// Sending pending (cached) values.
    Flush,
    /// A row lock (`lock_for_update`, `try_lock_for_update`).
    Lock,
}

impl WriteOp {
    /// The verb used in the guard's message.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Write => "write",
            Self::Upsert => "upsert",
            Self::Unlink => "unlink",
            Self::Flush => "flush pending values of",
            Self::Lock => "lock rows of",
        }
    }
}

/// Refuse to open a transaction for a principal whose impersonation chain starts at a principal
/// that is not `System` (A4): its switch is decided by the engine's admission check of the
/// environment, which a transaction of its own would skip. A principal only records the root of its
/// chain, so a chain that starts at `System` (`System` → A → T) opens one even when A's switch to T
/// would be refused in the environment; closing that needs the immediate switcher on the principal
/// and is left to the addon lint P6, which lists [`BeginOptions::for_principal`] beside
/// the hint-taking entries (any holder of an [`Orm`] can already open a transaction for any user).
fn refuse_switched_principal(principal: &Principal) -> Result<()> {
    match principal.actor() {
        Some(crate::Actor::Impersonator(switcher))
            if switcher.kind() != crate::PrincipalKind::System =>
        {
            Err(Error::access(format!(
                "a transaction cannot be opened for user {} as switched to by user {}: an \
                 environment made by with_user is admitted by the engine only; open the \
                 transaction for the caller (Orm::begin_caller)",
                principal.subject(),
                switcher.subject()
            )))
        }
        _ => Ok(()),
    }
}

/// The future of [`Transaction::admit_records`] and [`Transaction::prepare_switch`]: ready at once
/// for an environment without a pending `with_user` switch, otherwise the boxed resolution (and,
/// for the admission, the check).
#[must_use = "futures do nothing unless awaited"]
pub(crate) enum Admit<'a> {
    /// Decided when made (taken by the first poll).
    Ready(Option<Result<()>>),
    /// The switchers are resolved first.
    Pending(crate::BoxFuture<'a, Result<()>>),
}

impl std::future::Future for Admit<'_> {
    type Output = Result<()>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<()>> {
        match self.get_mut() {
            Self::Ready(outcome) => std::task::Poll::Ready(outcome.take().unwrap_or_else(|| {
                Err(Error::internal(
                    "the admission of an environment was polled after it completed",
                ))
            })),
            Self::Pending(future) => future.as_mut().poll(cx),
        }
    }
}

/// The declaration that the code opening a read-only transaction writes its attempt evidence
/// (amendment A2).
///
/// A read-only transaction cannot write its own audit row, so a scoped elevation block (or an
/// audited unscoped `sudo`) inside it leaves its evidence only in the transaction's attempt
/// ([`Transaction::take_audit_attempt`]). The owner is the opener that takes this contract on:
/// **before it releases any result, it writes that attempt evidence as one `read_ok` attempt row,
/// and it withholds the result when the row cannot be written.** A transaction without an owner
/// refuses every elevation, because nobody would write the evidence.
///
/// The name is a `&'static str` chosen in Rust by the opening code, never derived from request
/// input. It is declared with [`BeginOptions::read_only_audited`] or
/// [`CredentialOptions::read_only_audited`]. Only the server's request path and other core code
/// may declare one; addon crates must not (the P6 lint bans the constructors there).
///
/// ```
/// use erp_orm::ReadAuditOwner;
/// assert!(ReadAuditOwner::new("erp-rpc").is_ok());
/// assert!(ReadAuditOwner::new("").is_err());
/// ```
#[derive(Debug, Clone)]
pub struct ReadAuditOwner {
    name: &'static str,
}

impl ReadAuditOwner {
    /// The owner called `name` (for example `"erp-rpc"`).
    ///
    /// # Errors
    ///
    /// `Programming` for an empty name.
    pub fn new(name: &'static str) -> Result<Self> {
        if name.is_empty() {
            return Err(Error::programming("a read-audit owner needs a name"));
        }
        Ok(Self { name })
    }

    /// The owner's name.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// How [`Orm::begin_opts`] opens a transaction.
///
/// ```
/// use erp_orm::{BeginOptions, Context, SUPERUSER_ID};
/// let options = BeginOptions::new(SUPERUSER_ID)
///     .context(Context::new())
///     .read_only("res.partner.read");
/// assert!(options.intent().is_read());
/// ```
#[derive(Debug, Clone)]
#[must_use = "pass it to Orm::begin_opts"]
pub struct BeginOptions {
    operation_identity: Option<Arc<crate::OperationIdentity>>,
    principal: Principal,
    /// What to resolve at the start (with an installed resolver), when built from a hint.
    hint: Option<PrincipalHint>,
    context: Context,
    intent: TxIntent,
    /// Declared with [`BeginOptions::read_only_audited`]; only ever set together with a read
    /// intent.
    read_audit_owner: Option<ReadAuditOwner>,
    isolation: Isolation,
    timeouts: Option<crate::timeouts::Timeouts>,
    on_primary: bool,
    /// Made by [`BeginOptions::credential`]: only [`Orm::begin_credential`] opens it.
    credential: bool,
    #[cfg(feature = "test-faults")]
    import_snapshot: Option<String>,
    #[cfg(feature = "test-faults")]
    break_before_reroute: bool,
    #[cfg(feature = "test-faults")]
    lag_read_sql: Option<String>,
    #[cfg(feature = "test-faults")]
    signal_read_sql: Option<String>,
}

impl BeginOptions {
    /// A read/write transaction for the principal a hint (or a uid) stands for, with an empty
    /// context. With a resolver installed ([`OrmBuilder::resolver`]) the principal is resolved
    /// against the database when the transaction opens (one statement after `BEGIN`).
    pub fn new(principal: impl Into<PrincipalHint>) -> Self {
        let hint = principal.into();
        Self {
            hint: Some(hint),
            ..Self::for_principal(hint.principal())
        }
    }

    /// [`BeginOptions::new`] for a caller (A4): the `User` principal of
    /// `subject`, never `System` ([`PrincipalHint::caller`]).
    ///
    /// # Errors
    ///
    /// [`crate::PrincipalError::SuperuserAsCaller`] (kind `Access`) for [`crate::SUPERUSER_ID`].
    pub fn caller(subject: crate::UserId) -> std::result::Result<Self, crate::PrincipalError> {
        Ok(Self::new(PrincipalHint::caller(subject)?))
    }

    /// A read/write transaction for a resolved principal (its companies, scope and debug flag
    /// apply), with an empty context. [`Orm::begin_opts`] refuses (`Access`, before any
    /// statement) a principal whose impersonation chain starts at a principal other than `System`
    /// (A4): that switch is decided only by the engine's admission of the environment. A
    /// chain rooted at `System` is not refused here (the principal records the root only);
    /// P6's lint lists this entry.
    pub fn for_principal(principal: Principal) -> Self {
        Self {
            operation_identity: None,
            principal,
            hint: None,
            context: Context::new(),
            intent: TxIntent::Write,
            read_audit_owner: None,
            isolation: Isolation::RepeatableRead,
            timeouts: None,
            on_primary: false,
            credential: false,
            #[cfg(feature = "test-faults")]
            import_snapshot: None,
            #[cfg(feature = "test-faults")]
            break_before_reroute: false,
            #[cfg(feature = "test-faults")]
            lag_read_sql: None,
            #[cfg(feature = "test-faults")]
            signal_read_sql: None,
        }
    }

    /// Attach immutable correlation/execution metadata supplied by the entry point.
    /// Keep the same identity when retrying one call; create a fresh identity for
    /// a new execution. This grants no access and changes no idempotency binding.
    pub fn operation_identity(mut self, identity: crate::OperationIdentity) -> Self {
        self.operation_identity = Some(Arc::new(identity));
        self
    }

    /// A read/write transaction whose principal comes from a credential checked in its opening
    /// flight, for [`Orm::begin_credential`]. It
    /// always runs on the primary ([`BeginOptions::on_primary`]): credential rows are checked
    /// where revocation is immediate. [`Orm::begin_opts`] refuses it. Callers outside the ORM
    /// use [`CredentialOptions`], which cannot change the isolation or the route.
    pub(crate) fn credential() -> Self {
        Self {
            credential: true,
            on_primary: true,
            ..Self::for_principal(Principal::for_uid(
                crate::principal::UserId::CREDENTIAL_PLACEHOLDER,
            ))
        }
    }

    /// Set these limits for the transaction instead of [`OrmConfig::timeouts`] (a long import, a
    /// job chunk).
    pub fn timeouts(mut self, timeouts: crate::timeouts::Timeouts) -> Self {
        self.timeouts = Some(timeouts);
        self
    }

    /// Run the transaction on the primary even when it is a read and a read pool is configured
    /// (it stays `READ ONLY`). For decisions that must not rest on a replica's lagging state, such
    /// as "is this registry current". Not counted as a routing decision.
    pub fn on_primary(mut self) -> Self {
        self.on_primary = true;
        self
    }

    /// Test seam for the replica lag guard: send `SET TRANSACTION SNAPSHOT '<snapshot>'` right
    /// after `BEGIN`, so the transaction reads the state of an exported snapshot
    /// (`pg_export_snapshot()`), like a replica that has not replayed what came after. The server
    /// refuses an id that is not an exported snapshot. Only with the `test-faults` feature.
    #[cfg(feature = "test-faults")]
    pub fn import_snapshot(mut self, snapshot: impl Into<String>) -> Self {
        self.import_snapshot = Some(snapshot.into());
        self
    }

    /// Test seam for the failed-`ROLLBACK` path: when the transaction leaves the read pool, its
    /// backend is terminated first, so the `ROLLBACK` fails. Only with the `test-faults` feature.
    #[cfg(feature = "test-faults")]
    pub fn break_before_reroute(mut self) -> Self {
        self.break_before_reroute = true;
        self
    }

    /// Test seam for the replica lag guard: run `sql` instead of the replay lag read at `BEGIN`
    /// (it must answer `(in_recovery bool, lag_seconds float8)`), so a test can report a standby
    /// that is over the bound, or a read that cannot be parsed, without a real standby. Only with
    /// the `test-faults` feature.
    #[cfg(feature = "test-faults")]
    pub fn lag_read_sql(mut self, sql: impl Into<String>) -> Self {
        self.lag_read_sql = Some(sql.into());
        self
    }

    /// Test seam: run `sql` instead of the generation read at `BEGIN` (for a replica that lacks
    /// the signal table: name a table that does not exist). Used on the read route only; the
    /// primary after a reroute reads the real table. Only with the `test-faults` feature.
    #[cfg(feature = "test-faults")]
    pub fn signal_read_sql(mut self, sql: impl Into<String>) -> Self {
        self.signal_read_sql = Some(sql.into());
        self
    }

    /// The context of the transaction's environment.
    pub fn context(mut self, context: Context) -> Self {
        self.context = context;
        self
    }

    /// Open it read-only for the entry method `entry` (`<model>.<method>`), with no read-audit
    /// owner (clears one declared before): a scoped elevation block or an audited unscoped `sudo`
    /// in it is refused.
    pub fn read_only(mut self, entry: impl Into<Arc<str>>) -> Self {
        self.intent = TxIntent::Read {
            entry: entry.into(),
        };
        self.read_audit_owner = None;
        self
    }

    /// Open it read-only for the entry method `entry` (`<model>.<method>`) and declare `owner`,
    /// which promises to write the transaction's attempt evidence as one `read_ok` attempt row
    /// before releasing any result (see [`ReadAuditOwner`]). With an owner a scoped elevation
    /// block ([`Transaction::elevated`]) and [`Transaction::audited_unscoped_sudo`] are admitted;
    /// the transaction stays `READ ONLY`. The owner is not part of [`TxIntent`].
    pub fn read_only_audited(self, entry: impl Into<Arc<str>>, owner: ReadAuditOwner) -> Self {
        let mut options = self.read_only(entry);
        options.read_audit_owner = Some(owner);
        options
    }

    /// The intent it opens with.
    pub fn intent(&self) -> &TxIntent {
        &self.intent
    }

    /// The read-audit owner it opens with, if one was declared.
    pub fn read_audit_owner(&self) -> Option<&ReadAuditOwner> {
        self.read_audit_owner.as_ref()
    }

    /// Open it at `isolation` instead of [`Isolation::RepeatableRead`] (see [`Isolation`] for
    /// what changes); combinable with [`BeginOptions::read_only`]. A credential transaction
    /// cannot set it: [`CredentialOptions`] always opens REPEATABLE READ.
    pub fn isolation(mut self, isolation: Isolation) -> Self {
        self.isolation = isolation;
        self
    }

    /// The isolation level it opens with.
    pub fn isolation_level(&self) -> Isolation {
        self.isolation
    }

    /// The `BEGIN` statement it opens with (before the limits).
    #[cfg(test)]
    pub(crate) fn begin_sql(&self) -> &'static str {
        self.intent.begin_sql(self.isolation)
    }
}

/// How [`Orm::begin_credential`] opens the request's transaction: [`BeginOptions::credential`]
/// with the request's context, limits and read intent, and nothing else (no isolation level, no
/// route: a credential is checked on the primary at REPEATABLE READ; review round 1,
/// type design T4).
#[derive(Debug, Clone)]
#[must_use = "pass it to Orm::begin_credential"]
pub struct CredentialOptions(BeginOptions);

impl CredentialOptions {
    /// The read-audit owner it opens with, if one was declared.
    pub fn read_audit_owner(&self) -> Option<&ReadAuditOwner> {
        self.0.read_audit_owner()
    }

    /// On the primary, read/write, the ORM's limits.
    pub fn new() -> Self {
        Self(BeginOptions::credential())
    }

    /// The context of the request's environment.
    pub fn context(self, context: Context) -> Self {
        Self(self.0.context(context))
    }

    /// Attach the server's correlation/execution metadata to the credential transaction.
    pub fn operation_identity(self, identity: crate::OperationIdentity) -> Self {
        Self(self.0.operation_identity(identity))
    }

    /// These limits instead of [`OrmConfig::timeouts`].
    pub fn timeouts(self, timeouts: crate::timeouts::Timeouts) -> Self {
        Self(self.0.timeouts(timeouts))
    }

    /// Open it read-only for the entry method `entry` (`<model>.<method>`), with no read-audit
    /// owner.
    pub fn read_only(self, entry: impl Into<Arc<str>>) -> Self {
        Self(self.0.read_only(entry))
    }

    /// Open it read-only for the entry method `entry` and declare `owner`
    /// ([`BeginOptions::read_only_audited`]).
    pub fn read_only_audited(self, entry: impl Into<Arc<str>>, owner: ReadAuditOwner) -> Self {
        Self(self.0.read_only_audited(entry, owner))
    }
}

impl Default for CredentialOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The last statement [`Transaction::commit_pipelined`] sends before `COMMIT`, as an unnamed
/// statement with typed parameters (no prepare round trip).
#[derive(Clone, Copy)]
pub struct FinalStatement<'a> {
    sql: &'a str,
    params: &'a [(&'a (dyn ToSql + Sync), tokio_postgres::types::Type)],
}

impl<'a> FinalStatement<'a> {
    /// `sql` with its parameters and their types.
    pub fn new(
        sql: &'a str,
        params: &'a [(&'a (dyn ToSql + Sync), tokio_postgres::types::Type)],
    ) -> Self {
        Self { sql, params }
    }
}

impl std::fmt::Debug for FinalStatement<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FinalStatement")
            .field("sql", &self.sql)
            .field("params", &self.params.len())
            .finish()
    }
}

/// Why [`Transaction::commit_pipelined`] did not commit.
#[derive(Debug)]
#[non_exhaustive]
pub enum CommitError {
    /// Nothing was committed, for certain; the error says why (it keeps its kind and SQLSTATE,
    /// so `40001`/`40P01` can be retried). It also carries a refusal made before anything was
    /// sent (a managed transaction, a [`HookScope`]), when nothing was rolled back either.
    RolledBack(Error),
    /// `COMMIT` was sent but no answer arrived: the change may or may not have been applied.
    /// Never retry blindly. After-commit hooks do not run, because the outcome is not known.
    Unknown(Error),
}

impl CommitError {
    /// The underlying error.
    pub fn error(&self) -> &Error {
        match self {
            Self::RolledBack(e) | Self::Unknown(e) => e,
        }
    }

    /// The underlying error, by value.
    pub fn into_error(self) -> Error {
        match self {
            Self::RolledBack(e) | Self::Unknown(e) => e,
        }
    }
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RolledBack(e) => write!(f, "rolled back: {e}"),
            Self::Unknown(e) => write!(f, "commit outcome unknown: {e}"),
        }
    }
}

impl std::error::Error for CommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error())
    }
}

/// A fault [`Transaction::commit_pipelined`] simulates (tests of the ambiguous-commit path).
#[cfg(feature = "test-faults")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommitFault {
    /// The connection closes before `COMMIT` is sent: nothing is committed, the outcome is
    /// reported unknown.
    ConnectionClosedBeforeCommit,
    /// `COMMIT` is applied, then the answer is lost: the outcome is reported unknown.
    AnswerLostAfterCommit,
}

/// Functions a `READ ONLY` transaction still allows whose effect outlives the statement:
/// notifications sent at commit, session-level settings and session advisory locks, which stay on
/// the pooled connection after the call (design note §5.3). The read guard refuses them by name.
/// The transaction-level advisory locks are refused too: on a standby they lock only
/// the standby, so code that relied on them to exclude a primary holder would silently lose the
/// exclusion once its read is routed. So are (review round 5, L2) the functions that release a
/// session advisory lock taken earlier on the pooled connection by someone else, and those that
/// end or interrupt another session; the `dblink` functions are refused as a family
/// ([`DBLINK`]). A read that needs any of them declares a wider effect.
///
/// Not refused: a function of the database or of another extension that does the same inside.
const SESSION_STATE_FUNCTIONS: [&str; 15] = [
    "pg_notify",
    "set_config",
    "pg_advisory_lock",
    "pg_advisory_lock_shared",
    "pg_try_advisory_lock",
    "pg_try_advisory_lock_shared",
    "pg_advisory_xact_lock",
    "pg_advisory_xact_lock_shared",
    "pg_try_advisory_xact_lock",
    "pg_try_advisory_xact_lock_shared",
    "pg_advisory_unlock",
    "pg_advisory_unlock_shared",
    "pg_advisory_unlock_all",
    "pg_terminate_backend",
    "pg_cancel_backend",
];

/// The `dblink` extension's functions, `dblink` and every `dblink_…` name: they run statements
/// in another session, outside the read-only transaction.
const DBLINK: &str = "dblink";

/// The first [`SESSION_STATE_FUNCTIONS`] name `sql` calls (in the list's order, lowercase), else
/// the first `dblink…` name as written, judged on the words outside comments and string literals.
///
/// Precondition: the caller has read `sql` with certainty (`needs_isolation` returned false, which
/// it never does for a statement the scanner is not certain about): a `U&` name can spell a
/// function this does not see. Every name starts with `p`, `s` or `d` and is at least six bytes
/// long, so most words are passed over on their first byte and length.
fn session_state_function(sql: &str) -> Option<&str> {
    // which of the functions the statement names (bit i: SESSION_STATE_FUNCTIONS[i])
    let mut named = 0u32;
    let mut dblink = None;
    for word in sql_words::SqlWords::new(sql) {
        let w = word.text;
        let bytes = w.as_bytes();
        if bytes.len() < DBLINK.len()
            || !bytes
                .first()
                .is_some_and(|b| matches!(b | 0x20, b'p' | b's' | b'd'))
        {
            continue;
        }
        if dblink.is_none()
            && bytes
                .get(..DBLINK.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(DBLINK.as_bytes()))
            && bytes.get(DBLINK.len()).is_none_or(|b| *b == b'_')
        {
            dblink = Some(w);
            continue;
        }
        for (i, f) in SESSION_STATE_FUNCTIONS.iter().enumerate() {
            if f.len() == bytes.len() && w.eq_ignore_ascii_case(f) {
                named |= 1 << i;
            }
        }
    }
    SESSION_STATE_FUNCTIONS
        .iter()
        .enumerate()
        .find(|(i, _)| named & (1 << i) != 0)
        .map(|(_, f)| *f)
        .or(dblink)
}

/// The guard's error for a raw statement that may change data in a read-only transaction.
/// Whether the next significant text is `(`, skipping whitespace and SQL comments (`-- ...` and
/// nested `/* ... */`), so `nextval/**/(...)` counts as a call. An unterminated block comment
/// ends the text, which is not a call.
fn next_is_open_paren(mut rest: &str) -> bool {
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            match after.find('\n') {
                Some(end) => rest = &after[end + 1..],
                None => return false,
            }
        } else if rest.starts_with("/*") {
            let bytes = rest.as_bytes();
            let (mut depth, mut i) = (0usize, 0usize);
            loop {
                if i + 1 >= bytes.len() {
                    return false;
                }
                match (bytes[i], bytes[i + 1]) {
                    (b'/', b'*') => {
                        depth += 1;
                        i += 2;
                    }
                    (b'*', b'/') => {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => i += 1,
                }
            }
            rest = &rest[i..];
        } else {
            return rest.starts_with('(');
        }
    }
}

fn read_only_sql_error(entry: &str) -> Error {
    Error::programming(format!(
        "'{entry}' is declared effect = read but tried to write through SQL; declare effect = \
         write (or wider)."
    ))
}

/// The error of committing a transaction aborted by an earlier statement.
fn aborted_commit_error(sqlstate: Option<String>, message: &str) -> Error {
    let kind = if matches!(sqlstate.as_deref(), Some("40001" | "40P01")) {
        ErrorKind::Concurrency
    } else {
        ErrorKind::Database
    };
    Error::new(
        kind,
        format!("transaction rolled back: an earlier statement failed ({message})"),
    )
    .with_sqlstate(sqlstate)
}

/// Poll two futures that send requests on one connection, `a` always first, so their requests
/// go out back to back in that order and are answered in one round trip (as `begin_read_extra`).
pub(crate) async fn pipeline2<A, B>(a: A, b: B) -> (A::Output, B::Output)
where
    A: std::future::Future,
    B: std::future::Future,
{
    use std::task::Poll;
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    let mut a_out = None;
    let mut b_out = None;
    std::future::poll_fn(|cx| {
        if a_out.is_none()
            && let Poll::Ready(r) = a.as_mut().poll(cx)
        {
            a_out = Some(r);
        }
        if b_out.is_none()
            && let Poll::Ready(r) = b.as_mut().poll(cx)
        {
            b_out = Some(r);
        }
        match (a_out.take(), b_out.take()) {
            (Some(x), Some(y)) => Poll::Ready((x, y)),
            (x, y) => {
                a_out = x;
                b_out = y;
                Poll::Pending
            }
        }
    })
    .await
}

/// Await `$main`, a statement's future on `$conn`, with the pending `CLOSE`s of finished streams
/// (`$closes`, an `Option<&Closes>`) sent in front of it on the same connection, in the same round
/// trip (design note `perf-stream.md` §3.3: the `CLOSE` rides the transaction's next statement).
/// Gives `(close outcome, main's output)`.
///
/// A macro, not a generic `async fn`: `$main` is built inside the arm that awaits it, so without
/// closes the statement's future is awaited inline exactly as before streams existed, and the
/// pipelined pair is boxed. A wrapper `async fn` holds `main` both as its argument and as the
/// awaited value, which doubled the size of every statement future and overflowed the 2 MiB
/// test-thread stacks of deep debug-build call chains.
macro_rules! behind_closes {
    ($conn:expr, $closes:expr, $main:expr) => {
        match $closes {
            None => (Ok(()), $main.await),
            Some(closes) => {
                Box::pin($crate::transaction::pipeline2(
                    $conn.batch_execute(&closes.sql),
                    $main,
                ))
                .await
            }
        }
    };
}
pub(crate) use behind_closes;

/// A database transaction with its record cache.
pub struct Transaction {
    pub(crate) search_state: crate::search_method::SearchState,
    env: Env,
    /// The policy snapshot pinned for the transaction.
    pub(crate) policy: Arc<dyn AccessPolicy>,
    /// Memoized record checks (design note §3.3).
    pub(crate) access: crate::access::AccessState,
    /// Policy and membership changes of this transaction, its resolutions (design note §3.4).
    pub(crate) refresh: crate::access::RefreshState,
    /// Predicate texts compiled under a private policy snapshot (never in the shared memo).
    pub(crate) private_texts:
        Mutex<FxHashMap<(erp_sql::filter::ShapeKey, usize), crate::search::MemoText>>,
    /// Read-only or read/write, fixed at `BEGIN`.
    intent: TxIntent,
    /// The read-audit owner declared at open ([`BeginOptions::read_only_audited`]). It lives in
    /// the transaction value, so a replica-to-primary reroute (same value, new connection) keeps
    /// it.
    read_audit_owner: Option<ReadAuditOwner>,
    /// The isolation level of `BEGIN` ([`BeginOptions::isolation`]).
    isolation: Isolation,
    /// Scoped refusal of record operations, independent of database isolation.
    raw_statements_only: bool,
    method_effects: Option<Arc<MethodEffectState>>,
    /// The live [`HookScope`]'s hook and first refusal.
    hook_scope: Option<HookScopeState>,
    effect_scratch: FxHashMap<std::any::TypeId, Box<dyn std::any::Any + Send + Sync>>,
    /// The limits set with `BEGIN`; the errors of statements that hit them name them.
    timeouts: crate::timeouts::Timeouts,
    /// When the transaction began and its last statement was sent, and whether a limit was
    /// changed inside it: what typing a limit error needs besides the server's error.
    watch: crate::timeouts::LimitWatch,
    /// The pool the connection came from.
    route: crate::route::Route,
    /// The primary pool a routed transaction falls back to (`Some` only on the read pool).
    primary: Option<Pool>,
    /// The unit of work the transaction was opened in (its write mark), kept so a transaction
    /// used later or in another task still reads it.
    unit: Option<crate::route::UnitFlag>,
    /// Why this read transaction runs on the primary; counted once its `BEGIN` succeeded.
    fallback: Option<crate::route::Reason>,
    /// Test seam: the snapshot to import right after `BEGIN` ([`BeginOptions::import_snapshot`]).
    #[cfg(feature = "test-faults")]
    import_snapshot: Option<String>,
    /// Test seam: terminate the read backend before the reroute's `ROLLBACK`.
    #[cfg(feature = "test-faults")]
    break_before_reroute: bool,
    /// Test seam: the replay lag read to run instead of the real one ([`BeginOptions::lag_read_sql`]).
    #[cfg(feature = "test-faults")]
    lag_read_sql: Option<String>,
    /// Test seam: the generation read to run instead of the real one
    /// ([`BeginOptions::signal_read_sql`]).
    #[cfg(feature = "test-faults")]
    signal_read_sql: Option<String>,
    /// The last statement the server refused ([`Transaction::failed_statement`]).
    failed_sql: Option<String>,
    /// A fault the next [`Transaction::commit_pipelined`] simulates (tests only).
    #[cfg(feature = "test-faults")]
    commit_fault: Option<CommitFault>,
    pub(crate) shared: Arc<Shared>,
    conn: Option<Object>,
    state: State,
    pub(crate) cache: Cache,
    /// The field ranges of the row being decoded, reused for every fetched row.
    row_ranges: raw_row::RowRanges,
    queries: u64,
    /// Changes made so far: data statements sent and pending writes put in the cache. An ORM
    /// operation failing after it changed something poisons the transaction (F1).
    mutations: u64,
    /// Number of `default_get` method dispatches executed in this transaction.
    default_gets: u64,
    /// State other layers keep for the life of this transaction ([`Transaction::extension`]).
    extensions: FxHashMap<std::any::TypeId, Box<dyn std::any::Any + Send + Sync>>,
    capture: Option<Vec<String>>,
    /// Ids per statement of the company check, lowered by tests (review R10).
    #[cfg(feature = "test-faults")]
    pub(crate) company_chunk: Option<usize>,
    pub(crate) now: Option<DateTime>,
    after_commit: Vec<Box<dyn FnOnce() + Send>>,
    pub(crate) engine: EngineState,
    /// Open savepoints (names), innermost last.
    savepoints: Vec<String>,
    /// The SQL cursors of this transaction's streams and its serial (PERF.STREAM-WINDOW).
    pub(crate) cursors: cursors::Cursors,
    /// When the rows of each model last changed in this transaction, as far as its statements
    /// and invalidations tell (a cursor stream's windows read fresh after it).
    pub(crate) write_gens: write_gens::WriteGens,
    /// Open isolated scopes (onchange), innermost last.
    isolated: Vec<Isolated>,
    /// Source of isolated scope names.
    isolation_seq: u64,
    /// The first `Validation` error of create/write/recompute/flush, or the first error of an ORM
    /// operation that had already changed data (F1): commit refuses until rollback (or
    /// until the savepoint it happened in is rolled back).
    poison: Option<(ErrorKind, String)>,
    /// ORMs this transaction switched away from since its last commit or rollback
    /// ([`Transaction::switch_orm`]): their shared ormcache is cleared when it commits, since it
    /// may hold values of data this transaction changed.
    switched_from: Vec<std::sync::Weak<Shared>>,
    /// DDL ran since the last commit or rollback: statements prepared since then may describe a
    /// table shape that a rollback undoes, so a rollback (whole or to a savepoint) drops the
    /// connection's prepared statements.
    schema_changed: bool,
    /// What this transaction sent for the signals.
    pub(crate) signal_state: crate::signal::TxSignal,
    /// Attempt-local capture. It remains owned here while commit SQL is awaiting its answer.
    pub(crate) outbox: crate::outbox::Buffer,
    /// The audit content of this attempt, written with one INSERT at commit.
    pub(crate) audit: crate::audit::AuditBuffer,
    /// The open scoped elevation blocks of this database transaction and their identity counters,
    /// shared with the guards that close them.
    pub(crate) blocks: Arc<Mutex<crate::principal::block::OpenBlocks>>,
    pub(crate) pending_capture: crate::outbox::Pending,
    /// Read the generations with `BEGIN` (off only for `sync_schema`, which creates the table).
    pub(crate) signal_read: bool,
    /// Expected row versions not yet checked by a flush (`expect_versions`).
    pub(crate) versions: crate::version::Expectations,
    /// The tracked-field log, scoped to savepoints (D8).
    pub(crate) tracking: crate::tracking::TrackBuffer,
    /// The base module's data is being loaded in this transaction ([`Transaction::begin_base_install`]).
    base_installing: bool,
    /// Code outside the ORM may have changed the connection's session state
    /// ([`Transaction::taint_connection`]): the connection is closed instead of going back to the
    /// pool.
    connection_tainted: bool,
    /// Internal lifecycle opening statement, consumed before all signal/snapshot reads.
    initial_locks: Option<String>,
    /// The server reported a new `standard_conforming_strings` or `client_encoding` on this
    /// transaction object's connection ([`Transaction::note_string_reading`], review round 5, M3):
    /// from then on every raw statement is unknown to the classifiers, and a read-intent
    /// transaction refuses raw SQL. Never cleared: a session-level change outlives the
    /// transaction, and the connection is closed with the transaction object.
    strings_unsure: bool,
    /// Set by [`Transaction::manage_operation`]: the id of the [`OperationControl`] that alone may
    /// end the transaction or switch its ORM.
    operation: Option<u64>,
    /// Lineage id of the [`Orm`] that opened this transaction ([`Orm::id`], checked against a
    /// [`BaseInstallGrant`]); [`Transaction::switch_orm`] stays in the lineage, so it holds there.
    orm_id: u64,
    /// State for recursive copy and copy_data.
    pub(crate) copy_tracker: Option<crate::copy::CopyTracker>,
    #[cfg(feature = "test-faults")]
    pub(crate) copy_limits_for_tests: Option<(usize, usize)>,
    /// Savepoint-scoped id sets, one per marker type (
    /// [`Transaction::mark_scoped_id`]).
    pub(crate) scoped_ids: FxHashMap<std::any::TypeId, ScopedIds>,
    /// Diagnostic notices posted on this transaction.
    pub(crate) notices: Vec<Notice>,
    pub(crate) html_reports: crate::html::Reports,
    pub(crate) html_mode: erp_fields::html::SanitizeMode,
    /// Notice counts at the start of open savepoints, for rollback truncation.
    savepoint_notices: Vec<usize>,
    /// Notices refused because the channel was full, this attempt.
    notices_overflowed: u64,
    /// Active language state memo.
    pub(crate) active_langs: Option<Arc<crate::lang::ActiveLanguages>>,
    /// A `res.lang` row was created, written or deleted since the memo was last loaded.
    pub(crate) langs_dirty: bool,
    /// Baselines for translated delta flush `(model, field, id) -> base_cell`.
    pub(crate) translated_baselines: FxHashMap<(ModelId, FieldId, RecId), Cell>,
    /// Results of candidate computes that same-call monetary admission ran, collected while a
    /// write is admitted so that the write publishes them instead of running the compute again
    /// (`None`: nobody collects; the result is used for admission only).
    pub(crate) candidate_stage: Option<Vec<crate::monetary::StagedCandidate>>,
}

/// The right to end a module operation's transaction ([`Transaction::manage_operation`]).
///
/// A module operation runs hooks, data loaders and model `init` / `_register_hook` methods with
/// `&mut Transaction`. Were they free to `COMMIT` or `ROLLBACK` it, half an operation would persist
/// (an idle transaction opens a new one on its next statement). The module system takes the one
/// control of its transaction and ends it through [`Transaction::operation_commit`],
/// [`Transaction::operation_commit_classified`] or [`Transaction::operation_rollback`]; [`Transaction::commit`], [`Transaction::rollback`],
/// [`Transaction::commit_pipelined`] and [`Transaction::switch_orm`] refuse a managed
/// transaction. The control is not `Clone`, and the transaction hands out one only.
///
/// Management lasts for the transaction's life: there is no way to hand the right back, and a
/// managed transaction refuses [`Transaction::commit`] and [`Transaction::rollback`] until it
/// ends through the control (or is dropped, which rolls it back).
#[derive(Debug)]
#[must_use = "dropping the control leaves the transaction impossible to end except by dropping it"]
pub struct OperationControl {
    id: u64,
}

/// The right to manage a transaction as a module operation ([`Transaction::manage_operation`]).
///
/// There is one per process: the module system claims it ([`OperationKey::claim`]) before any
/// other code asks, and keeps it private, so a hook, data loader or model method that holds a
/// `&mut Transaction` cannot mark that transaction as managed. A managed `System` transaction
/// is what `erp_modules::schedule_post_commit` accepts, so without this seal any code with an
/// unmanaged `System` transaction could pass it.
///
/// Trust boundary: the key is a capability inside one process. Rust code running in the same
/// process (a crate linked into the binary) can call [`OperationKey::claim`] first, which makes
/// the module system fail loudly when it builds; it cannot take the key after the module system
/// holds it. Code in the process is trusted not to do that; addon logic reaches the database
/// through hooks and models that never see the key.
#[derive(Debug)]
pub struct OperationKey {
    _sealed: (),
}

/// Set by the one successful [`OperationKey::claim`].
static OPERATION_KEY_CLAIMED: AtomicBool = AtomicBool::new(false);

impl OperationKey {
    /// Take the process's operation key. Only the module system calls this, once.
    ///
    /// # Errors
    /// `Programming` when the key was claimed already (by the module system, or by other code
    /// that ran before it).
    pub fn claim() -> Result<Self> {
        if OPERATION_KEY_CLAIMED.swap(true, Ordering::AcqRel) {
            return Err(Error::programming(
                "OperationKey::claim: the operation key was claimed already; the module system \
                 takes it once per process, before any other code",
            ));
        }
        Ok(Self { _sealed: () })
    }

    /// A key for tests of transactions managed outside a module system. Does not use the
    /// process's single key; never in production builds.
    #[cfg(feature = "test-faults")]
    pub fn for_tests() -> Self {
        Self { _sealed: () }
    }
}

/// Source of [`OperationControl`] ids (process-wide, so a control never matches another
/// transaction's).
static NEXT_CONTROL_ID: AtomicU64 = AtomicU64::new(1);

const PURE_EFFECT_ERROR: &str = "ForbiddenEffect: pure method scope refuses database, record, transaction or environment effects";

// The same lifetime and sticky-error mechanism serves both callback policies (I4).
// Pure nesting retains the first pure allowlist; a pure scope inside an adapter adds restrictions.
#[derive(Clone, Copy)]
enum MethodEffectPolicy {
    Pure {
        model: &'static str,
        methods: &'static [&'static str],
    },
    GeneratedInput {
        model: &'static str,
    },
}
struct MethodEffectState {
    policy: MethodEffectPolicy,
    env: Env,
    parent: Option<Arc<MethodEffectState>>,
    refused: Mutex<Option<String>>,
}
impl MethodEffectState {
    fn message(&self) -> String {
        match self.policy {
            MethodEffectPolicy::Pure { .. } => PURE_EFFECT_ERROR.to_owned(),
            MethodEffectPolicy::GeneratedInput { model } => format!(
                "Generated input adapter on '{model}' may read but cannot mutate, flush or change execution context"
            ),
        }
    }
    fn refusal(&self) -> Option<String> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn refuse(&self, message: String) -> Error {
        let message = self
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(message)
            .clone();
        // A nested callback cannot hide its refusal by dropping its guard without finish().
        let mut parent = self.parent.as_deref();
        while let Some(state) = parent {
            state
                .refused
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert_with(|| message.clone());
            parent = state.parent.as_deref();
        }
        Error::programming(message)
    }
}

/// A borrowing scope for deterministic native metadata methods only.
///
/// SQL, record operations, transaction control and altered environments refuse before
/// effects. Rejected calls remain sticky even when a callback catches their errors.
/// Dropping the scope restores its caller's restrictions on errors and cancellation.
/// Native code remains trusted: direct I/O outside the ORM is not sandboxed.
#[must_use = "keep the scope alive while running the pure chain"]
#[non_exhaustive]
pub struct PureMethods<'a> {
    transaction: &'a mut Transaction,
    previous: Option<Arc<MethodEffectState>>,
}

impl std::ops::Deref for PureMethods<'_> {
    type Target = Transaction;
    fn deref(&self) -> &Self::Target {
        self.transaction
    }
}
impl std::ops::DerefMut for PureMethods<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.transaction
    }
}
impl PureMethods<'_> {
    /// Finish the method chain. A caught forbidden operation still fails this scope.
    pub fn finish(self) -> Result<()> {
        match self
            .transaction
            .method_effects
            .as_ref()
            .and_then(|state| state.refusal())
        {
            Some(message) => Err(Error::programming(message)),
            None => Ok(()),
        }
    }
}
impl Drop for PureMethods<'_> {
    fn drop(&mut self) {
        self.transaction.method_effects = self.previous.take();
        if self.transaction.method_effects.is_none() {
            self.transaction.effect_scratch.clear();
        }
    }
}

/// A borrowing raw-statements-only scope, made by [`Transaction::raw_statements_only`].
///
/// Record operations, including cached reads and explicit ORM flushes, refuse while this
/// guard lives. Raw SQL and transaction metadata remain available. This is a consistency
/// guard for trusted native code, not a sandbox or a change of database isolation.
/// Dropping it restores the prior restriction, including during error propagation, unwind
/// or cancellation. It does not clear a transaction's existing cancellation or poison state.
#[must_use = "keep the guard alive while running raw-only work"]
#[non_exhaustive]
pub struct RawStatements<'a> {
    transaction: &'a mut Transaction,
    previous: bool,
}

impl std::ops::Deref for RawStatements<'_> {
    type Target = Transaction;

    fn deref(&self) -> &Self::Target {
        self.transaction
    }
}

impl std::ops::DerefMut for RawStatements<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.transaction
    }
}

impl Drop for RawStatements<'_> {
    fn drop(&mut self) {
        self.transaction.raw_statements_only = self.previous;
    }
}

/// Source of [`HookScopeState`] tokens (process-wide, so a state never matches another scope's).
static NEXT_HOOK_SCOPE_TOKEN: AtomicU64 = AtomicU64::new(1);

/// The state of a live [`HookScope`]: the hook it runs and the first refusal inside it.
#[derive(Debug)]
struct HookScopeState {
    hook: &'static str,
    /// Matches the owning [`HookScope`]'s token while the scope lends this transaction.
    token: u64,
    /// The message of the first refusal, kept even when the hook caught the error.
    refused: Option<String>,
}

/// Transaction-control first keywords refused inside a [`HookScope`].
/// `PREPARE` is in the list only when `TRANSACTION` follows it ([`hook_control_keyword`]).
const HOOK_CONTROL_KEYWORDS: [&str; 9] = [
    "COMMIT",
    "END",
    "ROLLBACK",
    "ABORT",
    "BEGIN",
    "START",
    "SAVEPOINT",
    "RELEASE",
    "PREPARE",
];

/// The transaction-control keyword `sql` starts with (in upper case), if any: its first word
/// outside whitespace and comments, unquoted, in any case. `PREPARE` counts only when the next
/// word is an unquoted `TRANSACTION` (`PREPARE name AS …` is an ordinary prepared statement). A
/// quoted first word is an identifier, never a keyword.
///
/// The scanner's uncertain flag is not consulted. It marks text the scanner cannot read with
/// certainty (a `U&` string, a stray backslash or `$`), which matters to the read-only classifiers
/// that look at every word; this check looks only at the first word, and what the scanner skips
/// before it (whitespace, comments, literals, punctuation) cannot be a keyword. A statement sent
/// by `query` or `execute` is a single statement (the extended protocol refuses more, `42601`), so
/// a control keyword later in the text cannot run.
fn hook_control_keyword(sql: &str) -> Option<&'static str> {
    let mut words = sql_words::SqlWords::new(sql);
    let first = words.next()?;
    if first.quoted {
        return None;
    }
    let keyword = HOOK_CONTROL_KEYWORDS
        .iter()
        .copied()
        .find(|k| first.text.eq_ignore_ascii_case(k))?;
    if keyword == "PREPARE" {
        let next = words.next()?;
        if next.quoted || !next.text.eq_ignore_ascii_case("TRANSACTION") {
            return None;
        }
    }
    Some(keyword)
}

/// A borrowing scope for a hook chain that runs inside a transaction the framework ends, made by
/// [`Transaction::hook_scope`] (and amendment A3).
///
/// While it lives, [`Transaction::commit`], [`Transaction::rollback`],
/// [`Transaction::commit_pipelined`], [`Transaction::switch_orm`] and [`Transaction::batch`] are
/// refused, and so is a raw statement whose first keyword is transaction control (`COMMIT`,
/// `END`, `ROLLBACK`, `ABORT`, `BEGIN`, `START`, `SAVEPOINT`, `RELEASE`, `PREPARE TRANSACTION`),
/// all with `Programming` before anything is sent; the transaction stays as it was. The savepoint
/// API ([`Transaction::savepoint`], [`Transaction::savepoint_outcome`]), record work and raw
/// reads and writes stay available.
///
/// A refusal is sticky: [`HookScope::finish`] fails with the first refusal's message even when
/// the hook caught the error. A nested scope names its own hook; when it is dropped its refusal
/// is carried into the enclosing scope. Dropping the scope restores the caller's state, also
/// during error propagation, unwind or cancellation. This is a consistency guard for trusted
/// native code, not a sandbox.
///
/// Each scope's state carries a token. When the transaction behind the scope was replaced
/// (`std::mem::swap` through `DerefMut`) or a nested scope was forgotten (`std::mem::forget`),
/// the token no longer matches: `finish` fails, and dropping the scope leaves the state in place
/// and poisons the transaction it then lends, so neither transaction commits silently. A
/// top-level scope dropped with a refusal but without `finish` logs it at `error` and poisons the
/// transaction, so a later commit refuses; call `finish`
/// before passing the hook's result up (`let r = run(&mut scope).await; scope.finish()?; r?`).
/// After a token mismatch the stale state keeps refusing `rollback` too: such a transaction can
/// only be dropped, which closes its connection and so rolls it back on the server.
///
/// The holder of a module operation's [`OperationControl`] is outside the guard
/// (`operation_commit` and its siblings); a login transaction is never managed, and
/// [`Transaction::manage_operation`] is refused inside a scope.
#[must_use = "keep the scope alive while running the hook chain, then call finish"]
#[non_exhaustive]
pub struct HookScope<'a> {
    transaction: &'a mut Transaction,
    previous: Option<HookScopeState>,
    hook: &'static str,
    token: u64,
    finished: bool,
}

impl std::ops::Deref for HookScope<'_> {
    type Target = Transaction;

    fn deref(&self) -> &Self::Target {
        self.transaction
    }
}

impl std::ops::DerefMut for HookScope<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.transaction
    }
}

impl HookScope<'_> {
    /// Finish the hook chain.
    ///
    /// # Errors
    /// `Programming` with the first refusal's message when anything was refused inside this
    /// scope or a scope nested in it, even when the hook caught the error and returned `Ok`.
    /// `Programming` too when the transaction behind the scope was replaced or a nested scope was
    /// forgotten (its state is not this scope's).
    pub fn finish(mut self) -> Result<()> {
        self.finished = true;
        match self.transaction.hook_scope.as_ref() {
            Some(state) if state.token == self.token => match state.refused.as_deref() {
                Some(message) => Err(Error::programming(message)),
                None => Ok(()),
            },
            _ => Err(Error::programming(format!(
                "HookScope::finish: the transaction of the {} hook scope was replaced (swapped) or \
                 a nested scope was forgotten while the scope lived (nothing was committed)",
                self.hook
            ))),
        }
    }
}

impl Drop for HookScope<'_> {
    fn drop(&mut self) {
        let matches = self
            .transaction
            .hook_scope
            .as_ref()
            .is_some_and(|state| state.token == self.token);
        if !matches {
            // another transaction is behind the scope, or a nested scope was forgotten: leave
            // the state as it is and keep this transaction from committing
            self.transaction.poison_with(&Error::programming(format!(
                "HookScope: the transaction of the {} hook scope was replaced (swapped) or a \
                 nested scope was forgotten while the scope lived; this transaction must not \
                 commit",
                self.hook
            )));
            return;
        }
        let top_level = self.previous.is_none();
        let inner = std::mem::replace(&mut self.transaction.hook_scope, self.previous.take());
        let Some(HookScopeState {
            refused: Some(message),
            ..
        }) = inner
        else {
            return;
        };
        if let Some(outer) = self.transaction.hook_scope.as_mut() {
            outer.refused.get_or_insert(message);
        } else if top_level && !self.finished && !std::thread::panicking() {
            // a later commit of this transaction fails loudly: the hook ran past a refused commit
            self.transaction.poison_with(&Error::programming(format!(
                "HookScope: the {} hook scope was dropped without finish after a refusal ({message}); \
                 this transaction must not commit",
                self.hook
            )));
            tracing::error!(
                target: "erp::orm",
                hook = self.hook,
                refusal = %message,
                "a hook scope with a refusal was dropped without finish; the refusal may not \
                 have reached its caller (call finish before passing the hook's result up)"
            );
        }
    }
}

impl Transaction {
    /// Admit only the named metadata method chains in this transaction's exact
    /// original environment. Nested scopes keep the outer restrictions.
    ///
    /// ```no_run
    /// # async fn example(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let scope = tx.pure_methods("res.users", &["_get_login_order"]);
    /// // Dispatch a pure chain using the scope; ordinary queries are refused.
    /// scope.finish()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn pure_methods(
        &mut self,
        model: &'static str,
        methods: &'static [&'static str],
    ) -> PureMethods<'_> {
        let previous = self.method_effects.clone();
        if self.pure_method_state().is_none() {
            self.method_effects = Some(Arc::new(MethodEffectState {
                policy: MethodEffectPolicy::Pure { model, methods },
                env: self.env.clone(),
                parent: previous.clone(),
                refused: Mutex::new(None),
            }));
        }
        PureMethods {
            transaction: self,
            previous,
        }
    }

    fn effect_state(
        &self,
        matches: impl Fn(&MethodEffectPolicy) -> bool,
    ) -> Option<&MethodEffectState> {
        let mut current = self.method_effects.as_deref();
        while let Some(state) = current {
            if matches(&state.policy) {
                return Some(state);
            }
            current = state.parent.as_deref();
        }
        None
    }

    fn pure_method_state(&self) -> Option<&MethodEffectState> {
        self.effect_state(|policy| matches!(policy, MethodEffectPolicy::Pure { .. }))
    }

    fn adapter_state(&self) -> Option<&MethodEffectState> {
        self.effect_state(|policy| matches!(policy, MethodEffectPolicy::GeneratedInput { .. }))
    }

    pub(crate) fn in_generated_input(&self) -> bool {
        self.adapter_state().is_some()
    }

    pub(crate) fn generated_input_scope(&mut self, records: &Records) -> Result<PureMethods<'_>> {
        // An adapter never overrides a pure scope or recursively enters another adapter.
        self.refuse_effect()?;
        let previous = self.method_effects.clone();
        self.method_effects = Some(Arc::new(MethodEffectState {
            policy: MethodEffectPolicy::GeneratedInput {
                model: records.model_name(),
            },
            env: records.env().clone(),
            parent: previous.clone(),
            refused: Mutex::new(None),
        }));
        Ok(PureMethods {
            transaction: self,
            previous,
        })
    }

    pub(crate) fn refuse_adapter_effect(&self) -> Result<()> {
        if self.in_generated_input() {
            self.refuse_effect()
        } else {
            Ok(())
        }
    }

    pub(crate) fn refuse_adapter_read(&self, model: ModelId, field: FieldId) -> Result<()> {
        if let Some(state) = self.adapter_state()
            && let MethodEffectPolicy::GeneratedInput { model: adapter } = state.policy
        {
            let model = self.registry().model_by_id(model);
            return Err(state.refuse(format!(
                "Generated input adapter on '{adapter}': read of '{}.{}' requires a flush or recompute",
                model.name, model.field_by_id(field).name
            )));
        }
        Ok(())
    }

    fn refuse_effect(&self) -> Result<()> {
        match &self.method_effects {
            Some(state) => Err(state.refuse(state.message())),
            None => Ok(()),
        }
    }

    fn refuse_pure_effect(&self) -> Result<()> {
        match self.pure_method_state() {
            Some(state) => Err(state.refuse(state.message())),
            None => Ok(()),
        }
    }

    /// [`Transaction::check_method_records`] for an async method call: inside a pure-methods
    /// scope the scope's own check, otherwise [`Transaction::admit_records`] (the switchers of
    /// the records' environment are resolved first, A4).
    ///
    /// # Errors
    ///
    /// As [`Transaction::check_method_records`] and [`Transaction::admit_records`].
    pub(crate) fn admit_method_records<'a>(
        &'a mut self,
        records: &'a Records,
        method: &str,
    ) -> Admit<'a> {
        if self.pure_method_state().is_some() {
            return Admit::Ready(Some(self.check_method_records(records, method)));
        }
        self.admit_records(records)
    }

    pub(crate) fn check_method_records(&self, records: &Records, method: &str) -> Result<()> {
        if let Some(state) = self.pure_method_state() {
            self.check_not_cancelled()?;
            if let MethodEffectPolicy::Pure { model, methods } = state.policy
                && (records.model_name() != model
                    || !methods.contains(&method)
                    || records.env() != &state.env
                    || !records.is_empty()
                    || state.refusal().is_some())
            {
                return Err(state.refuse(state.message()));
            }
            self.check_adapter_environment(records)?;
            return Ok(());
        }
        self.check_records(records)
    }

    /// Restrict record operations for the lifetime of the returned borrowing guard.
    /// Nested scopes retain the outer restriction. Database isolation is unchanged;
    /// READ COMMITTED keeps its existing record-API diagnostic.
    ///
    /// ```no_run
    /// # async fn example(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// {
    ///     let mut raw = tx.raw_statements_only();
    ///     raw.query("SELECT 1", &[]).await?;
    /// }
    /// // The caller's prior record-access rules apply again.
    /// # Ok(())
    /// # }
    /// ```
    pub fn raw_statements_only(&mut self) -> RawStatements<'_> {
        let previous = std::mem::replace(&mut self.raw_statements_only, true);
        RawStatements {
            transaction: self,
            previous,
        }
    }

    /// Run a hook chain inside this transaction, which the framework ends after the hook, for the
    /// lifetime of the returned borrowing scope. Calls that end the
    /// transaction and raw transaction-control SQL are refused inside it ([`HookScope`]); call
    /// [`HookScope::finish`] after the chain, so a refusal the hook caught still fails it.
    ///
    /// `hook` names the hook in the refusal messages. A nested scope names its own hook.
    ///
    /// ```no_run
    /// # async fn example(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let mut scope = tx.hook_scope("res.users._post_login");
    /// // `scope.commit()` or `scope.query("COMMIT", &[])` would be refused here.
    /// let ran = scope.query("SELECT 1", &[]).await;
    /// // finish first, so a refusal the hook caught is not lost, then pass the result up
    /// scope.finish()?;
    /// ran?;
    /// // The caller ends the transaction.
    /// tx.commit().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn hook_scope(&mut self, hook: &'static str) -> HookScope<'_> {
        let token = NEXT_HOOK_SCOPE_TOKEN.fetch_add(1, Ordering::Relaxed);
        let previous = self.hook_scope.replace(HookScopeState {
            hook,
            token,
            refused: None,
        });
        HookScope {
            transaction: self,
            previous,
            hook,
            token,
            finished: false,
        }
    }

    /// Refuse `call`, an entry point that ends the transaction, inside a [`HookScope`], and
    /// record the refusal so [`HookScope::finish`] fails.
    fn refuse_in_hook_scope(&mut self, call: &'static str) -> Result<()> {
        let Some(state) = self.hook_scope.as_mut() else {
            return Ok(());
        };
        let message = format!(
            "{call}: refused inside the {} hook; the framework ends this transaction after the \
             hook (nothing was committed or rolled back)",
            state.hook
        );
        state.refused.get_or_insert_with(|| message.clone());
        Err(Error::programming(message))
    }

    /// Refuse raw transaction-control SQL sent through `call` inside a [`HookScope`], and record
    /// the refusal so [`HookScope::finish`] fails.
    fn refuse_hook_control_sql(&mut self, call: &'static str, sql: &str) -> Result<()> {
        let Some(state) = self.hook_scope.as_mut() else {
            return Ok(());
        };
        let Some(keyword) = hook_control_keyword(sql) else {
            return Ok(());
        };
        let message = format!(
            "{call}: transaction control ({keyword}) is refused inside the {} hook; the framework \
             ends this transaction after the hook (nothing was sent)",
            state.hook
        );
        state.refused.get_or_insert_with(|| message.clone());
        Err(Error::programming(message))
    }

    /// Refuse scoped record work, including entry points without a recordset (flush).
    pub(crate) fn require_record_scope(&self) -> Result<()> {
        self.refuse_pure_effect()?;
        if self.raw_statements_only {
            return Err(Error::programming(
                "the record API is unavailable in a raw-statements-only scope (query, execute, batch)",
            ));
        }
        Ok(())
    }
    /// Open savepoints.
    pub(crate) fn savepoint_depth(&self) -> usize {
        self.savepoints.len()
    }

    /// Records `id` in this transaction's id set for the marker type `K`, at the current
    /// savepoint depth.
    ///
    /// `K` is a marker type that the owning crate keeps private: the set is keyed by `K`'s
    /// `TypeId`, so no other crate can name it, read it or add to it. An entry made inside a
    /// savepoint is forgotten when that savepoint rolls back and kept when it is released; every
    /// set is cleared when the transaction ends. Nothing is written to the database.
    ///
    /// The `ir.ui.view` root tracking (`addons/base`, `ir_ui_view/tree.rs`) relies on this for the
    /// A3-D1 conflict-write guard: a root that is in its set is not written again by later view
    /// checks, so its marker type must stay private to that module.
    ///
    /// `K` must be a private type of the calling crate, never a public or foreign type (`()`,
    /// `i64`, `String`, …): two crates that use the same `K` share one set.
    ///
    /// ```rust,no_run
    /// use erp_orm::Transaction;
    ///
    /// // private to the module that owns the set
    /// struct ImportedKey;
    ///
    /// fn note_import(tx: &mut Transaction, id: i64) -> Vec<i64> {
    ///     tx.mark_scoped_id::<ImportedKey>(id);
    ///     tx.scoped_ids::<ImportedKey>()
    /// }
    /// ```
    #[inline]
    pub fn mark_scoped_id<K: 'static>(&mut self, id: i64) {
        if self.refuse_effect().is_err() {
            return;
        }
        let depth = self.savepoint_depth();
        self.scoped_ids
            .entry(std::any::TypeId::of::<K>())
            .or_default()
            .touch(id, depth);
    }

    /// The ids recorded with [`Transaction::mark_scoped_id`] for the marker type `K`, sorted;
    /// empty when none.
    ///
    /// Ids recorded inside a savepoint that rolled back are not included; ids recorded inside a
    /// released savepoint are. The set is cleared when the transaction ends. The `ir.ui.view`
    /// root tracking (`addons/base`, `ir_ui_view/tree.rs`) reads its set through this for the
    /// A3-D1 conflict-write guard, so the key type must stay private to the owning crate.
    ///
    /// `K` must be a private type of the calling crate, never a public or foreign type (`()`,
    /// `i64`, `String`, …): two crates that use the same `K` share one set.
    ///
    /// ```rust,no_run
    /// use erp_orm::Transaction;
    ///
    /// struct SeenKey;
    ///
    /// fn already_seen(tx: &Transaction, id: i64) -> bool {
    ///     tx.scoped_ids::<SeenKey>().binary_search(&id).is_ok()
    /// }
    /// ```
    #[inline]
    #[must_use]
    pub fn scoped_ids<K: 'static>(&self) -> Vec<i64> {
        self.scoped_ids
            .get(&std::any::TypeId::of::<K>())
            .map(ScopedIds::touched_ids)
            .unwrap_or_default()
    }

    /// Inside an isolated scope (onchange).
    pub(crate) fn in_isolated_scope(&self) -> bool {
        !self.isolated.is_empty()
    }

    /// The transaction can no longer run statements until a rollback (aborted) or at all
    /// (released): a stream whose first flight failed then forgets its cursor instead of closing
    /// it.
    pub(crate) fn is_aborted_or_released(&self) -> bool {
        matches!(self.state, State::Aborted { .. } | State::Released)
    }

    /// Number of cache slots of `model` (records with a slot, cached or invalidated): the window
    /// scope test of `Records::stream`. Only with the `test-faults` feature.
    ///
    /// # Errors
    ///
    /// `Programming` for an unknown model.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn cached_slots_for_tests(&self, model: &str) -> Result<usize> {
        let id = self
            .registry()
            .model_id(model)
            .ok_or_else(|| Error::programming(format!("unknown model '{model}'")))?;
        Ok(self.cache.slots(id))
    }

    /// The per-record record-rule decisions remembered now ([`crate::engine::ReadableMemo`]),
    /// over every environment and model. Only with the `test-faults` feature.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn readable_decisions_for_tests(&self) -> usize {
        self.engine.readable.decision_count()
    }

    /// The write tick of the latest change that may have touched every model (a statement whose
    /// target is unknown, `write_gens`): it grows when a raw statement is unknown to the
    /// classifier. Only with the `test-faults` feature.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn every_model_tick_for_tests(&self) -> u64 {
        self.write_gens.any_tick()
    }

    /// The record ids the memo of access checks holds now (`AccessState`), over every entry. Only
    /// with the `test-faults` feature.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn access_memo_ids_for_tests(&self) -> usize {
        self.access.memo_len()
    }

    /// Run `work` in an isolated scope (onchange's, [`Transaction::isolated`]). Only with the
    /// `test-faults` feature.
    ///
    /// # Errors
    ///
    /// As [`Transaction::isolated`].
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub async fn isolated_for_tests<T, F>(&mut self, work: F) -> Result<T>
    where
        F: AsyncFnOnce(&mut Transaction) -> Result<T>,
    {
        self.isolated(work).await
    }

    /// Allow `limit` SQL cursors open at once on this transaction object ([`Records::stream`];
    /// 32 by default). Each open cursor holds a server portal and, for a sorted query, up to
    /// `work_mem` of executor state until its stream ends or the transaction does.
    ///
    /// # Errors
    ///
    /// `Programming` for 0.
    pub fn set_cursor_limit(&mut self, limit: usize) -> Result<()> {
        self.refuse_effect()?;
        self.cursors.set_limit(limit)
    }

    /// The environment the transaction was opened with.
    pub fn env(&self) -> Env {
        self.env.clone()
    }

    /// Check `lang` against the languages active in `res.lang` as this transaction sees them
    /// : `None`, empty and `en_US` pass; any other code must be active.
    ///
    /// # Errors
    /// A user error `Invalid language code: <lang>`, or a database error reading `res.lang`.
    pub async fn validate_lang_code(&mut self, lang: &str) -> Result<()> {
        let active = self.active_languages().await?;
        crate::lang::validate_lang(Some(lang), active.codes())
    }

    /// Active language state of this transaction.
    pub(crate) async fn active_languages(&mut self) -> Result<Arc<crate::lang::ActiveLanguages>> {
        if let Some(cached) = &self.active_langs {
            return Ok(Arc::clone(cached));
        }
        let active = if self.shared.registry.model("res.lang").is_some() {
            if self.langs_dirty {
                // Pending `res.lang` changes must be visible to the SELECT below. The flag is
                // cleared first because the flush path can reach this function again (boxed), and
                // restored if the flush fails so the memo is not built from stale rows.
                self.langs_dirty = false;
                if let Err(e) = Box::pin(self.flush()).await {
                    self.langs_dirty = true;
                    return Err(e);
                }
            }
            let rows = self.query("SELECT code, active FROM res_lang", &[]).await?;
            let mut active_codes = rustc_hash::FxHashSet::default();
            for row in &rows {
                let code: String = row
                    .try_get(0)
                    .map_err(|e| Error::db(format!("res_lang.code: {e}")))?;
                let is_active: bool = row
                    .try_get(1)
                    .map_err(|e| Error::db(format!("res_lang.active: {e}")))?;
                if is_active {
                    active_codes.insert(code);
                }
            }
            crate::lang::ActiveLanguages::new(active_codes)
        } else {
            let mut active_codes = rustc_hash::FxHashSet::default();
            active_codes.insert("en_US".to_string());
            crate::lang::ActiveLanguages::new(active_codes)
        };
        let arc = Arc::new(active);
        self.active_langs = Some(Arc::clone(&arc));
        Ok(arc)
    }

    /// Whether `BEGIN` was not sent yet.
    pub(crate) fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    /// A new environment on this transaction's registry and lineage for `principal` with
    /// `context` (the companies it names are validated as `Env` does); not the transaction's own.
    pub(crate) fn env_for(&self, principal: Principal, context: Context) -> Env {
        Env::new(Arc::clone(&self.shared.registry), principal, context)
            .with_lineage(Arc::clone(&self.shared.database.lineage))
            .with_generation_attachments(Arc::clone(&self.shared.generation_attachments))
    }

    /// The transaction's environment with `principal` (the resolver's at the start).
    pub(crate) fn replace_principal(&mut self, principal: Principal) {
        let identity = self.env.operation_identity().cloned().map(Arc::new);
        self.env = Env::new(
            Arc::clone(&self.shared.registry),
            principal,
            self.env.context().clone(),
        )
        .with_lineage(Arc::clone(&self.shared.database.lineage))
        .with_operation_identity(identity)
        .with_generation_attachments(Arc::clone(&self.shared.generation_attachments));
    }

    /// After a rollback to a savepoint (or the end of an isolated scope): a private snapshot or
    /// resolutions read since may rest on undone writes, so the next engine call rebuilds and
    /// re-resolves (one extra rebuild, never a stale decision; design note §3.4).
    fn policy_after_rollback(&mut self) {
        if !self.policy.snapshot().is_shared() {
            self.refresh.policy_dirty = true;
        }
        if !self.refresh.resolved.is_empty() {
            self.refresh.principals_dirty = true;
        }
    }

    /// The pool this transaction runs on. A read transaction opened with a read pool configured
    /// is [`Route::ReadPool`] until the lag guard or the unit of work sends it to the primary;
    /// a write is always [`Route::Primary`].
    pub fn route(&self) -> crate::route::Route {
        self.route
    }

    /// Move a routed transaction to the primary: end it on the read pool (rolling back when it
    /// began), take a primary connection and start over there. The reason is counted once the new
    /// `BEGIN` succeeded. `connection_dead`: the read connection failed and goes nowhere near the
    /// pool again.
    ///
    /// Cancel-safe: everything that can be dropped mid-way happens after the transaction left the
    /// read pool's state. A dropped future leaves the transaction `Released` (never `Open` on the
    /// replica), and the read connection, if it had begun, is detached by its guard.
    pub(crate) async fn reroute_to_primary(
        &mut self,
        reason: crate::route::Reason,
        connection_dead: bool,
    ) -> Result<()> {
        let Some(primary) = self.primary.clone() else {
            return Err(Error::programming(
                "a transaction on the primary cannot be rerouted to the primary",
            ));
        };
        let begun = matches!(
            self.state,
            State::Open | State::Beginning | State::Aborted { .. }
        );
        // a connection whose session state changed (or whose string reading the server reported
        // changed) never goes back to the read pool
        let tainted = self.connection_tainted || self.string_reading_moved();
        // before the first await: nothing is left on the read pool's side of this transaction
        let mut old = ReadConn {
            conn: self.conn.take(),
            in_transaction: begun || connection_dead || tainted,
        };
        self.state = State::Released;
        self.signal_state.reset();
        self.outbox.clear();
        // the audit buffer is kept: a reroute happens only while a database transaction opens
        // (`start`), before any statement of it completed, so what was recorded since (an
        // elevation entered before the first statement) still describes this transaction, which
        // goes on, on the primary; evidence of an earlier database transaction that the caller
        // ended itself is already in the stash and stays there
        // the database transaction ended: none of its blocks is open any more, and the next
        // block takes a new serial (closing is the safe direction on a poisoned set)
        self.blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reset();
        self.pending_capture.clear();
        self.scoped_ids.clear();
        self.tracking.clear();
        #[cfg(feature = "test-faults")]
        if self.break_before_reroute
            && let Some(conn) = old.conn.as_ref()
        {
            // the answer is the closed connection: the point is that the ROLLBACK below fails
            // (a read-only role may not terminate the session of the role it was switched from)
            drop(
                conn.batch_execute("SET ROLE NONE; SELECT pg_terminate_backend(pg_backend_pid())")
                    .await,
            );
        }
        if begun && !connection_dead {
            let rolled_back = match old.conn.as_ref() {
                Some(conn) => tokio::time::timeout(
                    crate::route::REROUTE_ROLLBACK_TIMEOUT,
                    conn.batch_execute("ROLLBACK"),
                )
                .await
                .map_err(|_| "ROLLBACK did not finish in time".to_owned())
                .and_then(|result| result.map_err(|e| e.to_string())),
                None => Err("the read connection is gone".to_owned()),
            };
            match rolled_back {
                // a connection that rolled back cleanly goes back to the read pool
                Ok(()) => old.in_transaction = tainted,
                // detached by the guard: the connection is broken or hung
                Err(why) => self.shared.routing.rollback_failed(&why, &reason),
            }
        }
        drop(old);
        let conn = match primary.get().await {
            Ok(conn) => conn,
            Err(e) => {
                self.shared.routing.reroute_failed();
                return Err(checkout_error(e));
            }
        };
        self.shared.refresh_statements(&conn);
        self.conn = Some(conn);
        self.route = crate::route::Route::Primary;
        self.primary = None;
        self.state = State::Idle;
        if self.fallback.is_none() {
            self.fallback = Some(reason);
        }
        Ok(())
    }

    /// Count what the finished `BEGIN` decided: a fallback to the primary, or a read served by
    /// the read pool (and whether it ran outside any unit of work).
    pub(crate) fn count_begin(&mut self) {
        if let Some(reason) = self.fallback.take() {
            self.shared.routing.fell_back(&reason);
        } else if self.route == crate::route::Route::ReadPool {
            self.shared.routing.served();
            if self.unit.is_none() {
                self.shared.routing.unscoped();
            }
        }
    }

    /// The read connection died between checkout and `BEGIN` (replica restart, failover): follow
    /// [`crate::ReadPoolFailure`] like an unreachable pool.
    pub(crate) async fn read_pool_died(
        &mut self,
        resolver: Option<&crate::access::ResolverStatement>,
        error: tokio_postgres::Error,
    ) -> Result<Vec<Row>> {
        let cause = error.to_string();
        let on_failure = self
            .shared
            .read
            .as_ref()
            .map(|read| read.on_failure)
            .unwrap_or_default();
        match on_failure {
            crate::route::ReadPoolFailure::Primary => {
                self.reroute_to_primary(crate::route::Reason::ReadPoolUnavailable(cause), true)
                    .await?;
                Box::pin(self.start(resolver)).await
            }
            crate::route::ReadPoolFailure::Fail => {
                self.shared.routing.error(&cause);
                self.release();
                Err(crate::route::unavailable_with(
                    &cause,
                    Some(
                        Error::new(
                            ErrorKind::Database,
                            format!("the read connection failed: {error}"),
                        )
                        .with_source(error),
                    ),
                ))
            }
        }
    }

    /// What the transaction was opened for ([`TxIntent::Write`] unless
    /// [`BeginOptions::read_only`]).
    pub fn intent(&self) -> &TxIntent {
        &self.intent
    }

    /// The read-audit owner declared when the transaction was opened
    /// ([`BeginOptions::read_only_audited`]); `None` for a write transaction and for a read-only
    /// one opened without an owner.
    pub fn read_audit_owner(&self) -> Option<&ReadAuditOwner> {
        self.read_audit_owner.as_ref()
    }

    /// Refuse `op` on `model` in a read-only transaction (design note §5.3).
    ///
    /// # Errors
    ///
    /// `Programming` "'<entry>' is declared effect = read but tried to <op> '<model>'; declare
    /// effect = write (or wider)." when the intent is [`TxIntent::Read`].
    pub fn require_writable(&self, op: WriteOp, model: &str) -> Result<()> {
        self.refuse_adapter_effect()?;
        match &self.intent {
            TxIntent::Write => Ok(()),
            TxIntent::Read { entry } => Err(Error::programming(format!(
                "'{entry}' is declared effect = read but tried to {} '{model}'; declare effect = \
                 write (or wider).",
                op.as_str()
            ))),
        }
    }

    /// The isolation level this transaction was opened with.
    pub(crate) fn isolation_level(&self) -> Isolation {
        self.isolation
    }

    /// The record API (record reads and writes through the cache, searches, method calls, the
    /// access memo) needs one snapshot for the whole transaction: in a transaction opened with
    /// [`Isolation::ReadCommitted`] every path that checks its records here is refused with a
    /// `Programming` error before it sends anything; raw `query`, `execute` and `batch` stay
    /// allowed (round 1 review, lead ruling a).
    fn require_record_api(&self, records: &Records) -> Result<()> {
        match self.isolation {
            Isolation::RepeatableRead => self.require_record_scope(),
            Isolation::ReadCommitted => Err(Error::programming(format!(
                "{records}: the record API needs a REPEATABLE READ transaction; this one was opened \
                 with BeginOptions::isolation(ReadCommitted), which allows raw statements only \
                 (query, execute, batch)"
            ))),
        }
    }

    /// The guard for raw statements: in a read-only transaction a statement that may change
    /// something (the same allow-list as the isolation of onchange scopes) is refused before it
    /// is sent, and so is a call of a function whose effect outlives it
    /// ([`SESSION_STATE_FUNCTIONS`]).
    ///
    /// In a read-intent transaction a statement the word scanner does not read with certainty is
    /// refused with its own error (round 4, fail closed), and so is every raw statement once the
    /// server reported a new string reading on the connection ([`Transaction::note_string_reading`],
    /// round 5). In a write transaction this only notes such a report: the statement and every
    /// later one are then unknown to the classifiers.
    ///
    /// One pass of the word scanner on the way in (`needs_isolation`, which holds for anything
    /// uncertain), a second for the session-state functions; whether the scanner was certain is
    /// asked only to word the error.
    ///
    /// Inside a [`HookScope`] a statement whose first keyword is transaction control is refused
    /// first, whatever the intent; `call` is the public entry point named in that refusal.
    fn require_writable_sql(&mut self, call: &'static str, sql: &str) -> Result<()> {
        self.refuse_pure_effect()?;
        if self.in_generated_input() && self.require_select_only(sql).is_err() {
            return self.refuse_adapter_effect();
        }
        self.note_string_reading();
        if self.hook_scope.is_some() {
            self.refuse_hook_control_sql(call, sql)?;
        }
        let TxIntent::Read { entry } = &self.intent else {
            return Ok(());
        };
        if self.strings_unsure {
            return Err(Error::programming(format!(
                "'{entry}' is declared effect = read but the server reported a new \
                 standard_conforming_strings or client_encoding on this connection, so the ORM \
                 can no longer read raw SQL with certainty; declare effect = write (or wider)."
            )));
        }
        if needs_isolation(sql) {
            return Err(if sql_words::certain(sql) {
                read_only_sql_error(entry)
            } else {
                Error::programming(format!(
                    "'{entry}' is declared effect = read but sent SQL the ORM cannot read with \
                     certainty (a U& string or name, a backslash outside an E'' string, a stray \
                     $, or unterminated text), so it cannot tell it changes nothing; declare \
                     effect = write (or wider), or write it without those."
                ))
            });
        }
        match session_state_function(sql) {
            Some(function) => Err(Error::programming(format!(
                "'{entry}' is declared effect = read but called {function}(), which a read-only \
                 transaction allows but whose effect outlives the call; declare effect = write \
                 (or wider)."
            ))),
            None => Ok(()),
        }
    }

    /// Refuse SQL that is not certainly a read: the guard of the read-only view a candidate
    /// compute receives ([`crate::compute::ReadTx::query`]). The same lexical judgement as a
    /// transaction declared `effect = read` makes about its raw SQL, applied whatever the
    /// transaction's own intent is.
    pub(crate) fn require_select_only(&mut self, sql: &str) -> Result<()> {
        self.note_string_reading();
        let refusal = |why: &str| {
            Error::programming(format!(
                "a candidate compute may only run SELECT statements through its read-only view: {why}"
            ))
        };
        if self.strings_unsure {
            return Err(refusal(
                "the server reported a new standard_conforming_strings or client_encoding on this \
                 connection, so raw SQL can no longer be read with certainty",
            ));
        }
        if needs_isolation(sql) {
            return Err(refusal(
                "the statement is not a plain SELECT/WITH/SHOW/EXPLAIN, changes data, or cannot be \
                 read with certainty",
            ));
        }
        if let Some(function) = session_state_function(sql) {
            return Err(refusal(&format!(
                "{function}() has an effect that outlives the call"
            )));
        }
        // Sequence, sleep and large-object functions pass the read-only judgement above yet
        // change state or hold the connection. Only a call counts (the word is followed by `(`),
        // so a column or alias such as `lo_limit` or `"lo_x"` stays usable.
        for word in sql_words::SqlWords::new(sql) {
            let lower = word.text.to_ascii_lowercase();
            let called = sql.get(word.end..).is_some_and(next_is_open_paren);
            if called
                && (matches!(lower.as_str(), "nextval" | "setval" | "pg_sleep")
                    || lower.starts_with("lo_"))
            {
                return Err(refusal(&format!(
                    "{lower}() changes sequence or large-object state or waits, which a read-only view does not allow"
                )));
            }
        }
        Ok(())
    }

    /// Whether the server ever reported a new `standard_conforming_strings` or `client_encoding`
    /// on the connection since it connected (the driver counts the reports,
    /// `Client::string_reading_changes`; review round 5, M3). The count starts at 0 with the
    /// values the pool checked at connect ([`crate::pool::pg_version_hook`]), so a connection a
    /// user of the pool outside a transaction object changed and gave back (schema steps,
    /// partition upkeep, the signal listener) is not trusted either (review round 6, L2).
    fn string_reading_moved(&self) -> bool {
        self.conn
            .as_ref()
            .is_some_and(|conn| conn.string_reading_changes() != 0)
    }

    /// After the server reported a new string reading on the connection (now or before this
    /// transaction object got it), every later raw statement of this transaction object is
    /// unknown (the word scanner reads statements as a connection with the
    /// settings it was opened with reads them, which the pool checks,
    /// [`crate::pool::pg_version_hook`]) and the connection is closed instead of going back to
    /// the pool. Whatever caused the report is seen: a `SET`, `set_config`, a function, dynamic
    /// SQL in a `DO` block. The statement that caused it was read by the server as it was by the
    /// scanner, before the change.
    ///
    /// Called before every raw statement is read ([`Transaction::require_writable_sql`]): the
    /// server sends its reports before the `ReadyForQuery` that ends a statement, and each driver
    /// call returns only after it, so the count already holds the reports of every statement
    /// before. A flight's statements are all read before it is sent, and a per-call statement of
    /// it is parsed only after the one before it completed, so the count is read again when the
    /// flight returns and a report made during it makes the flight's effects unknown
    /// ([`Transaction::flight_with_closes`], review round 6, L1). The ORM's own statements carry
    /// their values as parameters and do not depend on it. When the connection leaves the
    /// transaction object (`Drop`, a reroute to the primary) the same count decides whether it
    /// may go back to its pool. Returns whether this call found the change.
    fn note_string_reading(&mut self) -> bool {
        if !self.strings_unsure && self.string_reading_moved() {
            self.strings_unsure = true;
            self.connection_tainted = true;
            return true;
        }
        false
    }

    /// What a statement unknown to the classifiers does, for statements already counted: the
    /// remembered record-rule decisions end and every model counts as written (a flight during
    /// which the string reading changed, [`Transaction::flight_with_closes`]).
    fn flight_effects_unknown(&mut self) {
        self.engine.readable.clear();
        self.write_gens.all_written();
    }

    /// Send `BEGIN` now, with the generation read when the ORM has a [`crate::Signal`] (not
    /// counted in [`Transaction::query_count`]), instead of with the first statement. A request
    /// that must know its registry is current before it answers (RPC's 404 for an unknown model,
    /// ) calls this.
    ///
    /// # Errors
    ///
    /// As the first statement would: a superseded ORM ([`Error::is_superseded`]), a failed
    /// refresh, a lost connection.
    pub async fn open(&mut self) -> Result<()> {
        self.ensure_open().await
    }

    /// Run a statement as an unnamed statement with typed parameters: planned for these values
    /// in the same round trip, nothing cached on the connection (statements whose parameters
    /// are cast in SQL, such as `$1::uuid` from text).
    pub async fn query_typed(
        &mut self,
        sql: &str,
        params: &[(&(dyn ToSql + Sync), tokio_postgres::types::Type)],
    ) -> Result<Vec<Row>> {
        self.query_per_call(sql, params).await
    }

    /// The text of the last statement the server refused in this transaction (a template with
    /// `$n` placeholders: values are never part of it), for diagnostics.
    pub fn failed_statement(&self) -> Option<&str> {
        self.failed_sql.as_deref()
    }

    /// The generations of the cross-process signals this transaction's snapshot read at
    /// `BEGIN`; `None` before the first statement or without a [`crate::Signal`].
    pub fn began_generations(&self) -> Option<&crate::signal::Generations> {
        self.signal_state.began_at()
    }

    /// The registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.shared.registry
    }

    /// Process-unique id of this transaction's database lineage (the key of the resolved guard
    /// cache; every [`Orm::rebuild`] of one [`Orm::builder`] shares it).
    pub(crate) fn database_id(&self) -> u64 {
        self.shared.database.orm_id
    }

    pub(crate) fn dispatch(&self) -> &Dispatch {
        &self.shared.dispatch
    }

    /// The state of type `T` that a layer above the ORM keeps for the life of this transaction
    /// (created with `T::default()` on first use, dropped with the transaction, so it never
    /// outlives a commit or rollback). One slot per type. The WASM host keeps its per-transaction
    /// instance pool here; `T` should hand out shared handles (`Arc`) rather than
    /// borrows, because the caller also needs `&mut self` for ORM work.
    pub fn extension<T: std::any::Any + Send + Sync + Default>(&mut self) -> &mut T {
        // The legacy API returns a borrow rather than Result. A pure callback gets
        // isolated scratch, never the real extension state; finish fails loudly.
        let extensions = if self.refuse_effect().is_err() {
            &mut self.effect_scratch
        } else {
            &mut self.extensions
        };
        let slot = extensions
            .entry(std::any::TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()));
        // The slot of `TypeId::of::<T>()` only ever holds a `T`, so the downcast cannot fail; a
        // failure would be a bug in this function, reported as a panic rather than a wrong value.
        match slot.downcast_mut::<T>() {
            Some(state) => state,
            None => unreachable!("extension slot holds the type it is keyed by"),
        }
    }

    pub(crate) fn config(&self) -> &OrmConfig {
        &self.shared.config
    }

    /// [`Transaction::check_records`] after resolving the switching principals of the pending
    /// `with_user` switches of the records' environment ([`Transaction::prepare_switch`]), so the
    /// admission check refuses a switcher without `principal.impersonate` with its `Access` error
    /// instead of an unresolved-principal one (A4). Every async entry that takes a
    /// recordset admits it this way; a synchronous one relies on the entry that called it.
    ///
    /// The future is a named type, not an `async fn` body: without a switch it is ready at once,
    /// and with one the resolution (which may refresh the policy, flush and read records again
    /// through this function) runs as an erased future, so neither the cycle nor an extra level
    /// of nesting enters the callers' future types.
    ///
    /// # Errors
    ///
    /// As [`Transaction::prepare_switch`] and [`Transaction::check_records`].
    pub(crate) fn admit_records<'a>(&'a mut self, records: &'a Records) -> Admit<'a> {
        if records.env().switches().is_empty() {
            return Admit::Ready(Some(self.check_records(records)));
        }
        let env = records.env();
        Admit::Pending(Box::pin(async move {
            self.prepare_switch(env).await?;
            self.check_records(records)
        }))
    }

    /// Records must come from this transaction's registry: a recordset built on another registry
    /// would resolve its model and field ids against the wrong metadata.
    pub(crate) fn check_records(&self, records: &Records) -> Result<()> {
        self.check_not_cancelled()?;
        self.check_adapter_environment(records)?;
        // an environment of an elevation block is admitted only while its block is open here,
        // before anything reads the cache
        self.check_env_block(records.env())?;
        self.require_record_api(records)?;
        if Arc::ptr_eq(records.env().registry(), &self.shared.registry) {
            crate::event_namespace::refuse_event_namespace(records.spec(), "every record operation")
        } else {
            Err(Error::programming(format!(
                "{records} belongs to another registry than this transaction's"
            )))
        }
    }

    fn check_adapter_environment(&self, records: &Records) -> Result<()> {
        if let Some(state) = self.adapter_state()
            && (records.env().principal() != state.env.principal()
                || records.env().companies()? != state.env.companies()?)
        {
            return Err(state.refuse(state.message()));
        }
        Ok(())
    }

    /// A [`Transaction::batch`] counts as one statement however many
    /// it contains.
    pub fn query_count(&self) -> u64 {
        self.queries
    }

    /// Number of `default_get` method dispatches executed in this transaction.
    pub fn default_get_count(&self) -> u64 {
        self.default_gets
    }

    /// Reset the `default_get` dispatch counter and return the count before reset.
    pub fn reset_default_get_count(&mut self) -> u64 {
        if self.refuse_effect().is_err() {
            return self.default_gets;
        }
        let prev = self.default_gets;
        self.default_gets = 0;
        prev
    }

    pub(crate) fn inc_default_get_count(&mut self) {
        self.default_gets += 1;
    }

    /// Lowers the number of distinct ids one statement of the company check carries (default
    /// [`crate::COMPANY_CHECK_CHUNK`]), so a test can run the chunked path with a few rows. A
    /// transaction setting, never a context key an RPC caller could send. Only with the
    /// `test-faults` feature.
    ///
    /// # Errors
    ///
    /// `Programming` for zero.
    #[cfg(feature = "test-faults")]
    pub fn set_company_check_chunk_for_tests(&mut self, ids: usize) -> Result<()> {
        if ids == 0 {
            return Err(Error::programming(
                "the company check chunk must be a positive number of ids",
            ));
        }
        self.company_chunk = Some(ids);
        Ok(())
    }

    /// Start or stop recording statement texts.
    pub fn capture_statements(&mut self, on: bool) {
        if self.refuse_effect().is_err() {
            return;
        }
        self.capture = on.then(Vec::new);
    }

    /// Recorded statement texts (since `capture_statements(true)`).
    pub fn captured(&self) -> &[String] {
        self.capture.as_deref().unwrap_or_default()
    }

    /// Post a diagnostic notice or warning to the transaction's notice channel.
    ///
    /// The channel keeps at most [`MAX_NOTICES_PER_TRANSACTION`] notices. A notice posted past
    /// that is not stored: it is counted ([`Transaction::notices_overflowed`]) and the first one
    /// of the attempt is logged as a warning.
    ///
    /// Notices belong to one transaction attempt. Commit, rollback and `reset()` clear them; a
    /// rollback to a savepoint drops the notices posted inside it, a release keeps them. With
    /// `Orm::run`, call [`take_notices`](Self::take_notices) inside the closure, before the
    /// commit; a retried attempt starts empty.
    pub fn post_notice(&mut self, notice: Notice) {
        if self.refuse_effect().is_err() {
            return;
        }
        if self.notices.len() >= MAX_NOTICES_PER_TRANSACTION {
            if self.notices_overflowed == 0 {
                tracing::warn!(
                    target: "erp::notices",
                    limit = MAX_NOTICES_PER_TRANSACTION,
                    "the transaction notice channel is full: further notices are counted, not kept"
                );
            }
            self.notices_overflowed = self.notices_overflowed.saturating_add(1);
            return;
        }
        self.notices.push(notice);
    }

    /// Diagnostic notices posted on this transaction attempt so far (at most
    /// [`MAX_NOTICES_PER_TRANSACTION`]; see [`notices_overflowed`](Self::notices_overflowed)).
    ///
    /// They are cleared by commit, rollback and `reset()`: with `Orm::run`, read them (or
    /// [`take_notices`](Self::take_notices)) inside the closure, before the commit.
    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }

    /// How many notices were refused since the channel was last emptied because it already held
    /// [`MAX_NOTICES_PER_TRANSACTION`]. A rollback to a savepoint does not decrease it. Reset
    /// by commit, rollback, `reset()`, [`take_notices`](Self::take_notices) and
    /// [`clear_notices`](Self::clear_notices).
    pub fn notices_overflowed(&self) -> u64 {
        self.notices_overflowed
    }

    /// Remove and return all notices posted on this transaction attempt so far, and reset the
    /// overflow counter ([`notices_overflowed`](Self::notices_overflowed); read it first).
    ///
    /// With `Orm::run`, call this inside the closure, before the commit, which clears the
    /// notices; a retried attempt starts empty.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        if self.refuse_effect().is_err() {
            return Vec::new();
        }
        self.savepoint_notices.iter_mut().for_each(|n| *n = 0);
        self.notices_overflowed = 0;
        std::mem::take(&mut self.notices)
    }

    /// Clear all notices posted on this transaction attempt and reset the overflow counter.
    /// Commit, rollback and `reset()` do the same.
    pub fn clear_notices(&mut self) {
        if self.refuse_effect().is_err() {
            return;
        }
        self.savepoint_notices.iter_mut().for_each(|n| *n = 0);
        self.notices_overflowed = 0;
        self.notices.clear();
    }

    /// The transaction timestamp, once a statement returned it.
    pub fn now(&self) -> Option<DateTime> {
        self.now
    }

    /// Run `hook` after this transaction commits successfully; dropped on rollback.
    pub fn after_commit(&mut self, hook: impl FnOnce() + Send + 'static) {
        if self.refuse_effect().is_err() {
            return;
        }
        self.after_commit.push(Box::new(hook));
    }

    /// True when the transaction can no longer be used (its connection was released).
    pub fn is_released(&self) -> bool {
        self.state == State::Released
    }

    /// Inspect native lifecycle source lookup on this held connection before its lazy BEGIN.
    /// This performs metadata reads only, issues no explicit table locks, and grants no lifecycle
    /// authority. Configured signals are validated before lazy BEGIN to ensure exact native storage
    /// types, absence of rewrite rules/triggers, and native built-in aggregate resolution before
    /// automatic generation reads. No signal read is suppressed, and the idle connection remains
    /// usable after a refusal. The caller still performs its in-transaction schema, authority
    /// and effect admission.
    ///
    /// # Errors
    /// Access without the fully elevated System principal; Programming unless still idle;
    /// Validation for catalog precedence, missing/temporary/nonordinary signal storage (including
    /// row-level security, partitions and inheritance, which run or reach user code on read), or
    /// non-native storage types/overloaded aggregates;
    /// Database when the metadata query fails. No second pool connection is acquired.
    #[doc(hidden)]
    pub async fn preflight_native_catalog_sources(&mut self) -> Result<()> {
        self.refuse_effect()?;
        self.require_system_full("Transaction::preflight_native_catalog_sources")?;
        if self.state != State::Idle {
            return Err(Error::programming(
                "Native lifecycle source preflight requires an idle transaction",
            ));
        }
        let row = self.conn()?.query_one(
            "SELECT pg_catalog.current_schemas(true), EXISTS(SELECT 1 FROM pg_catalog.pg_class c WHERE c.oid OPERATOR(pg_catalog.=) pg_catalog.to_regclass('erp_signal_generation') AND c.relpersistence::pg_catalog.text OPERATOR(pg_catalog.=) 'p' AND c.relkind::pg_catalog.text OPERATOR(pg_catalog.=) 'r' AND NOT c.relispartition AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhrelid OPERATOR(pg_catalog.=) c.oid OR i.inhparent OPERATOR(pg_catalog.=) c.oid))",
            &[],
        ).await.map_err(|error| Error::db("Cannot inspect native lifecycle sources").with_source(error))?;
        let schemas: Vec<String> = row.try_get(0).map_err(|error| {
            Error::programming("Invalid native lifecycle schema preflight result")
                .with_source(error)
        })?;
        let persistent: bool = row.try_get(1).map_err(|error| {
            Error::programming("Invalid native lifecycle signal source result").with_source(error)
        })?;
        if schemas.first().map(String::as_str) != Some("pg_catalog") {
            return Err(Error::validation(
                "Native lifecycle opening requires pg_catalog as the first effective schema",
            ));
        }
        if self.shared.signal.is_some() && !persistent {
            return Err(Error::validation(
                "Native lifecycle opening requires a persistent ordinary signal store table without row-level security, partitioning or inheritance",
            ));
        }
        if self.shared.signal.is_some() {
            let valid_row = self.conn()?.query_one(
                "SELECT \
                 EXISTS(SELECT 1 FROM pg_catalog.pg_attribute a WHERE a.attrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass('erp_signal_generation') AND a.attname OPERATOR(pg_catalog.=) 'weight' AND a.atttypid OPERATOR(pg_catalog.=) 20 AND a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated OPERATOR(pg_catalog.=) '') \
                 AND EXISTS(SELECT 1 FROM pg_catalog.pg_attribute a WHERE a.attrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass('erp_signal_generation') AND a.attname OPERATOR(pg_catalog.=) 'key' AND a.atttypid OPERATOR(pg_catalog.=) 25 AND a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated OPERATOR(pg_catalog.=) '') \
                 AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_trigger t WHERE t.tgrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass('erp_signal_generation') AND NOT t.tgisinternal AND t.tgenabled OPERATOR(pg_catalog.<>) 'D') \
                 AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_rewrite r WHERE r.ev_class OPERATOR(pg_catalog.=) pg_catalog.to_regclass('erp_signal_generation') AND r.rulename OPERATOR(pg_catalog.<>) '_RETURN') \
                 AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace WHERE p.proname OPERATOR(pg_catalog.=) 'sum' AND n.nspname OPERATOR(pg_catalog.=) ANY($1) AND n.nspname OPERATOR(pg_catalog.<>) 'pg_catalog')",
                &[&schemas],
            ).await.map_err(|error| Error::db("Cannot inspect signal storage and aggregate provenance").with_source(error))?;
            let valid: bool = valid_row.try_get(0).map_err(|error| {
                Error::programming("Invalid native signal validation query result")
                    .with_source(error)
            })?;
            if !valid {
                return Err(Error::validation(
                    "Native lifecycle opening requires native signal storage types and built-in aggregate resolution",
                ));
            }
        }
        Ok(())
    }

    /// Declare that statements which may change the connection's session state ran on it (a
    /// `SET`, `SET ROLE`, `SET search_path`, `LISTEN`, a temporary table, `PREPARE`): when the
    /// transaction is dropped its connection is closed instead of going back to the pool, so no
    /// later user of the pool sees that state.
    pub fn taint_connection(&mut self) {
        if self.refuse_effect().is_err() {
            return;
        }
        self.connection_tainted = true;
    }

    /// Cancel the transaction for tests (simulates a cancelled operation).
    pub fn taint_cancelled_for_tests(&mut self) {
        self.engine.cancelled.store(true, Ordering::SeqCst);
    }

    /// Poison the transaction with an error for tests.
    pub fn poison_for_tests(&mut self, error: &Error) {
        self.poison_with(error);
    }

    fn conn(&self) -> Result<&Object> {
        self.conn.as_ref().ok_or_else(released_error)
    }

    /// Detach the connection from the pool (closing it rolls back on the server) and mark the
    /// transaction unusable.
    fn release(&mut self) {
        if let Some(conn) = self.conn.take() {
            drop(Object::take(conn));
        }
        self.state = State::Released;
        self.reset();
    }

    async fn ensure_open(&mut self) -> Result<()> {
        self.refuse_pure_effect()?;
        self.check_not_cancelled()?;
        match &self.state {
            State::Open => Ok(()),
            // boxed: the opening flight is a large future and `ensure_open` sits in every
            // statement's (and so every recursive compute's) frame
            State::Idle => Box::pin(self.start(None)).await.map(|_| ()),
            State::Beginning => {
                // An earlier BEGIN never completed (its future was dropped).
                self.release();
                Err(released_error())
            }
            State::Aborted { sqlstate, message } => Err(Error::new(
                ErrorKind::Database,
                format!(
                    "current transaction is aborted after an earlier error ({message}); roll back \
                     first"
                ),
            )
            .with_sqlstate(sqlstate.clone().or_else(|| Some("25P02".to_owned())))),
            State::Released => Err(released_error()),
        }
    }

    fn record(&mut self, sql: &str) {
        self.watch.sent();
        self.count(sql);
    }

    /// [`Transaction::record`] for the ORM's own stream statements (`SET LOCAL
    /// cursor_tuple_fraction`, `DECLARE … FOR SELECT`, `FETCH`): they change no data, so they are
    /// counted without the raw-SQL classifier, which would end the remembered record-rule
    /// decisions on every window (review M2).
    pub(crate) fn record_own(&mut self, sql: &str) {
        self.watch.sent();
        self.queries += 1;
        if let Some(capture) = &mut self.capture {
            capture.push(sql.to_owned());
        }
    }

    /// The bookkeeping half of [`Transaction::record`], without restarting the send clock. The
    /// statement is classified once (`write_gens::classify`, no allocation).
    fn count(&mut self, sql: &str) {
        self.queries += 1;
        let class = if self.strings_unsure {
            write_gens::Classified::UNKNOWN
        } else {
            write_gens::classify(sql)
        };
        if class.data_change {
            self.mutations += 1;
        }
        // Remembered record-rule decisions end with any statement that may change rows (its
        // target is not `Reads`: a write, a rollback, anything unknown) or lock them (review
        // round 3, finding 8). Cursor, savepoint and session statements (`FETCH`, `SAVEPOINT`,
        // `RELEASE`, `SET`) keep them; a `SELECT` calling a function that writes is not
        // detectable, as for the isolation of scopes. The ORM's own stream statements are not
        // classified at all (`record_own`).
        if !self.engine.readable.is_empty()
            && class
                .clears_decisions
                .unwrap_or_else(|| write_gens::locks_rows(sql))
        {
            self.engine.readable.clear();
        }
        // the models whose rows the statement may change (a stream reads them fresh by id after
        // it, `rows_stream.rs`)
        match class.target {
            write_gens::Target::Reads => {}
            write_gens::Target::Unknown => self.write_gens.all_written(),
            // the models of the table matter only to a stream reading a cursor; without one, any
            // tick will do (a later stream compares with the tick at its own DECLARE)
            write_gens::Target::Rows { .. } if !self.cursors.any_open() => {
                self.write_gens.all_written();
            }
            write_gens::Target::Rows { table, delete } => {
                match self.shared.tables.models(table, delete) {
                    Some(models) => {
                        for model in models {
                            self.write_gens.model_written(*model);
                        }
                    }
                    // a table no model knows (a view, a table of raw SQL): fail closed
                    None => self.write_gens.all_written(),
                }
            }
        }
        if let Some(capture) = &mut self.capture {
            capture.push(sql.to_owned());
        }
    }

    /// The `CLOSE` text of the cursors whose streams are done with them, counted as one statement
    /// per cursor; the caller sends it in front of its own statement ([`behind_closes`]). Called
    /// after `ensure_open`, so never in front of a `BEGIN` or in an aborted transaction.
    pub(crate) fn take_closes(&mut self) -> Option<cursors::Closes> {
        let closes = self.cursors.take_closing()?;
        self.watch.sent();
        // a `CLOSE` changes no data and no row a record rule decided on: only counted
        self.queries += closes.numbers.len() as u64;
        if let Some(capture) = &mut self.capture {
            capture.extend(closes.sql.split("; ").map(str::to_owned));
        }
        Some(closes)
    }

    /// `CLOSE`s taken for a statement that was never sent (its flight failed before its
    /// executions went out): their cursors still exist and are closed in front of a later
    /// statement (once a rollback to a savepoint recovers the transaction).
    pub(crate) fn closes_unsent(&mut self, closes: Option<cursors::Closes>) {
        if let Some(closes) = closes {
            self.cursors.closes_requeued(&closes.numbers);
        }
    }

    /// The outcome of the `CLOSE`s sent with a statement: confirmed, their numbers are free; a
    /// refused `CLOSE` is the error (the server aborted the transaction, so the statement behind
    /// it failed too) and its numbers stay taken.
    pub(crate) fn closes_sent(
        &mut self,
        closes: Option<cursors::Closes>,
        closed: std::result::Result<(), tokio_postgres::Error>,
    ) -> Result<()> {
        match (closes, closed) {
            (Some(closes), Ok(())) => {
                self.cursors.closes_confirmed(&closes.numbers);
                Ok(())
            }
            (Some(closes), Err(e)) => {
                self.cursors.closes_refused(&closes.numbers);
                Err(self.statement_failed_in(&closes.sql, e))
            }
            (None, _) => Ok(()),
        }
    }

    /// [`Transaction::statement_failed`] for `sql`, remembered for diagnostics when the server
    /// refused it.
    fn statement_failed_in(&mut self, sql: &str, e: tokio_postgres::Error) -> Error {
        if e.as_db_error().is_some() {
            self.failed_sql = Some(sql.to_owned());
        }
        self.failed(Some(sql), e)
    }

    /// [`Transaction::failed`] for a statement the ORM sent without keeping its text.
    fn statement_failed(&mut self, e: tokio_postgres::Error) -> Error {
        self.failed(None, e)
    }

    /// Map a driver error and update the state: a server error aborts the transaction, a closed
    /// connection releases it, a client-side error (e.g. a parameter that could not be encoded)
    /// leaves it as it was.
    fn failed(&mut self, _sql: Option<&str>, e: tokio_postgres::Error) -> Error {
        let server = e
            .as_db_error()
            .map(|db| (db.code().code().to_owned(), db.message().to_owned()));
        let closed = e.is_closed();
        let hit = self.timeouts.hit(&self.watch, &e);
        let session_ended = hit.as_ref().is_some_and(crate::timeouts::Hit::ends_session);
        let error = match (&self.intent, &server, hit) {
            // the database backs the read-only guard (`25006`): the same error as the guard's
            (TxIntent::Read { entry }, Some((code, _)), _) if code == "25006" => {
                read_only_sql_error(entry)
                    .with_sqlstate(Some(code.clone()))
                    .with_source(e)
            }
            // a limit of the transaction fired: the error names it
            (_, _, Some(hit)) => hit.into_error(e),
            _ => map_pg_error(&self.shared.registry, e),
        };
        if session_ended {
            // `25P04` ends the session: the connection is gone with its prepared statements
            self.release();
        } else if let Some((code, message)) = server {
            self.state = State::Aborted {
                sqlstate: Some(code),
                message,
            };
        } else if closed {
            self.release();
        }
        error
    }

    /// The error of a failed `COMMIT` (or the statement sent with it): typed when a limit fired,
    /// else mapped as any database error. The caller decides what happens to the connection.
    fn commit_failed(&self, _sql: &str, e: tokio_postgres::Error) -> Error {
        match self.timeouts.hit(&self.watch, &e) {
            Some(hit) => hit.into_error(e),
            None => map_pg_error(&self.shared.registry, e),
        }
    }

    /// Code in the transaction changed `lock_timeout` or `statement_timeout` after `BEGIN` (the
    /// schema steps do, with `set_config`): later limit errors say so instead of claiming the
    /// value the transaction began with. `statement` is the new statement limit when the ORM
    /// knows it.
    pub(crate) fn note_limits_changed(&mut self, statement: Option<Duration>) {
        self.watch.limits_changed(statement);
    }

    /// Prepared statements cached on this transaction's connection.
    pub fn prepared_statements(&self) -> usize {
        self.conn.as_ref().map_or(0, |c| c.statement_cache.size())
    }

    /// Batches of response messages the driver has delivered on this transaction's connection
    /// since it connected, and how many of them parked it on a full channel
    /// (`tokio_postgres::Client::response_batches` and `parked_batches`, amendment A3),
    /// or `None` while the transaction holds no connection. How many batches a result arrives in,
    /// and how many park, depends on how much the server had sent at each read and on when the
    /// reader takes them, so allocation tests measure the driver's per-batch work against the
    /// growth of these counts over a window ([`ResponseBatches::since`]), never against bytes.
    pub fn driver_response_batches(&self) -> Option<ResponseBatches> {
        self.conn.as_ref().map(|c| {
            ResponseBatches::new(
                deadpool::managed::Object::id(c),
                c.response_batches(),
                c.parked_batches(),
            )
        })
    }

    /// Keep the connection's statement cache within [`OrmConfig::statement_cache_size`].
    fn bound_statement_cache(&self, conn: &Object) {
        if conn.statement_cache.size() >= self.shared.config.statement_cache_size.max(1) {
            conn.statement_cache.clear();
        }
    }

    /// Run a statement returning rows (prepared and cached per connection).
    pub fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> impl std::future::Future<Output = Result<Vec<Row>>> {
        self.query_allowed("Transaction::query", sql, params, true)
    }

    /// Generated ORM DELETE RETURNING whose caller applies followed-delete invalidation.
    pub(crate) fn query_orm(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> impl std::future::Future<Output = Result<Vec<Row>>> {
        self.query_allowed("ORM statement", sql, params, false)
    }

    // Return this future directly: an extra async wrapper deepens every caller's generator.
    // Admission and invalidation stay inside it, so an unpolled query has no side effects.
    async fn query_allowed(
        &mut self,
        call: &'static str,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        scan_deletes: bool,
    ) -> Result<Vec<Row>> {
        self.require_writable_sql(call, sql)?;
        if scan_deletes {
            self.follow_sql_deletes(sql);
        }
        self.ensure_open().await?;
        self.open_isolation(Some(sql)).await?;
        let closes = self.take_closes();
        self.record(sql);
        let (closed, result) = {
            let conn = self.conn()?;
            self.bound_statement_cache(conn);
            behind_closes!(conn, closes.as_ref(), async {
                match conn.prepare_cached(sql).await {
                    Ok(stmt) => conn.query(&stmt, params).await,
                    Err(e) => Err(e),
                }
            })
        };
        self.closes_sent(closes, closed)?;
        result.map_err(|e| self.statement_failed_in(sql, e))
    }

    /// Run a statement as an unnamed statement with typed parameters: PostgreSQL plans it for these
    /// values (a custom plan) in the same round trip, and nothing is cached on the connection
    /// (statements filtered by domain literals).
    pub(crate) async fn query_per_call(
        &mut self,
        sql: &str,
        params: &[(&(dyn ToSql + Sync), tokio_postgres::types::Type)],
    ) -> Result<Vec<Row>> {
        self.require_writable_sql("Transaction::query_typed", sql)?;
        self.follow_sql_deletes(sql);
        self.ensure_open().await?;
        self.open_isolation(Some(sql)).await?;
        let closes = self.take_closes();
        self.record(sql);
        let (closed, result) = {
            let conn = self.conn()?;
            behind_closes!(conn, closes.as_ref(), conn.query_typed(sql, params))
        };
        self.closes_sent(closes, closed)?;
        result.map_err(|e| self.statement_failed_in(sql, e))
    }

    /// Run a statement and return the number of rows affected.
    pub async fn execute(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        self.require_writable_sql("Transaction::execute", sql)?;
        self.follow_sql_deletes(sql);
        self.execute_allowed(sql, params).await
    }

    /// [`Transaction::execute`] for a statement the ORM generated whose followed deletions the
    /// caller forgets itself ([`Transaction::forget_followed`]): the statement text is not scanned
    /// for a `DELETE` on a followed table (review C3-R4), everything else is the same.
    pub(crate) async fn execute_orm(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64> {
        self.require_writable_sql("ORM statement", sql)?;
        self.execute_allowed(sql, params).await
    }

    /// [`Transaction::execute`] once the read-only guard allowed the statement.
    async fn execute_allowed(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        self.ensure_open().await?;
        self.open_isolation(Some(sql)).await?;
        let closes = self.take_closes();
        self.record(sql);
        let (closed, result) = {
            let conn = self.conn()?;
            self.bound_statement_cache(conn);
            behind_closes!(conn, closes.as_ref(), async {
                match conn.prepare_cached(sql).await {
                    Ok(stmt) => conn.execute(&stmt, params).await,
                    Err(e) => Err(e),
                }
            })
        };
        self.closes_sent(closes, closed)?;
        result.map_err(|e| self.statement_failed_in(sql, e))
    }

    /// Run several statements without parameters in one round trip (DDL). Counts as one statement
    /// in [`Transaction::query_count`]. Prepared statements of every pooled connection are dropped
    /// before their next use, since DDL can change result shapes.
    ///
    /// # Errors
    /// `Programming` inside a [`HookScope`] (nothing is sent: the simple protocol could carry a
    /// transaction-control statement, amendment A3); the statement's error otherwise.
    pub async fn batch(&mut self, sql: &str) -> Result<()> {
        // the simple protocol could carry several statements, a transaction-control one among
        // them (amendment A3)
        if let Some(state) = self.hook_scope.as_mut() {
            let message = format!(
                "Transaction::batch: refused inside the {} hook (the simple protocol can carry \
                 several statements, a transaction-control one among them); the framework ends \
                 this transaction after the hook (nothing was sent)",
                state.hook
            );
            state.refused.get_or_insert_with(|| message.clone());
            return Err(Error::programming(message));
        }
        self.require_writable_sql("Transaction::batch", sql)?;
        self.follow_sql_deletes(sql);
        self.ensure_open().await?;
        self.open_isolation(None).await?;
        let closes = self.take_closes();
        self.record(sql);
        // a batch is DDL or arbitrary statements: no classification, the decisions end and any
        // model may have changed
        self.engine.readable.clear();
        self.write_gens.all_written();
        let (closed, result) = {
            let conn = self.conn()?;
            conn.statement_cache.clear();
            let sent = behind_closes!(conn, closes.as_ref(), conn.batch_execute(sql));
            self.shared.schema_changed(conn);
            sent
        };
        self.schema_changed = true;
        self.closes_sent(closes, closed)?;
        result.map_err(|e| self.statement_failed_in(sql, e))
    }

    /// A `DELETE` this transaction sends as SQL on a table whose deletions are followed (design
    /// m2-s3 §6.1): the follow-delete triggers removed key rows and bumped named caches in the
    /// database, so the transaction forgets the same here and its own lookups do not serve what
    /// the statement removed (review C2-R3; other processes see the bump). The statement is read
    /// with the word scanner: a `DELETE` naming a followed table forgets that table's effects, one
    /// the scanner does not read with certainty forgets every followed table's (over-invalidation
    /// is the safe side). An unquoted table name matches in any case (PostgreSQL folds it), a
    /// quoted one exactly (review C3-R2). A statement without the bytes `delete` (any case) costs
    /// one byte search.
    ///
    /// Every entry point that takes caller-supplied SQL runs it: [`Transaction::query`],
    /// [`Transaction::execute`], [`Transaction::batch`], [`Transaction::query_typed`],
    /// [`Transaction::query_each`], the crate's generic per-call, streamed and flight paths
    /// (review C3-X2), and the principal resolver's statement sent with `BEGIN` (review C4-X1).
    /// The ORM's own `DELETE`s (unlink's, a many2many's removed pairs) go through
    /// [`Transaction::execute_orm`] instead, which skips the scan: those callers forget the
    /// followed effects themselves (review C3-R4).
    ///
    /// Not covered (review C3-X3): a deletion the statement's text does not show, such as a
    /// database function or rule that deletes from a followed table (`SELECT f()`). The triggers
    /// still keep the key table in step and bump the cache for the other processes, but this
    /// transaction's own cached bindings of that table can serve the removed rows until it ends;
    /// a caller deleting that way calls [`Transaction::invalidate_ormcache`] for the cache and
    /// [`Transaction::invalidate_model`] for the key-table model itself.
    fn follow_sql_deletes(&mut self, sql: &str) {
        if self.shared.relations.follow_tables().is_empty()
            || !sql
                .as_bytes()
                .windows(6)
                .any(|w| w.eq_ignore_ascii_case(b"delete"))
        {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let tables = shared.relations.follow_tables();
        for at in sql_deleted_tables(sql, tables) {
            self.forget_followed(&tables[at].1);
        }
    }

    /// After a rollback that may undo DDL of this transaction: drop the connection's prepared
    /// statements (they may describe the rolled-back shape). Other connections never saw the
    /// uncommitted shape, so the database's schema epoch is left alone.
    fn forget_rolled_back_schema(&self) {
        if self.schema_changed
            && let Some(conn) = &self.conn
        {
            conn.statement_cache.clear();
        }
    }

    /// Flush pending writes, then `COMMIT`; the cache is cleared and after-commit hooks run.
    /// A transaction aborted by an earlier error is rolled back and the commit
    /// fails (`Concurrency` for serialization failures and deadlocks, else `Database`); so does a
    /// failed flush. A failed `COMMIT` releases the connection.
    ///
    /// # Errors
    /// Also `Programming` on a transaction managed by a module operation
    /// ([`Transaction::manage_operation`]): the operation commits it itself; and inside a
    /// [`HookScope`] (nothing is sent; the framework ends the transaction after the hook).
    pub async fn commit(&mut self) -> Result<()> {
        self.refuse_adapter_effect()?;
        self.refuse_in_hook_scope("Transaction::commit")?;
        self.refuse_when_managed("Transaction::commit")?;
        self.commit_inner().await
    }

    async fn commit_inner(&mut self) -> Result<()> {
        if self.state == State::Released {
            return Err(released_error());
        }
        self.commit_preamble().await?;
        match self.state.clone() {
            State::Idle => self.publish_ormcache(),
            State::Open => {
                self.watch.sent();
                let result = self.conn()?.batch_execute("COMMIT").await;
                if let Err(e) = result {
                    let error = self.commit_failed("COMMIT", e);
                    self.release();
                    return Err(error);
                }
                // The DDL is visible to every session only now: statements other connections
                // prepared since the DDL ran describe the shape before it.
                if self.schema_changed
                    && let Some(conn) = &self.conn
                {
                    self.shared.schema_changed(conn);
                }
                self.state = State::Idle;
                self.publish_ormcache();
            }
            State::Aborted { sqlstate, message } => {
                return Err(self
                    .discard_after(aborted_commit_error(sqlstate, &message))
                    .await);
            }
            State::Beginning | State::Released => {
                self.release();
                return Err(released_error());
            }
        }
        self.committed();
        Ok(())
    }

    /// The checks and the flush before `COMMIT` (shared by [`Transaction::commit`] and
    /// [`Transaction::commit_pipelined`]). Flush, signal and capture-persistence failures roll
    /// back, including poison raised during a flush whose callback catches its error. Refusals
    /// before flushing leave the transaction open for the caller to roll back.
    async fn commit_preamble(&mut self) -> Result<()> {
        self.check_not_cancelled()?;
        if !self.savepoints.is_empty() {
            return Err(Error::programming(
                "Pending savepoints not released, cannot commit!",
            ));
        }
        if !self.isolated.is_empty() {
            return Err(Error::programming(
                "cannot commit inside an isolated scope (onchange): its changes are always \
                 discarded",
            ));
        }
        // A database error already doomed the transaction: report it (below), poisoned or not.
        if !matches!(self.state, State::Aborted { .. })
            && let Some(error) = self.poison_error()
        {
            return Err(error);
        }
        debug_assert!(
            !self.engine.has_protections(),
            "protection scopes left open at commit"
        );
        if !matches!(self.state, State::Aborted { .. })
            && let Err(error) = self.flush().await
        {
            return Err(self.discard_after(error).await);
        }
        // precommit hooks (D7): after the flush, before COMMIT
        if !matches!(self.state, State::Aborted { .. })
            && let Err(error) = self.run_precommit_hooks().await
        {
            return Err(self.discard_after(error).await);
        }
        // A compute or hook may catch an admission refusal while flushing, leaving its
        // business work poisoned even when flush itself returns Ok. Recheck the transaction
        // before signals or capture persistence; both commit paths must discard that work.
        if !matches!(self.state, State::Aborted { .. })
            && let Some(error) = self.poison_error()
        {
            return Err(self.discard_after(error).await);
        }
        // named caches invalidated here are signalled in the same transaction (this opens it
        // when nothing else did)
        if self.shared.signal.is_some()
            && matches!(self.state, State::Idle | State::Open)
            && let Err(error) = self.signal_cache_invalidations().await
        {
            return Err(self.discard_after(error).await);
        }
        if matches!(self.state, State::Idle | State::Open) && !self.outbox.events().is_empty() {
            // Never move capture out of the transaction across an await. If this future is
            // cancelled, the frozen buffer forces rollback before another commit attempt.
            let encoded = self
                .outbox
                .freeze()
                .and_then(|()| crate::outbox::encode(self.outbox.events()));
            let payload = match encoded {
                Ok(payload) => payload,
                Err(error) => return Err(self.discard_after(error).await),
            };
            let expected = self.outbox.events().len() as u64;
            match crate::outbox::persist_encoded(self, &payload).await {
                Ok(actual) if actual == expected => {}
                Ok(actual) => {
                    let error = Error::programming(format!(
                        "outbox: capture INSERT wrote {actual} events, expected {expected}"
                    ));
                    return Err(self.discard_after(error).await);
                }
                Err(error) => return Err(self.discard_after(error).await),
            }
        }
        // the audit row of this attempt: one INSERT, after the outbox's; a failure
        // rolls the business change back
        if matches!(self.state, State::Idle | State::Open)
            && let Err(error) = self.persist_audit().await
        {
            return Err(self.discard_after(error).await);
        }
        Ok(())
    }

    /// After a successful `COMMIT` (or nothing to commit): signal bookkeeping, cache reset and
    /// after-commit hooks.
    fn committed(&mut self) {
        let own_bumps = self.signal_state.take_own_bumps();
        let hooks = std::mem::take(&mut self.after_commit);
        self.reset();
        self.schema_changed = false;
        // Window: the server's `COMMIT` is visible to other sessions before this clear runs. A
        // transaction of an ORM we switched away from (another task of this process) can, in
        // between, read a value cached before the commit and so serve data one commit old; once
        // the clear below ran, everything cached later was read after the commit. The clear
        // cannot move before the `COMMIT`: readers would refill the cache from pre-commit rows
        // and nothing would clear it again. Callers that need the new data at once use the ORM
        // of the operation's outcome, whose cache starts empty.
        for old in std::mem::take(&mut self.switched_from) {
            // An ORM nobody holds any more has no cache to correct.
            if let Some(old) = old.upgrade() {
                old.ormcache.clear_all();
            }
        }
        if let Some(signal) = &self.shared.signal {
            for (key, began_at) in &own_bumps {
                signal.note_own_bump(key, *began_at);
            }
        }
        for hook in hooks {
            hook();
        }
    }

    /// Commit with an optional last statement pipelined before `COMMIT` (sent back to back,
    /// answered in one round trip; and 8), reporting the outcome the way
    /// the RPC layer needs it: [`CommitError::RolledBack`] when nothing was committed for certain,
    /// [`CommitError::Unknown`] when `COMMIT` may or may not have been applied.
    ///
    /// The outcome follows the server's answers. When the pipelined statement fails, PostgreSQL
    /// answers it with the error and answers `COMMIT` with the `ROLLBACK` command tag and no
    /// error: a definite rollback, reported with the statement's mapped error. A server error on
    /// `COMMIT` itself (`40001`, a deferred constraint) is a definite rollback too. Success is a
    /// `COMMIT` answered without error after every earlier statement of the transaction was
    /// answered without error: the state machine reads every answer, so a transaction that
    /// reported an error is `Aborted` and never reaches `COMMIT` here, and the server answers a
    /// `COMMIT` with `ROLLBACK` only in a transaction that reported an error (`tokio-postgres`
    /// does not expose the command tag text itself). Only a missing answer (closed connection,
    /// I/O error) is [`CommitError::Unknown`]; the connection is then discarded. A future dropped
    /// while `COMMIT` is in flight (a deadline) detaches the connection, as every transaction
    /// future does; the caller treats that as unknown too.
    ///
    /// # Errors
    ///
    /// [`CommitError::RolledBack`] with the error of [`Transaction::commit`] for the refusals
    /// before `COMMIT` (pending savepoints, a poisoned or aborted transaction, a failed flush;
    /// the transaction is rolled back), or with the mapped error of the failed statement or
    /// `COMMIT`; `Programming` when `last` is given but no transaction is open.
    /// [`CommitError::Unknown`] when an answer is missing.
    ///
    /// On a transaction managed by a module operation ([`Transaction::manage_operation`]) it
    /// returns [`CommitError::RolledBack`] with a `Programming` error without touching the
    /// transaction (which is not rolled back: the operation ends it). Inside a [`HookScope`] it
    /// does the same (the framework ends the transaction after the hook).
    pub async fn commit_pipelined(
        &mut self,
        last: Option<FinalStatement<'_>>,
    ) -> std::result::Result<(), CommitError> {
        if let Err(error) = self.refuse_in_hook_scope("Transaction::commit_pipelined") {
            return Err(CommitError::RolledBack(error));
        }
        if let Err(error) = self.refuse_when_managed("Transaction::commit_pipelined") {
            return Err(CommitError::RolledBack(error));
        }
        self.commit_classified_inner(last).await
    }

    /// Classified commit machinery shared by ordinary RPC and controlled module operations.
    async fn commit_classified_inner(
        &mut self,
        last: Option<FinalStatement<'_>>,
    ) -> std::result::Result<(), CommitError> {
        if self.state == State::Released {
            return Err(CommitError::RolledBack(released_error()));
        }
        if let Err(error) = self.commit_preamble().await {
            let error = self.discard_after(error).await;
            return Err(CommitError::RolledBack(error));
        }
        match self.state.clone() {
            State::Idle => {
                if let Some(last) = last {
                    return Err(CommitError::RolledBack(Error::programming(format!(
                        "commit_pipelined: no transaction is open to run '{}' in",
                        last.sql
                    ))));
                }
                self.publish_ormcache();
            }
            State::Open => self.send_commit(last).await?,
            State::Aborted { sqlstate, message } => {
                let error = self
                    .discard_after(aborted_commit_error(sqlstate, &message))
                    .await;
                return Err(CommitError::RolledBack(error));
            }
            State::Beginning | State::Released => {
                self.release();
                return Err(CommitError::RolledBack(released_error()));
            }
        }
        self.committed();
        Ok(())
    }

    /// Send `last` (if any) and `COMMIT` pipelined, and classify the answers.
    async fn send_commit(
        &mut self,
        last: Option<FinalStatement<'_>>,
    ) -> std::result::Result<(), CommitError> {
        #[cfg(feature = "test-faults")]
        let fault = self.commit_fault.take();
        #[cfg(feature = "test-faults")]
        if fault == Some(CommitFault::ConnectionClosedBeforeCommit) {
            self.release();
            return Err(CommitError::Unknown(Error::new(
                ErrorKind::Database,
                "database connection closed: injected fault before COMMIT",
            )));
        }
        self.watch.sent();
        let mut recorded = false;
        let sent = {
            let conn = self.conn().map_err(CommitError::RolledBack)?;
            match &last {
                Some(last) => {
                    // Poll the final statement alone first: with tokio-postgres 0.7.18
                    // (Cargo.lock) its first poll encodes the parameters and queues the
                    // request, so an error on that poll means it was never sent. The test
                    // `a_final_statement_refused_by_the_driver_rolls_back_the_earlier_writes`
                    // (rtt_start.rs) guards this assumption across driver upgrades. COMMIT must
                    // then not follow, or the earlier writes would commit without the final
                    // statement.
                    let statement = conn.query_typed(last.sql, last.params);
                    let mut statement = std::pin::pin!(statement);
                    let first = std::future::poll_fn(|cx| {
                        std::task::Poll::Ready(statement.as_mut().poll(cx))
                    })
                    .await;
                    // counted only once the statement is bound (a refused one is not counted)
                    if !matches!(first, std::task::Poll::Ready(Err(_))) {
                        recorded = true;
                    }
                    match first {
                        std::task::Poll::Ready(Err(e)) => Err(e),
                        std::task::Poll::Ready(Ok(rows)) => {
                            Ok((Some(Ok(rows)), conn.batch_execute("COMMIT").await))
                        }
                        std::task::Poll::Pending => {
                            let (statement, commit) =
                                pipeline2(statement, conn.batch_execute("COMMIT")).await;
                            Ok((Some(statement), commit))
                        }
                    }
                }
                None => Ok((None, conn.batch_execute("COMMIT").await)),
            }
        };
        // If this future is dropped while `COMMIT` is awaited above, a final statement that was
        // already sent stays uncounted. Harmless: the transaction is detached on drop.
        if recorded && let Some(last) = &last {
            // not `record`: the send clock was started before the statement left
            self.count(last.sql);
        }
        let (statement, commit) = match sent {
            Ok(pair) => pair,
            Err(e) => {
                let error = self.commit_failed(last.as_ref().map_or("COMMIT", |l| l.sql), e);
                let error = self.discard_after(error).await;
                return Err(CommitError::RolledBack(error));
            }
        };
        let last_sql = last.as_ref().map_or("COMMIT", |l| l.sql);
        match (statement, commit) {
            (Some(Err(e)), commit) if e.as_db_error().is_some() => {
                // the statement was answered with an error, so COMMIT answered ROLLBACK
                let error = self.commit_failed(last_sql, e);
                match commit {
                    Ok(()) => {
                        self.forget_rolled_back_schema();
                        self.schema_changed = false;
                        self.state = State::Idle;
                        self.reset();
                    }
                    Err(_) => self.release(),
                }
                Err(CommitError::RolledBack(error))
            }
            (Some(Err(e)), _) => {
                let error = self.commit_failed(last_sql, e);
                self.release();
                Err(CommitError::Unknown(error))
            }
            (_, Err(e)) if e.as_db_error().is_some() => {
                let error = self.commit_failed("COMMIT", e);
                self.release();
                Err(CommitError::RolledBack(error))
            }
            (_, Err(e)) => {
                let error = self.commit_failed("COMMIT", e);
                self.release();
                Err(CommitError::Unknown(error))
            }
            (Some(Ok(_)) | None, Ok(())) => {
                #[cfg(feature = "test-faults")]
                if fault == Some(CommitFault::AnswerLostAfterCommit) {
                    self.release();
                    return Err(CommitError::Unknown(Error::new(
                        ErrorKind::Database,
                        "database connection closed: injected fault after COMMIT",
                    )));
                }
                // The DDL is visible to every session only now: statements other connections
                // prepared since the DDL ran describe the shape before it.
                if self.schema_changed
                    && let Some(conn) = &self.conn
                {
                    self.shared.schema_changed(conn);
                }
                self.state = State::Idle;
                self.publish_ormcache();
                Ok(())
            }
        }
    }

    /// Make the next [`Transaction::commit_pipelined`] simulate a lost connection (tests of the
    /// ambiguous-commit path, RPC-012). Only with the `test-faults` feature.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn inject_commit_fault(&mut self, fault: CommitFault) {
        self.commit_fault = Some(fault);
    }

    /// Move this open transaction to another ORM of the same database: same
    /// connection, same PostgreSQL transaction, a new registry. Used by the module system to step
    /// through the stage registries of one install transaction.
    ///
    /// Pending writes are flushed first, on the current ORM. Then the record cache, the compute
    /// engine's state and the transaction's ormcache scope are cleared and the connection's
    /// prepared statements dropped; the query counter, captured statements, after-commit hooks and
    /// the transaction timestamp are kept. The environment keeps its uid, superuser mode and
    /// context on the new registry. Records built before the switch belong to the old registry:
    /// every ORM operation of this transaction refuses them. When the transaction commits, the
    /// shared ormcache of every ORM it switched away from is cleared (it may hold values of data
    /// the transaction changed); compiled searches (search memo) depend on the registry only and
    /// stay valid.
    ///
    /// # Errors
    /// `Access` (nothing is switched) unless the transaction acts as the `System` principal with
    /// full elevation (S1: a user transaction cannot move onto another registry).
    /// `Programming` (nothing is switched) when `orm` belongs to another database identity (see
    /// [`Orm::rebuild`]) or uses the transaction's current registry, inside a savepoint, an
    /// elevation block ([`Transaction::elevated`]) or an isolated (onchange) scope, when the
    /// transaction
    /// is poisoned, aborted, cancelled or released; the flush's error when the flush fails; also
    /// `Programming` on a transaction managed by a module operation
    /// ([`Transaction::manage_operation`]), which switches through
    /// [`Transaction::operation_switch_orm`], and inside a [`HookScope`].
    pub async fn switch_orm(&mut self, orm: &Orm) -> Result<()> {
        self.refuse_in_hook_scope("Transaction::switch_orm")?;
        self.refuse_when_managed("Transaction::switch_orm")?;
        self.switch_orm_inner(orm).await
    }

    async fn switch_orm_inner(&mut self, orm: &Orm) -> Result<()> {
        self.require_system_full("Transaction::switch_orm")?;
        if self.state == State::Released {
            return Err(released_error());
        }
        self.check_not_cancelled()?;
        if !Arc::ptr_eq(&self.shared.database, &orm.shared.database) {
            return Err(Error::programming(
                "cannot switch the transaction to an ORM of another database: switch_orm only \
                 accepts ORMs built with Orm::rebuild from this transaction's ORM",
            ));
        }
        if Arc::ptr_eq(&self.shared.registry, &orm.shared.registry) {
            // Records cannot tell two ORMs of one registry apart: refusing keeps "records of the
            // old ORM are refused" true for every successful switch.
            return Err(Error::programming(
                "cannot switch the transaction to the registry it already uses",
            ));
        }
        if !self.savepoints.is_empty() {
            return Err(Error::programming(
                "cannot switch the transaction's ORM inside a savepoint",
            ));
        }
        // a block's scope was validated against the current registry
        if let Some(block) = crate::principal::block::lock(&self.blocks)?.innermost() {
            return Err(Error::programming(format!(
                "cannot switch the transaction's ORM inside elevation block {}#{}",
                block.tx().get(),
                block.seq()
            )));
        }
        if !self.isolated.is_empty() {
            return Err(Error::programming(
                "cannot switch the transaction's ORM inside an isolated scope (onchange)",
            ));
        }
        if let State::Aborted { message, .. } = &self.state {
            return Err(Error::programming(format!(
                "cannot switch the ORM of an aborted transaction ({message}); roll back first"
            )));
        }
        let refuse_poisoned = |poison: &(ErrorKind, String)| {
            Error::programming(format!(
                "cannot switch the ORM of a transaction that must be rolled back ({})",
                poison.1
            ))
        };
        if let Some(poison) = &self.poison {
            return Err(refuse_poisoned(poison));
        }
        self.flush().await?;
        if let Some(poison) = &self.poison {
            return Err(refuse_poisoned(poison));
        }
        self.cache.reset();
        self.engine.reset();
        self.engine.ormcache.reset();
        if let Some(conn) = &self.conn {
            conn.statement_cache.clear();
            orm.shared.refresh_statements(conn);
        }
        self.env = self.env.rebind(
            Arc::clone(&orm.shared.registry),
            Arc::clone(&orm.shared.generation_attachments),
        );
        let old = std::mem::replace(&mut self.shared, Arc::clone(&orm.shared));
        let old = Arc::downgrade(&old);
        // Switching away from the same ORM twice (A, B, A, C) still clears its cache once.
        if !self.switched_from.iter().any(|known| known.ptr_eq(&old)) {
            self.switched_from.push(old);
        }
        Ok(())
    }

    /// `ROLLBACK`; pending writes and after-commit hooks are discarded and the cache cleared.
    ///
    /// # Errors
    /// `Database` when `ROLLBACK` failed (or the connection was already gone): the connection is
    /// then released instead, and closing it rolls the transaction back on the server, so nothing
    /// of the transaction persists either way. The error says why the rollback statement failed.
    /// Also `Programming` on a transaction managed by a module operation
    /// ([`Transaction::manage_operation`]), and inside a [`HookScope`] (nothing is sent; the
    /// framework ends the transaction after the hook).
    pub async fn rollback(&mut self) -> Result<()> {
        self.refuse_adapter_effect()?;
        self.refuse_in_hook_scope("Transaction::rollback")?;
        self.refuse_when_managed("Transaction::rollback")?;
        self.rollback_inner().await
    }

    async fn rollback_inner(&mut self) -> Result<()> {
        match self.discard_reporting().await {
            None => Ok(()),
            Some(error) => Err(error),
        }
    }

    /// Roll back whatever is open after `error`, and return the error to report: `error` itself,
    /// or, when `ROLLBACK` failed too (the connection was closed instead, which rolls back on the
    /// server), an error of the same kind, SQLSTATE and condition whose message adds the
    /// rollback's, with `error` as its source. The caller's error stays the primary one.
    pub(crate) async fn discard_after(&mut self, error: Error) -> Error {
        let Some(rollback) = self.discard_reporting().await else {
            return error;
        };
        let mut combined = Error::new(
            error.kind(),
            format!("{} (and {})", error.message(), rollback.message()),
        )
        .with_sqlstate(error.sqlstate().map(str::to_owned));
        if let Some(condition) = error.condition() {
            combined = combined.with_condition(condition);
        }
        if let Some(context) = error.context() {
            combined = combined.with_context(context.clone());
        }
        combined.with_source(error)
    }

    /// Take the right to end this transaction: from now on [`Transaction::commit`],
    /// [`Transaction::rollback`], [`Transaction::commit_pipelined`] and
    /// [`Transaction::switch_orm`] refuse it (`Programming`), and only the returned control ends
    /// it ([`Transaction::operation_commit`], [`Transaction::operation_commit_classified`],
    /// [`Transaction::operation_rollback`]) or switches its
    /// ORM ([`Transaction::operation_switch_orm`]). Used by the module system for the
    /// transactions of its operations, whose hooks and loaders must not end them.
    ///
    /// The [`OperationKey`] proves the caller is the module system (it is claimed once per
    /// process); the trust boundary is described there.
    ///
    /// # Errors
    /// `Programming` when the transaction is managed already (the control exists elsewhere), and
    /// inside a [`HookScope`].
    pub fn manage_operation(&mut self, _key: &OperationKey) -> Result<OperationControl> {
        self.refuse_in_hook_scope("Transaction::manage_operation")?;
        self.refuse_effect()?;
        if self.operation.is_some() {
            return Err(Error::programming(
                "Transaction::manage_operation: this transaction is managed by a module operation \
                 already",
            ));
        }
        let id = NEXT_CONTROL_ID.fetch_add(1, Ordering::Relaxed);
        self.operation = Some(id);
        Ok(OperationControl { id })
    }

    /// `Programming` naming `call` when the transaction is managed by a module operation.
    /// Refuse unless the transaction acts as the `System` principal with full elevation: the
    /// module system's own transaction. Switching the ORM swaps the dispatch tables a
    /// transaction runs on, so a user transaction must never do it.
    pub fn require_system_full(&self, call: &str) -> Result<()> {
        let principal = self.env.principal();
        if principal.kind() == crate::PrincipalKind::System && principal.elevation().is_full() {
            Ok(())
        } else {
            Err(Error::access(format!(
                "{call}: only a transaction of the System principal with full elevation may do \
                 this (this one acts as {} {})",
                principal.kind().as_str(),
                self.env.uid()
            )))
        }
    }

    fn refuse_when_managed(&self, call: &str) -> Result<()> {
        self.refuse_effect()?;
        if self.operation.is_some() {
            return Err(Error::programming(format!(
                "{call}: this transaction belongs to a module operation, which commits or rolls it \
                 back itself; a hook, data loader or model init must not end it (nothing was \
                 committed or rolled back)"
            )));
        }
        Ok(())
    }

    /// Whether a module operation manages this transaction ([`Transaction::manage_operation`]):
    /// only that operation commits or rolls it back.
    pub fn is_operation_managed(&self) -> bool {
        self.operation.is_some()
    }

    pub(crate) fn check_control(&self, control: &OperationControl, call: &str) -> Result<()> {
        self.refuse_effect()?;
        if self.operation == Some(control.id) {
            Ok(())
        } else {
            Err(Error::programming(format!(
                "{call}: the operation control belongs to another transaction"
            )))
        }
    }

    /// [`Transaction::commit`] for the holder of the control.
    ///
    /// # Errors
    /// As [`Transaction::commit`]; `Programming` for the control of another transaction.
    pub async fn operation_commit(&mut self, control: &OperationControl) -> Result<()> {
        self.check_control(control, "Transaction::operation_commit")?;
        self.commit_inner().await
    }

    /// Commit for the matching operation controller, preserving certainty at the database
    /// boundary. Uses the same flush, schema-cache publication and commit-hook machinery as
    /// [`Transaction::commit_pipelined`], without a final statement.
    ///
    /// An unknown outcome discards the connection and does not run commit hooks. The holder
    /// must reconcile durable state before retrying; it must not assume rollback or replay.
    ///
    /// ```no_run
    /// # async fn finish(tx: &mut erp_orm::Transaction, control: &erp_orm::OperationControl)
    /// # -> Result<(), erp_orm::CommitError> {
    /// tx.operation_commit_classified(control).await
    /// # }
    /// ```
    ///
    /// # Errors
    /// [`CommitError::RolledBack`] for a known failure before or at commit, retaining the
    /// underlying error; [`CommitError::Unknown`] when the commit answer is missing. A foreign
    /// control returns `RolledBack(Programming)` without touching the transaction, which stays
    /// owned by its matching controller.
    pub async fn operation_commit_classified(
        &mut self,
        control: &OperationControl,
    ) -> std::result::Result<(), CommitError> {
        self.check_control(control, "Transaction::operation_commit_classified")
            .map_err(CommitError::RolledBack)?;
        self.commit_classified_inner(None).await
    }

    /// [`Transaction::rollback`] for the holder of the control.
    ///
    /// # Errors
    /// As [`Transaction::rollback`]; `Programming` for the control of another transaction.
    pub async fn operation_rollback(&mut self, control: &OperationControl) -> Result<()> {
        self.check_control(control, "Transaction::operation_rollback")?;
        self.rollback_inner().await
    }

    /// [`Transaction::switch_orm`] for the holder of the control.
    ///
    /// # Errors
    /// As [`Transaction::switch_orm`]; `Programming` for the control of another transaction.
    pub async fn operation_switch_orm(
        &mut self,
        control: &OperationControl,
        orm: &Orm,
    ) -> Result<()> {
        self.check_control(control, "Transaction::operation_switch_orm")?;
        self.switch_orm_inner(orm).await
    }

    /// Bind the freshly authorized human to this lifecycle operation's internal elevation.
    /// Only the module engine holds the matching control; hooks cannot change this identity.
    /// The original human remains the impersonator for audit throughout registry switches.
    /// # Errors
    /// Programming for a foreign control or repeated binding; Access for a non-human,
    /// unresolved, impersonated or elevated principal.
    pub fn operation_bind_actor(
        &mut self,
        control: &OperationControl,
        principal: Principal,
    ) -> Result<()> {
        self.check_control(control, "Transaction::operation_bind_actor")?;
        self.require_system_full("Transaction::operation_bind_actor")?;
        if self.env.principal().actor().is_some() {
            return Err(Error::programming(
                "The lifecycle operation already has an actor",
            ));
        }
        if principal.kind() != crate::PrincipalKind::User
            || principal.actor().is_some()
            || principal.scope().is_some()
            || !matches!(principal.elevation(), crate::Elevation::None)
            || !principal.resolution().is_resolved()
        {
            return Err(Error::access(
                "A lifecycle actor must be a resolved, unelevated session user",
            ));
        }
        self.replace_principal(principal.switched_to(crate::SUPERUSER_ID));
        Ok(())
    }

    /// Roll back whatever is open; a failed `ROLLBACK` releases the connection (closing it rolls
    /// back on the server), is logged at WARN and returned.
    async fn discard_reporting(&mut self) -> Option<Error> {
        self.after_commit.clear();
        // Nothing of the transaction persists: the old ORMs' caches stay valid.
        self.switched_from.clear();
        match self.state {
            State::Idle | State::Released => {
                self.reset();
                None
            }
            // A `BEGIN` whose future was dropped: closing the connection ends whatever it opened.
            State::Beginning => {
                self.release();
                None
            }
            State::Open | State::Aborted { .. } => {
                // The state stays Open/Aborted while ROLLBACK is in flight: if this future is
                // dropped, Drop detaches the connection.
                let result = match self.conn.as_ref() {
                    Some(conn) => conn.batch_execute("ROLLBACK").await.map_err(|e| {
                        let error = map_pg_error(&self.shared.registry, e);
                        Error::new(
                            ErrorKind::Database,
                            format!(
                                "ROLLBACK failed, the connection was closed instead (which rolls \
                                 the transaction back on the server): {}",
                                error.message()
                            ),
                        )
                        .with_sqlstate(error.sqlstate().map(str::to_owned))
                    }),
                    None => Err(Error::new(
                        ErrorKind::Database,
                        "ROLLBACK not sent: the transaction has no connection",
                    )),
                };
                match result {
                    Ok(()) => {
                        self.forget_rolled_back_schema();
                        self.schema_changed = false;
                        self.state = State::Idle;
                        self.reset();
                        None
                    }
                    Err(error) => {
                        // Diagnostics only (the error is returned): no SQLSTATE logs as "".
                        tracing::warn!(
                            sqlstate = error.sqlstate().unwrap_or(""),
                            "{}",
                            error.message()
                        );
                        self.release();
                        Some(error)
                    }
                }
            }
        }
    }

    fn reset(&mut self) {
        // every rollback path ends here; `committed` takes the hooks out before calling it
        self.after_commit.clear();
        self.access.clear_memo();
        self.refresh = crate::access::RefreshState::default();
        self.private_texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.policy = self.shared.policy.current();
        self.cache.reset();
        self.isolated.clear();
        self.engine.reset();
        self.engine.ormcache.reset();
        self.savepoints.clear();
        // the server ended every cursor with the transaction: nothing to close, numbering restarts
        self.cursors.end_all();
        self.signal_state.reset();
        self.outbox.clear();
        self.audit.clear();
        // the database transaction ended: none of its blocks is open any more, and the next
        // block takes a new serial (closing is the safe direction on a poisoned set)
        self.blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reset();
        self.pending_capture.clear();
        self.scoped_ids.clear();
        self.versions.clear();
        self.tracking.clear();
        self.poison = None;
        self.now = None;
        self.html_reports.clear();
        self.html_mode = erp_fields::html::SanitizeMode::Clean;
        self.notices.clear();
        self.savepoint_notices.clear();
        self.notices_overflowed = 0;
        self.active_langs = None;
        self.translated_baselines.clear();
        self.candidate_stage = None;
    }

    /// Remember the first `Validation` error raised by a data operation (decision E1): the values
    /// it was about may already be in the cache or in the database, so commit is refused.
    pub(crate) fn poison_on_validation(&mut self, error: &Error) {
        if error.kind() == ErrorKind::Validation && self.poison.is_none() {
            self.poison = Some((error.kind(), error.message().to_owned()));
        }
    }

    /// Poison the transaction with `error`: an operation failed after its changes reached the
    /// database (a write hook after the flush `UPDATE`), so commit must refuse.
    pub(crate) fn poison_with(&mut self, error: &Error) {
        if self.poison.is_none() {
            self.poison = Some((error.kind(), error.message().to_owned()));
        }
    }

    /// The change counter, to compare before and after an operation (F1).
    pub(crate) fn mutations(&self) -> u64 {
        self.mutations
    }

    /// Count a change that sends no statement yet (a pending write in the cache).
    pub(crate) fn note_mutation(&mut self) {
        self.mutations += 1;
    }

    /// Database records of `model` have a pending change of `fields` (a write or a recompute in
    /// the cache): the remembered decisions of a rule that reads one of them end (a
    /// memo is dropped when a field its predicates read is written). Changes to in-memory
    /// records do not call this.
    pub(crate) fn note_database_change(
        &mut self,
        model: ModelId,
        fields: &[erp_registry::FieldId],
    ) {
        let registry = Arc::clone(&self.shared.registry);
        self.engine.readable.fields_written(
            model,
            fields,
            |m, f| registry.model_by_id(m).field_by_id(f).magic,
            &self.shared.graph,
        );
        // the memoized record checks of the access engine end on the same trigger
        self.access_written(model, Some(fields));
        if self
            .shared
            .registry
            .model("res.lang")
            .is_some_and(|m| m.id == model)
        {
            self.active_langs = None;
            self.langs_dirty = true;
        }
    }

    /// The error of an ORM operation that started when the change counter was `before`: it
    /// poisons the transaction when it is a `Validation` error or when the operation had already
    /// changed something (rows inserted, deleted or written, pending writes, relation rows), since
    /// committing would keep half of the operation (F1).
    pub(crate) fn poison_after(&mut self, before: u64, error: &Error) {
        if self.poison.is_none()
            && (error.kind() == ErrorKind::Validation || self.mutations != before)
        {
            self.poison = Some((error.kind(), error.message().to_owned()));
        }
    }

    pub(crate) fn poison_error(&self) -> Option<Error> {
        self.poison
            .as_ref()
            .map(|(kind, message)| Error::new(*kind, message.clone()))
    }

    /// Whether a savepoint or an isolated scope (onchange) is open: what is inside may still be
    /// rolled back.
    pub(crate) fn inside_scope(&self) -> bool {
        !self.savepoints.is_empty() || !self.isolated.is_empty()
    }

    /// Make commit refuse because an operation of a layer above the ORM failed after its effects
    /// reached the transaction (the WASM host: a guest trapped, ran out of time or broke a limit
    /// after calling the ORM). The first reason is kept; a rollback clears it.
    pub fn poison(&mut self, error: &Error) {
        if self.refuse_effect().is_err() {
            return;
        }
        self.poison_with(error);
    }

    /// The number of changes made so far (rows inserted, deleted or written, pending writes). A
    /// layer that caches reads compares it between calls: it moved when something was written.
    pub fn change_count(&self) -> u64 {
        self.mutations
    }

    /// True when commit will refuse: a `Validation` error, or an error of an operation that had
    /// already changed data, was raised since the last rollback (F1).
    pub fn is_poisoned(&self) -> bool {
        self.poison.is_some()
            || self
                .engine
                .cancelled
                .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Drop every cached value after flushing (`env.invalidate_all()`), and every memoized
    /// record-access check (data changed outside the ORM may change their outcome).
    pub async fn invalidate_all(&mut self) -> Result<()> {
        self.refuse_adapter_effect()?;
        self.flush().await?;
        self.engine.readable.clear();
        self.clear_values();
        self.access.clear_memo();
        // data changed outside the ORM: streams read fresh from here on
        self.write_gens.all_written();
        Ok(())
    }

    /// Drop every cached value without flushing; pending computations survive (they are
    /// recomputed from the database values).
    fn clear_values(&mut self) {
        self.engine.precisions.clear();
        self.engine.write_precisions.clear();
        let registry = Arc::clone(&self.shared.registry);
        for spec in registry.models() {
            if let Some(mc) = self.cache.model(spec.id)
                && mc.has_dirty()
            {
                // flushed just before: nothing dirty is left, but never drop a pending write
                continue;
            }
            self.cache.model_mut(spec.id).invalidate(None, None);
        }
        self.engine.context = Default::default();
    }

    /// Drop cached values of a model (`invalidate_model(fnames)`), after flushing, and the
    /// memoized record-access checks whose predicates read them.
    pub async fn invalidate_model(&mut self, model: &str, fields: Option<&[&str]>) -> Result<()> {
        self.refuse_adapter_effect()?;
        let registry = Arc::clone(&self.shared.registry);
        let spec = registry.model(model).ok_or_else(|| {
            Error::programming(format!("Model '{model}' does not exist in registry."))
        })?;
        let ids = field_ids(spec, fields)?;
        match &ids {
            Some(fids) => self.flush_fields(spec.id, fids).await?,
            None => self.flush_model(spec.id).await?,
        }
        self.engine.readable.clear();
        self.write_gens.model_written(spec.id);
        self.cache
            .model_mut(spec.id)
            .invalidate(ids.as_deref(), None);
        self.engine
            .context_mut()
            .invalidate(spec.id, ids.as_deref(), None);
        self.access_written(spec.id, ids.as_deref());
        Ok(())
    }

    /// Drop cached values of some records (`invalidate_recordset(fnames)`), after flushing, and
    /// the memoized record-access checks whose predicates read those fields of the model.
    pub async fn invalidate_records(
        &mut self,
        records: &Records,
        fields: Option<&[&str]>,
    ) -> Result<()> {
        self.admit_records(records).await?;
        let ids = field_ids(records.spec(), fields)?;
        match &ids {
            Some(fids) => self.flush_fields(records.model_id(), fids).await?,
            None => self.flush_model(records.model_id()).await?,
        }
        self.engine.readable.clear();
        self.write_gens.model_written(records.model_id());
        self.cache
            .model_mut(records.model_id())
            .invalidate(ids.as_deref(), Some(records.rec_ids()));
        self.engine.context_mut().invalidate(
            records.model_id(),
            ids.as_deref(),
            Some(records.rec_ids()),
        );
        self.access_written(records.model_id(), ids.as_deref());
        Ok(())
    }

    /// Whether `field` is cached for every record (for tests of cache behavior); an unknown field
    /// is an error.
    pub fn is_cached(&self, records: &Records, field: &str) -> Result<bool> {
        let fid = field_id(records.spec(), field)?;
        Ok(!records.is_empty()
            && records
                .rec_ids()
                .iter()
                .all(|id| self.cache.contains(records.model_id(), fid, *id)))
    }

    /// Names of the models with at least one value in the record cache, in registry order (for
    /// tests of cache behavior, e.g. nothing is cached after a savepoint rollback).
    #[doc(hidden)]
    pub fn cached_models(&self) -> Vec<&'static str> {
        self.shared
            .registry
            .models()
            .iter()
            .filter(|m| self.cache.model(m.id).is_some_and(|c| !c.is_empty()))
            .map(|m| m.name)
            .collect()
    }

    /// The captured baseline cell for a translated field slot, if any (test seam).
    #[cfg(feature = "test-faults")]
    pub fn translated_baseline(
        &self,
        model: erp_registry::ModelId,
        fid: erp_registry::FieldId,
        id: RecId,
    ) -> Option<&Cell> {
        self.translated_baselines.get(&(model, fid, id))
    }

    /// Whether some record has a pending write for `field`; an unknown field is an error.
    pub fn is_dirty(&self, records: &Records, field: &str) -> Result<bool> {
        let fid = field_id(records.spec(), field)?;
        Ok(self
            .cache
            .model(records.model_id())
            .is_some_and(|m| records.rec_ids().iter().any(|id| m.is_dirty(fid, *id))))
    }
}

/// The outcome of attempting work within a savepoint.
#[derive(Debug)]
pub enum SavepointOutcome<T> {
    /// The work succeeded and the savepoint was released.
    Committed(T),
    /// The work inside the savepoint failed, but the savepoint was cleanly rolled back.
    /// The transaction remains usable and unpoisoned.
    RolledBack(Error),
    /// Savepoint setup (cancellation check, poison check, pre-flush, isolation, or SAVEPOINT statement) failed.
    SetupFailed(Error),
    /// Rolling back the savepoint failed. The transaction is unusable.
    RollbackFailed {
        /// The original error returned by the closure or post-closure flush, if any.
        work_error: Option<Error>,
        /// The error encountered while attempting `ROLLBACK TO SAVEPOINT`.
        rollback_error: Error,
    },
}

impl<T> SavepointOutcome<T> {
    /// Converts this outcome into a standard `Result<T>`.
    pub fn into_result(self) -> Result<T> {
        match self {
            Self::Committed(v) => Ok(v),
            Self::RolledBack(err) | Self::SetupFailed(err) => Err(err),
            Self::RollbackFailed {
                work_error,
                rollback_error,
            } => {
                if let Some(err) = work_error {
                    Err(Error::new(
                        rollback_error.kind(),
                        format!("{} (after: {})", rollback_error.message(), err.message()),
                    ))
                } else {
                    Err(rollback_error)
                }
            }
        }
    }
}

impl Transaction {
    /// Attempt `work` inside a savepoint and return the typed [`SavepointOutcome`]
    /// distinguishing clean rollback from setup/rollback failures.
    pub async fn savepoint_outcome<T, F>(&mut self, work: F) -> SavepointOutcome<T>
    where
        F: AsyncFnOnce(&mut Transaction) -> Result<T>,
    {
        if let Err(err) = self.check_not_cancelled() {
            return SavepointOutcome::SetupFailed(err);
        }
        if let Some(error) = self.poison_error() {
            return SavepointOutcome::SetupFailed(error);
        }
        if let Err(err) = self.flush().await {
            return SavepointOutcome::SetupFailed(err);
        }
        if let Err(err) = self.open_isolation(None).await {
            return SavepointOutcome::SetupFailed(err);
        }
        let name = format!("erp_sp_{}", self.savepoints.len() + 1);
        let guard = self.guard();
        if let Err(error) = self.control(&format!("SAVEPOINT \"{name}\"")).await {
            guard.disarm();
            return SavepointOutcome::SetupFailed(error);
        }
        self.savepoints.push(name.clone());
        self.savepoint_notices.push(self.notices.len());
        self.html_reports.mark();
        self.engine.ormcache.push();
        // expectations recorded inside a rolled back savepoint go with it (review SF-S6)
        let new_records_before = Arc::clone(&self.engine.new_records);
        let versions_before = self.versions.clone();
        let base_installing_before = self.base_installing;
        // The running mutation belongs outside this savepoint. Rolled-back dirty
        // cells are discarded, but its nesting guard and writer context survive.
        let capture_origin_before = self.pending_capture.active.clone();
        let outcome = match work(self).await {
            Ok(value) => self.flush().await.map(|()| value),
            Err(error) => Err(error),
        };
        let outcome = match outcome {
            Ok(value) => match self.control(&format!("RELEASE SAVEPOINT \"{name}\"")).await {
                Ok(()) => {
                    self.savepoints.retain(|n| *n != name);
                    self.savepoint_notices.truncate(self.savepoints.len());
                    self.html_reports.release(self.savepoints.len());
                    debug_assert_eq!(self.savepoint_notices.len(), self.savepoints.len());
                    self.signal_state.released_to(self.savepoints.len());
                    self.outbox.released_to(self.savepoints.len());
                    self.audit.released_to(self.savepoints.len());
                    self.cursors.released_to(self.savepoints.len());
                    for set in self.scoped_ids.values_mut() {
                        set.released_to(self.savepoints.len());
                    }
                    self.tracking.released_to(self.savepoints.len());
                    self.engine.ormcache.release();
                    guard.disarm();
                    return SavepointOutcome::Committed(value);
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        let rolled_back = self.rollback_to_savepoint(&name).await;
        guard.disarm();
        if rolled_back.is_ok() {
            self.engine.new_records = new_records_before;
            self.versions = versions_before;
            self.base_installing = base_installing_before;
            self.pending_capture.active = capture_origin_before;
        } else {
            // the rollback itself failed (the transaction is unusable): fail closed, the
            // install window is not left open by work that did not roll back
            self.base_installing = false;
        }
        match (outcome, rolled_back) {
            (Err(error), Ok(())) => {
                // the savepoint's own validation errors are rolled back with it (decision E1)
                self.poison = None;
                SavepointOutcome::RolledBack(error)
            }
            (Err(error), Err(rollback)) => SavepointOutcome::RollbackFailed {
                work_error: Some(error),
                rollback_error: rollback,
            },
            (Ok(_), Err(rollback)) => SavepointOutcome::RollbackFailed {
                work_error: None,
                rollback_error: rollback,
            },
            (Ok(value), Ok(())) => SavepointOutcome::Committed(value),
        }
    }

    /// A savepoint around `work` (`with cr.savepoint():`, ENV §4.10). Pending writes are flushed
    /// before the savepoint and again before releasing it. When `work` (or that flush) fails, the
    /// database rolls back to the savepoint and the whole record cache is cleared — values, pending
    /// writes and computations — so later reads come from the database;
    /// ormcache values and invalidations of the savepoint are discarded. On success
    /// cached values are kept.
    pub async fn savepoint<T, F>(&mut self, work: F) -> Result<T>
    where
        F: AsyncFnOnce(&mut Transaction) -> Result<T>,
    {
        self.savepoint_outcome(work).await.into_result()
    }

    /// `ROLLBACK TO SAVEPOINT`, which also ends an aborted state, then clear the cache. A failure
    /// releases the connection (the transaction is then unusable).
    async fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        self.rollback_to_savepoint_sql(name).await?;
        // checks memoized after the savepoint may rest on writes it undid, and so may a private
        // policy or a resolution read after it: rebuild before the next decision
        self.access.clear_memo();
        self.policy_after_rollback();
        // the writes undone may be any model's: streams read fresh from here on
        self.write_gens.all_written();
        self.cache.clear();
        self.pending_capture.clear_cells();
        self.pending_capture.active = None;
        self.engine.clear();
        self.engine.ormcache.rollback();
        self.active_langs = None;
        self.translated_baselines.clear();
        Ok(())
    }

    /// `ROLLBACK TO SAVEPOINT` alone (the cache is left to the caller). A failure releases the
    /// connection.
    async fn rollback_to_savepoint_sql(&mut self, name: &str) -> Result<()> {
        if let Some(pos) = self.savepoints.iter().position(|n| n == name) {
            self.savepoints.truncate(pos);
            if let Some(&keep_len) = self.savepoint_notices.get(pos) {
                self.notices.truncate(keep_len);
                self.savepoint_notices.truncate(pos);
                debug_assert_eq!(self.savepoint_notices.len(), self.savepoints.len());
            }
            self.html_reports.rollback(pos);
            self.signal_state.rolled_back_to(pos);
            self.outbox.rolled_back_to(pos);
            self.audit.rolled_back_to(pos);
            // cursors declared under the savepoint are closed by the server with it
            self.cursors.rolled_back_to(pos);
            for set in self.scoped_ids.values_mut() {
                set.rolled_back_to(pos);
            }
            self.tracking.rolled_back_to(pos);
        }
        if matches!(self.state, State::Open | State::Aborted { .. }) {
            let sql = format!("ROLLBACK TO SAVEPOINT \"{name}\"");
            self.record(&sql);
            let result = match self.conn.as_ref() {
                Some(conn) => conn.batch_execute(&sql).await,
                None => {
                    self.release();
                    return Err(released_error());
                }
            };
            match result {
                Ok(()) => {
                    // The savepoint may have undone DDL; DDL before it stays, so the flag stays
                    // set until the transaction ends.
                    self.forget_rolled_back_schema();
                    self.state = State::Open;
                }
                Err(e) => {
                    let error = map_pg_error(&self.shared.registry, e);
                    self.release();
                    return Err(Error::new(
                        error.kind(),
                        format!(
                            "ROLLBACK TO SAVEPOINT failed ({}); the transaction was released",
                            error.message()
                        ),
                    )
                    .with_sqlstate(error.sqlstate().map(str::to_owned)));
                }
            }
        } else {
            return Err(released_error());
        }
        Ok(())
    }

    /// Run `work` isolated (onchange): nothing it does survives, and the record
    /// cache is left exactly as it was before.
    ///
    /// - The cache and the engine's cached data are snapshotted (copy-on-write per model) and
    ///   restored afterwards, whatever `work` returns: values loaded, assigned or computed inside
    ///   are dropped, values and pending writes from before are back (no cold cache afterwards).
    /// - No statement is sent for the isolation itself as long as `work` only reads. Before the
    ///   first statement that changes data, takes row locks, runs DDL or opens a savepoint, a
    ///   `SAVEPOINT` is sent; the scope then ends with `ROLLBACK TO SAVEPOINT`, so nothing `work`
    ///   wrote persists (lead decision G4). Pending writes from before the scope that `work`
    ///   flushes are rolled back with it and are pending again after the restore.
    /// - Errors of `work` are returned after the restore; validation errors raised inside do not
    ///   poison the transaction. A server error on a read before any change leaves the
    ///   transaction aborted (no savepoint covers it), as anywhere else; roll back.
    /// - Commit is refused inside the scope; a rollback inside it ends the scope (an error).
    pub(crate) async fn isolated<T, F>(&mut self, work: F) -> Result<T>
    where
        F: AsyncFnOnce(&mut Transaction) -> Result<T>,
    {
        self.check_not_cancelled()?;
        if let Some(error) = self.poison_error() {
            return Err(error);
        }
        self.isolation_seq += 1;
        let name = format!("erp_iso_{}", self.isolation_seq);
        // Admission must precede cache snapshot/protection so a refused snapshot leaves no scope.
        let pending_capture = self.pending_capture.snapshot()?;
        let guard = self.guard();
        let scope = Isolated {
            name: name.clone(),
            opened: false,
            cache: self.cache.snapshot(),
            engine: self.engine.snapshot(),
            access: self.access.snapshot(),
            pending_capture,
            html_reports: self.html_reports.position(),
        };
        self.engine.ormcache.push();
        self.isolated.push(scope);
        let outcome = work(self).await;
        let ended = self.end_isolated(&name).await;
        guard.disarm();
        match (outcome, ended) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(end)) => Err(end),
            (Err(error), Err(end)) => Err(Error::new(
                end.kind(),
                format!("{} (after: {})", end.message(), error.message()),
            )),
        }
    }

    /// End the isolated scope `name`: roll its savepoint back when one was opened, then restore
    /// the cache and the engine data.
    async fn end_isolated(&mut self, name: &str) -> Result<()> {
        let Some(pos) = self.isolated.iter().position(|s| s.name == name) else {
            return Err(Error::programming(
                "the isolated scope (onchange) was ended by a rollback inside it",
            ));
        };
        // scopes are nested by construction; anything opened inside has ended already
        let scope = self.isolated.remove(pos);
        self.isolated.truncate(pos);
        if scope.opened {
            self.rollback_to_savepoint_sql(&scope.name).await?;
        }
        self.cache.restore(scope.cache)?;
        self.pending_capture = scope.pending_capture;
        // HTML reports describe values the scope admitted and discarded, whether or not its
        // savepoint was opened (the savepoint mark is taken only when it opens)
        self.html_reports.restore(scope.html_reports);
        // checks memoized inside the scope may rest on writes it discarded: back to the memo of
        // its start (a private policy or a resolution read inside it is rebuilt)
        self.access.restore(scope.access);
        self.policy_after_rollback();
        self.engine.restore(scope.engine);
        self.engine.ormcache.rollback();
        // nothing of the scope persists, so neither do its validation errors (decision E1)
        self.poison = None;
        Ok(())
    }

    /// Send the `SAVEPOINT` of every isolated scope that has none yet, outermost first, when
    /// `sql` (`None`: whatever comes next) may change something a scope must discard.
    async fn open_isolation(&mut self, sql: Option<&str>) -> Result<()> {
        if self.isolated.iter().all(|s| s.opened)
            || sql.is_some_and(|s| !self.strings_unsure && !needs_isolation(s))
        {
            return Ok(());
        }
        for i in 0..self.isolated.len() {
            if self.isolated[i].opened {
                continue;
            }
            let name = self.isolated[i].name.clone();
            self.control(&format!("SAVEPOINT \"{name}\"")).await?;
            self.isolated[i].opened = true;
            self.savepoints.push(name);
            // one notice mark per savepoint, so positions stay aligned (D-S2)
            self.savepoint_notices.push(self.notices.len());
            self.html_reports.mark();
        }
        Ok(())
    }

    /// A statement without parameters or result (savepoints), counted like any statement.
    async fn control(&mut self, sql: &str) -> Result<()> {
        self.ensure_open().await?;
        let closes = self.take_closes();
        self.record(sql);
        let (closed, result) = {
            let conn = self.conn()?;
            behind_closes!(conn, closes.as_ref(), conn.batch_execute(sql))
        };
        self.closes_sent(closes, closed)?;
        result.map_err(|e| self.statement_failed(e))
    }

    /// The state lives in the transaction object, so nothing in
    /// the database (a `SET` on a role or database) can turn it on.
    pub fn base_installing(&self) -> bool {
        self.base_installing
    }

    /// Id of the [`Orm`] that opened this transaction. Exists for the dev-only testkit; it only
    /// identifies the ORM and grants nothing.
    #[doc(hidden)]
    pub fn orm_id(&self) -> u64 {
        self.orm_id
    }

    /// Mark this transaction as loading the base module's data. Needs the [`BaseInstallGrant`] of
    /// the [`Orm`] that opened it, and a transaction opened as the `System` principal. Logged.
    ///
    /// # Errors
    ///
    /// `Access` when the grant belongs to another ORM, or the transaction was not opened as the
    /// superuser; the flag then stays as it was.
    pub fn begin_base_install(&mut self, grant: &BaseInstallGrant) -> Result<()> {
        self.refuse_effect()?;
        if grant.orm_id != self.orm_id {
            return Err(Error::access(
                "the base-install grant belongs to another Orm than the one that opened this \
                 transaction",
            ));
        }
        if self.env.principal().subject() != crate::SUPERUSER_ID {
            return Err(Error::access(format!(
                "only a transaction opened as the System principal may mark the base \
                 module as installing (this one acts as user {})",
                self.env.uid()
            )));
        }
        tracing::warn!(target: "erp::orm", "base module installing: required groups may be missing");
        self.base_installing = true;
        Ok(())
    }

    /// Unmark this transaction as loading the base module's data (always allowed).
    pub fn end_base_install(&mut self) {
        if self.refuse_effect().is_err() {
            return;
        }
        self.base_installing = false;
    }

    /// A scoped RAII guard that marks the transaction as loading base module data and clears the
    /// flag on drop, so futures dropped during an await cannot leak the flag.
    pub fn base_install_scope<'a>(
        &'a mut self,
        grant: &BaseInstallGrant,
    ) -> Result<BaseInstallGuard<'a>> {
        self.begin_base_install(grant)?;
        Ok(BaseInstallGuard { tx: self })
    }

    /// Get the transaction's cached decimal precision for `name`, if resolved or explicitly set.
    /// [`crate::Records::fields_get`] loads stored metadata lazily; this accessor does no I/O.
    pub fn get_precision(&self, name: &str) -> Option<u32> {
        self.engine.precisions.get(name).copied()
    }

    /// Explicitly override decimal precision for `name` in this transaction's cache.
    /// `fields_get` validates the 0..28 range before use. Precision-row changes and rollback
    /// discard overrides along with stored metadata; prefer configuring `decimal.precision`.
    pub fn set_precision(&mut self, name: &str, digits: u32) {
        if self.refuse_effect().is_err() {
            return;
        }
        self.engine.precisions.insert(name.to_owned(), digits);
    }

    /// Call a registered selection ondelete callback.
    ///
    /// # Cancellation
    ///
    /// Dropping the future (or a panic unwinding it) ends the call where it stands; the
    /// transaction's environment is restored by a drop guard either way. Work the callback already
    /// did in the transaction stays; the caller decides whether to roll back.
    pub async fn call_selection_callback(
        &mut self,
        name: &str,
        model: &str,
        ids: &[erp_fields::RecId],
    ) -> Result<()> {
        self.refuse_effect()?;
        self.require_system_full("Transaction::call_selection_callback")?;
        self.dispatch_selection_callback(name, model, ids, None)
            .await
    }

    /// Call a registered selection ondelete callback with an active company context.
    ///
    /// # Cancellation
    ///
    /// As [`Transaction::call_selection_callback`]: the company switch is undone by a drop guard
    /// when the future completes, is dropped while pending, or unwinds, so a cancelled call never
    /// leaves the transaction in `company`.
    pub async fn call_selection_callback_in_company(
        &mut self,
        name: &str,
        model: &str,
        ids: &[erp_fields::RecId],
        company: Option<i64>,
    ) -> Result<()> {
        self.refuse_effect()?;
        self.require_system_full("Transaction::call_selection_callback_in_company")?;
        self.dispatch_selection_callback(name, model, ids, company)
            .await
    }

    /// Resolve static selection labels for a field in the given language or context language.
    pub async fn selection_labels(
        &mut self,
        model: &str,
        field: &str,
    ) -> Result<Vec<(String, String)>> {
        let lang = self.env().lang().unwrap_or("en_US").to_string();
        self.selection_labels_for_lang(model, field, &lang).await
    }

    /// Resolve static selection labels for a field in an explicit language.
    pub async fn selection_labels_for_lang(
        &mut self,
        model: &str,
        field: &str,
        lang: &str,
    ) -> Result<Vec<(String, String)>> {
        let registry = Arc::clone(self.env().registry());
        let spec = registry
            .model(model)
            .ok_or_else(|| Error::missing(format!("model '{model}' not found in registry")))?;
        let fld = spec
            .field(field)
            .ok_or_else(|| Error::missing(format!("model '{model}' has no field '{field}'")))?;
        let values = match &fld.ty {
            erp_fields::FieldType::Selection {
                values: Some(values),
                ..
            } => values.clone(),
            _ => {
                return Err(Error::programming(format!(
                    "field '{model}.{field}' is not a static selection"
                )));
            }
        };
        if lang == "en_US" || registry.model("ir.model.fields.selection").is_none() {
            return Ok(values);
        }
        let rows = self
            .query(
                r#"
                SELECT s.value, s.name->>$3
                FROM ir_model_fields_selection s
                JOIN ir_model_fields f ON f.id = s.field_id
                WHERE f.model = $1 AND f.name = $2 AND s.name ? $3
                "#,
                &[&model, &field, &lang],
            )
            .await?;
        // A failed lookup or a row that does not decode is an error, never "no translation".
        let column = |error: tokio_postgres::Error| {
            Error::db("Invalid translated selection label row").with_source(error)
        };
        let mut translations = std::collections::HashMap::new();
        for row in rows {
            let value: String = row.try_get(0).map_err(column)?;
            if let Some(label) = row.try_get::<_, Option<String>>(1).map_err(column)? {
                translations.insert(value, label);
            }
        }
        Ok(values
            .into_iter()
            .map(|(k, l)| {
                let label = translations.remove(&k).unwrap_or(l);
                (k, label)
            })
            .collect())
    }

    async fn dispatch_selection_callback(
        &mut self,
        name: &str,
        model: &str,
        ids: &[erp_fields::RecId],
        company: Option<i64>,
    ) -> Result<()> {
        let callback = self
            .shared
            .selection_callbacks
            .iter()
            .find(|cb| cb.name() == name)
            .copied();
        let Some(cb) = callback else {
            return Err(Error::programming(format!(
                "selection ondelete callback '{name}' is not registered on the ORM"
            )));
        };

        // The scope restores the environment in `Drop`, so a dropped (cancelled) or unwound
        // callback future cannot leave the company switched on this transaction (LOGIN-R4).
        let mut scope = CompanyEnvScope::enter(self, company)?;
        cb.call(&mut scope, model, ids).await
    }
}

/// Restores the transaction's environment when dropped: the company switch of a selection
/// callback ends with the call, however the call ends (completed, error, dropped while pending, or
/// unwound by a panic).
struct CompanyEnvScope<'a> {
    tx: &'a mut Transaction,
    previous: Option<Env>,
}

impl<'a> CompanyEnvScope<'a> {
    /// Switch `tx` to `company` (when given); the previous environment returns on drop.
    fn enter(tx: &'a mut Transaction, company: Option<i64>) -> Result<Self> {
        let previous = tx.env.clone();
        if let Some(cid) = company {
            tx.env = previous.with_company(cid)?;
        }
        Ok(Self {
            tx,
            previous: Some(previous),
        })
    }
}

impl std::ops::Deref for CompanyEnvScope<'_> {
    type Target = Transaction;

    fn deref(&self) -> &Self::Target {
        self.tx
    }
}

impl std::ops::DerefMut for CompanyEnvScope<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.tx
    }
}

impl Drop for CompanyEnvScope<'_> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            self.tx.env = previous;
        }
    }
}

/// An RAII guard from [`Transaction::base_install_scope`]: clears `base_installing` on drop.
#[must_use = "dropping BaseInstallGuard immediately ends the base install scope"]
pub struct BaseInstallGuard<'a> {
    tx: &'a mut Transaction,
}

impl std::fmt::Debug for BaseInstallGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseInstallGuard")
            .field("base_installing", &self.tx.base_installing())
            .finish()
    }
}

impl<'a> std::ops::Deref for BaseInstallGuard<'a> {
    type Target = Transaction;

    fn deref(&self) -> &Self::Target {
        self.tx
    }
}

impl<'a> std::ops::DerefMut for BaseInstallGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.tx
    }
}

impl Drop for BaseInstallGuard<'_> {
    fn drop(&mut self) {
        self.tx.end_base_install();
    }
}

/// An open isolated scope ([`Transaction::isolated`]).
struct Isolated {
    /// Name of its savepoint, sent lazily.
    name: String,
    /// Whether the savepoint was sent.
    opened: bool,
    cache: crate::cache::SnapshotId,
    pending_capture: crate::outbox::Pending,
    engine: crate::engine::EngineSnapshot,
    /// The memoized record checks when the scope opened (they rest on the state the scope
    /// returns to).
    access: crate::access::MemoSnapshot,
    /// HTML report position when the scope opened.
    html_reports: (usize, usize),
}

/// Whether a statement run inside an isolated scope needs the scope's savepoint first (review H1:
/// an allow-list, not a deny-list). Only statements that cannot change anything skip it: `SELECT`,
/// `WITH`, `SHOW` and `EXPLAIN` without `ANALYZE`, containing no data-modifying keyword
/// (`INSERT`, `UPDATE`, `DELETE`, `MERGE`, `TRUNCATE`, `COPY`) and no row-lock clause (`FOR UPDATE`,
/// `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE`), judged on the words outside comments and
/// string literals, case and whitespace normalised. Everything else (DDL, `TRUNCATE`, `COPY`,
/// `LOCK`, `CALL`, `DO`, `REFRESH`, savepoints, unknown statements) opens it. A false positive only
/// costs two statements.
///
/// Not detectable: a `SELECT` that calls a function which writes (`SELECT my_writing_fn()`). The
/// ORM never sends one; addon code that calls such a function inside an onchange must not rely on
/// the isolation and should send a statement the allow-list refuses.
///
/// A statement the word scanner does not read with certainty (`sql_words`: a `U&` string or name,
/// a backslash outside an `E''` string, unterminated input) needs it (round 4, fail closed), and so
/// does any `INTO` (round 5, L1: `SELECT … INTO` creates a table that outlives the scope).
fn needs_isolation(sql: &str) -> bool {
    let is = |w: &str, k: &str| w.eq_ignore_ascii_case(k);
    let mut words = sql_words::SqlWords::new(sql);
    let Some(first) = words.next() else {
        return true;
    };
    let first = if first.quoted { "" } else { first.text };
    let explain = is(first, "EXPLAIN");
    if !(explain || is(first, "SELECT") || is(first, "WITH") || is(first, "SHOW")) {
        return true;
    }
    let mut after_for = false;
    for word in words.by_ref() {
        let w = word.text;
        if explain && (is(w, "ANALYZE") || is(w, "ANALYSE")) {
            return true;
        }
        if [
            "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "COPY", "INTO",
        ]
        .iter()
        .any(|k| is(w, k))
        {
            return true;
        }
        if after_for && ["UPDATE", "NO", "SHARE", "KEY"].iter().any(|k| is(w, k)) {
            return true;
        }
        after_for = is(w, "FOR");
    }
    words.uncertain()
}

fn released_error() -> Error {
    Error::programming(
        "the transaction can no longer be used: its connection was released after a failure or a \
         dropped operation",
    )
}

/// A driver error that means the connection is gone (closed, an I/O failure, or the server
/// shutting the session down) rather than a statement the server refused.
fn connection_failed(e: &tokio_postgres::Error) -> bool {
    use std::error::Error as _;
    match e.as_db_error() {
        // 57P01 admin_shutdown, 57P02 crash_shutdown, 57P03 cannot_connect_now
        Some(db) => db.code().code().starts_with("57P"),
        None => e.is_closed() || e.source().is_some_and(|s| s.is::<std::io::Error>()),
    }
}

/// A read-pool connection that left its transaction's care: dropped while `in_transaction`, it is
/// detached from the pool (closing it rolls the server transaction back) instead of returning.
struct ReadConn {
    conn: Option<Object>,
    in_transaction: bool,
}

impl Drop for ReadConn {
    fn drop(&mut self) {
        if self.in_transaction
            && let Some(conn) = self.conn.take()
        {
            drop(Object::take(conn));
        }
    }
}

/// The error of a failed pool checkout: the pool's refusal of a connection whose settings read
/// strings differently ([`crate::pool::UnsupportedSessionSetting`]) or of an unsupported server
/// version ([`crate::pool::UnsupportedPostgresError`]) when that is the cause, else a connection
/// error; the pool's error is the source either way. If the version hook's prefix was present but
/// its payload was malformed, the parse error is preserved as the source.
pub(crate) fn checkout_error(error: deadpool_postgres::PoolError) -> Error {
    // a connection that reads strings differently from the ORM (review round 5, M4)
    if let Some(message) = crate::pool::UnsupportedSessionSetting::from_pool_error(&error) {
        return Error::new(ErrorKind::Database, message.to_owned()).with_source(error);
    }
    match crate::pool::UnsupportedPostgresError::from_pool_error(&error) {
        Ok(Some(unsupported)) => {
            Error::new(ErrorKind::Database, unsupported.to_string()).with_source(error)
        }
        Err(parse_err) => {
            tracing::error!(
                target: "erp::orm",
                error = %parse_err,
                "malformed hook payload in connection pool error"
            );
            Error::new(
                ErrorKind::Database,
                format!("malformed PostgreSQL version hook payload: {parse_err}"),
            )
            .with_source(parse_err)
        }
        Ok(None) => Error::new(ErrorKind::Database, "could not get a database connection")
            .with_source(error),
    }
}

fn field_id(spec: &ModelSpec, name: &str) -> Result<erp_registry::FieldId> {
    spec.field_id(name)
        .ok_or_else(|| Error::programming(format!("Invalid field '{name}' on '{}'", spec.name)))
}

fn field_ids(
    spec: &ModelSpec,
    fields: Option<&[&str]>,
) -> Result<Option<Vec<erp_registry::FieldId>>> {
    fields
        .map(|names| names.iter().map(|n| field_id(spec, n)).collect())
        .transpose()
}

impl Drop for Transaction {
    fn drop(&mut self) {
        // Beginning, Open or Aborted: the connection may be inside a transaction and must not go
        // back to the pool. Closing it rolls the transaction back on the server.
        if matches!(
            self.state,
            State::Beginning | State::Open | State::Aborted { .. }
        ) && let Some(conn) = self.conn.take()
        {
            drop(Object::take(conn));
        }
        // session state set by code outside the ORM, or a string reading the server reported
        // changed, must not outlive the transaction
        if (self.connection_tainted || self.string_reading_moved())
            && let Some(conn) = self.conn.take()
        {
            drop(Object::take(conn));
        }
    }
}

/// The positions in `tables` of the followed tables `sql` may delete from
/// ([`Transaction::follow_sql_deletes`]): none when the statement has no unquoted `DELETE`; the
/// tables it names otherwise, an unquoted name in any case (PostgreSQL folds it), a quoted one
/// exactly (review C3-R2); every table when the word scanner does not read it with certainty.
fn sql_deleted_tables<E>(sql: &str, tables: &[(String, E)]) -> Vec<usize> {
    let mut words = sql_words::SqlWords::new(sql);
    let mut delete = false;
    let mut hit: Vec<usize> = Vec::new();
    for word in words.by_ref() {
        if !word.quoted && word.text.eq_ignore_ascii_case("DELETE") {
            delete = true;
        }
        if let Some(at) = tables.iter().position(|(table, _)| {
            if word.quoted {
                table == word.text
            } else {
                table.eq_ignore_ascii_case(word.text)
            }
        }) && !hit.contains(&at)
        {
            hit.push(at);
        }
    }
    if words.uncertain() {
        return (0..tables.len()).collect();
    }
    if !delete {
        hit.clear();
    }
    hit
}

#[cfg(test)]
mod follow_delete_scan_tests {
    use super::sql_deleted_tables;

    /// Review C3-R2: the table names a `DELETE` gives are read as PostgreSQL reads them.
    #[test]
    fn deleted_tables_are_read_as_postgresql_folds_them() {
        let tables = [
            ("ir_act_url".to_owned(), ()),
            ("ir_act_server".to_owned(), ()),
        ];
        let cases: [(&str, &[usize]); 8] = [
            ("DELETE FROM ir_act_url WHERE id = $1", &[0]),
            ("DELETE FROM IR_ACT_SERVER WHERE id = $1", &[1]),
            ("delete from Ir_Act_Url", &[0]),
            ("DELETE FROM \"ir_act_server\" WHERE id = $1", &[1]),
            // a quoted name keeps its case: another table than ir_act_url
            ("DELETE FROM \"IR_ACT_URL\" WHERE id = $1", &[]),
            (
                "WITH d AS (DELETE FROM ir_act_url RETURNING id) SELECT id FROM d",
                &[0],
            ),
            ("SELECT id FROM ir_act_url", &[]),
            ("DELETE FROM res_partner", &[]),
        ];
        for (sql, expected) in cases {
            assert_eq!(sql_deleted_tables(sql, &tables), expected, "{sql}");
        }
        // a statement the scanner cannot read with certainty names every table
        assert_eq!(
            sql_deleted_tables("SELECT $q$ unterminated", &tables),
            [0, 1]
        );
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::needs_isolation;

    /// Review H1: only read-only statements skip the isolated scope's savepoint; anything else
    /// (DDL, TRUNCATE, COPY, LOCK, CALL, DO, data-modifying WITH, locking SELECT however it is
    /// spelled) opens it.
    #[test]
    fn only_read_only_statements_skip_the_savepoint() {
        for sql in [
            "SELECT \"t\".\"id\" FROM \"t\" WHERE \"t\".\"id\" = ANY($1::int8[])",
            "  select 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SHOW TimeZone",
            "EXPLAIN SELECT 1",
            "-- a comment\nSELECT 1",
            "/* c */ SELECT 1",
            "SELECT 'for update' AS label",
        ] {
            assert!(!needs_isolation(sql), "{sql}");
        }
        for sql in [
            "TRUNCATE t",
            "CALL p()",
            "COPY t FROM STDIN",
            "LOCK TABLE t",
            "REFRESH MATERIALIZED VIEW v",
            "CREATE TABLE x (a int)",
            "DO $$ BEGIN END $$",
            "SAVEPOINT a",
            "INSERT INTO t VALUES (1)",
            "  update t SET a = 1",
            "WITH d AS (DELETE FROM t RETURNING id) SELECT * FROM d",
            "with u as (update t set a = 1 returning id) select 1",
            "SELECT id FROM t WHERE id = 1\nFOR UPDATE",
            "SELECT id FROM t\tFOR\n NO   KEY UPDATE SKIP LOCKED",
            "select id from t for share",
            "SELECT id FROM t FOR KEY SHARE",
            "EXPLAIN ANALYZE DELETE FROM t",
            "EXPLAIN (ANALYZE) SELECT 1",
            "-- SELECT\nDELETE FROM t",
            "",
        ] {
            assert!(needs_isolation(sql), "{sql:?}");
        }
    }

    /// Review round 3, finding 1: what the word scanner once misread, hiding a data-modifying
    /// `WITH` or a session-state function: a quoted identifier holding `'`, `--` or `/*`, an
    /// `E''` string with a backslash-escaped quote, nested block comments, dollar tags with
    /// digits.
    #[test]
    fn quoted_names_escapes_nested_comments_and_dollar_tags_hide_nothing() {
        for sql in [
            r#"WITH x AS (SELECT 1 AS "a'b"), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d"#,
            r#"WITH x AS (SELECT 1 AS "a--b"), d AS (DELETE FROM t RETURNING 1) SELECT 1 FROM d"#,
            r#"WITH x AS (SELECT 1 AS "a/*b"), d AS (DELETE FROM t RETURNING 1) SELECT 1 FROM d */"#,
            r#"WITH x AS (SELECT 1 AS "a""'b"), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d"#,
            r"WITH x AS (SELECT E'\'' AS a), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d",
            "WITH x AS (SELECT 1 /* /* */ ' */), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d",
            "WITH x AS (SELECT $a1$ ' $a1$ AS v), d AS (DELETE FROM t RETURNING 1) SELECT 'z' FROM d",
            r#"SELECT "a'b" FROM t FOR UPDATE --'"#,
        ] {
            assert!(needs_isolation(sql), "{sql}");
        }
        for (sql, called) in [
            (
                r#"SELECT "a'b", pg_advisory_lock(1), 'x'"#,
                "pg_advisory_lock",
            ),
            (r"SELECT E'\'', pg_notify('c', 'p'), 'x'", "pg_notify"),
            (
                "SELECT /* /* */ ' */ set_config('a', 'b', false), 'x'",
                "set_config",
            ),
            (
                "SELECT $q1$ ' $q1$, pg_advisory_xact_lock(1), 'x'",
                "pg_advisory_xact_lock",
            ),
            (
                r#"SELECT pg_catalog."pg_advisory_lock"(1)"#,
                "pg_advisory_lock",
            ),
        ] {
            assert_eq!(super::session_state_function(sql), Some(called), "{sql}");
        }
        // what the scanner skips stays skipped
        for sql in [
            r"SELECT E'DELETE \' INSERT' AS a",
            "SELECT 1 /* outer /* DELETE */ UPDATE */",
            "SELECT $t2$ DELETE $t2$",
            "SELECT 'it''s DELETE'",
        ] {
            assert!(!needs_isolation(sql), "{sql}");
        }
    }

    /// Review round 4: `--` ends at `\r`; PostgreSQL's identifier class (`€$q$`, `a$$b$$` and
    /// `€$$` are names, not dollar quotes); what the scanner does not read with certainty (`U&`
    /// strings and names, backslashes outside `E''`, unterminated text) needs the savepoint, and
    /// the read guard refuses it before looking for a function name.
    #[test]
    fn carriage_returns_identifier_classes_and_uncertain_statements() {
        for sql in [
            "WITH x AS (SELECT 1) --\rDELETE FROM t",
            "WITH €$q$ AS (SELECT 1), d AS (DELETE FROM t RETURNING 1) SELECT 1 AS z$q$ FROM d",
            "WITH €$$ AS (SELECT 1), d AS (DELETE FROM t RETURNING 1) SELECT 1 AS z$$ FROM d",
            "SELECT 1 AS a$$b$$, 2 FROM t FOR UPDATE",
            r#"SELECT U&"d\0061ta" FROM t"#,
            r"SELECT u&'d\0061ta'",
            r"SELECT 'a\' AS x",
            r"SELECT 1 \ 2",
            "SELECT 'open",
            "SELECT 1 /* open",
        ] {
            assert!(needs_isolation(sql), "{sql:?}");
        }
        for (sql, called) in [
            ("SELECT 1 --\r, pg_advisory_lock(1)", "pg_advisory_lock"),
            (
                "SELECT 1 AS €$q$, pg_advisory_lock(1) AS z$q$",
                "pg_advisory_lock",
            ),
            ("SELECT 1 AS a$$b$$, pg_notify('c', 'p')", "pg_notify"),
        ] {
            assert_eq!(super::session_state_function(sql), Some(called), "{sql:?}");
        }
        // a `U&` name hides the function from the guard: the scanner calls it uncertain, which
        // the read guard refuses first
        let escaped = r#"SELECT U&"pg\005fadvisory_lock"(1)"#;
        assert!(!super::sql_words::certain(escaped));
        for sql in [
            "SELECT 1 -- a comment\nFROM t",
            r"SELECT E'a\\b' FROM t",
            "SELECT \"t\".\"id\" FROM \"t\" WHERE \"t\".\"id\" = ANY($1::int8[])",
        ] {
            assert!(!needs_isolation(sql), "{sql:?}");
            assert!(super::sql_words::certain(sql), "{sql:?}");
        }
    }

    /// Review round 5, L1 and L2: `SELECT … INTO` creates a table that outlives an onchange scope;
    /// the guard names the functions that reach outside the session (`dblink…`), end other
    /// sessions or release a session lock held earlier on the pooled connection; a quoted
    /// `EXPLAIN` option is the option (M2).
    #[test]
    fn select_into_and_more_session_functions() {
        for sql in [
            "SELECT * INTO t2 FROM t",
            "SELECT 1 INTO TEMP x",
            "select a into unlogged t3 from t",
            "WITH a AS (SELECT 1) SELECT * INTO t2 FROM a",
            "EXPLAIN (\"analyze\") SELECT 1",
        ] {
            assert!(needs_isolation(sql), "{sql:?}");
        }
        assert!(!needs_isolation("SELECT 'into' FROM t"));
        for (sql, called) in [
            ("SELECT dblink_exec('c', 'DELETE FROM t')", "dblink_exec"),
            (
                "SELECT * FROM DBLINK('c', 'SELECT 1') AS x(a int)",
                "DBLINK",
            ),
            (
                "SELECT dblink_connect_u('c', 'dbname=x')",
                "dblink_connect_u",
            ),
            ("SELECT pg_terminate_backend(42)", "pg_terminate_backend"),
            ("SELECT pg_cancel_backend(42)", "pg_cancel_backend"),
            ("SELECT pg_advisory_unlock(1)", "pg_advisory_unlock"),
            (
                "SELECT PG_ADVISORY_UNLOCK_SHARED(1)",
                "pg_advisory_unlock_shared",
            ),
            ("SELECT pg_advisory_unlock_all()", "pg_advisory_unlock_all"),
        ] {
            assert_eq!(super::session_state_function(sql), Some(called), "{sql:?}");
        }
        for sql in [
            "SELECT dblinks FROM t",
            "SELECT pg_backend_pid()",
            "SELECT 1",
        ] {
            assert_eq!(super::session_state_function(sql), None, "{sql:?}");
        }
    }
}

#[cfg(test)]
mod seen_epoch_tests {
    use super::*;

    fn dead(epoch: u64) -> SeenEpoch {
        SeenEpoch {
            cache: std::sync::Weak::new(),
            epoch,
        }
    }

    /// Entries of closed connections are removed once the map is large, and only then (a small
    /// map is left alone: lookups on the begin path stay O(1)).
    #[test]
    fn dead_entries_are_pruned_past_the_bound() {
        let mut seen: FxHashMap<usize, SeenEpoch> = FxHashMap::default();
        for key in 0..SEEN_EPOCHS_PRUNE_AT - 1 {
            seen.insert(key, dead(1));
        }
        DatabaseIdentity::prune(&mut seen);
        assert_eq!(seen.len(), SEEN_EPOCHS_PRUNE_AT - 1, "below the bound");
        seen.insert(SEEN_EPOCHS_PRUNE_AT, dead(2));
        DatabaseIdentity::prune(&mut seen);
        assert!(seen.is_empty(), "every connection of these entries is gone");
    }
}

#[cfg(test)]
mod begin_isolation_tests {
    use super::{BeginOptions, Isolation};
    use crate::SUPERUSER_ID;

    /// R1 db H1: the isolation level of `BEGIN` follows `BeginOptions::isolation`, stays
    /// REPEATABLE READ by default and combines with read-only.
    #[test]
    fn begin_statement_follows_the_isolation_option() {
        let base = BeginOptions::new(SUPERUSER_ID);
        assert_eq!(base.isolation_level(), Isolation::RepeatableRead);
        assert_eq!(base.begin_sql(), "BEGIN ISOLATION LEVEL REPEATABLE READ");
        assert_eq!(
            base.clone().read_only("m.read").begin_sql(),
            "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY"
        );
        let rc = base.isolation(Isolation::ReadCommitted);
        assert_eq!(rc.begin_sql(), "BEGIN ISOLATION LEVEL READ COMMITTED");
        assert_eq!(
            rc.clone().read_only("m.read").begin_sql(),
            "BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY"
        );
        assert_eq!(
            rc.isolation(Isolation::RepeatableRead).begin_sql(),
            "BEGIN ISOLATION LEVEL REPEATABLE READ"
        );
    }
}

#[cfg(test)]
mod lifecycle_actor_tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]
    use super::*;
    use crate::{
        ApiKeyId, GroupSet, MethodAllow, ModelAllow, PrincipalBuilder, Resolution, Scope, UserId,
    };
    use std::num::NonZeroU64;

    #[tokio::test]
    async fn scoped_api_key_cannot_bind_as_a_human_lifecycle_actor() {
        let registry = Arc::new(
            erp_registry::RegistryBuilder::new([&erp_registry::BASE_ADDON])
                .build()
                .unwrap(),
        );
        let config = std::env::var("DATABASE_URL").unwrap().parse().unwrap();
        let pool = crate::pool::create_pool(config, 1).unwrap();
        let orm = Orm::builder(registry, pool.clone()).build().unwrap();
        let mut tx = orm.begin(crate::SUPERUSER_ID).await.unwrap();
        // Unit fixture supplies a matching private control without claiming the process-wide
        // module-engine OperationKey, which other unit tests may also need.
        let control = OperationControl { id: 1 };
        tx.operation = Some(1);
        let scope = Scope::builder(ApiKeyId::new(1).unwrap(), "restricted key")
            .models(ModelAllow::All)
            .methods(MethodAllow::All)
            .read_only(true)
            .build()
            .unwrap();
        let principal = PrincipalBuilder::user(UserId::new(2).unwrap())
            .resolution(Resolution::in_id_order(GroupSet::default(), [], NonZeroU64::MIN).unwrap())
            .scope(scope)
            .build()
            .unwrap();
        let error = tx.operation_bind_actor(&control, principal).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "A lifecycle actor must be a resolved, unelevated session user"
        );
        assert!(tx.env().principal().actor().is_none());
        assert_eq!(tx.query_count(), 0, "shape denial must send no SQL");
        assert!(tx.is_idle());
        tx.operation = None; // Remove the unit fixture control before normal cleanup.
        tx.rollback().await.unwrap();
    }
}
