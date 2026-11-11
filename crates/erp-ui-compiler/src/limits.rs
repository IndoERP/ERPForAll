//! Compiler limits (design note §9; standards §4): on by default, the defaults are the
//! maxima, a host can only lower them, and zero never means unlimited. Each default is written
//! once, next to its kind ([`LimitKind::default_value`]).

use std::fmt;

/// One compiler limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LimitKind {
    /// Bytes of one source file (also of a combined template).
    FileBytes,
    /// XML nodes of one source file (roxmltree's node limit).
    FileNodes,
    /// Element nesting depth, checked before roxmltree recurses.
    Depth,
    /// Attributes on one element.
    Attributes,
    /// Templates and extensions in one set (counted per file before its tree is built).
    Templates,
    /// Bytes of one expression or format string (also a dynamic `t-call` or slot name).
    ExpressionBytes,
    /// Bracket nesting of one expression; the syntax-tree walk allows four times as many levels
    /// (one per expression node).
    ExpressionNesting,
    /// Bytes of one generated module, checked while it is generated.
    ModuleBytes,
    /// Bytes of all generated modules of a set, checked after each module.
    OutputBytes,
    /// Files in one set, checked before any is parsed.
    Files,
    /// Bytes of all files of one set, checked before any is parsed.
    InputBytes,
    /// Template locals (`t-foreach` variables, `t-set` names, slot scopes) declared in one
    /// template.
    Locals,
    /// Nested `t-call` recursion depth (default: 64).
    CallDepth,
    /// Reactive effects declared in a single template (default: 512).
    Effects,
}

/// Number of limits.
const COUNT: usize = 14;

impl LimitKind {
    /// Every limit, in declaration order.
    pub const ALL: &'static [LimitKind] = &ALL_KINDS;

    /// The default, which is also the maximum (a host can only lower it).
    #[must_use]
    pub const fn default_value(self) -> usize {
        match self {
            LimitKind::FileBytes => 4 * 1024 * 1024,
            LimitKind::FileNodes => 200_000,
            LimitKind::Depth => 64,
            LimitKind::Attributes => 256,
            LimitKind::Templates => 20_000,
            LimitKind::ExpressionBytes => 4096,
            LimitKind::ExpressionNesting => 64,
            LimitKind::ModuleBytes => 2 * 1024 * 1024,
            LimitKind::OutputBytes => 128 * 1024 * 1024,
            LimitKind::Files => 20_000,
            LimitKind::InputBytes => 128 * 1024 * 1024,
            LimitKind::Locals => 4096,
            LimitKind::CallDepth => 64,
            LimitKind::Effects => 512,
        }
    }

    /// The limit's name in messages, reports and the request framing.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            LimitKind::FileBytes => "file_bytes",
            LimitKind::FileNodes => "file_nodes",
            LimitKind::Depth => "depth",
            LimitKind::Attributes => "attributes",
            LimitKind::Templates => "templates",
            LimitKind::ExpressionBytes => "expression_bytes",
            LimitKind::ExpressionNesting => "expression_nesting",
            LimitKind::ModuleBytes => "module_bytes",
            LimitKind::OutputBytes => "output_bytes",
            LimitKind::Files => "files",
            LimitKind::InputBytes => "input_bytes",
            LimitKind::Locals => "locals",
            LimitKind::CallDepth => "call_depth",
            LimitKind::Effects => "effects",
        }
    }

    /// The limit called `name` ([`LimitKind::name`]), if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<LimitKind> {
        Self::ALL.iter().copied().find(|k| k.name() == name)
    }

    const fn index(self) -> usize {
        match self {
            LimitKind::FileBytes => 0,
            LimitKind::FileNodes => 1,
            LimitKind::Depth => 2,
            LimitKind::Attributes => 3,
            LimitKind::Templates => 4,
            LimitKind::ExpressionBytes => 5,
            LimitKind::ExpressionNesting => 6,
            LimitKind::ModuleBytes => 7,
            LimitKind::OutputBytes => 8,
            LimitKind::Files => 9,
            LimitKind::InputBytes => 10,
            LimitKind::Locals => 11,
            LimitKind::CallDepth => 12,
            LimitKind::Effects => 13,
        }
    }
}

const ALL_KINDS: [LimitKind; COUNT] = [
    LimitKind::FileBytes,
    LimitKind::FileNodes,
    LimitKind::Depth,
    LimitKind::Attributes,
    LimitKind::Templates,
    LimitKind::ExpressionBytes,
    LimitKind::ExpressionNesting,
    LimitKind::ModuleBytes,
    LimitKind::OutputBytes,
    LimitKind::Files,
    LimitKind::InputBytes,
    LimitKind::Locals,
    LimitKind::CallDepth,
    LimitKind::Effects,
];

// `index` and `ALL_KINDS` agree, so `CompilerLimits` stores each limit in its own slot.
const _: () = {
    let mut i = 0;
    while i < COUNT {
        assert!(ALL_KINDS[i].index() == i);
        i += 1;
    }
};

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A limit a template or set went over: carried by every [`crate::DiagnosticKind::Limit`]
/// diagnostic ([`crate::Diagnostic::limit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LimitHit {
    kind: LimitKind,
    limit: usize,
    actual: usize,
}

impl LimitHit {
    pub(crate) fn new(kind: LimitKind, limit: usize, actual: usize) -> Self {
        Self {
            kind,
            limit,
            actual,
        }
    }

    /// The limit.
    #[must_use]
    pub fn kind(&self) -> LimitKind {
        self.kind
    }

    /// Its value in this compile.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The value found (at least `limit + 1`; for a count found by a parser that stops at the
    /// limit, exactly `limit + 1`).
    #[must_use]
    pub fn actual(&self) -> usize {
        self.actual
    }

    /// The diagnostic message.
    pub(crate) fn message(&self) -> String {
        format!(
            "{} limit exceeded: maximum {}, got {}",
            self.kind, self.limit, self.actual
        )
    }
}

/// A limit could not be set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LimitError {
    /// Zero, or above the default (limits can only be lowered).
    #[error(
        "limit {limit} cannot be set to {value}: allowed 1..={max} (limits can only be lowered)"
    )]
    OutOfRange {
        /// The limit.
        limit: LimitKind,
        /// The refused value.
        value: usize,
        /// Its default, which is also its maximum.
        max: usize,
    },
}

/// The compiler's limits.
///
/// ```
/// use erp_ui_compiler::{CompilerLimits, LimitKind};
///
/// let limits = CompilerLimits::default().lowered(LimitKind::Depth, 32)?;
/// assert_eq!(limits.get(LimitKind::Depth), 32);
/// assert!(CompilerLimits::default().lowered(LimitKind::Depth, 0).is_err());
/// assert!(CompilerLimits::default().lowered(LimitKind::Depth, 65).is_err());
/// # Ok::<(), erp_ui_compiler::LimitError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompilerLimits {
    values: [usize; COUNT],
}

impl CompilerLimits {
    /// The defaults ([`LimitKind::default_value`]), which are also the maxima.
    pub const DEFAULT: CompilerLimits = {
        let mut values = [0; COUNT];
        let mut i = 0;
        while i < COUNT {
            values[i] = ALL_KINDS[i].default_value();
            i += 1;
        }
        CompilerLimits { values }
    };

    /// The current value of one limit.
    #[must_use]
    pub const fn get(&self, kind: LimitKind) -> usize {
        self.values[kind.index()]
    }

    /// A copy with one limit lowered to `value`.
    ///
    /// # Errors
    /// [`LimitError::OutOfRange`] when `value` is zero or above the default.
    pub fn lowered(mut self, kind: LimitKind, value: usize) -> Result<Self, LimitError> {
        let max = kind.default_value();
        if value == 0 || value > max {
            return Err(LimitError::OutOfRange {
                limit: kind,
                value,
                max,
            });
        }
        self.values[kind.index()] = value;
        Ok(self)
    }
}

impl Default for CompilerLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_limit_lowers_and_refuses_zero_and_raising() {
        assert_eq!(LimitKind::ALL.len(), COUNT);
        for &kind in LimitKind::ALL {
            assert_eq!(LimitKind::from_name(kind.name()), Some(kind));
            let max = CompilerLimits::DEFAULT.get(kind);
            assert_eq!(max, kind.default_value());
            assert!(max > 0);
            assert_eq!(
                CompilerLimits::default()
                    .lowered(kind, 1)
                    .unwrap()
                    .get(kind),
                1
            );
            assert_eq!(
                CompilerLimits::default().lowered(kind, 0).unwrap_err(),
                LimitError::OutOfRange {
                    limit: kind,
                    value: 0,
                    max
                }
            );
            let over = max + 1;
            assert_eq!(
                CompilerLimits::default()
                    .lowered(kind, over)
                    .unwrap_err()
                    .to_string(),
                format!(
                    "limit {} cannot be set to {over}: allowed 1..={max} (limits can only be lowered)",
                    kind.name()
                )
            );
        }
    }
}
