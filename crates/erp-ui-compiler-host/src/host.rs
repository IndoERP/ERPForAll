// SPDX-License-Identifier: MIT OR Apache-2.0
//! The bounded run of the compiler artifact.
//!
//! # How a run is bounded
//!
//! The guest's `_start` runs through wasmi's resumable calls. Fuel is handed out in slices of
//! [`FUEL_SLICE`] units: when a slice runs out, the call pauses, the host checks the
//! [`CancelToken`] and the deadline, and only then hands out the next slice. A loop that makes no
//! host call is therefore stopped within one slice of a cancellation or the deadline. When the
//! run's whole budget is spent, the result is [`LimitError::FuelExhausted`].
//!
//! Host functions check the token and the deadline as well, and stop the guest when the output
//! cap is reached. A denied memory growth is recorded by the limiter. These stops are sticky:
//! once recorded, the run ends with them whatever the guest does next. Any error discards the
//! guest's output.

use crate::artifact::WasmArtifact;
use crate::error::{CompilerHostError, HostExecutionError, LimitError, MAX_STDERR_IN_ERROR};
use crate::limits::ExecutionLimits;
use crate::wasi::{self, HostState, Stop};
use erp_ui_compiler_cli::{ArchCompileRequest, EXIT_CLEAN, EXIT_ERRORS};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use wasmi::{Engine, Error, Module, Store, TypedResumableCall};

/// The fuel handed to the guest at a time; between slices the host checks cancellation and the
/// deadline.
const FUEL_SLICE: u64 = 1_000_000;

/// A cancellation flag shared between the caller and a run. Clones share the flag; cancelling
/// any clone stops every run that holds it at its next check (at most one fuel slice away).
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels every run holding this token (or a clone). Irreversible.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the token was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The compiler's answer to one run: its standard output, whether the result is clean (exit
/// status 0) or carries compiler errors (exit status 1), and the fuel the run used. Built only by
/// the host from a completed run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostResponse {
    bytes: Vec<u8>,
    clean: bool,
    fuel_used: u64,
}

impl HostResponse {
    /// The compiler's canonical output bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The output bytes, owned.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Whether the compiler exited clean (no rejected template, no set error, a compiled set).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.clean
    }

    /// The compiler's exit status: [`EXIT_CLEAN`] or [`EXIT_ERRORS`].
    #[must_use]
    pub fn exit_status(&self) -> u8 {
        if self.clean { EXIT_CLEAN } else { EXIT_ERRORS }
    }

    /// The fuel the run used. Deterministic for the same artifact and input.
    #[must_use]
    pub fn fuel_used(&self) -> u64 {
        self.fuel_used
    }
}

/// How the guest's call ended, before the sticky stops are applied.
enum Ended {
    /// `_start` returned or the guest called `proc_exit`.
    Exited(i32),
    /// The whole fuel budget was spent.
    FuelExhausted,
    /// Cancellation or the deadline seen between slices.
    Interrupted(Stop),
    /// A host function returned an error that is not an exit (its description).
    HostCall(String),
    /// The guest trapped.
    Trap(Error),
}

/// The fuel a run may still hand out.
struct FuelMeter {
    budget: u64,
    unallocated: u64,
}

impl FuelMeter {
    /// Gives the store enough fuel for `required` more units (at least one slice), or returns
    /// `false` when the budget cannot cover it.
    fn refill(&mut self, store: &mut Store<HostState>, required: u64) -> Result<bool, Error> {
        let left = store.get_fuel()?;
        let needed = required.saturating_sub(left);
        if needed > self.unallocated {
            return Ok(false);
        }
        let grant = needed.max(FUEL_SLICE).min(self.unallocated);
        self.unallocated -= grant;
        store.set_fuel(left.saturating_add(grant))?;
        Ok(true)
    }

    fn used(&self, store: &Store<HostState>) -> Result<u64, Error> {
        let left = store.get_fuel()?;
        Ok(self
            .budget
            .saturating_sub(self.unallocated)
            .saturating_sub(left))
    }
}

fn engine_error(e: &Error) -> CompilerHostError {
    CompilerHostError::HostExecution(HostExecutionError::Engine(e.to_string()))
}

/// The guest's standard error as text, cut to [`MAX_STDERR_IN_ERROR`] bytes.
fn bounded_stderr(stderr: &[u8]) -> String {
    match stderr.get(..MAX_STDERR_IN_ERROR) {
        Some(head) if stderr.len() > MAX_STDERR_IN_ERROR => {
            format!("{} [truncated]", String::from_utf8_lossy(head))
        }
        _ => String::from_utf8_lossy(stderr).into_owned(),
    }
}

/// Runs the admitted compiler artifact on arch requests, one bounded run per request.
///
/// The host holds no per-run state: each call gets a fresh store, so one host can serve
/// concurrent calls from several threads.
#[derive(Clone)]
pub struct WasmCompilerHost {
    engine: Engine,
    module: Module,
    limits: ExecutionLimits,
}

impl std::fmt::Debug for WasmCompilerHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmCompilerHost")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl WasmCompilerHost {
    /// A host for `artifact` under `limits`.
    #[must_use]
    pub fn new(artifact: &WasmArtifact, limits: ExecutionLimits) -> Self {
        Self {
            engine: artifact.engine().clone(),
            module: artifact.module().clone(),
            limits,
        }
    }

    /// The limits every run of this host gets.
    #[must_use]
    pub fn limits(&self) -> &ExecutionLimits {
        &self.limits
    }

    /// Compiles one arch request in the sandbox.
    ///
    /// # Errors
    /// [`CompilerHostError::Request`] when the request cannot be encoded;
    /// [`CompilerHostError::Limit`] for fuel, memory, output or deadline;
    /// [`CompilerHostError::HostExecution`] for a cancellation, a trap, a failed instantiation or
    /// host call, or an exit status other than 0 and 1 (a refused request exits 2, with the
    /// compiler's message in the bounded standard error).
    pub fn compile_arch(
        &self,
        request: &ArchCompileRequest,
        cancel: &CancelToken,
    ) -> Result<HostResponse, CompilerHostError> {
        let bytes = request.encode()?;
        self.run(&bytes, cancel)
    }

    /// Runs arbitrary framed bytes, such as a trusted request, under the same limits.
    ///
    /// Test-only: compiled only with the `test-raw-input` feature, which the env-gated artefact
    /// test enables to compare a trusted request with the native `respond()`. A shipped build has
    /// no such entry point: the only input it runs is an [`ArchCompileRequest`].
    ///
    /// # Errors
    /// As [`WasmCompilerHost::compile_arch`].
    #[cfg(feature = "test-raw-input")]
    pub fn run_raw_for_tests(
        &self,
        input: &[u8],
        cancel: &CancelToken,
    ) -> Result<HostResponse, CompilerHostError> {
        self.run(input, cancel)
    }

    fn run(&self, input: &[u8], cancel: &CancelToken) -> Result<HostResponse, CompilerHostError> {
        let started = Instant::now();
        if cancel.is_cancelled() {
            return Err(HostExecutionError::Cancelled.into());
        }
        let memory_cap = usize::try_from(self.limits.memory_bytes()).map_err(|_| {
            HostExecutionError::Engine(format!(
                "the memory limit of {} bytes does not fit this platform's address space",
                self.limits.memory_bytes()
            ))
        })?;
        let budget = self.limits.fuel_for(input.len());
        let mut store = Store::new(
            &self.engine,
            HostState::new(
                input.to_vec(),
                self.limits.output_bytes(),
                memory_cap,
                cancel.clone(),
                started,
                self.limits.deadline(),
            ),
        );
        store.limiter(|state| &mut state.limiter);
        let mut meter = FuelMeter {
            budget,
            unallocated: budget,
        };
        if !meter.refill(&mut store, 0).map_err(|e| engine_error(&e))? {
            return Err(LimitError::FuelExhausted { budget }.into());
        }
        let linker = wasi::linker(&self.engine).map_err(|e| engine_error(&e))?;
        let instance = match linker.instantiate_and_start(&mut store, &self.module) {
            Ok(instance) => instance,
            Err(e) => {
                if store.data().memory_denied() {
                    return Err(self.memory_exhausted());
                }
                return Err(HostExecutionError::Instantiate(e.to_string()).into());
            }
        };
        let start = instance
            .get_typed_func::<(), ()>(&store, "_start")
            .map_err(|e| HostExecutionError::Instantiate(e.to_string()))?;

        let mut call = start.call_resumable(&mut store, ());
        let ended = loop {
            match call {
                Ok(TypedResumableCall::Finished(())) => break Ended::Exited(0),
                // A host function returned an error; it is never resumed.
                Ok(TypedResumableCall::HostTrap(paused)) => {
                    let error = paused.host_error();
                    break match error.i32_exit_status() {
                        Some(code) => Ended::Exited(code),
                        None => Ended::HostCall(error.to_string()),
                    };
                }
                Ok(TypedResumableCall::OutOfFuel(paused)) => {
                    if let Some(stop) = store.data_mut().check_interrupts() {
                        break Ended::Interrupted(stop);
                    }
                    let refilled = meter
                        .refill(&mut store, paused.required_fuel())
                        .map_err(|e| engine_error(&e))?;
                    if !refilled {
                        break Ended::FuelExhausted;
                    }
                    call = paused.resume(&mut store);
                }
                Err(error) => break Ended::Trap(error),
            }
        };
        let fuel_used = meter.used(&store).map_err(|e| engine_error(&e))?;
        self.classify(store, ended, budget, fuel_used)
    }

    fn memory_exhausted(&self) -> CompilerHostError {
        LimitError::MemoryExhausted {
            cap_bytes: self.limits.memory_bytes(),
        }
        .into()
    }

    fn stop_error(&self, stop: Stop) -> CompilerHostError {
        match stop {
            Stop::Output => LimitError::OutputExhausted {
                max_bytes: self.limits.output_bytes(),
            }
            .into(),
            Stop::Cancelled => HostExecutionError::Cancelled.into(),
            Stop::Deadline => LimitError::DeadlineExceeded {
                deadline: self.limits.deadline(),
            }
            .into(),
        }
    }

    /// The run's result: sticky limit and interrupt stops first, then how the call ended.
    fn classify(
        &self,
        store: Store<HostState>,
        ended: Ended,
        budget: u64,
        fuel_used: u64,
    ) -> Result<HostResponse, CompilerHostError> {
        let state = store.data();
        if state.memory_denied() {
            return Err(self.memory_exhausted());
        }
        if let Some(stop) = state.stop() {
            return Err(self.stop_error(stop));
        }
        let status = match ended {
            Ended::Exited(status) => status,
            Ended::FuelExhausted => return Err(LimitError::FuelExhausted { budget }.into()),
            Ended::HostCall(error) => return Err(HostExecutionError::HostCall(error).into()),
            Ended::Trap(error) => {
                let text = match error.as_trap_code() {
                    Some(code) => code.to_string(),
                    None => error.to_string(),
                };
                return Err(HostExecutionError::Trap(text).into());
            }
            Ended::Interrupted(stop) => return Err(self.stop_error(stop)),
        };
        let (stdout, stderr) = store.into_data().into_output();
        let clean = match status {
            0 => true,
            1 => false,
            other => {
                return Err(HostExecutionError::ExitStatus {
                    status: other,
                    stderr: bounded_stderr(&stderr),
                }
                .into());
            }
        };
        Ok(HostResponse {
            bytes: stdout,
            clean,
            fuel_used,
        })
    }
}
