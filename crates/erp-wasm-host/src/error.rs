//! Errors of the WASM host. A trap is sticky: the store that produced it refuses further calls.

/// Which limit a value was checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitKind {
    /// CPU time per call.
    Cpu,
    /// Guest linear memory.
    Memory,
    /// Host call-stack depth.
    StackDepth,
    /// Ids per boundary call.
    BatchIds,
    /// Serialized payload bytes.
    PayloadBytes,
    /// Memory of all the stores of one transaction.
    TxMemory,
    /// Idle pooled stores per addon.
    IdleStores,
}

impl std::fmt::Display for LimitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cpu => "cpu time",
            Self::Memory => "memory",
            Self::StackDepth => "stack depth",
            Self::BatchIds => "batch ids",
            Self::PayloadBytes => "payload bytes",
            Self::TxMemory => "transaction memory",
            Self::IdleStores => "idle stores",
        })
    }
}

/// Why the guest was stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrapKind {
    /// CPU epoch deadline exceeded.
    Epoch,
    /// Memory cap exceeded.
    Memory,
    /// Guest stack overflow.
    Stack,
    /// Any other trap (panic, unreachable, divide by zero, ...), with wasmtime's message.
    Other(String),
}

/// Failure of a host operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HostError {
    /// A limit value was zero or above its hard maximum.
    #[error("invalid {limit} limit {value}: must be between 1 and {max}")]
    InvalidLimit {
        /// Limit concerned.
        limit: LimitKind,
        /// Rejected value.
        value: u64,
        /// Hard maximum.
        max: u64,
    },
    /// A runtime check against a limit failed (nothing ran in the guest).
    #[error("{limit} limit exceeded: {value} > {max}")]
    LimitExceeded {
        /// Limit concerned.
        limit: LimitKind,
        /// Observed value.
        value: u64,
        /// Configured maximum.
        max: u64,
    },
    /// The guest trapped. The caller must roll back the active transaction and drop the store.
    #[error("wasm trap: {0:?}")]
    Trap(TrapKind),
    /// The store trapped earlier and refuses further calls.
    #[error("wasm store is poisoned by an earlier trap; roll back and discard it")]
    Poisoned,
    /// The component does not export the requested function (or with another signature).
    #[error("component export `{name}` not found or has another signature: {reason}")]
    Export {
        /// Requested export.
        name: String,
        /// Reason reported by the runtime.
        reason: String,
    },
    /// The component bytes did not compile.
    #[error("component compile failed: {0}")]
    Compile(String),
    /// The bytes given do not hash to the digest in the cache key.
    #[error("component bytes do not match the cache key digest")]
    DigestMismatch,
    /// The component could not be instantiated for a reason that is not a guest trap (unsatisfied
    /// import, pool slots exhausted). The store is poisoned too: start functions may have run.
    #[error("component instantiation failed: {0}")]
    Instantiate(String),
    /// The component imports an interface the host does not grant (files, sockets, HTTP, or anything
    /// outside `erpforall:addon` and the allowlisted WASI interfaces). Refused at load time.
    #[error("component imports `{name}`, which the host does not grant")]
    ForbiddenImport {
        /// The interface name as the component imports it.
        name: String,
    },
    /// The runtime could not be configured.
    #[error("wasm runtime configuration failed: {0}")]
    Config(String),
}
