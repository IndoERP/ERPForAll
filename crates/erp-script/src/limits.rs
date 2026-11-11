//! Resource limits of one evaluation.
//!
//! Same rule as `erp-expr`: every limit is on by default at its maximum, a host may
//! lower it, never raise or remove it, and zero is rejected (zero never means unlimited).

use std::time::Duration;

use crate::error::{LimitKind, ScriptError, ScriptResult};

/// Largest subject string the wrapped `string.find/match/gmatch/gsub` accept, in bytes.
pub const PATTERN_SUBJECT_MAX_BYTES: usize = 64 * 1024;
/// Largest pattern the wrapped `string.find/match/gmatch/gsub` accept, in bytes.
pub const PATTERN_MAX_BYTES: usize = 256;
/// Deepest nesting of Rust-mediated re-entry into Luau (`pcall` inside `pcall`, a `gsub` replacement
/// function calling `gsub`). Bounds native stack use per evaluation.
pub const NATIVE_NESTING_MAX: u32 = 32;

/// Validated resource limits. Private fields; build with [`ScriptLimits::default`] (all maxima) and
/// lower individual limits with the `with_*` methods, which reject zero and values above the maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct ScriptLimits {
    source_bytes: usize,
    steps: u64,
    time: Duration,
    memory_bytes: usize,
    depth: usize,
    nodes: usize,
}

impl ScriptLimits {
    /// Maximum source size: 256 KiB.
    pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
    /// Maximum interpreter steps: 10,000,000 (approximate, see [`LimitKind::Steps`]).
    pub const MAX_STEPS: u64 = 10_000_000;
    /// Maximum wall-clock time: 5 seconds.
    pub const MAX_TIME: Duration = Duration::from_secs(5);
    /// Maximum VM heap: 16 MiB.
    pub const MAX_MEMORY_BYTES: usize = 16 * 1024 * 1024;
    /// Maximum nesting depth of converted values: 64.
    pub const MAX_DEPTH: usize = 64;
    /// Maximum number of values in one conversion: 100,000.
    pub const MAX_NODES: usize = 100_000;

    /// Lowers the source size limit.
    pub fn with_source_bytes(mut self, bytes: usize) -> ScriptResult<Self> {
        self.source_bytes = check(LimitKind::SourceBytes, bytes, Self::MAX_SOURCE_BYTES)?;
        Ok(self)
    }

    /// Lowers the step limit.
    pub fn with_steps(mut self, steps: u64) -> ScriptResult<Self> {
        if steps == 0 || steps > Self::MAX_STEPS {
            return Err(ScriptError::InvalidLimit {
                limit: LimitKind::Steps,
                value: steps,
                max: Self::MAX_STEPS,
            });
        }
        self.steps = steps;
        Ok(self)
    }

    /// Lowers the wall-clock limit.
    pub fn with_time(mut self, time: Duration) -> ScriptResult<Self> {
        if time.is_zero() || time > Self::MAX_TIME {
            return Err(ScriptError::InvalidLimit {
                limit: LimitKind::Time,
                // Only shown in the message; a duration beyond u64 milliseconds saturates.
                value: u64::try_from(time.as_millis()).unwrap_or(u64::MAX),
                max: duration_ms(Self::MAX_TIME),
            });
        }
        self.time = time;
        Ok(self)
    }

    /// Lowers the memory limit.
    pub fn with_memory_bytes(mut self, bytes: usize) -> ScriptResult<Self> {
        self.memory_bytes = check(LimitKind::Memory, bytes, Self::MAX_MEMORY_BYTES)?;
        Ok(self)
    }

    /// Lowers the conversion depth limit.
    pub fn with_depth(mut self, depth: usize) -> ScriptResult<Self> {
        self.depth = check(LimitKind::Depth, depth, Self::MAX_DEPTH)?;
        Ok(self)
    }

    /// Lowers the conversion value-count limit.
    pub fn with_nodes(mut self, nodes: usize) -> ScriptResult<Self> {
        self.nodes = check(LimitKind::Nodes, nodes, Self::MAX_NODES)?;
        Ok(self)
    }

    /// Source size limit in bytes.
    pub fn source_bytes(&self) -> usize {
        self.source_bytes
    }

    /// Step limit.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Wall-clock limit.
    pub fn time(&self) -> Duration {
        self.time
    }

    /// Memory limit in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes
    }

    /// Conversion depth limit.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Conversion value-count limit.
    pub fn nodes(&self) -> usize {
        self.nodes
    }

    /// The `LimitExceeded` error for `kind` with this configuration's maximum.
    pub(crate) fn exceeded(&self, kind: LimitKind) -> ScriptError {
        let max = match kind {
            LimitKind::SourceBytes => as_u64(self.source_bytes),
            LimitKind::Steps => self.steps,
            LimitKind::Time => duration_ms(self.time),
            LimitKind::Memory => as_u64(self.memory_bytes),
            LimitKind::Depth => as_u64(self.depth),
            LimitKind::Nodes => as_u64(self.nodes),
            LimitKind::ConvertedBytes => as_u64(self.memory_bytes),
        };
        ScriptError::LimitExceeded { limit: kind, max }
    }
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            source_bytes: Self::MAX_SOURCE_BYTES,
            steps: Self::MAX_STEPS,
            time: Self::MAX_TIME,
            memory_bytes: Self::MAX_MEMORY_BYTES,
            depth: Self::MAX_DEPTH,
            nodes: Self::MAX_NODES,
        }
    }
}

fn check(kind: LimitKind, value: usize, max: usize) -> ScriptResult<usize> {
    if value == 0 || value > max {
        return Err(ScriptError::InvalidLimit {
            limit: kind,
            value: as_u64(value),
            max: as_u64(max),
        });
    }
    Ok(value)
}

/// `usize` to `u64` for messages; lossless on every supported target (64-bit and 32-bit).
fn as_u64(v: usize) -> u64 {
    // usize is at most 64 bits on every target Rust supports today; saturate instead of panicking
    // should a 128-bit target ever appear (the value is only displayed).
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// Milliseconds of a duration that is at most `MAX_TIME`, rounded up so a sub-millisecond limit is
/// not shown as zero.
fn duration_ms(d: Duration) -> u64 {
    let ms = d.as_millis() + u128::from(!d.subsec_nanos().is_multiple_of(1_000_000));
    // `d` is at most MAX_TIME (5 s) wherever this is called, so the value fits.
    u64::try_from(ms).unwrap_or(u64::MAX)
}
