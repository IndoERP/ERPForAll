//! Engine, store and trap translation (plan §4 runtime realities, §6.5 sticky traps).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use wasmtime::component::{Component, Instance, Linker, Val};
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, ResourceLimiter, Store,
    Trap,
};

use crate::bindings::erpforall::addon::orm_host::{self, LogLevel};
use crate::bindings::erpforall::addon::types::{
    self, BoundaryError, BoundaryErrorKind, ColumnVector, RecordBatch, ScalarValue,
};
use crate::bridge::{OrmBridge, boundary_error};
use crate::cache::{ModuleCache, ModuleKey};
use crate::error::{HostError, LimitKind, TrapKind};
use crate::limits::Limits;

/// Length of one epoch tick. CPU deadlines are rounded up to whole ticks.
const EPOCH_TICK: Duration = Duration::from_millis(5);
/// Slots of the pooling allocator (concurrent component instances).
const POOL_SLOTS: u32 = 64;
/// Per-store caps on core instances, memories and tables (a store's component may use several).
const MAX_INSTANCES_PER_STORE: usize = 8;
const MAX_MEMORIES_PER_STORE: usize = 4;
const MAX_TABLES_PER_STORE: usize = 8;
/// Largest table a guest may grow to, in elements.
const MAX_TABLE_ELEMENTS: usize = 100_000;

/// WASI interfaces the Rust standard library of `wasm32-wasip2` imports for a component that only
/// computes: process environment and exit, the standard streams (logging), clocks and randomness.
/// The host answers the wall clock with the real time and every other one deny-by-default (a trap)
/// when the guest calls it; this list is what
/// may be *imported*. Files, sockets and HTTP are not on it.
const WASI_ALLOWED: &[&str] = &[
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:cli/terminal-stderr",
    "wasi:io/error",
    "wasi:io/poll",
    "wasi:io/streams",
    "wasi:clocks/monotonic-clock",
    "wasi:clocks/wall-clock",
    "wasi:random/random",
    "wasi:random/insecure",
    "wasi:random/insecure-seed",
];

/// Whether a component may import `name` (an interface name such as `wasi:cli/exit@0.2.6`): the
/// host's own `erpforall:addon/...` interfaces and the [`WASI_ALLOWED`] ones, any version.
#[must_use]
pub fn import_allowed(name: &str) -> bool {
    let interface = name.split('@').next().unwrap_or(name);
    interface.starts_with("erpforall:addon/") || WASI_ALLOWED.contains(&interface)
}

/// Prefix of the wall-clock interface names (`wasi:clocks/wall-clock@0.2.6`, whatever version the
/// guest toolchain imports). The linker matches names exactly when it fills in unknown imports, so
/// the clock is defined under each version the component names (see [`add_wall_clock`]).
const WALL_CLOCK_PREFIX: &str = "wasi:clocks/wall-clock@";
/// Granularity the wall clock is reported with: whole microseconds, the unit of the ORM's datetime
/// values. A coarser clock than the host's gives a guest less to time other work with.
const WALL_CLOCK_RESOLUTION_NANOS: u32 = 1_000;

/// Answers the `wasi:clocks/wall-clock` imports of `component` (`now` and `resolution`) with the
/// host's real system clock (amendment 2026-10-28). The result is a record of seconds
/// and nanoseconds since 1970-01-01 UTC. Every other WASI import stays deny-by-default (a trap
/// when called). Call it once per linker, before `define_unknown_imports_as_traps`.
fn add_wall_clock(
    linker: &mut Linker<HostState>,
    engine: &Engine,
    component: &Component,
) -> Result<(), HostError> {
    let config = |e: wasmtime::Error| HostError::Config(format!("wall-clock linker: {e:#}"));
    let names: Vec<String> = component
        .component_type()
        .imports(engine)
        .map(|(name, _)| name)
        .filter(|name| name.starts_with(WALL_CLOCK_PREFIX))
        .map(str::to_owned)
        .collect();
    for name in names {
        let mut clock = linker.instance(&name).map_err(config)?;
        clock
            .func_new("now", |_store, _ty, _args, results| {
                let since_epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| {
                        wasmtime::Error::msg(format!("the host clock is before 1970: {e}"))
                    })?;
                let nanos = since_epoch.subsec_nanos() / WALL_CLOCK_RESOLUTION_NANOS
                    * WALL_CLOCK_RESOLUTION_NANOS;
                set_datetime(results, since_epoch.as_secs(), nanos)
            })
            .map_err(config)?;
        clock
            .func_new("resolution", |_store, _ty, _args, results| {
                set_datetime(results, 0, WALL_CLOCK_RESOLUTION_NANOS)
            })
            .map_err(config)?;
    }
    Ok(())
}

/// Writes a WASI `datetime` record into the single result slot.
fn set_datetime(results: &mut [Val], seconds: u64, nanoseconds: u32) -> wasmtime::Result<()> {
    let slot = results
        .first_mut()
        .ok_or_else(|| wasmtime::Error::msg("wall-clock function has no result slot"))?;
    *slot = Val::Record(vec![
        ("seconds".to_owned(), Val::U64(seconds)),
        ("nanoseconds".to_owned(), Val::U32(nanoseconds)),
    ]);
    Ok(())
}

/// The epoch ticker thread. Shared (`Arc`) by the host and every store, so it runs for as long as
/// any store can still execute guest code: a store outliving its host keeps its CPU deadline.
#[derive(Debug)]
struct Ticker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Ticker {
    fn start(engine: &Engine) -> Result<Self, HostError> {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (engine, stop) = (engine.clone(), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("erp-wasm-epoch".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(EPOCH_TICK);
                        engine.increment_epoch();
                    }
                })
                .map_err(|e| HostError::Config(format!("epoch ticker: {e}")))?
        };
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// Whether the thread still runs. A dead ticker would silently disable every CPU deadline.
    fn alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // A join error means the ticker panicked; `alive()` already refused calls meanwhile.
            let _ = handle.join();
        }
    }
}

/// Process-wide WASM runtime: one engine, the compiled-component cache and the epoch ticker.
///
/// The CPU limit is a wall-clock budget counted in epoch ticks of 5 ms, so a call can be stopped up
/// to one tick early or late, and time spent waiting in asynchronous host calls counts.
pub struct WasmHost {
    engine: Engine,
    cache: ModuleCache,
    ticker: Arc<Ticker>,
    /// Linker with the `orm-host` imports every addon store shares.
    linker: Linker<HostState>,
}

impl std::fmt::Debug for WasmHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmHost")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

impl WasmHost {
    /// Builds the engine (component model, async calls, epoch interruption, pooling allocator) and
    /// starts the epoch ticker thread.
    pub fn new() -> Result<Self, HostError> {
        let mut config = Config::new();
        config.wasm_component_model(true).epoch_interruption(true);
        let mut pool = PoolingAllocationConfig::default();
        // Engine-wide slots, and per-component caps so one component cannot take the whole pool.
        pool.total_component_instances(POOL_SLOTS)
            .total_core_instances(POOL_SLOTS)
            .total_memories(POOL_SLOTS)
            .total_tables(POOL_SLOTS)
            .total_stacks(POOL_SLOTS)
            .max_core_instances_per_component(MAX_INSTANCES_PER_STORE as u32)
            .max_memories_per_module(MAX_MEMORIES_PER_STORE as u32)
            .max_memories_per_component(MAX_MEMORIES_PER_STORE as u32)
            .max_tables_per_component(MAX_TABLES_PER_STORE as u32)
            .table_elements(MAX_TABLE_ELEMENTS)
            .max_tables_per_module(MAX_TABLES_PER_STORE as u32)
            .max_memory_size(Limits::MAX_MEMORY_BYTES);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        let engine = Engine::new(&config).map_err(|e| HostError::Config(format!("{e:#}")))?;
        let ticker = Arc::new(Ticker::start(&engine)?);
        let mut linker = Linker::new(&engine);
        crate::bindings::Addon::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |state| state,
        )
        .map_err(|e| HostError::Config(format!("orm-host linker: {e:#}")))?;
        Ok(Self {
            engine,
            cache: ModuleCache::default(),
            ticker,
            linker,
        })
    }

    /// The compiled-component cache.
    #[must_use]
    pub fn cache(&self) -> &ModuleCache {
        &self.cache
    }

    /// Compiles `bytes` (binary component) or returns the cached component for the same
    /// `(sha256, contract version, cpu)`.
    ///
    /// # Errors
    ///
    /// `HostError::ForbiddenImport` when the component imports an interface outside the allowlist
    /// ([`import_allowed`]); the check runs at load time, also for a cached component.
    pub fn component(&self, bytes: &[u8]) -> Result<Component, HostError> {
        let key = ModuleKey::for_bytes(&self.engine, bytes);
        let component = self.cache.get_or_compile(&self.engine, &key, bytes)?;
        self.check_imports(&component)?;
        Ok(component)
    }

    /// Refuses a component that imports an interface the host does not grant.
    fn check_imports(&self, component: &Component) -> Result<(), HostError> {
        for (name, _) in component.component_type().imports(&self.engine) {
            if !import_allowed(name) {
                return Err(HostError::ForbiddenImport {
                    name: name.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Cache key of `bytes` on this host.
    #[must_use]
    pub fn key_for(&self, bytes: &[u8]) -> ModuleKey {
        ModuleKey::for_bytes(&self.engine, bytes)
    }

    /// Cache key of `bytes` for another contract version.
    #[must_use]
    pub fn key_for_version(&self, bytes: &[u8], wit_version: &str) -> ModuleKey {
        ModuleKey::with_version(&self.engine, bytes, wit_version)
    }

    /// Like [`WasmHost::component`] with an explicit key (for example another contract version).
    pub fn component_with_key(
        &self,
        key: &ModuleKey,
        bytes: &[u8],
    ) -> Result<Component, HostError> {
        let component = self.cache.get_or_compile(&self.engine, key, bytes)?;
        self.check_imports(&component)?;
        Ok(component)
    }

    /// A fresh store for one (transaction, addon) pair under `limits`.
    pub fn store(&self, component: &Component, limits: Limits) -> AddonStore {
        let mut store = Store::new(&self.engine, HostState::new(limits));
        store.limiter(|state| state);
        AddonStore {
            store,
            component: component.clone(),
            linker: self.linker.clone(),
            instance: None,
            clock_defined: false,
            guest: None,
            _ticker: Arc::clone(&self.ticker),
            limits,
            depth: 0,
            poisoned: false,
        }
    }

    /// A store for inspecting a component (`describe()`), not for running addon code: every import
    /// the component declares (`orm-host`, WASI) is answered by a function that traps when called, so a
    /// component that only describes itself instantiates, and one that reaches for the host fails
    /// loudly. Same limits and sticky-trap rules as [`WasmHost::store`].
    pub fn inspection_store(
        &self,
        component: &Component,
        limits: Limits,
    ) -> Result<AddonStore, HostError> {
        let mut store = self.store(component, limits);
        store.define_wall_clock()?;
        store
            .linker
            .define_unknown_imports_as_traps(component)
            .map_err(|e| HostError::Config(format!("{e:#}")))?;
        Ok(store)
    }
}

/// State a store carries for its host imports and for limit enforcement.
#[derive(Debug)]
struct HostState {
    memory_cap: usize,
    /// Bytes granted so far across every memory of the store.
    memory_used: usize,
    /// Set when a growth was refused, so the resulting error is reported as a memory trap.
    limit_denied: Option<LimitKind>,
    /// The ORM the guest's `orm-host` imports reach, with the identity and approved capabilities
    /// of the current call. `None` until the host binds one.
    bridge: Option<OrmBridge>,
    /// Guest log events that arrived while no bridge was bound.
    unbound_logs: u64,
}

impl HostState {
    fn new(limits: Limits) -> Self {
        Self {
            memory_cap: limits.memory_bytes(),
            memory_used: 0,
            limit_denied: None,
            bridge: None,
            unbound_logs: 0,
        }
    }

    fn bridge(&self) -> Result<&OrmBridge, BoundaryError> {
        self.bridge.as_ref().ok_or_else(|| {
            boundary_error(
                BoundaryErrorKind::AccessDenied,
                "no ORM bridge is bound to this store",
            )
        })
    }
}

impl types::Host for HostState {}

impl orm_host::Host for HostState {
    async fn search(
        &mut self,
        model: String,
        domain_json: String,
        limit: Option<u32>,
        offset: Option<u32>,
        order: Option<String>,
    ) -> Result<Vec<i64>, BoundaryError> {
        self.bridge()?
            .search(&model, &domain_json, limit, offset, order.as_deref())
            .await
    }

    async fn read_columns(
        &mut self,
        model: String,
        ids: Vec<i64>,
        fields: Vec<String>,
    ) -> Result<RecordBatch, BoundaryError> {
        self.bridge()?.read_columns(&model, &ids, &fields).await
    }

    async fn create_batch(
        &mut self,
        model: String,
        values: Vec<Vec<(String, ScalarValue)>>,
    ) -> Result<Vec<i64>, BoundaryError> {
        self.bridge()?.create_batch(&model, &values).await
    }

    async fn write_batch(
        &mut self,
        model: String,
        ids: Vec<i64>,
        values: Vec<(String, ScalarValue)>,
    ) -> Result<(), BoundaryError> {
        self.bridge()?.write_batch(&model, &ids, &values).await
    }

    async fn write_columns(
        &mut self,
        model: String,
        ids: Vec<i64>,
        columns: Vec<ColumnVector>,
    ) -> Result<(), BoundaryError> {
        self.bridge()?.write_columns(&model, &ids, &columns).await
    }

    async fn unlink_batch(&mut self, model: String, ids: Vec<i64>) -> Result<(), BoundaryError> {
        self.bridge()?.unlink_batch(&model, &ids).await
    }

    async fn call_method(
        &mut self,
        model: String,
        method: String,
        ids: Vec<i64>,
        args_json: String,
    ) -> Result<String, BoundaryError> {
        self.bridge()?
            .call_method(&model, &method, &ids, &args_json)
            .await
    }

    async fn super_call(
        &mut self,
        model: String,
        method: String,
        ids: Vec<i64>,
        args_json: String,
    ) -> Result<String, BoundaryError> {
        self.bridge()?
            .super_call(&model, &method, &ids, &args_json)
            .await
    }

    async fn log(&mut self, level: LogLevel, target: String, message: String) {
        // A log call has no error channel; without a bridge the event is counted, not lost
        // silently (`AddonStore::unbound_logs`).
        match self.bridge.as_ref() {
            Some(bridge) => bridge.log(level, &target, &message),
            None => self.unbound_logs = self.unbound_logs.saturating_add(1),
        }
    }
}

impl ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // The cap is on the store as a whole: the growth of every memory is summed.
        let used = self
            .memory_used
            .saturating_add(desired.saturating_sub(current));
        if used > self.memory_cap {
            self.limit_denied = Some(LimitKind::Memory);
            // An error (not `false`) so the guest is trapped instead of seeing `memory.grow` = -1.
            return Err(wasmtime::Error::msg(format!(
                "guest memory {used} bytes exceeds the {} byte cap",
                self.memory_cap
            )));
        }
        self.memory_used = used;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > MAX_TABLE_ELEMENTS {
            self.limit_denied = Some(LimitKind::Memory);
            return Err(wasmtime::Error::msg(format!(
                "guest table of {desired} elements exceeds the {MAX_TABLE_ELEMENTS} element cap"
            )));
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        MAX_INSTANCES_PER_STORE
    }

    fn memories(&self) -> usize {
        MAX_MEMORIES_PER_STORE
    }

    fn tables(&self) -> usize {
        MAX_TABLES_PER_STORE
    }
}

/// One store per (transaction, addon): holds guest memory and the lazily created instance.
/// After any trap it is poisoned: callers roll back the transaction and drop it.
pub struct AddonStore {
    store: Store<HostState>,
    component: Component,
    linker: Linker<HostState>,
    instance: Option<Instance>,
    /// Whether the wall clock is defined on this store's linker.
    clock_defined: bool,
    /// The typed exports of the `addon-guest` interface, created on the first `invoke_method`.
    guest: Option<crate::bindings::Addon>,
    /// Keeps the epoch ticker alive for as long as this store can run guest code.
    _ticker: Arc<Ticker>,
    limits: Limits,
    depth: u32,
    poisoned: bool,
}

impl AddonStore {
    /// Binds the ORM the guest's `orm-host` imports reach: the backend, the identity of the call,
    /// the approved capabilities and the limits. Until it is bound, every import answers
    /// `access-denied`.
    pub fn bind_orm(&mut self, bridge: OrmBridge) {
        self.store.data_mut().bridge = Some(bridge);
    }

    /// Replaces the identity of the bound bridge for the next call. Fails when none is bound.
    pub fn set_caller(&mut self, caller: crate::bridge::Caller) -> Result<(), HostError> {
        match self.store.data_mut().bridge.as_mut() {
            Some(bridge) => {
                bridge.set_caller(caller);
                Ok(())
            }
            None => Err(HostError::Config("no ORM bridge is bound".into())),
        }
    }

    /// Guest log events dropped because no ORM bridge was bound.
    #[must_use]
    pub fn unbound_logs(&self) -> u64 {
        self.store.data().unbound_logs
    }

    /// Whether a bridge check that runs after the backend acted failed since the last call (see
    /// [`OrmBackend`](crate::OrmBackend)); clears the mark. The dispatcher rolls the call back when
    /// it returns `true`. `false` when no bridge is bound.
    pub fn take_tainted(&self) -> bool {
        self.store
            .data()
            .bridge
            .as_ref()
            .is_some_and(OrmBridge::take_tainted)
    }

    /// Bytes of linear memory granted to this store so far (what a pooled store keeps while idle).
    #[must_use]
    pub fn memory_used(&self) -> usize {
        self.store.data().memory_used
    }

    /// Whether an earlier trap made this store unusable.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Records entry into a guest call; refuses beyond the stack depth limit. The dispatcher
    /// pairs it with [`AddonStore::exit_call`].
    pub fn enter_call(&mut self) -> Result<(), HostError> {
        if self.depth >= self.limits.stack_depth() {
            return Err(HostError::LimitExceeded {
                limit: LimitKind::StackDepth,
                value: u64::from(self.depth) + 1,
                max: u64::from(self.limits.stack_depth()),
            });
        }
        self.depth += 1;
        Ok(())
    }

    /// Records the end of a guest call.
    pub fn exit_call(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn arm(&mut self) -> Result<(), HostError> {
        if self.poisoned {
            return Err(HostError::Poisoned);
        }
        if !self._ticker.alive() {
            return Err(HostError::Config(
                "epoch ticker stopped; CPU deadlines would not fire".into(),
            ));
        }
        // +1: the first tick may arrive immediately, so N ticks alone could fire after almost 0 ms.
        let ticks = self
            .limits
            .cpu()
            .as_millis()
            .div_ceil(EPOCH_TICK.as_millis())
            .max(1)
            + 1;
        self.store
            .set_epoch_deadline(u64::try_from(ticks).unwrap_or(u64::MAX));
        self.store.epoch_deadline_trap();
        Ok(())
    }

    /// Maps a runtime error that ended guest execution to the sticky trap error.
    fn trap(&mut self, err: &wasmtime::Error) -> HostError {
        self.poisoned = true;
        let kind = if self.store.data().limit_denied.is_some() {
            TrapKind::Memory
        } else {
            match err.downcast_ref::<Trap>() {
                Some(Trap::Interrupt) => TrapKind::Epoch,
                Some(Trap::StackOverflow) => TrapKind::Stack,
                _ => TrapKind::Other(format!("{err:#}")),
            }
        };
        HostError::Trap(kind)
    }

    /// Defines the wall clock on this store's linker, once.
    fn define_wall_clock(&mut self) -> Result<(), HostError> {
        if !self.clock_defined {
            let engine = self.store.engine().clone();
            add_wall_clock(&mut self.linker, &engine, &self.component)?;
            self.clock_defined = true;
        }
        Ok(())
    }

    async fn instance(&mut self) -> Result<Instance, HostError> {
        if let Some(instance) = self.instance {
            return Ok(instance);
        }
        self.define_wall_clock()?;
        // Poisoned while instantiating (start functions run guest code): a dropped future leaves a
        // half-built store that must not be reused. Cleared only on success.
        self.poisoned = true;
        match self
            .linker
            .instantiate_async(&mut self.store, &self.component)
            .await
        {
            Ok(instance) => {
                self.poisoned = false;
                self.instance = Some(instance);
                Ok(instance)
            }
            // A start function can trap or loop; those are guest faults. Anything else (link
            // error, pool exhausted) is not: nothing ran.
            Err(err)
                if self.store.data().limit_denied.is_some()
                    || err.downcast_ref::<Trap>().is_some() =>
            {
                Err(self.trap(&err))
            }
            Err(err) => Err(HostError::Instantiate(format!("{err:#}"))),
        }
    }

    async fn ensure_guest(&mut self) -> Result<(), HostError> {
        if self.guest.is_some() {
            return Ok(());
        }
        // The only interface the host grants is `orm-host`. What the guest toolchain adds (WASI
        // from the Rust standard library) is answered deny-by-default: each such import traps
        // when the guest calls it (no ambient authority).
        self.define_wall_clock()?;
        self.linker
            .define_unknown_imports_as_traps(&self.component)
            .map_err(|e| HostError::Instantiate(format!("{e:#}")))?;
        // Same rule as `instance()`: start functions run guest code, so a store that dropped this
        // future midway must not be reused.
        self.poisoned = true;
        match crate::bindings::Addon::instantiate_async(
            &mut self.store,
            &self.component,
            &self.linker,
        )
        .await
        {
            Ok(guest) => {
                self.poisoned = false;
                self.guest = Some(guest);
                Ok(())
            }
            Err(err)
                if self.store.data().limit_denied.is_some()
                    || err.downcast_ref::<Trap>().is_some() =>
            {
                Err(self.trap(&err))
            }
            Err(err) => Err(HostError::Instantiate(format!("{err:#}"))),
        }
    }

    /// Calls the export `addon-guest.invoke-method` under the CPU deadline. The outer result is the
    /// host's (a trap poisons the store); the inner one is the guest's own answer.
    pub async fn invoke_method(
        &mut self,
        ctx: &types::CallContext,
        model: &str,
        method: &str,
        ids: &[i64],
        args_json: &str,
    ) -> Result<Result<String, BoundaryError>, HostError> {
        self.arm()?;
        self.ensure_guest().await?;
        let Some(guest) = self.guest.as_ref() else {
            return Err(HostError::Instantiate("guest exports are missing".into()));
        };
        self.poisoned = true;
        // the generated binding takes `&String` for the text arguments
        let (model, method, args_json) =
            (model.to_owned(), method.to_owned(), args_json.to_owned());
        let result = guest
            .erpforall_addon_addon_guest()
            .call_invoke_method(&mut self.store, ctx, &model, &method, ids, &args_json)
            .await;
        self.finish(result)
    }

    /// Calls the export `addon-guest.describe` under the CPU deadline.
    pub async fn describe(&mut self) -> Result<String, HostError> {
        self.arm()?;
        self.ensure_guest().await?;
        let Some(guest) = self.guest.as_ref() else {
            return Err(HostError::Instantiate("guest exports are missing".into()));
        };
        self.poisoned = true;
        let result = guest
            .erpforall_addon_addon_guest()
            .call_describe(&mut self.store)
            .await;
        self.finish(result)
    }

    /// Calls the export `name: func()` under the CPU deadline.
    pub async fn call_unit(&mut self, name: &str) -> Result<(), HostError> {
        self.arm()?;
        let instance = self.instance().await?;
        let func = instance
            .get_typed_func::<(), ()>(&mut self.store, name)
            .map_err(|e| HostError::Export {
                name: name.to_owned(),
                reason: format!("{e:#}"),
            })?;
        // Poisoned for the duration of the call: a caller that drops this future midway (timeout)
        // leaves a half-run guest behind, and the store must not be reused.
        self.poisoned = true;
        let result = func.call_async(&mut self.store, ()).await;
        self.finish(result)
    }

    /// Calls `describe: func() -> string` of the `addon-guest` interface under the CPU deadline
    /// : the text is what the component's macros wrote at compile time.
    pub async fn call_describe(&mut self) -> Result<String, HostError> {
        let export = |reason: String| HostError::Export {
            name: "describe".to_owned(),
            reason,
        };
        self.arm()?;
        let instance = self.instance().await?;
        let interface_name = format!("{}/addon-guest@{}", crate::WIT_PACKAGE, crate::WIT_VERSION);
        let interface = instance
            .get_export_index(&mut self.store, None, &interface_name)
            .ok_or_else(|| export(format!("the component does not export `{interface_name}`")))?;
        let function = instance
            .get_export_index(&mut self.store, Some(&interface), "describe")
            .ok_or_else(|| export(format!("`{interface_name}` has no `describe`")))?;
        let func = instance
            .get_typed_func::<(), (String,)>(&mut self.store, function)
            .map_err(|e| export(format!("{e:#}")))?;
        self.poisoned = true;
        let result = func.call_async(&mut self.store, ()).await;
        self.finish(result.map(|(text,)| text))
    }

    /// Calls the export `name: func(u32) -> u32` under the CPU deadline.
    pub async fn call_u32(&mut self, name: &str, arg: u32) -> Result<u32, HostError> {
        self.arm()?;
        let instance = self.instance().await?;
        let func = instance
            .get_typed_func::<(u32,), (u32,)>(&mut self.store, name)
            .map_err(|e| HostError::Export {
                name: name.to_owned(),
                reason: format!("{e:#}"),
            })?;
        self.poisoned = true;
        let result = func.call_async(&mut self.store, (arg,)).await;
        self.finish(result.map(|(value,)| value))
    }

    /// Clears the in-call poison on success; on error keeps it and returns the trap.
    fn finish<T>(&mut self, result: wasmtime::Result<T>) -> Result<T, HostError> {
        match result {
            Ok(value) => {
                self.poisoned = false;
                Ok(value)
            }
            Err(err) => Err(self.trap(&err)),
        }
    }
}
