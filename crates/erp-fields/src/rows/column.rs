//! Typed columns of a row batch, their "false" and "absent" bitmaps, and the child levels of
//! relational keys (D3).

use std::sync::Arc;

use rust_decimal::Decimal;

use super::handle::{ChildPositions, RowRef, RowsRef};
use super::{MAX_ROWS, Rows};
use crate::range::RangeValue;
use crate::temporal::{Date, DateTime};
use crate::{Error, RecId, Result, Value};

/// The position of a many2one column ([`Targets`]) that reads as `False`.
pub const NO_TARGET: u32 = u32::MAX;

/// One bit per row. Bits past the last set bit need no storage, so a bitmap with no bit set
/// allocates nothing. Two bitmaps are equal when the same bits are set, however much storage
/// each keeps.
#[derive(Debug, Clone, Default)]
pub struct Bitmap {
    words: Vec<u64>,
}

impl Bitmap {
    /// A bitmap with no bit set.
    #[must_use]
    pub const fn new() -> Self {
        Self { words: Vec::new() }
    }

    /// A bitmap with the bits of `rows` set.
    ///
    /// # Errors
    /// `Programming` for a row at or past [`MAX_ROWS`](super::MAX_ROWS).
    pub fn from_rows(rows: impl IntoIterator<Item = usize>) -> Result<Self> {
        let mut bits = Self::new();
        for row in rows {
            bits.set(row, true)?;
        }
        Ok(bits)
    }

    /// `len` bits, all set.
    pub(crate) fn all(len: usize) -> Self {
        let mut words = vec![u64::MAX; len / 64];
        if !len.is_multiple_of(64) {
            words.push((1u64 << (len % 64)) - 1);
        }
        Self { words }
    }

    /// Whether the bit of `row` is set.
    #[must_use]
    pub fn get(&self, row: usize) -> bool {
        self.words
            .get(row / 64)
            .is_some_and(|word| word & (1u64 << (row % 64)) != 0)
    }

    /// Sets or clears the bit of `row`.
    ///
    /// # Errors
    /// `Programming` for a row at or past [`MAX_ROWS`](super::MAX_ROWS): no batch has it, and
    /// setting it would allocate up to 512 MiB of words.
    pub fn set(&mut self, row: usize, on: bool) -> Result<()> {
        if row >= MAX_ROWS {
            return Err(Error::programming(format!(
                "row {row} is past the most rows a batch holds ({MAX_ROWS})"
            )));
        }
        self.put(row, on);
        Ok(())
    }

    /// [`Bitmap::set`] for a row the caller checked against its batch (below `MAX_ROWS`).
    pub(crate) fn put(&mut self, row: usize, on: bool) {
        let word = row / 64;
        let bit = 1u64 << (row % 64);
        if on {
            if self.words.len() <= word {
                self.words.resize(word + 1, 0);
            }
            if let Some(w) = self.words.get_mut(word) {
                *w |= bit;
            }
        } else if let Some(w) = self.words.get_mut(word) {
            *w &= !bit;
        }
    }

    /// Clears every bit, keeping the storage for the next batch (`Rows::try_recycle`).
    pub(crate) fn clear(&mut self) {
        self.words.clear();
    }

    /// Whether no bit is set.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// The words up to the last one with a bit set.
    fn significant(&self) -> &[u64] {
        let end = self
            .words
            .iter()
            .rposition(|w| *w != 0)
            .map_or(0, |last| last + 1);
        &self.words[..end]
    }

    /// Whether every set bit is below `len`.
    pub(crate) fn fits(&self, len: usize) -> bool {
        match self.words.iter().enumerate().rev().find(|(_, w)| **w != 0) {
            None => true,
            Some((at, word)) => at * 64 + (63 - word.leading_zeros() as usize) < len,
        }
    }
}

impl PartialEq for Bitmap {
    fn eq(&self, other: &Self) -> bool {
        self.significant() == other.significant()
    }
}

impl Eq for Bitmap {}

/// A many2one key with a sub-specification: one position per parent row into a child batch of
/// the distinct targets, [`NO_TARGET`] for `False`.
#[derive(Debug, Clone)]
pub struct Targets {
    child: Rows,
    at: Vec<u32>,
}

impl Targets {
    /// Targets at `at` (one per parent row) in `child`.
    ///
    /// # Errors
    /// `Programming` when a position is past the child batch and not [`NO_TARGET`].
    pub fn new(child: Rows, at: Vec<u32>) -> Result<Self> {
        let len = child.len();
        if let Some((row, pos)) = at
            .iter()
            .enumerate()
            .find(|(_, pos)| **pos != NO_TARGET && **pos as usize >= len)
        {
            return Err(Error::programming(format!(
                "parent row {row} points at child row {pos}, past the {len} child rows"
            )));
        }
        Ok(Self { child, at })
    }

    /// The child batch.
    #[must_use]
    pub fn child(&self) -> &Rows {
        &self.child
    }

    /// The child row of parent row `row`: `Ok(None)` when it reads as `False`.
    ///
    /// # Errors
    /// `Programming` when `row` is out of range.
    pub fn target(&self, row: usize) -> Result<Option<usize>> {
        match self.at.get(row) {
            None => Err(out_of_range(row, self.at.len())),
            Some(&NO_TARGET) => Ok(None),
            Some(pos) => Ok(Some(*pos as usize)),
        }
    }
}

/// An x2many key with a sub-specification: per parent row a range of positions, either straight
/// child rows (one2many: children are contiguous and never repeated) or indices into a child batch
/// of distinct targets (many2many). Shared by the row handles of its parents.
#[derive(Debug, Clone)]
pub struct Children(pub(crate) Arc<ChildLevel>);

#[derive(Debug, Clone)]
pub(crate) struct ChildLevel {
    pub(crate) child: Rows,
    /// `parents + 1` entries; parent `i` owns positions `offsets[i]..offsets[i + 1]`.
    pub(crate) offsets: Vec<u32>,
    /// `None`: a position is a child row; `Some`: a position indexes this list of child rows.
    pub(crate) at: Option<Vec<u32>>,
}

impl Children {
    /// Contiguous children (one2many): parent `i` owns child rows `offsets[i]..offsets[i + 1]`.
    ///
    /// # Errors
    /// `Programming` when `offsets` is empty, decreases, or ends past the child batch.
    pub fn contiguous(child: Rows, offsets: Vec<u32>) -> Result<Self> {
        let positions = child.len();
        Self::checked(
            ChildLevel {
                child,
                offsets,
                at: None,
            },
            positions,
        )
    }

    /// Indexed children (many2many): parent `i` owns the child rows `at[offsets[i]..offsets[i + 1]]`.
    ///
    /// # Errors
    /// As [`Children::contiguous`], and when an index is past the child batch.
    pub fn indexed(child: Rows, offsets: Vec<u32>, at: Vec<u32>) -> Result<Self> {
        let len = child.len();
        if let Some((pos, row)) = at.iter().enumerate().find(|(_, row)| **row as usize >= len) {
            return Err(Error::programming(format!(
                "child position {pos} points at child row {row}, past the {len} child rows"
            )));
        }
        let positions = at.len();
        Self::checked(
            ChildLevel {
                child,
                offsets,
                at: Some(at),
            },
            positions,
        )
    }

    fn checked(level: ChildLevel, positions: usize) -> Result<Self> {
        let Some(last) = level.offsets.last() else {
            return Err(Error::programming(
                "child offsets need one entry more than there are parent rows; got none",
            ));
        };
        if let Some(i) = level.offsets.windows(2).position(|w| w[0] > w[1]) {
            return Err(Error::programming(format!(
                "child offsets decrease at parent row {i}"
            )));
        }
        if *last as usize > positions {
            return Err(Error::programming(format!(
                "child offsets end at {last}, past the {positions} child positions"
            )));
        }
        Ok(Self(Arc::new(level)))
    }

    /// The child batch.
    #[must_use]
    pub fn child(&self) -> &Rows {
        &self.0.child
    }

    /// How many parent rows.
    #[must_use]
    pub fn parents(&self) -> usize {
        self.0.offsets.len().saturating_sub(1)
    }

    /// The positions of parent row `row`, as `(start, end)`.
    pub(crate) fn range(&self, row: usize) -> Option<(u32, u32)> {
        let start = *self.0.offsets.get(row)?;
        let end = *self.0.offsets.get(row.checked_add(1)?)?;
        Some((start, end))
    }
}

/// The values of one column. Text, bytes, JSON and id lists share the cache's
/// `Arc`s. Lengths are checked when the batch is built ([`super::RowsBuilder::finish`]).
///
/// `False` has one representation per column, as in the transaction cache: `None` in the
/// optional columns, [`NO_TARGET`] in [`ColumnData::One`], a bit of the column's falsy bitmap in
/// the others ([`Column::with_false`]).
///
/// Deliberately not `#[non_exhaustive]` (unlike [`crate::Value`]): every
/// consumer lives in this workspace, and the matches over it (the batch builder in `erp-orm`, the
/// views and writer here) list every variant, so a new kind fails to compile wherever it must be
/// handled instead of reaching a fallback arm at run time.
#[derive(Debug, Clone)]
pub enum ColumnData {
    /// Booleans, one bit per row.
    Bool(Bitmap),
    /// Integers, raw many2one ids, many2one references.
    Int(Vec<i64>),
    /// Floats.
    Float(Vec<f64>),
    /// Exact decimals.
    Decimal(Vec<Decimal>),
    /// Text.
    Str(Vec<Option<Arc<str>>>),
    /// Raw bytes (written as base64).
    Bytes(Vec<Option<Arc<[u8]>>>),
    /// Dates.
    Date(Vec<Date>),
    /// Naive UTC datetimes.
    DateTime(Vec<DateTime>),
    /// Native UUIDs, with NULL represented by the column's falsy bitmap.
    Uuid(Vec<crate::Uuid>),
    /// Exact signed bigint values.
    BigInt(Vec<crate::BigInt>),
    /// Unsigned full-width transaction ids.
    Xid8(Vec<crate::Xid8>),
    /// Timezone-aware microsecond instants.
    TimestampTz(Vec<crate::TimestampTz>),
    /// JSON documents.
    Json(Vec<Option<Arc<serde_json::Value>>>),
    /// Date or datetime ranges.
    Range(Vec<Option<RangeValue>>),
    /// x2many ids without a sub-specification.
    Ids(Vec<Option<Arc<[RecId]>>>),
    /// many2one as `[id, label]` (`load='_classic_read'`).
    Label {
        /// The ids.
        ids: Vec<i64>,
        /// The labels (`None` reads as `False`).
        names: Vec<Option<Arc<str>>>,
    },
    /// many2one with a sub-specification.
    One(Targets),
    /// x2many with a sub-specification.
    Many(Children),
    /// Any value; also a column an override retyped ([`Rows::set`](super::Rows::set)).
    Dyn(Vec<Value>),
}

impl ColumnData {
    /// What the column holds, for error messages.
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "booleans",
            Self::Int(_) => "integers",
            Self::Float(_) => "floats",
            Self::Decimal(_) => "decimals",
            Self::Str(_) => "text",
            Self::Bytes(_) => "bytes",
            Self::Date(_) => "dates",
            Self::DateTime(_) => "datetimes",
            Self::Uuid(_) => "UUIDs",
            Self::BigInt(_) => "signed bigint values",
            Self::Xid8(_) => "full transaction ids",
            Self::TimestampTz(_) => "timezone-aware instants",
            Self::Json(_) => "JSON documents",
            Self::Range(_) => "ranges",
            Self::Ids(_) => "id lists",
            Self::Label { .. } => "id and label pairs",
            Self::One(_) => "a child row",
            Self::Many(_) => "child rows",
            Self::Dyn(_) => "dynamic values",
        }
    }

    /// Whether `False` is a bit of the falsy bitmap in this column.
    fn uses_falsy(&self) -> bool {
        matches!(
            self,
            Self::Bool(_)
                | Self::Int(_)
                | Self::Float(_)
                | Self::Decimal(_)
                | Self::Date(_)
                | Self::DateTime(_)
                | Self::Uuid(_)
                | Self::BigInt(_)
                | Self::Xid8(_)
                | Self::TimestampTz(_)
                | Self::Label { .. }
        )
    }

    /// Whether the data holds exactly `len` rows.
    fn holds(&self, len: usize) -> bool {
        match self {
            Self::Bool(bits) => bits.fits(len),
            Self::Int(v) => v.len() == len,
            Self::Float(v) => v.len() == len,
            Self::Decimal(v) => v.len() == len,
            Self::Str(v) => v.len() == len,
            Self::Bytes(v) => v.len() == len,
            Self::Date(v) => v.len() == len,
            Self::DateTime(v) => v.len() == len,
            Self::Uuid(v) => v.len() == len,
            Self::BigInt(v) => v.len() == len,
            Self::Xid8(v) => v.len() == len,
            Self::TimestampTz(v) => v.len() == len,
            Self::Json(v) => v.len() == len,
            Self::Range(v) => v.len() == len,
            Self::Ids(v) => v.len() == len,
            Self::Label { ids, names } => ids.len() == len && names.len() == len,
            Self::One(t) => t.at.len() == len,
            Self::Many(c) => c.parents() == len && !c.0.offsets.is_empty(),
            Self::Dyn(v) => v.len() == len,
        }
    }
}

/// One column: its values, which rows read as `False` (for the columns without an optional
/// representation) and which rows lack the key (`None`: every row has it).
#[derive(Debug, Clone)]
pub struct Column {
    data: ColumnData,
    falsy: Bitmap,
    absent: Option<Bitmap>,
}

impl Column {
    /// A column of `data` where no row reads as `False` through the bitmap and every row has the
    /// key.
    #[must_use]
    pub fn new(data: ColumnData) -> Self {
        Self {
            data,
            falsy: Bitmap::new(),
            absent: None,
        }
    }

    /// The rows of `rows` read as `False` (booleans, integers, floats, decimals, dates,
    /// datetimes, labels; the other columns hold `None` or [`NO_TARGET`] instead, and a bit set
    /// for them is refused when the batch is built).
    #[must_use]
    pub fn with_false(mut self, rows: Bitmap) -> Self {
        self.falsy = rows;
        self
    }

    #[must_use]
    pub fn with_absent(mut self, rows: Bitmap) -> Self {
        self.absent = (!rows.is_clear()).then_some(rows);
        self
    }

    /// The column taken apart: its values, its falsy bitmap and its absent bitmap (`None`: every
    /// row has the key). A recycled column ([`super::RowsBuilder::take_column`]) comes back with
    /// every buffer empty and its capacity kept, so a builder can fill it again without
    /// allocating.
    #[must_use]
    pub fn into_parts(self) -> (ColumnData, Bitmap, Option<Bitmap>) {
        (self.data, self.falsy, self.absent)
    }

    /// Empties every buffer and keeps its capacity (`Rows::try_recycle`). Child levels and
    /// dynamic values hold shared data, not a reusable buffer of the column's own: a child column
    /// becomes an empty dynamic one.
    pub(crate) fn clear(&mut self) {
        match &mut self.data {
            ColumnData::Bool(bits) => bits.clear(),
            ColumnData::Int(v) => v.clear(),
            ColumnData::Float(v) => v.clear(),
            ColumnData::Decimal(v) => v.clear(),
            ColumnData::Str(v) => v.clear(),
            ColumnData::Bytes(v) => v.clear(),
            ColumnData::Date(v) => v.clear(),
            ColumnData::DateTime(v) => v.clear(),
            ColumnData::Uuid(v) => v.clear(),
            ColumnData::BigInt(v) => v.clear(),
            ColumnData::Xid8(v) => v.clear(),
            ColumnData::TimestampTz(v) => v.clear(),
            ColumnData::Json(v) => v.clear(),
            ColumnData::Range(v) => v.clear(),
            ColumnData::Ids(v) => v.clear(),
            ColumnData::Label { ids, names } => {
                ids.clear();
                names.clear();
            }
            ColumnData::Dyn(v) => v.clear(),
            ColumnData::One(_) | ColumnData::Many(_) => self.data = ColumnData::Dyn(Vec::new()),
        }
        self.falsy.clear();
        if let Some(absent) = &mut self.absent {
            absent.clear();
        }
    }

    /// The raw values. Rows that read as `False` or lack the key hold placeholders here, so
    /// readers outside this module use [`Column::cell`] (through [`Rows::cell`]).
    pub(crate) fn data(&self) -> &ColumnData {
        &self.data
    }

    /// Whether `row` reads as `False` through the falsy bitmap. `None` and [`NO_TARGET`] in the
    /// columns that use them are not reported here.
    pub(crate) fn is_false(&self, row: usize) -> bool {
        self.falsy.get(row)
    }

    /// Whether `row` lacks the key.
    pub(crate) fn is_absent(&self, row: usize) -> bool {
        self.absent.as_ref().is_some_and(|bits| bits.get(row))
    }

    /// The value of `row` with absence and `False` applied, borrowed from the column; `None`
    /// when `row` is past the data (never, for a validated column and a row below its batch's
    /// length).
    pub(crate) fn cell(&self, row: usize) -> Option<CellRef<'_>> {
        if self.is_absent(row) {
            return Some(CellRef::Absent);
        }
        if self.data.uses_falsy() && self.falsy.get(row) {
            return Some(CellRef::False);
        }
        // `None` is how the optional columns hold `False` (one representation per column, checked
        // when the batch is built), so `map_or(CellRef::False, ..)` is that value, not a default
        Some(match &self.data {
            ColumnData::Bool(bits) => CellRef::Bool(bits.get(row)),
            ColumnData::Int(v) => CellRef::Int(*v.get(row)?),
            ColumnData::Float(v) => CellRef::Float(*v.get(row)?),
            ColumnData::Decimal(v) => CellRef::Decimal(*v.get(row)?),
            ColumnData::Str(v) => v.get(row)?.as_deref().map_or(CellRef::False, CellRef::Str),
            ColumnData::Bytes(v) => v
                .get(row)?
                .as_deref()
                .map_or(CellRef::False, CellRef::Bytes),
            ColumnData::Date(v) => CellRef::Date(*v.get(row)?),
            ColumnData::DateTime(v) => CellRef::DateTime(*v.get(row)?),
            ColumnData::Uuid(v) => CellRef::Uuid(*v.get(row)?),
            ColumnData::BigInt(v) => CellRef::BigInt(*v.get(row)?),
            ColumnData::Xid8(v) => CellRef::Xid8(*v.get(row)?),
            ColumnData::TimestampTz(v) => CellRef::TimestampTz(*v.get(row)?),
            ColumnData::Json(v) => v.get(row)?.as_deref().map_or(CellRef::False, CellRef::Json),
            ColumnData::Range(v) => v.get(row)?.map_or(CellRef::False, CellRef::Range),
            ColumnData::Ids(v) => v.get(row)?.as_deref().map_or(CellRef::False, CellRef::Ids),
            ColumnData::Label { ids, names } => CellRef::Label {
                id: *ids.get(row)?,
                name: names.get(row)?.as_deref(),
            },
            ColumnData::One(t) => match *t.at.get(row)? {
                NO_TARGET => CellRef::False,
                pos => CellRef::Row {
                    rows: &t.child,
                    row: pos as usize,
                },
            },
            ColumnData::Many(c) => {
                let (start, end) = c.range(row)?;
                let range = start as usize..end as usize;
                let positions = match &c.0.at {
                    None => ChildPositions::range(range),
                    Some(at) => ChildPositions::at(at.get(range)?),
                };
                CellRef::Children {
                    rows: &c.0.child,
                    positions,
                }
            }
            ColumnData::Dyn(v) => CellRef::Value(v.get(row)?),
        })
    }

    /// What the column holds, for error messages.
    pub(crate) fn kind_name(&self) -> &'static str {
        self.data.kind_name()
    }

    /// Checks the column against a batch of `len` rows.
    pub(crate) fn validate(
        &self,
        len: usize,
        key: &str,
        of_model: impl std::fmt::Display,
    ) -> Result<()> {
        if !self.data.holds(len) {
            return Err(Error::programming(format!(
                "column '{key}'{of_model} does not hold {len} rows of {}",
                self.data.kind_name()
            )));
        }
        if !self.falsy.fits(len) || self.absent.as_ref().is_some_and(|a| !a.fits(len)) {
            return Err(Error::programming(format!(
                "column '{key}'{of_model} marks rows past its {len} rows"
            )));
        }
        if !self.data.uses_falsy() && !self.falsy.is_clear() {
            return Err(Error::programming(format!(
                "column '{key}'{of_model} holds {}: False is None (or NO_TARGET) there, \
                 not a falsy bit",
                self.data.kind_name()
            )));
        }
        Ok(())
    }

    /// The value of `row`, which has the key; `None` when `row` is past the data (never, for a
    /// validated column).
    pub(crate) fn value(&self, row: usize) -> Option<Value> {
        if self.data.uses_falsy() && self.falsy.get(row) {
            return Some(Value::False);
        }
        Some(match &self.data {
            ColumnData::Bool(bits) => Value::Bool(bits.get(row)),
            ColumnData::Int(v) => Value::Int(*v.get(row)?),
            ColumnData::Float(v) => Value::Float(*v.get(row)?),
            ColumnData::Decimal(v) => Value::Decimal(*v.get(row)?),
            ColumnData::Str(v) => v
                .get(row)?
                .as_ref()
                .map_or(Value::False, |s| Value::Str(Arc::clone(s))),
            ColumnData::Bytes(v) => v
                .get(row)?
                .as_ref()
                .map_or(Value::False, |b| Value::Bytes(Arc::clone(b))),
            ColumnData::Date(v) => Value::Date(*v.get(row)?),
            ColumnData::DateTime(v) => Value::DateTime(*v.get(row)?),
            ColumnData::Uuid(v) => Value::Uuid(*v.get(row)?),
            ColumnData::BigInt(v) => Value::BigInt(*v.get(row)?),
            ColumnData::Xid8(v) => Value::Xid8(*v.get(row)?),
            ColumnData::TimestampTz(v) => Value::TimestampTz(*v.get(row)?),
            ColumnData::Json(v) => v
                .get(row)?
                .as_ref()
                .map_or(Value::False, |j| Value::Json(Arc::clone(j))),
            ColumnData::Range(v) => v.get(row)?.map_or(Value::False, Value::Range),
            ColumnData::Ids(v) => v.get(row)?.as_ref().map_or(Value::False, |ids| {
                Value::List(ids.iter().map(|id| Value::Int(id.get())).collect())
            }),
            ColumnData::Label { ids, names } => {
                let id = *ids.get(row)?;
                let name = names
                    .get(row)?
                    .as_ref()
                    .map_or(Value::False, |s| Value::Str(Arc::clone(s)));
                Value::list([Value::Int(id), name])
            }
            ColumnData::One(t) => match *t.at.get(row)? {
                NO_TARGET => Value::False,
                pos => Value::Row(RowRef::new(t.child.clone(), pos)),
            },
            ColumnData::Many(c) => {
                let (start, end) = c.range(row)?;
                Value::Rows(RowsRef::children(c.clone(), start, end))
            }
            ColumnData::Dyn(v) => v.get(row)?.clone(),
        })
    }

    /// Stores `value` in `row` when the column's type holds it; gives the value back otherwise.
    /// The row's falsy and absent bits follow the stored value.
    fn try_store(&mut self, row: usize, value: Value) -> Result<Option<Value>> {
        let missing = || Error::programming(format!("row {row} is past the column's data"));
        let falsy_column = self.data.uses_falsy();
        let Self {
            data,
            falsy,
            absent,
        } = self;
        match (data, value) {
            (_, Value::False) if falsy_column => falsy.put(row, true),
            (ColumnData::Bool(bits), Value::Bool(b)) => {
                bits.put(row, b);
                falsy.put(row, false);
            }
            (ColumnData::Int(v), Value::Int(i)) => {
                *v.get_mut(row).ok_or_else(missing)? = i;
                falsy.put(row, false);
            }
            (ColumnData::Float(v), Value::Float(f)) => {
                *v.get_mut(row).ok_or_else(missing)? = f;
                falsy.put(row, false);
            }
            (ColumnData::Decimal(v), Value::Decimal(d)) => {
                *v.get_mut(row).ok_or_else(missing)? = d;
                falsy.put(row, false);
            }
            (ColumnData::Date(v), Value::Date(d)) => {
                *v.get_mut(row).ok_or_else(missing)? = d;
                falsy.put(row, false);
            }
            (ColumnData::DateTime(v), Value::DateTime(d)) => {
                *v.get_mut(row).ok_or_else(missing)? = d;
                falsy.put(row, false);
            }
            (ColumnData::Uuid(v), Value::Uuid(value)) => {
                *v.get_mut(row).ok_or_else(missing)? = value;
                falsy.put(row, false);
            }
            (ColumnData::BigInt(v), Value::BigInt(value)) => {
                *v.get_mut(row).ok_or_else(missing)? = value;
                falsy.put(row, false);
            }
            (ColumnData::Xid8(v), Value::Xid8(value)) => {
                *v.get_mut(row).ok_or_else(missing)? = value;
                falsy.put(row, false);
            }
            (ColumnData::TimestampTz(v), Value::TimestampTz(value)) => {
                *v.get_mut(row).ok_or_else(missing)? = value;
                falsy.put(row, false);
            }
            (ColumnData::Str(v), Value::Str(s)) => *v.get_mut(row).ok_or_else(missing)? = Some(s),
            (ColumnData::Str(v), Value::False) => *v.get_mut(row).ok_or_else(missing)? = None,
            (ColumnData::Bytes(v), Value::Bytes(b)) => {
                *v.get_mut(row).ok_or_else(missing)? = Some(b);
            }
            (ColumnData::Bytes(v), Value::False) => *v.get_mut(row).ok_or_else(missing)? = None,
            (ColumnData::Json(v), Value::Json(j)) => *v.get_mut(row).ok_or_else(missing)? = Some(j),
            (ColumnData::Json(v), Value::False) => *v.get_mut(row).ok_or_else(missing)? = None,
            (ColumnData::Range(v), Value::Range(r)) => {
                *v.get_mut(row).ok_or_else(missing)? = Some(r);
            }
            (ColumnData::Range(v), Value::False) => *v.get_mut(row).ok_or_else(missing)? = None,
            (ColumnData::Ids(v), Value::False) => *v.get_mut(row).ok_or_else(missing)? = None,
            (ColumnData::One(t), Value::False) => {
                *t.at.get_mut(row).ok_or_else(missing)? = NO_TARGET;
            }
            (ColumnData::Dyn(v), value) => *v.get_mut(row).ok_or_else(missing)? = value,
            (_, value) => return Ok(Some(value)),
        }
        if let Some(bits) = absent {
            bits.put(row, false);
            if bits.is_clear() {
                *absent = None;
            }
        }
        Ok(None)
    }

    /// Stores `value` in `row`, turning the column into [`ColumnData::Dyn`] first when its type
    /// cannot hold the value (one conversion per column).
    pub(crate) fn store(&mut self, row: usize, len: usize, value: Value) -> Result<()> {
        let Some(value) = self.try_store(row, value)? else {
            return Ok(());
        };
        self.retype_dyn(len)?;
        match self.try_store(row, value)? {
            None => Ok(()),
            Some(_) => Err(Error::programming(
                "a dynamic column refused a value (internal error in erp_fields::rows)",
            )),
        }
    }

    /// Turns the column into [`ColumnData::Dyn`]: O(rows), one allocation, plus the lists of an
    /// id-list or label column. Child levels stay shared through the row handles.
    fn retype_dyn(&mut self, len: usize) -> Result<()> {
        if matches!(self.data, ColumnData::Dyn(_)) {
            return Ok(());
        }
        let mut values = Vec::with_capacity(len);
        for row in 0..len {
            if self.is_absent(row) {
                // the absent bit stays set; the placeholder is never read
                values.push(Value::False);
            } else {
                values.push(self.value(row).ok_or_else(|| {
                    Error::programming(format!(
                        "row {row} is past the column's data (internal error in erp_fields::rows)"
                    ))
                })?);
            }
        }
        self.data = ColumnData::Dyn(values);
        self.falsy = Bitmap::new();
        Ok(())
    }

    /// A column of `len` rows that lack the key (a key added by an override).
    pub(crate) fn absent_everywhere(len: usize) -> Self {
        Self {
            data: ColumnData::Dyn(vec![Value::False; len]),
            falsy: Bitmap::new(),
            absent: (len > 0).then(|| Bitmap::all(len)),
        }
    }

    /// The child level of a relational column, for [`super::Rows::children_mut`].
    pub(crate) fn child_mut(&mut self) -> Option<&mut Rows> {
        match &mut self.data {
            ColumnData::One(t) => Some(&mut t.child),
            ColumnData::Many(c) => Some(&mut Arc::make_mut(&mut c.0).child),
            _ => None,
        }
    }
}

/// One value of a batch as readers and writers see it ([`Rows::cell`]): absence and `False`
/// already applied, everything borrowed from the batch, nothing allocated. The JSON-2 writer
/// writes rows from these.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CellRef<'a> {
    /// The row lacks the key (not written at all).
    Absent,
    /// `False`.
    False,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// An exact decimal.
    Decimal(Decimal),
    /// Text.
    Str(&'a str),
    /// Raw bytes.
    Bytes(&'a [u8]),
    /// A date.
    Date(Date),
    /// A naive UTC datetime.
    DateTime(DateTime),
    /// A native UUID (not ordinary text).
    Uuid(crate::Uuid),
    /// Exact signed bigint cell.
    BigInt(crate::BigInt),
    /// A native unsigned full-width transaction id.
    Xid8(crate::Xid8),
    /// A timezone-aware microsecond instant.
    TimestampTz(crate::TimestampTz),
    /// A JSON document.
    Json(&'a serde_json::Value),
    /// A date or datetime range.
    Range(RangeValue),
    /// x2many ids without a sub-specification.
    Ids(&'a [RecId]),
    /// many2one as `[id, label]`; `None` for a label that reads as `False`.
    Label {
        /// The id.
        id: i64,
        /// The label.
        name: Option<&'a str>,
    },
    /// One child row (many2one with a sub-specification).
    Row {
        /// The child batch.
        rows: &'a Rows,
        /// The child row.
        row: usize,
    },
    /// Child rows (x2many with a sub-specification).
    Children {
        /// The child batch.
        rows: &'a Rows,
        /// The child rows, in order.
        positions: ChildPositions<'a>,
    },
    /// A value of a dynamic column.
    Value(&'a Value),
}

/// `Programming`: `row 12 is out of range (10 rows)`.
pub(crate) fn out_of_range(row: usize, len: usize) -> Error {
    Error::programming(format!("row {row} is out of range ({len} rows)"))
}
