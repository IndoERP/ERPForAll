// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded WASM execution host for database-stored (restricted) arch template compilation.
//!
//! The host runs the pinned `wasm32-wasip1` build of `erp-ui-compile` in wasmi and nothing else:
//!
//! - [`WasmArtifact::admit`] checks the artifact's SHA-256 against a pinned [`ArtifactDigest`]
//!   before it parses a byte, then allows only the eight WASI preview 1 imports the compiler needs
//!   ([`ALLOWED_WASI_FUNCTIONS`]) and requires the `_start` and `memory` exports;
//! - [`WasmCompilerHost::compile_arch`] runs one [`ArchCompileRequest`] under
//!   [`ExecutionLimits`] (linear memory, fuel, output bytes, wall-clock deadline), which can only
//!   be lowered, and a [`CancelToken`] the caller can trigger from another thread;
//! - every failure is a typed [`CompilerHostError`]; any error discards the guest's output, so no
//!   partial result is ever returned.
//!

mod artifact;
mod error;
mod host;
mod limits;
mod wasi;

pub use erp_ui_compiler_cli::ArchCompileRequest;

pub use artifact::{ALLOWED_WASI_FUNCTIONS, ALLOWED_WASI_MODULE, ArtifactDigest, WasmArtifact};
pub use error::{
    ArtifactError, CompilerHostError, HostExecutionError, InvalidLimit, LimitError,
    MAX_STDERR_IN_ERROR,
};
pub use host::{CancelToken, HostResponse, WasmCompilerHost};
pub use limits::{ExecutionLimits, MAX_DEADLINE, MAX_FUEL, MAX_OUTPUT_BYTES};
