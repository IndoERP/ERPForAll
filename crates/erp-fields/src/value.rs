//! The dynamic value layer: what JSON-2 bodies, Luau scripts, data files and
//! context dictionaries carry before the converter types them.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use indexmap::IndexMap;
use rust_decimal::Decimal;

use crate::cell::Cell;
use crate::range::RangeValue;
use crate::rows::handle as handle_impl;
use crate::rows::{DictRef, RowRef, RowsRef};
use crate::temporal::{Date, DateTime};

/// A dictionary of dynamic values that keeps insertion order, like a Python `dict` (keyword
/// arguments, specifications, form values keep the order the client sent). Two maps are equal when
/// they hold the same entries, in any order.
pub type ValueMap = IndexMap<String, Value>;

/// Reads return `Bool(false)` for boolean fields (NULL reads as false) and `False` for empty values
/// of every other type except integers and floats, which read `0` / `0.0`.
///
/// `#[non_exhaustive]`: code outside this crate matches it with a final arm
/// that refuses what it does not handle, so a new variant never breaks addons.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Value {
    /// Python `False` / `None`: "no value".
    False,
    /// `True` (a `False` boolean is `Bool(false)` or [`Value::False`]).
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// An exact decimal.
    Decimal(Decimal),
    /// A string.
    Str(Arc<str>),
    /// Raw bytes.
    Bytes(Arc<[u8]>),
    /// A date.
    Date(Date),
    /// A naive UTC datetime.
    DateTime(DateTime),
    /// Native UUID, never inferred from ordinary string contents.
    Uuid(crate::Uuid),
    /// Exact signed bigint; emitted as decimal text.
    BigInt(crate::BigInt),
    /// Native unsigned full-width transaction id.
    Xid8(crate::Xid8),
    /// Explicit timezone-aware microsecond instant.
    TimestampTz(crate::TimestampTz),
    /// A JSON document (for `Json` fields).
    Json(Arc<serde_json::Value>),
    /// A list or tuple.
    List(Arc<[Value]>),
    /// A dictionary with string keys, in insertion order.
    Map(Arc<ValueMap>),
    /// A range of dates or datetimes (`date_range`/`datetime_range` fields); written and rendered
    /// as `[lower, upper]` with `False` for an unbounded end.
    Range(RangeValue),
    /// One row of a result batch ([`crate::rows`]): equal to and hashed like a
    /// [`Value::Map`] with the same entries; read it with [`Value::dict`].
    Row(RowRef),
    /// Rows of a result batch, or one parent row's children: equal to and hashed like a
    /// [`Value::List`] of maps.
    Rows(RowsRef),
}

// Every `Value` stays 32 bytes on 64-bit targets: a handle that grew it would cost every value
// everywhere.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Value>() == 32);

impl Value {
    /// A string value.
    pub fn str(s: impl AsRef<str>) -> Self {
        Self::Str(Arc::from(s.as_ref()))
    }

    /// A list value.
    pub fn list(items: impl IntoIterator<Item = Value>) -> Self {
        Self::List(items.into_iter().collect())
    }

    /// A read-only dictionary view over a [`Value::Map`] or a [`Value::Row`]; `None` for any
    /// other value.
    #[must_use]
    pub fn dict(&self) -> Option<DictRef<'_>> {
        match self {
            Self::Map(map) => Some(DictRef::map(map)),
            Self::Row(row) => Some(DictRef::row(row)),
            _ => None,
        }
    }

    /// Python truthiness.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::False => false,
            Self::Bool(b) => *b,
            Self::BigInt(i) => i.get() != 0,
            Self::Int(i) => *i != 0,
            Self::Float(f) => *f != 0.0,
            Self::Decimal(d) => !d.is_zero(),
            Self::Str(s) => !s.is_empty(),
            Self::Bytes(b) => !b.is_empty(),
            Self::Date(_)
            | Self::DateTime(_)
            | Self::Uuid(_)
            | Self::Xid8(_)
            | Self::TimestampTz(_) => true,
            Self::Json(j) => crate::convert::json_truthy(j),
            Self::List(l) => !l.is_empty(),
            Self::Map(m) => !m.is_empty(),
            // a range field's "no value" is `False`; every stored range holds something
            Self::Range(_) => true,
            Self::Row(row) => !row.is_empty(),
            Self::Rows(rows) => !rows.is_empty(),
        }
    }

    /// The dynamic value of a cell, as a read returns it (NULL becomes `False`).
    pub fn from_cell(cell: &Cell) -> Self {
        match cell {
            Cell::Null => Self::False,
            Cell::Bool(b) => Self::Bool(*b),
            Cell::Int(i) => Self::Int(i64::from(*i)),
            Cell::Id(i) => Self::Int(*i),
            Cell::Float(f) => Self::Float(*f),
            Cell::Decimal(d) => Self::Decimal(*d),
            Cell::Str(s) => Self::Str(Arc::clone(s)),
            Cell::Date(d) => Self::Date(*d),
            Cell::DateTime(d) => Self::DateTime(*d),
            Cell::Uuid(value) => Self::Uuid(*value),
            Cell::BigInt(value) => Self::BigInt(*value),
            Cell::Xid8(value) => Self::Xid8(*value),
            Cell::TimestampTz(value) => Self::TimestampTz(*value),
            Cell::Json(j) => Self::Json(Arc::clone(j)),
            Cell::Ids(ids) => Self::List(ids.iter().map(|id| Self::Int(id.get())).collect()),
            Cell::DateRange(r) => Self::Range(RangeValue::Date(*r)),
            Cell::DatetimeRange(r) => Self::Range(RangeValue::Datetime(*r)),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::False, Self::False) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            (Self::Decimal(a), Self::Decimal(b)) => a == b,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::DateTime(a), Self::DateTime(b)) => a == b,
            (Self::Uuid(a), Self::Uuid(b)) => a == b,
            (Self::BigInt(a), Self::BigInt(b)) => a == b,
            (Self::Xid8(a), Self::Xid8(b)) => a == b,
            (Self::TimestampTz(a), Self::TimestampTz(b)) => a == b,
            (Self::Json(a), Self::Json(b)) => a == b,
            (Self::List(a), Self::List(b)) => a == b,
            (Self::Map(a), Self::Map(b)) => a == b,
            (Self::Range(a), Self::Range(b)) => a == b,
            (Self::Row(a), Self::Row(b)) => handle_impl::row_eq_row(a, b),
            (Self::Row(row), Self::Map(map)) | (Self::Map(map), Self::Row(row)) => {
                handle_impl::row_eq_map(row, map)
            }
            (Self::Rows(a), Self::Rows(b)) => handle_impl::rows_eq_rows(a, b),
            (Self::Rows(rows), Self::List(list)) | (Self::List(list), Self::Rows(rows)) => {
                handle_impl::rows_eq_list(rows, list)
            }
            _ => false,
        }
    }
}

// Floats compare by bit pattern above, so equality is reflexive.
impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // a tag per variant; `Row` shares `Map`'s and `Rows` shares `List`'s, since they compare
        // equal to them
        let tag: u8 = match self {
            Self::False => 0,
            Self::Bool(_) => 1,
            Self::Int(_) => 2,
            Self::Float(_) => 3,
            Self::Decimal(_) => 4,
            Self::Str(_) => 5,
            Self::Bytes(_) => 6,
            Self::Date(_) => 7,
            Self::DateTime(_) => 8,
            Self::Json(_) => 9,
            Self::List(_) | Self::Rows(_) => 10,
            Self::Map(_) | Self::Row(_) => 11,
            Self::Range(_) => 12,
            Self::Uuid(_) => 13,
            Self::BigInt(_) => 16,
            Self::Xid8(_) => 14,
            Self::TimestampTz(_) => 15,
        };
        tag.hash(state);
        match self {
            Self::False => {}
            Self::Bool(b) => b.hash(state),
            Self::Int(i) => i.hash(state),
            Self::Float(f) => f.to_bits().hash(state),
            Self::Decimal(d) => d.hash(state),
            Self::Str(s) => s.hash(state),
            Self::Bytes(b) => b.hash(state),
            Self::Date(d) => d.hash(state),
            Self::DateTime(d) => d.hash(state),
            Self::Uuid(value) => value.hash(state),
            Self::BigInt(value) => value.hash(state),
            Self::Xid8(value) => value.hash(state),
            Self::TimestampTz(value) => value.hash(state),
            Self::Json(j) => hash_json(j, state),
            Self::List(l) => {
                // spelled out (not `<[Value]>::hash`) so `Rows` can hash exactly the same way
                l.len().hash(state);
                for v in l.iter() {
                    v.hash(state);
                }
            }
            // order-independent, like equality
            Self::Map(m) => {
                handle_impl::hash_entries(m.iter().map(|(k, v)| (k.as_str(), v)), state)
            }
            Self::Range(r) => r.hash(state),
            Self::Row(row) => handle_impl::hash_row(row, state),
            Self::Rows(rows) => handle_impl::hash_rows(rows, state),
        }
    }
}

/// Hashes a JSON document consistently with `serde_json::Value`'s equality, without rendering it:
/// a tag per JSON kind, numbers through `serde_json::Number`'s own `Hash` (which agrees with its
/// equality, `0.0` and `-0.0` included), arrays in order, and objects order-independently (equality
/// ignores key order once `serde_json/preserve_order` keeps insertion order), like `Map` above.
fn hash_json<H: Hasher>(j: &serde_json::Value, state: &mut H) {
    use serde_json::Value as J;
    match j {
        J::Null => 0u8.hash(state),
        J::Bool(b) => {
            1u8.hash(state);
            b.hash(state);
        }
        J::Number(n) => {
            2u8.hash(state);
            n.hash(state);
        }
        J::String(s) => {
            3u8.hash(state);
            s.hash(state);
        }
        J::Array(a) => {
            4u8.hash(state);
            a.len().hash(state);
            for v in a {
                hash_json(v, state);
            }
        }
        J::Object(o) => {
            5u8.hash(state);
            let mut sum: u64 = 0;
            for (k, v) in o {
                let mut h = std::hash::DefaultHasher::new();
                k.hash(&mut h);
                hash_json(v, &mut h);
                sum = sum.wrapping_add(h.finish());
            }
            o.len().hash(state);
            sum.hash(state);
        }
    }
}

impl fmt::Display for Value {
    /// Python `repr()`-like rendering, used in error messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::False => f.write_str("False"),
            Self::Bool(true) => f.write_str("True"),
            Self::Bool(false) => f.write_str("False"),
            Self::Int(i) => write!(f, "{i}"),
            Self::Float(x) => write!(f, "{x:?}"),
            Self::Decimal(d) => write!(f, "{d}"),
            Self::Str(s) => write!(f, "'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
            Self::Bytes(b) => write!(f, "b'{}'", String::from_utf8_lossy(b)),
            Self::Date(d) => write!(f, "'{d}'"),
            Self::DateTime(d) => write!(f, "'{d}'"),
            Self::Uuid(value) => write!(f, "'{value}'"),
            Self::BigInt(value) => write!(f, "'{value}'"),
            Self::Xid8(value) => write!(f, "'{value}'"),
            Self::TimestampTz(value) => write!(f, "'{value}'"),
            Self::Json(j) => write!(f, "{j}"),
            Self::List(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Self::Map(map) => {
                f.write_str("{")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "'{k}': {v}")?;
                }
                f.write_str("}")
            }
            Self::Range(r) => write!(f, "{r}"),
            Self::Row(row) => handle_impl::fmt_row(row, f),
            Self::Rows(rows) => handle_impl::fmt_rows(rows, f),
        }
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Self::Int(i)
    }
}

impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Self::Int(i64::from(i))
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Self {
        Self::Float(x)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::str(s)
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Self::Str(Arc::from(s))
    }
}

impl From<Date> for Value {
    fn from(d: Date) -> Self {
        Self::Date(d)
    }
}

impl From<DateTime> for Value {
    fn from(d: DateTime) -> Self {
        Self::DateTime(d)
    }
}

impl From<RangeValue> for Value {
    fn from(r: RangeValue) -> Self {
        Self::Range(r)
    }
}

impl From<crate::DateRange> for Value {
    fn from(r: crate::DateRange) -> Self {
        Self::Range(RangeValue::Date(r))
    }
}

impl From<crate::DatetimeRange> for Value {
    fn from(r: crate::DatetimeRange) -> Self {
        Self::Range(RangeValue::Datetime(r))
    }
}

impl From<crate::BigInt> for Value {
    fn from(value: crate::BigInt) -> Self {
        Self::BigInt(value)
    }
}

impl From<crate::Uuid> for Value {
    fn from(value: crate::Uuid) -> Self {
        Self::Uuid(value)
    }
}
impl From<crate::Xid8> for Value {
    fn from(value: crate::Xid8) -> Self {
        Self::Xid8(value)
    }
}
impl From<crate::TimestampTz> for Value {
    fn from(value: crate::TimestampTz) -> Self {
        Self::TimestampTz(value)
    }
}

#[cfg(test)]
mod tests {
    use std::hash::DefaultHasher;

    use super::*;

    #[test]
    fn repr_like_python() {
        assert_eq!(Value::str("zz_ZZ").to_string(), "'zz_ZZ'");
        assert_eq!(Value::Bool(true).to_string(), "True");
        assert_eq!(Value::Float(1.9).to_string(), "1.9");
        assert_eq!(
            Value::list([Value::Int(1), Value::str("a")]).to_string(),
            "[1, 'a']"
        );
    }

    #[test]
    fn maps_keep_order_and_compare_as_sets() {
        let a: ValueMap = [
            ("b".to_owned(), Value::Int(1)),
            ("a".to_owned(), Value::Int(2)),
        ]
        .into_iter()
        .collect();
        let b: ValueMap = [
            ("a".to_owned(), Value::Int(2)),
            ("b".to_owned(), Value::Int(1)),
        ]
        .into_iter()
        .collect();
        assert_eq!(a.keys().next().map(String::as_str), Some("b"));
        let (va, vb) = (Value::Map(Arc::new(a)), Value::Map(Arc::new(b)));
        assert_eq!(va, vb);
        let hash = |v: &Value| {
            let mut h = DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(&va), hash(&vb));
        assert_eq!(va.to_string(), "{'b': 1, 'a': 2}");
    }

    /// `Value::Json` keeps the `Hash`/`Eq` contract: JSON documents that serde_json calls equal hash
    /// the same. The old `to_string` hash failed on `0.0` vs `-0.0` (equal numbers, different text)
    /// in every build, and on reordered object keys whenever `serde_json/preserve_order` is enabled
    /// anywhere in the dependency graph (not today: the object case is the guard for that build).
    /// Without `preserve_order`, `serde_json::Map` is a `BTreeMap` and the two objects below are
    /// stored in the same order, so the key-order half passes trivially; the `0.0`/`-0.0` half is
    /// what fails on the old hash in this build.
    #[test]
    fn json_hash_agrees_with_json_equality() {
        let hash = |v: &Value| {
            let mut h = DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        };
        let mut a = serde_json::Map::new();
        a.insert("b".to_owned(), serde_json::json!(1));
        a.insert(
            "a".to_owned(),
            serde_json::json!([0.0, {"y": null, "x": "s"}]),
        );
        let mut b = serde_json::Map::new();
        b.insert(
            "a".to_owned(),
            serde_json::json!([-0.0, {"x": "s", "y": null}]),
        );
        b.insert("b".to_owned(), serde_json::json!(1));
        let va = Value::Json(Arc::new(serde_json::Value::Object(a)));
        let vb = Value::Json(Arc::new(serde_json::Value::Object(b)));
        assert_eq!(va, vb);
        assert_eq!(hash(&va), hash(&vb));
        let zero = Value::Json(Arc::new(serde_json::json!(0.0)));
        let neg_zero = Value::Json(Arc::new(serde_json::json!(-0.0)));
        assert_eq!(zero, neg_zero);
        assert_eq!(hash(&zero), hash(&neg_zero));
        // unequal documents of different kinds still hash apart
        let null = Value::Json(Arc::new(serde_json::Value::Null));
        let empty = Value::Json(Arc::new(serde_json::json!([])));
        assert_ne!(null, empty);
        assert_ne!(hash(&null), hash(&empty));
    }

    #[test]
    fn truthiness() {
        assert!(!Value::False.is_truthy());
        assert!(!Value::str("").is_truthy());
        assert!(Value::str("False").is_truthy());
        assert!(!Value::Int(0).is_truthy());
    }
}
