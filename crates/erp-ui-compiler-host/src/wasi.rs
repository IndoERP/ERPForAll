// SPDX-License-Identifier: MIT OR Apache-2.0
//! The WASI preview 1 shim: the eight functions of [`crate::ALLOWED_WASI_FUNCTIONS`], bounded.
//!
//! Guest pointers and lengths are `i32` values reinterpreted as `u32`; every offset is computed
//! with checked arithmetic and an access outside linear memory returns `EFAULT`. A host function
//! that must stop the guest (output cap, cancellation, deadline) records a sticky [`Stop`] in the
//! store state and returns an error, which ends the call: the host never resumes it.

use crate::artifact::ALLOWED_WASI_MODULE;
use crate::host::CancelToken;
use std::time::{Duration, Instant};
use wasmi::errors::{MemoryError, TableError};
use wasmi::{
    Caller, Engine, Error, Extern, Linker, Memory, ResourceLimiter, StoreLimits, StoreLimitsBuilder,
};
use wasmi_core::LimiterError;

const ERRNO_SUCCESS: i32 = 0;
const ERRNO_BADF: i32 = 8;
const ERRNO_FAULT: i32 = 21;
const ERRNO_INVAL: i32 = 28;

/// The most iovec entries one `fd_read`/`fd_write` call may pass (WASI's `IOV_MAX` on common
/// systems); more is `EINVAL`, so a guest-chosen count never drives host work.
const MAX_IOVECS: u32 = 1024;

/// The most table elements a guest may have (its function table; growth beyond is denied).
const MAX_TABLE_ELEMENTS: usize = 1 << 20;

/// The program name passed as `argv[0]`.
const ARGV0: &[u8] = b"erp-ui-compile\0";

/// Why a host function stopped the guest. Sticky: the first stop wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The guest wrote past the output cap.
    Output,
    /// The caller cancelled the run.
    Cancelled,
    /// The deadline passed.
    Deadline,
}

/// The memory limiter: the linear-memory cap of [`StoreLimits`], plus a sticky flag set when the
/// cap (not the module's own maximum) denies a growth. A denied `memory.grow` returns -1 to the
/// guest, so the flag is the only reliable signal.
pub(crate) struct CappedLimiter {
    inner: StoreLimits,
    cap: usize,
    denied: bool,
}

impl CappedLimiter {
    fn new(cap: usize) -> Self {
        Self {
            inner: StoreLimitsBuilder::new()
                .memory_size(cap)
                .table_elements(MAX_TABLE_ELEMENTS)
                .instances(1)
                .memories(1)
                .tables(1)
                .build(),
            cap,
            denied: false,
        }
    }
}

impl ResourceLimiter for CappedLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        let allowed = self.inner.memory_growing(current, desired, maximum)?;
        if !allowed && desired > self.cap {
            self.denied = true;
        }
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        self.inner.table_growing(current, desired, maximum)
    }

    // Not a cap denial: wasmi also reports a growth that ran out of fuel here, which the host
    // handles as a resumable out-of-fuel stop.
    fn memory_grow_failed(&mut self, error: &MemoryError) -> Result<(), LimiterError> {
        self.inner.memory_grow_failed(error)
    }

    fn table_grow_failed(&mut self, error: &TableError) -> Result<(), LimiterError> {
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

/// The store state of one run.
pub(crate) struct HostState {
    stdin: Vec<u8>,
    stdin_at: usize,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_cap: usize,
    /// xorshift64 state of `random_get`: deterministic, not a security source (std seeds its
    /// `HashMap` hasher from it; the compiler's output does not depend on iteration order).
    rng: u64,
    cancel: CancelToken,
    started: Instant,
    deadline: Duration,
    stop: Option<Stop>,
    pub(crate) limiter: CappedLimiter,
}

impl HostState {
    pub(crate) fn new(
        stdin: Vec<u8>,
        output_cap: usize,
        memory_cap: usize,
        cancel: CancelToken,
        started: Instant,
        deadline: Duration,
    ) -> Self {
        Self {
            stdin,
            stdin_at: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            output_cap,
            rng: 0x9E37_79B9_7F4A_7C15,
            cancel,
            started,
            deadline,
            stop: None,
            limiter: CappedLimiter::new(memory_cap),
        }
    }

    /// Records `stop` unless an earlier stop is recorded.
    pub(crate) fn record_stop(&mut self, stop: Stop) {
        if self.stop.is_none() {
            self.stop = Some(stop);
        }
    }

    pub(crate) fn stop(&self) -> Option<Stop> {
        self.stop
    }

    pub(crate) fn memory_denied(&self) -> bool {
        self.limiter.denied
    }

    /// Checks the cancel token and the deadline, recording the first that applies.
    pub(crate) fn check_interrupts(&mut self) -> Option<Stop> {
        if self.cancel.is_cancelled() {
            self.record_stop(Stop::Cancelled);
        } else if self.started.elapsed() >= self.deadline {
            self.record_stop(Stop::Deadline);
        }
        self.stop
    }

    pub(crate) fn into_output(self) -> (Vec<u8>, Vec<u8>) {
        (self.stdout, self.stderr)
    }
}

/// Stops the guest when the run was cancelled or its deadline passed.
fn interrupt(caller: &mut Caller<'_, HostState>) -> Result<(), Error> {
    match caller.data_mut().check_interrupts() {
        None => Ok(()),
        Some(stop) => Err(Error::new(format!("host stopped the guest: {stop:?}"))),
    }
}

fn memory(caller: &Caller<'_, HostState>) -> Result<Memory, Error> {
    caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| Error::new("the module exports no memory"))
}

/// A guest `i32` address or length, reinterpreted as `u32`.
fn guest(value: i32) -> usize {
    value as u32 as usize
}

/// `mem[at..at + len]`, or `None` outside memory.
fn range(mem: &[u8], at: usize, len: usize) -> Option<&[u8]> {
    mem.get(at..at.checked_add(len)?)
}

fn range_mut(mem: &mut [u8], at: usize, len: usize) -> Option<&mut [u8]> {
    mem.get_mut(at..at.checked_add(len)?)
}

fn read_u32(mem: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = range(mem, at, 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn write_u32(mem: &mut [u8], at: usize, value: u32) -> Option<()> {
    range_mut(mem, at, 4)?.copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// The `(buf, len)` of iovec entry `index` of the array at `iovs`.
fn iovec(mem: &[u8], iovs: usize, index: u32) -> Option<(usize, usize)> {
    let at = iovs.checked_add(usize::try_from(index).ok()?.checked_mul(8)?)?;
    let buf = read_u32(mem, at)? as usize;
    let len = read_u32(mem, at.checked_add(4)?)? as usize;
    Some((buf, len))
}

fn errno(result: Option<()>) -> i32 {
    match result {
        Some(()) => ERRNO_SUCCESS,
        None => ERRNO_FAULT,
    }
}

fn fd_write(
    mut caller: Caller<'_, HostState>,
    fd: i32,
    iovs: i32,
    count: i32,
    written: i32,
) -> Result<i32, Error> {
    interrupt(&mut caller)?;
    let to_stdout = match fd {
        1 => true,
        2 => false,
        _ => return Ok(ERRNO_BADF),
    };
    let count = count as u32;
    if count > MAX_IOVECS {
        return Ok(ERRNO_INVAL);
    }
    let (iovs, written) = (guest(iovs), guest(written));
    let mem = memory(&caller)?;
    let (data, state) = mem.data_and_store_mut(&mut caller);
    // The whole length first, against the cap, before anything is copied.
    let mut total = 0usize;
    for i in 0..count {
        let Some((buf, len)) = iovec(data, iovs, i) else {
            return Ok(ERRNO_FAULT);
        };
        if range(data, buf, len).is_none() {
            return Ok(ERRNO_FAULT);
        }
        let Some(sum) = total.checked_add(len) else {
            return Ok(ERRNO_FAULT);
        };
        total = sum;
    }
    let used = state.stdout.len().saturating_add(state.stderr.len());
    let within_cap = used
        .checked_add(total)
        .is_some_and(|after| after <= state.output_cap);
    if !within_cap {
        state.record_stop(Stop::Output);
        return Err(Error::new("host stopped the guest: output limit reached"));
    }
    let Ok(total_u32) = u32::try_from(total) else {
        return Ok(ERRNO_FAULT);
    };
    for i in 0..count {
        let Some((buf, len)) = iovec(data, iovs, i) else {
            return Ok(ERRNO_FAULT);
        };
        let Some(bytes) = range(data, buf, len) else {
            return Ok(ERRNO_FAULT);
        };
        if to_stdout {
            state.stdout.extend_from_slice(bytes);
        } else {
            state.stderr.extend_from_slice(bytes);
        }
    }
    Ok(errno(write_u32(data, written, total_u32)))
}

fn fd_read(
    mut caller: Caller<'_, HostState>,
    fd: i32,
    iovs: i32,
    count: i32,
    read: i32,
) -> Result<i32, Error> {
    interrupt(&mut caller)?;
    if fd != 0 {
        return Ok(ERRNO_BADF);
    }
    let count = count as u32;
    if count > MAX_IOVECS {
        return Ok(ERRNO_INVAL);
    }
    let (iovs, read) = (guest(iovs), guest(read));
    let mem = memory(&caller)?;
    let (data, state) = mem.data_and_store_mut(&mut caller);
    let mut total = 0usize;
    for i in 0..count {
        let Some((buf, len)) = iovec(data, iovs, i) else {
            return Ok(ERRNO_FAULT);
        };
        let Some(remaining) = state.stdin.get(state.stdin_at..) else {
            return Ok(ERRNO_FAULT);
        };
        let n = remaining.len().min(len);
        let (Some(dest), Some(src)) = (range_mut(data, buf, n), remaining.get(..n)) else {
            return Ok(ERRNO_FAULT);
        };
        dest.copy_from_slice(src);
        state.stdin_at = state.stdin_at.saturating_add(n);
        total = total.saturating_add(n);
        if n < len {
            break;
        }
    }
    let Ok(total_u32) = u32::try_from(total) else {
        return Ok(ERRNO_FAULT);
    };
    Ok(errno(write_u32(data, read, total_u32)))
}

fn args_sizes_get(mut caller: Caller<'_, HostState>, argc: i32, size: i32) -> Result<i32, Error> {
    interrupt(&mut caller)?;
    let mem = memory(&caller)?;
    let data = mem.data_mut(&mut caller);
    Ok(errno(write_u32(data, guest(argc), 1).and_then(|()| {
        write_u32(data, guest(size), ARGV0.len() as u32)
    })))
}

fn args_get(mut caller: Caller<'_, HostState>, argv: i32, buf: i32) -> Result<i32, Error> {
    interrupt(&mut caller)?;
    let mem = memory(&caller)?;
    let data = mem.data_mut(&mut caller);
    let Some(dest) = range_mut(data, guest(buf), ARGV0.len()) else {
        return Ok(ERRNO_FAULT);
    };
    dest.copy_from_slice(ARGV0);
    Ok(errno(write_u32(data, guest(argv), buf as u32)))
}

fn environ_sizes_get(
    mut caller: Caller<'_, HostState>,
    count: i32,
    size: i32,
) -> Result<i32, Error> {
    interrupt(&mut caller)?;
    let mem = memory(&caller)?;
    let data = mem.data_mut(&mut caller);
    Ok(errno(
        write_u32(data, guest(count), 0).and_then(|()| write_u32(data, guest(size), 0)),
    ))
}

/// The environment is empty, so there is nothing to write.
fn environ_get(mut caller: Caller<'_, HostState>, _environ: i32, _buf: i32) -> Result<i32, Error> {
    interrupt(&mut caller)?;
    Ok(ERRNO_SUCCESS)
}

fn random_get(mut caller: Caller<'_, HostState>, buf: i32, len: i32) -> Result<i32, Error> {
    interrupt(&mut caller)?;
    let mem = memory(&caller)?;
    let (data, state) = mem.data_and_store_mut(&mut caller);
    let Some(dest) = range_mut(data, guest(buf), guest(len)) else {
        return Ok(ERRNO_FAULT);
    };
    for byte in dest {
        state.rng ^= state.rng << 13;
        state.rng ^= state.rng >> 7;
        state.rng ^= state.rng << 17;
        *byte = state.rng.to_le_bytes()[0];
    }
    Ok(ERRNO_SUCCESS)
}

fn proc_exit(code: i32) -> Result<(), Error> {
    Err(Error::i32_exit(code))
}

/// The linker with the eight shim functions.
///
/// # Errors
/// The wasmi error of a definition that fails (a duplicate name).
pub(crate) fn linker(engine: &Engine) -> Result<Linker<HostState>, Error> {
    let mut linker = Linker::<HostState>::new(engine);
    linker.func_wrap(ALLOWED_WASI_MODULE, "fd_write", fd_write)?;
    linker.func_wrap(ALLOWED_WASI_MODULE, "fd_read", fd_read)?;
    linker.func_wrap(ALLOWED_WASI_MODULE, "proc_exit", proc_exit)?;
    linker.func_wrap(ALLOWED_WASI_MODULE, "args_sizes_get", args_sizes_get)?;
    linker.func_wrap(ALLOWED_WASI_MODULE, "args_get", args_get)?;
    linker.func_wrap(ALLOWED_WASI_MODULE, "environ_sizes_get", environ_sizes_get)?;
    linker.func_wrap(ALLOWED_WASI_MODULE, "environ_get", environ_get)?;
    linker.func_wrap(ALLOWED_WASI_MODULE, "random_get", random_get)?;
    Ok(linker)
}
