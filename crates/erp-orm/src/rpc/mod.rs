//! RPC exposure tables and the dynamic argument binder (3 and 4;
//! ;, §5.2, §6).
//!
//! `#[rpc(effect = …)]` on a `#[method]` contributes an [`RpcImpl`] to [`RPC_IMPLS`]: the effect
//! class, the receiver (records or `model`), the parameter table and a binder that turns keyword
//! arguments ([`RpcArgs`]) into the method's typed arguments and calls it through its override
//! chain. `#[rpc(hide)]` contributes an [`RpcDecl::Hide`] entry. Core's own public methods
//! (`search`, `read`, `create`, …) are registered by this crate the same way.
//!
//! [`crate::OrmBuilder::build`] computes one [`ExposureTable`] per registry from those entries,
//! walking each model's method resolution order from the oldest declaration to the newest:
//!
//! otherwise **exposed** when any implementation carries `#[rpc(effect = …)]`; its effect is the
//!   maximum declared along the chain, and an override that declares a *lower* effect than one
//!   below it fails the build naming both locations;
//! - all `#[rpc]` declarations of a chain must agree on the receiver;
//!
//! The table holds no statement and no I/O: `erp-rpc` looks a call up before it opens a
//! transaction.
//!
//! **Data-callable level.** `#[data_callable]` on a `#[method]`
//! contributes an [`RpcDecl::DataCallable`]: the method may be called from a `<function>` of a data
//! file, with the same parameter table and binder as an `#[rpc]` method but no effect class.
//! The RPC lookup ([`ExposureTable::get`]) ignores `DataCallable` entries: a chain with
//! nothing but `#[data_callable]` answers [`Exposure::Internal`].

mod core;
mod relation_count;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use erp_fields::{Error, Result, Value, ValueMap};
use erp_registry::{ModelId, Registry};
use rustc_hash::{FxHashMap, FxHashSet};

pub use erp_registry::{Effect, ParamSpec, Receiver};

use crate::Transaction;
use crate::dispatch::{BoxFuture, Dispatch};
use crate::records::{Model, RecordSet, Records};

/// The binder of an exposed method: takes the call's records (empty for a model method) and its
/// keyword arguments, converts them to the method's typed arguments, calls the method through
/// its override chain and returns the result as a dynamic value.
pub type BindFn =
    for<'a> fn(&'a mut Transaction, Records, RpcArgs) -> BoxFuture<'a, Result<Value, BindError>>;

/// What one implementation declares.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum RpcDecl {
    /// `#[rpc(effect = …)]`: callable, with this effect.
    Expose {
        /// The effect class.
        effect: Effect,
        /// Records or model method.
        receiver: Receiver,
        /// The keyword parameters, in declaration order.
        params: &'static [ParamSpec],
        /// The binder.
        bind: BindFn,
    },
    /// `#[rpc(hide)]`: never callable over RPC, whatever other implementations declare.
    Hide,
    /// `#[data_callable]`: callable from a `<function>` of a data file, not over RPC.
    DataCallable {
        /// Records or model method.
        receiver: Receiver,
        /// The keyword parameters, in declaration order.
        params: &'static [ParamSpec],
        /// The binder.
        bind: BindFn,
    },
}

impl fmt::Debug for RpcDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expose {
                effect,
                receiver,
                params,
                ..
            } => f
                .debug_struct("Expose")
                .field("effect", effect)
                .field("receiver", receiver)
                .field("params", params)
                .finish_non_exhaustive(),
            Self::Hide => f.write_str("Hide"),
            Self::DataCallable {
                receiver, params, ..
            } => f
                .debug_struct("DataCallable")
                .field("receiver", receiver)
                .field("params", params)
                .finish_non_exhaustive(),
        }
    }
}

/// One `#[rpc]` declaration contributed by an addon (or by core).
#[derive(Debug)]
pub struct RpcImpl {
    /// Contributing addon.
    pub addon: &'static str,
    /// Model of the declaration it belongs to (`_name` of the `#[methods]` target).
    pub model: &'static str,
    /// Method name.
    pub method: &'static str,
    /// What it declares.
    pub decl: RpcDecl,
    /// Source file.
    pub file: &'static str,
    /// Source line.
    pub line: u32,
}

impl RpcImpl {
    fn location(&self) -> String {
        format!("{}:{} (addon '{}')", self.file, self.line, self.addon)
    }
}

/// A read method whose transaction must open on the primary even when a read pool is configured
/// (D-4: store answers carry a snapshot descriptor that is only comparable on the
/// primary). The exposure build refuses an entry whose method of an installed addon is not
/// exposed with effect `read`.
#[derive(Debug)]
pub struct PrimaryRead {
    /// Addon that declares it (only entries of installed addons count).
    pub addon: &'static str,
    /// Model of the method.
    pub model: &'static str,
    /// Method name.
    pub method: &'static str,
}

/// Primary-read declarations linked into the binary.
#[linkme::distributed_slice]
pub static PRIMARY_READS: [PrimaryRead];

/// `#[rpc]` declarations linked into the binary.
#[linkme::distributed_slice]
pub static RPC_IMPLS: [RpcImpl];

/// Where a declaration comes from, for error messages and tooling.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclLocation {
    addon: &'static str,
    file: &'static str,
    line: u32,
}

impl DeclLocation {
    fn of(imp: &RpcImpl) -> Self {
        Self {
            addon: imp.addon,
            file: imp.file,
            line: imp.line,
        }
    }

    /// The contributing addon.
    pub fn addon(&self) -> &'static str {
        self.addon
    }

    /// Source file and line.
    pub fn file_line(&self) -> (&'static str, u32) {
        (self.file, self.line)
    }
}

impl fmt::Display for DeclLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{} (addon '{}')", self.file, self.line, self.addon)
    }
}

/// A callable method's entry in the [`ExposureTable`].
#[derive(Clone)]
pub struct Exposed {
    effect: Effect,
    receiver: Receiver,
    params: &'static [ParamSpec],
    bind: BindFn,
    /// The declaration that set the effect (the highest one).
    effect_from: DeclLocation,
    /// A method of a WebAssembly addon: called through the chain's wire form instead of `bind`.
    wire: Option<WireTarget>,
    /// Listed in [`PRIMARY_READS`]: opened on the primary even for a read.
    primary: bool,
}

/// The (model, method) a WebAssembly [`Exposed`] entry calls through the wire form.
#[derive(Debug, Clone, Copy)]
struct WireTarget {
    model: &'static str,
    method: &'static str,
}

/// What a WebAssembly method declares for RPC (the host reads it from the addon's description;
/// [`crate::MethodInvoker::rpc`]). The effect classes the method for the gates (read-only API
/// keys, idempotency); the parameters are named in call order: a client passes them by keyword
/// and they reach the guest as the positional JSON array of its argument tuple.
#[derive(Debug, Clone)]
pub struct WasmRpc {
    /// The effect class.
    pub effect: Effect,
    /// The parameters in call order.
    pub params: Vec<WasmRpcParam>,
}

/// One parameter of a [`WasmRpc`].
#[derive(Debug, Clone)]
pub struct WasmRpcParam {
    /// The keyword name (ASCII letters, digits, `_`).
    pub name: String,
    /// False for an optional (`Option<T>`) parameter: absent is passed as `null`.
    pub required: bool,
    /// The accepted shape named in binding errors.
    pub expected: &'static str,
}

/// Most parameter tables of WebAssembly methods one process keeps; a table is shared by every
/// registry that declares the same parameters, so reloads do not grow it.
const MAX_WIRE_PARAM_TABLES: usize = 16_384;

type ParamTable = Vec<(&'static str, bool, &'static str)>;
type ParamTables = std::collections::HashMap<ParamTable, &'static [ParamSpec]>;

static WIRE_PARAM_TABLES: std::sync::Mutex<Option<ParamTables>> = std::sync::Mutex::new(None);

/// The `'static` parameter table of a WebAssembly method (names interned, equal tables shared).
fn wire_params(params: &[WasmRpcParam]) -> Result<&'static [ParamSpec]> {
    const MAX_PARAMS: usize = 64;
    if params.len() > MAX_PARAMS {
        return Err(Error::new(
            erp_fields::ErrorKind::ResourceExhausted,
            format!("a WebAssembly method has more than {MAX_PARAMS} parameters"),
        ));
    }
    let mut key: ParamTable = Vec::with_capacity(params.len());
    for p in params {
        key.push((crate::dispatch::intern(&p.name)?, p.required, p.expected));
    }
    let mut guard = WIRE_PARAM_TABLES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tables = guard.get_or_insert_with(ParamTables::new);
    if let Some(known) = tables.get(&key) {
        return Ok(known);
    }
    if tables.len() >= MAX_WIRE_PARAM_TABLES {
        return Err(Error::new(
            erp_fields::ErrorKind::ResourceExhausted,
            format!(
                "more than {MAX_WIRE_PARAM_TABLES} distinct WebAssembly parameter tables in this \
                 process"
            ),
        ));
    }
    let leaked: &'static [ParamSpec] = Box::leak(
        key.iter()
            .map(|(name, required, expected)| ParamSpec::new(name, *required, expected))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    tables.insert(key, leaked);
    Ok(leaked)
}

/// The exposure entry of a WebAssembly method that declares RPC and that no native `#[rpc]`
/// declaration covers: callable like a native records method, through the wire form.
fn wasm_exposed(
    registry: &Registry,
    dispatch: &Dispatch,
    model: ModelId,
    method: &'static str,
) -> Result<Option<Exposed>> {
    use crate::dispatch::{ImplKind, MethodDispatchTarget};
    let declared = dispatch
        .targets(model, method)
        .into_iter()
        .find_map(|target| match target {
            MethodDispatchTarget::Wasm(invoker) if invoker.kind() == ImplKind::Define => {
                invoker.rpc().map(|rpc| (invoker.addon().to_owned(), rpc))
            }
            _ => None,
        });
    let Some((addon, rpc)) = declared else {
        return Ok(None);
    };
    let model_name = registry.model_by_id(model).name;
    Ok(Some(Exposed {
        effect: rpc.effect,
        receiver: Receiver::Records,
        params: wire_params(&rpc.params)?,
        bind: wire_bind_unreachable,
        effect_from: DeclLocation {
            addon: crate::dispatch::intern(&addon)?,
            file: "<webassembly addon>",
            line: 0,
        },
        wire: Some(WireTarget {
            model: model_name,
            method,
        }),
        primary: false,
    }))
}

/// The `bind` of a wire entry is never used (`Exposed::call` takes the wire path first).
fn wire_bind_unreachable<'a>(
    _: &'a mut Transaction,
    _: Records,
    _: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async {
        Err(BindError::Call(Error::programming(
            "a WebAssembly method was bound through the native binder",
        )))
    })
}

impl Exposed {
    /// The chain's effect (the maximum declared along it).
    pub fn effect(&self) -> Effect {
        self.effect
    }

    /// Whether the method is a primary read ([`PRIMARY_READS`]): its read-only transaction must be
    /// opened on the primary, never the read pool (D-4).
    pub fn primary_read(&self) -> bool {
        self.primary
    }

    /// Records or model method.
    pub fn receiver(&self) -> Receiver {
        self.receiver
    }

    /// The keyword parameters of the defining declaration.
    pub fn params(&self) -> &'static [ParamSpec] {
        self.params
    }

    /// The declaration that set the effect.
    pub fn effect_from(&self) -> &DeclLocation {
        &self.effect_from
    }

    /// Bind `args` and call the method on `records`.
    ///
    /// # Errors
    ///
    /// [`BindError::Argument`] when an argument cannot be converted (callers check names and
    /// required arguments against [`Exposed::params`] first); [`BindError::Call`] with the
    /// method's own error.
    pub async fn call(
        &self,
        tx: &mut Transaction,
        records: Records,
        args: RpcArgs,
    ) -> Result<Value, BindError> {
        if let Some(wire) = self.wire {
            return self.call_wire(wire, tx, records, args).await;
        }
        (self.bind)(tx, records, args).await
    }

    /// A WebAssembly method: keyword arguments to the positional JSON of the argument tuple, the
    /// chain's wire entry, the JSON result back as a value. Access is the method's own, through
    /// the ORM as the caller (the guest runs under the records' environment).
    async fn call_wire(
        &self,
        wire: WireTarget,
        tx: &mut Transaction,
        records: Records,
        mut args: RpcArgs,
    ) -> Result<Value, BindError> {
        if records.spec().name != wire.model {
            return Err(BindError::Call(Error::programming(format!(
                "{}.{} was called on records of model '{}'",
                wire.model,
                wire.method,
                records.spec().name
            ))));
        }
        let mut tuple: Vec<erp_fields::serde_json::Value> = Vec::with_capacity(self.params.len());
        for param in self.params {
            let value = args.take::<Option<Value>>(param.name())?;
            let json = match value {
                // absent was checked by the service; an explicit `null` (read as absent here) is
                // as missing as an absent required argument
                None if param.required() => {
                    return Err(BindError::Argument {
                        name: param.name(),
                        expected: param.expected(),
                        error: ArgError::Missing,
                    });
                }
                None => erp_fields::serde_json::Value::Null,
                Some(v) => {
                    // the shape a native binder checks, before the guest runs
                    let fits = match param.expected() {
                        "a string" => matches!(v, Value::Str(_)),
                        "a boolean" => matches!(v, Value::Bool(_)),
                        "an integer" => matches!(v, Value::Int(_)),
                        "a number" => matches!(v, Value::Int(_) | Value::Float(_)),
                        _ => true,
                    };
                    if !fits {
                        return Err(BindError::Argument {
                            name: param.name(),
                            expected: param.expected(),
                            error: ArgError::Mismatch { got: json_type(&v) },
                        });
                    }
                    erp_fields::convert::value_to_json(&v).map_err(|error| BindError::Argument {
                        name: param.name(),
                        expected: param.expected(),
                        error: ArgError::Invalid(error),
                    })?
                }
            };
            tuple.push(json);
        }
        let text = if tuple.is_empty() {
            "null".to_owned()
        } else {
            erp_fields::serde_json::Value::Array(tuple).to_string()
        };
        let result = crate::dispatch::call_method_json(tx, &records, wire.method, &text)
            .await
            .map_err(BindError::Call)?;
        let parsed: erp_fields::serde_json::Value = erp_fields::serde_json::from_str(&result)
            .map_err(|e| {
                BindError::Call(Error::internal(format!(
                    "{}.{}: the WebAssembly method returned text that is not JSON: {e}",
                    wire.model, wire.method
                )))
            })?;
        Ok(Value::Json(Arc::new(parsed)))
    }
}

impl fmt::Debug for Exposed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exposed")
            .field("effect", &self.effect)
            .field("receiver", &self.receiver)
            .field("params", &self.params)
            .field("effect_from", &self.effect_from)
            .finish_non_exhaustive()
    }
}

/// A method a `<function>` of a data file may call.
#[derive(Clone)]
pub struct DataCallable {
    model_name: &'static str,
    method: &'static str,
    receiver: Receiver,
    params: &'static [ParamSpec],
    bind: BindFn,
    /// The defining (oldest) declaration of the chain that is `#[rpc]` or `#[data_callable]`.
    declared_at: DeclLocation,
}

impl DataCallable {
    /// Records or model method.
    pub fn receiver(&self) -> Receiver {
        self.receiver
    }

    /// The keyword parameters of the defining declaration, in declaration order (positional
    /// arguments of a `<function>` bind to them in this order).
    pub fn params(&self) -> &'static [ParamSpec] {
        self.params
    }

    /// The declaration whose parameter table and binder are used.
    pub fn declared_at(&self) -> &DeclLocation {
        &self.declared_at
    }

    /// Bind `args` and call the method on `records` (empty for a model method).
    ///
    /// # Errors
    ///
    /// [`BindError::Call`] with a `Programming` error when `records` is not a recordset of the
    /// callable's model, or is not empty for a model method; [`BindError::Argument`] when an
    /// argument cannot be converted (callers check names against [`DataCallable::params`]
    /// first); [`BindError::Call`] with the method's own error.
    pub async fn call(
        &self,
        tx: &mut Transaction,
        records: Records,
        args: RpcArgs,
    ) -> Result<Value, BindError> {
        if records.spec().name != self.model_name {
            return Err(BindError::Call(Error::programming(format!(
                "{}.{} was called on records of model '{}'",
                self.model_name,
                self.method,
                records.spec().name
            ))));
        }
        if self.receiver == Receiver::Model && !records.is_empty() {
            return Err(BindError::Call(Error::programming(format!(
                "{}.{} is a model method and takes no records, but was called on {} record(s)",
                self.model_name,
                self.method,
                records.len()
            ))));
        }
        (self.bind)(tx, records, args).await
    }
}

impl fmt::Debug for DataCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DataCallable")
            .field("receiver", &self.receiver)
            .field("params", &self.params)
            .field("declared_at", &self.declared_at)
            .finish_non_exhaustive()
    }
}

impl Transaction {
    /// The data-callable entry of `model.method` in this transaction's registry, if the method
    /// may be called from a `<function>` of a data file (see [`ExposureTable::data_callable`]).
    pub fn data_callable(&self, model: ModelId, method: &str) -> Option<&DataCallable> {
        self.shared.exposure.data_callable(model, method)
    }

    /// How `model.method` answers over RPC in this transaction's registry (`None` when the model
    /// has no such method). A layer above the ORM that lets untrusted code name methods (the WASM
    /// host) re-checks it at the moment of the call.
    pub fn exposure(&self) -> &ExposureTable {
        &self.shared.exposure
    }

    /// Whether `model` has a method called `method` at all (callable or not): the loader tells
    /// "no such method" from "not data-callable" with it.
    pub fn has_method(&self, model: ModelId, method: &str) -> bool {
        // every chain key has an exposure entry, a data-callable one included
        self.shared.exposure.get(model, method).is_some()
    }
}

/// How a (model, method) answers over RPC.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Exposure {
    /// Callable.
    Exposed(Exposed),
    /// `#[rpc(hide)]` somewhere in the chain: 403.
    Hidden {
        /// The first hiding declaration (oldest first).
        by: DeclLocation,
    },
    /// The method exists (it has an override chain) but nothing exposes it: 403.
    Internal,
}

/// Exposure of every (model, method) of one registry ([`crate::Orm::exposure`]).
#[derive(Debug, Default)]
pub struct ExposureTable {
    entries: FxHashMap<ModelId, FxHashMap<&'static str, Exposure>>,
    data: FxHashMap<ModelId, FxHashMap<&'static str, DataCallable>>,
}

impl ExposureTable {
    /// Build the table from the linked declarations, core's own and `extra`, for the models of
    /// `registry` (only declarations of installed addons count).
    pub(crate) fn build(
        registry: &Registry,
        dispatch: &Dispatch,
        extra: &[&'static RpcImpl],
    ) -> Result<Self> {
        let installed: FxHashSet<&str> = registry.addons().iter().map(|a| a.name).collect();
        let mut by_owner: FxHashMap<(&str, &str), Vec<&'static RpcImpl>> = FxHashMap::default();
        for imp in RPC_IMPLS
            .iter()
            .chain(core::CORE_RPC.iter().copied())
            .chain(extra.iter().copied())
            .filter(|i| installed.contains(i.addon))
        {
            by_owner
                .entry((imp.model, imp.addon))
                .or_default()
                .push(imp);
        }
        let mut owners: Vec<(&str, &str)> = by_owner.keys().copied().collect();
        owners.sort_unstable();
        for owner in &owners {
            let Some(list) = by_owner.get_mut(owner) else {
                continue;
            };
            // one `#[rpc]` (effect or hide) and one `#[data_callable]` per method are allowed
            list.sort_by_key(|i| (i.method, is_data_callable(i), i.file, i.line));
            if let Some(pair) = list.windows(2).find(|w| {
                w[0].method == w[1].method && is_data_callable(w[0]) == is_data_callable(w[1])
            }) {
                let what = if is_data_callable(pair[0]) {
                    "#[data_callable]"
                } else {
                    "#[rpc]"
                };
                return Err(Error::programming(format!(
                    "method '{}' of model '{}' has two {what} declarations in addon '{}': {} and \
                     {}",
                    pair[0].method,
                    pair[0].model,
                    pair[0].addon,
                    pair[0].location(),
                    pair[1].location()
                )));
            }
        }
        let mut entries: FxHashMap<ModelId, FxHashMap<&'static str, Exposure>> =
            FxHashMap::default();
        let mut data: FxHashMap<ModelId, FxHashMap<&'static str, DataCallable>> =
            FxHashMap::default();
        for model in registry.models() {
            // newest first in the MRO; walk from the oldest declaration up
            let mut seen: Vec<(&str, &str)> = Vec::new();
            let mut per_method: BTreeMap<&'static str, Vec<&'static RpcImpl>> = BTreeMap::new();
            for decl in model.mro.iter().rev() {
                let owner = (decl.name, decl.addon);
                if seen.contains(&owner) {
                    continue;
                }
                seen.push(owner);
                for imp in by_owner.get(&owner).map(Vec::as_slice).unwrap_or_default() {
                    per_method.entry(imp.method).or_default().push(*imp);
                }
            }
            let methods = entries.entry(model.id).or_default();
            let callable = data.entry(model.id).or_default();
            for (method, chain) in per_method {
                methods.insert(method, resolve_chain(model.name, method, &chain)?);
                if let Some(entry) = resolve_data_callable(model.name, method, &chain)? {
                    callable.insert(method, entry);
                }
            }
        }
        for (model, method) in dispatch.chain_keys() {
            let slot = entries.entry(model).or_default();
            if slot.contains_key(method) {
                continue;
            }
            // a method a WebAssembly addon introduces and declares for RPC (no native `#[rpc]`
            // covers it); anything else with a chain stays internal
            let exposure = match wasm_exposed(registry, dispatch, model, method)? {
                Some(exposed) => Exposure::Exposed(exposed),
                None => Exposure::Internal,
            };
            slot.insert(method, exposure);
        }
        for entry in PRIMARY_READS.iter().filter(|e| installed.contains(e.addon)) {
            let found = registry
                .model_id(entry.model)
                .and_then(|model| entries.get_mut(&model)?.get_mut(entry.method));
            match found {
                Some(Exposure::Exposed(exposed)) if exposed.effect == Effect::Read => {
                    exposed.primary = true;
                }
                _ => {
                    return Err(Error::programming(format!(
                        "primary read '{}.{}' (addon '{}') is not an exposed method with effect = \
                         read",
                        entry.model, entry.method, entry.addon
                    )));
                }
            }
        }
        // the call names the concrete model of a record (`doc.a`), not the mixin that declares the
        // method (`mail.thread`): every model that inherits the entry's model carries the flag too
        for entry in PRIMARY_READS.iter().filter(|e| installed.contains(e.addon)) {
            for model in registry.models() {
                if model.name == entry.model || !model.mro.iter().any(|d| d.name == entry.model) {
                    continue;
                }
                if let Some(Exposure::Exposed(exposed)) = entries
                    .get_mut(&model.id)
                    .and_then(|methods| methods.get_mut(entry.method))
                    && exposed.effect == Effect::Read
                {
                    exposed.primary = true;
                }
            }
        }
        Ok(Self { entries, data })
    }

    /// How `model.method` answers; `None` when the method is unknown on the model.
    pub fn get(&self, model: ModelId, method: &str) -> Option<&Exposure> {
        self.entries.get(&model)?.get(method)
    }

    /// The data-callable entry of `model.method`: `Some` when an implementation along the method's
    /// chain declares `#[data_callable]` or `#[rpc(effect = …)]`, also when another one carries
    /// `#[rpc(hide)]`.
    pub fn data_callable(&self, model: ModelId, method: &str) -> Option<&DataCallable> {
        self.data.get(&model)?.get(method)
    }

    /// Every (method, exposure) of `model`, sorted by method name (for the parity test and the
    /// tool registry).
    pub fn methods(&self, model: ModelId) -> Vec<(&'static str, &Exposure)> {
        let mut out: Vec<(&'static str, &Exposure)> = self
            .entries
            .get(&model)
            .into_iter()
            .flat_map(|methods| methods.iter().map(|(name, exposure)| (*name, exposure)))
            .collect();
        out.sort_unstable_by_key(|(name, _)| *name);
        out
    }
}

fn is_data_callable(imp: &RpcImpl) -> bool {
    matches!(imp.decl, RpcDecl::DataCallable { .. })
}

/// Apply the chain rules to the declarations of one (model, method), oldest first.
fn resolve_chain(model: &str, method: &str, chain: &[&'static RpcImpl]) -> Result<Exposure> {
    let mut hidden: Option<DeclLocation> = None;
    // (effect, receiver, params, bind, where the effect came from)
    let mut current: Option<(Effect, Receiver, &'static [ParamSpec], BindFn, &RpcImpl)> = None;
    let mut receiver_from: Option<(Receiver, &RpcImpl)> = None;
    for imp in chain {
        match imp.decl {
            // the data-callable level has its own table (`resolve_data_callable`)
            RpcDecl::DataCallable { .. } => {}
            RpcDecl::Hide => {
                if hidden.is_none() {
                    hidden = Some(DeclLocation::of(imp));
                }
            }
            RpcDecl::Expose {
                effect,
                receiver,
                params,
                bind,
            } => {
                if let Some((first, at)) = receiver_from {
                    if first != receiver {
                        return Err(Error::programming(format!(
                            "{model}.{method}: addon '{}' declares a {} method at {}:{} but \
                             addon '{}' declares a {} method at {}:{}; #[rpc(…, model)] must \
                             agree along the chain",
                            imp.addon,
                            receiver_name(receiver),
                            imp.file,
                            imp.line,
                            at.addon,
                            receiver_name(first),
                            at.file,
                            at.line
                        )));
                    }
                } else {
                    receiver_from = Some((receiver, imp));
                }
                current = match current {
                    None => Some((effect, receiver, params, bind, imp)),
                    Some((below, _, _, _, at)) if effect < below => {
                        return Err(Error::programming(format!(
                            "{model}.{method}: addon '{}' declares effect = {effect} below effect \
                             = {below} declared by '{}' at {}:{}; an override may only widen an \
                             effect",
                            imp.addon, at.addon, at.file, at.line
                        )));
                    }
                    // the defining (oldest) declaration keeps its parameter table and binder;
                    // the effect is the maximum
                    Some((below, r, p, b, at)) if effect == below => Some((below, r, p, b, at)),
                    Some((_, r, p, b, _)) => Some((effect, r, p, b, imp)),
                };
            }
        }
    }
    if let Some(by) = hidden {
        return Ok(Exposure::Hidden { by });
    }
    match current {
        Some((effect, receiver, params, bind, from)) => Ok(Exposure::Exposed(Exposed {
            effect,
            receiver,
            params,
            bind,
            effect_from: DeclLocation::of(from),
            wire: None,
            primary: false,
        })),
        // nothing but `#[data_callable]` declarations: not exposed over RPC, but the method exists
        None if !chain.is_empty() => Ok(Exposure::Internal),
        None => Err(Error::programming(format!(
            "{model}.{method}: an empty chain of declarations"
        ))),
    }
}

/// The data-callable entry of one (model, method), from its declarations oldest first:
/// `#[rpc(effect = …)]` and `#[data_callable]` both count, `#[rpc(hide)]` is ignored. All of them
/// must agree on the receiver; the defining (oldest) one keeps its parameter table and binder.
fn resolve_data_callable(
    model: &'static str,
    method: &'static str,
    chain: &[&'static RpcImpl],
) -> Result<Option<DataCallable>> {
    let mut found: Option<(Receiver, &'static [ParamSpec], BindFn, &RpcImpl)> = None;
    for imp in chain {
        let (receiver, params, bind) = match imp.decl {
            RpcDecl::Hide => continue,
            RpcDecl::Expose {
                receiver,
                params,
                bind,
                ..
            }
            | RpcDecl::DataCallable {
                receiver,
                params,
                bind,
            } => (receiver, params, bind),
        };
        match found {
            None => found = Some((receiver, params, bind, imp)),
            Some((first, _, _, at)) if first != receiver => {
                return Err(Error::programming(format!(
                    "{model}.{method}: addon '{}' declares a {} method at {}:{} but addon '{}' \
                     declares a {} method at {}:{}; the receiver of #[rpc(…, model)] and \
                     #[data_callable(model)] must agree along the chain",
                    imp.addon,
                    receiver_name(receiver),
                    imp.file,
                    imp.line,
                    at.addon,
                    receiver_name(first),
                    at.file,
                    at.line
                )));
            }
            Some(_) => {}
        }
    }
    Ok(found.map(|(receiver, params, bind, at)| DataCallable {
        model_name: model,
        method,
        receiver,
        params,
        bind,
        declared_at: DeclLocation::of(at),
    }))
}

fn receiver_name(receiver: Receiver) -> &'static str {
    match receiver {
        Receiver::Records => "records",
        Receiver::Model => "model",
    }
}

// ---- arguments --------------------------------------------------------------------------------

/// Keyword arguments of one call, by name, as dynamic values (JSON `null` is [`Value::False`],
/// JSON `false` is `Bool(false)`). Built by `erp-rpc` after checking names and required
/// parameters against [`Exposed::params`].
#[derive(Debug, Clone, Default)]
pub struct RpcArgs {
    values: ValueMap,
}

impl RpcArgs {
    /// Arguments from a map.
    pub fn new(values: ValueMap) -> Self {
        Self { values }
    }

    /// Take argument `name` (absent counts as `None`) and convert it.
    ///
    /// # Errors
    ///
    /// [`BindError::Argument`] naming the parameter when the value has the wrong shape or is
    /// missing for a required parameter.
    pub fn take<T: FromRpc>(&mut self, name: &'static str) -> Result<T, BindError> {
        let value = self.values.shift_remove(name);
        T::from_rpc(value).map_err(|error| BindError::Argument {
            name,
            expected: T::EXPECTED,
            error,
        })
    }

    /// Names still present (none after a binder took every parameter).
    pub fn remaining(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

/// Why one argument could not be converted.
#[derive(Debug)]
#[non_exhaustive]
pub enum ArgError {
    /// A required argument is absent.
    Missing,
    /// The JSON shape is not the expected one; `got` names the JSON type received.
    Mismatch {
        /// `null`, `boolean`, `integer`, `number`, `string`, `array` or `object`.
        got: &'static str,
    },
    /// An integer outside the accepted range (a negative `limit`); the message names the value,
    /// since "expected a non-negative integer, got integer" would not say what is wrong.
    OutOfRange {
        /// The integer received.
        got: i64,
    },
    /// The shape is right but the value is invalid (a malformed domain, bad base64); the error
    /// keeps its kind (`Validation`).
    Invalid(Error),
}

/// A binder failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum BindError {
    /// Argument `name` could not be converted to `expected`.
    Argument {
        /// The parameter.
        name: &'static str,
        /// What it accepts.
        expected: &'static str,
        /// Why it failed.
        error: ArgError,
    },
    /// The method ran and failed, or the result could not be converted.
    Call(Error),
}

impl From<Error> for BindError {
    fn from(error: Error) -> Self {
        Self::Call(error)
    }
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument {
                name,
                expected,
                error,
            } => match error {
                ArgError::Missing => write!(f, "missing a required argument: '{name}'"),
                ArgError::Mismatch { got } => {
                    write!(f, "argument '{name}': expected {expected}, got {got}")
                }
                ArgError::OutOfRange { got } => {
                    write!(f, "argument '{name}': expected {expected}, got {got}")
                }
                ArgError::Invalid(e) => write!(f, "argument '{name}': {e}"),
            },
            Self::Call(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Argument {
                error: ArgError::Invalid(e),
                ..
            }
            | Self::Call(e) => Some(e),
            Self::Argument { .. } => None,
        }
    }
}

/// The JSON type name of a dynamic value built from JSON (binding error messages).
pub fn json_type(value: &Value) -> &'static str {
    match value {
        Value::False => "null",
        Value::Bool(_) => "boolean",
        Value::Int(_) => "integer",
        Value::Float(_) | Value::Decimal(_) => "number",
        Value::Str(_) | Value::Bytes(_) | Value::Date(_) | Value::DateTime(_) => "string",
        // a range renders as `[lower, upper]`
        Value::List(_) | Value::Range(_) | Value::Rows(_) => "array",
        Value::Map(_) | Value::Row(_) => "object",
        Value::Json(j) => match &**j {
            erp_fields::serde_json::Value::Null => "null",
            erp_fields::serde_json::Value::Bool(_) => "boolean",
            erp_fields::serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
            erp_fields::serde_json::Value::Number(_) => "number",
            erp_fields::serde_json::Value::String(_) => "string",
            erp_fields::serde_json::Value::Array(_) => "array",
            erp_fields::serde_json::Value::Object(_) => "object",
        },
        // a variant added to `Value` later: named as such in the message, never as another type
        _ => "unknown",
    }
}

/// A method parameter type the binder can fill from a keyword argument.
///
/// Conversions are lossless: integers take JSON integers only, floats take
/// integers and numbers, booleans take booleans and the integers `0`/`1`, strings take strings.
/// `Option<T>` makes a parameter optional: absent or `null` gives `None`.
pub trait FromRpc: Sized + Send + 'static {
    /// The accepted JSON shape, as named in binding errors.
    const EXPECTED: &'static str;
    /// Whether a call must pass the argument.
    const REQUIRED: bool = true;

    /// Convert the argument (`None` when absent).
    ///
    /// # Errors
    ///
    /// [`ArgError`] when it is absent (required types), of the wrong shape or invalid.
    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError>;
}

pub(super) fn present(value: Option<Value>) -> Result<Value, ArgError> {
    value.ok_or(ArgError::Missing)
}

pub(super) fn mismatch<T>(value: &Value) -> Result<T, ArgError> {
    Err(ArgError::Mismatch {
        got: json_type(value),
    })
}

impl<T: FromRpc> FromRpc for Option<T> {
    const EXPECTED: &'static str = T::EXPECTED;
    const REQUIRED: bool = false;

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match value {
            None | Some(Value::False) => Ok(None),
            Some(v) => T::from_rpc(Some(v)).map(Some),
        }
    }
}

impl FromRpc for bool {
    const EXPECTED: &'static str = "a boolean";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match present(value)? {
            Value::Bool(b) => Ok(b),
            Value::Int(0) => Ok(false),
            Value::Int(1) => Ok(true),
            other => mismatch(&other),
        }
    }
}

impl FromRpc for i64 {
    const EXPECTED: &'static str = "an integer";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match present(value)? {
            Value::Int(i) => Ok(i),
            other => mismatch(&other),
        }
    }
}

impl FromRpc for u64 {
    const EXPECTED: &'static str = "a non-negative integer";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match present(value)? {
            Value::Int(i) => u64::try_from(i).map_err(|_| ArgError::OutOfRange { got: i }),
            other => mismatch(&other),
        }
    }
}

impl FromRpc for f64 {
    const EXPECTED: &'static str = "a number";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match present(value)? {
            Value::Float(f) => Ok(f),
            // lossless only: every integer of magnitude up to 2^53 is an exact float
            Value::Int(i) if i.unsigned_abs() <= 1 << 53 => Ok(i as f64),
            other => mismatch(&other),
        }
    }
}

impl FromRpc for String {
    const EXPECTED: &'static str = "a string";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match present(value)? {
            Value::Str(s) => Ok(s.to_string()),
            other => mismatch(&other),
        }
    }
}

impl FromRpc for Value {
    const EXPECTED: &'static str = "any value";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        present(value)
    }
}

impl FromRpc for ValueMap {
    const EXPECTED: &'static str = "an object";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match present(value)? {
            Value::Map(map) => Ok(Arc::unwrap_or_clone(map)),
            other => mismatch(&other),
        }
    }
}

impl<T: FromRpc> FromRpc for Vec<T> {
    const EXPECTED: &'static str = "an array";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match present(value)? {
            Value::List(items) => items
                .iter()
                .map(|item| T::from_rpc(Some(item.clone())))
                .collect(),
            other => mismatch(&other),
        }
    }
}

impl FromRpc for erp_domain::Domain {
    const EXPECTED: &'static str = "a domain (array)";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        let value = present(value)?;
        match &value {
            Value::List(_) | Value::Bool(_) => {
                let json = erp_fields::convert::value_to_json(&value).map_err(ArgError::Invalid)?;
                erp_domain::Domain::from_json(&json).map_err(|e| ArgError::Invalid(e.into()))
            }
            other => mismatch(other),
        }
    }
}

impl FromRpc for crate::web::Spec {
    const EXPECTED: &'static str = "a specification (object)";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        let value = present(value)?;
        match &value {
            Value::Map(_) => crate::web::Spec::from_value(&value).map_err(ArgError::Invalid),
            other => mismatch(other),
        }
    }
}

/// Binary data over RPC: standard padded base64 (RFC 4648 §4) in, decoded strictly; base64 out
/// .
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Binary(pub Vec<u8>);

impl FromRpc for Binary {
    const EXPECTED: &'static str = "a base64 string";

    fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
        match present(value)? {
            Value::Str(s) => erp_fields::base64::decode_strict(&s)
                .map(Binary)
                .map_err(|e| {
                    ArgError::Invalid(Error::validation(format!("Invalid base64 value: {e}")))
                }),
            other => mismatch(&other),
        }
    }
}

/// A method result the binder can return as a dynamic value.
pub trait IntoRpc {
    /// Convert the result.
    ///
    /// # Errors
    ///
    /// `Programming` when the value cannot be represented (an unsigned integer above `i64::MAX`).
    fn into_rpc(self) -> Result<Value>;
}

/// JSON `null` (Python's `None`), as the binder returns it for `()`.
pub fn null() -> Value {
    Value::Json(Arc::new(erp_fields::serde_json::Value::Null))
}

impl IntoRpc for () {
    fn into_rpc(self) -> Result<Value> {
        Ok(null())
    }
}

impl IntoRpc for bool {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::Bool(self))
    }
}

impl IntoRpc for i64 {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::Int(self))
    }
}

impl IntoRpc for i32 {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::Int(i64::from(self)))
    }
}

impl IntoRpc for u64 {
    fn into_rpc(self) -> Result<Value> {
        i64::try_from(self).map(Value::Int).map_err(|_| {
            Error::programming(format!(
                "result {self} does not fit a signed 64-bit integer"
            ))
        })
    }
}

impl IntoRpc for f64 {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::Float(self))
    }
}

impl IntoRpc for String {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::str(self))
    }
}

impl IntoRpc for Arc<str> {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::Str(self))
    }
}

impl IntoRpc for Value {
    fn into_rpc(self) -> Result<Value> {
        Ok(self)
    }
}

impl IntoRpc for ValueMap {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::Map(Arc::new(self)))
    }
}

impl IntoRpc for BTreeMap<String, Value> {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::Map(Arc::new(self.into_iter().collect())))
    }
}

impl IntoRpc for Binary {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::str(erp_fields::base64::encode(&self.0)))
    }
}

impl IntoRpc for crate::web::FormattedGroups {
    fn into_rpc(self) -> Result<Value> {
        Ok(self.to_value())
    }
}

impl<T: IntoRpc> IntoRpc for Option<T> {
    fn into_rpc(self) -> Result<Value> {
        match self {
            Some(v) => v.into_rpc(),
            None => Ok(Value::False),
        }
    }
}

impl<T: IntoRpc> IntoRpc for Vec<T> {
    fn into_rpc(self) -> Result<Value> {
        let items: Result<Vec<Value>> = self.into_iter().map(IntoRpc::into_rpc).collect();
        Ok(Value::List(items?.into()))
    }
}

impl<A: IntoRpc, B: IntoRpc> IntoRpc for (A, B) {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::list([self.0.into_rpc()?, self.1.into_rpc()?]))
    }
}

impl IntoRpc for Records {
    fn into_rpc(self) -> Result<Value> {
        Ok(Value::list(self.ids().into_iter().map(Value::Int)))
    }
}

impl<M: Model> IntoRpc for RecordSet<M> {
    fn into_rpc(self) -> Result<Value> {
        self.as_records().clone().into_rpc()
    }
}

#[cfg(test)]
mod tests;
