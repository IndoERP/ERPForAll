//! RPC exposure metadata (3 and 4, §5, §6).
//!
//! Pure values: the effect class of a method, whether it takes records or is a model method, and
//! the parameter table its binder accepts. `erp-orm` collects them from `#[rpc]` declarations into
//! per-registry exposure tables; `erp-rpc` reads those tables at dispatch.

use std::fmt;

/// What a method may change, ordered by risk: `Read < Write < Destructive < External`.
/// A chain's effect is the maximum declared along it.
///
/// ```
/// use erp_registry::Effect;
/// assert!(Effect::Read < Effect::Write && Effect::Destructive < Effect::External);
/// assert_eq!(Effect::parse("destructive"), Some(Effect::Destructive));
/// assert_eq!(Effect::parse("delete"), None);
/// assert!(Effect::Read.read_only_hint());
/// assert!(Effect::External.open_world_hint());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effect {
    /// Changes nothing anywhere; runs in a read-only transaction.
    Read,
    /// Changes this database, reversible by another write.
    Write,
    /// Loses data or is hard to reverse (`unlink`, posting that locks a period, purge).
    Destructive,
    /// Acts outside the database (mail, payment, HTTP), after commit through the outbox.
    External,
}

// Deliberately not `#[non_exhaustive]`: a new effect changes the read-only gate, the approval
// default and the MCP hints, so every `match` on it must be revisited (compile error, not a
// silent wildcard arm).

impl Effect {
    /// Every effect, in order.
    pub const ALL: [Effect; 4] = [
        Effect::Read,
        Effect::Write,
        Effect::Destructive,
        Effect::External,
    ];

    /// The name used in `#[rpc(effect = …)]`, error texts and WASM `describe` data.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
            Self::External => "external",
        }
    }

    /// Parse an effect name; `None` for anything else (callers report it as a build error).
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == name)
    }

    /// MCP `readOnlyHint` (design note §5.1).
    pub const fn read_only_hint(self) -> bool {
        matches!(self, Self::Read)
    }

    /// MCP `destructiveHint` (design note §5.1; meaningful when not read-only).
    pub const fn destructive_hint(self) -> bool {
        matches!(self, Self::Destructive | Self::External)
    }

    /// MCP `openWorldHint` (design note §5.1).
    pub const fn open_world_hint(self) -> bool {
        matches!(self, Self::External)
    }
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Receiver {
    /// The call's `ids` become the recordset (`[]` is an empty recordset).
    Records,
    /// A model method (`#[rpc(…, model)]`): a non-empty `ids` is refused (422
    /// "cannot call <model>.<method> with ids").
    Model,
}

/// The JSON shape a parameter binder accepts, as a closed type (
/// R1, R2). A shape is **closed** when no [`ArgShape::Open`]
/// appears in it; only methods whose every parameter is closed get a tool schema. A shape never
/// advertises a payload the binder refuses (it may be stricter than the binder), with one
/// exception: [`ArgShape::Domain`] accepts any scalar in each leaf position and does not check
/// connector arity; the binder's domain parser refuses those payloads with a 422.
///
/// ```
/// use erp_registry::ArgShape;
/// const NAMES: ArgShape = ArgShape::Array(&ArgShape::String);
/// assert!(NAMES.is_closed());
/// assert!(!ArgShape::OrNull(&ArgShape::Open("any value")).is_closed());
/// assert_eq!(ArgShape::Array(&ArgShape::Open("an object")).open_reason(), Some("an object"));
/// ```
// Deliberately not `#[non_exhaustive]` (like `Effect`): a new shape must be handled by the schema
// export and the validator, so every `match` on it breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgShape {
    /// `true` or `false`.
    Boolean,
    /// A JSON integer that fits `i64`, at least `min` when given.
    Integer {
        /// Smallest accepted value.
        min: Option<i64>,
    },
    /// A JSON number (integer or fraction).
    Number,
    /// A string.
    String,
    /// One of these strings.
    Enum(&'static [&'static str]),
    /// An array whose items have this shape.
    Array(&'static ArgShape),
    /// This shape or `null`.
    OrNull(&'static ArgShape),
    /// A domain: an array of `"&"`, `"|"`, `"!"` and `[field, operator, value]` leaves.
    Domain,
    /// No closed schema; the text says why (for the export error).
    Open(&'static str),
}

impl ArgShape {
    /// Whether no [`ArgShape::Open`] appears in the shape.
    pub const fn is_closed(&self) -> bool {
        self.open_reason().is_none()
    }

    /// The reason of the first [`ArgShape::Open`] in the shape, if any.
    pub const fn open_reason(&self) -> Option<&'static str> {
        match self {
            Self::Open(reason) => Some(*reason),
            Self::Array(inner) | Self::OrNull(inner) => inner.open_reason(),
            Self::Boolean
            | Self::Integer { .. }
            | Self::Number
            | Self::String
            | Self::Enum(_)
            | Self::Domain => None,
        }
    }
}

/// One keyword parameter an exposed method accepts (design note §6.3). Names are the Rust
/// parameter names of the declaring implementation; `expected` names the JSON shape the binder
/// accepts, for the 422 "expected <type>" message.
///
/// ```
/// use erp_registry::ParamSpec;
/// const DOMAIN: ParamSpec = ParamSpec::new("domain", true, "a domain (list)");
/// assert_eq!((DOMAIN.name(), DOMAIN.required()), ("domain", true));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamSpec {
    name: &'static str,
    required: bool,
    expected: &'static str,
    shape: ArgShape,
}

impl ParamSpec {
    /// A parameter: `required` is false for optional (`Option<T>`) and defaulted parameters.
    /// Its shape is [`ArgShape::Open`]`("no shape declared")`, so a table built with it has no
    /// closed schema and is never offered to the assistant (use [`ParamSpec::with_shape`]).
    pub const fn new(name: &'static str, required: bool, expected: &'static str) -> Self {
        Self::with_shape(
            name,
            required,
            expected,
            ArgShape::Open("no shape declared"),
        )
    }

    /// A parameter with the typed shape its binder accepts (`#[rpc]` emits it from
    /// `FromRpc::SHAPE`).
    ///
    /// ```
    /// use erp_registry::{ArgShape, ParamSpec};
    /// const LIMIT: ParamSpec =
    ///     ParamSpec::with_shape("limit", false, "an integer", ArgShape::Integer { min: Some(0) });
    /// assert!(LIMIT.shape().is_closed());
    /// assert!(!ParamSpec::new("limit", false, "an integer").shape().is_closed());
    /// ```
    pub const fn with_shape(
        name: &'static str,
        required: bool,
        expected: &'static str,
        shape: ArgShape,
    ) -> Self {
        Self {
            name,
            required,
            expected,
            shape,
        }
    }

    /// The JSON shape the binder accepts, for the assistant's tool schemas.
    pub const fn shape(&self) -> ArgShape {
        self.shape
    }

    /// The keyword name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Whether a call must pass it.
    pub const fn required(&self) -> bool {
        self.required
    }

    /// The accepted JSON shape, as named in binding errors.
    pub const fn expected(&self) -> &'static str {
        self.expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effects_round_trip_and_are_ordered() {
        for (i, effect) in Effect::ALL.into_iter().enumerate() {
            assert_eq!(Effect::parse(effect.as_str()), Some(effect));
            assert_eq!(effect.to_string(), effect.as_str());
            for later in &Effect::ALL[i + 1..] {
                assert!(effect < *later, "{effect} < {later}");
            }
        }
        for bad in ["", "READ", "read ", "none", "readonly"] {
            assert_eq!(Effect::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn mcp_hints_follow_the_class() {
        let hints: Vec<(bool, bool, bool)> = Effect::ALL
            .into_iter()
            .map(|e| {
                (
                    e.read_only_hint(),
                    e.destructive_hint(),
                    e.open_world_hint(),
                )
            })
            .collect();
        assert_eq!(
            hints,
            vec![
                (true, false, false),
                (false, false, false),
                (false, true, false),
                (false, true, true)
            ]
        );
    }
}
