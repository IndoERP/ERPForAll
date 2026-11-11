//! The field references of the expression attributes of a view (5, d10,
//! ): `invisible`, `readonly`, `required`,
//! `column_invisible`, `decoration-*`, `context` (with the field names of its `group_by`), the
//! `domain` of a `<field>` and of a `<filter>`, `filter_domain` of a search `<field>`,
//! `default_order`, and the `date` / `end_date` of a `<filter>`.
//!
//! [`walk`] parses one attribute value with the expression engine (`erp-expr`, its default
//! limits) and returns the fields it reads, each as `(model, path)`.
//! A domain is read by its structure: the left side of a leaf is a
//! field path, the right side is an expression.
//!
//! A domain leaf with a computed operator, an `any` family operator whose right side
//! is not a literal list of leaves, a client-provided name that is also a field, and a `group_by`
//! of another type than string or list of strings are refused too. Every walk is iterative; none recurses over the expression.
//!
//! The walk is pure. What the registry knows comes through [`FieldSource`].

use std::collections::HashSet;

use erp_expr::{EvalLimits, Expr, Literal, SubscriptKind};

use crate::arch::NodeOrigin;
use crate::guard::{GuardLookup, echo};
use crate::validate::ArchIssue;

/// Most hops of a domain's left path; the plan's own bound (`GuardPlanLimits::MAX_PATH_HOPS`).
pub(crate) const MAX_PATH_HOPS: usize = 8;

/// Longest field name, in bytes (the plan's bound, `GuardPlanLimits::MAX_NAME_BYTES`).
const MAX_NAME_BYTES: usize = 128;

/// Names the client provides; they are never fields (`self` is the
/// value typed into a search field).
const PROVIDED: [&str; 22] = [
    "True",
    "False",
    "None",
    "self",
    "uid",
    "context",
    "context_today",
    "allowed_company_ids",
    "current_company_id",
    "time",
    "datetime",
    "relativedelta",
    "current_date",
    "today",
    "now",
    "abs",
    "len",
    "bool",
    "float",
    "str",
    "unicode",
    "set",
];

/// What the walk asks of the registry; implemented over [`GuardLookup`] here and over the
/// install-time `RegistryLookup` by the validator.
pub(crate) trait FieldSource {
    /// Whether `model` has the field `field`.
    fn has_field(&self, model: &str, field: &str) -> bool;
    /// The comodel of the relational field `field` on `model`; `None` for any other field.
    fn comodel(&self, model: &str, field: &str) -> Option<&str>;
}

impl<'a> FieldSource for dyn GuardLookup + 'a {
    fn has_field(&self, model: &str, field: &str) -> bool {
        GuardLookup::has_field(self, model, field)
    }
    fn comodel(&self, model: &str, field: &str) -> Option<&str> {
        GuardLookup::comodel(self, model, field)
    }
}

/// How an attribute value is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrKind {
    /// A Python expression (modifiers, `decoration-*`).
    Expression,
    /// The `context` attribute: an expression whose `group_by` value is read as field names
    /// .
    Context,
    /// A bare field name (`date` / `end_date` of a `<filter>`).
    FieldName,
    /// The `domain` of a `<field>`: left paths on the field's comodel, right names on the node's
    /// model.
    FieldDomain,
    /// The `domain` of a `<filter>` or the `filter_domain` of a search `<field>`: both sides on
    /// the view's model.
    FilterDomain,
    /// `default_order` of a `<list>`: `field [asc|desc], ...`.
    ListOrder,
    /// `default_order` of a `<pivot>`: one measure (a field, or `__count`) and a direction.
    PivotOrder,
}

impl AttrKind {
    /// How attribute `name` of an element `tag` is read, `None` when its value is not an
    /// expression this module reads. `t-if`/`t-elif`/`t-att-*` (card templates compile in
    /// restricted JS mode, d10) and `options` (widget-specific) are not.
    pub(crate) fn of(tag: &str, name: &str) -> Option<Self> {
        match (tag, name) {
            (_, "invisible" | "readonly" | "required" | "column_invisible") => {
                Some(Self::Expression)
            }
            (_, "context") => Some(Self::Context),
            ("filter", "date" | "end_date") => Some(Self::FieldName),
            (_, decoration) if decoration.starts_with("decoration-") => Some(Self::Expression),
            ("field", "domain") => Some(Self::FieldDomain),
            ("filter", "domain") | ("field", "filter_domain") => Some(Self::FilterDomain),
            ("list", "default_order") => Some(Self::ListOrder),
            ("pivot", "default_order") => Some(Self::PivotOrder),
            _ => None,
        }
    }
}

/// Where an attribute sits: the models its names resolve against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Site<'a> {
    pub(crate) kind: AttrKind,
    /// The model of the node: the view's model, or a sub-view's comodel.
    pub(crate) model: &'a str,
    /// The model enclosing the sub-view the node is in; `None` at the top of the view.
    pub(crate) parent: Option<&'a str>,
    /// For [`AttrKind::FieldDomain`]: the comodel of the `<field>`, `None` when it is not
    /// relational.
    pub(crate) comodel: Option<&'a str>,
}

/// A field read: the model and a dotted path of at most [`MAX_PATH_HOPS`] hops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExprRef {
    pub(crate) model: Box<str>,
    pub(crate) path: Box<str>,
}

/// Why an attribute cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ExprProblem {
    /// The source does not parse (the expression engine's own message).
    #[error("{0}")]
    Syntax(String),
    /// A construct a view expression cannot use.
    #[error("{0} is not supported in a view expression")]
    Unsupported(&'static str),
    /// `parent.x` at the top of a view.
    #[error("'parent' is only available inside the sub-view of a relational field")]
    ParentOutsideSubView,
    /// A name the plan cannot hold.
    #[error("'{0}' is not a field name (ASCII letters, digits and '_', at most 128 bytes)")]
    BadName(String),
    /// A value that is not a domain.
    #[error("{0}")]
    NotADomain(String),
    /// A `domain` on a field that has no comodel.
    #[error("a domain on a field that is not relational makes no sense")]
    NonRelationalDomain,
    /// A path that continues through a scalar field.
    #[error(
        "field \"{field}\" of model \"{model}\" is not relational, so the path '{path}' cannot \
         continue through it"
    )]
    NotRelationalHop {
        model: String,
        field: String,
        path: String,
    },
    /// A path over the hop bound.
    #[error("the path '{path}' has {hops} hops: at most {max} are allowed")]
    TooManyHops {
        path: String,
        hops: usize,
        max: usize,
    },
    /// A bad `default_order`.
    #[error("{0}")]
    Order(String),
    /// A name the client provides that is also a field of the model it would resolve on.
    #[error(
        "'{name}' is a name the client provides and also a field of model \"{model}\": the view \
         cannot say which one it reads"
    )]
    ShadowedName { name: String, model: String },
    /// A name or path hop that is not a field of its model.
    #[error("Field \"{field}\" does not exist in model \"{model}\"")]
    UnknownField { model: String, field: String },
}

impl ExprProblem {
    /// The arch issue for attribute `attribute` with value `value` on the node `origin`.
    pub(crate) fn into_issue(self, attribute: &str, value: &str, origin: NodeOrigin) -> ArchIssue {
        match self {
            Self::UnknownField { model, field } => ArchIssue::UnknownFieldInExpression {
                attribute: attribute.to_owned(),
                field,
                model,
                origin,
            },
            other => ArchIssue::InvalidExpression {
                attribute: attribute.to_owned(),
                value: echo(value),
                problem: other.to_string(),
                origin,
            },
        }
    }
}

/// Whether `value` is one of the constants most attributes hold (`0`, `1`, `True`, `False`, any
/// case) or blank: nothing to read.
pub(crate) fn is_constant(value: &str) -> bool {
    value.trim().is_empty()
        || ["0", "1", "true", "false"]
            .iter()
            .any(|c| value.eq_ignore_ascii_case(c))
}

/// The fields read by `value`, the value of an attribute read as `site.kind`, in order of first
/// occurrence and without repeats.
///
/// # Errors
///
/// An [`ExprProblem`] naming what is wrong; nothing is returned partly read.
pub(crate) fn walk<S: FieldSource + ?Sized>(
    site: &Site<'_>,
    value: &str,
    source: &S,
) -> Result<Vec<ExprRef>, ExprProblem> {
    let mut refs = Refs::default();
    match site.kind {
        AttrKind::FieldName => {
            let name = value.trim();
            if !name.is_empty() {
                field_of(site.model, name, source, &mut refs)?;
            }
        }
        AttrKind::Expression | AttrKind::Context => {
            if is_constant(value) {
                return Ok(refs.list);
            }
            let expr = parse(value.trim())?;
            names(&expr, site, source, &mut refs)?;
        }
        AttrKind::FieldDomain | AttrKind::FilterDomain => {
            if value.trim().is_empty() {
                return Ok(refs.list);
            }
            let expr = parse(&format!("({})", value.trim()))?;
            domain(&expr, site, source, &mut refs)?;
        }
        AttrKind::ListOrder | AttrKind::PivotOrder => {
            order(value, site, source, &mut refs)?;
        }
    }
    Ok(refs.list)
}

fn parse(source: &str) -> Result<Expr, ExprProblem> {
    erp_expr::parse_with_limits(source, EvalLimits::default())
        .map_err(|e| ExprProblem::Syntax(e.to_string()))
}

#[derive(Default)]
struct Refs {
    list: Vec<ExprRef>,
    seen: HashSet<(Box<str>, Box<str>)>,
}

impl Refs {
    fn add(&mut self, model: &str, path: &str) {
        let key: (Box<str>, Box<str>) = (model.into(), path.into());
        if self.seen.insert(key.clone()) {
            self.list.push(ExprRef {
                model: key.0,
                path: key.1,
            });
        }
    }
}

/// A field name the plan can hold: ASCII letter or `_`, then letters, digits and `_`, at most
/// 128 bytes (the rule of `GuardPlanBuilder::field`).
fn check_name(name: &str) -> Result<(), ExprProblem> {
    let mut chars = name.chars();
    let valid = name.len() <= MAX_NAME_BYTES
        && matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(ExprProblem::BadName(echo(name)))
    }
}

fn is_provided(name: &str) -> bool {
    PROVIDED.contains(&name)
}

/// Reads `name` as a field of `model`.
fn field_of<S: FieldSource + ?Sized>(
    model: &str,
    name: &str,
    source: &S,
    refs: &mut Refs,
) -> Result<(), ExprProblem> {
    check_name(name)?;
    if !source.has_field(model, name) {
        return Err(ExprProblem::UnknownField {
            model: model.to_owned(),
            field: name.to_owned(),
        });
    }
    refs.add(model, name);
    Ok(())
}

/// The names an expression reads. Iterative, children in source order.
fn names<S: FieldSource + ?Sized>(
    root: &Expr,
    site: &Site<'_>,
    source: &S,
    refs: &mut Refs,
) -> Result<(), ExprProblem> {
    let mut stack: Vec<&Expr> = vec![root];
    while let Some(expr) = stack.pop() {
        match expr {
            Expr::Literal(_) => {}
            Expr::Name(name) => {
                if name == "parent" {
                    return Err(ExprProblem::Unsupported(
                        "'parent' other than as `parent.<field>`",
                    ));
                }
                if is_provided(name) {
                    // the reference client lets a field value override a provided name
                    // (`record.js:1460-1463`), so a same-named field would be read unseen
                    if source.has_field(site.model, name) {
                        return Err(ExprProblem::ShadowedName {
                            name: name.clone(),
                            model: site.model.to_owned(),
                        });
                    }
                } else {
                    field_of(site.model, name, source, refs)?;
                }
            }
            Expr::Attribute { value, attr } => match &**value {
                Expr::Name(base) if base == "parent" => {
                    let Some(parent) = site.parent else {
                        return Err(ExprProblem::ParentOutsideSubView);
                    };
                    if attr == "parent" && source.has_field(parent, "parent") {
                        return Err(ExprProblem::Unsupported(
                            "'parent.parent' on a model that has a field named 'parent'",
                        ));
                    }
                    field_of(parent, attr, source, refs)?;
                }
                other => stack.push(other),
            },
            Expr::Unary { operand, .. } => stack.push(operand),
            Expr::Binary { left, right, .. } => {
                stack.push(right);
                stack.push(left);
            }
            Expr::BoolOp { values, .. } => stack.extend(values.iter().rev()),
            Expr::Compare {
                left, comparators, ..
            } => {
                stack.extend(comparators.iter().rev());
                stack.push(left);
            }
            Expr::IfExp { test, body, orelse } => {
                stack.push(orelse);
                stack.push(body);
                stack.push(test);
            }
            Expr::Subscript { value, slice } => {
                match slice {
                    SubscriptKind::Index(index) => stack.push(index),
                    SubscriptKind::Slice { lower, upper, step } => {
                        for part in [step, upper, lower].into_iter().flatten() {
                            stack.push(part);
                        }
                    }
                    _ => return Err(ExprProblem::Unsupported("this kind of subscript")),
                }
                stack.push(value);
            }
            Expr::Call {
                func,
                args,
                keywords,
            } => {
                for (name, value) in keywords.iter().rev() {
                    stack.push(value);
                    if site.kind == AttrKind::Context && name.as_deref() == Some("group_by") {
                        group_by(value, site, source, refs)?;
                    }
                }
                stack.extend(args.iter().rev());
                stack.push(func);
            }
            Expr::List(items) | Expr::Tuple(items) => stack.extend(items.iter().rev()),
            Expr::Dict(pairs) => {
                for (key, value) in pairs.iter().rev() {
                    stack.push(value);
                    stack.push(key);
                }
                if site.kind == AttrKind::Context {
                    for (key, value) in pairs {
                        match key {
                            Expr::Literal(Literal::Str(k)) if k == "group_by" => {
                                group_by(value, site, source, refs)?;
                            }
                            Expr::Literal(_) => {}
                            // a computed key could be `group_by`
                            _ => {
                                return Err(ExprProblem::Unsupported(
                                    "a context key that is not a string literal",
                                ));
                            }
                        }
                    }
                }
            }
            Expr::Walrus { .. } => {
                return Err(ExprProblem::Unsupported("an assignment expression (:=)"));
            }
            Expr::JoinedStr(_) | Expr::FormattedValue { .. } => {
                return Err(ExprProblem::Unsupported("an f-string"));
            }
            Expr::ListComp { .. } | Expr::GeneratorExp { .. } => {
                return Err(ExprProblem::Unsupported("a comprehension"));
            }
            Expr::Starred(_) => return Err(ExprProblem::Unsupported("a starred expression")),
            _ => return Err(ExprProblem::Unsupported("this kind of expression")),
        }
    }
    Ok(())
}

fn group_by<S: FieldSource + ?Sized>(
    value: &Expr,
    site: &Site<'_>,
    source: &S,
    refs: &mut Refs,
) -> Result<(), ExprProblem> {
    const NOT_NAMES: ExprProblem =
        ExprProblem::Unsupported("a 'group_by' value that is not a string or a list of strings");
    let one = |expr: &Expr, refs: &mut Refs| match expr {
        Expr::Literal(Literal::Str(spec)) => {
            let name = spec.split(':').next().unwrap_or_default();
            field_of(site.model, name, source, refs)
        }
        _ => Err(NOT_NAMES),
    };
    match value {
        Expr::List(items) | Expr::Tuple(items) => {
            for item in items {
                one(item, refs)?;
            }
            Ok(())
        }
        other => one(other, refs),
    }
}

/// Work of the domain walk: an expression read as a domain (with the dotted path from the left
/// model that its leaves continue, empty at the top and the comodel path under `any`), or as a
/// plain expression.
enum Work<'a> {
    Domain(&'a Expr, String),
    Names(&'a Expr),
}

fn is_domainish(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::List(_) | Expr::IfExp { .. } | Expr::BoolOp { .. } | Expr::Binary { .. }
    )
}

const NOT_A_DOMAIN: &str = "the domain is not a list of leaves";
const BAD_ITEM: &str = "a domain item must be an operator, True, False or a leaf";
const BAD_LEAF: &str = "a domain leaf must have three elements (field, operator, value)";
const BAD_LEFT: &str = "the left side of a domain leaf must be a field name";
const BAD_OPERATOR: &str = "the operator of a domain leaf must be a string literal";
const ANY_RIGHT: &str =
    "the right side of 'any', 'not any', 'any!' or 'not any!' must be a literal list of leaves";
const ANY_LEFT: &str =
    "the left side of 'any', 'not any', 'any!' or 'not any!' must be a relational field";

fn not_a_domain(message: &str) -> ExprProblem {
    ExprProblem::NotADomain(message.to_owned())
}

/// The fields a domain reads: a bare name is read as an
/// expression; otherwise the structure is a list of leaves, possibly behind `if`/`and`/`or`/`+`.
/// The list on the right of an `any` family operator is read as a domain on the comodel of the
/// left path.
fn domain<S: FieldSource + ?Sized>(
    root: &Expr,
    site: &Site<'_>,
    source: &S,
    refs: &mut Refs,
) -> Result<(), ExprProblem> {
    let left_model = match site.kind {
        AttrKind::FieldDomain => site.comodel.ok_or(ExprProblem::NonRelationalDomain)?,
        _ => site.model,
    };
    let mut stack = vec![if matches!(root, Expr::Name(_)) {
        Work::Names(root)
    } else {
        Work::Domain(root, String::new())
    }];
    while let Some(work) = stack.pop() {
        let (expr, prefix) = match work {
            Work::Names(expr) => {
                names(expr, site, source, refs)?;
                continue;
            }
            Work::Domain(expr, prefix) => (expr, prefix),
        };
        match expr {
            Expr::IfExp { test, body, orelse } => {
                stack.push(Work::Domain(orelse, prefix.clone()));
                stack.push(Work::Domain(body, prefix));
                stack.push(Work::Names(test));
            }
            Expr::BoolOp { values, .. } => {
                for value in values.iter().rev() {
                    stack.push(if is_domainish(value) {
                        Work::Domain(value, prefix.clone())
                    } else {
                        Work::Names(value)
                    });
                }
            }
            Expr::Binary { left, right, .. } => {
                for side in [&**right, &**left] {
                    stack.push(if is_domainish(side) {
                        Work::Domain(side, prefix.clone())
                    } else {
                        Work::Names(side)
                    });
                }
            }
            Expr::List(items) | Expr::Tuple(items) => {
                // nested domains found in this list are walked after it, in order
                let mut nested = Vec::new();
                for item in items {
                    if let Some(next) = leaf(item, left_model, &prefix, site, source, refs)? {
                        nested.push(next);
                    }
                }
                stack.extend(nested.into_iter().rev());
            }
            _ => return Err(not_a_domain(NOT_A_DOMAIN)),
        }
    }
    Ok(())
}

/// One item of a domain list: an operator, a boolean, or a `(left, operator, right)` leaf. A
/// leaf under an `any` family operator returns the domain on its right, to be walked on the
/// comodel of its left path.
fn leaf<'a, S: FieldSource + ?Sized>(
    item: &'a Expr,
    left_model: &str,
    prefix: &str,
    site: &Site<'_>,
    source: &S,
    refs: &mut Refs,
) -> Result<Option<Work<'a>>, ExprProblem> {
    match item {
        Expr::Literal(Literal::Bool(_)) => Ok(None),
        Expr::Literal(Literal::Int(0 | 1)) => Ok(None),
        Expr::Literal(Literal::Str(op)) if matches!(op.as_str(), "&" | "|" | "!") => Ok(None),
        Expr::List(parts) | Expr::Tuple(parts) => {
            let [left, operator, right] = parts.as_slice() else {
                return Err(not_a_domain(BAD_LEAF));
            };
            let Expr::Literal(Literal::Str(operator)) = operator else {
                return Err(not_a_domain(BAD_OPERATOR));
            };
            let any = matches!(operator.as_str(), "any" | "not any" | "any!" | "not any!");
            let mut comodel_path = None;
            match left {
                Expr::Literal(Literal::Str(path)) => {
                    let full = if prefix.is_empty() {
                        path.clone()
                    } else {
                        format!("{prefix}.{path}")
                    };
                    let end = path_of(&full, left_model, source, refs)?;
                    if any {
                        comodel_path = end.map(|_| full);
                    }
                }
                Expr::Literal(Literal::Int(0 | 1) | Literal::Bool(_)) => {}
                // the deprecated form `(python expression, '=', 1)`
                _ if matches!(right, Expr::Literal(Literal::Int(1))) => {
                    names(left, site, source, refs)?;
                }
                _ => return Err(not_a_domain(BAD_LEFT)),
            }
            if !any {
                names(right, site, source, refs)?;
                return Ok(None);
            }
            let (Expr::List(_) | Expr::Tuple(_)) = right else {
                return Err(not_a_domain(ANY_RIGHT));
            };
            let Some(full) = comodel_path else {
                return Err(not_a_domain(ANY_LEFT));
            };
            Ok(Some(Work::Domain(right, full)))
        }
        _ => Err(not_a_domain(BAD_ITEM)),
    }
}

/// Resolves the dotted path `path` from `model`, hop by hop, and reads it.
fn path_of<S: FieldSource + ?Sized>(
    path: &str,
    model: &str,
    source: &S,
    refs: &mut Refs,
) -> Result<Option<()>, ExprProblem> {
    // one hop past the bound is enough to refuse; a longer literal is not split further
    let hops: Vec<&str> = path.split('.').take(MAX_PATH_HOPS + 1).collect();
    if hops.first() == Some(&"parent") {
        if source.has_field(model, "parent") {
            return Err(ExprProblem::Unsupported(
                "a domain path starting with 'parent' on a model that has a field named 'parent'",
            ));
        }
        return Ok(None);
    }
    if hops.len() > MAX_PATH_HOPS {
        return Err(ExprProblem::TooManyHops {
            path: echo(path),
            hops: hops.len(),
            max: MAX_PATH_HOPS,
        });
    }
    let mut current = model;
    for (i, hop) in hops.iter().enumerate() {
        check_name(hop)?;
        if !source.has_field(current, hop) {
            return Err(ExprProblem::UnknownField {
                model: current.to_owned(),
                field: (*hop).to_owned(),
            });
        }
        if i + 1 < hops.len() {
            current =
                source
                    .comodel(current, hop)
                    .ok_or_else(|| ExprProblem::NotRelationalHop {
                        model: current.to_owned(),
                        field: (*hop).to_owned(),
                        path: echo(path),
                    })?;
        }
    }
    refs.add(model, path);
    Ok(source.comodel(current, hops[hops.len() - 1]).map(|_| ()))
}

/// `default_order`: comma-separated `field [asc|desc]`; a pivot has one measure, which may be
/// the row count `__count` (`pivot_model.js` reads it as `measure order`).
fn order<S: FieldSource + ?Sized>(
    value: &str,
    site: &Site<'_>,
    source: &S,
    refs: &mut Refs,
) -> Result<(), ExprProblem> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    let pivot = site.kind == AttrKind::PivotOrder;
    let terms: Vec<&str> = value.split(',').collect();
    if pivot && terms.len() != 1 {
        return Err(ExprProblem::Order(
            "a pivot order is one measure and an optional direction".to_owned(),
        ));
    }
    for term in terms {
        let mut parts = term.split_whitespace();
        let Some(name) = parts.next() else {
            return Err(ExprProblem::Order("an empty term in the order".to_owned()));
        };
        if let Some(direction) = parts.next()
            && !(direction.eq_ignore_ascii_case("asc") || direction.eq_ignore_ascii_case("desc"))
        {
            return Err(ExprProblem::Order(format!(
                "'{}' is not asc or desc",
                echo(direction)
            )));
        }
        if parts.next().is_some() {
            return Err(ExprProblem::Order(format!(
                "'{}' is not a field name with an optional direction",
                echo(term.trim())
            )));
        }
        if pivot && name == "__count" {
            continue;
        }
        field_of(site.model, name, source, refs)?;
    }
    Ok(())
}
