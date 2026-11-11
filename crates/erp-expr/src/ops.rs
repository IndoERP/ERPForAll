//! Operators with Python semantics, checked integer arithmetic and budget charges
//! computed before any result is built (review round 2).

use std::cmp::Ordering;

use crate::ast::{BinaryOp, CompareOp, UnaryOp};
use crate::datetime::{self, DateOrTime, TimeDelta};
use crate::error::{ExprError, ExprResult};
use crate::format;
use crate::limits::Budget;
use crate::num;
use crate::value::{ExprValue, Map, SLOT, Seq};

fn unsupported(op: &str, a: &ExprValue, b: &ExprValue) -> ExprError {
    ExprError::type_error(format!(
        "unsupported operand type(s) for {op}: '{}' and '{}'",
        a.type_name(),
        b.type_name()
    ))
}

/// Integer view of `int` and `bool` operands (a `bool` is an `int` in Python).
fn ints(a: &ExprValue, b: &ExprValue) -> Option<(i64, i64)> {
    Some((a.as_int()?, b.as_int()?))
}

/// Float view when both operands are numbers and at least one is a float.
fn floats(a: &ExprValue, b: &ExprValue) -> Option<(f64, f64)> {
    if a.is_number() && b.is_number() {
        Some((a.as_f64()?, b.as_f64()?))
    } else {
        None
    }
}

fn op_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mult => "*",
        BinaryOp::Div => "/",
        BinaryOp::FloorDiv => "//",
        BinaryOp::Mod => "%",
        BinaryOp::Pow => "** or pow()",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "^",
        BinaryOp::LShift => "<<",
        BinaryOp::RShift => ">>",
    }
}

pub(crate) fn binary(
    op: BinaryOp,
    a: &ExprValue,
    b: &ExprValue,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    match op {
        BinaryOp::Add => add(a, b, budget),
        BinaryOp::Sub => sub(a, b),
        BinaryOp::Mult => mul(a, b, budget),
        BinaryOp::Div => div(a, b),
        BinaryOp::FloorDiv => floor_div(a, b),
        BinaryOp::Mod => modulo(a, b, budget),
        BinaryOp::Pow => pow(a, b),
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => bitwise(op, a, b, budget),
        BinaryOp::LShift | BinaryOp::RShift => {
            let (x, y) = ints(a, b).ok_or_else(|| unsupported(op_symbol(op), a, b))?;
            let r = if op == BinaryOp::LShift {
                num::shl(x, y)?
            } else {
                num::shr(x, y)?
            };
            Ok(ExprValue::Int(r))
        }
    }
}

/// Concatenation of two sequences: the result's deep size is charged before it is built.
fn concat(x: &Seq, y: &Seq, budget: &mut Budget) -> ExprResult<Vec<ExprValue>> {
    let bytes = x
        .meta()
        .bytes
        .saturating_add(y.meta().bytes)
        .saturating_sub(SLOT);
    budget.alloc(bytes)?;
    budget.steps(x.len().saturating_add(y.len()))?;
    let mut items = Vec::with_capacity(x.len() + y.len());
    items.extend_from_slice(x.items());
    items.extend_from_slice(y.items());
    Ok(items)
}

fn add(a: &ExprValue, b: &ExprValue, budget: &mut Budget) -> ExprResult<ExprValue> {
    use ExprValue as V;
    if let Some((x, y)) = ints(a, b) {
        return Ok(V::Int(num::add(x, y)?));
    }
    if let Some((x, y)) = floats(a, b) {
        return Ok(V::Float(x + y));
    }
    Ok(match (a, b) {
        (V::Str(x), V::Str(y)) => {
            let len = x.len().saturating_add(y.len());
            budget.alloc(len)?;
            budget.scan(len)?;
            let mut s = String::with_capacity(len);
            s.push_str(x);
            s.push_str(y);
            V::Str(s)
        }
        (V::Bytes(x), V::Bytes(y)) => {
            let len = x.len().saturating_add(y.len());
            budget.alloc(len)?;
            budget.scan(len)?;
            let mut v = Vec::with_capacity(len);
            v.extend_from_slice(x);
            v.extend_from_slice(y);
            V::Bytes(v)
        }
        (V::List(x), V::List(y)) => V::list(concat(x, y, budget)?),
        (V::Tuple(x), V::Tuple(y)) => V::tuple(concat(x, y, budget)?),
        (V::Dict(_), V::Dict(_)) => return Err(unsupported("+", a, b)),
        (V::Date(d), V::TimeDelta(td)) | (V::TimeDelta(td), V::Date(d)) => {
            V::Date(datetime::date_add(*d, *td)?)
        }
        (V::DateTime(dt), V::TimeDelta(td)) | (V::TimeDelta(td), V::DateTime(dt)) => {
            V::DateTime(datetime::datetime_add(*dt, *td)?)
        }
        (V::TimeDelta(x), V::TimeDelta(y)) => {
            V::TimeDelta(TimeDelta::from_micros(x.total_micros() + y.total_micros())?)
        }
        (V::Date(d), V::RelativeDelta(rd)) | (V::RelativeDelta(rd), V::Date(d)) => {
            match rd.add_to_date(*d)? {
                DateOrTime::Date(d) => V::Date(d),
                DateOrTime::DateTime(dt) => V::DateTime(dt),
            }
        }
        (V::DateTime(dt), V::RelativeDelta(rd)) | (V::RelativeDelta(rd), V::DateTime(dt)) => {
            V::DateTime(rd.add_to_datetime(*dt)?)
        }
        (V::RelativeDelta(x), V::RelativeDelta(y)) => V::RelativeDelta(Box::new(x.plus(y)?)),
        (V::Str(_) | V::List(_) | V::Tuple(_), other) => {
            return Err(ExprError::type_error(format!(
                "can only concatenate {} (not \"{}\") to {}",
                a.type_name(),
                other.type_name(),
                a.type_name()
            )));
        }
        _ => return Err(unsupported("+", a, b)),
    })
}

fn sub(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    use ExprValue as V;
    if let Some((x, y)) = ints(a, b) {
        return Ok(V::Int(num::sub(x, y)?));
    }
    if let Some((x, y)) = floats(a, b) {
        return Ok(V::Float(x - y));
    }
    Ok(match (a, b) {
        (V::Date(d), V::TimeDelta(td)) => {
            let neg = TimeDelta::from_micros(-td.total_micros())?;
            V::Date(datetime::date_add(*d, neg)?)
        }
        (V::DateTime(dt), V::TimeDelta(td)) => {
            let neg = TimeDelta::from_micros(-td.total_micros())?;
            V::DateTime(datetime::datetime_add(*dt, neg)?)
        }
        (V::Date(x), V::Date(y)) => V::TimeDelta(datetime::date_diff(*x, *y)?),
        (V::DateTime(x), V::DateTime(y)) => V::TimeDelta(datetime::datetime_diff(*x, *y)?),
        (V::TimeDelta(x), V::TimeDelta(y)) => {
            V::TimeDelta(TimeDelta::from_micros(x.total_micros() - y.total_micros())?)
        }
        (V::Date(d), V::RelativeDelta(rd)) => match rd.negated()?.add_to_date(*d)? {
            DateOrTime::Date(d) => V::Date(d),
            DateOrTime::DateTime(dt) => V::DateTime(dt),
        },
        (V::DateTime(dt), V::RelativeDelta(rd)) => V::DateTime(rd.negated()?.add_to_datetime(*dt)?),
        (V::RelativeDelta(x), V::RelativeDelta(y)) => {
            V::RelativeDelta(Box::new(x.plus(&y.negated()?)?))
        }
        _ => return Err(unsupported("-", a, b)),
    })
}

/// Repeat count of `seq * n`: non-positive counts give an empty result, as in Python.
fn repeat_count(n: i64) -> usize {
    // Saturates on 32-bit targets; the size check then rejects the result.
    usize::try_from(n.max(0)).unwrap_or(usize::MAX)
}

fn repeat_seq(s: &Seq, n: i64, budget: &mut Budget) -> ExprResult<Vec<ExprValue>> {
    let count = repeat_count(n);
    if count == 0 || s.is_empty() {
        budget.alloc(SLOT)?;
        return Ok(Vec::new());
    }
    let per_copy = s.meta().bytes.saturating_sub(SLOT);
    let bytes = per_copy.saturating_mul(count).saturating_add(SLOT);
    budget.alloc(bytes)?;
    let len = s.len().saturating_mul(count);
    budget.steps(len)?;
    let mut items = Vec::with_capacity(len);
    for _ in 0..count {
        items.extend_from_slice(s.items());
    }
    Ok(items)
}

fn mul(a: &ExprValue, b: &ExprValue, budget: &mut Budget) -> ExprResult<ExprValue> {
    use ExprValue as V;
    if let Some((x, y)) = ints(a, b) {
        return Ok(V::Int(num::mul(x, y)?));
    }
    if let Some((x, y)) = floats(a, b) {
        return Ok(V::Float(x * y));
    }
    let (seq, n) = match (a, b) {
        (V::TimeDelta(td), f) | (f, V::TimeDelta(td)) if f.is_number() => {
            let micros = match f {
                V::Float(x) => datetime::unit_micros(*x, td.total_micros())?,
                V::Int(i) => td
                    .total_micros()
                    .checked_mul(i128::from(*i))
                    .ok_or_else(datetime::date_range_error)?,
                V::Bool(b) => td.total_micros() * i128::from(*b),
                other => return Err(unsupported("*", a, other)),
            };
            return Ok(V::TimeDelta(TimeDelta::from_micros(micros)?));
        }
        (seq, V::Int(n)) | (V::Int(n), seq) => (seq, *n),
        (seq, V::Bool(b)) | (V::Bool(b), seq) => (seq, i64::from(*b)),
        (V::Str(_) | V::Bytes(_) | V::List(_) | V::Tuple(_), other)
        | (other, V::Str(_) | V::Bytes(_) | V::List(_) | V::Tuple(_)) => {
            return Err(ExprError::type_error(format!(
                "can't multiply sequence by non-int of type '{}'",
                other.type_name()
            )));
        }
        _ => return Err(unsupported("*", a, b)),
    };
    let count = repeat_count(n);
    Ok(match seq {
        V::Str(s) => {
            let len = s.len().saturating_mul(count);
            budget.alloc(len)?;
            budget.scan(len)?;
            V::Str(s.repeat(count))
        }
        V::Bytes(s) => {
            let len = s.len().saturating_mul(count);
            budget.alloc(len)?;
            budget.scan(len)?;
            V::Bytes(s.repeat(count))
        }
        V::List(s) => V::list(repeat_seq(s, n, budget)?),
        V::Tuple(s) => V::tuple(repeat_seq(s, n, budget)?),
        _ => return Err(unsupported("*", a, b)),
    })
}

fn div(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    use ExprValue as V;
    if let Some((x, y)) = ints(a, b) {
        if y == 0 {
            return Err(ExprError::zero_division("division by zero"));
        }
        return Ok(V::Float(x as f64 / y as f64));
    }
    if let Some((x, y)) = floats(a, b) {
        if y == 0.0 {
            return Err(ExprError::zero_division("float division by zero"));
        }
        return Ok(V::Float(x / y));
    }
    match (a, b) {
        (V::TimeDelta(x), V::TimeDelta(y)) => {
            if y.total_micros() == 0 {
                return Err(ExprError::zero_division("division by zero"));
            }
            Ok(V::Float(x.total_micros() as f64 / y.total_micros() as f64))
        }
        _ => Err(unsupported("/", a, b)),
    }
}

fn floor_div(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if let Some((x, y)) = ints(a, b) {
        return Ok(ExprValue::Int(num::floor_div(x, y)?));
    }
    if let Some((x, y)) = floats(a, b) {
        return Ok(ExprValue::Float(
            num::float_divmod(x, y, "float floor division by zero")?.0,
        ));
    }
    Err(unsupported("//", a, b))
}

fn modulo(a: &ExprValue, b: &ExprValue, budget: &mut Budget) -> ExprResult<ExprValue> {
    if let ExprValue::Str(template) = a {
        return Ok(ExprValue::Str(format::printf(template, b, budget)?));
    }
    if let Some((x, y)) = ints(a, b) {
        return Ok(ExprValue::Int(num::modulo(x, y)?));
    }
    if let Some((x, y)) = floats(a, b) {
        return Ok(ExprValue::Float(num::float_divmod(x, y, "float modulo")?.1));
    }
    Err(unsupported("%", a, b))
}

fn pow(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if let Some((x, y)) = ints(a, b) {
        if y < 0 {
            return Ok(ExprValue::Float(num::float_pow(x as f64, y as f64)?));
        }
        return Ok(ExprValue::Int(num::pow(x, y)?));
    }
    if let Some((x, y)) = floats(a, b) {
        return Ok(ExprValue::Float(num::float_pow(x, y)?));
    }
    Err(unsupported("** or pow()", a, b))
}

fn bitwise(
    op: BinaryOp,
    a: &ExprValue,
    b: &ExprValue,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    use ExprValue as V;
    match (a, b) {
        (V::Bool(x), V::Bool(y)) => Ok(V::Bool(match op {
            BinaryOp::BitAnd => *x & *y,
            BinaryOp::BitOr => *x | *y,
            _ => *x ^ *y,
        })),
        (V::Object(o), other) if op == BinaryOp::BitOr => o.bit_or(other, budget),
        (V::Dict(x), V::Dict(y)) if op == BinaryOp::BitOr => {
            let bytes = x.meta().bytes.saturating_add(y.meta().bytes);
            budget.alloc(bytes)?;
            budget.steps(x.len().saturating_add(y.len()))?;
            Ok(V::Dict(Map::merged(x, y)))
        }
        _ => {
            let (x, y) = ints(a, b).ok_or_else(|| unsupported(op_symbol(op), a, b))?;
            Ok(V::Int(match op {
                BinaryOp::BitAnd => x & y,
                BinaryOp::BitOr => x | y,
                _ => x ^ y,
            }))
        }
    }
}

pub(crate) fn unary(op: UnaryOp, v: &ExprValue) -> ExprResult<ExprValue> {
    use ExprValue as V;
    let bad = |sym: &str| {
        ExprError::type_error(format!(
            "bad operand type for unary {sym}: '{}'",
            v.type_name()
        ))
    };
    match op {
        UnaryOp::Not => Ok(V::Bool(!v.is_truthy())),
        UnaryOp::USub => match v {
            V::Int(i) => Ok(V::Int(num::neg(*i)?)),
            V::Bool(b) => Ok(V::Int(-i64::from(*b))),
            V::Float(f) => Ok(V::Float(-f)),
            V::TimeDelta(td) => Ok(V::TimeDelta(TimeDelta::from_micros(-td.total_micros())?)),
            V::RelativeDelta(rd) => Ok(V::RelativeDelta(Box::new(rd.negated()?))),
            _ => Err(bad("-")),
        },
        UnaryOp::UAdd => match v {
            V::Int(i) => Ok(V::Int(*i)),
            V::Bool(b) => Ok(V::Int(i64::from(*b))),
            V::Float(f) => Ok(V::Float(*f)),
            V::TimeDelta(_) | V::RelativeDelta(_) => Ok(v.clone()),
            _ => Err(bad("+")),
        },
        UnaryOp::Invert => match v.as_int() {
            Some(i) => Ok(V::Int(!i)),
            None => Err(bad("~")),
        },
    }
}

/// Membership `item in container`; steps are charged for the data scanned.
pub(crate) fn contains(
    container: &ExprValue,
    item: &ExprValue,
    budget: &mut Budget,
) -> ExprResult<bool> {
    use ExprValue as V;
    match container {
        V::Str(s) => match item {
            V::Str(needle) => {
                budget.scan(s.len())?;
                Ok(s.contains(needle.as_str()))
            }
            other => Err(ExprError::type_error(format!(
                "'in <string>' requires string as left operand, not {}",
                other.type_name()
            ))),
        },
        V::Bytes(b) => match item {
            V::Bytes(needle) => {
                // Window comparison costs up to len(haystack) * len(needle) byte compares.
                budget.scan(b.len().saturating_mul(needle.len().max(1)))?;
                Ok(needle.is_empty() || b.windows(needle.len()).any(|w| w == needle.as_slice()))
            }
            V::Int(_) | V::Bool(_) => {
                let i = item
                    .as_int()
                    .ok_or_else(|| ExprError::type_error("byte must be an int"))?;
                let byte = u8::try_from(i)
                    .map_err(|_| ExprError::value_error("byte must be in range(0, 256)"))?;
                budget.scan(b.len())?;
                Ok(b.contains(&byte))
            }
            other => Err(ExprError::type_error(format!(
                "a bytes-like object is required, not '{}'",
                other.type_name()
            ))),
        },
        V::List(s) | V::Tuple(s) => {
            budget.steps(s.meta().nodes)?;
            Ok(s.iter().any(|x| x.py_eq(item)))
        }
        V::Dict(m) => {
            budget.steps(item.meta().nodes)?;
            Ok(m.get(item)?.is_some())
        }
        other => Err(ExprError::type_error(format!(
            "argument of type '{}' is not iterable",
            other.type_name()
        ))),
    }
}

/// One comparison of a (possibly chained) comparison expression.
pub(crate) fn compare(
    op: CompareOp,
    a: &ExprValue,
    b: &ExprValue,
    budget: &mut Budget,
) -> ExprResult<bool> {
    let ordered = |sym: &str, budget: &mut Budget| -> ExprResult<Option<Ordering>> {
        budget.steps(a.meta().nodes.min(b.meta().nodes))?;
        a.py_partial_cmp(b, sym)
    };
    Ok(match op {
        CompareOp::Eq | CompareOp::NotEq => {
            budget.steps(a.meta().nodes.min(b.meta().nodes))?;
            a.py_eq(b) == (op == CompareOp::Eq)
        }
        CompareOp::Lt => ordered("<", budget)? == Some(Ordering::Less),
        CompareOp::LtE => matches!(
            ordered("<=", budget)?,
            Some(Ordering::Less | Ordering::Equal)
        ),
        CompareOp::Gt => ordered(">", budget)? == Some(Ordering::Greater),
        CompareOp::GtE => matches!(
            ordered(">=", budget)?,
            Some(Ordering::Greater | Ordering::Equal)
        ),
        CompareOp::In => contains(b, a, budget)?,
        CompareOp::NotIn => !contains(b, a, budget)?,
        CompareOp::Is | CompareOp::IsNot => {
            // identity only against None, True and False.
            let singleton = |v: &ExprValue| matches!(v, ExprValue::None | ExprValue::Bool(_));
            if !singleton(a) && !singleton(b) {
                return Err(ExprError::type_error(
                    "'is' / 'is not' compare only against None, True or False",
                ));
            }
            let same = match (a, b) {
                (ExprValue::None, ExprValue::None) => true,
                (ExprValue::Bool(x), ExprValue::Bool(y)) => x == y,
                _ => false,
            };
            same == (op == CompareOp::Is)
        }
    })
}
