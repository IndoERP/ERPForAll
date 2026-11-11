//! The immutable domain AST (SPEC-DOM §4.1, §4.3).

use std::any::Any;
use std::collections::VecDeque;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{BitAnd, BitOr, Not};
use std::sync::Arc;

use crate::error::{ConditionError, DomainError, ErrorKind, ErrorReason};
use crate::operator::Operator;
use crate::param::PrincipalParam;
use crate::value::{
    ReprWriter, Value, ValueKind, hash_of, py_str_repr, write_items, write_str_repr,
};

/// Maximum nesting depth accepted when building a domain from list syntax or JSON (and of JSON
/// values, see [`Value::from_json`]).
///
/// Prefix lists are flat but can describe arbitrarily deep trees (`['&', A, '|', B, '&', …]`);
/// the limit keeps untrusted input from exhausting the stack in the recursive algorithms. Real
/// domains stay far below it. Domains combined in Rust code with `&`, `|` and `!` are trusted and
/// not checked.
pub const MAX_DEPTH: usize = 256;

/// Maximum number of list-syntax items (operators, conditions and nested domain items, counted
/// across `any` sub-domains parsed internally) accepted from external input. Bounds parsing and
/// optimization work; values such as a long `id in [...]` list count as one item.
pub const MAX_ITEMS: usize = 250_000;

/// How far a domain has been optimized (SPEC-DOM §4.4). Levels are ordered; each one includes the
/// rewrites of the previous ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OptimizationLevel {
    /// Not optimized.
    None,
    /// Depends only on field definitions; the result can be reused across transactions.
    Basic,
    /// Adds values that depend on "now" and on the environment (relative dates, `access`).
    DynamicValues,
    /// Valid only at this point in the transaction; may run queries. Only `FULL` domains compile
    /// to SQL.
    Full,
}

impl OptimizationLevel {
    /// The level after this one (`Full` stays `Full`).
    pub(crate) fn next(self) -> OptimizationLevel {
        match self {
            Self::None => Self::Basic,
            Self::Basic => Self::DynamicValues,
            Self::DynamicValues | Self::Full => Self::Full,
        }
    }
}

/// An immutable domain: a boolean combination of conditions on fields.
///
/// Cloning is cheap (shared nodes). Equality and hashing are structural (operator, field, child
/// order, values compared strictly by variant with sets compared without order), so equal domains
/// can key a memoization cache; hashes are computed once when a node is
/// built, so hashing a domain is O(1). The optimization level and error provenance are
/// annotations of the handle and do not take part in equality.
///
/// To combine many domains in Rust code prefer [`Domain::and`]/[`Domain::or`] over repeated
/// `a & b` (the operators reuse the child list when they own the only reference, but the
/// iterator forms never copy).
#[derive(Clone)]
pub struct Domain(Repr);

#[derive(Clone)]
enum Repr {
    Const(bool),
    Node(Node),
}

/// A handle on a shared node. Annotations live in the handle, not in the shared node: marking a
/// domain as optimized makes a new handle (cheap) and the AST stays free of interior mutability,
/// so domains are sound hash-map keys.
#[derive(Clone)]
struct Node {
    data: Arc<NodeData>,
    level: OptimizationLevel,
    /// The optimization context the level was reached in (see `ModelContext::stamp`); `0` means
    /// valid in any context (fresh and custom nodes).
    stamp: u64,
    /// Error provenance is handle metadata, excluded from predicate equality and hashing.
    origin: Origin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Origin {
    Native,
    External,
    Programming(Arc<str>),
}

struct NodeData {
    kind: NodeKind,
    depth: usize,
    hash: u64,
    /// For AND/OR: the order-dependent fold of the child hashes, so that appending children to
    /// a node we own updates the hash without re-reading every child.
    fold: u64,
}

const FOLD_INIT: u64 = 0xcbf2_9ce4_8422_2325;

fn fold_child(fold: u64, child_hash: u64) -> u64 {
    fold.wrapping_mul(0x0000_0100_0000_01b3)
        .rotate_left(17)
        .wrapping_add(child_hash)
}

fn nary_hash(kind: NaryKind, len: usize, fold: u64) -> u64 {
    let tag: u8 = match kind {
        NaryKind::And => 2,
        NaryKind::Or => 3,
    };
    hash_of(&(tag, len, fold))
}

enum NodeKind {
    Not(Domain),
    And(Vec<Domain>),
    Or(Vec<Domain>),
    Condition(Condition),
    Custom(Arc<dyn CustomDomain>),
}

/// The two n-ary connectives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NaryKind {
    And,
    Or,
}

impl NaryKind {
    /// The neutral constant: `TRUE` for AND, `FALSE` for OR.
    pub(crate) fn zero(self) -> bool {
        matches!(self, NaryKind::And)
    }

    /// The other connective.
    pub(crate) fn inverse(self) -> NaryKind {
        match self {
            NaryKind::And => NaryKind::Or,
            NaryKind::Or => NaryKind::And,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            NaryKind::And => "&",
            NaryKind::Or => "|",
        }
    }
}

impl NodeKind {
    fn nary_children(&self, kind: NaryKind) -> Option<&[Domain]> {
        match (self, kind) {
            (NodeKind::And(children), NaryKind::And) | (NodeKind::Or(children), NaryKind::Or) => {
                Some(children)
            }
            _ => None,
        }
    }
}

/// A read-only view of a domain node, for code that walks the tree (SQL generation, in-memory
/// evaluation).
#[derive(Clone, Copy, Debug)]
pub enum DomainKind<'a> {
    /// Matches every record.
    True,
    /// Matches no record.
    False,
    /// Negation of the child.
    Not(&'a Domain),
    /// Conjunction of at least two children.
    And(&'a [Domain]),
    /// Disjunction of at least two children.
    Or(&'a [Domain]),
    /// A condition on a field.
    Condition(&'a Condition),
    /// A custom node supplied by another layer.
    Custom(&'a Arc<dyn CustomDomain>),
}

/// This crate only carries custom nodes: they are left unchanged by BASIC optimization, render as
/// themselves in list syntax and cannot be serialized to JSON.
pub trait CustomDomain: fmt::Debug + Send + Sync + 'static {
    /// Such nodes start at the `DynamicValues` level; the others at `Full`.
    fn has_optimizer(&self) -> bool {
        false
    }

    /// Access to the concrete type, for the layer that created the node.
    fn as_any(&self) -> &dyn Any;
}

/// A condition `(field_expr, operator, value)`.
///
/// `field_expr` is a field name, optionally followed by `.path` (a relational traversal, turned
/// into `any` by optimization) or `.property` (a date part, a properties key …).
#[derive(Clone)]
pub struct Condition {
    field_expr: Arc<str>,
    operator: Operator,
    value: Value,
}

impl Condition {
    /// The field expression.
    pub fn field_expr(&self) -> &str {
        &self.field_expr
    }

    /// The operator.
    pub fn operator(&self) -> Operator {
        self.operator
    }

    /// The value.
    pub fn value(&self) -> &Value {
        &self.value
    }

    pub(crate) fn field_expr_arc(&self) -> &Arc<str> {
        &self.field_expr
    }

    /// Python repr `('field', 'op', value)`.
    pub(crate) fn py_repr(&self) -> String {
        let mut out = ReprWriter::unlimited();
        write_condition_repr(
            &mut out,
            &self.field_expr,
            &Value::Str(self.operator.as_str().to_owned()),
            &self.value,
        );
        out.finish()
    }

    pub(crate) fn error(&self, reason: ConditionError) -> ErrorReason {
        ErrorReason::Condition {
            reason,
            condition: self.error_repr(),
        }
    }

    /// `('field', 'op', value)` cut to the error budget.
    pub(crate) fn error_repr(&self) -> String {
        condition_error_repr(
            &self.field_expr,
            &Value::Str(self.operator.as_str().to_owned()),
            &self.value,
        )
    }

    fn hash_u64(&self) -> u64 {
        hash_of(&(
            4u8,
            &*self.field_expr,
            self.operator,
            self.value.kind(),
            &self.value,
        ))
    }
}

impl PartialEq for Condition {
    fn eq(&self, other: &Self) -> bool {
        self.field_expr == other.field_expr
            && self.operator == other.operator
            && self.value.strict_eq(&other.value)
    }
}

impl Eq for Condition {}

impl Hash for Condition {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash_u64());
    }
}

impl fmt::Debug for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.py_repr())
    }
}

fn write_condition_repr(out: &mut ReprWriter, field_expr: &str, operator: &Value, value: &Value) {
    out.push("(");
    write_str_repr(out, field_expr);
    out.push(", ");
    operator.write_repr(out);
    out.push(", ");
    value.write_repr(out);
    out.push(")");
}

/// `('field', 'op', value)` cut to the error budget.
/// A condition value with its parameters bound (`None` when it holds none); collection values
/// spread into the collection holding them.
fn bind_value(
    value: &Value,
    bind: &dyn Fn(&PrincipalParam) -> Result<Value, DomainError>,
) -> Result<Option<Value>, DomainError> {
    let spread = |items: &mut Vec<Value>, member: &Value| -> Result<(), DomainError> {
        match member {
            Value::Param(param) => match bind(param)? {
                Value::List(values) => items.extend(values),
                single => items.push(single),
            },
            other => items.push(other.clone()),
        }
        Ok(())
    };
    Ok(match value {
        Value::Param(param) => Some(bind(param)?),
        Value::List(list) if value.has_param() => {
            let mut items = Vec::with_capacity(list.len());
            for member in list {
                spread(&mut items, member)?;
            }
            Some(Value::List(items))
        }
        Value::Set(set) if value.has_param() => {
            let mut items = Vec::with_capacity(set.len());
            for member in set.iter() {
                spread(&mut items, member)?;
            }
            Some(Value::Set(items.into_iter().collect()))
        }
        Value::Domain(sub) if sub.has_params() => Some(Value::Domain(sub.bind_params(bind)?)),
        _ => None,
    })
}

pub(crate) fn condition_error_repr(field_expr: &str, operator: &Value, value: &Value) -> String {
    let mut out = ReprWriter::for_errors();
    write_condition_repr(&mut out, field_expr, operator, value);
    out.finish()
}

/// A field name or path, for building conditions in Rust code:
/// `Domain::field("partner_id.name")?.ilike("acme")`.
#[derive(Clone, Debug)]
pub struct FieldExpr(Arc<str>);

impl FieldExpr {
    pub fn op(&self, operator: Operator, value: impl Into<Value>) -> Domain {
        Domain::checked_condition(self.0.clone(), operator, value.into())
    }

    /// `=`.
    pub fn eq(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::Eq, value)
    }

    /// `!=`.
    pub fn ne(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::Ne, value)
    }

    /// `=?` (`TRUE` when the value is falsy).
    pub fn eq_if_set(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::EqIfSet, value)
    }

    /// `in`.
    pub fn in_(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::In, value)
    }

    /// `not in`.
    pub fn not_in(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::NotIn, value)
    }

    /// `<`.
    pub fn lt(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::Lt, value)
    }

    /// `>`.
    pub fn gt(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::Gt, value)
    }

    /// `<=`.
    pub fn le(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::Le, value)
    }

    /// `>=`.
    pub fn ge(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::Ge, value)
    }

    /// `like`.
    pub fn like(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::Like, value)
    }

    /// `not like`.
    pub fn not_like(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::NotLike, value)
    }

    /// `ilike`.
    pub fn ilike(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::ILike, value)
    }

    /// `not ilike`.
    pub fn not_ilike(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::NotILike, value)
    }

    /// `any` with a sub-domain.
    pub fn any(&self, domain: Domain) -> Domain {
        self.op(Operator::Any, domain)
    }

    /// `not any` with a sub-domain.
    pub fn not_any(&self, domain: Domain) -> Domain {
        self.op(Operator::NotAny, domain)
    }

    /// `child_of`.
    pub fn child_of(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::ChildOf, value)
    }

    /// `parent_of`.
    pub fn parent_of(&self, value: impl Into<Value>) -> Domain {
        self.op(Operator::ParentOf, value)
    }

    /// `overlaps` with the range `[lower, upper]` (a `date_range`/`datetime_range` field; `false`
    /// for an unbounded end).
    ///
    /// ```
    /// use erp_domain::Domain;
    /// let open = Domain::field("validity")?.overlaps("2026-01-01", false);
    /// assert_eq!(
    ///     open.to_json()?,
    ///     serde_json::json!([["validity", "overlaps", ["2026-01-01", false]]])
    /// );
    /// # Ok::<(), erp_domain::DomainError>(())
    /// ```
    pub fn overlaps(&self, lower: impl Into<Value>, upper: impl Into<Value>) -> Domain {
        self.op(
            Operator::Overlaps,
            Value::List(vec![lower.into(), upper.into()]),
        )
    }

    /// `contains` a date or datetime (a `date_range`/`datetime_range` field).
    pub fn contains(&self, point: impl Into<Value>) -> Domain {
        self.op(Operator::Contains, point)
    }
}

impl Domain {
    pub const TRUE: Domain = Domain(Repr::Const(true));

    pub const FALSE: Domain = Domain(Repr::Const(false));

    /// `TRUE` or `FALSE`.
    pub const fn constant(value: bool) -> Domain {
        Domain(Repr::Const(value))
    }

    /// Start a condition on a field name or path. Fails on an empty name
    /// ([`crate::ErrorKind::Programming`]).
    pub fn field(field_expr: &str) -> Result<FieldExpr, DomainError> {
        if field_expr.is_empty() {
            return Err(DomainError::programming(
                ErrorReason::InvalidFieldExpression(py_str_repr(field_expr)),
            ));
        }
        Ok(FieldExpr(Arc::from(field_expr)))
    }

    /// The value is kept as given (normalization happens in
    /// [`Domain::optimize`]).
    pub fn condition(
        field_expr: &str,
        operator: Operator,
        value: impl Into<Value>,
    ) -> Result<Domain, DomainError> {
        let value = value.into();
        if field_expr.is_empty() {
            return Err(DomainError::programming(ErrorReason::Condition {
                reason: ConditionError::EmptyFieldName,
                condition: condition_error_repr(
                    field_expr,
                    &Value::Str(operator.as_str().to_owned()),
                    &value,
                ),
            }));
        }
        Ok(Self::checked_condition(
            Arc::from(field_expr),
            operator,
            value,
        ))
    }

    /// Like [`Domain::condition`], with the operator given as a string.
    pub fn condition_str(
        field_expr: &str,
        operator: &str,
        value: impl Into<Value>,
    ) -> Result<Domain, DomainError> {
        condition_from_parts(field_expr, &Value::Str(operator.to_owned()), value.into())
            .map_err(DomainError::programming)
    }

    /// Conjunction of domains: flattens nested ANDs, drops `TRUE`, returns `FALSE` as soon as one
    /// item is `FALSE`; an empty AND is `TRUE` and a single item is returned as is. The preferred
    /// way to combine many domains in Rust code.
    pub fn and(items: impl IntoIterator<Item = Domain>) -> Domain {
        Self::apply(NaryKind::And, items)
    }

    /// Disjunction of domains, the mirror of [`Domain::and`]; an empty OR is `FALSE`.
    pub fn or(items: impl IntoIterator<Item = Domain>) -> Domain {
        Self::apply(NaryKind::Or, items)
    }

    /// Wrap a custom node.
    pub fn custom(node: Arc<dyn CustomDomain>) -> Domain {
        Self::node(NodeKind::Custom(node))
    }

    /// Errors are [`crate::ErrorKind::Validation`] and the result is tagged as
    /// external input (see [`crate::DomainError`]).
    pub fn from_value(value: &Value) -> Result<Domain, DomainError> {
        Self::from_value_inner(value)
            .map(Domain::into_external)
            .map_err(DomainError::validation)
    }

    pub(crate) fn from_value_inner(value: &Value) -> Result<Domain, ErrorReason> {
        match value {
            Value::Domain(domain) => Ok(domain.clone()),
            Value::Bool(b) => Ok(Domain::constant(*b)),
            Value::List(items) => parse_list(items, false, &mut 0, 0),
            other => Err(ErrorReason::InvalidArgumentType(other.error_repr())),
        }
    }

    /// Parse list syntax (external input): prefix `&`, `|`, `!`, implicit AND between top-level
    /// items. Internal operators (`any!`, `not any!`) are rejected. Linear in the number of items;
    /// at most [`MAX_ITEMS`] items and [`MAX_DEPTH`] levels. Errors are
    /// [`crate::ErrorKind::Validation`]; the result is tagged as external input.
    pub fn from_list(items: &[Value]) -> Result<Domain, DomainError> {
        parse_list(items, false, &mut 0, 0)
            .map(Domain::into_external)
            .map_err(DomainError::validation)
    }

    /// Same limits,
    /// error kind and tagging as [`Domain::from_list`].
    pub fn from_list_internal(items: &[Value]) -> Result<Domain, DomainError> {
        parse_list(items, true, &mut 0, 0)
            .map(Domain::into_external)
            .map_err(DomainError::validation)
    }

    /// Parse list syntax built by trusted native framework code. Internal traversal operators
    /// are accepted and quantifier lists are parsed recursively, with the same item and depth
    /// limits as [`Domain::from_list_internal`]. Errors have [`crate::ErrorKind::Programming`]
    /// provenance; parsed nodes are native and existing nested provider contexts are preserved.
    /// Native search providers must still pass ORM output admission, which refuses internal
    /// traversal operators even when this constructor accepted them.
    ///
    /// ```
    /// use erp_domain::{Domain, Value};
    /// let items = vec![Value::list(["partner_id".into(), "any!".into(), Value::List(vec![])])];
    /// let domain = Domain::from_list_native(&items)?;
    /// assert!(!domain.is_external());
    /// # Ok::<(), erp_domain::DomainError>(())
    /// ```
    pub fn from_list_native(items: &[Value]) -> Result<Domain, DomainError> {
        parse_list(items, true, &mut 0, 0).map_err(DomainError::programming)
    }

    /// Read a domain from JSON (JSON-2 surface, external input): a list-syntax array,
    /// `true`/`false`, or `[]`. A top-level `null` is rejected: a request without a domain means
    /// `TRUE`, and that default belongs to the caller (`erp-orm`/`erp-rpc`), not to the parser.
    /// Errors are [`crate::ErrorKind::Validation`].
    pub fn from_json(json: &serde_json::Value) -> Result<Domain, DomainError> {
        if json.is_null() {
            return Err(DomainError::validation(ErrorReason::InvalidArgumentType(
                "None".to_owned(),
            )));
        }
        Value::from_json_depth(json, 0)
            .and_then(|value| Self::from_value_inner(&value))
            .map(Domain::into_external)
            .map_err(DomainError::validation)
    }

    pub fn to_list(&self) -> Vec<Value> {
        let mut out = Vec::new();
        self.write_list(&mut out);
        out
    }

    /// The prefix-list form as JSON. Fails on custom nodes, opaque values and non-finite floats
    /// ([`crate::ErrorKind::Programming`]).
    pub fn to_json(&self) -> Result<serde_json::Value, DomainError> {
        self.to_json_inner().map_err(DomainError::programming)
    }

    pub(crate) fn to_json_inner(&self) -> Result<serde_json::Value, ErrorReason> {
        let items = self
            .to_list()
            .iter()
            .map(|item| match item {
                Value::Domain(_) => Err(ErrorReason::UnsupportedValue(item.error_repr())),
                other => other.to_json_inner(),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(serde_json::Value::Array(items))
    }

    /// Whether this is `TRUE`.
    pub fn is_true(&self) -> bool {
        matches!(self.0, Repr::Const(true))
    }

    /// Whether this is `FALSE`.
    pub fn is_false(&self) -> bool {
        matches!(self.0, Repr::Const(false))
    }

    /// Whether this domain is a single condition.
    pub fn is_condition(&self) -> bool {
        self.as_condition().is_some()
    }

    pub fn is_condition_matching(
        &self,
        field_expr: Option<&str>,
        operators: &[Operator],
        kinds: &[ValueKind],
    ) -> bool {
        let Some(condition) = self.as_condition() else {
            return false;
        };
        field_expr.is_none_or(|f| f == condition.field_expr())
            && (operators.is_empty() || operators.contains(&condition.operator))
            && (kinds.is_empty() || kinds.contains(&condition.value.kind()))
    }

    /// The condition, when this domain is one.
    pub fn as_condition(&self) -> Option<&Condition> {
        match self.node_kind() {
            Some(NodeKind::Condition(condition)) => Some(condition),
            _ => None,
        }
    }

    /// A view of the top node.
    pub fn kind(&self) -> DomainKind<'_> {
        match &self.0 {
            Repr::Const(true) => DomainKind::True,
            Repr::Const(false) => DomainKind::False,
            Repr::Node(node) => match &node.data.kind {
                NodeKind::Not(child) => DomainKind::Not(child),
                NodeKind::And(children) => DomainKind::And(children),
                NodeKind::Or(children) => DomainKind::Or(children),
                NodeKind::Condition(condition) => DomainKind::Condition(condition),
                NodeKind::Custom(custom) => DomainKind::Custom(custom),
            },
        }
    }

    /// The optimization level this domain handle is known to have reached, in the context it was
    /// last optimized in ([`Domain::optimize`] does not trust it for another context).
    pub fn level(&self) -> OptimizationLevel {
        match &self.0 {
            Repr::Const(_) => OptimizationLevel::Full,
            Repr::Node(node) => node.level,
        }
    }

    /// Whether the domain came from external input ([`Domain::from_json`], [`Domain::from_list`],
    /// [`Domain::from_value`]), or contains such a domain. Errors about it are
    /// [`crate::ErrorKind::Validation`].
    pub fn is_external(&self) -> bool {
        match &self.0 {
            Repr::Const(_) => false,
            Repr::Node(node) => matches!(node.origin, Origin::External),
        }
    }

    /// Whether a condition anywhere in the domain, sub-domains of `any` conditions included, has
    /// a principal parameter in its value ([`crate::PrincipalParam`]): such a domain is decided
    /// in SQL only.
    pub fn has_params(&self) -> bool {
        match self.kind() {
            DomainKind::True | DomainKind::False | DomainKind::Custom(_) => false,
            DomainKind::Not(child) => child.has_params(),
            DomainKind::And(children) | DomainKind::Or(children) => {
                children.iter().any(Domain::has_params)
            }
            DomainKind::Condition(condition) => match condition.value() {
                Value::Domain(sub) => sub.has_params(),
                value => value.has_param(),
            },
        }
    }

    /// The domain with every principal parameter replaced by `value(param)`, sub-domains
    /// included: the **test oracle** of the access engine's property tests (R-S6).
    /// Production decisions never bind parameters in memory; they are bound in SQL.
    ///
    /// `value` returns a scalar (`False` for an empty user path) or, for a collection parameter,
    /// a [`Value::List`] whose members spread into the `in` set holding the parameter.
    ///
    /// ```
    /// use erp_domain::{Domain, Operator, PrincipalParam, Value};
    /// let rule = Domain::condition("company_id", Operator::In, Value::Param(PrincipalParam::CompanyIds))?;
    /// let bound = rule.bind_params(&|p: &PrincipalParam| match p {
    ///     PrincipalParam::CompanyIds => Ok(Value::list([1, 2])),
    ///     _ => Ok(Value::FALSE),
    /// })?;
    /// assert_eq!(format!("{bound:?}"), "[('company_id', 'in', [1, 2])]");
    /// # Ok::<(), erp_domain::DomainError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// The error `value` returns.
    pub fn bind_params(
        &self,
        value: &dyn Fn(&PrincipalParam) -> Result<Value, DomainError>,
    ) -> Result<Domain, DomainError> {
        if !self.has_params() {
            return Ok(self.clone());
        }
        let mut failure: Option<DomainError> = None;
        let bound = self.map_conditions(|node| {
            if failure.is_some() {
                return node.clone();
            }
            let Some(condition) = node.as_condition() else {
                return node.clone();
            };
            match bind_value(condition.value(), value) {
                Ok(Some(new)) => Domain::raw_condition(
                    condition.field_expr_arc().clone(),
                    condition.operator(),
                    new,
                ),
                Ok(None) => node.clone(),
                Err(error) => {
                    failure = Some(error);
                    node.clone()
                }
            }
        });
        match failure {
            Some(error) => Err(error),
            None => Ok(bound),
        }
    }

    /// The conditions of the boolean tree, in order. Sub-domains of `any` conditions are not
    /// entered; constants and custom nodes are not conditions.
    pub fn iter_conditions(&self) -> impl Iterator<Item = &Domain> {
        let mut out = Vec::new();
        self.collect_conditions(&mut out);
        out.into_iter()
    }

    /// Replace every condition of the boolean tree by `f(condition)` and recombine the results
    /// with the same connectives (constants are simplified). Sub-domains of `any` conditions are
    /// not visited.
    pub fn map_conditions<F: FnMut(&Domain) -> Domain>(&self, mut f: F) -> Domain {
        let external = self.is_external();
        self.map_conditions_inner(&mut f).tagged(external)
    }

    // ----- crate-internal construction --------------------------------------------------------

    fn node(kind: NodeKind) -> Domain {
        let mut fold = 0;
        let (level, depth, hash, external) = match &kind {
            NodeKind::Not(child) => (
                OptimizationLevel::None,
                child.depth() + 1,
                hash_of(&(1u8, child.hash_u64())),
                child.is_external(),
            ),
            NodeKind::And(children) | NodeKind::Or(children) => {
                let nary = if matches!(kind, NodeKind::And(_)) {
                    NaryKind::And
                } else {
                    NaryKind::Or
                };
                let mut max_depth = 0;
                let mut external = false;
                fold = FOLD_INIT;
                for child in children {
                    fold = fold_child(fold, child.hash_u64());
                    max_depth = max_depth.max(child.depth());
                    external |= child.is_external();
                }
                (
                    OptimizationLevel::None,
                    max_depth + 1,
                    nary_hash(nary, children.len(), fold),
                    external,
                )
            }
            NodeKind::Condition(condition) => (
                OptimizationLevel::None,
                match &condition.value {
                    Value::Domain(sub) => sub.depth() + 1,
                    _ => 1,
                },
                condition.hash_u64(),
                false,
            ),
            NodeKind::Custom(custom) => (
                if custom.has_optimizer() {
                    OptimizationLevel::DynamicValues
                } else {
                    OptimizationLevel::Full
                },
                1,
                hash_of(&(5u8, custom_addr(custom))),
                false,
            ),
        };
        Domain(Repr::Node(Node {
            data: Arc::new(NodeData {
                kind,
                depth,
                hash,
                fold,
            }),
            level,
            stamp: 0,
            origin: if external {
                Origin::External
            } else {
                Origin::Native
            },
        }))
    }

    /// An AND/OR node from children whose hash fold and maximum depth are already known.
    fn nary_from_parts(
        kind: NaryKind,
        children: Vec<Domain>,
        fold: u64,
        max_child_depth: usize,
        external: bool,
    ) -> Domain {
        if children.len() < 2 {
            return Self::raw_nary(kind, children).tagged(external);
        }
        let hash = nary_hash(kind, children.len(), fold);
        let kind = match kind {
            NaryKind::And => NodeKind::And(children),
            NaryKind::Or => NodeKind::Or(children),
        };
        Domain(Repr::Node(Node {
            data: Arc::new(NodeData {
                kind,
                depth: max_child_depth + 1,
                hash,
                fold,
            }),
            level: OptimizationLevel::None,
            stamp: 0,
            origin: if external {
                Origin::External
            } else {
                Origin::Native
            },
        }))
    }

    pub(crate) fn raw_condition(field_expr: Arc<str>, operator: Operator, value: Value) -> Domain {
        Self::node(NodeKind::Condition(Condition {
            field_expr,
            operator,
            value,
        }))
    }

    fn checked_condition(field_expr: Arc<str>, operator: Operator, value: Value) -> Domain {
        match value {
            Value::NewId => {
                let operator = if operator.is_negative() {
                    Operator::NotIn
                } else {
                    Operator::In
                };
                Self::raw_condition(field_expr, operator, Value::List(Vec::new()))
            }
            value => Self::raw_condition(field_expr, operator, value),
        }
    }

    /// A NOT node, without simplification.
    pub(crate) fn raw_not(child: Domain) -> Domain {
        Self::node(NodeKind::Not(child))
    }

    /// An AND/OR node over the children as given (no flattening). Falls back to the neutral
    /// constant or the single child when fewer than two children are given.
    pub(crate) fn raw_nary(kind: NaryKind, mut children: Vec<Domain>) -> Domain {
        match children.len() {
            0 => Domain::constant(kind.zero()),
            1 => children.pop().unwrap_or(Domain::constant(kind.zero())),
            _ => Self::node(match kind {
                NaryKind::And => NodeKind::And(children),
                NaryKind::Or => NodeKind::Or(children),
            }),
        }
    }

    pub(crate) fn apply(kind: NaryKind, items: impl IntoIterator<Item = Domain>) -> Domain {
        let mut external = false;
        let items = items.into_iter().inspect(|d| external |= d.is_external());
        let mut children = flatten(kind, items.map(Ok::<_, ErrorReason>))
            .unwrap_or_else(|_| vec![Domain::constant(kind.zero())]);
        let result = if children.len() == 1 {
            children.pop().unwrap_or(Domain::constant(kind.zero()))
        } else {
            Self::raw_nary(kind, children)
        };
        result.tagged(external)
    }

    /// `a & b` / `a | b` on owned operands: constants absorbed, same-connective children
    /// flattened, reusing the child list of an operand nobody else references.
    fn combine_owned(kind: NaryKind, a: Domain, b: Domain) -> Domain {
        let zero = kind.zero();
        match (&a.0, &b.0) {
            (Repr::Const(x), _) => return if *x == zero { b } else { a },
            (_, Repr::Const(y)) => return if *y == zero { a } else { b },
            _ => {}
        }
        let external = a.is_external() || b.is_external();
        let (mut children, mut fold, mut max_depth) = a.into_parts(kind);
        let (more, _, _) = b.into_parts(kind);
        children.reserve(more.len());
        for child in more {
            fold = fold_child(fold, child.hash_u64());
            max_depth = max_depth.max(child.depth());
            children.push(child);
        }
        Self::nary_from_parts(kind, children, fold, max_depth, external)
    }

    /// The operands this domain contributes to a `kind` node, with their hash fold and maximum
    /// depth: the children of a same-connective node (moved out, without copying, when this is
    /// the only reference), or the domain itself.
    fn into_parts(self, kind: NaryKind) -> (Vec<Domain>, u64, usize) {
        match self.0 {
            Repr::Node(node) if node.data.kind.nary_children(kind).is_some() => {
                match Arc::try_unwrap(node.data) {
                    Ok(data) => {
                        let (fold, max_depth) = (data.fold, data.depth.saturating_sub(1));
                        match data.kind {
                            NodeKind::And(children) | NodeKind::Or(children) => {
                                (children, fold, max_depth)
                            }
                            other => {
                                let single = Self::node(other);
                                let fold = fold_child(FOLD_INIT, single.hash_u64());
                                let depth = single.depth();
                                (vec![single], fold, depth)
                            }
                        }
                    }
                    Err(shared) => (
                        shared
                            .kind
                            .nary_children(kind)
                            .map(<[Domain]>::to_vec)
                            .unwrap_or_default(),
                        shared.fold,
                        shared.depth.saturating_sub(1),
                    ),
                }
            }
            repr => {
                let single = Domain(repr);
                let fold = fold_child(FOLD_INIT, single.hash_u64());
                let depth = single.depth();
                (vec![single], fold, depth)
            }
        }
    }

    fn invert(&self) -> Domain {
        let result = match &self.0 {
            Repr::Const(b) => return Domain::constant(!b),
            Repr::Node(node) => match &node.data.kind {
                NodeKind::Not(child) => child.clone(),
                NodeKind::And(children) => {
                    Self::raw_nary(NaryKind::Or, children.iter().map(Domain::invert).collect())
                }
                NodeKind::Or(children) => {
                    Self::raw_nary(NaryKind::And, children.iter().map(Domain::invert).collect())
                }
                NodeKind::Condition(condition) => {
                    // Only plain fields: a path keeps its negation until optimization, and
                    // inequalities need the field type (handled when optimizing).
                    match condition.operator.inverse() {
                        Some(inverse) if !condition.field_expr.contains('.') => {
                            Self::raw_condition(
                                condition.field_expr.clone(),
                                inverse,
                                condition.value.clone(),
                            )
                        }
                        _ => Self::raw_not(self.clone()),
                    }
                }
                NodeKind::Custom(_) => Self::raw_not(self.clone()),
            },
        };
        result.inherit_origin(self)
    }

    // ----- crate-internal annotations and accessors -------------------------------------------

    pub(crate) fn depth(&self) -> usize {
        match &self.0 {
            Repr::Const(_) => 0,
            Repr::Node(node) => node.data.depth,
        }
    }

    pub(crate) fn hash_u64(&self) -> u64 {
        match &self.0 {
            Repr::Const(b) => hash_of(&(0u8, *b)),
            Repr::Node(node) => node.data.hash,
        }
    }

    /// The level reached for the optimization context with this stamp (`None` when the handle
    /// was optimized for another context).
    pub(crate) fn level_in(&self, stamp: u64) -> OptimizationLevel {
        match &self.0 {
            Repr::Const(_) => OptimizationLevel::Full,
            Repr::Node(node) if node.stamp == 0 || node.stamp == stamp => node.level,
            Repr::Node(_) => OptimizationLevel::None,
        }
    }

    /// The same domain, known to have reached `level` in the context `stamp`. Shares the node.
    pub(crate) fn with_level(&self, level: OptimizationLevel, stamp: u64) -> Domain {
        match &self.0 {
            Repr::Node(node) => Domain(Repr::Node(Node {
                data: node.data.clone(),
                level: if node.stamp == stamp || node.stamp == 0 {
                    node.level.max(level)
                } else {
                    level
                },
                stamp,
                origin: node.origin.clone(),
            })),
            Repr::Const(_) => self.clone(),
        }
    }

    /// Attach an inherited external origin without changing explicit provider origins.
    pub(crate) fn tagged(self, external: bool) -> Domain {
        if external {
            self.annotate_origin(&Origin::External)
        } else {
            self
        }
    }

    /// The same domain tagged as external input (errors about it become `Validation`), for a
    /// higher layer that rewrites an external domain and must keep its provenance.
    /// Explicit programming contexts on framework-derived descendants remain internal.
    pub fn into_external(self) -> Domain {
        self.tagged(true)
    }

    /// Mark framework-derived nodes with the context used for their programming errors.
    ///
    /// The context and provenance are annotations, excluded from predicate equality and hashing.
    /// Annotation enters boolean children and relational subdomains; an existing nested
    /// programming context takes precedence. Callers admit and bound native ASTs before
    /// annotating them, just as before walking or optimizing them. Annotation does not validate
    /// fields or operators and does not change the predicate.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use erp_domain::{Domain, Operator};
    /// let domain = Domain::condition("name", Operator::Eq, "Ada")?
    ///     .with_programming_context(Arc::from("field search provider"));
    /// assert_eq!(domain.programming_context().map(Arc::as_ref), Some("field search provider"));
    /// # Ok::<(), erp_domain::DomainError>(())
    /// ```
    pub fn with_programming_context(self, context: Arc<str>) -> Domain {
        self.annotate_origin(&Origin::Programming(context))
    }

    /// Framework context attached to this node, if it came from a domain provider.
    /// See [`Domain::with_programming_context`] for a usage example. Constants have no context.
    pub fn programming_context(&self) -> Option<&Arc<str>> {
        match &self.0 {
            Repr::Node(Node {
                origin: Origin::Programming(context),
                ..
            }) => Some(context),
            Repr::Const(_) | Repr::Node(_) => None,
        }
    }

    /// Preserve the source node's origin on replacement nodes, keeping nested provider origins.
    pub(crate) fn inherit_origin(self, source: &Domain) -> Domain {
        match &source.0 {
            Repr::Node(node) => self.annotate_origin(&node.origin),
            Repr::Const(_) => self,
        }
    }

    pub(crate) fn same_origin(&self, other: &Domain) -> bool {
        match (&self.0, &other.0) {
            (Repr::Node(a), Repr::Node(b)) => a.origin == b.origin,
            (Repr::Const(_), Repr::Const(_)) => true,
            _ => false,
        }
    }

    /// Whether semantically equal predicates can be deduplicated without losing a descendant's
    /// diagnostic origin. Unlike merge-run grouping this checks the complete provenance tree.
    pub(crate) fn same_origin_tree(&self, other: &Domain) -> bool {
        if !self.same_origin(other) {
            return false;
        }
        match (self.kind(), other.kind()) {
            (DomainKind::Not(a), DomainKind::Not(b)) => a.same_origin_tree(b),
            (DomainKind::And(a), DomainKind::And(b)) | (DomainKind::Or(a), DomainKind::Or(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_origin_tree(b))
            }
            (DomainKind::Condition(a), DomainKind::Condition(b)) => {
                Self::same_value_origins(a.value(), b.value())
            }
            // Called only after semantic equality: constants and custom nodes have no children.
            _ => true,
        }
    }

    fn same_value_origins(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Domain(a), Value::Domain(b)) => a.same_origin_tree(b),
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| Self::same_value_origins(a, b))
            }
            (Value::Set(a), Value::Set(b)) => {
                // Set equality ignores insertion order. Conservatively retain both nodes
                // when corresponding members differ rather than compare unrelated origins.
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(a, b)| a.strict_eq(b) && Self::same_value_origins(a, b))
            }
            // Scalar equality has already been checked; scalars carry no nested provenance.
            _ => true,
        }
    }

    /// Errors acquire the nearest explicit source; a native ancestor adds no new classification.
    pub(crate) fn error_origin(&self, reason: ErrorReason) -> ErrorReason {
        if matches!(reason, ErrorReason::Provenance { .. }) {
            return reason;
        }
        let (kind, context) = match &self.0 {
            Repr::Node(Node {
                origin: Origin::External,
                ..
            }) => (ErrorKind::Validation, None),
            Repr::Node(Node {
                origin: Origin::Programming(context),
                ..
            }) => (ErrorKind::Programming, Some(Arc::clone(context))),
            Repr::Const(_) | Repr::Node(_) => return reason,
        };
        ErrorReason::Provenance {
            kind,
            context,
            reason: Box::new(reason),
        }
    }

    fn annotate_origin(self, inherited: &Origin) -> Domain {
        if matches!(inherited, Origin::Native) {
            return self;
        }
        let Repr::Node(mut node) = self.0 else {
            return self;
        };
        if matches!(node.origin, Origin::Programming(_)) {
            // Existing contexts were already applied to their complete subtrees.
            return Domain(Repr::Node(node));
        }
        let kind = match &node.data.kind {
            NodeKind::Not(child) => NodeKind::Not(child.clone().annotate_origin(inherited)),
            NodeKind::And(children) => NodeKind::And(
                children
                    .iter()
                    .cloned()
                    .map(|child| child.annotate_origin(inherited))
                    .collect(),
            ),
            NodeKind::Or(children) => NodeKind::Or(
                children
                    .iter()
                    .cloned()
                    .map(|child| child.annotate_origin(inherited))
                    .collect(),
            ),
            NodeKind::Condition(condition) => {
                let Value::Domain(subdomain) = &condition.value else {
                    // Scalar payloads need only a handle annotation; keep shared NodeData.
                    node.origin = inherited.clone();
                    return Domain(Repr::Node(node));
                };
                let mut condition = condition.clone();
                condition.value = Value::Domain(subdomain.clone().annotate_origin(inherited));
                NodeKind::Condition(condition)
            }
            NodeKind::Custom(_) => {
                // Custom payload identity is unchanged by provenance annotation.
                node.origin = inherited.clone();
                return Domain(Repr::Node(node));
            }
        };
        node.data = Arc::new(NodeData {
            kind,
            depth: node.data.depth,
            hash: node.data.hash,
            fold: node.data.fold,
        });
        node.origin = inherited.clone();
        Domain(Repr::Node(node))
    }

    pub(crate) fn ptr_eq(&self, other: &Domain) -> bool {
        match (&self.0, &other.0) {
            (Repr::Const(a), Repr::Const(b)) => a == b,
            (Repr::Node(a), Repr::Node(b)) => Arc::ptr_eq(&a.data, &b.data),
            _ => false,
        }
    }

    fn node_kind(&self) -> Option<&NodeKind> {
        match &self.0 {
            Repr::Const(_) => None,
            Repr::Node(node) => Some(&node.data.kind),
        }
    }

    fn collect_conditions<'a>(&'a self, out: &mut Vec<&'a Domain>) {
        match self.node_kind() {
            Some(NodeKind::Condition(_)) => out.push(self),
            Some(NodeKind::Not(child)) => child.collect_conditions(out),
            Some(NodeKind::And(children) | NodeKind::Or(children)) => {
                for child in children {
                    child.collect_conditions(out);
                }
            }
            Some(NodeKind::Custom(_)) | None => {}
        }
    }

    fn map_conditions_inner(&self, f: &mut dyn FnMut(&Domain) -> Domain) -> Domain {
        match self.node_kind() {
            Some(NodeKind::Condition(_)) => f(self).inherit_origin(self),
            Some(NodeKind::Not(child)) => child.map_conditions_inner(f).invert(),
            Some(NodeKind::And(children)) => Self::apply(
                NaryKind::And,
                children
                    .iter()
                    .map(|c| c.map_conditions_inner(f))
                    .collect::<Vec<_>>(),
            ),
            Some(NodeKind::Or(children)) => Self::apply(
                NaryKind::Or,
                children
                    .iter()
                    .map(|c| c.map_conditions_inner(f))
                    .collect::<Vec<_>>(),
            ),
            Some(NodeKind::Custom(_)) | None => self.clone(),
        }
    }

    fn write_list(&self, out: &mut Vec<Value>) {
        match &self.0 {
            Repr::Const(b) => out.push(Value::List(vec![
                Value::Int(i64::from(*b)),
                Value::Str("=".to_owned()),
                Value::Int(1),
            ])),
            Repr::Node(node) => match &node.data.kind {
                NodeKind::Not(child) => {
                    out.push(Value::Str("!".to_owned()));
                    child.write_list(out);
                }
                NodeKind::And(children) | NodeKind::Or(children) => {
                    let symbol = if matches!(node.data.kind, NodeKind::And(_)) {
                        NaryKind::And.symbol()
                    } else {
                        NaryKind::Or.symbol()
                    };
                    for _ in 1..children.len() {
                        out.push(Value::Str(symbol.to_owned()));
                    }
                    for child in children {
                        child.write_list(out);
                    }
                }
                NodeKind::Condition(condition) => out.push(Value::List(vec![
                    Value::Str(condition.field_expr.to_string()),
                    Value::Str(condition.operator.as_str().to_owned()),
                    condition.value.to_list_form(),
                ])),
                NodeKind::Custom(_) => out.push(Value::Domain(self.clone())),
            },
        }
    }

    /// Python `repr(list(domain))`, with conditions as tuples.
    pub(crate) fn py_repr(&self) -> String {
        let mut out = ReprWriter::unlimited();
        self.write_repr_into(&mut out);
        out.finish()
    }

    pub(crate) fn write_repr_into(&self, out: &mut ReprWriter) {
        out.push("[");
        let mut first = true;
        self.write_items_repr(out, &mut first);
        out.push("]");
    }

    fn write_items_repr(&self, out: &mut ReprWriter, first: &mut bool) {
        fn sep(out: &mut ReprWriter, first: &mut bool) {
            if !std::mem::take(first) {
                out.push(", ");
            }
        }
        if out.is_full() {
            return;
        }
        match &self.0 {
            Repr::Const(b) => {
                sep(out, first);
                out.push(if *b { "(1, '=', 1)" } else { "(0, '=', 1)" });
            }
            Repr::Node(node) => match &node.data.kind {
                NodeKind::Not(child) => {
                    sep(out, first);
                    out.push("'!'");
                    child.write_items_repr(out, first);
                }
                NodeKind::And(children) | NodeKind::Or(children) => {
                    let symbol = if matches!(node.data.kind, NodeKind::And(_)) {
                        "'&'"
                    } else {
                        "'|'"
                    };
                    for _ in 1..children.len() {
                        if out.is_full() {
                            return;
                        }
                        sep(out, first);
                        out.push(symbol);
                    }
                    for child in children {
                        child.write_items_repr(out, first);
                    }
                }
                NodeKind::Condition(condition) => {
                    sep(out, first);
                    out.push("(");
                    write_str_repr(out, &condition.field_expr);
                    out.push(", ");
                    write_str_repr(out, condition.operator.as_str());
                    out.push(", ");
                    match &condition.value {
                        Value::Domain(sub) => sub.write_repr_into(out),
                        Value::Set(set) => {
                            Value::List(set.iter().cloned().collect()).write_repr(out);
                        }
                        other => other.write_repr(out),
                    }
                    out.push(")");
                }
                NodeKind::Custom(custom) => {
                    sep(out, first);
                    out.push(&format!("{custom:?}"));
                }
            },
        }
    }
}

/// Never returns an empty list.
pub(crate) fn flatten<I>(kind: NaryKind, items: I) -> Result<Vec<Domain>, ErrorReason>
where
    I: IntoIterator<Item = Result<Domain, ErrorReason>>,
{
    let zero = kind.zero();
    let mut result = Vec::new();
    for item in items {
        let child = item?;
        match &child.0 {
            Repr::Const(b) => {
                if *b != zero {
                    return Ok(vec![child]);
                }
            }
            Repr::Node(node) => match node.data.kind.nary_children(kind) {
                Some(children) => result.extend(children.iter().cloned()),
                None => result.push(child),
            },
        }
    }
    if result.is_empty() {
        result.push(Domain::constant(zero));
    }
    Ok(result)
}

fn and_pair(a: &Domain, b: &Domain) -> Domain {
    match (&a.0, &b.0) {
        (_, Repr::Const(true)) | (Repr::Const(false), _) => a.clone(),
        (_, Repr::Const(false)) | (Repr::Const(true), _) => b.clone(),
        _ => Domain::apply(NaryKind::And, [a.clone(), b.clone()]),
    }
}

fn or_pair(a: &Domain, b: &Domain) -> Domain {
    match (&a.0, &b.0) {
        (_, Repr::Const(false)) | (Repr::Const(true), _) => a.clone(),
        (_, Repr::Const(true)) | (Repr::Const(false), _) => b.clone(),
        _ => Domain::apply(NaryKind::Or, [a.clone(), b.clone()]),
    }
}

/// `a & b`: conjunction, with constants absorbed and ANDs flattened (reusing the child list of an
/// operand that is not shared).
impl BitAnd for Domain {
    type Output = Domain;

    fn bitand(self, rhs: Domain) -> Domain {
        Domain::combine_owned(NaryKind::And, self, rhs)
    }
}

/// `&a & &b`: conjunction of borrowed domains.
impl BitAnd for &Domain {
    type Output = Domain;

    fn bitand(self, rhs: &Domain) -> Domain {
        and_pair(self, rhs)
    }
}

/// `a | b`: disjunction, with constants absorbed and ORs flattened (reusing the child list of an
/// operand that is not shared).
impl BitOr for Domain {
    type Output = Domain;

    fn bitor(self, rhs: Domain) -> Domain {
        Domain::combine_owned(NaryKind::Or, self, rhs)
    }
}

/// `&a | &b`: disjunction of borrowed domains.
impl BitOr for &Domain {
    type Output = Domain;

    fn bitor(self, rhs: &Domain) -> Domain {
        or_pair(self, rhs)
    }
}

impl Not for Domain {
    type Output = Domain;

    fn not(self) -> Domain {
        self.invert()
    }
}

/// `!&d`: negation of a borrowed domain.
impl Not for &Domain {
    type Output = Domain;

    fn not(self) -> Domain {
        self.invert()
    }
}

impl PartialEq for Domain {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Repr::Const(a), Repr::Const(b)) => a == b,
            (Repr::Node(a), Repr::Node(b)) => {
                Arc::ptr_eq(&a.data, &b.data)
                    || (a.data.hash == b.data.hash && a.data.kind == b.data.kind)
            }
            _ => false,
        }
    }
}

impl Eq for Domain {}

impl PartialEq for NodeKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (NodeKind::Not(a), NodeKind::Not(b)) => a == b,
            (NodeKind::And(a), NodeKind::And(b)) | (NodeKind::Or(a), NodeKind::Or(b)) => a == b,
            (NodeKind::Condition(a), NodeKind::Condition(b)) => a == b,
            (NodeKind::Custom(a), NodeKind::Custom(b)) => custom_addr(a) == custom_addr(b),
            _ => false,
        }
    }
}

fn custom_addr(custom: &Arc<dyn CustomDomain>) -> usize {
    Arc::as_ptr(custom).cast::<()>() as usize
}

/// Hashing writes the hash computed when the node was built: O(1).
impl Hash for Domain {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash_u64());
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.py_repr())
    }
}

impl fmt::Debug for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.py_repr())
    }
}

impl From<Condition> for Domain {
    fn from(condition: Condition) -> Self {
        Domain::node(NodeKind::Condition(condition))
    }
}

// ----- list syntax ----------------------------------------------------------------------------

/// A stack entry of the list parser: a finished domain, or an AND/OR still collecting operands.
/// Building n-ary nodes in place keeps parsing linear: `['|', '|', …, A, B, C, …]` appends to one
/// list instead of re-flattening the growing node at every operator.
enum Item {
    Done(Domain),
    Open {
        kind: NaryKind,
        children: VecDeque<Domain>,
        depth: usize,
    },
}

impl Item {
    fn depth(&self) -> usize {
        match self {
            Item::Done(domain) => domain.depth(),
            Item::Open { depth, .. } => *depth,
        }
    }

    fn freeze(self) -> Domain {
        match self {
            Item::Done(domain) => domain,
            Item::Open { kind, children, .. } => Domain::raw_nary(kind, Vec::from(children)),
        }
    }

    /// The depth this item contributes to a `kind` node it is flattened into.
    fn depth_in(&self, kind: NaryKind) -> usize {
        match self {
            Item::Open { kind: k, depth, .. } if *k == kind => *depth,
            Item::Done(domain) if domain.is_nary(kind) => domain.depth(),
            other => other.depth() + 1,
        }
    }

    /// The operands this item contributes to a `kind` node.
    fn into_operands(self, kind: NaryKind) -> VecDeque<Domain> {
        match self {
            Item::Open {
                kind: k, children, ..
            } if k == kind => children,
            Item::Done(domain) => match domain.node_kind().and_then(|n| n.nary_children(kind)) {
                Some(children) => children.iter().cloned().collect(),
                None => VecDeque::from([domain]),
            },
            open => VecDeque::from([open.freeze()]),
        }
    }

    fn constant(&self) -> Option<bool> {
        match self {
            Item::Done(Domain(Repr::Const(b))) => Some(*b),
            _ => None,
        }
    }
}

impl Domain {
    fn is_nary(&self, kind: NaryKind) -> bool {
        self.node_kind()
            .is_some_and(|n| n.nary_children(kind).is_some())
    }
}

fn combine_items(kind: NaryKind, left: Item, right: Item) -> Item {
    let zero = kind.zero();
    if let Some(b) = left.constant() {
        return if b == zero { right } else { left };
    }
    if let Some(b) = right.constant() {
        return if b == zero { left } else { right };
    }
    let depth = left.depth_in(kind).max(right.depth_in(kind));
    let left_open = matches!(&left, Item::Open { kind: k, .. } if *k == kind);
    let right_open = matches!(&right, Item::Open { kind: k, .. } if *k == kind);
    let children = if left_open || !right_open {
        let mut children = left.into_operands(kind);
        children.extend(right.into_operands(kind));
        children
    } else {
        // Right-nested chains grow at the front.
        let mut children = right.into_operands(kind);
        for operand in left.into_operands(kind).into_iter().rev() {
            children.push_front(operand);
        }
        children
    };
    Item::Open {
        kind,
        children,
        depth,
    }
}

fn parse_list(
    items: &[Value],
    internal: bool,
    budget: &mut usize,
    descent_depth: usize,
) -> Result<Domain, ErrorReason> {
    // Check before descent and before cloning an operand: completed-entry checks alone
    // run too late for deeply nested native quantifier lists.
    if descent_depth > MAX_DEPTH {
        return Err(ErrorReason::TooDeep(MAX_DEPTH));
    }
    let malformed = || {
        let mut out = ReprWriter::for_errors();
        out.push("[");
        write_items(&mut out, items.iter());
        out.push("]");
        ErrorReason::Malformed(out.finish())
    };
    let mut stack: Vec<Item> = Vec::new();
    for item in items.iter().rev() {
        *budget += 1;
        if *budget > MAX_ITEMS {
            return Err(ErrorReason::TooManyItems(MAX_ITEMS));
        }
        let entry = match item {
            Value::List(triple) if triple.len() == 3 => {
                Item::Done(parse_triple(triple, internal, budget, descent_depth)?)
            }
            Value::Str(s) if s == "&" || s == "|" => {
                let kind = if s == "&" {
                    NaryKind::And
                } else {
                    NaryKind::Or
                };
                let left = stack.pop().ok_or_else(malformed)?;
                let right = stack.pop().ok_or_else(malformed)?;
                combine_items(kind, left, right)
            }
            Value::Str(s) if s == "!" => Item::Done(!stack.pop().ok_or_else(malformed)?.freeze()),
            Value::Domain(domain) => Item::Done(domain.clone()),
            other => return Err(ErrorReason::InvalidItem(item_error_repr(other))),
        };
        if entry.depth() > MAX_DEPTH {
            return Err(ErrorReason::TooDeep(MAX_DEPTH));
        }
        stack.push(entry);
    }
    if stack.len() == 1 {
        return Ok(stack.pop().map_or(Domain::TRUE, Item::freeze));
    }
    Ok(Domain::and(stack.into_iter().rev().map(Item::freeze)))
}

/// Python repr of a list-syntax item cut for an error message, with 3-item lists as tuples.
fn item_error_repr(item: &Value) -> String {
    match item {
        Value::List(triple) if triple.len() == 3 => tuple_error_repr(triple),
        other => other.error_repr(),
    }
}

fn tuple_error_repr(triple: &[Value]) -> String {
    let mut out = ReprWriter::for_errors();
    out.push("(");
    write_items(&mut out, triple.iter());
    out.push(")");
    out.finish()
}

fn parse_triple(
    triple: &[Value],
    internal: bool,
    budget: &mut usize,
    descent_depth: usize,
) -> Result<Domain, ErrorReason> {
    let [field, operator, value] = triple else {
        return Err(ErrorReason::InvalidItem(tuple_error_repr(triple)));
    };
    let op_str = match operator {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    };
    let is_internal_op = matches!(op_str, Some("any!" | "not any!"));
    if !internal && is_internal_op {
        return Err(ErrorReason::InvalidItem(tuple_error_repr(triple)));
    }
    let is_any_op = is_internal_op || matches!(op_str, Some("any" | "not any"));
    let value = if internal && is_any_op {
        if let Value::List(sub) = value {
            Value::Domain(parse_list(sub, true, budget, descent_depth + 1)?)
        } else {
            clone_bounded_value(value)?
        }
    } else {
        clone_bounded_value(value)?
    };
    match field {
        Value::Str(field_expr) => condition_from_parts(field_expr, operator, value),
        _ => {
            // The constant leaves (1, '=', 1) and (0, '=', 1).
            let is_eq_one = *operator == Value::Str("=".to_owned()) && value == Value::Int(1);
            if is_eq_one && *field == Value::Int(1) {
                Ok(Domain::TRUE)
            } else if is_eq_one && *field == Value::Int(0) {
                Ok(Domain::FALSE)
            } else {
                Err(ErrorReason::InvalidArguments(tuple_error_repr(triple)))
            }
        }
    }
}

// Bound recursive Value::clone independently of quantifier parsing. Iterator frames keep
// scratch memory proportional to nesting depth, including wide membership operands.
fn clone_bounded_value(value: &Value) -> Result<Value, ErrorReason> {
    let mut pending = value;
    let mut stack: Vec<Box<dyn Iterator<Item = &Value> + '_>> = Vec::new();
    loop {
        if stack.len() > MAX_DEPTH {
            return Err(ErrorReason::TooDeep(MAX_DEPTH));
        }
        if let Some(items) = pending.collection_items() {
            if stack.len() >= MAX_DEPTH {
                return Err(ErrorReason::TooDeep(MAX_DEPTH));
            }
            stack.push(items);
        }
        loop {
            let Some(items) = stack.last_mut() else {
                return Ok(value.clone());
            };
            if let Some(child) = items.next() {
                pending = child;
                break;
            }
            stack.pop();
        }
    }
}

/// Checked condition from dynamic parts: empty field and unknown operator are errors.
fn condition_from_parts(
    field_expr: &str,
    operator: &Value,
    value: Value,
) -> Result<Domain, ErrorReason> {
    if field_expr.is_empty() {
        return Err(ErrorReason::Condition {
            reason: ConditionError::EmptyFieldName,
            condition: condition_error_repr(field_expr, operator, &value),
        });
    }
    let parsed = match operator {
        Value::Str(s) => Operator::parse(s),
        _ => None,
    };
    let Some(operator_parsed) = parsed else {
        return Err(ErrorReason::Condition {
            reason: ConditionError::InvalidOperator,
            condition: condition_error_repr(field_expr, operator, &value),
        });
    };
    Ok(Domain::checked_condition(
        Arc::from(field_expr),
        operator_parsed,
        value,
    ))
}
