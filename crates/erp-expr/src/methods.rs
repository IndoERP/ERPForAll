//! Attributes, methods, subscripts, slices and iteration of the built-in value types. Every result
//! is charged to the budget before it is built.

use std::sync::Arc;

use crate::args::{Sig, int_arg, optional, required, str_arg};
use crate::datetime::{self, make_date, make_time};
use crate::error::{ExprError, ExprResult};
use crate::format;
use crate::limits::Budget;
use crate::value::{ExprObject, ExprValue, Kwargs, Map, Meta, SLOT, Seq};

fn no_attr(v: &ExprValue, name: &str) -> ExprError {
    ExprError::attribute_error(v.type_name(), name)
}

const STR_METHODS: &[&str] = &[
    "startswith",
    "endswith",
    "split",
    "join",
    "strip",
    "lstrip",
    "rstrip",
    "lower",
    "upper",
    "replace",
    "capitalize",
    "format",
];
const DICT_METHODS: &[&str] = &["get", "keys", "values", "items"];
const DATE_METHODS: &[&str] = &["strftime", "isoformat", "weekday", "isoweekday", "replace"];
const DATETIME_METHODS: &[&str] = &[
    "strftime",
    "isoformat",
    "weekday",
    "isoweekday",
    "replace",
    "date",
];
const TIMEDELTA_METHODS: &[&str] = &["total_seconds"];

fn method_name(v: &ExprValue, name: &str) -> Option<&'static str> {
    let table: &[&'static str] = match v {
        ExprValue::Str(_) => STR_METHODS,
        ExprValue::Dict(_) => DICT_METHODS,
        ExprValue::Date(_) => DATE_METHODS,
        ExprValue::DateTime(_) => DATETIME_METHODS,
        ExprValue::TimeDelta(_) => TIMEDELTA_METHODS,
        _ => &[],
    };
    table.iter().copied().find(|m| *m == name)
}

/// `true` when `name` is a method of the built-in type of `v`.
pub(crate) fn has_method(v: &ExprValue, name: &str) -> bool {
    method_name(v, name).is_some()
}

/// A method bound to its receiver (`'x'.upper` without the call).
#[derive(Debug)]
struct BoundMethod {
    recv: ExprValue,
    name: &'static str,
}

impl ExprObject for BoundMethod {
    fn type_name(&self) -> &str {
        "builtin_function_or_method"
    }

    fn repr(&self) -> String {
        format!(
            "<built-in method {} of {} object>",
            self.name,
            self.recv.type_name()
        )
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        call_method(&self.recv, self.name, args, kwargs, budget)
    }

    fn self_charging(&self) -> bool {
        true
    }

    /// The receiver is reachable through the method, one level down.
    fn meta(&self) -> Meta {
        Meta::wrapping(self.recv.meta())
    }
}

/// `value.name`.
pub(crate) fn get_attr(v: ExprValue, name: &str, budget: &mut Budget) -> ExprResult<ExprValue> {
    use ExprValue as V;
    let int = |i: i64| Ok(V::Int(i));
    match &v {
        V::Object(o) => return o.get_attr(name, budget),
        V::Date(d) => match name {
            "year" => return int(i64::from(d.year())),
            "month" => return int(i64::from(u8::from(d.month()))),
            "day" => return int(i64::from(d.day())),
            _ => {}
        },
        V::DateTime(dt) => match name {
            "year" => return int(i64::from(dt.year())),
            "month" => return int(i64::from(u8::from(dt.month()))),
            "day" => return int(i64::from(dt.day())),
            "hour" => return int(i64::from(dt.hour())),
            "minute" => return int(i64::from(dt.minute())),
            "second" => return int(i64::from(dt.second())),
            "microsecond" => return int(i64::from(dt.microsecond())),
            _ => {}
        },
        V::TimeDelta(td) => match name {
            "days" => return int(td.days()),
            "seconds" => return int(td.seconds()),
            "microseconds" => return int(td.microseconds()),
            _ => {}
        },
        V::RelativeDelta(rd) => {
            let rel = match name {
                "years" => Some(rd.years),
                "months" => Some(rd.months),
                "days" => Some(rd.days),
                "leapdays" => Some(rd.leapdays),
                "hours" => Some(rd.hours),
                "minutes" => Some(rd.minutes),
                "seconds" => Some(rd.seconds),
                "microseconds" => Some(rd.microseconds),
                _ => None,
            };
            if let Some(x) = rel {
                return int(x);
            }
            let abs = match name {
                "year" => Some(rd.year),
                "month" => Some(rd.month),
                "day" => Some(rd.day),
                "hour" => Some(rd.hour),
                "minute" => Some(rd.minute),
                "second" => Some(rd.second),
                "microsecond" => Some(rd.microsecond),
                _ => None,
            };
            if let Some(x) = abs {
                return Ok(x.map_or(V::None, V::Int));
            }
        }
        _ => {}
    }
    match method_name(&v, name) {
        Some(m) => {
            let method = V::Object(Arc::new(BoundMethod { recv: v, name: m }));
            budget.check_value(&method)?;
            budget.alloc(SLOT)?;
            Ok(method)
        }
        None => Err(no_attr(&v, name)),
    }
}

/// `recv.name(*args, **kwargs)` for built-in types, without materializing a bound method.
pub(crate) fn call_method(
    recv: &ExprValue,
    name: &str,
    args: &[ExprValue],
    kwargs: &Kwargs,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    match recv {
        ExprValue::Str(s) => str_method(s, name, args, kwargs, budget),
        ExprValue::Dict(m) => dict_method(m, name, args, kwargs, budget),
        ExprValue::Date(_) | ExprValue::DateTime(_) => {
            date_method(recv, name, args, kwargs, budget)
        }
        ExprValue::TimeDelta(td) if name == "total_seconds" => {
            sig0("total_seconds").bind(args, kwargs)?;
            Ok(ExprValue::Float(td.total_seconds()))
        }
        other => Err(no_attr(other, name)),
    }
}

fn sig0(name: &'static str) -> Sig {
    Sig {
        name,
        params: &[],
        required: 0,
        pos_only: 0,
        max_pos: 0,
    }
}

/// Builds a `str` of a length known in advance, charging it first.
fn new_str(
    len: usize,
    budget: &mut Budget,
    build: impl FnOnce() -> String,
) -> ExprResult<ExprValue> {
    budget.alloc(len)?;
    budget.scan(len)?;
    Ok(ExprValue::Str(build()))
}

/// A list of strings whose total text length and count are known in advance.
fn str_list(
    count: usize,
    text: usize,
    budget: &mut Budget,
    items: impl Iterator<Item = String>,
) -> ExprResult<ExprValue> {
    let bytes = SLOT
        .saturating_add(count.saturating_mul(SLOT))
        .saturating_add(text);
    budget.alloc(bytes)?;
    budget.steps(count)?;
    Ok(ExprValue::list(items.map(ExprValue::Str).collect()))
}

fn prefixes<'v>(fname: &str, v: &'v ExprValue) -> ExprResult<Vec<&'v str>> {
    match v {
        ExprValue::Str(s) => Ok(vec![s.as_str()]),
        ExprValue::Tuple(items) => items
            .iter()
            .map(|item| {
                item.as_str().ok_or_else(|| {
                    ExprError::type_error(format!(
                        "tuple for {fname} must only contain str, not {}",
                        item.type_name()
                    ))
                })
            })
            .collect(),
        other => Err(ExprError::type_error(format!(
            "{fname} first arg must be str or a tuple of str, not {}",
            other.type_name()
        ))),
    }
}

fn str_method(
    s: &str,
    name: &str,
    args: &[ExprValue],
    kwargs: &Kwargs,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    match name {
        "startswith" | "endswith" => {
            let sig = Sig {
                name: if name == "startswith" {
                    "startswith"
                } else {
                    "endswith"
                },
                params: &["prefix"],
                required: 1,
                pos_only: 1,
                max_pos: 1,
            };
            let slots = sig.bind(args, kwargs)?;
            let ps = prefixes(sig.name, required(&sig, &slots, 0)?)?;
            budget.steps(ps.len())?;
            let hit = if name == "startswith" {
                ps.iter().any(|p| s.starts_with(p))
            } else {
                ps.iter().any(|p| s.ends_with(p))
            };
            Ok(ExprValue::Bool(hit))
        }
        "split" => {
            let sig = Sig {
                name: "split",
                params: &["sep", "maxsplit"],
                required: 0,
                pos_only: 0,
                max_pos: 2,
            };
            let slots = sig.bind(args, kwargs)?;
            let sep = match optional(&slots, 0) {
                None | Some(ExprValue::None) => None,
                Some(v) => Some(str_arg("split", "sep", v)?),
            };
            let maxsplit = match optional(&slots, 1) {
                None => -1,
                Some(v) => int_arg("split", "maxsplit", v)?,
            };
            budget.scan(s.len())?;
            split(s, sep, maxsplit, budget)
        }
        "join" => {
            let sig = Sig {
                name: "join",
                params: &["iterable"],
                required: 1,
                pos_only: 1,
                max_pos: 1,
            };
            let slots = sig.bind(args, kwargs)?;
            let items = iterable_items(required(&sig, &slots, 0)?, budget)?;
            let mut pieces: Vec<&str> = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                pieces.push(item.as_str().ok_or_else(|| {
                    ExprError::type_error(format!(
                        "sequence item {i}: expected str instance, {} found",
                        item.type_name()
                    ))
                })?);
            }
            let len = s
                .len()
                .saturating_mul(pieces.len().saturating_sub(1))
                .saturating_add(pieces.iter().map(|p| p.len()).sum::<usize>());
            new_str(len, budget, || pieces.join(s))
        }
        "strip" | "lstrip" | "rstrip" => {
            let sig = Sig {
                name: match name {
                    "strip" => "strip",
                    "lstrip" => "lstrip",
                    _ => "rstrip",
                },
                params: &["chars"],
                required: 0,
                pos_only: 1,
                max_pos: 1,
            };
            let slots = sig.bind(args, kwargs)?;
            let chars: Option<std::collections::HashSet<char>> = match optional(&slots, 0) {
                None | Some(ExprValue::None) => None,
                Some(v) => {
                    let set = str_arg(sig.name, "chars", v)?;
                    budget.scan(set.len())?;
                    Some(set.chars().collect())
                }
            };
            budget.scan(s.len())?;
            let pred = |c: char| match &chars {
                Some(set) => set.contains(&c),
                None => c.is_whitespace(),
            };
            let out = match name {
                "strip" => s.trim_matches(pred),
                "lstrip" => s.trim_start_matches(pred),
                _ => s.trim_end_matches(pred),
            };
            new_str(out.len(), budget, || out.to_string())
        }
        "lower" | "upper" => {
            sig0(if name == "lower" { "lower" } else { "upper" }).bind(args, kwargs)?;
            budget.scan(s.len())?;
            let len: usize = if name == "lower" {
                s.chars()
                    .map(|c| c.to_lowercase().map(char::len_utf8).sum::<usize>())
                    .sum()
            } else {
                s.chars()
                    .map(|c| c.to_uppercase().map(char::len_utf8).sum::<usize>())
                    .sum()
            };
            new_str(len, budget, || {
                if name == "lower" {
                    s.to_lowercase()
                } else {
                    s.to_uppercase()
                }
            })
        }
        "capitalize" => {
            sig0("capitalize").bind(args, kwargs)?;
            budget.scan(s.len())?;
            let mut chars = s.chars();
            // An empty string capitalizes to an empty string.
            let first: String = chars
                .next()
                .map(|c| c.to_uppercase().collect())
                .unwrap_or_default();
            let rest = chars.as_str().to_lowercase();
            let len = first.len().saturating_add(rest.len());
            new_str(len, budget, || format!("{first}{rest}"))
        }
        "replace" => {
            let sig = Sig {
                name: "replace",
                params: &["old", "new", "count"],
                required: 2,
                pos_only: 2,
                max_pos: 3,
            };
            let slots = sig.bind(args, kwargs)?;
            let old = str_arg("replace", "old", required(&sig, &slots, 0)?)?;
            let new = str_arg("replace", "new", required(&sig, &slots, 1)?)?;
            let count = match optional(&slots, 2) {
                None => -1,
                Some(v) => int_arg("replace", "count", v)?,
            };
            budget.scan(s.len())?;
            let found = if old.is_empty() {
                s.chars().count().saturating_add(1)
            } else {
                s.matches(old).count()
            };
            let n = if count < 0 {
                found
            } else {
                found.min(usize::try_from(count).unwrap_or(usize::MAX))
            };
            let len = s
                .len()
                .saturating_sub(n.saturating_mul(old.len()))
                .saturating_add(n.saturating_mul(new.len()));
            new_str(len, budget, || s.replacen(old, new, n))
        }
        "format" => Ok(ExprValue::Str(format::str_format(s, args, kwargs, budget)?)),
        _ => Err(ExprError::attribute_error("str", name)),
    }
}

/// Python `str.split`.
fn split(s: &str, sep: Option<&str>, maxsplit: i64, budget: &mut Budget) -> ExprResult<ExprValue> {
    let limit = usize::try_from(maxsplit).ok();
    match sep {
        Some("") => Err(ExprError::value_error("empty separator")),
        Some(sep) => {
            let pieces = || -> Box<dyn Iterator<Item = &str>> {
                match limit {
                    Some(n) => Box::new(s.splitn(n.saturating_add(1), sep)),
                    None => Box::new(s.split(sep)),
                }
            };
            let count = pieces().count();
            str_list(count, s.len(), budget, pieces().map(str::to_string))
        }
        None => {
            // Runs of whitespace separate; leading/trailing whitespace yields no empty pieces.
            let mut parts: Vec<(usize, usize)> = Vec::new();
            let mut rest_start = None;
            let mut iter = s.char_indices().peekable();
            while let Some(&(i, c)) = iter.peek() {
                if c.is_whitespace() {
                    iter.next();
                    continue;
                }
                if limit.is_some_and(|n| parts.len() >= n) {
                    rest_start = Some(i);
                    break;
                }
                let start = i;
                let mut end = s.len();
                while let Some(&(j, d)) = iter.peek() {
                    if d.is_whitespace() {
                        end = j;
                        break;
                    }
                    iter.next();
                }
                parts.push((start, end));
            }
            // Every bound comes from `char_indices`, so the slices are on character boundaries.
            let mut owned: Vec<&str> = parts.iter().filter_map(|&(a, b)| s.get(a..b)).collect();
            if let Some(start) = rest_start
                && let Some(rest) = s.get(start..)
            {
                owned.push(rest);
            }
            str_list(
                owned.len(),
                s.len(),
                budget,
                owned.into_iter().map(str::to_string),
            )
        }
    }
}

fn dict_method(
    m: &Map,
    name: &str,
    args: &[ExprValue],
    kwargs: &Kwargs,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    match name {
        "get" => {
            let sig = Sig {
                name: "get",
                params: &["key", "default"],
                required: 1,
                pos_only: 2,
                max_pos: 2,
            };
            let slots = sig.bind(args, kwargs)?;
            let key = required(&sig, &slots, 0)?;
            let found = match m.get(key)? {
                Some(v) => v.clone(),
                None => optional(&slots, 1).cloned().unwrap_or(ExprValue::None),
            };
            budget.admit(&found)?;
            Ok(found)
        }
        "keys" | "values" | "items" => {
            sig0(match name {
                "keys" => "keys",
                "values" => "values",
                _ => "items",
            })
            .bind(args, kwargs)?;
            let mut bytes = SLOT;
            for (k, v) in m.iter() {
                let add = match name {
                    "keys" => k.deep_size(),
                    "values" => v.deep_size(),
                    _ => SLOT
                        .saturating_add(k.deep_size())
                        .saturating_add(v.deep_size()),
                };
                bytes = bytes.saturating_add(add);
            }
            budget.alloc(bytes)?;
            budget.steps(m.len())?;
            let items: Vec<ExprValue> = match name {
                "keys" => m.keys().cloned().collect(),
                "values" => m.values().cloned().collect(),
                _ => m
                    .iter()
                    .map(|(k, v)| ExprValue::tuple(vec![k.clone(), v.clone()]))
                    .collect(),
            };
            Ok(ExprValue::list(items))
        }
        _ => Err(ExprError::attribute_error("dict", name)),
    }
}

fn date_method(
    recv: &ExprValue,
    name: &str,
    args: &[ExprValue],
    kwargs: &Kwargs,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    let (date, time, is_dt) = match recv {
        ExprValue::Date(d) => (*d, time::Time::MIDNIGHT, false),
        ExprValue::DateTime(dt) => (dt.date(), dt.time(), true),
        other => return Err(no_attr(other, name)),
    };
    match name {
        "strftime" => {
            let sig = Sig {
                name: "strftime",
                params: &["format"],
                required: 1,
                pos_only: 0,
                max_pos: 1,
            };
            let slots = sig.bind(args, kwargs)?;
            let fmt = str_arg("strftime", "1", required(&sig, &slots, 0)?)?;
            Ok(ExprValue::Str(datetime::strftime(date, time, fmt, budget)?))
        }
        "isoformat" => {
            let s = if is_dt {
                let sig = Sig {
                    name: "isoformat",
                    params: &["sep"],
                    required: 0,
                    pos_only: 0,
                    max_pos: 1,
                };
                let slots = sig.bind(args, kwargs)?;
                let sep = match optional(&slots, 0) {
                    None => 'T',
                    Some(v) => {
                        let s = str_arg("isoformat", "sep", v)?;
                        let mut cs = s.chars();
                        match (cs.next(), cs.next()) {
                            (Some(c), None) => c,
                            _ => {
                                return Err(ExprError::type_error(
                                    "isoformat() argument 'sep' must be a single character",
                                ));
                            }
                        }
                    }
                };
                datetime::datetime_iso(time::PrimitiveDateTime::new(date, time), sep)
            } else {
                sig0("isoformat").bind(args, kwargs)?;
                datetime::date_iso(date)
            };
            new_str(s.len(), budget, || s)
        }
        "weekday" => {
            sig0("weekday").bind(args, kwargs)?;
            Ok(ExprValue::Int(i64::from(
                date.weekday().number_days_from_monday(),
            )))
        }
        "isoweekday" => {
            sig0("isoweekday").bind(args, kwargs)?;
            Ok(ExprValue::Int(i64::from(
                date.weekday().number_from_monday(),
            )))
        }
        "date" if is_dt => {
            sig0("date").bind(args, kwargs)?;
            Ok(ExprValue::Date(date))
        }
        "replace" => {
            let sig = Sig {
                name: "replace",
                params: if is_dt {
                    &[
                        "year",
                        "month",
                        "day",
                        "hour",
                        "minute",
                        "second",
                        "microsecond",
                    ]
                } else {
                    &["year", "month", "day"]
                },
                required: 0,
                pos_only: 0,
                max_pos: if is_dt { 7 } else { 3 },
            };
            let slots = sig.bind(args, kwargs)?;
            let field = |i: usize, current: i64, pname: &str| -> ExprResult<i64> {
                match optional(&slots, i) {
                    None => Ok(current),
                    Some(v) => int_arg("replace", pname, v),
                }
            };
            let d = make_date(
                field(0, i64::from(date.year()), "year")?,
                field(1, i64::from(u8::from(date.month())), "month")?,
                field(2, i64::from(date.day()), "day")?,
            )?;
            if !is_dt {
                return Ok(ExprValue::Date(d));
            }
            let t = make_time(
                field(3, i64::from(time.hour()), "hour")?,
                field(4, i64::from(time.minute()), "minute")?,
                field(5, i64::from(time.second()), "second")?,
                field(6, i64::from(time.microsecond()), "microsecond")?,
            )?;
            Ok(ExprValue::DateTime(time::PrimitiveDateTime::new(d, t)))
        }
        _ => Err(no_attr(recv, name)),
    }
}

// ---------------------------------------------------------------------------------------------
// Iteration, subscripts and slices
// ---------------------------------------------------------------------------------------------

/// Lazy iteration over an iterable value; each produced item is charged when it is produced.
pub(crate) enum Iter {
    Seq(Seq, usize),
    Chars(String, usize),
    Keys(Map, usize),
    Bytes(Vec<u8>, usize),
}

impl Iter {
    pub(crate) fn new(v: ExprValue) -> ExprResult<Self> {
        match v {
            ExprValue::List(s) | ExprValue::Tuple(s) => Ok(Iter::Seq(s, 0)),
            ExprValue::Str(s) => Ok(Iter::Chars(s, 0)),
            ExprValue::Dict(m) => Ok(Iter::Keys(m, 0)),
            ExprValue::Bytes(b) => Ok(Iter::Bytes(b, 0)),
            other => Err(ExprError::type_error(format!(
                "'{}' object is not iterable",
                other.type_name()
            ))),
        }
    }

    pub(crate) fn next(&mut self, budget: &mut Budget) -> ExprResult<Option<ExprValue>> {
        let item = match self {
            Iter::Seq(s, i) => {
                let item = s.items().get(*i).cloned();
                *i += 1;
                item
            }
            Iter::Chars(s, pos) => {
                let c = s.get(*pos..).and_then(|rest| rest.chars().next());
                if let Some(c) = c {
                    *pos += c.len_utf8();
                }
                c.map(|c| ExprValue::Str(c.to_string()))
            }
            Iter::Keys(m, i) => {
                let item = m.key_at(*i).cloned();
                *i += 1;
                item
            }
            Iter::Bytes(b, i) => {
                let item = b.get(*i).map(|&byte| ExprValue::Int(i64::from(byte)));
                *i += 1;
                item
            }
        };
        match item {
            Some(v) => {
                budget.admit(&v)?;
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }
}

/// All items of an iterable, charged.
pub(crate) fn iterable_items(v: &ExprValue, budget: &mut Budget) -> ExprResult<Vec<ExprValue>> {
    let mut it = Iter::new(v.clone())?;
    let mut out = Vec::new();
    while let Some(item) = it.next(budget)? {
        out.push(item);
    }
    Ok(out)
}

fn normalize_index(i: i64, len: usize) -> Option<usize> {
    let len_i = i64::try_from(len).ok()?;
    let idx = if i < 0 { i.checked_add(len_i)? } else { i };
    usize::try_from(idx).ok().filter(|&u| u < len)
}

/// `target[key]` for built-in types (host objects are handled by the evaluator).
pub(crate) fn get_item(
    target: &ExprValue,
    key: &ExprValue,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    use ExprValue as V;
    let int_key = |kind: &str| {
        key.as_int().ok_or_else(|| {
            ExprError::type_error(format!(
                "{kind} indices must be integers or slices, not {}",
                key.type_name()
            ))
        })
    };
    let value = match target {
        V::List(s) | V::Tuple(s) => {
            let kind = if matches!(target, V::List(_)) {
                "list"
            } else {
                "tuple"
            };
            let i = int_key(kind)?;
            normalize_index(i, s.len())
                .and_then(|u| s.items().get(u))
                .cloned()
                .ok_or_else(|| ExprError::index_error(format!("{kind} index out of range")))?
        }
        V::Str(s) => {
            let i = key.as_int().ok_or_else(|| {
                ExprError::type_error(format!(
                    "string indices must be integers, not '{}'",
                    key.type_name()
                ))
            })?;
            budget.scan(s.len())?;
            let c = if s.is_ascii() {
                normalize_index(i, s.len())
                    .and_then(|u| s.get(u..=u))
                    .map(str::to_string)
            } else {
                let count = s.chars().count();
                normalize_index(i, count)
                    .and_then(|u| s.chars().nth(u))
                    .map(|c| c.to_string())
            };
            V::Str(c.ok_or_else(|| ExprError::index_error("string index out of range"))?)
        }
        V::Bytes(b) => {
            let i = int_key("byte")?;
            let byte = normalize_index(i, b.len())
                .and_then(|u| b.get(u))
                .ok_or_else(|| ExprError::index_error("index out of range"))?;
            V::Int(i64::from(*byte))
        }
        V::Dict(m) => m
            .get(key)?
            .cloned()
            .ok_or_else(|| ExprError::key_error(key.repr_short()))?,
        other => {
            return Err(ExprError::type_error(format!(
                "'{}' object is not subscriptable",
                other.type_name()
            )));
        }
    };
    budget.admit(&value)?;
    Ok(value)
}

/// Python slice bounds: `(start, step, count)` with all arithmetic in `i128` (no overflow for any
/// `i64` bounds or step, review round 2 blocking 2).
pub(crate) fn slice_plan(
    len: usize,
    lower: Option<i64>,
    upper: Option<i64>,
    step: Option<i64>,
) -> ExprResult<(i128, i128, usize)> {
    let step = i128::from(step.unwrap_or(1));
    if step == 0 {
        return Err(ExprError::value_error("slice step cannot be zero"));
    }
    let len = i128::try_from(len).map_err(|_| ExprError::overflow("sequence too long"))?;
    let clamp = |v: Option<i64>, default: i128, lo: i128, hi: i128| -> i128 {
        match v {
            None => default,
            Some(x) => {
                let x = i128::from(x);
                let x = if x < 0 { x + len } else { x };
                x.clamp(lo, hi)
            }
        }
    };
    let (start, stop) = if step > 0 {
        (clamp(lower, 0, 0, len), clamp(upper, len, 0, len))
    } else {
        (
            clamp(lower, len - 1, -1, len - 1),
            clamp(upper, -1, -1, len - 1),
        )
    };
    let count = if step > 0 && start < stop {
        (stop - start - 1) / step + 1
    } else if step < 0 && start > stop {
        (start - stop - 1) / (-step) + 1
    } else {
        0
    };
    // 0 <= count <= len, so the conversion cannot fail.
    Ok((start, step, usize::try_from(count).unwrap_or(0)))
}

/// Indices of a slice; `slice_plan` guarantees each lies in `0..len`.
pub(crate) fn slice_indices(start: i128, step: i128, count: usize) -> impl Iterator<Item = usize> {
    (0..count).filter_map(move |k| usize::try_from(start + step * k as i128).ok())
}

/// `target[lower:upper:step]`.
pub(crate) fn get_slice(
    target: &ExprValue,
    lower: Option<i64>,
    upper: Option<i64>,
    step: Option<i64>,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    use ExprValue as V;
    match target {
        V::List(s) | V::Tuple(s) => {
            let (start, step, count) = slice_plan(s.len(), lower, upper, step)?;
            budget.steps(count)?;
            let mut bytes = SLOT;
            for i in slice_indices(start, step, count) {
                if let Some(item) = s.items().get(i) {
                    bytes = bytes.saturating_add(item.deep_size());
                }
            }
            budget.alloc(bytes)?;
            let items: Vec<ExprValue> = slice_indices(start, step, count)
                .filter_map(|i| s.items().get(i).cloned())
                .collect();
            Ok(if matches!(target, V::List(_)) {
                V::list(items)
            } else {
                V::tuple(items)
            })
        }
        V::Str(s) => {
            budget.scan(s.len())?;
            if s.is_ascii() {
                let bytes = s.as_bytes();
                let (start, step, count) = slice_plan(bytes.len(), lower, upper, step)?;
                budget.alloc(count)?;
                let out: String = slice_indices(start, step, count)
                    .filter_map(|i| bytes.get(i).map(|&b| char::from(b)))
                    .collect();
                Ok(V::Str(out))
            } else {
                let chars: Vec<char> = s.chars().collect();
                let (start, step, count) = slice_plan(chars.len(), lower, upper, step)?;
                let len: usize = slice_indices(start, step, count)
                    .filter_map(|i| chars.get(i).map(|c| c.len_utf8()))
                    .sum();
                budget.alloc(len)?;
                let out: String = slice_indices(start, step, count)
                    .filter_map(|i| chars.get(i).copied())
                    .collect();
                Ok(V::Str(out))
            }
        }
        V::Bytes(b) => {
            let (start, step, count) = slice_plan(b.len(), lower, upper, step)?;
            budget.scan(count)?;
            budget.alloc(count)?;
            Ok(V::Bytes(
                slice_indices(start, step, count)
                    .filter_map(|i| b.get(i).copied())
                    .collect(),
            ))
        }
        V::Object(o) => o.get_slice(lower, upper, step, budget),
        other => Err(ExprError::type_error(format!(
            "'{}' object is not subscriptable",
            other.type_name()
        ))),
    }
}
