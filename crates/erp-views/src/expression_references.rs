//! Pure semantic references in description expressions; caller authorization stays host-side.
//!
//! One private walker serves every entry: the source helper below, the builder's admission of an
//! expression and both payload visitors (which decode the embedded wire document first). It
//! derives each reference from the expression itself, charges every visited node to a [`Meter`]
//! (the request's shared expression work budget, or nothing for the bounded source helper) and
//! hands every path to a sink as borrowed segments, so deriving a reference allocates nothing.
use crate::{ViewError, ViewResult as Result};
use erp_expr::Expr;
use std::collections::BTreeSet;

/// The meaning of expression text, so literal context/value arrays are not guessed as domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExpressionKind {
    /// A domain declaration, including its literal criterion operators.
    Domain,
    /// A context dictionary, including declared group_by field references.
    Context,
    /// An ordinary value expression inside a criterion.
    Value,
}

/// Parse bounded expression source and return all semantic model-field references.
///
/// These strings confer no access: the host must resolve every segment against its current
/// registry, principal, field policy and groups before using the expression or metadata.
/// This function does not certify whole-request construction/work accounting. A
/// `parent.<path>` reference is refused here because a bare source string declares no parent model.
pub fn expression_references(source: &str, kind: ExpressionKind) -> Result<BTreeSet<String>> {
    let limits = erp_expr::EvalLimits::default()
        .with_max_source_bytes(64 * 1024)
        .with_max_depth(100);
    let parsed = erp_expr::parse_with_limits(source, limits)
        .map_err(|error| fail(format!("Unsupported search expression: {error}")))?;
    let mut fields = BTreeSet::new();
    walk_expression(&parsed, kind, false, &mut NoMeter, |found| {
        fields.insert(found.parts.join("."));
        Ok(())
    })?;
    Ok(fields)
}
fn fail(message: impl Into<String>) -> ViewError {
    ViewError::InvalidDescription(message.into())
}

/// Maximum logical depth of an admitted expression (the wire limit).
const MAX_DEPTH: usize = erp_expr::wire::MAX_AST_DEPTH;
/// An attribute chain has at most one segment per tree level, plus its base name.
const MAX_PARTS: usize = MAX_DEPTH + 2;
const TRAVERSAL: &str = "Unsupported search expression traversal requires a later release.";
const SUBSCRIPT: &str = "Unsupported search subscript requires a later release.";
const DYNAMIC: &str = "Unsupported dynamic search field attribute traversal. Additional expression traversal requires a later release.";

/// Charges the work of one walk; a refusal ends it.
pub(crate) trait Meter {
    fn step(&mut self) -> Result<()>;
}
/// No accounting: the bounded source helper, whose input the parser has already limited.
pub(crate) struct NoMeter;
impl Meter for NoMeter {
    fn step(&mut self) -> Result<()> {
        Ok(())
    }
}
/// One step of the request's shared expression work budget per visited node.
pub(crate) struct BudgetMeter<'b>(pub(crate) &'b crate::DescriptionBudget);
impl Meter for BudgetMeter<'_> {
    fn step(&mut self) -> Result<()> {
        self.0.charge_work(1)
    }
}

/// One derived reference, borrowed from the expression: `parts` joined by `.` is its path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Found<'p> {
    /// Resolved against the declared parent model instead of the slot's scope.
    pub(crate) parent: bool,
    /// Literal criterion LHS, independently of value references with the same spelling.
    pub(crate) criterion: bool,
    pub(crate) parts: &'p [&'p str],
}

const GRANULARITIES: [&str; 5] = ["day", "week", "month", "quarter", "year"];

/// Split `path[:granularity]`: at most one supported suffix; the path is returned without it.
pub(crate) fn parse_group_by(text: &str) -> Result<&str> {
    // A path is at most 4096 bytes and the longest suffix ("quarter") plus its colon is 8.
    if text.len() > 4096 + 8 {
        return Err(fail("Description group_by has an invalid field path"));
    }
    let mut parts = text.split(':');
    let path = parts.next().unwrap_or_default();
    let granularity = parts.next();
    if parts.next().is_some() {
        return Err(fail(
            "Description group_by has more than one granularity suffix",
        ));
    }
    if path.is_empty() || !crate::description_schema::field_path(path) {
        return Err(fail("Description group_by has an invalid field path"));
    }
    match granularity {
        Some(name) if !GRANULARITIES.contains(&name) => Err(fail(format!(
            "Description group_by granularity '{name}' is unsupported; expected day, week, month, quarter or year"
        ))),
        _ => Ok(path),
    }
}

fn is_execution_identifier(name: &str) -> bool {
    matches!(
        name,
        "self"
            | "context"
            | "uid"
            | "user"
            | "time"
            | "datetime"
            | "date"
            | "relativedelta"
            | "active_id"
            | "active_ids"
            | "active_model"
            | "parent"
            | "current_date"
            | "True"
            | "False"
            | "None"
    )
}
fn is_domain_operator(op: &str) -> bool {
    matches!(
        op,
        "=" | "!="
            | "<="
            | "<"
            | ">="
            | ">"
            | "=?"
            | "=like"
            | "=ilike"
            | "like"
            | "not like"
            | "ilike"
            | "not ilike"
            | "in"
            | "not in"
            | "child_of"
            | "parent_of"
    )
}
fn is_logical_prefix(text: &str) -> bool {
    matches!(text, "&" | "|" | "!")
}

/// Walk `expr` as `kind`, reporting every model-field reference to `sink`; returns the logical
/// height reached. `parent_declared` says whether the slot declares a parent model; without one
/// a `parent.<path>` reference refuses instead of being silently dropped.
pub(crate) fn walk_expression<M: Meter>(
    expr: &Expr,
    kind: ExpressionKind,
    parent_declared: bool,
    meter: &mut M,
    sink: impl FnMut(Found<'_>) -> Result<()>,
) -> Result<usize> {
    if kind == ExpressionKind::Domain {
        validate_search_domain(expr, 1, meter)?;
    }
    let mut walker = Walker {
        meter,
        sink,
        parent_declared,
        scoped: false,
        height: 0,
    };
    let mode = match kind {
        ExpressionKind::Value => Mode::Values,
        ExpressionKind::Domain | ExpressionKind::Context => Mode::Fields,
    };
    walker.visit(expr, 1, mode)?;
    Ok(walker.height)
}

/// `Fields` interprets criterion triples and `group_by` keys; `Values` treats every list as data.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Fields,
    Values,
}

struct Walker<'m, M: Meter, S: FnMut(Found<'_>) -> Result<()>> {
    meter: &'m mut M,
    sink: S,
    parent_declared: bool,
    scoped: bool,
    height: usize,
}
impl<M: Meter, S: FnMut(Found<'_>) -> Result<()>> Walker<'_, M, S> {
    fn emit(&mut self, parent: bool, parts: &[&str]) -> Result<()> {
        if parent && !self.parent_declared {
            return Err(fail(
                "Description expression uses parent.<path> without a declared parent model",
            ));
        }
        (self.sink)(Found {
            parent,
            criterion: false,
            parts,
        })
    }

    fn attribute(&mut self, expr: &Expr, mut depth: usize) -> Result<()> {
        let mut parts = [""; MAX_PARTS];
        let mut count = 0usize;
        let mut current = expr;
        while let Expr::Attribute { value, attr } = current {
            let slot = parts
                .get_mut(count)
                .ok_or_else(|| fail("Description expression exceeds its logical depth"))?;
            *slot = attr.as_str();
            count += 1;
            current = value;
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(fail("Description expression exceeds its logical depth"));
            }
            // visit() already charged the outer Attribute. Every nested Attribute and the
            // terminal base node is examined here and must contribute one work/depth step.
            self.meter.step()?;
            self.height = self.height.max(depth);
        }
        let Expr::Name(name) = current else {
            return Err(fail(DYNAMIC));
        };
        if name == "parent" && !self.scoped {
            parts[..count].reverse();
            return self.emit(true, &parts[..count]);
        }
        if is_execution_identifier(name) && !self.scoped {
            return Ok(());
        }
        let slot = parts
            .get_mut(count)
            .ok_or_else(|| fail("Description expression exceeds its logical depth"))?;
        *slot = name.as_str();
        count += 1;
        parts[..count].reverse();
        self.emit(false, &parts[..count])
    }

    fn group_by_text(&mut self, text: &str) -> Result<()> {
        // An empty string names no field; anything else must be a path with a supported suffix.
        if text.is_empty() {
            return Ok(());
        }
        let path = parse_group_by(text)?;
        self.emit(false, &[path])
    }

    fn visit(&mut self, expr: &Expr, depth: usize, mode: Mode) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(fail("Description expression exceeds its logical depth"));
        }
        self.meter.step()?;
        self.height = self.height.max(depth);
        let next = depth + 1;
        match expr {
            Expr::Literal(_) => {}
            Expr::Name(name) => {
                if self.scoped || !is_execution_identifier(name) {
                    self.emit(false, &[name.as_str()])?;
                }
            }
            Expr::Unary { operand, .. } => self.visit(operand, next, mode)?,
            Expr::Binary { left, right, .. } => {
                self.visit(left, next, mode)?;
                self.visit(right, next, mode)?;
            }
            Expr::List(values) | Expr::Tuple(values) if mode == Mode::Fields => {
                self.list(values, next)?;
            }
            Expr::List(values) | Expr::Tuple(values) | Expr::BoolOp { values, .. } => {
                for value in values {
                    self.visit(value, next, mode)?;
                }
            }
            Expr::Compare {
                left, comparators, ..
            } => {
                self.visit(left, next, mode)?;
                for value in comparators {
                    self.visit(value, next, mode)?;
                }
            }
            Expr::IfExp { test, body, orelse } => {
                self.visit(test, next, mode)?;
                self.visit(body, next, mode)?;
                self.visit(orelse, next, mode)?;
            }
            Expr::Attribute { .. } => self.attribute(expr, depth)?,
            Expr::Subscript { value, slice } => {
                self.visit(value, next, mode)?;
                match slice {
                    erp_expr::SubscriptKind::Index(index) => {
                        self.visit(index, next, Mode::Values)?;
                    }
                    erp_expr::SubscriptKind::Slice { lower, upper, step } => {
                        for part in [lower, upper, step].into_iter().flatten() {
                            self.visit(part, next, Mode::Values)?;
                        }
                    }
                    _ => return Err(fail(SUBSCRIPT)),
                }
            }
            Expr::Call {
                func,
                args,
                keywords,
            } => {
                self.visit(func, next, mode)?;
                for arg in args {
                    self.visit(arg, next, mode)?;
                }
                for (_, value) in keywords {
                    self.visit(value, next, mode)?;
                }
            }
            Expr::Dict(items) => {
                for (key, value) in items {
                    if mode == Mode::Fields
                        && let Expr::Literal(erp_expr::Literal::Str(name)) = key
                        && name == "group_by"
                    {
                        self.group_by(value)?;
                    }
                    // Context values may contain literal triples; only group_by strings above
                    // describe model fields. Inspect expressions without domain guessing.
                    self.visit(key, next, Mode::Values)?;
                    self.visit(value, next, Mode::Values)?;
                }
            }
            Expr::Walrus { value, .. }
            | Expr::FormattedValue { value, .. }
            | Expr::Starred(value) => self.visit(value, next, mode)?,
            Expr::JoinedStr(parts) => {
                for part in parts {
                    self.visit(part, next, mode)?;
                }
            }
            _ => return Err(fail(TRAVERSAL)),
        }
        Ok(())
    }

    /// A literal group_by entry, or a list of them; other list entries are inspected as values.
    fn group_by(&mut self, value: &Expr) -> Result<()> {
        use erp_expr::Literal;
        match value {
            Expr::Literal(Literal::Str(text)) => self.group_by_text(text),
            Expr::List(elements) | Expr::Tuple(elements) => {
                for element in elements {
                    if let Expr::Literal(Literal::Str(text)) = element {
                        self.group_by_text(text)?;
                    }
                }
                Ok(())
            }
            _ => Err(fail(TRAVERSAL)),
        }
    }

    /// A list or tuple under `Mode::Fields`: a criterion `(field, operator, value)` when it has
    /// three elements and a domain operator second, otherwise a list of criteria and prefixes.
    fn list(&mut self, values: &[Expr], depth: usize) -> Result<()> {
        use erp_expr::Literal;
        if let [first, Expr::Literal(Literal::Str(op)), value] = values
            && is_domain_operator(op)
        {
            // Scoped traversal charges the two static criterion elements it inspects here.
            if self.scoped {
                self.meter.step()?;
                self.meter.step()?;
            }
            match first {
                Expr::Literal(Literal::Str(field)) => {
                    if !is_logical_prefix(field) && !field.is_empty() {
                        (self.sink)(Found {
                            parent: false,
                            criterion: true,
                            parts: &[field.as_str()],
                        })?;
                    }
                }
                other => self.visit(other, depth, Mode::Fields)?,
            }
            // The criterion value is data: sub-expressions only, never nested criteria.
            return self.visit(value, depth, Mode::Values);
        }
        for value in values {
            match value {
                Expr::Literal(Literal::Str(text)) if is_logical_prefix(text) => {}
                other => self.visit(other, depth, Mode::Fields)?,
            }
        }
        Ok(())
    }
}

/// Reject unsupported literal domain operators before any expression can be emitted.
fn validate_search_domain<M: Meter>(expr: &Expr, depth: usize, meter: &mut M) -> Result<()> {
    use erp_expr::Literal;
    if depth > MAX_DEPTH {
        return Err(fail("Description expression exceeds its logical depth"));
    }
    meter.step()?;
    match expr {
        Expr::List(values) | Expr::Tuple(values) => {
            if values.len() == 3
                && let Expr::Literal(Literal::Str(field)) = &values[0]
                && !is_logical_prefix(field)
                && let Expr::Literal(Literal::Str(op)) = &values[1]
            {
                if !is_domain_operator(op) {
                    return Err(fail(format!(
                        "Unsupported search domain operator '{op}'. Additional operators require a later release."
                    )));
                }
                // The criterion value is data, not a nested domain declaration.
                return Ok(());
            }
            for value in values {
                if matches!(value, Expr::Literal(Literal::Str(op)) if is_logical_prefix(op)) {
                    continue;
                }
                validate_search_domain(value, depth + 1, meter)?;
            }
            Ok(())
        }
        Expr::IfExp { body, orelse, .. } => {
            validate_search_domain(body, depth + 1, meter)?;
            validate_search_domain(orelse, depth + 1, meter)
        }
        Expr::Binary { left, right, .. } => {
            validate_search_domain(left, depth + 1, meter)?;
            validate_search_domain(right, depth + 1, meter)
        }
        _ => Err(fail(
            "Unsupported search domain structure. Additional domain expressions require a later release.",
        )),
    }
}

const SCOPED_STRUCTURE: &str = "Description scoped domain has an unsupported domain structure or operator; additional syntax requires a later release";
const PANEL_SYNTAX: &str = "Search-panel domain syntax is unsupported in version 1; additional syntax requires a later release";

/// Sanitize scoped-domain diagnostics while preserving admission failure kinds and messages.
pub(crate) fn scoped_problem(error: ViewError) -> ViewError {
    match error {
        ViewError::InvalidDescription(_) => fail(SCOPED_STRUCTURE),
        other => other,
    }
}

/// Flat complete-prefix domain structure; RHS expressions are deliberately not panel-authorized.
fn scoped_structure<M: Meter>(expr: &Expr, meter: &mut M) -> Result<()> {
    use erp_expr::Literal;
    meter.step()?;
    let (Expr::List(items) | Expr::Tuple(items)) = expr else {
        return Err(fail(SCOPED_STRUCTURE));
    };
    let mut pending = 0usize;
    for item in items {
        meter.step()?;
        if pending == 0 {
            pending = 1;
        }
        match item {
            Expr::Literal(Literal::Str(op)) if is_logical_prefix(op) => {
                if op != "!" {
                    pending = pending
                        .checked_add(1)
                        .ok_or_else(|| fail(SCOPED_STRUCTURE))?;
                }
            }
            Expr::List(parts) | Expr::Tuple(parts) => {
                let [
                    Expr::Literal(Literal::Str(path)),
                    Expr::Literal(Literal::Str(op)),
                    _,
                ] = parts.as_slice()
                else {
                    return Err(fail(SCOPED_STRUCTURE));
                };
                meter.step()?;
                meter.step()?;
                if !crate::description_schema::field_path(path) || !is_domain_operator(op) {
                    return Err(fail(SCOPED_STRUCTURE));
                }
                pending -= 1;
            }
            _ => return Err(fail(SCOPED_STRUCTURE)),
        }
    }
    if pending != 0 {
        return Err(fail(SCOPED_STRUCTURE));
    }
    Ok(())
}

/// Generic scoped-domain traversal: criterion role is explicit and every RHS name is visible.
pub(crate) fn walk_scoped_domain<M: Meter>(
    expr: &Expr,
    meter: &mut M,
    sink: impl FnMut(Found<'_>) -> Result<()>,
) -> Result<usize> {
    scoped_structure(expr, meter)?;
    let mut walker = Walker {
        meter,
        sink,
        parent_declared: false,
        scoped: true,
        height: 0,
    };
    walker
        .visit(expr, 1, Mode::Fields)
        .map_err(scoped_problem)?;
    Ok(walker.height)
}

/// Validate the closed version-one search-panel domain grammar without resolving dependencies.
///
/// Complete flat prefix expressions and literal criteria are allowed. RHS values are scalar
/// literals, list/tuple data, bare category names or one sign directly on a numeric literal.
/// A name, including `uid` or `context`, is a dependency for the host to check against retained
/// categories. This function grants no model, category or field authority and does not charge
/// a request allowance; schema builders and independent visitors do that separately.
///
/// # Errors
/// Returns a sanitized [`ViewError::InvalidDescription`] for malformed prefix structure,
/// unsupported operators, execution syntax, nested domain groups or unsupported RHS nodes.
///
/// ```
/// let expression = erp_expr::parse("[('category_id', '=', category_id)]")?;
/// let admitted = erp_expr::wire::ValidatedAst::from_expr(&expression)?;
/// erp_views::validate_panel_domain(&admitted)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn validate_panel_domain(admitted: &erp_expr::wire::ValidatedAst) -> Result<()> {
    let validate = || {
        scoped_structure(admitted.as_expr(), &mut NoMeter)?;
        let (Expr::List(items) | Expr::Tuple(items)) = admitted.as_expr() else {
            return Err(fail(PANEL_SYNTAX));
        };
        for item in items {
            if let Expr::List(parts) | Expr::Tuple(parts) = item {
                let [_, _, rhs] = parts.as_slice() else {
                    return Err(fail(PANEL_SYNTAX));
                };
                panel_value(rhs, 1)?;
            }
        }
        Ok(())
    };
    validate().map_err(|_| fail(PANEL_SYNTAX))
}

fn panel_value(expr: &Expr, depth: usize) -> Result<()> {
    use erp_expr::{Literal, UnaryOp};
    if depth > MAX_DEPTH {
        return Err(fail(PANEL_SYNTAX));
    }
    match expr {
        Expr::Literal(Literal::None | Literal::Bool(_) | Literal::Int(_) | Literal::Str(_)) => {
            Ok(())
        }
        Expr::Literal(Literal::Float(value)) if value.is_finite() => Ok(()),
        Expr::Name(name) if crate::description_schema::field_path(name) && !name.contains('.') => {
            Ok(())
        }
        Expr::List(items) | Expr::Tuple(items) => items
            .iter()
            .try_for_each(|item| panel_value(item, depth + 1)),
        Expr::Unary {
            op: UnaryOp::UAdd | UnaryOp::USub,
            operand,
        } => match operand.as_ref() {
            Expr::Literal(Literal::Int(_)) => Ok(()),
            Expr::Literal(Literal::Float(value)) if value.is_finite() => Ok(()),
            _ => Err(fail(PANEL_SYNTAX)),
        },
        _ => Err(fail(PANEL_SYNTAX)),
    }
}

#[cfg(test)]
mod traversal_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
    use super::*;

    fn chain() -> Expr {
        Expr::Attribute {
            value: Box::new(Expr::Attribute {
                value: Box::new(Expr::Name("partner".into())),
                attr: "company".into(),
            }),
            attr: "name".into(),
        }
    }
    struct Counting(usize);
    impl Meter for Counting {
        fn step(&mut self) -> Result<()> {
            self.0 += 1;
            Ok(())
        }
    }
    #[test]
    fn every_attribute_and_base_node_counts_work_and_height() {
        let mut meter = Counting(0);
        let mut paths = Vec::new();
        let height = walk_expression(
            &chain(),
            ExpressionKind::Value,
            false,
            &mut meter,
            |found| {
                paths.push(found.parts.join("."));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(meter.0, 3);
        assert_eq!(height, 3);
        assert_eq!(paths, ["partner.company.name"]);
    }
    #[test]
    fn attribute_work_refusal_is_typed_and_sticky() {
        let budget = crate::DescriptionBudget::with_limits(1024, 2, 1024).unwrap();
        let error = walk_expression(
            &chain(),
            ExpressionKind::Value,
            false,
            &mut BudgetMeter(&budget),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error,
            ViewError::LimitExceeded {
                limit: crate::LimitKind::DescriptionExpressionWork,
                max: 2,
                actual: Some(3)
            }
        );
        assert_eq!(
            error.to_string(),
            "expression work steps per request limit exceeded: maximum 2, reached 3"
        );
        assert!(budget.check().is_err());
    }
    #[test]
    fn attribute_chain_obeys_the_same_nested_depth_limit() {
        let mut expr = chain();
        for _ in 0..MAX_DEPTH - 2 {
            expr = Expr::Unary {
                op: erp_expr::UnaryOp::Not,
                operand: Box::new(expr),
            };
        }
        let error = walk_expression(
            &expr,
            ExpressionKind::Value,
            false,
            &mut NoMeter,
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error,
            fail("Description expression exceeds its logical depth")
        );
        assert_eq!(
            error.to_string(),
            "Description expression exceeds its logical depth"
        );
    }
}
