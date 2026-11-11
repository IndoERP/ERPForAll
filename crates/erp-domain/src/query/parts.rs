//! The parts of a validated query: group-bys, aggregates, order terms, `having` conditions and
//! the context. All have private fields, are read through accessors and are built only by the
//! decoder, so they cannot hold what validation refuses.

use std::fmt;

use crate::Value;

use super::names::{FieldName, FieldPath};
use crate::Granularity;

/// Sort direction of an order term.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OrderDir {
    /// Ascending (the absent form).
    Asc,
    /// Descending.
    Desc,
}

impl OrderDir {
    /// The name in documents: `asc` or `desc`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "asc" => Some(Self::Asc),
            "desc" => Some(Self::Desc),
            _ => None,
        }
    }
}

/// Where nulls sort in an order term; absent means the database's default for the direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NullsPos {
    /// `nulls first`.
    First,
    /// `nulls last`.
    Last,
}

impl NullsPos {
    /// The name in documents: `first` or `last`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Last => "last",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "first" => Some(Self::First),
            "last" => Some(Self::Last),
            _ => None,
        }
    }
}

/// One term of a `search_read` order: a field, a direction, where nulls go.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FieldOrder {
    pub(crate) field: FieldName,
    pub(crate) dir: OrderDir,
    pub(crate) nulls: Option<NullsPos>,
}

impl FieldOrder {
    /// The field.
    pub fn field(&self) -> &FieldName {
        &self.field
    }

    /// The direction.
    pub fn dir(&self) -> OrderDir {
        self.dir
    }

    /// Where nulls sort, when the document says.
    pub fn nulls(&self) -> Option<NullsPos> {
        self.nulls
    }
}

/// One term of a `read_group` order: the spec name of a declared group-by or aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GroupOrder {
    pub(crate) key: Box<str>,
    pub(crate) dir: OrderDir,
    pub(crate) nulls: Option<NullsPos>,
}

impl GroupOrder {
    /// The spec name (`invoice_date:month`, `amount:sum`, `__count`) of a group-by or aggregate
    /// of the same query.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The direction.
    pub fn dir(&self) -> OrderDir {
        self.dir
    }

    /// Where nulls sort, when the document says.
    pub fn nulls(&self) -> Option<NullsPos> {
        self.nulls
    }
}

/// A grouping: a field path and, for date and datetime fields, a granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GroupBy {
    pub(crate) path: FieldPath,
    pub(crate) granularity: Option<Granularity>,
}

impl GroupBy {
    /// The field path.
    pub fn path(&self) -> &FieldPath {
        &self.path
    }

    /// The granularity, when there is one.
    pub fn granularity(&self) -> Option<Granularity> {
        self.granularity
    }

    /// The spec name: `path`, or `path:granularity`. It is what `order` and the result columns
    /// call this grouping.
    ///
    /// ```
    /// use erp_domain::query::QueryAst;
    ///
    /// let ast = QueryAst::read_group("account.move")
    ///     .group_by("invoice_date", Some(erp_domain::Granularity::Month))
    ///     .count()
    ///     .limit(10)
    ///     .build(&Default::default())?;
    /// let erp_domain::query::QueryKind::ReadGroup { groupby, .. } = ast.kind() else { unreachable!() };
    /// assert_eq!(groupby[0].spec_name(), "invoice_date:month");
    /// # Ok::<(), erp_domain::query::QueryAstError>(())
    /// ```
    pub fn spec_name(&self) -> String {
        match self.granularity {
            Some(granularity) => format!("{}:{}", self.path, granularity.name()),
            None => self.path.to_string(),
        }
    }
}

/// The aggregate functions a query AST accepts. `array_agg`, `array_agg_distinct` and `recordset`
/// (unbounded output per group) and `sum_currency` are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AggregateFunc {
    /// `count` of non-null values.
    Count,
    /// `count_distinct`.
    CountDistinct,
    /// `sum`.
    Sum,
    /// `avg`.
    Avg,
    /// `min`.
    Min,
    /// `max`.
    Max,
    /// `bool_and`.
    BoolAnd,
    /// `bool_or`.
    BoolOr,
}

impl AggregateFunc {
    /// The name in documents and spec names.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::CountDistinct => "count_distinct",
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::Min => "min",
            Self::Max => "max",
            Self::BoolAnd => "bool_and",
            Self::BoolOr => "bool_or",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "count" => Self::Count,
            "count_distinct" => Self::CountDistinct,
            "sum" => Self::Sum,
            "avg" => Self::Avg,
            "min" => Self::Min,
            "max" => Self::Max,
            "bool_and" => Self::BoolAnd,
            "bool_or" => Self::BoolOr,
            _ => return None,
        })
    }
}

/// An aggregate column of a `read_group` query.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Aggregate {
    /// The number of records of the group (`__count`).
    Count,
    /// A function over a field.
    Field {
        /// The field.
        field: FieldName,
        /// The function.
        func: AggregateFunc,
    },
}

impl Aggregate {
    /// The spec name: `__count` or `field:func`.
    pub fn spec_name(&self) -> String {
        match self {
            Self::Count => "__count".to_owned(),
            Self::Field { field, func } => format!("{field}:{}", func.as_str()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HavingOp {
    /// `=`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `>`.
    Gt,
    /// `<=`.
    Le,
    /// `>=`.
    Ge,
    /// `in` a list.
    In,
    /// `not in` a list.
    NotIn,
}

impl HavingOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Ge => ">=",
            Self::In => "in",
            Self::NotIn => "not in",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "=" => Self::Eq,
            "!=" => Self::Ne,
            "<" => Self::Lt,
            ">" => Self::Gt,
            "<=" => Self::Le,
            ">=" => Self::Ge,
            "in" => Self::In,
            "not in" => Self::NotIn,
            _ => return None,
        })
    }

    /// Whether the operator takes a list.
    pub fn takes_list(self) -> bool {
        matches!(self, Self::In | Self::NotIn)
    }
}

/// A condition on an aggregate of the groups (`amount:sum > 1000`); conditions are ANDed.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct HavingCondition {
    pub(crate) aggregate: Box<str>,
    pub(crate) op: HavingOp,
    pub(crate) value: Value,
}

impl HavingCondition {
    /// The spec name of a declared aggregate of the same query.
    pub fn aggregate(&self) -> &str {
        &self.aggregate
    }

    /// The operator.
    pub fn op(&self) -> HavingOp {
        self.op
    }

    /// The value: for `in` and `not in` a [`Value::List`] of scalars (bool, integer, float or
    /// string), for every other operator one scalar. Nothing else is stored.
    pub fn value(&self) -> &Value {
        &self.value
    }
}

/// The context keys a query AST carries. v1 has one: `active_test`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct QueryContext {
    pub(crate) active_test: Option<bool>,
}

impl QueryContext {
    /// `active_test` when the document sets it; absent means the running environment's value.
    pub fn active_test(&self) -> Option<bool> {
        self.active_test
    }
}

impl fmt::Display for OrderDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
