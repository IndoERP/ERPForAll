//! Resource limits and the per-evaluation budget that enforces them.
//!
//! Limits are a ceiling, not a default: [`EvalLimits`] can only be lowered below the values
//! (lead decision, review round 2). A [`Budget`] is created fresh for every parse-and-evaluate call,
//! so counters never carry over between evaluations.
//!
//! Accounting model:
//! - **Steps**: one per AST node evaluated and per comprehension item, plus steps in proportion to
//!   the data an operation touches (elements compared or copied, one step per
//!   [`BYTES_PER_STEP`] bytes of string scanned).
//! - **Bytes**: every value the evaluator produces is charged its deep size (see
//!   [`crate::ExprValue::deep_size`]) *before* it is built — clones of named values, repeats,
//!   concatenations, slices, formatting output, builtin results. Literal containers charge their own
//!   slots; their items were charged when they were produced.
//! - **Per value**: no single produced value may exceed `max_value_bytes` deep size or
//!   `max_depth` nesting levels.

use crate::error::{ExprError, ExprResult};
use crate::value::ExprValue;

/// String and byte scanning is charged one step per this many bytes.
pub const BYTES_PER_STEP: usize = 64;

/// Resource limits for parsing and evaluation.
///
/// The fields are private: [`EvalLimits::default`] is the ceiling and the `with_*` methods
/// clamp to it, so a caller can tighten a limit but never raise or remove one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalLimits {
    max_source_bytes: usize,
    max_depth: usize,
    max_steps: usize,
    max_value_bytes: usize,
    max_total_bytes: usize,
    max_format_width: usize,
}

impl EvalLimits {
    /// The values; also the ceiling for every `with_*` method.
    pub const CEILING: EvalLimits = EvalLimits {
        max_source_bytes: 64 * 1024,
        max_depth: 100,
        max_steps: 1_000_000,
        max_value_bytes: 1024 * 1024,
        max_total_bytes: 16 * 1024 * 1024,
        max_format_width: 1_000,
    };

    /// Maximum expression source length in bytes (64 KiB).
    pub fn max_source_bytes(&self) -> usize {
        self.max_source_bytes
    }
    /// Maximum nesting depth of the syntax tree, of evaluation and of produced values (100).
    pub fn max_depth(&self) -> usize {
        self.max_depth
    }
    /// Maximum evaluation steps per evaluation (1,000,000).
    pub fn max_steps(&self) -> usize {
        self.max_steps
    }
    /// Maximum deep size in bytes of any single produced value (1 MiB).
    pub fn max_value_bytes(&self) -> usize {
        self.max_value_bytes
    }
    /// Maximum bytes charged in total per evaluation (16 MiB).
    pub fn max_total_bytes(&self) -> usize {
        self.max_total_bytes
    }
    /// Maximum width or precision in a format specification (1,000).
    pub fn max_format_width(&self) -> usize {
        self.max_format_width
    }

    /// Lowers the source length limit; values above the ceiling are clamped to it.
    pub fn with_max_source_bytes(mut self, n: usize) -> Self {
        self.max_source_bytes = n.min(Self::CEILING.max_source_bytes);
        self
    }
    /// Lowers the nesting depth limit; values above the ceiling are clamped to it.
    pub fn with_max_depth(mut self, n: usize) -> Self {
        self.max_depth = n.min(Self::CEILING.max_depth);
        self
    }
    /// Lowers the step limit; values above the ceiling are clamped to it.
    pub fn with_max_steps(mut self, n: usize) -> Self {
        self.max_steps = n.min(Self::CEILING.max_steps);
        self
    }
    /// Lowers the per-value size limit; values above the ceiling are clamped to it.
    pub fn with_max_value_bytes(mut self, n: usize) -> Self {
        self.max_value_bytes = n.min(Self::CEILING.max_value_bytes);
        self
    }
    /// Lowers the total memory budget; values above the ceiling are clamped to it.
    pub fn with_max_total_bytes(mut self, n: usize) -> Self {
        self.max_total_bytes = n.min(Self::CEILING.max_total_bytes);
        self
    }
    /// Lowers the format width/precision limit; values above the ceiling are clamped to it.
    pub fn with_max_format_width(mut self, n: usize) -> Self {
        self.max_format_width = n.min(Self::CEILING.max_format_width);
        self
    }

    /// Field-wise minimum of two limit sets.
    pub fn min(self, other: EvalLimits) -> Self {
        EvalLimits {
            max_source_bytes: self.max_source_bytes.min(other.max_source_bytes),
            max_depth: self.max_depth.min(other.max_depth),
            max_steps: self.max_steps.min(other.max_steps),
            max_value_bytes: self.max_value_bytes.min(other.max_value_bytes),
            max_total_bytes: self.max_total_bytes.min(other.max_total_bytes),
            max_format_width: self.max_format_width.min(other.max_format_width),
        }
    }
}

impl Default for EvalLimits {
    fn default() -> Self {
        Self::CEILING
    }
}

/// Counters of one evaluation. Builtins and host callables receive it to charge the work and the
/// memory they produce before producing it.
#[derive(Debug)]
pub struct Budget {
    limits: EvalLimits,
    steps: usize,
    bytes: usize,
    depth: usize,
}

impl Budget {
    /// A fresh budget with zeroed counters.
    pub fn new(limits: EvalLimits) -> Self {
        Self {
            limits,
            steps: 0,
            bytes: 0,
            depth: 0,
        }
    }

    pub fn limits(&self) -> EvalLimits {
        self.limits
    }

    /// Steps used so far.
    pub fn steps_used(&self) -> usize {
        self.steps
    }

    /// Bytes charged so far.
    pub fn bytes_used(&self) -> usize {
        self.bytes
    }

    /// Charges one step.
    pub fn tick(&mut self) -> ExprResult<()> {
        self.steps(1)
    }

    /// Charges `n` steps.
    pub fn steps(&mut self, n: usize) -> ExprResult<()> {
        self.steps = self.steps.saturating_add(n);
        if self.steps > self.limits.max_steps {
            return Err(ExprError::limit(format!(
                "evaluation step limit of {} exceeded",
                self.limits.max_steps
            )));
        }
        Ok(())
    }

    /// Charges the steps for scanning `bytes` bytes of string data.
    pub fn scan(&mut self, bytes: usize) -> ExprResult<()> {
        self.steps(bytes / BYTES_PER_STEP + 1)
    }

    /// Checks that a value of `bytes` deep size may be produced, without charging it.
    pub fn check_size(&self, bytes: usize) -> ExprResult<()> {
        if bytes > self.limits.max_value_bytes {
            return Err(ExprError::limit(format!(
                "value of {bytes} bytes exceeds the per-value limit of {} bytes",
                self.limits.max_value_bytes
            )));
        }
        Ok(())
    }

    /// Checks that a value nested `depth` levels deep may be produced.
    pub fn check_depth(&self, depth: usize) -> ExprResult<()> {
        if depth > self.limits.max_depth {
            return Err(ExprError::limit(format!(
                "value nesting depth limit of {} exceeded",
                self.limits.max_depth
            )));
        }
        Ok(())
    }

    /// Charges `bytes` against the total budget only.
    pub fn charge(&mut self, bytes: usize) -> ExprResult<()> {
        let total = self.bytes.saturating_add(bytes);
        if total > self.limits.max_total_bytes {
            return Err(ExprError::limit(format!(
                "evaluation memory budget of {} bytes exceeded",
                self.limits.max_total_bytes
            )));
        }
        self.bytes = total;
        Ok(())
    }

    /// Checks the per-value limit for a new value of `bytes` and charges it.
    pub fn alloc(&mut self, bytes: usize) -> ExprResult<()> {
        self.check_size(bytes)?;
        self.charge(bytes)
    }

    /// Admits a value produced outside the evaluator's own accounting (a clone of a named value, a
    /// host callable's result): checks depth and size, charges its deep size and the steps to copy
    /// it.
    pub fn admit(&mut self, value: &ExprValue) -> ExprResult<()> {
        let meta = value.meta();
        self.check_depth(meta.depth)?;
        self.alloc(meta.bytes)?;
        self.steps(meta.nodes)
    }

    /// Checks depth and size of a value the evaluator built from already-charged parts.
    pub fn check_value(&self, value: &ExprValue) -> ExprResult<()> {
        let meta = value.meta();
        self.check_depth(meta.depth)?;
        self.check_size(meta.bytes)
    }

    /// Bytes a new string may still use: the smaller of the per-value limit and what is left of the
    /// total budget.
    pub fn string_capacity(&self) -> usize {
        self.limits
            .max_value_bytes
            .min(self.limits.max_total_bytes.saturating_sub(self.bytes))
    }

    /// A capped output buffer for building a string of unknown final length.
    pub(crate) fn out(&self) -> Out {
        let total_left = self.limits.max_total_bytes.saturating_sub(self.bytes);
        if total_left < self.limits.max_value_bytes {
            Out::new(
                total_left,
                format!(
                    "evaluation memory budget of {} bytes exceeded",
                    self.limits.max_total_bytes
                ),
            )
        } else {
            Out::new(
                self.limits.max_value_bytes,
                format!(
                    "string exceeds the per-value limit of {} bytes",
                    self.limits.max_value_bytes
                ),
            )
        }
    }

    /// Charges a finished [`Out`] buffer and returns its string.
    pub(crate) fn finish(&mut self, out: Out) -> ExprResult<String> {
        let s = out.into_string();
        self.scan(s.len())?;
        self.alloc(s.len())?;
        Ok(s)
    }

    /// Enters one level of evaluation recursion.
    pub(crate) fn enter(&mut self) -> ExprResult<()> {
        if self.depth >= self.limits.max_depth {
            return Err(ExprError::limit(format!(
                "nesting depth limit of {} exceeded",
                self.limits.max_depth
            )));
        }
        self.depth += 1;
        Ok(())
    }

    pub(crate) fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Checks a format width or precision against the limit.
    pub fn check_format_width(&self, n: usize) -> ExprResult<()> {
        if n > self.limits.max_format_width {
            return Err(ExprError::limit(format!(
                "format width or precision {n} exceeds the limit of {}",
                self.limits.max_format_width
            )));
        }
        Ok(())
    }
}

/// A string buffer that refuses to grow past a capacity, so formatting and `repr` never allocate
/// beyond the budget even when the final length is not known in advance.
#[derive(Debug)]
pub(crate) struct Out {
    buf: String,
    cap: usize,
    reason: String,
    /// Keep the prefix that fits when a push overflows (for truncated messages).
    truncate: bool,
}

impl Out {
    pub(crate) fn new(cap: usize, reason: String) -> Self {
        Self {
            buf: String::new(),
            cap,
            reason,
            truncate: false,
        }
    }

    /// A buffer that keeps what fits of an overflowing push before reporting the overflow.
    pub(crate) fn truncating(cap: usize) -> Self {
        Self {
            buf: String::new(),
            cap,
            reason: format!("text longer than {cap} bytes"),
            truncate: true,
        }
    }

    fn room(&self, n: usize) -> ExprResult<()> {
        if self.buf.len().saturating_add(n) > self.cap {
            return Err(ExprError::limit(self.reason.clone()));
        }
        Ok(())
    }

    pub(crate) fn push_str(&mut self, s: &str) -> ExprResult<()> {
        if let Err(e) = self.room(s.len()) {
            if self.truncate {
                let left = self.cap.saturating_sub(self.buf.len());
                let mut end = left.min(s.len());
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                self.buf.push_str(s.get(..end).unwrap_or(""));
            }
            return Err(e);
        }
        self.buf.push_str(s);
        Ok(())
    }

    pub(crate) fn push(&mut self, c: char) -> ExprResult<()> {
        self.room(c.len_utf8())?;
        self.buf.push(c);
        Ok(())
    }

    /// Pushes `c` `n` times (padding), checking the room first.
    pub(crate) fn pad(&mut self, c: char, n: usize) -> ExprResult<()> {
        self.room(c.len_utf8().saturating_mul(n))?;
        for _ in 0..n {
            self.buf.push(c);
        }
        Ok(())
    }

    pub(crate) fn into_string(self) -> String {
        self.buf
    }
}
