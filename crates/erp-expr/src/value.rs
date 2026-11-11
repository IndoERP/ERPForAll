//! Values with Python semantics: truthiness, `repr`/`str`, equality, ordering, hashing of keys.
//!
//! Containers are immutable and shared (`Arc`), and cache their [`Meta`] (deep size, node count,
//! nesting depth), so budget checks on any value are O(1) (review round 2: "budget is
//! charged in bytes of deep size, before building any value").

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use indexmap::IndexMap;
use time::{Date, PrimitiveDateTime};

use crate::datetime::{self, RelativeDelta, TimeDelta};
use crate::error::{ExprError, ExprResult};
use crate::limits::{Budget, Out};
use crate::num;

/// Length cap of [`ExprValue::repr_short`].
pub const REPR_SHORT_BYTES: usize = 200;

/// Keyword arguments of a call, in source order.
pub type Kwargs = IndexMap<String, ExprValue>;

/// Bytes one value slot occupies inside a container (normative logical accounting).
pub(crate) const SLOT: usize = 32;
/// Payload bytes assigned to a RelativeDelta value (normative logical accounting).
pub(crate) const RELATIVE_DELTA_PAYLOAD: usize = 144;
/// Per-entry overhead of a dict beyond its key and value slots (hash and index).
const DICT_ENTRY_OVERHEAD: usize = 16;

/// Size and shape of a value, cached on containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Meta {
    /// Deep size in bytes: the value's own slot plus everything reachable from it.
    pub bytes: usize,
    /// Number of values reachable, the value itself included.
    pub nodes: usize,
    /// Nesting depth: 0 for scalars, 1 + the deepest item for containers.
    pub depth: usize,
}

impl Meta {
    /// Meta of a value holding nothing but `heap` bytes of its own.
    pub fn scalar(heap: usize) -> Self {
        Meta {
            bytes: SLOT.saturating_add(heap),
            nodes: 1,
            depth: 0,
        }
    }

    /// Meta of a value wrapping `inner` one level deeper (a container of one item, a bound method).
    pub fn wrapping(inner: Meta) -> Self {
        Meta {
            bytes: SLOT.saturating_add(inner.bytes),
            nodes: inner.nodes.saturating_add(1),
            depth: inner.depth.saturating_add(1),
        }
    }
}

/// An object provided by the host (a record, a model proxy). Implementations must be `Send + Sync`
/// so an evaluation context can be shared by worker threads.
///
/// Every method has a failing default, so a host only implements what it exposes; an attribute the
/// object does not expose must be an `AttributeError`, never an invented value.
pub trait ExprObject: Send + Sync + fmt::Debug {
    /// Python type name used in error messages (`"res.partner"`).
    fn type_name(&self) -> &str;

    fn repr(&self) -> String {
        format!("<{} object>", self.type_name())
    }

    fn is_truthy(&self) -> bool {
        true
    }

    fn get_attr(&self, name: &str, _budget: &mut Budget) -> ExprResult<ExprValue> {
        Err(ExprError::attribute_error(self.type_name(), name))
    }

    fn call(
        &self,
        _args: &[ExprValue],
        _kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        Err(ExprError::type_error(format!(
            "'{}' object is not callable",
            self.type_name()
        )))
    }

    fn get_item(&self, _key: &ExprValue, _budget: &mut Budget) -> ExprResult<ExprValue> {
        Err(ExprError::type_error(format!(
            "'{}' object is not subscriptable",
            self.type_name()
        )))
    }

    /// Converts this object to an integer (Python `__int__`).
    fn as_int(&self) -> ExprResult<i64> {
        Err(ExprError::type_error(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            self.type_name()
        )))
    }

    /// Whether this object represents a database record or recordset.
    fn is_record(&self) -> bool {
        false
    }

    /// The database ids of a record, recordset or model proxy (an empty slice for the latter),
    /// `None` for any other object.
    fn record_ids(&self) -> Option<std::borrow::Cow<'_, [i64]>> {
        None
    }

    /// Attaches host hooks to this object, returning a clone with the hooks populated.
    fn with_hooks(&self, _hooks: Arc<crate::proxy::HostHooks>) -> Option<Arc<dyn ExprObject>> {
        None
    }

    /// Slices this object `[lower:upper:step]`.
    fn get_slice(
        &self,
        _lower: Option<i64>,
        _upper: Option<i64>,
        _step: Option<i64>,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        Err(ExprError::type_error(format!(
            "'{}' object is not subscriptable",
            self.type_name()
        )))
    }

    fn bit_or(&self, other: &ExprValue, _budget: &mut Budget) -> ExprResult<ExprValue> {
        Err(ExprError::type_error(format!(
            "unsupported operand type(s) for |: '{}' and '{}'",
            self.type_name(),
            other.type_name()
        )))
    }

    /// Python `==` against another host object; the same object is always equal to itself.
    fn py_eq(&self, _other: &dyn ExprObject) -> bool {
        false
    }

    /// `true` when the object charges the values it returns to the budget itself. Host objects
    /// leave this `false`: the evaluator then charges their results' deep size.
    fn self_charging(&self) -> bool {
        false
    }

    /// Size and nesting of the values the object holds and can hand back to expressions. An
    /// object that keeps expression values (a bound method keeps its receiver) must report them,
    /// so that depth and size limits see through it. The default is an opaque handle.
    fn meta(&self) -> Meta {
        Meta::scalar(0)
    }
}

/// A function provided by the host or by the standard library. `Send + Sync` for the same reason
/// as [`ExprObject`].
pub trait ExprCallable: Send + Sync + fmt::Debug {
    fn name(&self) -> &str {
        "function"
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        budget: &mut Budget,
    ) -> ExprResult<ExprValue>;

    /// See [`ExprObject::self_charging`].
    fn self_charging(&self) -> bool {
        false
    }
}

/// An immutable, shared sequence (the payload of `list` and `tuple`).
#[derive(Clone)]
pub struct Seq(Arc<SeqData>);

struct SeqData {
    items: Vec<ExprValue>,
    meta: Meta,
}

impl Seq {
    pub fn new(items: Vec<ExprValue>) -> Self {
        let mut meta = Meta {
            bytes: SLOT,
            nodes: 1,
            depth: 1,
        };
        for item in &items {
            let m = item.meta();
            meta.bytes = meta.bytes.saturating_add(m.bytes);
            meta.nodes = meta.nodes.saturating_add(m.nodes);
            meta.depth = meta.depth.max(m.depth.saturating_add(1));
        }
        Seq(Arc::new(SeqData { items, meta }))
    }

    pub fn items(&self) -> &[ExprValue] {
        &self.0.items
    }

    pub fn len(&self) -> usize {
        self.0.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.items.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ExprValue> {
        self.0.items.iter()
    }

    pub(crate) fn meta(&self) -> Meta {
        self.0.meta
    }

    pub(crate) fn ptr_eq(&self, other: &Seq) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for Seq {
    type Target = [ExprValue];
    fn deref(&self) -> &[ExprValue] {
        &self.0.items
    }
}

/// A dict key: a hashable value, hashed and compared with Python semantics (`1`, `1.0` and `True`
/// are the same key).
#[derive(Clone)]
struct HKey(ExprValue);

struct HRef<'a>(&'a ExprValue);

struct HStr<'a>(&'a str);

const TAG_NONE: u8 = 0;
const TAG_NUM: u8 = 1;
const TAG_FLOAT: u8 = 2;
const TAG_STR: u8 = 3;
const TAG_BYTES: u8 = 4;
const TAG_DATE: u8 = 5;
const TAG_DATETIME: u8 = 6;
const TAG_TIMEDELTA: u8 = 7;
const TAG_TUPLE: u8 = 8;

fn hash_value<H: Hasher>(v: &ExprValue, h: &mut H) {
    match v {
        ExprValue::None => TAG_NONE.hash(h),
        ExprValue::Bool(b) => {
            TAG_NUM.hash(h);
            i64::from(*b).hash(h);
        }
        ExprValue::Int(i) => {
            TAG_NUM.hash(h);
            i.hash(h);
        }
        ExprValue::Float(f) => match num::float_as_int(*f) {
            Some(i) => {
                TAG_NUM.hash(h);
                i.hash(h);
            }
            None => {
                TAG_FLOAT.hash(h);
                if f.is_nan() {
                    u64::MAX.hash(h);
                } else {
                    f.to_bits().hash(h);
                }
            }
        },
        ExprValue::Str(s) => {
            TAG_STR.hash(h);
            s.as_str().hash(h);
        }
        ExprValue::Bytes(b) => {
            TAG_BYTES.hash(h);
            b.hash(h);
        }
        ExprValue::Date(d) => {
            TAG_DATE.hash(h);
            d.to_julian_day().hash(h);
        }
        ExprValue::DateTime(dt) => {
            TAG_DATETIME.hash(h);
            dt.date().to_julian_day().hash(h);
            dt.time().as_hms_nano().hash(h);
        }
        ExprValue::TimeDelta(td) => {
            TAG_TIMEDELTA.hash(h);
            td.hash(h);
        }
        ExprValue::Tuple(items) => {
            TAG_TUPLE.hash(h);
            items.len().hash(h);
            for item in items.iter() {
                hash_value(item, h);
            }
        }
        // Never inserted: `is_hashable` rejects these before insertion.
        ExprValue::RelativeDelta(_)
        | ExprValue::List(_)
        | ExprValue::Dict(_)
        | ExprValue::Object(_)
        | ExprValue::Callable(_) => u8::MAX.hash(h),
    }
}

impl Hash for HKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        hash_value(&self.0, h);
    }
}

impl PartialEq for HKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.py_eq(&other.0)
    }
}

impl Eq for HKey {}

impl Hash for HRef<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        hash_value(self.0, h);
    }
}

impl indexmap::Equivalent<HKey> for HRef<'_> {
    fn equivalent(&self, key: &HKey) -> bool {
        self.0.py_eq(&key.0)
    }
}

impl Hash for HStr<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        TAG_STR.hash(h);
        self.0.hash(h);
    }
}

impl indexmap::Equivalent<HKey> for HStr<'_> {
    fn equivalent(&self, key: &HKey) -> bool {
        matches!(&key.0, ExprValue::Str(s) if s == self.0)
    }
}

fn is_hashable(v: &ExprValue) -> bool {
    match v {
        ExprValue::None
        | ExprValue::Bool(_)
        | ExprValue::Int(_)
        | ExprValue::Float(_)
        | ExprValue::Str(_)
        | ExprValue::Bytes(_)
        | ExprValue::Date(_)
        | ExprValue::DateTime(_)
        | ExprValue::TimeDelta(_) => true,
        ExprValue::Tuple(items) => items.iter().all(is_hashable),
        ExprValue::RelativeDelta(_)
        | ExprValue::List(_)
        | ExprValue::Dict(_)
        | ExprValue::Object(_)
        | ExprValue::Callable(_) => false,
    }
}

pub(crate) fn unhashable(v: &ExprValue) -> ExprError {
    ExprError::type_error(format!("unhashable type: '{}'", v.type_name()))
}

/// An immutable, shared, insertion-ordered dict.
#[derive(Clone)]
pub struct Map(Arc<MapData>);

struct MapData {
    entries: IndexMap<HKey, ExprValue>,
    meta: Meta,
}

impl Map {
    /// Builds a dict from pairs. A repeated key keeps its first position and takes the last value,
    /// as in Python. Unhashable keys are a `TypeError`.
    pub fn from_pairs(pairs: Vec<(ExprValue, ExprValue)>) -> ExprResult<Self> {
        let mut entries = IndexMap::with_capacity(pairs.len());
        for (k, v) in pairs {
            if !is_hashable(&k) {
                return Err(unhashable(&k));
            }
            entries.insert(HKey(k), v);
        }
        Ok(Self::from_entries(entries))
    }

    fn from_entries(entries: IndexMap<HKey, ExprValue>) -> Self {
        let mut meta = Meta {
            bytes: SLOT,
            nodes: 1,
            depth: 1,
        };
        for (k, v) in &entries {
            let km = k.0.meta();
            let vm = v.meta();
            meta.bytes = meta
                .bytes
                .saturating_add(km.bytes)
                .saturating_add(vm.bytes)
                .saturating_add(DICT_ENTRY_OVERHEAD);
            meta.nodes = meta.nodes.saturating_add(km.nodes).saturating_add(vm.nodes);
            meta.depth = meta
                .depth
                .max(km.depth.saturating_add(1))
                .max(vm.depth.saturating_add(1));
        }
        Map(Arc::new(MapData { entries, meta }))
    }

    pub fn empty() -> Self {
        Self::from_entries(IndexMap::new())
    }

    /// Value for `key`; an unhashable key is a `TypeError` (as in Python), never "not found".
    pub fn get(&self, key: &ExprValue) -> ExprResult<Option<&ExprValue>> {
        if !is_hashable(key) {
            return Err(unhashable(key));
        }
        Ok(self.0.entries.get(&HRef(key)))
    }

    pub fn get_str(&self, key: &str) -> Option<&ExprValue> {
        self.0.entries.get(&HStr(key))
    }

    pub fn len(&self) -> usize {
        self.0.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ExprValue, &ExprValue)> {
        self.0.entries.iter().map(|(k, v)| (&k.0, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &ExprValue> {
        self.0.entries.keys().map(|k| &k.0)
    }

    pub fn values(&self) -> impl Iterator<Item = &ExprValue> {
        self.0.entries.values()
    }

    pub(crate) fn key_at(&self, i: usize) -> Option<&ExprValue> {
        self.0.entries.get_index(i).map(|(k, _)| &k.0)
    }

    pub(crate) fn meta(&self) -> Meta {
        self.0.meta
    }

    /// `self | other`: a new dict with `other`'s entries overriding.
    pub(crate) fn merged(&self, other: &Map) -> Map {
        let mut entries = self.0.entries.clone();
        for (k, v) in &other.0.entries {
            entries.insert(k.clone(), v.clone());
        }
        Self::from_entries(entries)
    }

    fn ptr_eq(&self, other: &Map) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A value of the expression language.
#[derive(Clone)]
#[non_exhaustive]
pub enum ExprValue {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Date(Date),
    DateTime(PrimitiveDateTime),
    TimeDelta(TimeDelta),
    RelativeDelta(Box<RelativeDelta>),
    List(Seq),
    Tuple(Seq),
    Dict(Map),
    Object(Arc<dyn ExprObject>),
    Callable(Arc<dyn ExprCallable>),
}

impl ExprValue {
    pub fn str(s: impl Into<String>) -> Self {
        Self::Str(s.into())
    }

    pub fn list(items: Vec<ExprValue>) -> Self {
        Self::List(Seq::new(items))
    }

    pub fn tuple(items: Vec<ExprValue>) -> Self {
        Self::Tuple(Seq::new(items))
    }

    /// A dict from pairs; unhashable keys are a `TypeError`.
    pub fn dict(pairs: Vec<(ExprValue, ExprValue)>) -> ExprResult<Self> {
        Ok(Self::Dict(Map::from_pairs(pairs)?))
    }

    /// A dict with string keys (cannot fail).
    pub fn dict_str<K: Into<String>>(pairs: impl IntoIterator<Item = (K, ExprValue)>) -> Self {
        let mut entries = IndexMap::new();
        for (k, v) in pairs {
            entries.insert(HKey(ExprValue::Str(k.into())), v);
        }
        Self::Dict(Map::from_entries(entries))
    }

    /// Size and shape, O(1).
    pub fn meta(&self) -> Meta {
        match self {
            Self::Str(s) => Meta::scalar(s.len()),
            Self::Bytes(b) => Meta::scalar(b.len()),
            Self::RelativeDelta(_) => Meta::scalar(RELATIVE_DELTA_PAYLOAD),
            Self::List(s) | Self::Tuple(s) => s.meta(),
            Self::Dict(m) => m.meta(),
            Self::Object(o) => o.meta(),
            Self::None
            | Self::Bool(_)
            | Self::Int(_)
            | Self::Float(_)
            | Self::Date(_)
            | Self::DateTime(_)
            | Self::TimeDelta(_)
            | Self::Callable(_) => Meta::scalar(0),
        }
    }

    /// Deep size in bytes (accounting unit).
    pub fn deep_size(&self) -> usize {
        self.meta().bytes
    }

    /// Attaches host hooks to an inner object or collection of objects.
    ///
    /// # Errors
    ///
    /// The rebuilt dict cannot be built (never silently left without hooks).
    pub fn attach_hooks(&mut self, hooks: Arc<crate::proxy::HostHooks>) -> ExprResult<()> {
        match self {
            ExprValue::Object(o) => {
                if let Some(new_o) = o.with_hooks(hooks) {
                    *o = new_o;
                }
            }
            ExprValue::List(items) => {
                let mut new_items: Vec<ExprValue> = items.iter().cloned().collect();
                for item in &mut new_items {
                    item.attach_hooks(Arc::clone(&hooks))?;
                }
                *items = Seq::new(new_items);
            }
            ExprValue::Tuple(items) => {
                let mut new_items: Vec<ExprValue> = items.iter().cloned().collect();
                for item in &mut new_items {
                    item.attach_hooks(Arc::clone(&hooks))?;
                }
                *items = Seq::new(new_items);
            }
            ExprValue::Dict(map) => {
                let mut pairs = Vec::with_capacity(map.len());
                for (k, v) in map.iter() {
                    let mut val = v.clone();
                    val.attach_hooks(Arc::clone(&hooks))?;
                    pairs.push((k.clone(), val));
                }
                *map = Map::from_pairs(pairs)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Nesting depth (0 for scalars).
    pub fn depth(&self) -> usize {
        self.meta().depth
    }

    /// Python truthiness.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::None => false,
            Self::Bool(b) => *b,
            Self::Int(i) => *i != 0,
            Self::Float(f) => *f != 0.0,
            Self::Str(s) => !s.is_empty(),
            Self::Bytes(b) => !b.is_empty(),
            Self::Date(_) | Self::DateTime(_) => true,
            Self::TimeDelta(td) => td.total_micros() != 0,
            Self::RelativeDelta(rd) => !rd.is_zero(),
            Self::List(s) | Self::Tuple(s) => !s.is_empty(),
            Self::Dict(m) => !m.is_empty(),
            Self::Object(o) => o.is_truthy(),
            Self::Callable(_) => true,
        }
    }

    /// Python type name, as used in error messages.
    pub fn type_name(&self) -> &str {
        match self {
            Self::None => "NoneType",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::Str(_) => "str",
            Self::Bytes(_) => "bytes",
            Self::Date(_) => "datetime.date",
            Self::DateTime(_) => "datetime.datetime",
            Self::TimeDelta(_) => "datetime.timedelta",
            Self::RelativeDelta(_) => "relativedelta",
            Self::List(_) => "list",
            Self::Tuple(_) => "tuple",
            Self::Dict(_) => "dict",
            Self::Object(o) => o.type_name(),
            Self::Callable(_) => "builtin_function_or_method",
        }
    }

    /// The integer value of an `int` or `bool`. A record is not an int here: Python only
    /// converts it through `__int__` (`int(rec)`, and the loader's record-to-id step), so
    /// arithmetic, ordering, bitwise operators and indexing on a record stay a `TypeError`.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            Self::Bool(b) => Some(i64::from(*b)),
            _ => None,
        }
    }

    /// The float value of a `float`, `int` or `bool`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(f) => Some(*f),
            Self::Int(i) => Some(*i as f64),
            Self::Bool(b) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn is_number(&self) -> bool {
        matches!(self, Self::Int(_) | Self::Bool(_) | Self::Float(_))
    }

    /// Python `repr()` without a size cap (for `Display`, `Debug` and hosts).
    pub fn repr(&self) -> String {
        let mut s = String::new();
        // A `String` sink never fails.
        if self.write_repr(&mut s).is_err() {
            s.clear();
        }
        s
    }

    /// `repr()` cut to at most [`REPR_SHORT_BYTES`] bytes (with a trailing `...`), for error
    /// messages: an error never carries a copy of a large value.
    pub fn repr_short(&self) -> String {
        let mut out = Out::truncating(REPR_SHORT_BYTES);
        let complete = self.write_repr(&mut out).is_ok();
        let mut s = out.into_string();
        if !complete {
            s.push_str("...");
        }
        s
    }

    /// Python `str()` without a size cap.
    pub fn to_py_str(&self) -> String {
        let mut s = String::new();
        if self.write_str(&mut s).is_err() {
            s.clear();
        }
        s
    }

    /// `repr()` charged to the budget, never growing past it.
    pub fn repr_budgeted(&self, budget: &mut Budget) -> ExprResult<String> {
        budget.steps(self.meta().nodes)?;
        let mut out = budget.out();
        self.write_repr(&mut out)?;
        budget.finish(out)
    }

    /// `str()` charged to the budget, never growing past it.
    pub fn str_budgeted(&self, budget: &mut Budget) -> ExprResult<String> {
        budget.steps(self.meta().nodes)?;
        let mut out = budget.out();
        self.write_str(&mut out)?;
        budget.finish(out)
    }

    pub(crate) fn write_str<S: Sink>(&self, out: &mut S) -> ExprResult<()> {
        match self {
            Self::Str(s) => out.put(s),
            Self::Date(d) => out.put(&datetime::date_iso(*d)),
            Self::DateTime(dt) => out.put(&datetime::datetime_iso(*dt, ' ')),
            Self::TimeDelta(td) => out.put(&td.to_py_str()),
            _ => self.write_repr(out),
        }
    }

    pub(crate) fn write_repr<S: Sink>(&self, out: &mut S) -> ExprResult<()> {
        match self {
            Self::None => out.put("None"),
            Self::Bool(true) => out.put("True"),
            Self::Bool(false) => out.put("False"),
            Self::Int(i) => out.put(&i.to_string()),
            Self::Float(f) => out.put(&num::float_repr(*f)),
            Self::Str(s) => write_str_repr(s, out),
            Self::Bytes(b) => write_bytes_repr(b, out),
            Self::Date(d) => out.put(&format!(
                "datetime.date({}, {}, {})",
                d.year(),
                u8::from(d.month()),
                d.day()
            )),
            Self::DateTime(dt) => out.put(&datetime::datetime_repr(*dt)),
            Self::TimeDelta(td) => out.put(&td.to_py_repr()),
            Self::RelativeDelta(rd) => out.put(&rd.to_py_repr()),
            Self::List(items) => {
                out.put("[")?;
                write_items(items, out)?;
                out.put("]")
            }
            Self::Tuple(items) => {
                out.put("(")?;
                write_items(items, out)?;
                if items.len() == 1 {
                    out.put(",")?;
                }
                out.put(")")
            }
            Self::Dict(m) => {
                out.put("{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.put(", ")?;
                    }
                    k.write_repr(out)?;
                    out.put(": ")?;
                    v.write_repr(out)?;
                }
                out.put("}")
            }
            Self::Object(o) => out.put(&o.repr()),
            Self::Callable(c) => out.put(&format!("<built-in function {}>", c.name())),
        }
    }

    /// Python `==`. Values of unrelated types are unequal, never an error.
    pub fn py_eq(&self, other: &ExprValue) -> bool {
        use ExprValue as V;
        match (self, other) {
            (V::None, V::None) => true,
            (V::Bool(_) | V::Int(_), V::Bool(_) | V::Int(_)) => self.as_int() == other.as_int(),
            (V::Float(a), V::Float(b)) => a == b,
            (V::Bool(_) | V::Int(_), V::Float(f)) | (V::Float(f), V::Bool(_) | V::Int(_)) => {
                match self.as_int().or(other.as_int()) {
                    Some(i) => num::eq_int_float(i, *f),
                    None => false,
                }
            }
            (V::Str(a), V::Str(b)) => a == b,
            (V::Bytes(a), V::Bytes(b)) => a == b,
            (V::Date(a), V::Date(b)) => a == b,
            (V::DateTime(a), V::DateTime(b)) => a == b,
            (V::TimeDelta(a), V::TimeDelta(b)) => a == b,
            (V::RelativeDelta(a), V::RelativeDelta(b)) => a == b,
            (V::List(a), V::List(b)) | (V::Tuple(a), V::Tuple(b)) => {
                a.ptr_eq(b)
                    || (a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.py_eq(y)))
            }
            (V::Dict(a), V::Dict(b)) => {
                a.ptr_eq(b)
                    || (a.len() == b.len()
                        && a.iter()
                            .all(|(k, v)| matches!(b.get(k), Ok(Some(bv)) if v.py_eq(bv))))
            }
            (V::Object(a), V::Object(b)) => Arc::ptr_eq(a, b) || a.py_eq(b.as_ref()),
            (V::Callable(a), V::Callable(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Python ordering for `<`, `<=`, `>`, `>=`. `Ok(None)` means unordered (a NaN is involved):
    /// every ordering comparison is then `False`, as in Python. Unorderable types are a `TypeError`
    /// naming `op`.
    pub fn py_partial_cmp(&self, other: &ExprValue, op: &str) -> ExprResult<Option<Ordering>> {
        use ExprValue as V;
        match (self.as_int(), other.as_int(), self, other) {
            (Some(a), Some(b), _, _) => return Ok(Some(a.cmp(&b))),
            (Some(a), None, _, V::Float(f)) => return Ok(num::cmp_int_float(a, *f)),
            (None, Some(b), V::Float(f), _) => {
                return Ok(num::cmp_int_float(b, *f).map(Ordering::reverse));
            }
            _ => {}
        }
        Ok(match (self, other) {
            (V::Float(a), V::Float(b)) => a.partial_cmp(b),
            (V::Str(a), V::Str(b)) => Some(a.cmp(b)),
            (V::Bytes(a), V::Bytes(b)) => Some(a.cmp(b)),
            (V::Date(a), V::Date(b)) => Some(a.cmp(b)),
            (V::DateTime(a), V::DateTime(b)) => Some(a.cmp(b)),
            (V::Date(_), V::DateTime(_)) | (V::DateTime(_), V::Date(_)) => {
                return Err(ExprError::type_error(format!(
                    "can't compare {} to {}",
                    self.type_name(),
                    other.type_name()
                )));
            }
            (V::TimeDelta(a), V::TimeDelta(b)) => Some(a.cmp(b)),
            (V::List(a), V::List(b)) | (V::Tuple(a), V::Tuple(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    if !x.py_eq(y) {
                        return x.py_partial_cmp(y, op);
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            _ => {
                return Err(ExprError::type_error(format!(
                    "'{op}' not supported between instances of '{}' and '{}'",
                    self.type_name(),
                    other.type_name()
                )));
            }
        })
    }
}

fn write_items<S: Sink>(items: &Seq, out: &mut S) -> ExprResult<()> {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.put(", ")?;
        }
        item.write_repr(out)?;
    }
    Ok(())
}

/// Python's quote choice: single quotes unless the text contains `'` and no `"`.
fn pick_quote(has_single: bool, has_double: bool) -> char {
    if has_single && !has_double { '"' } else { '\'' }
}

fn write_str_repr<S: Sink>(s: &str, out: &mut S) -> ExprResult<()> {
    let quote = pick_quote(s.contains('\''), s.contains('"'));
    let mut buf = String::with_capacity(s.len().saturating_add(2));
    buf.push(quote);
    for c in s.chars() {
        match c {
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if c == quote => {
                buf.push('\\');
                buf.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => {
                buf.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => buf.push(c),
        }
        if buf.len() >= 4096 {
            out.put(&buf)?;
            buf.clear();
        }
    }
    buf.push(quote);
    out.put(&buf)
}

fn write_bytes_repr<S: Sink>(b: &[u8], out: &mut S) -> ExprResult<()> {
    let quote = pick_quote(b.contains(&b'\''), b.contains(&b'"'));
    let mut buf = String::with_capacity(b.len().saturating_add(3));
    buf.push('b');
    buf.push(quote);
    for &byte in b {
        match byte {
            b'\\' => buf.push_str("\\\\"),
            b'\n' => buf.push_str("\\n"),
            b'\r' => buf.push_str("\\r"),
            b'\t' => buf.push_str("\\t"),
            c if char::from(c) == quote => {
                buf.push('\\');
                buf.push(char::from(c));
            }
            0x20..=0x7e => buf.push(char::from(byte)),
            _ => buf.push_str(&format!("\\x{byte:02x}")),
        }
        if buf.len() >= 4096 {
            out.put(&buf)?;
            buf.clear();
        }
    }
    buf.push(quote);
    out.put(&buf)
}

/// Destination of `repr`/`str` output: a capped [`Out`] or an unbounded `String`.
pub(crate) trait Sink {
    fn put(&mut self, s: &str) -> ExprResult<()>;
}

impl Sink for String {
    fn put(&mut self, s: &str) -> ExprResult<()> {
        self.push_str(s);
        Ok(())
    }
}

impl Sink for Out {
    fn put(&mut self, s: &str) -> ExprResult<()> {
        self.push_str(s)
    }
}

/// Structural, type-strict equality (`Int(1) != Float(1.0)`), for tests and hosts. Python `==`
/// is [`ExprValue::py_eq`].
impl PartialEq for ExprValue {
    fn eq(&self, other: &Self) -> bool {
        use ExprValue as V;
        match (self, other) {
            (V::None, V::None) => true,
            (V::Bool(a), V::Bool(b)) => a == b,
            (V::Int(a), V::Int(b)) => a == b,
            (V::Float(a), V::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (V::Str(a), V::Str(b)) => a == b,
            (V::Bytes(a), V::Bytes(b)) => a == b,
            (V::Date(a), V::Date(b)) => a == b,
            (V::DateTime(a), V::DateTime(b)) => a == b,
            (V::TimeDelta(a), V::TimeDelta(b)) => a == b,
            (V::RelativeDelta(a), V::RelativeDelta(b)) => a == b,
            (V::List(a), V::List(b)) | (V::Tuple(a), V::Tuple(b)) => a.items() == b.items(),
            (V::Dict(a), V::Dict(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(x, y)| x.0 == y.0 && x.1 == y.1)
            }
            (V::Object(a), V::Object(b)) => Arc::ptr_eq(a, b),
            (V::Callable(a), V::Callable(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl fmt::Debug for ExprValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.repr())
    }
}

impl fmt::Display for ExprValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_py_str())
    }
}

impl From<bool> for ExprValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<i64> for ExprValue {
    fn from(i: i64) -> Self {
        Self::Int(i)
    }
}

impl From<f64> for ExprValue {
    fn from(f: f64) -> Self {
        Self::Float(f)
    }
}

impl From<&str> for ExprValue {
    fn from(s: &str) -> Self {
        Self::Str(s.to_string())
    }
}

impl From<String> for ExprValue {
    fn from(s: String) -> Self {
        Self::Str(s)
    }
}
