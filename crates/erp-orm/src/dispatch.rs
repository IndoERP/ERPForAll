//! Method dispatch with override chains (Layer 2,, design §7).
//!
//! `#[methods(Model)]` blocks contribute [`MethodImpl`] entries to [`METHOD_IMPLS`]. For each
//! (model, method) the chain lists the implementations of the declarations in the model's method
//! resolution order, newest first, so an override's `Super::call` reaches the previously loaded
//! addon's code exactly like Python's `super()`. Entries are type-erased function pointers; every
//! entry of a chain must have the same signature, checked once when the dispatch table is built.
//!
//! Build errors (all name the source file and line): two signatures in one chain; an override with
//! nothing below it; two plain definitions in one chain (the later one must be
//! `#[method(override)]`); an implementation whose addon has no declaration of the
//! model (`#[methods]` without a matching `#[model]`/`#[extend]`, D5); one addon implementing a
//! method twice.

use std::any::{Any, TypeId};
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use erp_fields::{Error, Result};
use erp_registry::{ModelId, Registry};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Transaction;
use crate::records::Records;

/// A boxed `Send` future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The erased signature of a method taking arguments `A` (a tuple) and returning `R`.
pub type MethodFn<A, R> =
    for<'a> fn(&'a mut Transaction, &'a Records, A, Super<A, R>) -> BoxFuture<'a, Result<R>>;

/// Whether an implementation introduces a method or overrides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplKind {
    /// `#[method]`: the bottom of a chain.
    Define,
    /// `#[method(override)]`: calls further down through [`Super`].
    Override,
}

/// One implementation contributed by an addon.
#[derive(Debug)]
pub struct MethodImpl {
    /// Contributing addon.
    pub addon: &'static str,
    /// Model of the declaration it belongs to (`_name` of the `#[methods]` target).
    pub model: &'static str,
    /// Method name.
    pub method: &'static str,
    /// Define or override.
    pub kind: ImplKind,
    /// Human-readable signature for error messages.
    pub sig_name: fn() -> &'static str,
    /// The erased entry (a `&'static MethodFn<A, R>`); its `TypeId` is the signature.
    pub entry: fn() -> &'static (dyn Any + Send + Sync),
    /// Source file.
    pub file: &'static str,
    /// Source line.
    pub line: u32,
}

impl MethodImpl {
    fn location(&self) -> String {
        format!("{}:{} (addon '{}')", self.file, self.line, self.addon)
    }

    /// The signature, derived from the entry itself (so it cannot disagree with it).
    fn sig(&self) -> TypeId {
        (self.entry)().type_id()
    }
}

/// Method implementations linked into the binary.
#[linkme::distributed_slice]
pub static METHOD_IMPLS: [MethodImpl];

/// What a chain entry is built from: a function linked into the binary, or a WebAssembly export.
///
/// Both kinds sit in one chain, ordered by `(depth, order_name)` of their addons exactly like
/// native overrides, and `super` crosses from either kind to either kind
/// . `erp-orm` knows the WASM side only through [`MethodInvoker`]: no WebAssembly
/// runtime type appears in this crate.
#[derive(Clone)]
pub enum MethodDispatchTarget {
    /// A `#[methods]` function linked into the binary.
    Native(&'static MethodImpl),
    /// An export of a WebAssembly addon, run by the invoker.
    Wasm(Arc<dyn MethodInvoker>),
}

impl MethodDispatchTarget {
    /// The contributing addon.
    pub fn addon(&self) -> &str {
        match self {
            Self::Native(imp) => imp.addon,
            Self::Wasm(invoker) => invoker.addon(),
        }
    }

    /// Define or override.
    pub fn kind(&self) -> ImplKind {
        match self {
            Self::Native(imp) => imp.kind,
            Self::Wasm(invoker) => invoker.kind(),
        }
    }

    /// Whether this entry runs in a WebAssembly sandbox.
    pub fn is_wasm(&self) -> bool {
        matches!(self, Self::Wasm(_))
    }

    fn location(&self) -> String {
        match self {
            Self::Native(imp) => imp.location(),
            Self::Wasm(invoker) => format!("WASM addon '{}'", invoker.addon()),
        }
    }
}

impl fmt::Debug for MethodDispatchTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(imp) => write!(f, "Native({})", imp.location()),
            Self::Wasm(invoker) => write!(f, "Wasm({})", invoker.addon()),
        }
    }
}

/// Runs one method implementation that lives outside the binary (a WebAssembly export). Implemented
/// by `erp-wasm-host`; `erp-orm` calls it through [`MethodDispatchTarget::Wasm`] and never sees the
/// runtime behind it.
///
/// Arguments and result are the method's wire form: JSON text, the serialisation of the argument
/// tuple (an array, in declaration order) and of the result.
/// `sup` is the rest of the chain below this entry; the invoker reaches it for the guest's
/// `super()` through [`DynSuper::call_json`], with the same transaction.
///
/// An error returned from `invoke` fails the whole method call; the caller rolls the transaction
/// back as it does for any other method error.
pub trait MethodInvoker: Send + Sync + fmt::Debug {
    /// The addon this implementation belongs to.
    fn addon(&self) -> &str;

    /// Whether it introduces the method or overrides it.
    fn kind(&self) -> ImplKind;

    /// Run the implementation on `records`.
    fn invoke<'a>(
        &'a self,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
        sup: DynSuper,
    ) -> BoxFuture<'a, Result<String>>;

    /// How an RPC client may call this method, when this entry introduces it and the host wants
    /// it callable from the web client and `call_kw`: the exposure table adds a
    /// wire-form entry for it, with the same checks as a native `#[rpc]` method. The
    /// default is `None`: not callable over RPC.
    fn rpc(&self) -> Option<crate::rpc::WasmRpc> {
        None
    }
}

/// A WebAssembly implementation to add to the dispatch tables of one registry
/// ([`crate::OrmBuilder::wasm_methods`]). The addon must be installed in the registry and declare
/// the model there (`#[extend]`), like a native addon; while it is listed here, its linked native
/// implementations are not used (an addon runs in one mode).
#[derive(Clone)]
pub struct WasmMethodImpl {
    /// `_name` of the model it belongs to.
    pub model: String,
    /// Method name.
    pub method: String,
    /// Runs it.
    pub invoker: Arc<dyn MethodInvoker>,
}

impl fmt::Debug for WasmMethodImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WasmMethodImpl({}.{} of {})",
            self.model,
            self.method,
            self.invoker.addon()
        )
    }
}

/// Longest WebAssembly method or model name accepted, in bytes (the bridge's own cap).
pub(crate) const MAX_WASM_NAME_BYTES: usize = 128;

/// Most distinct method names a process interns for loaded packages. A package that declares
/// fresh names on every probe cannot grow the process without bound (security review M3).
const MAX_INTERNED_NAMES: usize = 65_536;

/// Most WebAssembly method implementations one registry build accepts, over all loaded packages
/// together: a build over the cap fails before any name is interned (G security review r2 N1).
/// Each package is limited further, to `MAX_DESCRIBED_METHODS` of `erp-wasm-host`, when its
/// `describe()` is read.
pub const MAX_WASM_IMPLS_PER_BUILD: usize = 16_384;

/// Whether `name` is a method name a package may declare: ASCII letters, digits and `_`.
pub(crate) fn valid_wasm_method(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_WASM_NAME_BYTES
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `name` is a model name a package may declare: lower-case ASCII, digits, `_` and `.`.
pub(crate) fn valid_wasm_model(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_WASM_NAME_BYTES
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

static NAMES: Mutex<Option<FxHashSet<&'static str>>> = Mutex::new(None);

/// Method names are `'static` in the tables (the exposure table and the compute graph key on
/// them); a name that arrives at run time from a loaded package is interned once. Callers validate
/// the name first ([`valid_wasm_method`]); the set is bounded by [`MAX_INTERNED_NAMES`] and the
/// call fails loudly past it. Names already known cost nothing.
pub(crate) fn intern(name: &str) -> Result<&'static str> {
    let mut guard = NAMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let set = guard.get_or_insert_with(FxHashSet::default);
    if let Some(known) = set.get(name) {
        return Ok(known);
    }
    if set.len() >= MAX_INTERNED_NAMES {
        return Err(Error::new(
            erp_fields::ErrorKind::ResourceExhausted,
            format!(
                "more than {MAX_INTERNED_NAMES} distinct WebAssembly method names were declared                  in this process; refusing '{name}'"
            ),
        ));
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    set.insert(leaked);
    Ok(leaked)
}

/// Whether `name` is already in the process-wide table of interned WebAssembly method names, without
/// adding it. For tests that prove a refused build interned nothing.
#[doc(hidden)]
pub fn wasm_method_name_is_interned(name: &str) -> bool {
    NAMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(name))
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn names_are_checked_by_charset_and_length() {
        assert!(valid_wasm_method("action_confirm"));
        assert!(!valid_wasm_method(""));
        assert!(!valid_wasm_method("a b"));
        assert!(!valid_wasm_method("a.b"));
        assert!(!valid_wasm_method(&"a".repeat(MAX_WASM_NAME_BYTES + 1)));
        assert!(valid_wasm_model("sale.order.line"));
        assert!(!valid_wasm_model("Sale.Order"));
        assert!(!valid_wasm_model("sale order"));
        assert!(!valid_wasm_model(&"a".repeat(MAX_WASM_NAME_BYTES + 1)));
    }

    #[test]
    fn interning_a_known_name_returns_the_same_string() {
        let a = intern("interned_probe_name").unwrap();
        let b = intern("interned_probe_name").unwrap();
        assert!(std::ptr::eq(a, b));
    }
}

/// The wire codec of one method signature: it turns the typed arguments and result of a native
/// implementation into JSON and back, so a chain can cross into WebAssembly and a WebAssembly
/// `super()` can enter a native implementation. Built by `#[method(wire)]` (one per signature).
pub trait MethodCodec: Send + Sync {
    /// The `TypeId` of the [`MethodFn`] the codec belongs to; checked against the chain's own.
    fn signature(&self) -> TypeId;

    /// Serialise the typed argument tuple (a `Box<A>`).
    fn encode_args(&self, args: Box<dyn Any + Send>) -> Result<String>;

    /// Parse a result into a `Box<R>`. `records` are the records the method ran on: a result that
    /// is itself a recordset (`create`) is rebuilt in their environment.
    fn decode_result(&self, records: &Records, json: &str) -> Result<Box<dyn Any + Send>>;

    /// Parse the arguments, run entry `index` of `chain` (and, through its `super()`, the rest) and
    /// serialise the result.
    fn call_native<'a>(
        &'a self,
        chain: Chain,
        index: usize,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
    ) -> BoxFuture<'a, Result<String>>;
}

/// [`MethodCodec`] of a method taking `A` and returning `R`.
pub struct TypedCodec<A, R>(PhantomData<fn(A) -> R>);

impl<A, R> TypedCodec<A, R> {
    /// For `#[method(wire)]` output.
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<A, R> Default for TypedCodec<A, R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A, R> MethodCodec for TypedCodec<A, R>
where
    A: Serialize + DeserializeOwned + Send + 'static,
    R: Serialize + DeserializeOwned + Send + 'static,
{
    fn signature(&self) -> TypeId {
        TypeId::of::<MethodFn<A, R>>()
    }

    fn encode_args(&self, args: Box<dyn Any + Send>) -> Result<String> {
        let args = args.downcast::<A>().map_err(|_| {
            Error::programming("method codec used with arguments of another signature")
        })?;
        serde_json::to_string(&*args)
            .map_err(|e| Error::programming(format!("method arguments are not serialisable: {e}")))
    }

    fn decode_result(&self, _records: &Records, json: &str) -> Result<Box<dyn Any + Send>> {
        let value: R = serde_json::from_str(json).map_err(|e| {
            Error::validation(format!(
                "a WASM implementation returned a malformed result: {e}"
            ))
        })?;
        Ok(Box::new(value))
    }

    fn call_native<'a>(
        &'a self,
        chain: Chain,
        index: usize,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let args: A = serde_json::from_str(args_json).map_err(|e| {
                Error::validation(format!(
                    "arguments of '{}' do not match its signature {}: {e}",
                    chain.method,
                    std::any::type_name::<A>()
                ))
            })?;
            let result: R = invoke::<A, R>(chain, index, tx, records, args).await?;
            serde_json::to_string(&result)
                .map_err(|e| Error::programming(format!("method result is not serialisable: {e}")))
        })
    }
}

/// A [`MethodCodec`] contributed by `#[method(wire)]`.
pub struct MethodCodecImpl {
    /// Contributing addon.
    pub addon: &'static str,
    /// Model of the declaration it belongs to.
    pub model: &'static str,
    /// Method name.
    pub method: &'static str,
    /// The codec.
    pub codec: &'static dyn MethodCodec,
}

impl fmt::Debug for MethodCodecImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MethodCodecImpl({}.{} of {})",
            self.model, self.method, self.addon
        )
    }
}

/// Wire codecs linked into the binary.
#[linkme::distributed_slice]
pub static METHOD_CODECS: [MethodCodecImpl];

/// One method's chain: its entries newest first and the codec of its signature.
pub struct ChainData {
    model: ModelId,
    method: &'static str,
    targets: Vec<MethodDispatchTarget>,
    /// Present when some native implementation declared `wire`.
    codec: Option<&'static dyn MethodCodec>,
}

impl fmt::Debug for ChainData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainData")
            .field("method", &self.method)
            .field("targets", &self.targets)
            .finish_non_exhaustive()
    }
}

/// A method's chain, shared by every call.
pub type Chain = Arc<ChainData>;

impl ChainData {
    /// The entries, newest first.
    pub fn targets(&self) -> &[MethodDispatchTarget] {
        &self.targets
    }
}

/// Per-registry dispatch tables.
#[derive(Debug, Default)]
pub struct Dispatch {
    chains: FxHashMap<ModelId, FxHashMap<&'static str, Chain>>,
}

/// One implementation while a chain is assembled.
struct Candidate {
    method: &'static str,
    target: MethodDispatchTarget,
    /// Source position, for deterministic ordering and messages.
    file: &'static str,
    line: u32,
}

impl Dispatch {
    /// Build the chains of every model from the linked implementations plus `extra`.
    pub fn build(registry: &Registry, extra: &[&'static MethodImpl]) -> Result<Self> {
        Self::build_with(registry, extra, &[])
    }

    /// [`Dispatch::build`] with the WebAssembly implementations of some addons: each lands in the
    /// chain of its model at the position of its addon in the model's resolution order, like a
    /// native override. The native implementations of those addons are left out.
    pub fn build_with(
        registry: &Registry,
        extra: &[&'static MethodImpl],
        wasm: &[WasmMethodImpl],
    ) -> Result<Self> {
        let installed: FxHashSet<&str> = registry.addons().iter().map(|a| a.name).collect();
        let wasm_addons: FxHashSet<&str> = wasm.iter().map(|w| w.invoker.addon()).collect();
        let builtin = crate::write::BUILTIN_IMPLS
            .iter()
            .chain(crate::unlink::BUILTIN_IMPLS.iter())
            .chain(crate::defaults::BUILTIN_IMPLS.iter())
            .chain(crate::rset_ops::BUILTIN_IMPLS.iter())
            .chain(crate::read_group::BUILTIN_IMPLS.iter())
            .chain(crate::web::BUILTIN_IMPLS.iter())
            .chain(crate::model_hooks::BUILTIN_IMPLS.iter())
            .chain(crate::upsert::BUILTIN_IMPLS.iter())
            .chain(crate::copy::BUILTIN_IMPLS.iter())
            .chain(crate::name_create::BUILTIN_IMPLS.iter())
            .chain(crate::archive::BUILTIN_IMPLS.iter())
            .copied();
        let mut by_owner: FxHashMap<(&str, &str), Vec<Candidate>> = FxHashMap::default();
        for imp in METHOD_IMPLS
            .iter()
            .chain(builtin)
            .chain(extra.iter().copied())
            .filter(|i| installed.contains(i.addon) && !wasm_addons.contains(i.addon))
        {
            by_owner
                .entry((imp.model, imp.addon))
                .or_default()
                .push(Candidate {
                    method: imp.method,
                    target: MethodDispatchTarget::Native(imp),
                    file: imp.file,
                    line: imp.line,
                });
        }
        // Names a package declares are validated and matched against the registry BEFORE anything
        // is interned, so a rejected build leaks nothing (security review M3).
        let declared_models: FxHashMap<(&str, &str), &'static str> = registry
            .models()
            .iter()
            .flat_map(|m| m.mro.iter().map(|d| ((d.name, d.addon), d.name)))
            .collect();
        // Pass 1: validate every implementation. Nothing is interned until all of them passed, so
        // a package whose last entry is bad leaves no name behind (G security review r2 N1).
        if wasm.len() > MAX_WASM_IMPLS_PER_BUILD {
            return Err(Error::new(
                erp_fields::ErrorKind::ResourceExhausted,
                format!(
                    "{} WebAssembly method implementations in one registry build; at most \
                     {MAX_WASM_IMPLS_PER_BUILD} are accepted",
                    wasm.len()
                ),
            ));
        }
        let mut checked: Vec<(&'static str, &str, &WasmMethodImpl)> =
            Vec::with_capacity(wasm.len());
        for imp in wasm {
            let addon = imp.invoker.addon();
            let Some(&addon) = installed.get(addon) else {
                return Err(Error::programming(format!(
                    "WASM implementation of {}.{} belongs to addon '{addon}', which is not installed \
                     in the registry",
                    imp.model, imp.method
                )));
            };
            if !valid_wasm_model(&imp.model) || !valid_wasm_method(&imp.method) {
                return Err(Error::programming(format!(
                    "WASM implementation of addon '{addon}' declares the invalid name \
                     '{}.{}': a model is lower-case ASCII letters, digits, '_' and '.', a method \
                     ASCII letters, digits and '_', each at most {MAX_WASM_NAME_BYTES} bytes",
                    imp.model.chars().take(64).collect::<String>(),
                    imp.method.chars().take(64).collect::<String>(),
                )));
            }
            let Some(&model) = declared_models.get(&(imp.model.as_str(), addon)) else {
                return Err(Error::programming(format!(
                    "WASM implementation of {}.{} has no declaration of model '{}' in addon \
                     '{addon}': add an #[extend] (or #[model]) for it in that addon",
                    imp.model, imp.method, imp.model
                )));
            };
            checked.push((model, addon, imp));
        }
        // Pass 2: intern the method names (bounded by the process-wide budget).
        for (model, addon, imp) in checked {
            by_owner.entry((model, addon)).or_default().push(Candidate {
                method: intern(&imp.method)?,
                target: MethodDispatchTarget::Wasm(Arc::clone(&imp.invoker)),
                file: "<wasm>",
                line: 0,
            });
        }
        // Deterministic: owners and their implementations sorted, so the first error reported is
        // always the same one.
        let mut owners: Vec<(&str, &str)> = by_owner.keys().copied().collect();
        owners.sort_unstable();
        for owner in &owners {
            let Some(list) = by_owner.get_mut(owner) else {
                continue;
            };
            list.sort_by_key(|c| (c.method, c.file, c.line));
            if let Some(pair) = list.windows(2).find(|w| w[0].method == w[1].method) {
                return Err(Error::programming(format!(
                    "method '{}' of model '{}' is implemented twice by addon '{}': {} and {}",
                    pair[0].method,
                    owner.0,
                    owner.1,
                    pair[0].target.location(),
                    pair[1].target.location()
                )));
            }
        }
        // Every implementation needs a declaration of its model in its addon.
        let declared: FxHashSet<(&str, &str)> = registry
            .models()
            .iter()
            .flat_map(|m| m.mro.iter().map(|d| (d.name, d.addon)))
            .collect();
        for owner in &owners {
            if !declared.contains(owner)
                && let Some(first) = by_owner.get(owner).and_then(|l| l.first())
            {
                return Err(Error::programming(format!(
                    "#[methods({})] at {} has no declaration of model '{}' in addon '{}': add an \
                     #[extend] (or #[model]) for it in that addon",
                    owner.0,
                    first.target.location(),
                    owner.0,
                    owner.1
                )));
            }
        }

        let mut codecs: FxHashMap<(&str, &str, &str), &'static MethodCodecImpl> =
            FxHashMap::default();
        for c in METHOD_CODECS
            .iter()
            .chain(crate::wire::BUILTIN_CODECS.iter().copied())
            .filter(|c| installed.contains(c.addon))
        {
            codecs.insert((c.model, c.addon, c.method), c);
        }

        let mut chains: FxHashMap<ModelId, FxHashMap<&'static str, Chain>> = FxHashMap::default();
        for model in registry.models() {
            let mut per_method: FxHashMap<&'static str, Vec<&Candidate>> = FxHashMap::default();
            let mut seen_owners: Vec<(&str, &str)> = Vec::new();
            for decl in &model.mro {
                let owner = (decl.name, decl.addon);
                if seen_owners.contains(&owner) {
                    continue;
                }
                seen_owners.push(owner);
                for cand in by_owner.get(&owner).map(Vec::as_slice).unwrap_or_default() {
                    per_method.entry(cand.method).or_default().push(cand);
                }
            }
            let mut methods: Vec<(&'static str, Vec<&Candidate>)> =
                per_method.into_iter().collect();
            methods.sort_unstable_by_key(|(name, _)| *name);
            for (method, chain) in methods {
                let targets: Vec<MethodDispatchTarget> =
                    chain.iter().map(|c| c.target.clone()).collect();
                check_chain(model.name, method, &targets)?;
                let codec = chain_codec(model.name, method, &targets, &codecs)?;
                chains.entry(model.id).or_default().insert(
                    method,
                    Arc::new(ChainData {
                        model: model.id,
                        method,
                        targets,
                        codec,
                    }),
                );
            }
        }
        Ok(Self { chains })
    }

    /// The chain of a method on a model.
    pub(crate) fn chain(&self, model: ModelId, method: &str) -> Option<Chain> {
        self.chains.get(&model)?.get(method).cloned()
    }

    /// Every (model, method) that has a chain.
    pub(crate) fn chain_keys(&self) -> impl Iterator<Item = (ModelId, &'static str)> + '_ {
        self.chains
            .iter()
            .flat_map(|(model, methods)| methods.keys().map(|m| (*model, *m)))
    }

    /// The native implementations of a method on a model, newest first (for tests and `erp check`).
    /// WebAssembly entries are not listed; see [`Dispatch::targets`].
    pub fn implementations(&self, model: ModelId, method: &str) -> Vec<&'static MethodImpl> {
        self.chain(model, method)
            .map(|c| {
                c.targets
                    .iter()
                    .filter_map(|t| match t {
                        MethodDispatchTarget::Native(imp) => Some(*imp),
                        MethodDispatchTarget::Wasm(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every entry of the chain of a method on a model, native and WebAssembly, newest first.
    pub fn targets(&self, model: ModelId, method: &str) -> Vec<MethodDispatchTarget> {
        self.chain(model, method)
            .map(|c| c.targets.clone())
            .unwrap_or_default()
    }

    /// True when the model's implementation of `method` is base-only (no addon overrides).
    #[must_use]
    pub fn is_base_method(&self, model: ModelId, method: &str) -> bool {
        match self.chains.get(&model).and_then(|m| m.get(method)) {
            Some(chain) => {
                chain.targets.len() == 1
                    && matches!(&chain.targets[0], MethodDispatchTarget::Native(m) if m.addon == "base")
            }
            None => false,
        }
    }
}

/// The codec of a chain: any native implementation's, which must agree with the chain's signature.
/// A chain that mixes native and WebAssembly entries needs one; a chain of one kind only does not
/// (a WebAssembly-only chain is called in the wire form, a native-only chain is typed).
fn chain_codec(
    model: &str,
    method: &str,
    targets: &[MethodDispatchTarget],
    codecs: &FxHashMap<(&str, &str, &str), &'static MethodCodecImpl>,
) -> Result<Option<&'static dyn MethodCodec>> {
    let mut found: Option<(&'static dyn MethodCodec, &MethodImpl)> = None;
    for target in targets {
        let MethodDispatchTarget::Native(imp) = target else {
            continue;
        };
        let Some(c) = codecs.get(&(imp.model, imp.addon, imp.method)) else {
            continue;
        };
        if c.codec.signature() != imp.sig() {
            return Err(Error::programming(format!(
                "#[method(wire)] of '{method}' at {} was generated for another signature than the \
                 function it marks",
                imp.location()
            )));
        }
        if found.is_none() {
            found = Some((c.codec, imp));
        }
    }
    let crosses =
        targets.iter().any(MethodDispatchTarget::is_wasm) && targets.iter().any(|t| !t.is_wasm());
    if crosses && found.is_none() {
        let natives: Vec<String> = targets
            .iter()
            .filter(|t| !t.is_wasm())
            .map(MethodDispatchTarget::location)
            .collect();
        return Err(Error::programming(format!(
            "method '{method}' of model '{model}' has a WebAssembly implementation, but no native \
             implementation is marked #[method(wire)] ({}): a chain that crosses between native \
             code and a component needs the argument and result types to be serialisable",
            natives.join(", ")
        )));
    }
    Ok(found.map(|(c, _)| c))
}

/// One signature; exactly one definition, at the bottom (newest first: overrides, then it). Only
/// native entries have a typed signature to compare; a WebAssembly entry is checked by its wire
/// form at the call.
fn check_chain(model: &str, method: &str, chain: &[MethodDispatchTarget]) -> Result<()> {
    let Some(first) = chain.first() else {
        return Ok(());
    };
    let defines: Vec<&MethodDispatchTarget> = chain
        .iter()
        .filter(|i| i.kind() == ImplKind::Define)
        .collect();
    if let [newer, older, ..] = defines.as_slice() {
        return Err(Error::programming(format!(
            "method '{method}' of model '{model}' is defined at {} and again at {}: the later one \
             must be #[method(override)]",
            older.location(),
            newer.location()
        )));
    }
    let first_native = chain.iter().find_map(|t| match t {
        MethodDispatchTarget::Native(imp) => Some(*imp),
        MethodDispatchTarget::Wasm(_) => None,
    });
    if let Some(reference) = first_native {
        for target in chain {
            if let MethodDispatchTarget::Native(other) = target
                && other.sig() != reference.sig()
            {
                return Err(Error::programming(format!(
                    "method '{method}' of model '{model}': {} has signature {} but {} has {}",
                    other.location(),
                    (other.sig_name)(),
                    reference.location(),
                    (reference.sig_name)()
                )));
            }
        }
    }
    match defines.as_slice() {
        [] => Err(Error::programming(format!(
            "override of method '{method}' at {} has no definition below it in model '{model}'",
            first.location()
        ))),
        [_] if chain.last().is_some_and(|l| l.kind() == ImplKind::Define) => Ok(()),
        [newer, older, ..] => Err(Error::programming(format!(
            "method '{method}' of model '{model}' is defined at {} and again at {}: the later one \
             must be #[method(override)]",
            older.location(),
            newer.location()
        ))),
        [define] => Err(Error::programming(format!(
            "method '{method}' of model '{model}': the definition at {} is newer than an override \
             below it; declare it #[method(override)]",
            define.location()
        ))),
    }
}

/// The rest of a method chain, handed to an override (`super()`).
pub struct Super<A, R> {
    chain: Chain,
    next: usize,
    _p: PhantomData<fn(A) -> R>,
}

impl<A: Send + 'static, R: Send + 'static> Super<A, R> {
    /// Call the next implementation in the chain, on records of the chain's model.
    pub fn call<'a>(
        self,
        tx: &'a mut Transaction,
        records: &'a Records,
        args: A,
    ) -> BoxFuture<'a, Result<R>> {
        if let Err(error) = tx.check_method_records(records, self.chain.method) {
            return Box::pin(async move { Err(error) });
        }
        if records.model_id() != self.chain.model {
            let message = format!(
                "super() of method '{}' called with {records}, not with records of the model whose \
                 chain it belongs to",
                self.chain.method
            );
            return Box::pin(async move { Err(Error::programming(message)) });
        }
        invoke(self.chain, self.next, tx, records, args)
    }
}

/// The rest of a method chain as seen from an implementation that speaks the wire form: what a
/// [`MethodInvoker`] gets for the guest's `super()`. Cheap to clone; each call runs the next entry.
#[derive(Clone)]
pub struct DynSuper {
    chain: Chain,
    next: usize,
}

impl DynSuper {
    /// Whether an implementation is left below the caller.
    pub fn has_next(&self) -> bool {
        self.next < self.chain.targets.len()
    }

    /// The method this chain belongs to.
    pub fn method(&self) -> &'static str {
        self.chain.method
    }

    /// Call the next implementation with JSON arguments and get its JSON result, on `records` of
    /// the chain's model. Native entries run through their codec, WebAssembly entries through
    /// their invoker, so the call crosses either way.
    pub fn call_json<'a>(
        self,
        tx: &'a mut Transaction,
        records: &'a Records,
        args_json: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        if let Err(error) = tx.check_method_records(records, self.chain.method) {
            return Box::pin(async move { Err(error) });
        }
        if records.model_id() != self.chain.model {
            let message = format!(
                "super() of method '{}' called with {records}, not with records of the model whose \
                 chain it belongs to",
                self.chain.method
            );
            return Box::pin(async move { Err(Error::programming(message)) });
        }
        invoke_json(self.chain, self.next, tx, records, args_json)
    }
}

/// Run entry `index` of the chain in the wire form.
fn invoke_json<'a>(
    chain: Chain,
    index: usize,
    tx: &'a mut Transaction,
    records: &'a Records,
    args_json: &'a str,
) -> BoxFuture<'a, Result<String>> {
    let Some(target) = chain.targets.get(index).cloned() else {
        let method = chain.method;
        return Box::pin(async move {
            Err(Error::programming(format!(
                "super() of method '{method}' called on the last implementation"
            )))
        });
    };
    if chain.targets.iter().any(MethodDispatchTarget::is_wasm) {
        match crate::wasm_abi1::json_plan(tx, records, chain.method, args_json) {
            Ok(Some(plan)) => return Box::pin(async move { Err(plan.reject(tx).await) }),
            Err(error) => return Box::pin(async move { Err(error) }),
            Ok(None) => {}
        }
    }
    match target {
        MethodDispatchTarget::Wasm(invoker) => {
            let sup = DynSuper {
                chain,
                next: index + 1,
            };
            Box::pin(async move { invoker.invoke(tx, records, args_json, sup).await })
        }
        MethodDispatchTarget::Native(imp) => match chain.codec {
            Some(codec) => codec.call_native(chain, index, tx, records, args_json),
            None => {
                let message = format!(
                    "method '{}' reached the native implementation at {} through its wire form, \
                     but no implementation of the chain is marked #[method(wire)]",
                    chain.method,
                    imp.location()
                );
                Box::pin(async move { Err(Error::programming(message)) })
            }
        },
    }
}

pub(crate) fn invoke<'a, A: Send + 'static, R: Send + 'static>(
    chain: Chain,
    index: usize,
    tx: &'a mut Transaction,
    records: &'a Records,
    args: A,
) -> BoxFuture<'a, Result<R>> {
    let Some(target) = chain.targets.get(index).cloned() else {
        let method = chain.method;
        return Box::pin(async move {
            Err(Error::programming(format!(
                "super() of method '{method}' called on the last implementation"
            )))
        });
    };
    match target {
        MethodDispatchTarget::Native(imp) => {
            let Some(f) = (imp.entry)().downcast_ref::<MethodFn<A, R>>() else {
                return Box::pin(async move {
                    Err(Error::programming(format!(
                        "method '{}' called with signature {} but {} implements {}",
                        imp.method,
                        std::any::type_name::<MethodFn<A, R>>(),
                        imp.location(),
                        (imp.sig_name)()
                    )))
                });
            };
            if chain.targets.iter().any(MethodDispatchTarget::is_wasm) {
                match crate::wasm_abi1::typed_plan(tx, records, chain.method, &args) {
                    Ok(Some(plan)) => return Box::pin(async move { Err(plan.reject(tx).await) }),
                    Err(error) => return Box::pin(async move { Err(error) }),
                    Ok(None) => {}
                }
            }
            let sup = Super {
                chain,
                next: index + 1,
                _p: PhantomData,
            };
            f(tx, records, args, sup)
        }
        MethodDispatchTarget::Wasm(invoker) => Box::pin(async move {
            let Some(codec) = chain.codec else {
                return Err(Error::programming(format!(
                    "method '{}' reached a WebAssembly implementation of addon '{}' from typed code, \
                     but no native implementation is marked #[method(wire)]",
                    chain.method,
                    invoker.addon()
                )));
            };
            if let Some(plan) = crate::wasm_abi1::typed_plan(tx, records, chain.method, &args)? {
                return Err(plan.reject(tx).await);
            }
            let json = codec.encode_args(Box::new(args))?;
            let sup = DynSuper {
                chain,
                next: index + 1,
            };
            let answer = invoker.invoke(tx, records, &json, sup).await?;
            let boxed = codec.decode_result(records, &answer)?;
            boxed.downcast::<R>().map(|b| *b).map_err(|_| {
                Error::programming(format!(
                    "method result of a WebAssembly implementation does not have the type {}",
                    std::any::type_name::<R>()
                ))
            })
        }),
    }
}

/// Call a method on a recordset through its chain (used by generated method traits).
pub fn call_method<'a, A: Send + 'static, R: Send + 'static>(
    tx: &'a mut Transaction,
    records: &'a Records,
    method: &'static str,
    args: A,
) -> BoxFuture<'a, Result<R>> {
    if let Err(error) = tx.check_method_records(records, method) {
        return Box::pin(async move { Err(error) });
    }
    match tx.dispatch().chain(records.model_id(), method) {
        Some(chain) => invoke(chain, 0, tx, records, args),
        None => {
            let model = records.model_name();
            Box::pin(async move {
                Err(Error::programming(format!(
                    "'{model}' object has no attribute '{method}'"
                )))
            })
        }
    }
}

/// Whether the chain of `method` on `model` has a WebAssembly entry.
pub(crate) fn chain_has_wasm(tx: &Transaction, model: ModelId, method: &str) -> bool {
    tx.dispatch()
        .chain(model, method)
        .is_some_and(|chain| chain.targets.iter().any(MethodDispatchTarget::is_wasm))
}

/// Call a method that takes no arguments and returns nothing (a constraint) through its chain. A
/// chain with a WebAssembly entry is called in the wire form (`null` in, `null` out), because a
/// WebAssembly-only chain has no typed form; every other chain is called typed.
pub(crate) fn call_unit_method<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    method: &'static str,
) -> BoxFuture<'a, Result<()>> {
    let crosses = tx
        .dispatch()
        .chain(records.model_id(), method)
        .is_some_and(|chain| chain.targets.iter().any(MethodDispatchTarget::is_wasm));
    if !crosses {
        return call_method::<(), ()>(tx, records, method, ());
    }
    Box::pin(async move {
        let answer = call_method_json(tx, records, method, "null").await?;
        serde_json::from_str::<()>(&answer).map_err(|e| {
            Error::validation(format!(
                "constraint '{method}' of a WebAssembly addon returned something but null: {e}"
            ))
        })
    })
}

/// Call a method on a recordset through its chain in the wire form: JSON arguments (an array, the
/// serialised argument tuple) in, JSON result out. This is how a component's `call-method` reaches
/// any implementation, native or WebAssembly. A chain with native entries needs one marked
/// `#[method(wire)]`.
pub fn call_method_json<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    method: &'a str,
    args_json: &'a str,
) -> BoxFuture<'a, Result<String>> {
    if let Err(error) = tx.check_records(records) {
        return Box::pin(async move { Err(error) });
    }
    match tx.dispatch().chain(records.model_id(), method) {
        Some(chain) => invoke_json(chain, 0, tx, records, args_json),
        None => {
            let model = records.model_name();
            Box::pin(async move {
                Err(Error::programming(format!(
                    "'{model}' object has no attribute '{method}'"
                )))
            })
        }
    }
}
