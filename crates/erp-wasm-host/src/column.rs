//! Length of a `column-data` value, whatever its kind.

use crate::bindings::erpforall::addon::types::ColumnData;

/// Number of values (one per record) in a column.
#[must_use]
pub fn len(data: &ColumnData) -> usize {
    match data {
        ColumnData::ColBool(v) => v.len(),
        ColumnData::ColOptBool(v) => v.len(),
        ColumnData::ColInt(v) => v.len(),
        ColumnData::ColOptInt(v) => v.len(),
        ColumnData::ColFloat(v) => v.len(),
        ColumnData::ColOptFloat(v) => v.len(),
        ColumnData::ColOptDecimal(v) => v.len(),
        ColumnData::ColString(v) => v.len(),
        ColumnData::ColOptString(v) => v.len(),
        ColumnData::ColOptDate(v) => v.len(),
        ColumnData::ColOptDatetime(v) => v.len(),
        ColumnData::ColOptDateRange(v) => v.len(),
        ColumnData::ColOptDatetimeRange(v) => v.len(),
        ColumnData::ColOptId(v) => v.len(),
        ColumnData::ColIds(v) => v.len(),
        ColumnData::ColOptJson(v) => v.len(),
        ColumnData::ColOptBytes(v) => v.len(),
    }
}

/// Bytes a column carries: strings, JSON and binary values by length, everything else 16 bytes
/// per value (a conservative width for flags, numbers, dates and ranges).
#[must_use]
pub fn bytes(data: &ColumnData) -> usize {
    fn strs<'a>(it: impl Iterator<Item = Option<&'a str>>) -> usize {
        it.fold(0usize, |a, s| {
            a.saturating_add(s.map_or(1, str::len).saturating_add(8))
        })
    }
    match data {
        ColumnData::ColString(v) => strs(v.iter().map(|s| Some(s.as_str()))),
        ColumnData::ColOptString(v) | ColumnData::ColOptDecimal(v) | ColumnData::ColOptJson(v) => {
            strs(v.iter().map(Option::as_deref))
        }
        ColumnData::ColOptBytes(v) => v.iter().fold(0usize, |a, b| {
            a.saturating_add(b.as_ref().map_or(1, Vec::len).saturating_add(8))
        }),
        ColumnData::ColIds(v) => v.iter().fold(0usize, |a, ids| {
            a.saturating_add(ids.len().saturating_mul(8).saturating_add(8))
        }),
        other => len(other).saturating_mul(16),
    }
}
