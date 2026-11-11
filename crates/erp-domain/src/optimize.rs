//! BASIC optimization (SPEC-DOM §4.4–4.6).
//!
//! Only the BASIC level lives here: it
//! depends on field definitions alone, so its result can be memoized across transactions
//! . DYNAMIC_VALUES and FULL need the environment (clock, access rules, search
//! methods, queries) and are built on top of this in `erp-orm`.

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use time::{Duration, PrimitiveDateTime};

use crate::dates;
use crate::domain::{Condition, Domain, DomainKind, NaryKind, OptimizationLevel, flatten};
use crate::error::{ConditionError, DomainError, ErrorReason};
use crate::operator::Operator;
use crate::param::{NowKind, PrincipalParam};
use crate::schema::{FieldInfo, FieldType, ModelContext, TimeZone};
use crate::value::{OpaqueKind, Value, ValueSet, py_str_repr};

const MAX_OPTIMIZE_ITERATIONS: usize = 1000;

impl Domain {
    /// The result depends only on field definitions (and on the timezone for date values given
    /// to datetime fields), so it can be memoized per model. Optimizing an optimized domain
    /// returns an equal domain.
    ///
    /// Errors follow the failing node's provenance: original external input is
    /// [`crate::ErrorKind::Validation`], while typed/native input and nodes with an explicit
    /// [`Domain::with_programming_context`] are [`crate::ErrorKind::Programming`]. A provider
    /// context takes precedence inside an enclosing external domain.
    /// Levels
    /// reached under another [`ModelContext::cache_key`] are not trusted: the domain is checked
    /// again for this context.
    pub fn optimize(&self, model: &ModelContext<'_>) -> Result<Domain, DomainError> {
        let steps = AtomicUsize::new(0);
        let ctx = model.with_steps(&steps);
        let external = self.is_external();
        optimize_basic(self, &ctx)
            .map(|domain| domain.tagged(external))
            .map_err(|reason| {
                if external {
                    DomainError::validation(reason)
                } else {
                    DomainError::programming(reason)
                }
            })
    }
}

/// Upper bound of optimization steps for one [`Domain::optimize`] call, all nodes together.
const MAX_TOTAL_STEPS: usize = 4_000_000;

fn optimize_basic(domain: &Domain, model: &ModelContext<'_>) -> Result<Domain, ErrorReason> {
    optimize_basic_inner(domain, model)
        .map(|optimized| optimized.inherit_origin(domain))
        .map_err(|reason| domain.error_origin(reason))
}

fn optimize_basic_inner(domain: &Domain, model: &ModelContext<'_>) -> Result<Domain, ErrorReason> {
    let target = OptimizationLevel::Basic;
    let stamp = model.stamp();
    let mut current = domain.clone();
    let mut count = 0;
    while current.level_in(stamp) < target {
        count += 1;
        if count > MAX_OPTIMIZE_ITERATIONS {
            return Err(ErrorReason::TooManyLoops);
        }
        if let Some(steps) = model.steps()
            && steps.fetch_add(1, AtomicOrdering::Relaxed) >= MAX_TOTAL_STEPS
        {
            return Err(ErrorReason::StepBudgetExceeded(MAX_TOTAL_STEPS));
        }
        let next = current.level_in(stamp).next();
        let stepped = step(&current, model)?.inherit_origin(&current);
        if stepped == current {
            // Keep the existing node (identity is cheaper to compare later) and record its level
            // for this context.
            current = current.with_level(next, stamp);
        } else {
            current = stepped;
        }
    }
    Ok(current)
}

fn step(domain: &Domain, model: &ModelContext<'_>) -> Result<Domain, ErrorReason> {
    match domain.kind() {
        DomainKind::True | DomainKind::False | DomainKind::Custom(_) => Ok(domain.clone()),
        DomainKind::Not(child) => negate(&optimize_basic(child, model)?, model),
        DomainKind::And(children) => nary_step(domain, NaryKind::And, children, model),
        DomainKind::Or(children) => nary_step(domain, NaryKind::Or, children, model),
        DomainKind::Condition(condition) => condition_step(domain, condition, model),
    }
}

fn negate(domain: &Domain, model: &ModelContext<'_>) -> Result<Domain, ErrorReason> {
    match domain.kind() {
        DomainKind::Condition(condition) => {
            let Some(inverse) = condition.operator().inverse_inequality() else {
                return Ok(!domain);
            };
            // The complementary inequality; types without a falsy value also keep NULL rows,
            // so that D and ~D partition the records.
            let complement = Domain::raw_condition(
                condition.field_expr_arc().clone(),
                inverse,
                condition.value().clone(),
            );
            let field = condition_field(condition, model)?;
            if field.field_type.has_falsy_value() {
                Ok(complement)
            } else {
                let is_null = Domain::raw_condition(
                    condition.field_expr_arc().clone(),
                    Operator::In,
                    set_of_false(),
                );
                Ok(is_null | complement)
            }
        }
        DomainKind::And(children) => Ok(Domain::raw_nary(
            NaryKind::Or,
            children
                .iter()
                .map(|child| negate(child, model))
                .collect::<Result<_, _>>()?,
        )),
        DomainKind::Or(children) => Ok(Domain::raw_nary(
            NaryKind::And,
            children
                .iter()
                .map(|child| negate(child, model))
                .collect::<Result<_, _>>()?,
        )),
        _ => Ok(!domain),
    }
}

// ----- conditions -----------------------------------------------------------------------------

type Rule =
    fn(&Domain, &Condition, &FieldInfo<'_>, &ModelContext<'_>) -> Result<Domain, ErrorReason>;

const EQ_IF_SET_RULES: &[Rule] = &[rule_eq_if_set];
const EQ_RULES: &[Rule] = &[rule_eq_as_in];
const IN_RULES: &[Rule] = &[rule_in_set, rule_in_required];
const ANY_RULES: &[Rule] = &[rule_any_value, rule_any_sub_domain];
const LIKE_RULES: &[Rule] = &[rule_like_value];
const RELATIONAL_RULES: &[Rule] = &[rule_relational_name_search];
const BOOLEAN_RULES: &[Rule] = &[rule_boolean_in];
const DATE_RULES: &[Rule] = &[rule_date_value];
const DATETIME_RULES: &[Rule] = &[rule_datetime_value];

fn operator_rules(operator: Operator) -> &'static [Rule] {
    match operator {
        Operator::EqIfSet => EQ_IF_SET_RULES,
        Operator::Eq | Operator::Ne => EQ_RULES,
        Operator::In | Operator::NotIn => IN_RULES,
        op if op.is_any() => ANY_RULES,
        op if op.is_like() => LIKE_RULES,
        _ => &[],
    }
}

fn type_rules(field_type: FieldType) -> &'static [Rule] {
    match field_type {
        FieldType::Many2one | FieldType::One2many | FieldType::Many2many => RELATIONAL_RULES,
        FieldType::Boolean => BOOLEAN_RULES,
        FieldType::Date => DATE_RULES,
        FieldType::Datetime => DATETIME_RULES,
        _ => &[],
    }
}

fn condition_step(
    domain: &Domain,
    condition: &Condition,
    model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let (name, property) = split_field_expr(condition.field_expr())?;
    let field = model.field_of(name, || condition.error_repr())?;
    if let Some(property) = property
        && field.field_type.is_relational()
    {
        // a.b.c op v  ->  a any (b.c op v)
        let sub = Domain::raw_condition(
            Arc::from(property),
            condition.operator(),
            condition.value().clone(),
        );
        return Ok(Domain::raw_condition(
            Arc::from(name),
            Operator::Any,
            Value::Domain(sub),
        ));
    }
    let rules = operator_rules(condition.operator())
        .iter()
        .chain(type_rules(field.field_type));
    for rule in rules {
        let result = rule(domain, condition, &field, model)?;
        if result != *domain {
            return Ok(result);
        }
    }
    Ok(domain.clone())
}

fn split_field_expr(field_expr: &str) -> Result<(&str, Option<&str>), ErrorReason> {
    let (name, property) = match field_expr.split_once('.') {
        Some((name, property)) => (name, (!property.is_empty()).then_some(property)),
        None => (field_expr, None),
    };
    if name.is_empty() {
        return Err(ErrorReason::InvalidFieldExpression(py_str_repr(name)));
    }
    Ok((name, property))
}

fn condition_field<'a>(
    condition: &Condition,
    model: &ModelContext<'a>,
) -> Result<FieldInfo<'a>, ErrorReason> {
    let (name, _) = split_field_expr(condition.field_expr())?;
    model.field_of(name, || condition.error_repr())
}

/// The same condition when nothing changed, otherwise a new one with the same field expression.
fn rebuild(domain: &Domain, condition: &Condition, operator: Operator, value: Value) -> Domain {
    if operator == condition.operator() && value.strict_eq(condition.value()) {
        domain.clone()
    } else {
        Domain::raw_condition(condition.field_expr_arc().clone(), operator, value)
    }
}

fn set_of_false() -> Value {
    Value::Set(std::iter::once(Value::FALSE).collect())
}

/// The first principal parameter in a condition value (the value itself or a collection member).
fn first_param(value: &Value) -> Option<&PrincipalParam> {
    match value {
        Value::Param(param) => Some(param),
        Value::List(items) => items.iter().find_map(first_param),
        Value::Set(set) => set.iter().find_map(first_param),
        _ => None,
    }
}

/// Refuse a condition whose value holds a principal parameter: the rewrite needs a literal.
fn refuse_param(condition: &Condition, needs: &'static str) -> Result<(), ErrorReason> {
    match first_param(condition.value()) {
        Some(param) => Err(condition.error(ConditionError::ParamNotAllowed {
            param: param.symbol(),
            needs,
        })),
        None => Ok(()),
    }
}

/// Refuse every principal parameter in a date/datetime comparison except "now" of `kind`.
fn refuse_param_except_now(
    condition: &Condition,
    kind: NowKind,
    needs: &'static str,
) -> Result<(), ErrorReason> {
    let check = |value: &Value| match value {
        Value::Param(PrincipalParam::Now(k)) if *k == kind => Ok(()),
        Value::Param(param) => Err(condition.error(ConditionError::ParamNotAllowed {
            param: param.symbol(),
            needs,
        })),
        _ => Ok(()),
    };
    match condition.value() {
        Value::List(items) => items.iter().try_for_each(check),
        Value::Set(set) => set.iter().try_for_each(check),
        other => check(other),
    }
}

fn is_positive_any(operator: Operator) -> bool {
    matches!(operator, Operator::Any | Operator::AnyBypass)
}

/// `a =? b`  <=>  `not b or a = b`.
fn rule_eq_if_set(
    _domain: &Domain,
    condition: &Condition,
    _field: &FieldInfo<'_>,
    _model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    // `=?` decides on the value's truthiness, which a parameter only has in SQL
    refuse_param(condition, "'=?' needs a literal value")?;
    if !condition.value().is_truthy() {
        return Ok(Domain::TRUE);
    }
    Ok(Domain::raw_condition(
        condition.field_expr_arc().clone(),
        Operator::Eq,
        condition.value().clone(),
    ))
}

/// `=`/`!=` become `in`/`not in` of a one-value set; a collection is used as the set (an empty
/// one means `[False]`).
fn rule_eq_as_in(
    _domain: &Domain,
    condition: &Condition,
    _field: &FieldInfo<'_>,
    _model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let operator = if condition.operator() == Operator::Eq {
        Operator::In
    } else {
        Operator::NotIn
    };
    let value = match condition.value() {
        Value::List(items) if items.is_empty() => set_of_false(),
        Value::Set(set) if set.is_empty() => set_of_false(),
        Value::List(items) => Value::Set(items.iter().cloned().collect()),
        Value::Set(set) => Value::Set(set.clone()),
        Value::Opaque(opaque) if opaque.kind() == OpaqueKind::Sql => condition.value().clone(),
        other => Value::Set(std::iter::once(other.clone()).collect()),
    };
    Ok(Domain::raw_condition(
        condition.field_expr_arc().clone(),
        operator,
        value,
    ))
}

/// The value of `in`/`not in` becomes a non-empty ordered set; an empty value is a constant, a
/// domain or query value switches to `any`/`not any`.
fn rule_in_set(
    domain: &Domain,
    condition: &Condition,
    _field: &FieldInfo<'_>,
    _model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let operator = condition.operator();
    let field_expr = condition.field_expr_arc().clone();
    Ok(match condition.value() {
        Value::Set(set) if !set.is_empty() => domain.clone(),
        // a parameter is a member (a collection parameter contributes its members)
        Value::Param(_) => Domain::raw_condition(
            field_expr,
            operator,
            Value::Set(std::iter::once(condition.value().clone()).collect()),
        ),
        Value::Opaque(_) | Value::Domain(_) => {
            let any = if operator == Operator::In {
                Operator::Any
            } else {
                Operator::NotAny
            };
            Domain::raw_condition(field_expr, any, condition.value().clone())
        }
        value if !value.is_truthy() => Domain::constant(operator == Operator::NotIn),
        Value::List(items) => Domain::raw_condition(
            field_expr,
            operator,
            Value::Set(items.iter().cloned().collect()),
        ),
        value => Domain::raw_condition(
            field_expr,
            operator,
            Value::Set(std::iter::once(value.clone()).collect()),
        ),
    })
}

/// `False` is removed from the set for `id` and for required fields whose column is `NOT NULL`,
/// unless the model holds new records.
fn rule_in_required(
    domain: &Domain,
    condition: &Condition,
    field: &FieldInfo<'_>,
    model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let applies = !field.field_type.has_falsy_value()
        && (field.not_null || field.name == "id")
        && !model.has_new_records();
    let Value::Set(set) = condition.value() else {
        return Ok(domain.clone());
    };
    // Large id sets are common here: only rebuild when there is a False to drop.
    if !applies || !set.iter().any(Value::is_false_literal) {
        return Ok(domain.clone());
    }
    let kept: ValueSet = set
        .iter()
        .filter(|v| !v.is_false_literal())
        .cloned()
        .collect();
    Ok(Domain::raw_condition(
        condition.field_expr_arc().clone(),
        condition.operator(),
        Value::Set(kept),
    ))
}

/// The value of an `any`-family condition becomes a domain; a query or SQL value forces the
/// bypass form; `id any D` is `D` and `id not any D` is `~D`.
fn rule_any_value(
    domain: &Domain,
    condition: &Condition,
    field: &FieldInfo<'_>,
    _model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let operator = condition.operator();
    let value = condition.value();
    if matches!(value, Value::Opaque(_)) {
        return Ok(if matches!(operator, Operator::Any | Operator::NotAny) {
            Domain::raw_condition(
                condition.field_expr_arc().clone(),
                operator.bypass(),
                value.clone(),
            )
        } else {
            domain.clone()
        });
    }
    let sub = Domain::from_value_inner(value)?;
    if field.name == "id" {
        return Ok(if is_positive_any(operator) { sub } else { !sub });
    }
    if let Value::Domain(existing) = value
        && existing.ptr_eq(&sub)
    {
        return Ok(domain.clone());
    }
    Ok(Domain::raw_condition(
        condition.field_expr_arc().clone(),
        operator,
        Value::Domain(sub),
    ))
}

/// The sub-domain of an `any`-family condition is optimized against the comodel; a `FALSE`
/// sub-domain makes the condition constant.
fn rule_any_sub_domain(
    domain: &Domain,
    condition: &Condition,
    field: &FieldInfo<'_>,
    model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let Value::Domain(sub) = condition.value() else {
        return Ok(domain.clone());
    };
    if !field.field_type.is_relational() {
        return Err(condition.error(ConditionError::AnyOnNonRelational));
    }
    let comodel = field
        .comodel
        .filter(|name| model.schema().has_model(name))
        .ok_or_else(|| condition.error(ConditionError::UnknownComodel))?;
    let optimized = optimize_basic(sub, &model.comodel(comodel))?;
    if optimized.is_false() {
        return Ok(Domain::constant(!is_positive_any(condition.operator())));
    }
    if optimized.ptr_eq(sub) || optimized == *sub {
        return Ok(domain.clone());
    }
    Ok(Domain::raw_condition(
        condition.field_expr_arc().clone(),
        condition.operator(),
        Value::Domain(optimized),
    ))
}

/// Pattern values of the `like` family: an empty pattern is a constant or a "set"/"not set" test,
/// non-strings are converted with `str()` (except for the exact `=like` forms).
fn rule_like_value(
    domain: &Domain,
    condition: &Condition,
    field: &FieldInfo<'_>,
    _model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let operator = condition.operator();
    let value = condition.value();
    refuse_param(condition, "a pattern must be a literal string")?;
    if !value.is_truthy() {
        // `=like ''` matches only the empty string, i.e. "not set" after normalization.
        let result = operator.is_negative() == operator.is_exact_like();
        if field.field_type.is_relational() || operator.is_exact_like() {
            let operator = if result { Operator::Ne } else { Operator::Eq };
            return Ok(Domain::raw_condition(
                condition.field_expr_arc().clone(),
                operator,
                Value::FALSE,
            ));
        }
        return Ok(Domain::constant(result));
    }
    match value {
        Value::Str(_) => Ok(domain.clone()),
        Value::Opaque(opaque) if opaque.kind() == OpaqueKind::Sql => {
            Err(condition.error(ConditionError::LikeWithSql))
        }
        _ if operator.is_exact_like() => Err(condition.error(ConditionError::PatternNotString)),
        other => Ok(Domain::raw_condition(
            condition.field_expr_arc().clone(),
            operator,
            Value::Str(other.py_str()),
        )),
    }
}

/// Relational fields compared to strings search the comodel's `display_name`.
fn rule_relational_name_search(
    domain: &Domain,
    condition: &Condition,
    _field: &FieldInfo<'_>,
    _model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let operator = condition.operator();
    let value = condition.value();
    let positive = operator.positive();
    let any = if positive == operator {
        Operator::Any
    } else {
        Operator::NotAny
    };
    let field_expr = condition.field_expr_arc();
    if operator.is_like() {
        let sub = Domain::raw_condition(Arc::from("display_name"), positive, value.clone());
        return Ok(Domain::raw_condition(
            field_expr.clone(),
            any,
            Value::Domain(sub),
        ));
    }
    if operator.is_inequality() && matches!(value, Value::Str(_)) {
        return Err(condition.error(ConditionError::RelationalInequalityWithString));
    }
    if positive != Operator::In {
        return Ok(domain.clone());
    }
    let Some(items) = value.collection_items() else {
        return Ok(domain.clone());
    };
    let (strings, others): (Vec<Value>, Vec<Value>) =
        items.cloned().partition(|v| matches!(v, Value::Str(_)));
    if strings.is_empty() {
        return Ok(domain.clone());
    }
    let sub = Domain::raw_condition(Arc::from("display_name"), positive, Value::List(strings));
    let names = Domain::raw_condition(field_expr.clone(), any, Value::Domain(sub));
    if others.is_empty() {
        return Ok(names);
    }
    let ids = Domain::raw_condition(field_expr.clone(), operator, Value::List(others));
    Ok(if positive == operator {
        names | ids
    } else {
        names & ids
    })
}

/// Python `str2bool(s.lower(), False)`.
fn str_to_bool(s: &str) -> bool {
    matches!(
        s.to_lowercase().as_str(),
        "y" | "yes" | "1" | "true" | "t" | "on"
    )
}

/// Boolean fields accept only `in`/`not in` a collection; non-boolean members are parsed; a
/// single `False` flips the operator so that the value is always `[True]`.
fn rule_boolean_in(
    domain: &Domain,
    condition: &Condition,
    _field: &FieldInfo<'_>,
    _model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let operator = condition.operator();
    let value = condition.value();
    refuse_param(condition, "a boolean field compares with literal booleans")?;
    let items: Vec<&Value> = match (operator, value.collection_items()) {
        (Operator::In | Operator::NotIn, Some(items)) => items.collect(),
        _ => {
            return Err(condition.error(ConditionError::BooleanNotCollection {
                field: py_str_repr(condition.field_expr()),
                value_type: value.py_type().to_owned(),
            }));
        }
    };
    let (values, changed): (Vec<bool>, bool) = if items.iter().all(|v| matches!(v, Value::Bool(_)))
    {
        (items.iter().map(|v| v.is_truthy()).collect(), false)
    } else {
        // Python builds a set here: duplicates disappear.
        let parsed: ValueSet = items
            .iter()
            .map(|v| {
                Value::Bool(match v {
                    Value::Str(s) => str_to_bool(s),
                    other => other.is_truthy(),
                })
            })
            .collect();
        (parsed.iter().map(Value::is_truthy).collect(), true)
    };
    if values.len() == 1 && !values.iter().any(|b| *b) {
        let Some(inverse) = operator.inverse() else {
            return Ok(domain.clone());
        };
        return Ok(Domain::raw_condition(
            condition.field_expr_arc().clone(),
            inverse,
            Value::List(vec![Value::Bool(true)]),
        ));
    }
    if !changed {
        return Ok(domain.clone());
    }
    Ok(Domain::raw_condition(
        condition.field_expr_arc().clone(),
        operator,
        Value::Set(values.into_iter().map(Value::Bool).collect()),
    ))
}

fn is_comparison(operator: Operator) -> bool {
    matches!(
        operator,
        Operator::In | Operator::NotIn | Operator::Gt | Operator::Lt | Operator::Le | Operator::Ge
    )
}

/// Date fields: values become dates (datetimes truncated, ISO strings parsed); relative strings are
/// validated and kept; `False` with an inequality matches nothing.
fn rule_date_value(
    domain: &Domain,
    condition: &Condition,
    _field: &FieldInfo<'_>,
    _model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let operator = condition.operator();
    if !is_comparison(operator) || condition.field_expr().contains('.') {
        return Ok(domain.clone());
    }
    refuse_param_except_now(
        condition,
        NowKind::Date,
        "a date field compares with context_today()",
    )?;
    let value = value_to_date(condition.value())?;
    if value.is_false_literal() && operator.is_inequality() {
        return Ok(Domain::FALSE);
    }
    Ok(rebuild(domain, condition, operator, value))
}

fn value_to_date(value: &Value) -> Result<Value, ErrorReason> {
    Ok(match value {
        Value::Datetime(dt) => Value::Date(dt.date()),
        // checked by the caller: only "now" as a date gets here
        Value::Date(_) | Value::Bool(false) | Value::Param(PrincipalParam::Now(NowKind::Date)) => {
            value.clone()
        }
        Value::Str(s) => match dates::parse_iso(s) {
            Ok(parsed) => value_to_date(&parsed)?,
            Err(_) => {
                // Not ISO: must be a valid relative date, resolved at DYNAMIC_VALUES.
                dates::validate_relative(s)?;
                value.clone()
            }
        },
        Value::List(_) | Value::Set(_) => {
            let mut set = ValueSet::new();
            for item in value.collection_items().into_iter().flatten() {
                set.insert(value_to_date(item)?);
            }
            Value::Set(set)
        }
        other => return Err(ErrorReason::NotADate(other.error_repr())),
    })
}

/// Converted value and whether every input was a date (dates widen to a whole day).
fn value_to_datetime(value: &Value, tz: &dyn TimeZone) -> Result<(Value, bool), ErrorReason> {
    Ok(match value {
        Value::Bool(false) => (Value::FALSE, true),
        // checked by the caller: only "now" as a timestamp gets here
        Value::Datetime(_) | Value::Param(PrincipalParam::Now(NowKind::Timestamp)) => {
            (value.clone(), false)
        }
        Value::Str(s) => match dates::parse_iso(s) {
            Ok(parsed) => value_to_datetime(&parsed, tz)?,
            Err(_) => (value.clone(), dates::validate_relative(s)?),
        },
        Value::Date(d) => {
            let midnight = d.midnight();
            // Midnight in the user's timezone, except at the ends of the calendar.
            let utc = if matches!(d.year(), 1 | 9999) {
                midnight
            } else {
                tz.local_to_utc(midnight).ok_or(ErrorReason::DateOverflow)?
            };
            (Value::Datetime(utc), true)
        }
        Value::List(_) | Value::Set(_) => {
            let mut set = ValueSet::new();
            let mut all_dates = true;
            let mut any = false;
            for item in value.collection_items().into_iter().flatten() {
                let (converted, is_date) = value_to_datetime(item, tz)?;
                set.insert(converted);
                all_dates &= is_date;
                any = true;
            }
            if !any {
                return Err(ErrorReason::NotADatetime(value.error_repr()));
            }
            (Value::Set(set), all_dates)
        }
        other => return Err(ErrorReason::NotADatetime(other.error_repr())),
    })
}

fn truncate_micros(dt: PrimitiveDateTime) -> PrimitiveDateTime {
    dt.replace_microsecond(0).unwrap_or(dt)
}

/// Datetime fields: values become naive UTC datetimes compared to the whole second; dates mean
/// midnight in the user's timezone and widen to the whole day for `>`/`<=`.
fn rule_datetime_value(
    domain: &Domain,
    condition: &Condition,
    _field: &FieldInfo<'_>,
    model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    let mut operator = condition.operator();
    if !is_comparison(operator) || condition.field_expr().contains('.') {
        return Ok(domain.clone());
    }
    let field_expr = condition.field_expr_arc();
    refuse_param_except_now(
        condition,
        NowKind::Timestamp,
        "a datetime field compares with datetime.datetime.now()",
    )?;
    let (mut value, is_date) = value_to_datetime(condition.value(), model.tz())?;

    if operator.is_inequality() {
        let dt = match &value {
            Value::Bool(false) => return Ok(Domain::FALSE),
            Value::Datetime(dt) => truncate_micros(*dt),
            _ => return Ok(domain.clone()),
        };
        let delta = if is_date {
            Duration::days(1)
        } else {
            Duration::seconds(1)
        };
        let dt = match operator {
            Operator::Gt => match dt.checked_add(delta) {
                Some(next) => {
                    operator = Operator::Ge;
                    next
                }
                // Beyond the largest datetime: nothing is greater.
                None => return Ok(Domain::FALSE),
            },
            Operator::Le => match dt.checked_add(delta) {
                Some(next) => {
                    operator = Operator::Lt;
                    next
                }
                // Everything is lower: only check that the field is set.
                None => {
                    return Ok(Domain::raw_condition(
                        field_expr.clone(),
                        Operator::Ne,
                        Value::FALSE,
                    ));
                }
            },
            _ => dt,
        };
        value = Value::Datetime(dt);
    }

    if matches!(operator, Operator::In | Operator::NotIn)
        && let Some(items) = value.collection_items()
    {
        let items: Vec<&Value> = items.collect();
        if items.iter().any(|v| matches!(v, Value::Datetime(_))) {
            // Equality to the whole second: v <= field < v + 1s, per value.
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                parts.push(match item {
                    Value::Datetime(dt) => {
                        let start = truncate_micros(*dt);
                        let end = start
                            .checked_add(Duration::seconds(1))
                            .ok_or(ErrorReason::DateOverflow)?;
                        Domain::raw_condition(
                            field_expr.clone(),
                            Operator::Ge,
                            Value::Datetime(start),
                        ) & Domain::raw_condition(
                            field_expr.clone(),
                            Operator::Lt,
                            Value::Datetime(end),
                        )
                    }
                    other => Domain::raw_condition(field_expr.clone(), Operator::Eq, other.clone()),
                });
            }
            let ranges = Domain::or(parts);
            return Ok(if operator == Operator::NotIn {
                !ranges
            } else {
                ranges
            });
        }
    }
    Ok(rebuild(domain, condition, operator, value))
}

// ----- n-ary nodes ----------------------------------------------------------------------------

fn nary_step(
    domain: &Domain,
    kind: NaryKind,
    children: &[Domain],
    model: &ModelContext<'_>,
) -> Result<Domain, ErrorReason> {
    // Children are optimized lazily: after an absorbing constant the rest is not looked at.
    let mut current = flatten(kind, children.iter().map(|c| optimize_basic(c, model)))?;
    let size = current.len();
    if size > 1 {
        current.sort_by(compare_sort_keys);
        let mut merged = false;
        for merge in MERGES {
            current = merge(kind, current, model)?;
            if current.len() < size {
                merged = true;
                break;
            }
        }
        if !merged
            && current.len() == children.len()
            && current.iter().zip(children).all(|(a, b)| a.ptr_eq(b))
        {
            return Ok(domain.clone());
        }
    }
    Ok(Domain::apply(kind, current))
}

fn sort_key(domain: &Domain) -> (&str, &str, &str) {
    match domain.kind() {
        DomainKind::Condition(condition) => {
            let operator = condition.operator();
            let positive = operator.positive();
            let group = match positive {
                Operator::In => "0in",
                Operator::Any => "1any",
                Operator::AnyBypass => "2any",
                p if p.is_like() => "like",
                p => p.as_str(),
            };
            (condition.field_expr(), group, operator.as_str())
        }
        DomainKind::Not(_) => ("~", "", "!"),
        DomainKind::And(_) => ("~", "", "&"),
        DomainKind::Or(_) => ("~", "", "|"),
        DomainKind::Custom(_) => ("~", "~", "DomainCustom"),
        DomainKind::True | DomainKind::False => ("~", "~", "DomainBool"),
    }
}

fn compare_sort_keys(a: &Domain, b: &Domain) -> Ordering {
    sort_key(a).cmp(&sort_key(b))
}

type Merge = fn(NaryKind, Vec<Domain>, &ModelContext<'_>) -> Result<Vec<Domain>, ErrorReason>;

const MERGES: [Merge; 8] = [
    merge_in_not_in,
    merge_x2many_in,
    merge_x2many_not_in,
    merge_any_bypass,
    merge_any,
    merge_not_any_bypass,
    merge_not_any,
    merge_same,
];

type BlockMerge = fn(NaryKind, &[Domain], &FieldInfo<'_>) -> Result<Vec<Domain>, ErrorReason>;

const RELATIONAL: &[FieldType] = &[
    FieldType::Many2one,
    FieldType::One2many,
    FieldType::Many2many,
];
const X2MANY: &[FieldType] = &[FieldType::Many2many, FieldType::One2many];

fn merge_runs(
    kind: NaryKind,
    domains: Vec<Domain>,
    model: &ModelContext<'_>,
    operators: &[Operator],
    field_types: Option<&[FieldType]>,
    merge: BlockMerge,
) -> Result<Vec<Domain>, ErrorReason> {
    let mut result: Option<Vec<Domain>> = None;
    let mut block: Option<usize> = None;
    for index in 0..=domains.len() {
        let current = domains.get(index);
        let matching = current
            .and_then(Domain::as_condition)
            .filter(|c| operators.contains(&c.operator()));
        if let Some(start) = block {
            let start_condition = domains.get(start).and_then(Domain::as_condition);
            let same_field = match (matching, start_condition) {
                (Some(c), Some(s)) => {
                    c.field_expr() == s.field_expr()
                        && domains.get(start).is_some_and(|first| {
                            current.is_some_and(|node| first.same_origin(node))
                        })
                }
                _ => false,
            };
            if !same_field {
                let run = domains.get(start..index).unwrap_or_default();
                let mut replaced = false;
                if let (true, Some(first)) = (run.len() > 1, start_condition) {
                    let field = condition_field(first, model)?;
                    if field_types.is_none_or(|types| types.contains(&field.field_type)) {
                        let merged = merge(kind, run, &field)?;
                        let merged = merged.into_iter().map(|node| node.inherit_origin(&run[0]));
                        result
                            .get_or_insert_with(|| {
                                domains.get(..start).unwrap_or_default().to_vec()
                            })
                            .extend(merged);
                        replaced = true;
                    }
                }
                if !replaced && let Some(result) = result.as_mut() {
                    result.extend_from_slice(run);
                }
                block = None;
            }
        }
        let Some(current) = current else {
            break;
        };
        if matching.is_some() {
            if block.is_none() {
                block = Some(index);
            }
        } else if let Some(result) = result.as_mut() {
            result.push(current.clone());
        }
    }
    Ok(result.unwrap_or(domains))
}

fn merge_set_conditions(kind: NaryKind, run: &[Domain]) -> Vec<Domain> {
    let mut in_sets: Vec<&ValueSet> = Vec::new();
    let mut not_in_sets: Vec<&ValueSet> = Vec::new();
    let mut field_expr = None;
    for domain in run {
        let Some(condition) = domain.as_condition() else {
            return run.to_vec();
        };
        let Value::Set(set) = condition.value() else {
            return run.to_vec();
        };
        match condition.operator() {
            Operator::In => in_sets.push(set),
            Operator::NotIn => not_in_sets.push(set),
            _ => return run.to_vec(),
        }
        field_expr.get_or_insert_with(|| condition.field_expr_arc().clone());
    }
    let Some(field_expr) = field_expr else {
        return run.to_vec();
    };
    // A parameter's value is only known in SQL: intersections and differences cannot be computed
    // on it, so a run holding one merges only into a plain union (`in` under OR, `not in` under
    // AND).
    let has_param = in_sets
        .iter()
        .chain(not_in_sets.iter())
        .any(|set| set.iter().any(Value::has_param));
    if has_param {
        return match kind {
            NaryKind::Or if not_in_sets.is_empty() => vec![Domain::raw_condition(
                field_expr,
                Operator::In,
                Value::Set(union(&in_sets)),
            )],
            NaryKind::And if in_sets.is_empty() => vec![Domain::raw_condition(
                field_expr,
                Operator::NotIn,
                Value::Set(union(&not_in_sets)),
            )],
            _ => run.to_vec(),
        };
    }
    let (operator, set) = match kind {
        NaryKind::And if !in_sets.is_empty() => (
            Operator::In,
            intersection(&in_sets).difference(&union(&not_in_sets)),
        ),
        NaryKind::And => (Operator::NotIn, union(&not_in_sets)),
        NaryKind::Or if !not_in_sets.is_empty() => (
            Operator::NotIn,
            intersection(&not_in_sets).difference(&union(&in_sets)),
        ),
        NaryKind::Or => (Operator::In, union(&in_sets)),
    };
    vec![Domain::raw_condition(field_expr, operator, Value::Set(set))]
}

/// Intersection keeping the order of the first set.
fn intersection(sets: &[&ValueSet]) -> ValueSet {
    let mut iter = sets.iter();
    let Some(first) = iter.next() else {
        return ValueSet::new();
    };
    iter.fold((*first).clone(), |acc, set| acc.intersection(set))
}

/// Union in order of appearance.
fn union(sets: &[&ValueSet]) -> ValueSet {
    sets.iter().flat_map(|set| set.iter().cloned()).collect()
}

fn merge_in_not_in(
    kind: NaryKind,
    domains: Vec<Domain>,
    model: &ModelContext<'_>,
) -> Result<Vec<Domain>, ErrorReason> {
    merge_runs(
        kind,
        domains,
        model,
        &[Operator::In, Operator::NotIn],
        None,
        |kind, run, field| {
            // x2many sets mean "has a line in": they merge by their own rules below.
            if matches!(
                field.field_type,
                FieldType::Many2many | FieldType::One2many | FieldType::Properties
            ) {
                return Ok(run.to_vec());
            }
            Ok(merge_set_conditions(kind, run))
        },
    )
}

fn merge_x2many_in(
    kind: NaryKind,
    domains: Vec<Domain>,
    model: &ModelContext<'_>,
) -> Result<Vec<Domain>, ErrorReason> {
    merge_runs(
        kind,
        domains,
        model,
        &[Operator::In],
        Some(X2MANY),
        |kind, run, _| {
            // "has line 1 and has line 2" is not "has a line in {1} ∩ {2}".
            if kind == NaryKind::And {
                return Ok(run.to_vec());
            }
            Ok(merge_set_conditions(kind, run))
        },
    )
}

fn merge_x2many_not_in(
    kind: NaryKind,
    domains: Vec<Domain>,
    model: &ModelContext<'_>,
) -> Result<Vec<Domain>, ErrorReason> {
    merge_runs(
        kind,
        domains,
        model,
        &[Operator::NotIn],
        Some(X2MANY),
        |kind, run, _| {
            if kind == NaryKind::Or {
                return Ok(run.to_vec());
            }
            Ok(merge_set_conditions(kind, run))
        },
    )
}

/// `a any L | a any R` = `a any (L | R)` for every relation; under AND only for many2one.
/// Conditions whose value is a query or SQL are not merged.
fn merge_any_run(kind: NaryKind, run: &[Domain], field: &FieldInfo<'_>) -> Vec<Domain> {
    merge_sub_domains(run, field, kind == NaryKind::And, kind)
}

/// `a not any L & a not any R` = `a not any (L | R)` for every relation; under OR only for
/// many2one.
fn merge_not_any_run(kind: NaryKind, run: &[Domain], field: &FieldInfo<'_>) -> Vec<Domain> {
    merge_sub_domains(run, field, kind == NaryKind::Or, kind.inverse())
}

fn merge_sub_domains(
    run: &[Domain],
    field: &FieldInfo<'_>,
    many2one_only: bool,
    connective: NaryKind,
) -> Vec<Domain> {
    if many2one_only && field.field_type != FieldType::Many2one {
        return run.to_vec();
    }
    let (with_domain, others): (Vec<&Domain>, Vec<&Domain>) = run.iter().partition(|d| {
        d.as_condition()
            .is_some_and(|c| matches!(c.value(), Value::Domain(_)))
    });
    if with_domain.len() < 2 {
        return run.to_vec();
    }
    let Some(base) = with_domain.first().and_then(|d| d.as_condition()) else {
        return run.to_vec();
    };
    let sub_domains: Vec<Domain> = with_domain
        .iter()
        .filter_map(|d| match d.as_condition().map(Condition::value) {
            Some(Value::Domain(sub)) => Some(sub.clone()),
            _ => None,
        })
        .collect();
    let merged = Domain::raw_condition(
        base.field_expr_arc().clone(),
        base.operator(),
        Value::Domain(Domain::raw_nary(connective, sub_domains)),
    );
    std::iter::once(merged)
        .chain(others.into_iter().cloned())
        .collect()
}

fn merge_any_bypass(
    kind: NaryKind,
    domains: Vec<Domain>,
    model: &ModelContext<'_>,
) -> Result<Vec<Domain>, ErrorReason> {
    merge_runs(
        kind,
        domains,
        model,
        &[Operator::AnyBypass],
        Some(RELATIONAL),
        |k, r, f| Ok(merge_any_run(k, r, f)),
    )
}

fn merge_any(
    kind: NaryKind,
    domains: Vec<Domain>,
    model: &ModelContext<'_>,
) -> Result<Vec<Domain>, ErrorReason> {
    merge_runs(
        kind,
        domains,
        model,
        &[Operator::Any],
        Some(RELATIONAL),
        |k, r, f| Ok(merge_any_run(k, r, f)),
    )
}

fn merge_not_any_bypass(
    kind: NaryKind,
    domains: Vec<Domain>,
    model: &ModelContext<'_>,
) -> Result<Vec<Domain>, ErrorReason> {
    merge_runs(
        kind,
        domains,
        model,
        &[Operator::NotAnyBypass],
        Some(RELATIONAL),
        |k, r, f| Ok(merge_not_any_run(k, r, f)),
    )
}

fn merge_not_any(
    kind: NaryKind,
    domains: Vec<Domain>,
    model: &ModelContext<'_>,
) -> Result<Vec<Domain>, ErrorReason> {
    merge_runs(
        kind,
        domains,
        model,
        &[Operator::NotAny],
        Some(RELATIONAL),
        |k, r, f| Ok(merge_not_any_run(k, r, f)),
    )
}

/// Adjacent identical children are kept once.
fn merge_same(
    _kind: NaryKind,
    domains: Vec<Domain>,
    _model: &ModelContext<'_>,
) -> Result<Vec<Domain>, ErrorReason> {
    if !domains
        .windows(2)
        .any(|pair| pair[0] == pair[1] && pair[0].same_origin_tree(&pair[1]))
    {
        return Ok(domains);
    }
    let mut result: Vec<Domain> = Vec::with_capacity(domains.len());
    for domain in domains {
        if !result
            .last()
            .is_some_and(|previous| *previous == domain && previous.same_origin_tree(&domain))
        {
            result.push(domain);
        }
    }
    Ok(result)
}
