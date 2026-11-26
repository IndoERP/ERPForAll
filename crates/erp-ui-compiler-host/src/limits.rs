// SPDX-License-Identifier: MIT OR Apache-2.0
//! Execution limits of one compiler run: lower-only, never disabled.

use crate::error::InvalidLimit;
use erp_ui_compiler_cli::{MAX_REQUEST_BYTES, WASM_MEMORY_CAP_BYTES, wasm_fuel_budget};
use std::time::Duration;

/// The output ceiling: standard output and standard error together.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// The wall-clock deadline ceiling of one run.
pub const MAX_DEADLINE: Duration = Duration::from_secs(30);

/// The largest fuel cap: the budget of the largest request the compiler reads.
pub const MAX_FUEL: u64 = wasm_fuel_budget(MAX_REQUEST_BYTES);

/// The limits of one run. [`Default`] gives the ceilings: linear memory
/// [`WASM_MEMORY_CAP_BYTES`], fuel [`wasm_fuel_budget`] of the request size, output
/// [`MAX_OUTPUT_BYTES`], deadline [`MAX_DEADLINE`].
///
/// The `lowered_*` methods accept `1..=current` only: zero never disables a limit and nothing
/// raises one. A lowered fuel cap is a cap on top of the per-request budget: a run gets the
/// smaller of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionLimits {
    memory_bytes: u64,
    fuel_cap: u64,
    output_bytes: usize,
    deadline: Duration,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            memory_bytes: WASM_MEMORY_CAP_BYTES,
            fuel_cap: MAX_FUEL,
            output_bytes: MAX_OUTPUT_BYTES,
            deadline: MAX_DEADLINE,
        }
    }
}

impl ExecutionLimits {
    /// Lowers the linear-memory cap to `bytes`.
    ///
    /// # Errors
    /// [`InvalidLimit::Memory`] unless `1 <= bytes <= `[`ExecutionLimits::memory_bytes`].
    pub fn lowered_memory_bytes(mut self, bytes: u64) -> Result<Self, InvalidLimit> {
        if bytes == 0 || bytes > self.memory_bytes {
            return Err(InvalidLimit::Memory {
                value: bytes,
                max: self.memory_bytes,
            });
        }
        self.memory_bytes = bytes;
        Ok(self)
    }

    /// Lowers the fuel cap to `fuel`.
    ///
    /// # Errors
    /// [`InvalidLimit::Fuel`] unless `1 <= fuel <=` the current cap ([`MAX_FUEL`] by default).
    pub fn lowered_fuel(mut self, fuel: u64) -> Result<Self, InvalidLimit> {
        if fuel == 0 || fuel > self.fuel_cap {
            return Err(InvalidLimit::Fuel {
                value: fuel,
                max: self.fuel_cap,
            });
        }
        self.fuel_cap = fuel;
        Ok(self)
    }

    /// Lowers the output cap to `bytes`.
    ///
    /// # Errors
    /// [`InvalidLimit::Output`] unless `1 <= bytes <= `[`ExecutionLimits::output_bytes`].
    pub fn lowered_output_bytes(mut self, bytes: usize) -> Result<Self, InvalidLimit> {
        if bytes == 0 || bytes > self.output_bytes {
            return Err(InvalidLimit::Output {
                value: bytes,
                max: self.output_bytes,
            });
        }
        self.output_bytes = bytes;
        Ok(self)
    }

    /// Lowers the deadline to `deadline`.
    ///
    /// # Errors
    /// [`InvalidLimit::Deadline`] unless `0 < deadline <= `[`ExecutionLimits::deadline`].
    pub fn lowered_deadline(mut self, deadline: Duration) -> Result<Self, InvalidLimit> {
        if deadline.is_zero() || deadline > self.deadline {
            return Err(InvalidLimit::Deadline {
                value: deadline,
                max: self.deadline,
            });
        }
        self.deadline = deadline;
        Ok(self)
    }

    /// The linear-memory cap in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    /// The fuel budget of a request of `request_bytes` bytes: [`wasm_fuel_budget`] of the size,
    /// or the lowered cap when that is smaller.
    #[must_use]
    pub fn fuel_for(&self, request_bytes: usize) -> u64 {
        wasm_fuel_budget(request_bytes).min(self.fuel_cap)
    }

    /// The output cap in bytes (standard output and error together).
    #[must_use]
    pub fn output_bytes(&self) -> usize {
        self.output_bytes
    }

    /// The wall-clock deadline of one run.
    #[must_use]
    pub fn deadline(&self) -> Duration {
        self.deadline
    }
}
