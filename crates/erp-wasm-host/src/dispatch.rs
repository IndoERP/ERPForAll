//! Running a WASM addon's methods inside the ORM's override chains (plan §5).
//!
//! [`WasmAddon`] is one loaded component with the rights the administrator approved for it.
//! [`WasmAddon::method_impls`] turns what its `describe()` lists into the [`WasmMethodImpl`]s an
//! ORM takes ([`erp_orm::OrmBuilder::wasm_methods`]); each one is a [`MethodInvoker`] that
//! `erp-orm` calls like any other entry of an override chain, and through which the guest's
//! `super()` goes back into `erp-orm` (native or WebAssembly, whichever sits below).
//!
//! # Lock and borrow model
//!
//! * The transaction is `&mut` for the whole call and is never shared: it stays with the invoker
//!   (the *driver*). The guest runs in a `wasmtime` store that owns nothing of the transaction.
//! * A guest's `orm-host` import does not reach the ORM itself. It sends a request over a channel
//!   ([`ChannelBackend`]) and waits for the answer; the driver, which polls the guest call and the
//!   channel together, runs the request on the transaction and answers. So no `unsafe`, no raw
//!   pointer to the transaction in the store and no lock held across the guest or the database.
//! * The guest call is suspended while its request runs, so one store is never entered twice.
//!   A request that reaches the same addon again (`super()` into another layer of it, or
//!   `call-method` to a model it also extends) leases **another** store from the transaction's
//!   [`InstancePools`] (kept in the transaction, so a commit or rollback drops them with it). The
//!   pool holds stores that are idle; a store is out of it while it runs. Instances share the
//!   compiled component and nothing else (memories are disjoint, plan §4).
//! * Nesting is bounded by [`Limits::stack_depth`]; one call is bounded in wall-clock time by
//!   [`Limits::cpu`], for the guest (epoch deadline) and for every request it waits on.
//!
//! # Errors
//!
//! A guest error comes back as an ORM error of the nearest kind. A trap, a timeout, a limit hit or
//! a tainted bridge (a check that failed after the ORM acted) fails the call *and* poisons the
//! transaction (commit refuses), whatever the caller does with the error; the store is discarded.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::rpc::{Effect, WasmRpc, WasmRpcParam};
use erp_orm::{
    BoxFuture, DynSuper, Env, ImplKind, MethodInvoker, Records, Transaction, WasmMethodImpl,
};
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::Component;

use crate::adapter::{
    MethodRole, Policy, Reply, Request, Running, caller_of, check_create_answer, check_real,
    error_of, execute,
};
use crate::bindings::erpforall::addon::orm_host::LogLevel;
use crate::bindings::erpforall::addon::types::{
    BoundaryErrorKind, CallContext, ColumnVector, RecordBatch, ScalarValue,
};
use crate::bridge::{
    BackendFuture, BridgeResult, Caller, OrmBackend, OrmBridge, OrmCapabilities, boundary_error,
};
use crate::error::{HostError, TrapKind};
use crate::host::{AddonStore, WasmHost};
use crate::limits::Limits;

/// Name used in logs and errors for a guest's events.
const LOG_TARGET: &str = "erp::wasm_guest";

/// Longest model or method name a package may declare, in bytes.
const MAX_NAME_BYTES: usize = crate::bridge::MAX_NAME_BYTES;

/// Most `method` and `override` contributions one addon's `describe()` may list. A package over it
/// is refused when its description is read, before any name reaches the process-wide tables (G
/// security review r2 N1: one package must not be able to use up the shared budget of method names).
pub const MAX_DESCRIBED_METHODS: usize = 4096;

/// One method a component's `describe()` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribedMethod {
    /// `_name` of the model.
    pub model: String,
    /// Method name.
    pub method: String,
    /// Define or override.
    pub kind: ImplKind,
    /// The arguments after `this` and `tx`, as the source declared them: `(name, type text)`.
    /// Present in the description of every method and override (a missing or malformed list is
    /// refused); a compute has none, its inputs arrive as data.
    pub args: Vec<(String, String)>,
    /// The author marked the method for RPC (`#[method(rpc)]`, `"rpc": true` in `describe()`). A
    /// method without the marker is never callable from a client (opt-in).
    pub rpc: bool,
    /// What the method is for: it decides what its guest may ask of the host.
    pub role: MethodRole,
}

fn valid_model(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

fn valid_method(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The methods `addon` lists in a `describe()` text (`{"schema":1,"contributions":[...]}`): its
/// own `method` and `override` contributions. Contributions of other addons the component was
/// linked with are not this addon's layers and are skipped.
///
/// # Errors
///
/// `HostError::Config` when the text is not a description of schema 1, a name is invalid, or the
/// addon lists more than [`MAX_DESCRIBED_METHODS`] methods.
pub fn described_methods(
    addon: &str,
    describe_json: &str,
) -> std::result::Result<Vec<DescribedMethod>, HostError> {
    let bad = |what: String| HostError::Config(format!("describe() of '{addon}': {what}"));
    let doc: serde_json::Value =
        serde_json::from_str(describe_json).map_err(|e| bad(format!("not JSON: {e}")))?;
    if doc["schema"] != 1 {
        return Err(bad(format!("unknown schema {}", doc["schema"])));
    }
    let list = doc["contributions"]
        .as_array()
        .ok_or_else(|| bad("no contributions list".into()))?;
    // the methods the addon declares as constraints (also listed as `method`)
    let constraints: HashSet<(&str, &str)> = list
        .iter()
        .filter(|e| e["addon"].as_str() == Some(addon) && e["kind"].as_str() == Some("constraint"))
        .filter_map(|e| Some((e["model"].as_str()?, e["name"].as_str()?)))
        .collect();
    let mut out = Vec::new();
    for entry in list {
        let (Some(owner), Some(kind)) = (entry["addon"].as_str(), entry["kind"].as_str()) else {
            return Err(bad(format!("malformed contribution {entry}")));
        };
        if owner != addon {
            continue;
        }
        let role = match (kind, entry["model"].as_str(), entry["name"].as_str()) {
            ("compute", _, _) => MethodRole::Compute,
            ("method", Some(model), Some(name)) if constraints.contains(&(model, name)) => {
                MethodRole::Constraint
            }
            _ => MethodRole::Method,
        };
        let kind = match kind {
            // a compute runs as a method of the component named like the compute (the host hands
            // it the inputs as data); a constraint is listed as a `method` as well
            "method" | "compute" => ImplKind::Define,
            "override" => ImplKind::Override,
            _ => continue,
        };
        let (Some(model), Some(method)) = (entry["model"].as_str(), entry["name"].as_str()) else {
            return Err(bad(format!("malformed method contribution {entry}")));
        };
        // Names come from the package's own text: validated before anything keeps them.
        if !valid_model(model) || !valid_method(method) {
            return Err(bad(format!(
                "invalid name '{}.{}': a model is lower-case ASCII letters, digits, '_' and '.', a \
                 method ASCII letters, digits and '_', each at most {MAX_NAME_BYTES} bytes",
                model.chars().take(64).collect::<String>(),
                method.chars().take(64).collect::<String>(),
            )));
        }
        if out.len() >= MAX_DESCRIBED_METHODS {
            return Err(bad(format!(
                "more than {MAX_DESCRIBED_METHODS} method contributions; refusing the package"
            )));
        }
        // a compute is described with its dependencies, not arguments: its inputs arrive as data
        // and it is never callable from a client
        let (args, rpc) = if role == MethodRole::Compute {
            (Vec::new(), false)
        } else {
            let at = |field: &str| format!("method '{model}.{method}': describe() {field}");
            let args = entry["detail"]["args"]
                .as_array()
                .ok_or_else(|| bad(at("has no `args` list")))?
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let text = |key: &str| {
                        a[key]
                            .as_str()
                            .filter(|t| !t.is_empty())
                            .map(str::to_owned)
                            .ok_or_else(|| bad(at(&format!("argument {i} has no `{key}`"))))
                    };
                    Ok((text("name")?, text("type")?))
                })
                .collect::<std::result::Result<Vec<_>, HostError>>()?;
            let rpc = match &entry["detail"]["rpc"] {
                serde_json::Value::Null => false,
                serde_json::Value::Bool(flag) => *flag,
                other => {
                    return Err(bad(at(&format!(
                        "`rpc` is {other}, expected true or false"
                    ))));
                }
            };
            (args, rpc)
        };
        out.push(DescribedMethod {
            model: model.to_owned(),
            method: method.to_owned(),
            kind,
            args,
            rpc,
            role,
        });
    }
    Ok(out)
}

/// The `(model, method)` pairs a guest of one addon may name in `call-method`.
///
/// Two sources, nothing else: the methods the registry exposes over RPC (`#[rpc]` or
/// `#[data_callable]`, per model, so a name exposed on one model is not callable on another), and
/// the methods the addon itself defines with a `method` contribution on the model it extends. A
/// name the guest declares never unlocks a native method of the same name on another model, and
/// `Hidden` or internal methods are in neither source. The adapter checks the registry again at
/// the moment of every call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallableMethods {
    pairs: Arc<HashSet<(String, String)>>,
    own: Arc<HashSet<(String, String)>>,
}

impl CallableMethods {
    /// Whether `method` of `model` is in the list.
    #[must_use]
    pub fn contains(&self, model: &str, method: &str) -> bool {
        self.pairs.contains(&(model.to_owned(), method.to_owned()))
    }

    /// Number of `(model, method)` pairs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether no method is callable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

/// The callable methods of a guest of `addon` ([`CallableMethods`]).
///
/// Parses `describe_json` with [`described_methods`]; a caller that also builds the ORM layers
/// should parse once and use [`callable_from`] and [`WasmAddon::method_impls_from`] on the same list,
/// so the callable set and the chain come from one source.
///
/// # Errors
///
/// As [`described_methods`].
pub fn callable_methods(
    orm: &erp_orm::Orm,
    addon: &str,
    describe_json: &str,
) -> std::result::Result<CallableMethods, HostError> {
    Ok(callable_from(
        orm,
        &described_methods(addon, describe_json)?,
    ))
}

/// The callable methods of a guest whose `describe()` listed `described`: the same list the ORM
/// layers are built from ([`WasmAddon::method_impls_from`]), so what the guest may call is never
/// derived from a second reading of the description (G security review r2 N2).
#[must_use]
pub fn callable_from(orm: &erp_orm::Orm, described: &[DescribedMethod]) -> CallableMethods {
    let mut pairs = HashSet::new();
    let mut own = HashSet::new();
    for m in described {
        if m.kind == ImplKind::Define {
            own.insert((m.model.clone(), m.method.clone()));
            pairs.insert((m.model.clone(), m.method.clone()));
        }
    }
    let exposure = orm.exposure();
    for model in orm.registry().models() {
        for (method, exposed) in exposure.methods(model.id) {
            if matches!(exposed, erp_orm::rpc::Exposure::Exposed(_)) {
                pairs.insert((model.name.to_owned(), method.to_owned()));
            }
        }
    }
    CallableMethods {
        pairs: Arc::new(pairs),
        own: Arc::new(own),
    }
}

/// One loaded component, with the rights approved for it and the limits of its calls.
pub struct WasmAddon {
    host: Arc<WasmHost>,
    name: String,
    component: Component,
    limits: Limits,
    capabilities: OrmCapabilities,
    callable: CallableMethods,
}

impl std::fmt::Debug for WasmAddon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmAddon")
            .field("name", &self.name)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl WasmAddon {
    /// A component of addon `name`. `capabilities` are the ORM operations approved for it and
    /// `callable` the methods its `call-method` may reach ([`callable_methods`];
    /// [`CallableMethods::default`] reaches none).
    pub fn new(
        host: Arc<WasmHost>,
        name: impl Into<String>,
        component: Component,
        limits: Limits,
        capabilities: OrmCapabilities,
        callable: CallableMethods,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            name: name.into(),
            component,
            limits,
            capabilities,
            callable,
        })
    }

    /// The addon's technical name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `describe()` text of the component (one throwaway store, outside any transaction).
    ///
    /// # Errors
    ///
    /// The trap or instantiation error of the call.
    pub async fn describe(&self) -> std::result::Result<String, HostError> {
        let mut store = self.host.store(&self.component, self.limits);
        store.describe().await
    }

    /// The implementations to give [`erp_orm::OrmBuilder::wasm_methods`]: one per method the
    /// description lists for this addon.
    ///
    /// # Errors
    ///
    /// As [`described_methods`].
    pub fn method_impls(
        self: &Arc<Self>,
        describe_json: &str,
    ) -> std::result::Result<Vec<WasmMethodImpl>, HostError> {
        Ok(self.method_impls_from(&described_methods(&self.name, describe_json)?))
    }

    /// As [`WasmAddon::method_impls`], from a list already read with [`described_methods`] (the one
    /// [`callable_from`] used).
    #[must_use]
    pub fn method_impls_from(
        self: &Arc<Self>,
        described: &[DescribedMethod],
    ) -> Vec<WasmMethodImpl> {
        described
            .iter()
            .cloned()
            .map(|m| WasmMethodImpl {
                invoker: Arc::new(WasmMethod {
                    addon: Arc::clone(self),
                    model: m.model.clone(),
                    method: m.method.clone(),
                    kind: m.kind,
                    rpc: rpc_of(&m),
                    role: m.role,
                }),
                model: m.model,
                method: m.method,
            })
            .collect()
    }
}

/// One method of one addon, as an entry of an override chain.
struct WasmMethod {
    addon: Arc<WasmAddon>,
    model: String,
    method: String,
    kind: ImplKind,
    rpc: Option<WasmRpc>,
    role: MethodRole,
}

/// The JSON shape a parameter of Rust type `ty` accepts over RPC, and whether the client must pass
/// it. `Option<T>` is optional. A type this mapping does not know is accepted as any JSON value
/// (the guest's own deserialisation refuses a wrong shape).
fn param_shape(ty: &str) -> (&'static str, bool) {
    let ty: String = ty.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some(inner) = ty.strip_prefix("Option<").and_then(|t| t.strip_suffix('>')) {
        return (param_shape(inner).0, false);
    }
    let shape = match ty.as_str() {
        "String" | "&str" | "Arc<str>" => "a string",
        "bool" => "a boolean",
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize" => {
            "an integer"
        }
        "f32" | "f64" => "a number",
        _ => "any value",
    };
    (shape, true)
}

/// How an RPC client may call a method: a method an addon defines, marks for RPC
/// (`#[method(rpc)]`; opt-in as `#[rpc]` is) and whose name does not start with `_`
/// becomes callable from the web client and `call_kw` under the same exposure and access rules as
/// a native `#[rpc]` method. The effect
/// is `Write`: a component has no way to declare a read-only method yet, and `Write` is the
/// conservative class (read-only API keys are refused; the transaction is a writing one). A method
/// whose argument names are not plain identifiers is not exposed (logged), never exposed with a
/// guess.
fn rpc_of(m: &DescribedMethod) -> Option<WasmRpc> {
    // only an ordinary method: a compute or a constraint runs under its own narrower policy and is
    // called by the host, never by a client
    if !m.rpc
        || m.kind != ImplKind::Define
        || m.role != MethodRole::Method
        || m.method.starts_with('_')
    {
        return None;
    }
    let mut params = Vec::with_capacity(m.args.len());
    for (name, ty) in &m.args {
        if !valid_method(name) || name.starts_with('_') {
            tracing::warn!(
                target: "erp::wasm_host",
                "{}.{} of a WebAssembly addon is not callable over RPC: argument '{}' is not a plain name",
                m.model, m.method, name
            );
            return None;
        }
        let (expected, required) = param_shape(ty);
        params.push(WasmRpcParam {
            name: name.clone(),
            required,
            expected,
        });
    }
    Some(WasmRpc {
        effect: Effect::Write,
        params,
    })
}

/// The methods of `described` a client may call, as `model.method`, sorted: what an administrator
/// is told at install time.
#[must_use]
pub fn exposed_methods(described: &[DescribedMethod]) -> Vec<String> {
    let mut out: Vec<String> = described
        .iter()
        .filter(|m| rpc_of(m).is_some())
        .map(|m| format!("{}.{}", m.model, m.method))
        .collect();
    out.sort();
    out
}

impl std::fmt::Debug for WasmMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WasmMethod({}.{} of {})",
            self.model, self.method, self.addon.name
        )
    }
}

impl MethodInvoker for WasmMethod {
    fn addon(&self) -> &str {
        &self.addon.name
    }

    fn kind(&self) -> ImplKind {
        self.kind
    }

    fn rpc(&self) -> Option<WasmRpc> {
        self.rpc.clone()
    }

    fn invoke<'a>(
        &'a self,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
        sup: DynSuper,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(self.run(tx, records, args_json, sup))
    }
}

/// Idle stores of the addons called in one transaction. Lives in the transaction
/// ([`Transaction::extension`]) and is dropped with it.
///
/// Memory is bounded per transaction: every running store counts at its full cap, every idle one at
/// what it grew to, and a call that would take the total past [`Limits::tx_memory_bytes`] first
/// drops idle stores and then fails with `ResourceExhausted`. At most [`Limits::idle_stores`] idle
/// stores are kept per addon.
///
/// A pooled store is reused by later calls of the same transaction, so guest statics carry over
/// from one call to the next. The user is the same for the whole transaction's pool only when the
/// caller does not switch identity inside it; the guest is still handed the identity of each call
/// and holds no rights of its own, but it must not keep one user's data in statics for another.
#[derive(Default)]
pub struct InstancePools {
    idle: std::sync::Mutex<HashMap<String, Vec<AddonStore>>>,
    /// Calls into guests running now (nesting), shared with the guards that count them.
    depth: Arc<AtomicU32>,
    /// Memory reserved by the stores running now (their full cap each), shared with the guards.
    running_bytes: Arc<AtomicUsize>,
    /// Stores created in this transaction, for tests and diagnostics.
    created: u64,
}

impl InstancePools {
    fn take(&mut self, addon: &str) -> Option<AddonStore> {
        let idle = self.idle.get_mut().unwrap_or_else(|p| p.into_inner());
        idle.get_mut(addon).and_then(Vec::pop)
    }

    /// Keeps `store` idle unless `addon` already has `keep` idle stores.
    fn put(&mut self, addon: &str, store: AddonStore, keep: usize) {
        let idle = self.idle.get_mut().unwrap_or_else(|p| p.into_inner());
        let list = idle.entry(addon.to_owned()).or_default();
        if list.len() < keep {
            list.push(store);
        }
    }

    /// Memory the idle stores hold.
    fn idle_bytes(&mut self) -> usize {
        let idle = self.idle.get_mut().unwrap_or_else(|p| p.into_inner());
        idle.values().flatten().map(AddonStore::memory_used).sum()
    }

    /// Drops one idle store of any addon; false when none is left.
    fn evict_one(&mut self) -> bool {
        let idle = self.idle.get_mut().unwrap_or_else(|p| p.into_inner());
        idle.values_mut().any(|list| list.pop().is_some())
    }

    /// Stores created for this transaction so far (one more per level of re-entry).
    #[must_use]
    pub fn created(&self) -> u64 {
        self.created
    }

    /// Idle stores of `addon`.
    #[must_use]
    pub fn idle(&mut self, addon: &str) -> usize {
        let idle = self.idle.get_mut().unwrap_or_else(|p| p.into_inner());
        idle.get(addon).map_or(0, Vec::len)
    }
}

/// Counts one running guest call and the memory reserved for it; both drop again however the call
/// ends.
struct LeaseGuard {
    depth: Arc<AtomicU32>,
    running_bytes: Arc<AtomicUsize>,
    reserved: usize,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.depth.fetch_sub(1, Ordering::SeqCst);
        self.running_bytes
            .fetch_sub(self.reserved, Ordering::SeqCst);
    }
}

/// The imports of one guest call, forwarded to the driver that holds the transaction.
struct ChannelBackend {
    requests: mpsc::Sender<(Caller, Request, oneshot::Sender<BridgeResult<Reply>>)>,
    /// Which addon and method the guest is running, so its log lines say so.
    addon: Arc<str>,
    entry: Arc<str>,
}

impl ChannelBackend {
    async fn ask(&self, caller: &Caller, request: Request) -> BridgeResult<Reply> {
        let (answer, wait) = oneshot::channel();
        self.requests
            .send((caller.clone(), request, answer))
            .await
            .map_err(|_| {
                boundary_error(
                    BoundaryErrorKind::InternalError,
                    "the call that owns this guest has ended",
                )
            })?;
        wait.await.map_err(|_| {
            boundary_error(
                BoundaryErrorKind::InternalError,
                "the call that owns this guest ended before it answered",
            )
        })?
    }
}

fn wrong_reply<T>() -> BridgeResult<T> {
    Err(boundary_error(
        BoundaryErrorKind::InternalError,
        "the driver answered with another kind of reply than the request needs",
    ))
}

impl OrmBackend for ChannelBackend {
    fn search<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        domain_json: &'a str,
        limit: Option<u32>,
        offset: Option<u32>,
        order: Option<&'a str>,
    ) -> BackendFuture<'a, BridgeResult<Vec<i64>>> {
        Box::pin(async move {
            match self
                .ask(
                    caller,
                    Request::Search {
                        model: model.to_owned(),
                        domain_json: domain_json.to_owned(),
                        limit,
                        offset,
                        order: order.map(str::to_owned),
                    },
                )
                .await?
            {
                Reply::Ids(ids) => Ok(ids),
                _ => wrong_reply(),
            }
        })
    }

    fn read_columns<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
        fields: &'a [String],
    ) -> BackendFuture<'a, BridgeResult<RecordBatch>> {
        Box::pin(async move {
            match self
                .ask(
                    caller,
                    Request::Read {
                        model: model.to_owned(),
                        ids: ids.to_vec(),
                        fields: fields.to_vec(),
                    },
                )
                .await?
            {
                Reply::Batch(batch) => Ok(batch),
                _ => wrong_reply(),
            }
        })
    }

    fn create_batch<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        values: &'a [Vec<(String, ScalarValue)>],
    ) -> BackendFuture<'a, BridgeResult<Vec<i64>>> {
        Box::pin(async move {
            match self
                .ask(
                    caller,
                    Request::Create {
                        model: model.to_owned(),
                        values: values.to_vec(),
                    },
                )
                .await?
            {
                Reply::Ids(ids) => Ok(ids),
                _ => wrong_reply(),
            }
        })
    }

    fn write_batch<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
        values: &'a [(String, ScalarValue)],
    ) -> BackendFuture<'a, BridgeResult<()>> {
        Box::pin(async move {
            match self
                .ask(
                    caller,
                    Request::Write {
                        model: model.to_owned(),
                        ids: ids.to_vec(),
                        values: values.to_vec(),
                    },
                )
                .await?
            {
                Reply::Unit => Ok(()),
                _ => wrong_reply(),
            }
        })
    }

    fn write_columns<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
        columns: &'a [ColumnVector],
    ) -> BackendFuture<'a, BridgeResult<()>> {
        Box::pin(async move {
            match self
                .ask(
                    caller,
                    Request::WriteColumns {
                        model: model.to_owned(),
                        ids: ids.to_vec(),
                        columns: columns.to_vec(),
                    },
                )
                .await?
            {
                Reply::Unit => Ok(()),
                _ => wrong_reply(),
            }
        })
    }

    fn unlink_batch<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
    ) -> BackendFuture<'a, BridgeResult<()>> {
        Box::pin(async move {
            match self
                .ask(
                    caller,
                    Request::Unlink {
                        model: model.to_owned(),
                        ids: ids.to_vec(),
                    },
                )
                .await?
            {
                Reply::Unit => Ok(()),
                _ => wrong_reply(),
            }
        })
    }

    fn call_method<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        method: &'a str,
        ids: &'a [i64],
        args_json: &'a str,
    ) -> BackendFuture<'a, BridgeResult<String>> {
        Box::pin(async move {
            match self
                .ask(
                    caller,
                    Request::Call {
                        model: model.to_owned(),
                        method: method.to_owned(),
                        ids: ids.to_vec(),
                        args_json: args_json.to_owned(),
                    },
                )
                .await?
            {
                Reply::Text(text) => Ok(text),
                _ => wrong_reply(),
            }
        })
    }

    fn super_call<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        method: &'a str,
        ids: &'a [i64],
        args_json: &'a str,
    ) -> BackendFuture<'a, BridgeResult<String>> {
        Box::pin(async move {
            match self
                .ask(
                    caller,
                    Request::Super {
                        model: model.to_owned(),
                        method: method.to_owned(),
                        ids: ids.to_vec(),
                        args_json: args_json.to_owned(),
                    },
                )
                .await?
            {
                Reply::Text(text) => Ok(text),
                _ => wrong_reply(),
            }
        })
    }

    fn log(&self, caller: &Caller, level: LogLevel, target: &str, message: &str) {
        let user = caller.user_id();
        let addon = &*self.addon;
        let entry = &*self.entry;
        let request_id = erp_orm::trace::current_request_id();
        let request_id = request_id.as_deref();
        match level {
            LogLevel::Trace => {
                tracing::trace!(target: LOG_TARGET, user, addon, entry, request_id, guest_target = target, "{message}");
            }
            LogLevel::Debug => {
                tracing::debug!(target: LOG_TARGET, user, addon, entry, request_id, guest_target = target, "{message}");
            }
            LogLevel::Info => {
                tracing::info!(target: LOG_TARGET, user, addon, entry, request_id, guest_target = target, "{message}");
            }
            LogLevel::Warn => {
                tracing::warn!(target: LOG_TARGET, user, addon, entry, request_id, guest_target = target, "{message}");
            }
            LogLevel::Error => {
                tracing::error!(target: LOG_TARGET, user, addon, entry, request_id, guest_target = target, "{message}");
            }
        }
    }
}

/// A host failure as the ORM error the caller sees.
fn host_failure(addon: &str, err: &HostError) -> Error {
    let kind = match err {
        HostError::Trap(TrapKind::Epoch | TrapKind::Memory | TrapKind::Stack)
        | HostError::LimitExceeded { .. } => ErrorKind::ResourceExhausted,
        _ => ErrorKind::Internal,
    };
    Error::new(kind, format!("WASM addon '{addon}': {err}"))
}

/// The `context-json` of a call: the context's keys besides the ones the call context carries as
/// fields (`lang`, `tz`, `allowed_company_ids`) and besides identity keys, which the host owns.
fn context_json(env: &Env, limits: &Limits) -> Result<String> {
    /// Most context entries handed to a guest.
    const MAX_CONTEXT_ENTRIES: usize = 256;
    let mut map = env.context().to_map_bounded(MAX_CONTEXT_ENTRIES)?;
    for key in ["lang", "tz", "allowed_company_ids"] {
        map.remove(key);
    }
    map.retain(|key, _| {
        !crate::bridge::FORBIDDEN_KEYS
            .iter()
            .any(|f| key.eq_ignore_ascii_case(f))
    });
    let mut object = serde_json::Map::new();
    for (key, value) in &map {
        object.insert(key.clone(), erp_orm::wasm_abi1_value_to_json(value)?);
    }
    let text = serde_json::to_string(&serde_json::Value::Object(object))
        .map_err(|e| Error::internal(format!("the context is not serialisable: {e}")))?;
    limits
        .check_payload(text.len())
        .map_err(|e| Error::new(ErrorKind::ResourceExhausted, format!("context: {e}")))?;
    Ok(text)
}

/// What one activation of a guest method is, built from the records it was invoked on.
struct Activation {
    ctx: CallContext,
    running: Running,
    ids: Vec<i64>,
}

impl Activation {
    /// The identity and context of `records.env()` (the environment the method was invoked in, not
    /// the transaction's opening one), the ids and the `super()` of the running layer.
    async fn new(
        tx: &mut Transaction,
        addon: &WasmAddon,
        records: &Records,
        sup: DynSuper,
        role: MethodRole,
    ) -> Result<Self> {
        let ids = check_real(records)?;
        let env = records.env().clone();
        let caller = caller_of(tx, &env).await.map_err(|e| {
            Error::new(
                e.kind(),
                format!("WASM addon '{}': {}", addon.name, e.message()),
            )
        })?;
        let ctx = CallContext {
            user_id: caller.user_id(),
            company_id: caller.company_id(),
            allowed_company_ids: caller.allowed_company_ids().to_vec(),
            // An empty language is "none set in the context"; no language is substituted.
            lang: env.lang().unwrap_or_default().to_owned(),
            tz: env.tz().to_owned(),
            context_json: context_json(&env, &addon.limits)?,
            cache_epoch: tx.change_count(),
        };
        let policy = Policy::of(role, &env);
        let running = Running {
            method: sup_method(&sup),
            sup,
            env,
            caller,
            ids: ids.iter().copied().collect(),
            own: Arc::clone(&addon.callable.own),
            policy,
            created: std::sync::Mutex::new(Vec::new()),
        };
        Ok(Self { ctx, running, ids })
    }
}

impl WasmMethod {
    async fn run(
        &self,
        tx: &mut Transaction,
        records: &Records,
        args_json: &str,
        sup: DynSuper,
    ) -> Result<String> {
        let addon = &*self.addon;
        let create_input_count = if self.kind == ImplKind::Override && self.method == "create" {
            addon.limits.check_payload(args_json.len()).map_err(|e| {
                Error::new(
                    ErrorKind::ResourceExhausted,
                    format!("create arguments: {e}"),
                )
            })?;
            let (values,): (Vec<serde_json::Value>,) = serde_json::from_str(args_json)
                .map_err(|e| Error::validation(format!("invalid create arguments: {e}")))?;
            Some(values.len())
        } else {
            None
        };
        // the request trace names the addon in the RPC layer's own failure line
        erp_orm::trace::note_wasm_addon(&addon.name);
        let Activation { ctx, running, ids } =
            Activation::new(tx, addon, records, sup, self.role).await?;
        let caller = running.caller.clone();

        // Lease a store: an idle one, or a new one when every store of this addon is out (the call
        // is re-entering the addon). Depth and memory are counted before anything is created.
        let pools = tx.extension::<InstancePools>();
        let max = addon.limits.stack_depth();
        if pools.depth.fetch_add(1, Ordering::SeqCst) >= max {
            pools.depth.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::new(
                ErrorKind::ResourceExhausted,
                format!(
                    "WASM addon '{}': nesting exceeds the depth limit of {max}",
                    addon.name
                ),
            ));
        }
        let reserved = addon.limits.memory_bytes();
        let taken = pools.take(&addon.name);
        // this store runs at its full cap; the rest of the pool at what it grew to
        let tx_cap = addon.limits.tx_memory_bytes();
        let fits = |pools: &mut InstancePools| {
            pools
                .running_bytes
                .load(Ordering::SeqCst)
                .saturating_add(reserved)
                .saturating_add(pools.idle_bytes())
                <= tx_cap
        };
        let mut room = fits(pools);
        while !room && pools.evict_one() {
            room = fits(pools);
        }
        if !room {
            pools.depth.fetch_sub(1, Ordering::SeqCst);
            if let Some(store) = taken {
                pools.put(&addon.name, store, addon.limits.idle_stores() as usize);
            }
            return Err(Error::new(
                ErrorKind::ResourceExhausted,
                format!(
                    "WASM addon '{}': the transaction would hold more than {tx_cap} bytes of \
                     guest memory",
                    addon.name
                ),
            ));
        }
        pools.running_bytes.fetch_add(reserved, Ordering::SeqCst);
        let _lease = LeaseGuard {
            depth: Arc::clone(&pools.depth),
            running_bytes: Arc::clone(&pools.running_bytes),
            reserved,
        };
        let mut store = match taken {
            Some(store) => store,
            None => {
                pools.created += 1;
                addon.host.store(&addon.component, addon.limits)
            }
        };

        let (requests, mut inbox) = mpsc::channel(1);
        let bridge = OrmBridge::new(
            Arc::new(ChannelBackend {
                requests,
                addon: Arc::from(addon.name.as_str()),
                entry: Arc::from(format!("{}.{}", self.model, self.method)),
            }),
            caller.clone(),
            addon.capabilities,
            addon.limits,
        )
        .with_callable_model_methods(Arc::clone(&addon.callable.pairs));
        store.bind_orm(bridge);

        let started = Instant::now();
        let budget = addon.limits.cpu();
        let mut failure: Option<Error> = None;
        let outcome = {
            let call = store.invoke_method(&ctx, &self.model, &self.method, &ids, args_json);
            tokio::pin!(call);
            loop {
                tokio::select! {
                    biased;
                    done = &mut call => break Some(done),
                    request = inbox.recv() => {
                        let Some((bridge_caller, request, answer)) = request else { break None };
                        let left = budget.saturating_sub(started.elapsed());
                        let work = execute(tx, &bridge_caller, &running, request);
                        let result = match tokio::time::timeout(left, work).await {
                            Ok(result) => result,
                            Err(_) => {
                                failure = Some(Error::new(
                                    ErrorKind::ResourceExhausted,
                                    format!(
                                        "WASM addon '{}': a host call exceeded the time budget of {} ms",
                                        addon.name,
                                        budget.as_millis()
                                    ),
                                ));
                                break None;
                            }
                        };
                        // The guest may have been dropped meanwhile (it cannot be: it is polled
                        // here); a closed answer channel is its own bug, reported below.
                        if answer.send(result).is_err() {
                            failure = Some(Error::internal(
                                "a guest import stopped waiting for its answer",
                            ));
                            break None;
                        }
                    }
                }
            }
        };

        let tainted = store.take_tainted();
        let poisoned = store.is_poisoned();
        // `abort`: the guest or the transaction is in an unknown state (a trap, a timeout, a
        // check that failed after the ORM acted). The store is dropped and the transaction
        // poisoned, whatever the caller does with the error. A guest that merely returns an error
        // is an ordinary failure of the method.
        let (result, abort) = match outcome {
            Some(Ok(Ok(text))) if !tainted => {
                if let Some(input_count) = create_input_count {
                    match check_create_answer(&running, &text, input_count) {
                        Ok(()) => (Ok(text), false),
                        // the layer lied about what was created: the transaction is not safe to
                        // commit whatever the caller does
                        Err(error) => (Err(error), true),
                    }
                } else {
                    (Ok(text), false)
                }
            }
            Some(Ok(Ok(_))) => (
                Err(Error::internal(format!(
                    "WASM addon '{}': a check failed after the ORM had acted; the call is \
                     rolled back",
                    addon.name
                ))),
                true,
            ),
            Some(Ok(Err(guest))) => {
                let error = error_of(&guest);
                // a constraint only ever says "this is not valid": whatever kind the guest used
                // is a validation failure to the caller
                let error = if self.role == MethodRole::Constraint {
                    Error::validation(error.message().to_owned())
                } else {
                    error
                };
                (Err(error), tainted)
            }
            Some(Err(host)) => (Err(host_failure(&addon.name, &host)), true),
            None => (
                Err(failure.unwrap_or_else(|| {
                    Error::internal(format!("WASM addon '{}': the call ended early", addon.name))
                })),
                true,
            ),
        };
        if let Err(error) = &result {
            // A trap, a timeout, a limit or an error the guest returned: one line with the addon,
            // the method and the request id, whatever the caller then does with the error. The
            // message is the one the caller gets.
            let request_id = erp_orm::trace::current_request_id();
            tracing::warn!(
                target: "erp::wasm_host",
                addon = %addon.name,
                entry = %format!("{}.{}", self.model, self.method),
                kind = ?error.kind(),
                abort,
                request_id = request_id.as_deref(),
                reason = ?error.message(),
                "WebAssembly addon call failed"
            );
        }
        if abort || poisoned {
            drop(store);
            if let Err(error) = &result {
                tx.poison(error);
            }
        } else {
            let keep = addon.limits.idle_stores() as usize;
            tx.extension::<InstancePools>()
                .put(&addon.name, store, keep);
        }
        result
    }
}

fn sup_method(sup: &DynSuper) -> &'static str {
    sup.method()
}

/// A request a test sends through the adapter as the guest of a running method would
/// ([`probe_request`]).
#[doc(hidden)]
#[derive(Debug, Clone)]
pub enum ProbeRequest {
    /// `call-method`.
    Call {
        /// Model name.
        model: String,
        /// Method name.
        method: String,
        /// Record ids.
        ids: Vec<i64>,
        /// Arguments as JSON.
        args_json: String,
    },
    /// `super-call`.
    Super {
        /// Model name.
        model: String,
        /// Method name.
        method: String,
        /// Record ids.
        ids: Vec<i64>,
        /// Arguments as JSON.
        args_json: String,
    },
    /// The `language|context-json` pair of the call context the guest would receive.
    Context,
    /// The `user:company:allowed-companies` identity the guest would be bound to.
    Identity,
    /// `read-columns`.
    Read {
        /// Model name.
        model: String,
        /// Record ids.
        ids: Vec<i64>,
        /// Fields.
        fields: Vec<String>,
    },
    Search {
        /// Model name.
        model: String,
        /// Domain as JSON.
        domain_json: String,
    },
    /// `write-batch` of one text value.
    WriteText {
        /// Model name.
        model: String,
        /// Record ids.
        ids: Vec<i64>,
        /// Field.
        field: String,
        /// Value.
        value: String,
    },
    /// `unlink-batch`.
    Unlink {
        /// Model name.
        model: String,
        /// Record ids.
        ids: Vec<i64>,
    },
}

/// Runs `request` through the adapter exactly as the host runs a guest's request while `records`'
/// method is active in `addon` (same environment, same identity, same checks), without a guest.
/// Test support: the answer is the result text, the ids or the batch as text. Not part of the
/// stable API.
///
/// # Errors
///
/// The refusal or failure the guest would see: its kind and message.
#[doc(hidden)]
pub async fn probe_request(
    tx: &mut Transaction,
    addon: &WasmAddon,
    records: &Records,
    sup: DynSuper,
    request: ProbeRequest,
) -> std::result::Result<String, crate::BoundaryError> {
    probe_request_as(tx, addon, records, sup, MethodRole::Method, request).await
}

/// [`probe_request`] for a method that plays `role` (a compute or a constraint asks less of the
/// host than a method does).
///
/// # Errors
///
/// The refusal or failure the guest would see: its kind and message.
#[doc(hidden)]
pub async fn probe_request_as(
    tx: &mut Transaction,
    addon: &WasmAddon,
    records: &Records,
    sup: DynSuper,
    role: MethodRole,
    request: ProbeRequest,
) -> std::result::Result<String, crate::BoundaryError> {
    let activation = Activation::new(tx, addon, records, sup, role)
        .await
        .map_err(|e| crate::adapter::boundary_of(&e))?;
    let request = match request {
        ProbeRequest::Context => {
            return Ok(format!(
                "{}|{}",
                activation.ctx.lang, activation.ctx.context_json
            ));
        }
        ProbeRequest::Identity => {
            let c = &activation.running.caller;
            return Ok(format!(
                "{}:{}:{:?}",
                c.user_id(),
                c.company_id(),
                c.allowed_company_ids()
            ));
        }
        ProbeRequest::Call {
            model,
            method,
            ids,
            args_json,
        } => Request::Call {
            model,
            method,
            ids,
            args_json,
        },
        ProbeRequest::Super {
            model,
            method,
            ids,
            args_json,
        } => Request::Super {
            model,
            method,
            ids,
            args_json,
        },
        ProbeRequest::Read { model, ids, fields } => Request::Read { model, ids, fields },
        ProbeRequest::Search { model, domain_json } => Request::Search {
            model,
            domain_json,
            limit: None,
            offset: None,
            order: None,
        },
        ProbeRequest::WriteText {
            model,
            ids,
            field,
            value,
        } => Request::Write {
            model,
            ids,
            values: vec![(field, ScalarValue::ValStr(value))],
        },
        ProbeRequest::Unlink { model, ids } => Request::Unlink { model, ids },
    };
    let caller = activation.running.caller.clone();
    match execute(tx, &caller, &activation.running, request).await? {
        Reply::Text(text) => Ok(text),
        Reply::Ids(ids) => Ok(format!("{ids:?}")),
        Reply::Batch(batch) => Ok(format!("{batch:?}")),
        Reply::Unit => Ok(String::new()),
    }
}

#[cfg(test)]
mod rpc_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    fn described(kind: ImplKind, method: &str, args: &[(&str, &str)]) -> DescribedMethod {
        DescribedMethod {
            model: "m.x".to_owned(),
            method: method.to_owned(),
            kind,
            args: args
                .iter()
                .map(|(n, t)| ((*n).to_owned(), (*t).to_owned()))
                .collect(),
            rpc: true,
            role: MethodRole::Method,
        }
    }

    #[test]
    fn a_defined_public_method_is_callable_with_its_arguments_in_order() {
        let rpc = rpc_of(&described(
            ImplKind::Define,
            "lend",
            &[
                ("borrower", "Option < String >"),
                ("days", "i32"),
                ("rate", "f64"),
                ("urgent", "bool"),
                ("extra", "Vec<String>"),
            ],
        ))
        .unwrap();
        assert_eq!(rpc.effect, Effect::Write);
        let got: Vec<_> = rpc
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.required, p.expected))
            .collect();
        assert_eq!(
            got,
            [
                ("borrower", false, "a string"),
                ("days", true, "an integer"),
                ("rate", true, "a number"),
                ("urgent", true, "a boolean"),
                ("extra", true, "any value"),
            ]
        );
        // no arguments is a callable method too
        assert!(
            rpc_of(&described(ImplKind::Define, "ping", &[]))
                .unwrap()
                .params
                .is_empty()
        );
    }

    #[test]
    fn overrides_private_names_and_odd_arguments_are_not_callable() {
        assert!(rpc_of(&described(ImplKind::Override, "write", &[])).is_none());
        assert!(rpc_of(&described(ImplKind::Define, "_lend", &[])).is_none());
        // a pattern or a private-looking argument name is not guessed at
        for name in ["mut note", "(a, b)", "_note", ""] {
            assert!(
                rpc_of(&described(ImplKind::Define, "lend", &[(name, "String")])).is_none(),
                "{name}"
            );
        }
    }

    #[test]
    fn argument_types_are_read_from_the_description() {
        let text = serde_json::json!({"schema": 1, "contributions": [{
            "addon": "a", "kind": "method", "model": "m.x", "name": "go",
            "detail": {"args": [{"name": "note", "type": "String"}], "ret": "i32", "rpc": true}
        }]})
        .to_string();
        let methods = described_methods("a", &text).unwrap();
        assert_eq!(methods[0].args, [("note".to_owned(), "String".to_owned())]);
        assert!(methods[0].rpc);
        assert_eq!(exposed_methods(&methods), ["m.x.go"]);
    }

    #[test]
    fn a_marked_constraint_or_compute_is_not_callable() {
        for role in [MethodRole::Constraint, MethodRole::Compute] {
            let mut m = described(ImplKind::Define, "check_renewals", &[]);
            m.role = role;
            assert!(rpc_of(&m).is_none(), "{role:?}");
            assert!(exposed_methods(&[m]).is_empty(), "{role:?}");
        }
    }

    #[test]
    fn a_compute_is_described_without_arguments() {
        let text = serde_json::json!({"schema": 1, "contributions": [{
            "addon": "a", "kind": "compute", "model": "m.x", "name": "summary",
            "detail": {"async": false, "depends": ["name"], "reads": [], "depends_context": [],
                       "rpc": true}
        }]})
        .to_string();
        let methods = described_methods("a", &text).unwrap();
        assert_eq!(methods[0].role, MethodRole::Compute);
        assert!(methods[0].args.is_empty());
        assert!(!methods[0].rpc);
        assert!(exposed_methods(&methods).is_empty());
    }

    #[test]
    fn without_the_marker_nothing_is_exposed() {
        let mut m = described(ImplKind::Define, "lend", &[]);
        m.rpc = false;
        assert!(rpc_of(&m).is_none());
        assert!(exposed_methods(&[m]).is_empty());
    }

    #[test]
    fn a_malformed_description_is_refused_naming_the_method_and_field() {
        let of = |detail: serde_json::Value| {
            let text = serde_json::json!({"schema": 1, "contributions": [{
                "addon": "a", "kind": "method", "model": "m.x", "name": "go", "detail": detail
            }]})
            .to_string();
            described_methods("a", &text).unwrap_err().to_string()
        };
        for (detail, needle) in [
            (serde_json::json!({}), "m.x.go"),
            (serde_json::json!({}), "no `args` list"),
            (serde_json::json!({"args": "x"}), "no `args` list"),
            (
                serde_json::json!({"args": [{"type": "String"}]}),
                "argument 0 has no `name`",
            ),
            (
                serde_json::json!({"args": [{"name": "a"}]}),
                "argument 0 has no `type`",
            ),
            (
                serde_json::json!({"args": [{"name": "", "type": "i32"}]}),
                "has no `name`",
            ),
            (serde_json::json!({"args": [], "rpc": "yes"}), "`rpc`"),
        ] {
            let err = of(detail.clone());
            assert!(err.contains(needle), "{detail}: {err}");
        }
    }
}
