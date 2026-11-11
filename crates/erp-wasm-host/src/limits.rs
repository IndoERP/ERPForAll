//! Per-call resource limits (plan, shape).
//!
//! Every limit is on by default at its documented default; a host may lower it per database or
//! tenant, never raise it above the hard maximum or remove it, and zero is rejected.

use std::time::Duration;

use crate::error::{HostError, LimitKind};

/// Validated resource limits. Build with [`Limits::default`] and the `with_*` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct Limits {
    cpu: Duration,
    memory_bytes: usize,
    stack_depth: u32,
    batch_ids: u32,
    payload_bytes: usize,
    tx_memory_bytes: usize,
    idle_stores: u32,
}

impl Limits {
    /// Hard maximum CPU time per call: 2 s.
    pub const MAX_CPU: Duration = Duration::from_millis(2_000);
    /// Hard maximum guest memory: 128 MiB.
    pub const MAX_MEMORY_BYTES: usize = 128 * 1024 * 1024;
    /// Hard maximum host call-stack depth: 64 frames.
    pub const MAX_STACK_DEPTH: u32 = 64;
    /// Hard maximum ids per boundary call: 500,000.
    pub const MAX_BATCH_IDS: u32 = 500_000;
    /// Hard maximum serialized payload: 64 MiB.
    pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

    /// Hard maximum guest memory held by all the stores of one transaction: 1 GiB.
    pub const MAX_TX_MEMORY_BYTES: usize = 1024 * 1024 * 1024;
    /// Hard maximum of idle pooled stores kept per addon and transaction: 32.
    pub const MAX_IDLE_STORES: u32 = 32;

    /// Lowers the memory all the stores of one transaction may hold together (idle ones at what
    /// they grew to, running ones at their full cap). Must be at least the per-store cap.
    pub fn with_tx_memory_bytes(mut self, bytes: usize) -> Result<Self, HostError> {
        check(
            LimitKind::TxMemory,
            bytes as u64,
            Self::MAX_TX_MEMORY_BYTES as u64,
        )?;
        if bytes < self.memory_bytes {
            return Err(HostError::InvalidLimit {
                limit: LimitKind::TxMemory,
                value: bytes as u64,
                max: Self::MAX_TX_MEMORY_BYTES as u64,
            });
        }
        self.tx_memory_bytes = bytes;
        Ok(self)
    }

    /// Lowers the idle stores kept per addon between calls of one transaction.
    pub fn with_idle_stores(mut self, stores: u32) -> Result<Self, HostError> {
        check(
            LimitKind::IdleStores,
            stores.into(),
            Self::MAX_IDLE_STORES.into(),
        )?;
        self.idle_stores = stores;
        Ok(self)
    }

    /// Lowers the CPU time per call.
    pub fn with_cpu(mut self, cpu: Duration) -> Result<Self, HostError> {
        if cpu.is_zero() || cpu > Self::MAX_CPU {
            return Err(HostError::InvalidLimit {
                limit: LimitKind::Cpu,
                value: u64::try_from(cpu.as_millis()).unwrap_or(u64::MAX),
                max: Self::MAX_CPU.as_millis() as u64,
            });
        }
        self.cpu = cpu;
        Ok(self)
    }

    /// Lowers the guest memory cap.
    pub fn with_memory_bytes(mut self, bytes: usize) -> Result<Self, HostError> {
        check(
            LimitKind::Memory,
            bytes as u64,
            Self::MAX_MEMORY_BYTES as u64,
        )?;
        self.memory_bytes = bytes;
        Ok(self)
    }

    /// Lowers the call-stack depth.
    pub fn with_stack_depth(mut self, depth: u32) -> Result<Self, HostError> {
        check(
            LimitKind::StackDepth,
            depth.into(),
            Self::MAX_STACK_DEPTH.into(),
        )?;
        self.stack_depth = depth;
        Ok(self)
    }

    /// Lowers the ids per boundary call.
    pub fn with_batch_ids(mut self, ids: u32) -> Result<Self, HostError> {
        check(LimitKind::BatchIds, ids.into(), Self::MAX_BATCH_IDS.into())?;
        self.batch_ids = ids;
        Ok(self)
    }

    /// Lowers the serialized payload cap.
    pub fn with_payload_bytes(mut self, bytes: usize) -> Result<Self, HostError> {
        check(
            LimitKind::PayloadBytes,
            bytes as u64,
            Self::MAX_PAYLOAD_BYTES as u64,
        )?;
        self.payload_bytes = bytes;
        Ok(self)
    }

    /// Memory all the stores of one transaction may hold together, in bytes.
    #[must_use]
    pub fn tx_memory_bytes(&self) -> usize {
        self.tx_memory_bytes
    }

    /// Idle stores kept per addon between calls of one transaction.
    #[must_use]
    pub fn idle_stores(&self) -> u32 {
        self.idle_stores
    }

    /// Wall-clock budget per call, enforced in epoch ticks (see `WasmHost`).
    #[must_use]
    pub fn cpu(&self) -> Duration {
        self.cpu
    }

    /// Guest memory cap in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes
    }

    /// Host call-stack depth. Enforced by `AddonStore::enter_call`, which the dispatcher
    /// calls; the call primitives of this slice do not nest.
    #[must_use]
    pub fn stack_depth(&self) -> u32 {
        self.stack_depth
    }

    /// Ids per boundary call; larger recordsets are split with [`Limits::chunks`].
    #[must_use]
    pub fn batch_ids(&self) -> u32 {
        self.batch_ids
    }

    /// Rejects a serialized payload above the cap.
    pub fn check_payload(&self, len: usize) -> Result<(), HostError> {
        if len > self.payload_bytes {
            return Err(HostError::LimitExceeded {
                limit: LimitKind::PayloadBytes,
                value: len as u64,
                max: self.payload_bytes as u64,
            });
        }
        Ok(())
    }

    /// Splits `ids` into slices of at most [`Limits::batch_ids`] for separate boundary calls.
    pub fn chunks<'a, T>(&self, ids: &'a [T]) -> std::slice::Chunks<'a, T> {
        ids.chunks(self.batch_ids as usize)
    }
}

impl Default for Limits {
    /// Documented defaults: 500 ms, 32 MiB, 32 frames, 100,000 ids, 16 MiB, 256 MiB for all the
    /// stores of a transaction, 4 idle stores per addon.
    fn default() -> Self {
        Self {
            cpu: Duration::from_millis(500),
            memory_bytes: 32 * 1024 * 1024,
            stack_depth: 32,
            batch_ids: 100_000,
            payload_bytes: 16 * 1024 * 1024,
            tx_memory_bytes: 256 * 1024 * 1024,
            idle_stores: 4,
        }
    }
}

fn check(limit: LimitKind, value: u64, max: u64) -> Result<(), HostError> {
    if value == 0 || value > max {
        return Err(HostError::InvalidLimit { limit, value, max });
    }
    Ok(())
}
