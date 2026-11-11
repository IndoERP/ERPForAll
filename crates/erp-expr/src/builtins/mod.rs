//! The standard library of the expression language: Python builtins (`str`, `int`, `len`, ...)

pub(crate) mod command;
pub(crate) mod dates;

use std::sync::Arc;

use crate::args::{Sig, optional, required};
use crate::error::{ExprError, ExprResult};
use crate::limits::Budget;
use crate::methods::{Iter, iterable_items};
use crate::num;
use crate::value::{ExprCallable, ExprValue, Kwargs, Map, SLOT};

type BuiltinFn = fn(&[ExprValue], &Kwargs, &mut Budget) -> ExprResult<ExprValue>;

/// A builtin function implemented in this crate.
pub(crate) struct Builtin {
    pub name: &'static str,
    pub f: BuiltinFn,
}

impl std::fmt::Debug for Builtin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<built-in function {}>", self.name)
    }
}

impl ExprCallable for Builtin {
    fn name(&self) -> &str {
        self.name
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        (self.f)(args, kwargs, budget)
    }

    fn self_charging(&self) -> bool {
        true
    }
}

pub(crate) fn builtin(name: &'static str, f: BuiltinFn) -> ExprValue {
    ExprValue::Callable(Arc::new(Builtin { name, f }))
}

/// The Python builtins of the standard context.
pub(crate) fn python_builtins() -> Vec<(&'static str, ExprValue)> {
    vec![
        ("str", builtin("str", py_str)),
        ("repr", builtin("repr", py_repr)),
        ("int", builtin("int", py_int)),
        ("float", builtin("float", py_float)),
        ("bool", builtin("bool", py_bool)),
        ("len", builtin("len", py_len)),
        ("list", builtin("list", py_list)),
        ("tuple", builtin("tuple", py_tuple)),
        ("dict", builtin("dict", py_dict)),
        ("min", builtin("min", py_min)),
        ("max", builtin("max", py_max)),
        ("sum", builtin("sum", py_sum)),
        ("abs", builtin("abs", py_abs)),
        ("round", builtin("round", py_round)),
    ]
}

fn one_optional(name: &'static str, param: &'static [&'static str]) -> Sig {
    Sig {
        name,
        params: param,
        required: 0,
        pos_only: 1,
        max_pos: 1,
    }
}

fn py_repr(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = Sig {
        name: "repr",
        params: &["obj"],
        required: 1,
        pos_only: 1,
        max_pos: 1,
    };
    let slots = sig.bind(args, kwargs)?;
    Ok(ExprValue::Str(
        required(&sig, &slots, 0)?.repr_budgeted(budget)?,
    ))
}

fn py_str(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = one_optional("str", &["object"]);
    let slots = sig.bind(args, kwargs)?;
    match optional(&slots, 0) {
        None => Ok(ExprValue::str("")),
        Some(v) => Ok(ExprValue::Str(v.str_budgeted(budget)?)),
    }
}

/// `int(str)`: Python's literal rules (surrounding whitespace, sign, single underscores between
/// digits); values outside 64 bits are an `OverflowError`.
fn parse_int_literal(s: &str) -> ExprResult<i64> {
    let invalid = || {
        ExprError::value_error(format!(
            "invalid literal for int() with base 10: {}",
            ExprValue::str(s).repr_short()
        ))
    };
    let t = s.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return Err(invalid());
    }
    let mut value: i64 = 0;
    for c in digits.chars().filter(|c| *c != '_') {
        let d = i64::from(c.to_digit(10).ok_or_else(invalid)?);
        value = num::add(num::mul(value, 10)?, if negative { -d } else { d })?;
    }
    Ok(value)
}

fn py_int(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    if let Some(key) = kwargs.keys().next() {
        return Err(ExprError::type_error(if key == "base" {
            "int() with a base is not supported in expressions".to_string()
        } else {
            format!("'{key}' is an invalid keyword argument for int()")
        }));
    }
    let sig = one_optional("int", &["x"]);
    let slots = sig.bind(args, kwargs)?;
    let Some(v) = optional(&slots, 0) else {
        return Ok(ExprValue::Int(0));
    };
    match v {
        ExprValue::Int(i) => Ok(ExprValue::Int(*i)),
        ExprValue::Bool(b) => Ok(ExprValue::Int(i64::from(*b))),
        ExprValue::Float(f) => Ok(ExprValue::Int(num::float_to_int(*f)?)),
        ExprValue::Str(s) => {
            budget.scan(s.len())?;
            Ok(ExprValue::Int(parse_int_literal(s)?))
        }
        ExprValue::Object(o) => o.as_int().map(ExprValue::Int),
        other => Err(ExprError::type_error(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            other.type_name()
        ))),
    }
}

fn parse_float_literal(s: &str) -> ExprResult<f64> {
    let invalid = || {
        ExprError::value_error(format!(
            "could not convert string to float: {}",
            ExprValue::str(s).repr_short()
        ))
    };
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    let special = matches!(unsigned, "inf" | "infinity" | "nan");
    if !special
        && (t.is_empty()
            || t.starts_with('_')
            || t.ends_with('_')
            || t.contains("__")
            || !t
                .chars()
                .all(|c| c.is_ascii_digit() || "+-._eE".contains(c)))
    {
        return Err(invalid());
    }
    let cleaned: String = t.chars().filter(|c| *c != '_').collect();
    cleaned.parse::<f64>().map_err(|_| invalid())
}

fn py_float(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = one_optional("float", &["x"]);
    let slots = sig.bind(args, kwargs)?;
    let Some(v) = optional(&slots, 0) else {
        return Ok(ExprValue::Float(0.0));
    };
    match v {
        ExprValue::Str(s) => {
            budget.scan(s.len())?;
            Ok(ExprValue::Float(parse_float_literal(s)?))
        }
        other => other.as_f64().map(ExprValue::Float).ok_or_else(|| {
            ExprError::type_error(format!(
                "float() argument must be a string or a real number, not '{}'",
                other.type_name()
            ))
        }),
    }
}

fn py_bool(args: &[ExprValue], kwargs: &Kwargs, _budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = one_optional("bool", &["x"]);
    let slots = sig.bind(args, kwargs)?;
    Ok(ExprValue::Bool(
        optional(&slots, 0).is_some_and(ExprValue::is_truthy),
    ))
}

fn py_len(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    if !kwargs.is_empty() {
        return Err(ExprError::type_error("len() takes no keyword arguments"));
    }
    if args.len() != 1 {
        return Err(ExprError::type_error(format!(
            "len() takes exactly one argument ({} given)",
            args.len()
        )));
    }
    let sig = Sig {
        name: "len",
        params: &["obj"],
        required: 1,
        pos_only: 1,
        max_pos: 1,
    };
    let slots = sig.bind(args, kwargs)?;
    let len = match required(&sig, &slots, 0)? {
        ExprValue::Str(s) => {
            budget.scan(s.len())?;
            s.chars().count()
        }
        ExprValue::Bytes(b) => b.len(),
        ExprValue::List(s) | ExprValue::Tuple(s) => s.len(),
        ExprValue::Dict(m) => m.len(),
        other => {
            return Err(ExprError::type_error(format!(
                "object of type '{}' has no len()",
                other.type_name()
            )));
        }
    };
    i64::try_from(len)
        .map(ExprValue::Int)
        .map_err(|_| num::overflow())
}

fn py_list(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = one_optional("list", &["iterable"]);
    let slots = sig.bind(args, kwargs)?;
    let items = match optional(&slots, 0) {
        None => Vec::new(),
        Some(v) => iterable_items(v, budget)?,
    };
    budget.alloc(SLOT)?;
    Ok(ExprValue::list(items))
}

fn py_tuple(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = one_optional("tuple", &["iterable"]);
    let slots = sig.bind(args, kwargs)?;
    let items = match optional(&slots, 0) {
        None => Vec::new(),
        Some(v) => iterable_items(v, budget)?,
    };
    budget.alloc(SLOT)?;
    Ok(ExprValue::tuple(items))
}

fn py_dict(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    if args.len() > 1 {
        return Err(ExprError::type_error(format!(
            "dict expected at most 1 argument, got {}",
            args.len()
        )));
    }
    let mut pairs: Vec<(ExprValue, ExprValue)> = Vec::new();
    match args.first() {
        None => {}
        Some(ExprValue::Dict(m)) => {
            budget.alloc(m.meta().bytes)?;
            budget.steps(m.len())?;
            pairs.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        Some(other) => {
            for (i, item) in iterable_items(other, budget)?.into_iter().enumerate() {
                match item {
                    ExprValue::List(s) | ExprValue::Tuple(s) if s.len() == 2 => {
                        let mut it = s.iter().cloned();
                        if let (Some(k), Some(v)) = (it.next(), it.next()) {
                            pairs.push((k, v));
                        }
                    }
                    other => {
                        return Err(ExprError::value_error(format!(
                            "dictionary update sequence element #{i} has length {}; 2 is required",
                            match &other {
                                ExprValue::List(s) | ExprValue::Tuple(s) => s.len().to_string(),
                                _ => "an invalid".to_string(),
                            }
                        )));
                    }
                }
            }
        }
    }
    for (k, v) in kwargs {
        budget.admit(v)?;
        pairs.push((ExprValue::Str(k.clone()), v.clone()));
    }
    budget.alloc(SLOT)?;
    let d = ExprValue::Dict(Map::from_pairs(pairs)?);
    budget.check_value(&d)?;
    Ok(d)
}

/// Shared by `min` and `max`: `want` is the ordering that replaces the current best.
fn extremum(
    name: &'static str,
    want: std::cmp::Ordering,
    args: &[ExprValue],
    kwargs: &Kwargs,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    let mut default = None;
    for (k, v) in kwargs {
        if k == "default" {
            default = Some(v);
        } else {
            return Err(ExprError::type_error(format!(
                "{name}() got an unexpected keyword argument '{k}'"
            )));
        }
    }
    let items: Vec<ExprValue> = match args {
        [] => {
            return Err(ExprError::type_error(format!(
                "{name} expected at least 1 argument, got 0"
            )));
        }
        [single] => iterable_items(single, budget)?,
        many => {
            if default.is_some() {
                return Err(ExprError::type_error(format!(
                    "Cannot specify a default for {name}() with multiple positional arguments"
                )));
            }
            many.to_vec()
        }
    };
    let op = if want == std::cmp::Ordering::Less {
        "<"
    } else {
        ">"
    };
    let mut it = items.into_iter();
    let Some(mut best) = it.next() else {
        return match default {
            Some(d) => Ok(d.clone()),
            None => Err(ExprError::value_error(format!(
                "{name}() iterable argument is empty"
            ))),
        };
    };
    for item in it {
        budget.tick()?;
        if item.py_partial_cmp(&best, op)? == Some(want) {
            best = item;
        }
    }
    budget.admit(&best)?;
    Ok(best)
}

fn py_min(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    extremum("min", std::cmp::Ordering::Less, args, kwargs, budget)
}

fn py_max(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    extremum("max", std::cmp::Ordering::Greater, args, kwargs, budget)
}

fn py_sum(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = Sig {
        name: "sum",
        params: &["iterable", "start"],
        required: 1,
        pos_only: 1,
        max_pos: 2,
    };
    let slots = sig.bind(args, kwargs)?;
    // Python's default start is 0.
    let start = optional(&slots, 1).cloned().unwrap_or(ExprValue::Int(0));
    match &start {
        ExprValue::Str(_) => {
            return Err(ExprError::type_error(
                "sum() can't sum strings [use ''.join(seq) instead]",
            ));
        }
        ExprValue::Bytes(_) => {
            return Err(ExprError::type_error(
                "sum() can't sum bytes [use b''.join(seq) instead]",
            ));
        }
        _ => {}
    }
    let mut it = Iter::new(required(&sig, &slots, 0)?.clone())?;
    let mut acc = start;
    while let Some(item) = it.next(budget)? {
        budget.tick()?;
        acc = crate::ops::binary(crate::ast::BinaryOp::Add, &acc, &item, budget)?;
    }
    Ok(acc)
}

fn py_abs(args: &[ExprValue], kwargs: &Kwargs, _budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = Sig {
        name: "abs",
        params: &["x"],
        required: 1,
        pos_only: 1,
        max_pos: 1,
    };
    let slots = sig.bind(args, kwargs)?;
    match required(&sig, &slots, 0)? {
        ExprValue::Int(i) => Ok(ExprValue::Int(num::abs(*i)?)),
        ExprValue::Bool(b) => Ok(ExprValue::Int(i64::from(*b))),
        ExprValue::Float(f) => Ok(ExprValue::Float(f.abs())),
        ExprValue::TimeDelta(td) => Ok(ExprValue::TimeDelta(
            crate::datetime::TimeDelta::from_micros(td.total_micros().abs())?,
        )),
        other => Err(ExprError::type_error(format!(
            "bad operand type for abs(): '{}'",
            other.type_name()
        ))),
    }
}

fn py_round(args: &[ExprValue], kwargs: &Kwargs, _budget: &mut Budget) -> ExprResult<ExprValue> {
    let sig = Sig {
        name: "round",
        params: &["number", "ndigits"],
        required: 1,
        pos_only: 0,
        max_pos: 2,
    };
    let slots = sig.bind(args, kwargs)?;
    let number = required(&sig, &slots, 0)?;
    let ndigits = match optional(&slots, 1) {
        None | Some(ExprValue::None) => None,
        Some(v) => Some(v.as_int().ok_or_else(|| {
            ExprError::type_error(format!(
                "'{}' object cannot be interpreted as an integer",
                v.type_name()
            ))
        })?),
    };
    match (number, ndigits) {
        (ExprValue::Float(f), None) => Ok(ExprValue::Int(num::round_float_to_int(*f)?)),
        (ExprValue::Float(f), Some(n)) => Ok(ExprValue::Float(num::round_float(*f, n)?)),
        (v @ (ExprValue::Int(_) | ExprValue::Bool(_)), n) => {
            let i = v
                .as_int()
                .ok_or_else(|| ExprError::type_error("round() of a non-integer"))?;
            Ok(ExprValue::Int(match n {
                None => i,
                Some(n) => num::round_int(i, n)?,
            }))
        }
        (other, _) => Err(ExprError::type_error(format!(
            "type {} doesn't define __round__ method",
            other.type_name()
        ))),
    }
}
