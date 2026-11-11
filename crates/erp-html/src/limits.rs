//! Resource limits for one sanitising call.
//!
//! [`Limits`] starts at the [ceiling](Limits::ceiling): a `with_*` method sets one limit to a value
//! from 1 up to the ceiling and returns a [`LimitError`] otherwise (it may set a limit lower or
//! raise one it lowered earlier, never above the ceiling). Zero is never "unlimited" and nothing
//! can go above the ceiling, so a caller cannot widen what the sanitiser accepts beyond it.
//! [`Limits::recommended`] is the ceiling with the input cap the real-sink envelope supports.

use std::fmt;
use std::num::NonZeroU64;

/// Which limit a [`LimitError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitKind {
    /// Bytes of input markup.
    InputBytes,
    /// Element nesting depth, counting the generated root as depth 1.
    Depth,
    /// DOM nodes, including text, comments and generated nodes.
    Nodes,
    /// Bytes of serialised output.
    OutputBytes,
    /// Live working bytes: DOM, buffers and reports.
    WorkingBytes,
    /// Work units across token input bytes, tree operations and emitted bytes.
    WorkUnits,
    /// Bytes of the aggregated removal report.
    ReportBytes,
    /// Sample locations kept per removal report entry.
    ReportSamples,
}

impl LimitKind {
    /// The name used in messages, matching the [`Limits`] accessor.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            LimitKind::InputBytes => "input_bytes",
            LimitKind::Depth => "depth",
            LimitKind::Nodes => "nodes",
            LimitKind::OutputBytes => "output_bytes",
            LimitKind::WorkingBytes => "working_bytes",
            LimitKind::WorkUnits => "work_units",
            LimitKind::ReportBytes => "report_bytes",
            LimitKind::ReportSamples => "report_samples",
        }
    }
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A rejected limit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitError {
    /// The value was 0; a limit is at least 1.
    Zero {
        /// The limit being set.
        limit: LimitKind,
    },
    /// The value was above the [ceiling](Limits::ceiling) for that limit.
    AboveCeiling {
        /// The limit being set.
        limit: LimitKind,
        /// The value that was asked for.
        requested: u64,
        /// The highest value allowed.
        ceiling: u64,
    },
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitError::Zero { limit } => write!(f, "limit `{limit}` must be at least 1"),
            LimitError::AboveCeiling {
                limit,
                requested,
                ceiling,
            } => {
                write!(
                    f,
                    "limit `{limit}` is {requested}, above the ceiling of {ceiling}"
                )
            }
        }
    }
}

impl std::error::Error for LimitError {}

const CEILING_INPUT_BYTES: u64 = 10_000_000;
const CEILING_DEPTH: u64 = 256;
const CEILING_NODES: u64 = 100_000;
const CEILING_OUTPUT_BYTES: u64 = 32_000_000;
const CEILING_WORKING_BYTES: u64 = 128_000_000;
const CEILING_WORK_UNITS: u64 = 50_000_000;
const CEILING_REPORT_BYTES: u64 = 1_000_000;
const CEILING_REPORT_SAMPLES: u64 = 8;

/// Validated limits for one sanitising call. Build with [`Limits::ceiling`] and lower with the
/// `with_*` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    input_bytes: NonZeroU64,
    depth: NonZeroU64,
    nodes: NonZeroU64,
    output_bytes: NonZeroU64,
    working_bytes: NonZeroU64,
    work_units: NonZeroU64,
    report_bytes: NonZeroU64,
    report_samples: NonZeroU64,
}

/// Validate `value` against `1..=ceiling`.
fn checked(limit: LimitKind, value: u64, ceiling: u64) -> Result<NonZeroU64, LimitError> {
    let value = NonZeroU64::new(value).ok_or(LimitError::Zero { limit })?;
    if value.get() > ceiling {
        return Err(LimitError::AboveCeiling {
            limit,
            requested: value.get(),
            ceiling,
        });
    }
    Ok(value)
}

// The recommended limits (`Limits::recommended`). The input cap is the formula (ENGINE.D
// amendment) applied to the real-sink envelope log named below, `min(ceiling, floor((W - C) / K_input))`
// with `sink_reserved` 0: `scripts/check-html-limits.py` (CI step `html-limits-log`) recomputes it from
// the log's rows and fails when this file differs. Do not edit the numbers by hand.
// ENVELOPE-LOG: crates/erp-html/limits/05a6b8925591648f73976066a93f50421a5cca40.log
const RECOMMENDED_WORKING_BYTES: u64 = 128_000_000;
const RECOMMENDED_INPUT_BYTES: u64 = 1_788_829;

// The ceilings are compile-time positive; these assertions fail the build otherwise.
const _: () = assert!(CEILING_INPUT_BYTES > 0 && CEILING_DEPTH > 0 && CEILING_NODES > 0);
const _: () =
    assert!(CEILING_OUTPUT_BYTES > 0 && CEILING_WORKING_BYTES > 0 && CEILING_WORK_UNITS > 0);
const _: () = assert!(CEILING_REPORT_BYTES > 0 && CEILING_REPORT_SAMPLES > 0);
// The recommended limits are valid: positive and not above their ceilings.
const _: () =
    assert!(RECOMMENDED_INPUT_BYTES > 0 && RECOMMENDED_INPUT_BYTES <= CEILING_INPUT_BYTES);
const _: () =
    assert!(RECOMMENDED_WORKING_BYTES > 0 && RECOMMENDED_WORKING_BYTES <= CEILING_WORKING_BYTES);

/// A ceiling constant as `NonZeroU64`. The fallback arm is unreachable for the constants
/// above, which the assertions guarantee.
const fn nz(value: u64) -> NonZeroU64 {
    match NonZeroU64::new(value) {
        Some(value) => value,
        None => NonZeroU64::MIN,
    }
}

impl Limits {
    /// The highest limits allows: 10,000,000 input bytes, depth 256, 100,000 nodes,
    /// 32,000,000 output bytes, 128,000,000 working bytes and 50,000,000 work units.
    #[must_use]
    pub const fn ceiling() -> Limits {
        Limits {
            input_bytes: nz(CEILING_INPUT_BYTES),
            depth: nz(CEILING_DEPTH),
            nodes: nz(CEILING_NODES),
            output_bytes: nz(CEILING_OUTPUT_BYTES),
            working_bytes: nz(CEILING_WORKING_BYTES),
            work_units: nz(CEILING_WORK_UNITS),
            report_bytes: nz(CEILING_REPORT_BYTES),
            report_samples: nz(CEILING_REPORT_SAMPLES),
        }
    }

    /// The limits to use for markup that comes from a user: the [ceiling](Limits::ceiling) with the
    /// input cap lowered so the call's heap use stays inside the working limit on the adversarial
    /// corpus.
    ///
    /// The input cap is `floor((W - C) / K_input)` where `W` is the working limit (128,000,000, the
    /// ceiling), `C` the heap peak of the empty input and `K_input` the largest heap peak per input
    /// byte over the corpus entries the real sanitiser parsed, measured with the real sink
    /// (`sink_reserved` = 0) by `scripts/html-budget-envelope.sh --sink erp-html`; the log is named
    /// in `limits.rs` and `scripts/check-html-limits.py` recomputes the cap from it on every CI run.
    /// Depth, nodes, output bytes and work units stay at their ceilings (the work limit is not
    /// re-derived here: latency and work cost are not measured).
    ///
    /// This is a corpus envelope, not a proof that every input of that size stays under `W`; the
    /// working-memory limit refuses a call whose charged working memory would pass it. It counts
    /// charged bytes, not heap: the heap can sit slightly above the charge (see the log).
    #[must_use]
    pub const fn recommended() -> Limits {
        let mut limits = Limits::ceiling();
        limits.input_bytes = nz(RECOMMENDED_INPUT_BYTES);
        limits.working_bytes = nz(RECOMMENDED_WORKING_BYTES);
        limits
    }

    /// Maximum input bytes.
    #[must_use]
    pub fn input_bytes(&self) -> u64 {
        self.input_bytes.get()
    }

    /// Maximum element depth (the generated root is depth 1).
    #[must_use]
    pub fn depth(&self) -> u64 {
        self.depth.get()
    }

    /// Maximum DOM nodes.
    #[must_use]
    pub fn nodes(&self) -> u64 {
        self.nodes.get()
    }

    /// Maximum serialised output bytes.
    #[must_use]
    pub fn output_bytes(&self) -> u64 {
        self.output_bytes.get()
    }

    /// Maximum live working bytes.
    #[must_use]
    pub fn working_bytes(&self) -> u64 {
        self.working_bytes.get()
    }

    /// Maximum work units.
    #[must_use]
    pub fn work_units(&self) -> u64 {
        self.work_units.get()
    }

    /// Maximum bytes of the aggregated removal report.
    #[must_use]
    pub fn report_bytes(&self) -> u64 {
        self.report_bytes.get()
    }

    /// Maximum sample locations kept per report entry.
    #[must_use]
    pub fn report_samples(&self) -> u64 {
        self.report_samples.get()
    }

    /// Set the input byte limit.
    ///
    /// # Errors
    /// [`LimitError::Zero`] for 0, [`LimitError::AboveCeiling`] above 10,000,000.
    pub fn with_input_bytes(mut self, value: u64) -> Result<Limits, LimitError> {
        self.input_bytes = checked(LimitKind::InputBytes, value, CEILING_INPUT_BYTES)?;
        Ok(self)
    }

    /// Set the depth limit.
    ///
    /// # Errors
    /// [`LimitError::Zero`] for 0, [`LimitError::AboveCeiling`] above 256.
    pub fn with_depth(mut self, value: u64) -> Result<Limits, LimitError> {
        self.depth = checked(LimitKind::Depth, value, CEILING_DEPTH)?;
        Ok(self)
    }

    /// Set the node limit.
    ///
    /// # Errors
    /// [`LimitError::Zero`] for 0, [`LimitError::AboveCeiling`] above 100,000.
    pub fn with_nodes(mut self, value: u64) -> Result<Limits, LimitError> {
        self.nodes = checked(LimitKind::Nodes, value, CEILING_NODES)?;
        Ok(self)
    }

    /// Set the output byte limit.
    ///
    /// # Errors
    /// [`LimitError::Zero`] for 0, [`LimitError::AboveCeiling`] above 32,000,000.
    pub fn with_output_bytes(mut self, value: u64) -> Result<Limits, LimitError> {
        self.output_bytes = checked(LimitKind::OutputBytes, value, CEILING_OUTPUT_BYTES)?;
        Ok(self)
    }

    /// Set the working byte limit.
    ///
    /// # Errors
    /// [`LimitError::Zero`] for 0, [`LimitError::AboveCeiling`] above 128,000,000.
    pub fn with_working_bytes(mut self, value: u64) -> Result<Limits, LimitError> {
        self.working_bytes = checked(LimitKind::WorkingBytes, value, CEILING_WORKING_BYTES)?;
        Ok(self)
    }

    /// Set the work-unit limit.
    ///
    /// # Errors
    /// [`LimitError::Zero`] for 0, [`LimitError::AboveCeiling`] above 50,000,000.
    pub fn with_work_units(mut self, value: u64) -> Result<Limits, LimitError> {
        self.work_units = checked(LimitKind::WorkUnits, value, CEILING_WORK_UNITS)?;
        Ok(self)
    }

    /// Set the report byte limit.
    ///
    /// # Errors
    /// [`LimitError::Zero`] for 0, [`LimitError::AboveCeiling`] above 1,000,000.
    pub fn with_report_bytes(mut self, value: u64) -> Result<Limits, LimitError> {
        self.report_bytes = checked(LimitKind::ReportBytes, value, CEILING_REPORT_BYTES)?;
        Ok(self)
    }

    /// Set the number of sample locations kept per report entry.
    ///
    /// # Errors
    /// [`LimitError::Zero`] for 0, [`LimitError::AboveCeiling`] above 8.
    pub fn with_report_samples(mut self, value: u64) -> Result<Limits, LimitError> {
        self.report_samples = checked(LimitKind::ReportSamples, value, CEILING_REPORT_SAMPLES)?;
        Ok(self)
    }
}
