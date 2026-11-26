// SPDX-License-Identifier: MIT OR Apache-2.0
//! Typed errors of the compiler host.

use std::time::Duration;

/// The most bytes of the guest's standard error kept in [`HostExecutionError::ExitStatus`].
pub const MAX_STDERR_IN_ERROR: usize = 4096;

/// Any failure of [`crate::WasmCompilerHost`]. No variant carries a partial compiler result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CompilerHostError {
    /// The artifact failed admission.
    #[error("artifact error: {0}")]
    Artifact(#[from] ArtifactError),

    /// An execution limit (fuel, memory, output, deadline) was reached.
    #[error("limit error: {0}")]
    Limit(#[from] LimitError),

    /// The run failed: trap, failed instantiation, host-call failure, cancellation, or a
    /// non-clean exit status.
    #[error("host execution error: {0}")]
    HostExecution(#[from] HostExecutionError),

    /// The request could not be encoded.
    #[error("request error: {0}")]
    Request(#[from] erp_ui_compiler_cli::RequestError),
}

/// Why an artifact was not admitted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ArtifactError {
    /// The pinned digest is not 64 lowercase hexadecimal characters.
    #[error("artifact digest must be 64 lowercase hexadecimal characters (SHA-256), got '{value}'")]
    InvalidDigest {
        /// The refused text.
        value: String,
    },

    /// The artifact's SHA-256 is not the pinned digest. Checked before the bytes are parsed.
    #[error("artifact digest mismatch: expected sha256 {expected}, got {actual}")]
    DigestMismatch {
        /// The pinned digest.
        expected: String,
        /// The digest of the offered bytes.
        actual: String,
    },

    /// The bytes do not parse or validate as a module the host accepts (a `start` section is
    /// refused too: it would run outside the sliced, cancellable call).
    #[error("invalid WASM artifact: {0}")]
    InvalidModule(String),

    /// The module imports something outside the allowed WASI functions.
    #[error(
        "forbidden WASM import '{module}.{name}': only functions {} of '{}' are allowed",
        crate::ALLOWED_WASI_FUNCTIONS.join(", "),
        crate::ALLOWED_WASI_MODULE
    )]
    ForbiddenImport {
        /// The import's module name.
        module: String,
        /// The import's field name.
        name: String,
    },

    /// A required export is missing or has the wrong kind.
    #[error("missing required WASM export '{name}' (a {kind})")]
    MissingExport {
        /// The export name.
        name: &'static str,
        /// The required kind: `function () -> ()` or `memory`.
        kind: &'static str,
    },
}

/// An execution limit the run reached. Limits are sticky: once one is hit, the run ends with it
/// whatever the guest does next.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LimitError {
    /// The run spent its whole fuel budget.
    #[error("WASM fuel exhausted (budget: {budget} units)")]
    FuelExhausted {
        /// The run's fuel budget.
        budget: u64,
    },

    /// The guest asked for linear memory beyond the cap (a denied `memory.grow` or an initial
    /// memory over the cap).
    #[error("WASM linear memory cap exceeded (cap: {cap_bytes} bytes)")]
    MemoryExhausted {
        /// The memory cap.
        cap_bytes: u64,
    },

    /// The guest wrote more than the output cap (standard output and error together).
    #[error("WASM output limit exceeded (maximum: {max_bytes} bytes)")]
    OutputExhausted {
        /// The output cap.
        max_bytes: usize,
    },

    /// The wall-clock deadline passed.
    #[error("WASM execution deadline exceeded (deadline: {deadline:?})")]
    DeadlineExceeded {
        /// The run's deadline.
        deadline: Duration,
    },
}

/// A failed run that is not a limit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HostExecutionError {
    /// The guest trapped (the wasmi trap description).
    #[error("WASM execution trapped: {0}")]
    Trap(String),

    /// A host function failed for a reason that is not a limit or a cancellation.
    #[error("WASM host call failed: {0}")]
    HostCall(String),

    /// The module could not be instantiated (an import signature that does not match the host
    /// function, for example). Not a trap: no guest code ran.
    #[error("WASM instantiation failed: {0}")]
    Instantiate(String),

    /// The engine could not be set up or queried (fuel metering, linker definitions).
    #[error("WASM engine error: {0}")]
    Engine(String),

    /// The compiler exited with a status other than clean (0) or compiler errors (1); 2 is a
    /// refused request. The guest's standard error, truncated to [`MAX_STDERR_IN_ERROR`] bytes.
    #[error("compiler exited with status {status}: {stderr}")]
    ExitStatus {
        /// The exit status.
        status: i32,
        /// The guest's standard error (lossy UTF-8, at most [`MAX_STDERR_IN_ERROR`] bytes, with a
        /// `[truncated]` marker when cut).
        stderr: String,
    },

    /// The caller cancelled the run through its [`crate::CancelToken`].
    #[error("WASM execution was cancelled")]
    Cancelled,
}

/// A lowering of [`crate::ExecutionLimits`] outside `1..=current`: zero never disables a limit
/// and no limit can be raised.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidLimit {
    /// The linear-memory cap.
    #[error("memory limit cannot be set to {value} bytes: it must be 1..={max}")]
    Memory {
        /// The refused value.
        value: u64,
        /// The current (largest allowed) value.
        max: u64,
    },
    /// The fuel cap.
    #[error("fuel limit cannot be set to {value} units: it must be 1..={max}")]
    Fuel {
        /// The refused value.
        value: u64,
        /// The current (largest allowed) value.
        max: u64,
    },
    /// The output cap.
    #[error("output limit cannot be set to {value} bytes: it must be 1..={max}")]
    Output {
        /// The refused value.
        value: usize,
        /// The current (largest allowed) value.
        max: usize,
    },
    /// The deadline.
    #[error("deadline cannot be set to {value:?}: it must be above zero and at most {max:?}")]
    Deadline {
        /// The refused value.
        value: Duration,
        /// The current (largest allowed) value.
        max: Duration,
    },
}
