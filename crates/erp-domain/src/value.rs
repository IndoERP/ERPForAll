//! Literal values of domain conditions.
//!
//! [`Value`] is a minimal dynamic value owned by this crate.
//!
//! Hashing is
//! consistent with that equality. Domains compare their values strictly instead (same variant at
//! every level, see [`crate::Domain`]).

use std::any::Any;
use std::fmt::{self, Write as _};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use indexmap::IndexSet;
use time::{Date, PrimitiveDateTime};

use crate::dates;
use crate::domain::{Domain, MAX_DEPTH};
use crate::error::{DomainError, ErrorReason};
use crate::param::PrincipalParam;

/// A literal value in a domain condition.
#[derive(Clone)]
pub enum Value {
    /// A boolean.
    Bool(bool),
    /// An integer (record ids included).
    Int(i64),
    /// A floating-point number.
    Float(f64),
    /// A string.
    Str(String),
    /// A calendar date.
    Date(Date),
    /// A naive UTC datetime (microsecond precision, as Python).
    Datetime(PrimitiveDateTime),
    /// A list or tuple, kept as given until optimization normalizes it.
    List(Vec<Value>),
    Set(ValueSet),
    /// A sub-domain (value of `any`/`not any`).
    Domain(Domain),
    /// A value produced by another layer that this crate does not interpret, such as a compiled
    /// query or raw SQL (see [`Opaque`]).
    Opaque(Opaque),
    NewId,
    /// A value of the principal a statement runs for: a leaf of a compiled
    /// access domain, bound per statement in SQL, never decided in memory (see
    /// [`PrincipalParam`]). Only Rust code builds it; no external input produces one.
    Param(PrincipalParam),
}

/// The variant of a [`Value`], used to filter conditions by value type
/// ([`crate::Domain::is_condition_matching`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValueKind {
    /// [`Value::Bool`].
    Bool,
    /// [`Value::Int`].
    Int,
    /// [`Value::Float`].
    Float,
    /// [`Value::Str`].
    Str,
    /// [`Value::Date`].
    Date,
    /// [`Value::Datetime`].
    Datetime,
    /// [`Value::List`].
    List,
    /// [`Value::Set`].
    Set,
    /// [`Value::Domain`].
    Domain,
    /// [`Value::Opaque`].
    Opaque,
    /// [`Value::NewId`].
    NewId,
    /// [`Value::Param`].
    Param,
}

impl Value {
    pub const FALSE: Value = Value::Bool(false);

    /// A list value built from any values.
    pub fn list<T: Into<Value>>(items: impl IntoIterator<Item = T>) -> Value {
        Value::List(items.into_iter().map(Into::into).collect())
    }

    /// A set value built from any values (duplicates are dropped, first occurrence kept).
    pub fn set<T: Into<Value>>(items: impl IntoIterator<Item = T>) -> Value {
        Value::Set(items.into_iter().map(Into::into).collect())
    }

    /// The variant of this value.
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Bool(_) => ValueKind::Bool,
            Value::Int(_) => ValueKind::Int,
            Value::Float(_) => ValueKind::Float,
            Value::Str(_) => ValueKind::Str,
            Value::Date(_) => ValueKind::Date,
            Value::Datetime(_) => ValueKind::Datetime,
            Value::List(_) => ValueKind::List,
            Value::Set(_) => ValueKind::Set,
            Value::Domain(_) => ValueKind::Domain,
            Value::Opaque(_) => ValueKind::Opaque,
            Value::NewId => ValueKind::NewId,
            Value::Param(_) => ValueKind::Param,
        }
    }

    /// Python truthiness of the value: `False`, `0`, `0.0`, `''`, empty collections and the `TRUE`
    /// domain are falsy (the last one for backward compatibility with the empty-list domain).
    ///
    /// A [`Value::Param`] counts as truthy: it stands for a value bound per statement, and every
    /// rewrite that would decide on that value's truthiness refuses parameters before asking
    /// (see [`crate::PrincipalParam`]).
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Date(_) | Value::Datetime(_) | Value::Opaque(_) | Value::Param(_) => true,
            Value::List(items) => !items.is_empty(),
            Value::Set(set) => !set.is_empty(),
            Value::Domain(domain) => !domain.is_true(),
            Value::NewId => false,
        }
    }

    /// The items of a collection value, or `None` for other values.
    pub(crate) fn collection_items(&self) -> Option<Box<dyn Iterator<Item = &Value> + '_>> {
        match self {
            Value::List(items) => Some(Box::new(items.iter())),
            Value::Set(set) => Some(Box::new(set.iter())),
            _ => None,
        }
    }

    /// Exact equality: same variant at every level (`1`, `1.0` and `True` differ), sets still
    /// compared without order. Domains compare values this way, so that a rewrite from `{1}` to
    /// `{True}` counts as a change and memoized domains never mix value types.
    pub(crate) fn strict_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Date(a), Value::Date(b)) => a == b,
            (Value::Datetime(a), Value::Datetime(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.strict_eq(y))
            }
            (Value::Set(a), Value::Set(b)) => a.strict_eq(b),
            (Value::Domain(a), Value::Domain(b)) => a == b,
            (Value::Opaque(a), Value::Opaque(b)) => a == b,
            (Value::NewId, Value::NewId) => true,
            (Value::Param(a), Value::Param(b)) => a == b,
            _ => false,
        }
    }

    /// The principal parameter, when this value is one.
    pub fn as_param(&self) -> Option<&PrincipalParam> {
        match self {
            Value::Param(param) => Some(param),
            _ => None,
        }
    }

    /// Whether this value is a principal parameter or a collection holding one.
    pub fn has_param(&self) -> bool {
        match self {
            Value::Param(_) => true,
            Value::List(items) => items.iter().any(Value::has_param),
            Value::Set(set) => set.iter().any(Value::has_param),
            _ => false,
        }
    }

    pub fn is_false_literal(&self) -> bool {
        matches!(self, Value::Bool(false))
    }

    /// Convert a JSON value (JSON-2 surface, external input: errors are
    /// [`crate::ErrorKind::Validation`]). `null` is `False`, integers that fit in an `i64` are
    /// integers, numbers with a fraction or exponent are floats, arrays are lists. Objects,
    /// integers beyond 64 bits (never silently turned into a float) and arrays nested deeper than
    /// [`MAX_DEPTH`] are rejected.
    pub fn from_json(json: &serde_json::Value) -> Result<Value, DomainError> {
        Self::from_json_depth(json, 0).map_err(DomainError::validation)
    }

    pub(crate) fn from_json_depth(
        json: &serde_json::Value,
        depth: usize,
    ) -> Result<Value, ErrorReason> {
        Ok(match json {
            serde_json::Value::Null => Value::FALSE,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if n.is_f64() {
                    match n.as_f64() {
                        Some(f) => Value::Float(f),
                        None => return Err(ErrorReason::UnsupportedValue(clip(n.to_string()))),
                    }
                } else {
                    // An integer outside the i64 range: refuse rather than lose digits.
                    return Err(ErrorReason::UnsupportedValue(clip(n.to_string())));
                }
            }
            serde_json::Value::String(s) => Value::Str(s.clone()),
            serde_json::Value::Array(items) => {
                if depth >= MAX_DEPTH {
                    return Err(ErrorReason::TooDeep(MAX_DEPTH));
                }
                Value::List(
                    items
                        .iter()
                        .map(|item| Value::from_json_depth(item, depth + 1))
                        .collect::<Result<Vec<_>, _>>()?,
                )
            }
            serde_json::Value::Object(_) => {
                return Err(ErrorReason::UnsupportedValue(clip(json.to_string())));
            }
        })
    }

    /// Non-finite floats, opaque values and
    /// new-record references have no JSON form ([`crate::ErrorKind::Programming`] errors).
    pub fn to_json(&self) -> Result<serde_json::Value, DomainError> {
        self.to_json_inner().map_err(DomainError::programming)
    }

    pub(crate) fn to_json_inner(&self) -> Result<serde_json::Value, ErrorReason> {
        Ok(match self {
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::from(*i),
            Value::Float(f) => match serde_json::Number::from_f64(*f) {
                Some(n) => serde_json::Value::Number(n),
                None => return Err(ErrorReason::UnsupportedValue(self.error_repr())),
            },
            Value::Str(s) => serde_json::Value::String(s.clone()),
            Value::Date(d) => serde_json::Value::String(dates::format_date(*d)),
            Value::Datetime(dt) => serde_json::Value::String(dates::format_datetime_server(*dt)),
            Value::List(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(Value::to_json_inner)
                    .collect::<Result<_, _>>()?,
            ),
            Value::Set(set) => serde_json::Value::Array(
                set.iter()
                    .map(Value::to_json_inner)
                    .collect::<Result<_, _>>()?,
            ),
            Value::Domain(domain) => domain.to_json_inner()?,
            Value::Opaque(_) | Value::NewId | Value::Param(_) => {
                return Err(ErrorReason::UnsupportedValue(self.error_repr()));
            }
        })
    }

    pub(crate) fn to_list_form(&self) -> Value {
        match self {
            Value::Set(set) => Value::List(set.iter().cloned().collect()),
            Value::Domain(domain) => Value::List(domain.to_list()),
            other => other.clone(),
        }
    }

    /// Python `repr()` of the value.
    pub(crate) fn py_repr(&self) -> String {
        let mut out = ReprWriter::unlimited();
        self.write_repr(&mut out);
        out.finish()
    }

    /// Python `repr()` cut to about [`ERROR_REPR_LIMIT`] bytes, for error messages.
    pub(crate) fn error_repr(&self) -> String {
        let mut out = ReprWriter::for_errors();
        self.write_repr(&mut out);
        out.finish()
    }

    pub(crate) fn write_repr(&self, out: &mut ReprWriter) {
        if out.is_full() {
            return;
        }
        match self {
            Value::Bool(true) => out.push("True"),
            Value::Bool(false) => out.push("False"),
            Value::Int(i) => out.push(&i.to_string()),
            Value::Float(f) => out.push(&py_float_repr(*f)),
            Value::Str(s) => write_str_repr(out, s),
            Value::Date(d) => out.push(&format!(
                "datetime.date({}, {}, {})",
                d.year(),
                u8::from(d.month()),
                d.day()
            )),
            Value::Datetime(dt) => out.push(&dates::datetime_repr(*dt)),
            Value::List(items) => {
                out.push("[");
                write_items(out, items.iter());
                out.push("]");
            }
            Value::Set(set) => {
                out.push("OrderedSet([");
                write_items(out, set.iter());
                out.push("])");
            }
            Value::Domain(domain) => domain.write_repr_into(out),
            Value::Opaque(opaque) => out.push(&format!("{opaque:?}")),
            Value::NewId => out.push("NewId"),
            Value::Param(param) => out.push(&param.symbol()),
        }
    }

    pub(crate) fn py_str(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Date(d) => dates::format_date(*d),
            Value::Datetime(dt) => dates::format_datetime_str(*dt),
            other => other.py_repr(),
        }
    }

    /// Python `type()` of the value, as printed by `repr(type(v))`.
    pub(crate) fn py_type(&self) -> &'static str {
        match self {
            Value::Bool(_) => "<class 'bool'>",
            Value::Int(_) => "<class 'int'>",
            Value::Float(_) => "<class 'float'>",
            Value::Str(_) => "<class 'str'>",
            Value::Date(_) => "<class 'datetime.date'>",
            Value::Datetime(_) => "<class 'datetime.datetime'>",
            Value::List(_) => "<class 'list'>",
            Value::Set(_) => "<class 'odoo.tools.misc.OrderedSet'>",
            Value::Domain(_) => "<class 'odoo.orm.domains.Domain'>",
            Value::Opaque(opaque) => match opaque.kind() {
                OpaqueKind::Query => "<class 'odoo.orm.query.Query'>",
                OpaqueKind::Sql => "<class 'odoo.tools.sql.SQL'>",
            },
            Value::NewId => "<class 'odoo.orm.identifiers.NewId'>",
            Value::Param(_) => "<class 'erpforall.PrincipalParam'>",
        }
    }
}

/// Byte budget of a value rendered in an error message (before the trailing `…`).
pub(crate) const ERROR_REPR_LIMIT: usize = 200;

/// A `repr` buffer that stops growing at a byte limit, so that error messages stay short and huge
/// values are not rendered in full.
pub(crate) struct ReprWriter {
    buf: String,
    limit: usize,
    truncated: bool,
}

impl ReprWriter {
    pub(crate) fn unlimited() -> Self {
        Self::with_limit(usize::MAX)
    }

    pub(crate) fn for_errors() -> Self {
        Self::with_limit(ERROR_REPR_LIMIT)
    }

    fn with_limit(limit: usize) -> Self {
        ReprWriter {
            buf: String::new(),
            limit,
            truncated: false,
        }
    }

    pub(crate) fn is_full(&self) -> bool {
        self.truncated
    }

    pub(crate) fn push(&mut self, s: &str) {
        if self.truncated {
            return;
        }
        let room = self.limit.saturating_sub(self.buf.len());
        if s.len() <= room {
            self.buf.push_str(s);
        } else {
            let mut cut = room;
            while cut > 0 && !s.is_char_boundary(cut) {
                cut -= 1;
            }
            self.buf.push_str(s.get(..cut).unwrap_or_default());
            self.truncated = true;
        }
    }

    pub(crate) fn finish(mut self) -> String {
        if self.truncated {
            self.buf.push('…');
        }
        self.buf
    }
}

/// Cut an already-rendered text to the error budget.
pub(crate) fn clip(text: String) -> String {
    if text.len() <= ERROR_REPR_LIMIT {
        return text;
    }
    let mut out = ReprWriter::for_errors();
    out.push(&text);
    out.finish()
}

pub(crate) fn write_items<'a>(out: &mut ReprWriter, items: impl Iterator<Item = &'a Value>) {
    for (i, item) in items.enumerate() {
        if out.is_full() {
            return;
        }
        if i > 0 {
            out.push(", ");
        }
        item.write_repr(out);
    }
}

/// Python `repr()` of a string: single quotes unless the string contains a single quote and no
/// double quote.
pub(crate) fn py_str_repr(s: &str) -> String {
    let mut out = ReprWriter::unlimited();
    write_str_repr(&mut out, s);
    out.finish()
}

pub(crate) fn write_str_repr(out: &mut ReprWriter, s: &str) {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut piece = String::with_capacity(8);
    piece.push(quote);
    out.push(&piece);
    for c in s.chars() {
        if out.is_full() {
            return;
        }
        piece.clear();
        match c {
            '\\' => piece.push_str("\\\\"),
            '\n' => piece.push_str("\\n"),
            '\r' => piece.push_str("\\r"),
            '\t' => piece.push_str("\\t"),
            c if c == quote => {
                piece.push('\\');
                piece.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(piece, "\\x{:02x}", c as u32);
            }
            c => piece.push(c),
        }
        out.push(&piece);
    }
    piece.clear();
    piece.push(quote);
    out.push(&piece);
}

/// Python `repr()` of a float (shortest round-trip digits, fixed notation for exponents in
/// `-4..16`, otherwise `1e+16` style).
fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_owned();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    let sci = format!("{f:e}");
    let (mantissa, exponent) = match sci.split_once('e') {
        Some((m, e)) => (m.to_owned(), e.parse::<i32>().unwrap_or(0)),
        None => (sci.clone(), 0),
    };
    if (-4..16).contains(&exponent) {
        let fixed = format!("{f}");
        if fixed.contains('.') {
            fixed
        } else {
            format!("{fixed}.0")
        }
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs())
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.py_repr())
    }
}

/// Displays the value as Python `repr()` would, e.g. `'abc'`, `False`, `[1, 2]`.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.py_repr())
    }
}

/// Numeric key shared by `Bool`, `Int` and `Float` so that `True == 1 == 1.0` as in Python.
#[derive(PartialEq, Eq, Hash)]
enum NumKey {
    Int(i64),
    Float(u64),
}

const I64_BOUND: f64 = 9_223_372_036_854_775_808.0;

fn num_key(value: &Value) -> Option<NumKey> {
    match value {
        Value::Bool(b) => Some(NumKey::Int(i64::from(*b))),
        Value::Int(i) => Some(NumKey::Int(*i)),
        Value::Float(f) => Some(if f.is_nan() {
            NumKey::Float(f64::NAN.to_bits())
        } else if f.fract() == 0.0 && *f >= -I64_BOUND && *f < I64_BOUND {
            // Integral and in range: the conversion is exact.
            NumKey::Int(*f as i64)
        } else {
            NumKey::Float(f.to_bits())
        }),
        _ => None,
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        if let (Some(a), Some(b)) = (num_key(self), num_key(other)) {
            return a == b;
        }
        match (self, other) {
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Date(a), Value::Date(b)) => a == b,
            (Value::Datetime(a), Value::Datetime(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Set(a), Value::Set(b)) => a == b,
            (Value::Domain(a), Value::Domain(b)) => a == b,
            (Value::Opaque(a), Value::Opaque(b)) => a == b,
            (Value::NewId, Value::NewId) => true,
            (Value::Param(a), Value::Param(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if let Some(key) = num_key(self) {
            0u8.hash(state);
            key.hash(state);
            return;
        }
        match self {
            Value::Str(s) => {
                1u8.hash(state);
                s.hash(state);
            }
            Value::Date(d) => {
                2u8.hash(state);
                d.hash(state);
            }
            Value::Datetime(dt) => {
                3u8.hash(state);
                dt.hash(state);
            }
            Value::List(items) => {
                4u8.hash(state);
                items.hash(state);
            }
            Value::Set(set) => {
                5u8.hash(state);
                set.hash(state);
            }
            Value::Domain(domain) => {
                6u8.hash(state);
                domain.hash(state);
            }
            Value::Opaque(opaque) => {
                7u8.hash(state);
                opaque.hash(state);
            }
            Value::NewId => 8u8.hash(state),
            Value::Param(param) => {
                9u8.hash(state);
                param.hash(state);
            }
            // Numeric variants are handled above.
            Value::Bool(_) | Value::Int(_) | Value::Float(_) => {}
        }
    }
}

/// A stand-alone hash of a value, independent of the outer hasher.
pub(crate) fn hash_of<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i64::from(i))
    }
}

impl From<u32> for Value {
    fn from(i: u32) -> Self {
        Value::Int(i64::from(i))
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<Date> for Value {
    fn from(d: Date) -> Self {
        Value::Date(d)
    }
}

impl From<PrimitiveDateTime> for Value {
    fn from(dt: PrimitiveDateTime) -> Self {
        Value::Datetime(dt)
    }
}

impl From<Domain> for Value {
    fn from(d: Domain) -> Self {
        Value::Domain(d)
    }
}

impl From<ValueSet> for Value {
    fn from(set: ValueSet) -> Self {
        Value::Set(set)
    }
}

impl From<Opaque> for Value {
    fn from(opaque: Opaque) -> Self {
        Value::Opaque(opaque)
    }
}

impl From<PrincipalParam> for Value {
    fn from(param: PrincipalParam) -> Self {
        Value::Param(param)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Self {
        Value::list(items)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Value::FALSE, Into::into)
    }
}

/// Equality and hashing ignore the order, like Python sets; iteration keeps the first-insertion
/// order, which decides the order of values in generated SQL. The order-independent hash is kept
/// up to date on every insertion, so hashing a set (and every domain holding it) is O(1).
#[derive(Clone, Default)]
pub struct ValueSet {
    values: IndexSet<Value>,
    /// Wrapping sum and xor of the element hashes.
    sum: u64,
    xor: u64,
}

impl ValueSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Whether the set contains a value (Python equality: `1` matches `True` and `1.0`).
    pub fn contains(&self, value: &Value) -> bool {
        self.values.contains(value)
    }

    /// The values in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Value> {
        self.values.iter()
    }

    /// Insert a value; returns `false` when an equal value was already present.
    pub fn insert(&mut self, value: Value) -> bool {
        let h = hash_of(&value);
        let inserted = self.values.insert(value);
        if inserted {
            self.sum = self.sum.wrapping_add(h);
            self.xor ^= h;
        }
        inserted
    }

    /// Same members with the same variants, in any order.
    pub(crate) fn strict_eq(&self, other: &ValueSet) -> bool {
        // A set never holds two Python-equal values, so the Python-equal candidate is unique.
        self.len() == other.len()
            && self.sum == other.sum
            && self
                .iter()
                .all(|v| other.values.get(v).is_some_and(|w| v.strict_eq(w)))
    }

    /// Values of `self` that are also in `other`, in the order of `self`.
    pub(crate) fn intersection(&self, other: &ValueSet) -> ValueSet {
        self.iter().filter(|v| other.contains(v)).cloned().collect()
    }

    /// Values of `self` that are not in `other`, in the order of `self`.
    pub(crate) fn difference(&self, other: &ValueSet) -> ValueSet {
        self.iter()
            .filter(|v| !other.contains(v))
            .cloned()
            .collect()
    }
}

impl PartialEq for ValueSet {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.sum == other.sum && self.values == other.values
    }
}

impl Eq for ValueSet {}

impl Hash for ValueSet {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Order-independent combination of the element hashes, consistent with set equality.
        self.values.len().hash(state);
        self.sum.hash(state);
        self.xor.hash(state);
    }
}

impl fmt::Debug for ValueSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = ReprWriter::unlimited();
        out.push("OrderedSet([");
        write_items(&mut out, self.iter());
        out.push("])");
        f.write_str(&out.finish())
    }
}

impl FromIterator<Value> for ValueSet {
    fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Self {
        let mut set = ValueSet::new();
        for value in iter {
            set.insert(value);
        }
        set
    }
}

impl<'a> IntoIterator for &'a ValueSet {
    type Item = &'a Value;
    type IntoIter = indexmap::set::Iter<'a, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}

/// What an [`Opaque`] value stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpaqueKind {
    Query,
    Sql,
}

/// A value defined by a higher layer (a compiled query or SQL fragment) carried through a domain
/// without interpretation. Implemented by `erp-sql`.
pub trait ExternalValue: fmt::Debug + Send + Sync + 'static {
    /// What the value stands for; the optimizer treats queries and SQL differently.
    fn kind(&self) -> OpaqueKind;

    /// Access to the concrete type, for the layer that created the value.
    fn as_any(&self) -> &dyn Any;
}

/// A shared [`ExternalValue`].
/// Domains holding opaque values depend on the transaction
/// and must not be memoized across transactions.
#[derive(Clone)]
pub struct Opaque(Arc<dyn ExternalValue>);

impl Opaque {
    /// Wrap an external value.
    pub fn new(value: Arc<dyn ExternalValue>) -> Self {
        Opaque(value)
    }

    /// What the value stands for.
    pub fn kind(&self) -> OpaqueKind {
        self.0.kind()
    }

    /// The wrapped value.
    pub fn get(&self) -> &Arc<dyn ExternalValue> {
        &self.0
    }

    fn addr(&self) -> usize {
        Arc::as_ptr(&self.0).cast::<()>() as usize
    }
}

impl PartialEq for Opaque {
    fn eq(&self, other: &Self) -> bool {
        self.addr() == other.addr()
    }
}

impl Eq for Opaque {}

impl Hash for Opaque {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.addr().hash(state);
    }
}

impl fmt::Debug for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}
