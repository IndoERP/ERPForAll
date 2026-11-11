//! Condition operators (SPEC-DOM §4.2).

use std::fmt;
use std::str::FromStr;

/// A condition operator.
///
/// Any other operator string is
/// rejected when a condition is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Operator {
    /// `in`: the field value is one of a collection; `False` in the collection means "not set".
    In,
    /// `not in`.
    NotIn,
    /// `=`: rewritten to `in` by BASIC optimization.
    Eq,
    /// `!=`: rewritten to `not in` by BASIC optimization.
    Ne,
    /// `=?`: `TRUE` when the value is falsy, otherwise `=`.
    EqIfSet,
    /// `<`.
    Lt,
    /// `>`.
    Gt,
    /// `<=`.
    Le,
    /// `>=`.
    Ge,
    /// `like`: case-sensitive pattern match with `%` added around the value.
    Like,
    /// `not like`.
    NotLike,
    /// `ilike`: case- and accent-insensitive pattern match with `%` added around the value.
    ILike,
    /// `not ilike`.
    NotILike,
    /// `=like`: case-sensitive pattern match on the value as given.
    EqLike,
    /// `not =like`.
    NotEqLike,
    /// `=ilike`: case-insensitive pattern match on the value as given.
    EqILike,
    /// `not =ilike`.
    NotEqILike,
    /// `any`: some related record matches a sub-domain (comodel access rules apply).
    Any,
    /// `not any`.
    NotAny,
    /// `any!`: like `any`, bypassing the comodel's access rules. Internal: rejected in list syntax
    /// unless parsed with [`crate::Domain::from_list_internal`].
    AnyBypass,
    /// `not any!`. Internal, like [`Operator::AnyBypass`].
    NotAnyBypass,
    /// `child_of`: hierarchy expansion, resolved at FULL optimization.
    ChildOf,
    /// `parent_of`: hierarchy expansion, resolved at FULL optimization.
    ParentOf,
    /// `access`: record-level permission test (`read`, `write`, `create`, `unlink`), resolved at
    /// DYNAMIC_VALUES/FULL optimization.
    Access,
    /// `overlaps` (ERPForAll, design §5.2): a `date_range`/`datetime_range` field shares a
    /// point with the range `[lower, upper]` (`False` for an unbounded end), taken with the
    /// field's bounds. SQL `&&`.
    Overlaps,
    /// `contains` (ERPForAll, design §5.2): a `date_range`/`datetime_range` field contains
    /// the date or datetime given. SQL `@>`.
    Contains,
}

impl Operator {
    /// Every operator, in a fixed order.
    pub const ALL: [Operator; 26] = [
        Self::In,
        Self::NotIn,
        Self::Eq,
        Self::Ne,
        Self::EqIfSet,
        Self::Lt,
        Self::Gt,
        Self::Le,
        Self::Ge,
        Self::Like,
        Self::NotLike,
        Self::ILike,
        Self::NotILike,
        Self::EqLike,
        Self::NotEqLike,
        Self::EqILike,
        Self::NotEqILike,
        Self::Any,
        Self::NotAny,
        Self::AnyBypass,
        Self::NotAnyBypass,
        Self::ChildOf,
        Self::ParentOf,
        Self::Access,
        Self::Overlaps,
        Self::Contains,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::NotIn => "not in",
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::EqIfSet => "=?",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Ge => ">=",
            Self::Like => "like",
            Self::NotLike => "not like",
            Self::ILike => "ilike",
            Self::NotILike => "not ilike",
            Self::EqLike => "=like",
            Self::NotEqLike => "not =like",
            Self::EqILike => "=ilike",
            Self::NotEqILike => "not =ilike",
            Self::Any => "any",
            Self::NotAny => "not any",
            Self::AnyBypass => "any!",
            Self::NotAnyBypass => "not any!",
            Self::ChildOf => "child_of",
            Self::ParentOf => "parent_of",
            Self::Access => "access",
            Self::Overlaps => "overlaps",
            Self::Contains => "contains",
        }
    }

    pub fn parse(s: &str) -> Option<Operator> {
        Self::ALL.into_iter().find(|op| op.as_str() == s)
    }

    /// Whether SQL generation accepts this operator once a domain is fully optimized (the 18
    /// standard operators of the glossary, and ERPForAll's range operators `overlaps` and
    /// `contains`).
    pub const fn is_standard(self) -> bool {
        !matches!(
            self,
            Self::Eq | Self::Ne | Self::EqIfSet | Self::ChildOf | Self::ParentOf | Self::Access
        )
    }

    /// Whether the operator is internal (`any!`, `not any!`).
    pub const fn is_internal(self) -> bool {
        matches!(self, Self::AnyBypass | Self::NotAnyBypass)
    }

    /// Whether the operator has a negative meaning (`not in`, `!=`, `not like`, `not any`, …).
    pub const fn is_negative(self) -> bool {
        matches!(
            self,
            Self::NotIn
                | Self::Ne
                | Self::NotLike
                | Self::NotILike
                | Self::NotEqLike
                | Self::NotEqILike
                | Self::NotAny
                | Self::NotAnyBypass
        )
    }

    /// The positive form of a negative operator; other operators are returned unchanged.
    pub const fn positive(self) -> Operator {
        match self {
            Self::NotIn => Self::In,
            Self::Ne => Self::Eq,
            Self::NotLike => Self::Like,
            Self::NotILike => Self::ILike,
            Self::NotEqLike => Self::EqLike,
            Self::NotEqILike => Self::EqILike,
            Self::NotAny => Self::Any,
            Self::NotAnyBypass => Self::AnyBypass,
            other => other,
        }
    }

    /// The operator with the opposite meaning, used when negating a condition at construction
    /// (`=`↔`!=`, `in`↔`not in`, the `like` family, `any`↔`not any`, `any!`↔`not any!`).
    /// Inequalities have no inverse here: their negation depends on the field type and happens
    /// during optimization.
    pub const fn inverse(self) -> Option<Operator> {
        Some(match self {
            Self::In => Self::NotIn,
            Self::NotIn => Self::In,
            Self::Eq => Self::Ne,
            Self::Ne => Self::Eq,
            Self::Like => Self::NotLike,
            Self::NotLike => Self::Like,
            Self::ILike => Self::NotILike,
            Self::NotILike => Self::ILike,
            Self::EqLike => Self::NotEqLike,
            Self::NotEqLike => Self::EqLike,
            Self::EqILike => Self::NotEqILike,
            Self::NotEqILike => Self::EqILike,
            Self::Any => Self::NotAny,
            Self::NotAny => Self::Any,
            Self::AnyBypass => Self::NotAnyBypass,
            Self::NotAnyBypass => Self::AnyBypass,
            _ => return None,
        })
    }

    /// The complementary inequality (`<`↔`>=`, `>`↔`<=`), for inequalities only.
    pub const fn inverse_inequality(self) -> Option<Operator> {
        Some(match self {
            Self::Lt => Self::Ge,
            Self::Gt => Self::Le,
            Self::Ge => Self::Lt,
            Self::Le => Self::Gt,
            _ => return None,
        })
    }

    /// Whether the operator is an inequality (`<`, `>`, `<=`, `>=`).
    pub const fn is_inequality(self) -> bool {
        matches!(self, Self::Lt | Self::Gt | Self::Le | Self::Ge)
    }

    /// Whether the operator is one of the eight `like` variants.
    pub const fn is_like(self) -> bool {
        matches!(
            self,
            Self::Like
                | Self::NotLike
                | Self::ILike
                | Self::NotILike
                | Self::EqLike
                | Self::NotEqLike
                | Self::EqILike
                | Self::NotEqILike
        )
    }

    /// Whether the operator is a range operator (`overlaps`, `contains`).
    pub const fn is_range(self) -> bool {
        matches!(self, Self::Overlaps | Self::Contains)
    }

    /// Whether the operator is one of `any`, `not any`, `any!`, `not any!`.
    pub const fn is_any(self) -> bool {
        matches!(
            self,
            Self::Any | Self::NotAny | Self::AnyBypass | Self::NotAnyBypass
        )
    }

    /// For the `any` family, the bypass form (`any` → `any!`, `not any` → `not any!`).
    pub const fn bypass(self) -> Operator {
        match self {
            Self::Any => Self::AnyBypass,
            Self::NotAny => Self::NotAnyBypass,
            other => other,
        }
    }

    /// Whether the spelling contains `=` among the `like` family (`=like`, `=ilike` and their
    /// negations): the pattern is used as given, without added wildcards.
    pub(crate) const fn is_exact_like(self) -> bool {
        matches!(
            self,
            Self::EqLike | Self::NotEqLike | Self::EqILike | Self::NotEqILike
        )
    }
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown operator with [`str::parse`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown domain operator {0:?}")]
pub struct UnknownOperator(pub String);

impl FromStr for Operator {
    type Err = UnknownOperator;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Operator::parse(s).ok_or_else(|| UnknownOperator(s.to_owned()))
    }
}
