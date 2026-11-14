//! Batches from dictionaries: synthetic results of overrides that do not call `super`
//! (`Rows::from_maps`).

use std::sync::Arc;

use super::column::{Bitmap, Column, ColumnData};
use super::layout::{Key, Layout, MAX_KEYS};
use super::{MAX_ROWS, Rows, RowsBuilder};
use crate::{Error, Result, Value, ValueMap};

/// The typed column kinds `from_maps` infers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Int,
    Float,
    Decimal,
    Str,
    Bytes,
    Date,
    DateTime,
    Uuid,
    BigInt,
    Xid8,
    TimestampTz,
    Json,
    Range,
}

fn kind_of(value: &Value) -> Option<Kind> {
    Some(match value {
        Value::Bool(_) => Kind::Bool,
        Value::Int(_) => Kind::Int,
        Value::Float(_) => Kind::Float,
        Value::Decimal(_) => Kind::Decimal,
        Value::Str(_) => Kind::Str,
        Value::Bytes(_) => Kind::Bytes,
        Value::Date(_) => Kind::Date,
        Value::DateTime(_) => Kind::DateTime,
        Value::Uuid(_) => Kind::Uuid,
        Value::BigInt(_) => Kind::BigInt,
        Value::Xid8(_) => Kind::Xid8,
        Value::TimestampTz(_) => Kind::TimestampTz,
        Value::Json(_) => Kind::Json,
        Value::Range(_) => Kind::Range,
        Value::False | Value::List(_) | Value::Map(_) | Value::Row(_) | Value::Rows(_) => {
            return None;
        }
    })
}

impl Rows {
    /// A batch from dictionaries, for overrides that build their result instead of calling
    /// `super`. Keys are the union of the maps' keys in first-seen order; a map without a key
    /// leaves it absent in its row. Every row is read (and written as JSON) in that one layout
    /// order, so a map whose keys come in another order than the first map's is reordered:
    /// `[{a, b}, {b, a}]` gives two rows keyed `a, b` (`web_read` results have one key order per
    /// result; PERF.ROWS-API revisits this if an override needs per-row order). A key whose present values all have one scalar type (or are
    /// `False`) gets a typed column; any other key a dynamic one.
    ///
    /// # Errors
    /// `Programming` for more than [`MAX_ROWS`] maps or [`MAX_KEYS`] distinct keys.
    ///
    /// ```
    /// use erp_fields::{Value, ValueMap};
    /// use erp_fields::rows::Rows;
    /// let map = |pairs: &[(&str, Value)]| -> ValueMap {
    ///     pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    /// };
    /// let rows = Rows::from_maps([
    ///     map(&[("id", Value::Int(1)), ("help", Value::str("Read me"))]),
    ///     map(&[("id", Value::Int(2))]),
    /// ])?;
    /// assert_eq!(rows.keys().collect::<Vec<_>>(), ["id", "help"]);
    /// assert_eq!(rows.get_opt(1, "help")?, None);
    /// # Ok::<(), erp_fields::Error>(())
    /// ```
    pub fn from_maps(maps: impl IntoIterator<Item = ValueMap>) -> Result<Rows> {
        let maps: Vec<ValueMap> = maps.into_iter().collect();
        if maps.len() > MAX_ROWS {
            return Err(Error::programming(format!(
                "a row batch holds at most {MAX_ROWS} rows, not {}",
                maps.len()
            )));
        }
        // the union of keys, first-seen order; the lookup is the layout's own once built
        let mut keys: Vec<&str> = Vec::new();
        let mut seen: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();
        for map in &maps {
            for key in map.keys() {
                if seen.insert(key.as_str()) {
                    keys.push(key.as_str());
                    if keys.len() > MAX_KEYS {
                        return Err(Error::programming(format!(
                            "a row layout holds at most {MAX_KEYS} keys, not more"
                        )));
                    }
                }
            }
        }
        let layout = Layout::new(keys.iter().map(|k| Key::from(Arc::<str>::from(*k))))?;
        let mut builder = RowsBuilder::new(Arc::clone(&layout), maps.len())?;
        for (pos, key) in keys.iter().enumerate() {
            let column = column_of(&maps, key)?;
            builder.column_at(pos, column)?;
        }
        builder.finish()
    }
}

/// The column of `key` over `maps`: typed when every present value has one scalar type or is
/// `False`, dynamic otherwise.
fn column_of(maps: &[ValueMap], key: &str) -> Result<Column> {
    let mut absent = Bitmap::new();
    let mut kind: Option<Kind> = None;
    let mut mixed = false;
    for (row, map) in maps.iter().enumerate() {
        match map.get(key) {
            None => absent.put(row, true),
            Some(Value::False) => {}
            Some(value) => match (kind, kind_of(value)) {
                (_, None) => mixed = true,
                (None, Some(k)) => kind = Some(k),
                (Some(a), Some(b)) if a == b => {}
                (Some(_), Some(_)) => mixed = true,
            },
        }
    }
    let values = || maps.iter().map(|m| m.get(key));
    let column = match (mixed, kind) {
        // no typed value at all (only False or absent), or several types: dynamic
        (true, _) | (false, None) => Column::new(ColumnData::Dyn(
            values()
                .map(|v| match v {
                    Some(v) => v.clone(),
                    // the placeholder of an absent row, hidden by the absent bitmap
                    None => Value::False,
                })
                .collect(),
        )),
        (false, Some(kind)) => typed(kind, maps.len(), values())?,
    };
    Ok(column.with_absent(absent))
}

/// A typed column; every value is of `kind`, `False`, or absent (`None`, stored as a placeholder
/// that the absent bitmap hides).
fn typed<'a>(
    kind: Kind,
    len: usize,
    values: impl Iterator<Item = Option<&'a Value>>,
) -> Result<Column> {
    let mut falsy = Bitmap::new();
    macro_rules! plain {
        ($variant:ident, $pattern:ident, $default:expr) => {{
            let mut out = Vec::with_capacity(len);
            for (row, value) in values.enumerate() {
                out.push(match value {
                    Some(Value::$pattern(x)) => *x,
                    Some(Value::False) => {
                        falsy.put(row, true);
                        $default
                    }
                    None => $default,
                    Some(other) => return Err(mismatch(other)),
                });
            }
            ColumnData::$variant(out)
        }};
    }
    macro_rules! optional {
        ($variant:ident, $pattern:ident, $take:expr) => {{
            let mut out = Vec::with_capacity(len);
            for value in values {
                out.push(match value {
                    Some(Value::$pattern(x)) => Some($take(x)),
                    Some(Value::False) | None => None,
                    Some(other) => return Err(mismatch(other)),
                });
            }
            ColumnData::$variant(out)
        }};
    }
    let data = match kind {
        Kind::Bool => {
            let mut bits = Bitmap::new();
            for (row, value) in values.enumerate() {
                match value {
                    Some(Value::Bool(b)) => bits.put(row, *b),
                    Some(Value::False) => falsy.put(row, true),
                    None => {}
                    Some(other) => return Err(mismatch(other)),
                }
            }
            ColumnData::Bool(bits)
        }
        Kind::Int => plain!(Int, Int, 0),
        Kind::Float => plain!(Float, Float, 0.0),
        Kind::Decimal => plain!(Decimal, Decimal, rust_decimal::Decimal::ZERO),
        // the placeholder of a False or absent row, hidden by the falsy or absent bitmap
        Kind::Date => plain!(Date, Date, crate::Date::from_days(0)),
        Kind::DateTime => plain!(DateTime, DateTime, crate::DateTime::from_micros(0)),
        Kind::Uuid => plain!(Uuid, Uuid, crate::Uuid::NIL),
        Kind::BigInt => plain!(BigInt, BigInt, crate::BigInt::from_i64(0)),
        Kind::Xid8 => plain!(Xid8, Xid8, crate::Xid8::from_u64(0)),
        Kind::TimestampTz => plain!(TimestampTz, TimestampTz, crate::TimestampTz::UNIX_EPOCH),
        Kind::Str => optional!(Str, Str, Arc::clone),
        Kind::Bytes => optional!(Bytes, Bytes, Arc::clone),
        Kind::Json => optional!(Json, Json, Arc::clone),
        Kind::Range => optional!(Range, Range, |r: &crate::RangeValue| *r),
    };
    Ok(Column::new(data).with_false(falsy))
}

fn mismatch(value: &Value) -> Error {
    Error::programming(format!(
        "{value} does not fit the column inferred for it (internal error in erp_fields::rows)"
    ))
}
