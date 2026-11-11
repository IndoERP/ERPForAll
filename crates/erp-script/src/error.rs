//! Error types of the Luau sandbox.
//!
//! Every failure reaches the caller as a [`ScriptError`] whose variant says what went wrong (invalid
//! source, syntax, runtime, a limit, a lossy conversion, a host callback failure, a panic). Host
//! callback errors keep their original error value as [`std::error::Error::source`], so the caller can
//! downcast it back to its own type.

use std::fmt;

use thiserror::Error;

/// Result alias for scripting operations.
pub type ScriptResult<T> = Result<T, ScriptError>;

/// Boxed error returned by a host callback; kept intact as the `source` of [`ScriptError::Host`].
pub type BoxedHostError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A resource limit of the sandbox. Every limit has a default maximum that a host may lower but not
/// raise (see [`crate::ScriptLimits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitKind {
    /// Size of the script source in bytes.
    SourceBytes,
    /// Interpreter steps. One step is counted per VM interrupt: function calls, loop back-edges and
    /// pattern-matching steps, plus bulk work charged by wrapped library functions. Approximate by
    /// design: it bounds work, it is not an instruction count.
    Steps,
    /// Wall-clock time of one evaluation, in milliseconds.
    Time,
    /// Heap memory of the whole VM (standard library included), in bytes.
    Memory,
    /// Nesting depth of a value converted between Luau and the host.
    Depth,
    /// Number of values (table entries and leaves) converted between Luau and the host.
    Nodes,
    /// Host bytes built by one conversion of script values (string values, byte strings, keys).
    /// Equal to the memory limit: a string repeated many times is stored once in the VM, but each
    /// host copy is separate.
    ConvertedBytes,
}

impl LimitKind {
    /// Unit used when the limit's value is shown.
    pub fn unit(self) -> &'static str {
        match self {
            LimitKind::SourceBytes | LimitKind::Memory | LimitKind::ConvertedBytes => "bytes",
            LimitKind::Steps => "steps",
            LimitKind::Time => "ms",
            LimitKind::Depth => "levels",
            LimitKind::Nodes => "values",
        }
    }
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LimitKind::SourceBytes => "source size",
            LimitKind::Steps => "step",
            LimitKind::Time => "time",
            LimitKind::Memory => "memory",
            LimitKind::Depth => "nesting depth",
            LimitKind::Nodes => "value count",
            LimitKind::ConvertedBytes => "converted size",
        })
    }
}

/// Why a script source was rejected before it reached the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceIssue {
    /// A NUL byte.
    Nul,
    /// Bytes that are not valid UTF-8.
    InvalidUtf8,
    /// A C0 control byte other than tab, line feed and carriage return. Luau bytecode starts with
    /// such bytes, so rejecting them keeps bytecode away from the VM even before the text-only load.
    ControlCharacter(u8),
}

impl fmt::Display for SourceIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceIssue::Nul => f.write_str("NUL byte (Luau bytecode is not accepted)"),
            SourceIssue::InvalidUtf8 => f.write_str("invalid UTF-8"),
            SourceIssue::ControlCharacter(b) => {
                write!(f, "control byte 0x{b:02x} (Luau bytecode is not accepted)")
            }
        }
    }
}

/// Errors of the Luau sandbox.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScriptError {
    /// The host cancelled the evaluation. Sticky like a resource limit.
    #[error("script evaluation cancelled by host")]
    Cancelled,
    /// The source was rejected before loading (bytecode, NUL, invalid UTF-8, control bytes).
    #[error("invalid script source at byte {offset}: {issue}")]
    InvalidSource {
        /// What is wrong.
        issue: SourceIssue,
        /// Byte offset of the first offending byte.
        offset: usize,
    },

    /// The source does not compile.
    #[error("syntax error{}: {message}", line_suffix(*line))]
    Syntax {
        /// Line reported by the Luau compiler, `None` when it reports no position.
        line: Option<u32>,
        /// Compiler message without the chunk-name prefix.
        message: String,
    },

    /// The script raised an error (`error(...)`, a failed operation, a bad library call).
    #[error("runtime error: {message}")]
    Runtime {
        /// Error message, including the `script:<line>:` position Luau adds.
        message: String,
        /// Luau stack traceback when one was produced.
        traceback: Option<String>,
    },

    /// A resource limit was reached. Raised out of band: a script cannot catch it with `pcall` and
    /// cannot produce it with `error(...)`.
    #[error("{limit} limit of {max} {} exceeded", limit.unit())]
    LimitExceeded {
        /// Which limit.
        limit: LimitKind,
        /// The configured maximum.
        max: u64,
    },

    /// A limit given by the host is zero or above the default maximum.
    #[error("invalid {limit} limit {value} {}: must be between 1 and {max}", limit.unit())]
    InvalidLimit {
        /// Which limit.
        limit: LimitKind,
        /// The rejected value (milliseconds for time, saturated at `u64::MAX`).
        value: u64,
        /// The default maximum.
        max: u64,
    },

    /// A value cannot be converted between Luau and the host without losing information.
    #[error("type error at {path}: {message}")]
    Type {
        /// Where the value sits (`$` is the root, `$.items[2]` a nested entry, `env.search arg 1` an
        /// argument of a host callback).
        path: String,
        /// What is wrong.
        message: String,
    },

    /// A name given to [`crate::ScriptContext`] is invalid, reserved or already used.
    #[error("invalid script name '{name}': {reason}")]
    InvalidName {
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// A host callback returned an error. The script only saw a fixed message; the caller gets the
    /// original error as `source`.
    #[error("host function '{function}' failed")]
    Host {
        /// Name the callback was registered under (`env.search`).
        function: String,
        /// The error the callback returned, unchanged.
        #[source]
        source: BoxedHostError,
    },

    /// Rust code panicked during the evaluation (a host callback or the engine).
    #[error("panic in {context}: {message}")]
    Panic {
        /// Where the panic happened (`host function 'env.search'`, `evaluation`).
        context: String,
        /// Panic message, when it was a string.
        message: String,
    },

    /// The Luau engine failed in a way unrelated to the script (VM creation, environment setup).
    #[error("Luau engine error: {message}")]
    Engine {
        /// Engine message.
        message: String,
    },
}

fn line_suffix(line: Option<u32>) -> String {
    match line {
        Some(l) => format!(" on line {l}"),
        None => String::new(),
    }
}

impl ScriptError {
    pub(crate) fn type_error(path: impl Into<String>, message: impl Into<String>) -> Self {
        ScriptError::Type {
            path: path.into(),
            message: message.into(),
        }
    }

    pub(crate) fn engine(message: impl fmt::Display) -> Self {
        ScriptError::Engine {
            message: message.to_string(),
        }
    }
}

/// Error a host callback returns. Any `std::error::Error + Send + Sync` converts into it with `?`;
/// the original value comes back to the caller as the `source` of [`ScriptError::Host`].
pub struct HostError(pub(crate) BoxedHostError);

impl HostError {
    /// Wraps an error value.
    pub fn new(error: impl Into<BoxedHostError>) -> Self {
        HostError(error.into())
    }

    /// The wrapped error.
    pub fn get_ref(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        &*self.0
    }
}

impl<E: Into<BoxedHostError>> From<E> for HostError {
    fn from(error: E) -> Self {
        HostError(error.into())
    }
}

impl fmt::Debug for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("HostError").field(&self.0).finish()
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
