//! Host runtime for WASM addons (L7).
//!
//! This crate implements the host side of the WIT contract in `wit/addon/`. adds the
//! contract and its identity below; the `wasmtime` runtime (pooling allocator, limits, module cache)
//! arrives with. `tests/wit_contract.rs` checks that the files and these constants agree.

/// WIT package of the contract, without the version.
pub const WIT_PACKAGE: &str = "erpforall:addon";

/// Version of the contract in `wit/addon/`. Minor versions only add functions, interfaces or
/// imports; any change to an existing type or function is a new major.
pub const WIT_VERSION: &str = "1.0.0";

/// World an addon component targets.
pub const WIT_WORLD: &str = "addon";

mod adapter;
mod bridge;
mod cache;
mod column;
mod dispatch;
mod error;
mod host;
mod limits;

/// Rust types and traits generated from `wit/addon` (world `addon`), host side. Not part of the
/// supported API: the contract is the WIT files, and the generated shapes may change with them.
#[doc(hidden)]
pub mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/addon",
        world: "addon",
        imports: { default: async },
        exports: { default: async },
    });
}

pub use adapter::MethodRole;
pub use bindings::erpforall::addon::orm_host::LogLevel;
pub use bindings::erpforall::addon::types::{
    BoundaryError, BoundaryErrorKind, ColumnData, ColumnVector, RecordBatch, ScalarValue,
    X2manyCommand,
};
pub use bridge::{
    BackendFuture, BridgeResult, Caller, MAX_FIELDS, MAX_NAME_BYTES, MAX_OFFSET, MAX_ORDER_BYTES,
    MAX_ORDER_TERMS, OrmBackend, OrmBridge, OrmCapabilities, OrmOp, boundary_error,
};
pub use cache::{ModuleCache, ModuleKey};
pub use dispatch::{
    CallableMethods, DescribedMethod, InstancePools, MAX_DESCRIBED_METHODS, ProbeRequest,
    WasmAddon, callable_from, callable_methods, described_methods, exposed_methods, probe_request,
    probe_request_as,
};
pub use error::{HostError, LimitKind, TrapKind};
pub use host::{AddonStore, WasmHost, import_allowed};
pub use limits::Limits;
