//! Columnar result batches.
//!
//! Every row-shaped result is a [`Rows`] batch: one shared [`Layout`] (the keys, held once, never
//! per row), one typed [`Column`] per key, and a row count. Relational keys with a
//! sub-specification point at child batches by position ([`Targets`], [`Children`]), never by
//! copy. A row is read through a handle ([`RowRef`], 16 bytes) or the dict view
//! ([`Value::dict`](crate::Value::dict)); the JSON-2 writer writes rows straight from the
//! columns.
//!
//! Overrides change a result by key and row index ([`Rows::set`], [`Rows::insert_key`],
//! [`Rows::remove_key`], [`Rows::children_mut`]). Mutation is copy-on-write: free for the sole
//! owner (a result from `super` is), a batch copy while a [`RowRef`] or [`RowsRef`] into it is still
//! alive, so drop handles before mutating.
//!
//! ```
//! use std::sync::Arc;
//! use erp_fields::rows::{Column, ColumnData, Key, Layout, Rows, RowsBuilder};
//! use erp_fields::Value;
//!
//! let layout = Layout::for_model("bench.order.line", [Key::from("id"), Key::from("qty")])?;
//! let mut builder = RowsBuilder::new(layout, 2)?;
//! builder.column("id", Column::new(ColumnData::Int(vec![7, 8])))?;
//! builder.column("qty", Column::new(ColumnData::Float(vec![1.0, 0.0])))?;
//! let mut rows = builder.finish()?;
//!
//! // an override blanks zero quantities: the float column gains a false bit
//! for i in rows.indices() {
//!     if rows.f64(i, "qty")? == Some(0.0) {
//!         rows.set(i, "qty", Value::False)?;
//!     }
//! }
//! assert_eq!(rows.get(1, "qty")?, Value::False);
//! assert_eq!(
//!     rows.get(0, "qtyy").unwrap_err().message(),
//!     "bench.order.line row has no key 'qtyy' (keys: id, qty)"
//! );
//! # Ok::<(), erp_fields::Error>(())
//! ```

mod column;
/// Equality, hashing and display of the handles are also used by `Value`'s impls.
pub(crate) mod handle;
mod layout;
mod maps;

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

pub use column::{Bitmap, CellRef, Children, Column, ColumnData, NO_TARGET, Targets};
pub use handle::{ChildPositions, DictRef, RowRef, RowsRef};
pub use layout::{Key, Layout, MAX_KEYS};

use crate::{Error, Result, Value};
use column::out_of_range;

/// Most rows a batch holds: a row position fits a `u32`, with `u32::MAX` kept for
/// [`NO_TARGET`].
pub const MAX_ROWS: usize = u32::MAX as usize - 1;

#[derive(Clone)]
struct Batch {
    layout: Arc<Layout>,
    /// One per layout key, in layout order.
    columns: Vec<Column>,
    len: u32,
}

/// An owned columnar batch of rows. Cloning shares it (one `Arc`); it outlives the
/// transaction that built it and is `Send + Sync`.
#[derive(Clone)]
pub struct Rows(Arc<Batch>);

/// Builds a [`Rows`] batch column by column: the read path gathers each key's values from the
/// cache into a typed vector, then hands the vector over.
pub struct RowsBuilder {
    layout: Arc<Layout>,
    len: usize,
    columns: Vec<Option<Column>>,
    /// The emptied batch of a recycled [`Rows`] ([`Rows::try_recycle`]), its one owner: `finish`
    /// fills it again instead of allocating a new one.
    spare: Option<Arc<Batch>>,
}

impl fmt::Debug for RowsBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RowsBuilder")
            .field("layout", &self.layout)
            .field("len", &self.len)
            .field("columns", &self.columns)
            .field("recycled", &self.spare.is_some())
            .finish()
    }
}

// `Rows::try_recycle` and `RowsBuilder::finish` turn the column vector of a batch into the
// builder's and back in place (the standard library reuses a vector's buffer for `into_iter` +
// `map` + `collect` when both element types have the same size and alignment): no allocation
// per recycled batch. A layout change that broke this would make recycling allocate silently,
// so it fails to compile instead.
const _: () = {
    assert!(std::mem::size_of::<Option<Column>>() == std::mem::size_of::<Column>());
    assert!(std::mem::align_of::<Option<Column>>() == std::mem::align_of::<Column>());
};

impl RowsBuilder {
    /// A builder for `len` rows of `layout`.
    ///
    /// # Errors
    /// `Programming` when `len` exceeds [`MAX_ROWS`].
    pub fn new(layout: Arc<Layout>, len: usize) -> Result<Self> {
        if len > MAX_ROWS {
            return Err(Error::programming(format!(
                "a row batch holds at most {MAX_ROWS} rows, not {len}"
            )));
        }
        let mut columns = Vec::with_capacity(layout.len());
        columns.resize_with(layout.len(), || None);
        Ok(Self {
            layout,
            len,
            columns,
            spare: None,
        })
    }

    /// The layout.
    #[must_use]
    pub fn layout(&self) -> &Arc<Layout> {
        &self.layout
    }

    /// Sets the number of rows of the batch (a recycled builder starts at 0).
    ///
    /// # Errors
    /// `Programming` when `len` exceeds [`MAX_ROWS`].
    pub fn set_len(&mut self, len: usize) -> Result<&mut Self> {
        if len > MAX_ROWS {
            return Err(Error::programming(format!(
                "a row batch holds at most {MAX_ROWS} rows, not {len}"
            )));
        }
        self.len = len;
        Ok(self)
    }

    /// Takes the column at layout position `pos` out of the builder: the emptied column of a
    /// recycled batch ([`Rows::try_recycle`]), whose buffers keep their capacity, or a column
    /// given before; `None` when the position holds none. The position must be given again
    /// ([`RowsBuilder::column_at`]) before [`RowsBuilder::finish`].
    pub fn take_column(&mut self, pos: usize) -> Option<Column> {
        self.columns.get_mut(pos).and_then(Option::take)
    }

    /// Sets the column of `key`.
    ///
    /// # Errors
    /// `Programming` for a key the layout lacks or a key given twice.
    pub fn column(&mut self, key: &str, column: Column) -> Result<&mut Self> {
        let pos = self
            .layout
            .position(key)
            .ok_or_else(|| self.layout.unknown_key(key))?;
        self.column_at(pos, column)
    }

    /// Sets the column at layout position `pos`.
    ///
    /// # Errors
    /// `Programming` for a position past the layout or a column given twice.
    pub fn column_at(&mut self, pos: usize, column: Column) -> Result<&mut Self> {
        let of_model = self.layout.of_model();
        let (Some(key), Some(slot)) = (self.layout.key(pos), self.columns.get_mut(pos)) else {
            return Err(Error::programming(format!(
                "column position {pos} is past the {} keys{of_model}",
                self.layout.len()
            )));
        };
        if slot.is_some() {
            return Err(Error::programming(format!(
                "column '{key}'{of_model} is given twice"
            )));
        }
        *slot = Some(column);
        Ok(self)
    }

    /// The batch, after checking that every key has a column holding exactly the batch's rows.
    ///
    /// # Errors
    /// `Programming` for a missing column, a column of another length, a falsy bit in a column
    /// that holds `False` as `None`, or a bitmap marking rows past the batch.
    pub fn finish(self) -> Result<Rows> {
        let of_model = self.layout.of_model();
        for (pos, column) in self.columns.iter().enumerate() {
            let Some(key) = self.layout.key(pos) else {
                return Err(inconsistent(
                    "the builder has more columns than its layout keys",
                ));
            };
            let Some(column) = column else {
                return Err(Error::programming(format!(
                    "column '{key}'{of_model} was not given"
                )));
            };
            column.validate(self.len, key, of_model)?;
        }
        // every column is `Some`, checked above; the vector's buffer is reused (see the size
        // assertion next to `RowsBuilder`)
        let columns = self
            .columns
            .into_iter()
            .map(|column| column.ok_or_else(|| inconsistent("a checked column is missing")))
            .collect::<Result<Vec<Column>>>()?;
        let batch = Batch {
            layout: self.layout,
            columns,
            // `len` ≤ MAX_ROWS, checked in `new` and `set_len`
            len: self.len as u32,
        };
        match self.spare {
            Some(mut spare) => match Arc::get_mut(&mut spare) {
                Some(slot) => {
                    *slot = batch;
                    Ok(Rows(spare))
                }
                // the builder is the spare's only owner: no clone of it was ever made
                None => Err(inconsistent("a recycled batch is shared")),
            },
            None => Ok(Rows(Arc::new(batch))),
        }
    }
}

impl Rows {
    /// Takes the batch's buffers back for the next batch of the same layout (design note
    /// `perf-stream.md` §3.6): when this handle is the batch's only owner (no clone, no
    /// [`RowRef`] or [`RowsRef`] into it is alive), a builder with every column emptied, its
    /// buffers' capacity kept, and the batch's own allocation kept for [`RowsBuilder::finish`];
    /// the builder has 0 rows ([`RowsBuilder::set_len`]) and each column is taken out with
    /// [`RowsBuilder::take_column`] and given back with [`RowsBuilder::column_at`]. Otherwise the
    /// batch is given back unchanged, and whoever shares it keeps reading the same rows.
    ///
    /// Shared values (text, JSON, id lists, child levels) are released, not reused: only the
    /// column vectors and bitmaps of this batch are.
    ///
    /// # Errors
    /// The batch itself when it is shared.
    pub fn try_recycle(mut self) -> std::result::Result<RowsBuilder, Rows> {
        let Some(batch) = Arc::get_mut(&mut self.0) else {
            return Err(self);
        };
        let layout = Arc::clone(&batch.layout);
        let mut columns = std::mem::take(&mut batch.columns);
        for column in &mut columns {
            column.clear();
        }
        batch.len = 0;
        // the vector's buffer is reused (see the size assertion next to `RowsBuilder`)
        let columns: Vec<Option<Column>> = columns.into_iter().map(Some).collect();
        Ok(RowsBuilder {
            layout,
            len: 0,
            columns,
            spare: Some(self.0),
        })
    }

    /// A batch of no rows with `layout`.
    #[must_use]
    pub fn empty(layout: Arc<Layout>) -> Self {
        let columns = (0..layout.len())
            .map(|_| Column::new(ColumnData::Dyn(Vec::new())))
            .collect();
        Self(Arc::new(Batch {
            layout,
            columns,
            len: 0,
        }))
    }

    /// The layout.
    #[must_use]
    pub fn layout(&self) -> &Arc<Layout> {
        &self.0.layout
    }

    /// The model the rows belong to, when known.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.0.layout.model()
    }

    /// How many rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len as usize
    }

    /// Whether there is no row.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.len == 0
    }

    /// The row positions, `0..len`.
    #[must_use]
    pub fn indices(&self) -> std::ops::Range<usize> {
        0..self.len()
    }

    /// The keys, in layout order.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.0.layout.keys()
    }

    /// Whether the layout has `key` (a row may still lack it: [`RowRef::contains`]).
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.layout.position(key).is_some()
    }

    /// The column at layout position `pos`.
    pub(crate) fn column(&self, pos: usize) -> Option<&Column> {
        self.0.columns.get(pos)
    }

    /// The value at layout position `pos` in row `row`, borrowed and resolved: [`CellRef::Absent`]
    /// when the row lacks the key, [`CellRef::False`] for `False`, child levels by position.
    /// Allocates nothing; writers and bulk readers use it instead of building [`Value`]s.
    ///
    /// # Errors
    /// `Programming` for a row out of range or a position past the layout.
    pub fn cell(&self, row: usize, pos: usize) -> Result<CellRef<'_>> {
        self.check_row(row)?;
        let Some(column) = self.0.columns.get(pos) else {
            return Err(Error::programming(format!(
                "key position {pos} is past the {} keys{}",
                self.0.layout.len(),
                self.0.layout.of_model()
            )));
        };
        column
            .cell(row)
            .ok_or_else(|| inconsistent("a column is shorter than its batch"))
    }

    /// Whether both handles share one batch.
    pub(crate) fn same_batch(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn check_row(&self, row: usize) -> Result<()> {
        if row < self.len() {
            Ok(())
        } else {
            Err(self.row_out_of_range(row))
        }
    }

    fn row_out_of_range(&self, row: usize) -> Error {
        match self.model() {
            Some(model) => Error::programming(format!(
                "row {row} of {model} rows is out of range ({} rows)",
                self.len()
            )),
            None => out_of_range(row, self.len()),
        }
    }

    /// The position of `key`, or the unknown-key error.
    fn position(&self, key: &str) -> Result<usize> {
        self.0
            .layout
            .position(key)
            .ok_or_else(|| self.0.layout.unknown_key(key))
    }

    fn column_of(&self, pos: usize) -> Result<&Column> {
        self.0
            .columns
            .get(pos)
            .ok_or_else(|| inconsistent("a layout key has no column"))
    }

    /// The handle of row `row`.
    ///
    /// # Errors
    /// `Programming` when `row` is out of range.
    pub fn row(&self, row: usize) -> Result<RowRef> {
        self.check_row(row)?;
        // `row` < len ≤ MAX_ROWS < u32::MAX
        Ok(RowRef::new(self.clone(), row as u32))
    }

    /// Every row as a handle.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = RowRef> + '_ {
        // every position < len ≤ MAX_ROWS < u32::MAX
        (0..self.0.len).map(|row| RowRef::new(self.clone(), row))
    }

    /// The value of `key` in row `row`; relational keys read as [`Value::Row`] or
    /// [`Value::Rows`] handles into their child batch.
    ///
    /// # Errors
    /// `Programming` for a key the layout lacks, a row out of range, or a key absent in that
    /// row (use [`Rows::get_opt`] when absence is expected).
    pub fn get(&self, row: usize, key: &str) -> Result<Value> {
        self.get_opt(row, key)?.ok_or_else(|| {
            Error::programming(format!(
                "key '{key}' is absent in row {row}{}",
                self.0.layout.of_model()
            ))
        })
    }

    /// The value of `key` in row `row`, `Ok(None)` when that row lacks the key.
    ///
    /// # Errors
    /// `Programming` for a key the layout lacks or a row out of range.
    pub fn get_opt(&self, row: usize, key: &str) -> Result<Option<Value>> {
        let pos = self.position(key)?;
        self.get_at(row, pos)
    }

    /// The value at layout position `pos` in row `row`, `Ok(None)` when that row lacks the key.
    ///
    /// # Errors
    /// `Programming` for a position past the layout or a row out of range.
    pub fn get_at(&self, row: usize, pos: usize) -> Result<Option<Value>> {
        self.check_row(row)?;
        let column = self.column_of(pos)?;
        if column.is_absent(row) {
            return Ok(None);
        }
        column
            .value(row)
            .map(Some)
            .ok_or_else(|| inconsistent("a column is shorter than its batch"))
    }

    /// The text of `key` in row `row` without copying it: `Ok(None)` for `False`.
    ///
    /// # Errors
    /// As [`Rows::get`], and `Programming` when the key holds something other than text.
    pub fn str(&self, row: usize, key: &str) -> Result<Option<&str>> {
        let column = self.present(row, key)?;
        match column.data() {
            column::ColumnData::Str(v) => v
                .get(row)
                .map(|s| s.as_deref())
                .ok_or_else(|| inconsistent("a column is shorter than its batch")),
            column::ColumnData::Dyn(v) => match v.get(row) {
                Some(Value::Str(s)) => Ok(Some(s)),
                Some(Value::False) => Ok(None),
                other => Err(self.not_a(row, key, other, "text")),
            },
            _ => Err(self.column_not_a(key, column, "text")),
        }
    }

    /// The integer of `key` in row `row`: `Ok(None)` for `False`.
    ///
    /// # Errors
    /// As [`Rows::get`], and `Programming` when the key holds something other than integers.
    pub fn i64(&self, row: usize, key: &str) -> Result<Option<i64>> {
        let column = self.present(row, key)?;
        match column.data() {
            column::ColumnData::BigInt(v) if !column.is_false(row) => v
                .get(row)
                .map(|i| Some(i.get()))
                .ok_or_else(|| inconsistent("a column is shorter than its batch")),
            column::ColumnData::BigInt(_) => Ok(None),
            column::ColumnData::Int(v) if !column.is_false(row) => v
                .get(row)
                .map(|i| Some(*i))
                .ok_or_else(|| inconsistent("a column is shorter than its batch")),
            column::ColumnData::Int(_) => Ok(None),
            column::ColumnData::Dyn(v) => match v.get(row) {
                Some(Value::BigInt(i)) => Ok(Some(i.get())),
                Some(Value::Int(i)) => Ok(Some(*i)),
                Some(Value::False) => Ok(None),
                other => Err(self.not_a(row, key, other, "an integer")),
            },
            _ => Err(self.column_not_a(key, column, "an integer")),
        }
    }

    /// The float of `key` in row `row`: `Ok(None)` for `False`.
    ///
    /// # Errors
    /// As [`Rows::get`], and `Programming` when the key holds something other than floats.
    pub fn f64(&self, row: usize, key: &str) -> Result<Option<f64>> {
        let column = self.present(row, key)?;
        match column.data() {
            column::ColumnData::Float(v) if !column.is_false(row) => v
                .get(row)
                .map(|f| Some(*f))
                .ok_or_else(|| inconsistent("a column is shorter than its batch")),
            column::ColumnData::Float(_) => Ok(None),
            column::ColumnData::Dyn(v) => match v.get(row) {
                Some(Value::Float(f)) => Ok(Some(*f)),
                Some(Value::False) => Ok(None),
                other => Err(self.not_a(row, key, other, "a float")),
            },
            _ => Err(self.column_not_a(key, column, "a float")),
        }
    }

    /// The boolean of `key` in row `row`: `Ok(None)` for `False` (not `Bool(false)`).
    ///
    /// # Errors
    /// As [`Rows::get`], and `Programming` when the key holds something other than booleans.
    pub fn bool(&self, row: usize, key: &str) -> Result<Option<bool>> {
        let column = self.present(row, key)?;
        match column.data() {
            column::ColumnData::Bool(bits) if !column.is_false(row) => Ok(Some(bits.get(row))),
            column::ColumnData::Bool(_) => Ok(None),
            column::ColumnData::Dyn(v) => match v.get(row) {
                Some(Value::Bool(b)) => Ok(Some(*b)),
                Some(Value::False) => Ok(None),
                other => Err(self.not_a(row, key, other, "a boolean")),
            },
            _ => Err(self.column_not_a(key, column, "a boolean")),
        }
    }

    /// The column of `key` when row `row` has the key.
    fn present(&self, row: usize, key: &str) -> Result<&Column> {
        let pos = self.position(key)?;
        self.check_row(row)?;
        let column = self.column_of(pos)?;
        if column.is_absent(row) {
            return Err(Error::programming(format!(
                "key '{key}' is absent in row {row}{}",
                self.0.layout.of_model()
            )));
        }
        Ok(column)
    }

    fn column_not_a(&self, key: &str, column: &Column, wanted: &str) -> Error {
        Error::programming(format!(
            "key '{key}'{} holds {}, not {wanted}",
            self.0.layout.of_model(),
            column.kind_name()
        ))
    }

    fn not_a(&self, row: usize, key: &str, value: Option<&Value>, wanted: &str) -> Error {
        let Some(value) = value else {
            return inconsistent("a dynamic column is shorter than its batch");
        };
        Error::programming(format!(
            "key '{key}' in row {row}{} holds {value}, not {wanted}",
            self.0.layout.of_model()
        ))
    }

    /// The children of row `row` under the x2many key `key`.
    ///
    /// # Errors
    /// As [`Rows::get`], and `Programming` when `key` holds no child rows.
    pub fn children(&self, row: usize, key: &str) -> Result<RowsRef> {
        let column = self.present(row, key)?;
        match column.data() {
            column::ColumnData::Many(c) => {
                let (start, end) = c
                    .range(row)
                    .ok_or_else(|| inconsistent("child offsets are shorter than their batch"))?;
                Ok(RowsRef::children(c.clone(), start, end))
            }
            _ => Err(self.column_not_a(key, column, "child rows")),
        }
    }

    /// The positions in the child batch of row `row`'s children under the relational key `key`
    /// (none for a many2one that reads as `False`).
    ///
    /// # Errors
    /// As [`Rows::children`].
    pub fn child_positions(&self, row: usize, key: &str) -> Result<ChildPositions<'_>> {
        let column = self.present(row, key)?;
        match column.data() {
            column::ColumnData::Many(c) => {
                let (start, end) = c
                    .range(row)
                    .ok_or_else(|| inconsistent("child offsets are shorter than their batch"))?;
                let range = start as usize..end as usize;
                match &c.0.at {
                    None => Ok(ChildPositions::range(range)),
                    Some(at) => at
                        .get(range)
                        .map(ChildPositions::at)
                        .ok_or_else(|| inconsistent("child offsets end past their positions")),
                }
            }
            column::ColumnData::One(t) => Ok(match t.target(row)? {
                None => ChildPositions::range(0..0),
                Some(target) => ChildPositions::range(target..target + 1),
            }),
            _ => Err(self.column_not_a(key, column, "child rows")),
        }
    }

    fn batch_mut(&mut self) -> &mut Batch {
        Arc::make_mut(&mut self.0)
    }

    /// Sets the value of `key` in row `row` (the row gains the key if it lacked it). A value the
    /// column's type cannot hold turns that column into a dynamic one, once per column; `False`
    /// never retypes a typed column.
    ///
    /// # Errors
    /// `Programming` for a key the layout lacks or a row out of range.
    pub fn set(&mut self, row: usize, key: &str, value: impl Into<Value>) -> Result<()> {
        let pos = self.position(key)?;
        self.check_row(row)?;
        let len = self.len();
        let column = self
            .batch_mut()
            .columns
            .get_mut(pos)
            .ok_or_else(|| inconsistent("a layout key has no column"))?;
        column.store(row, len, value.into())
    }

    /// Adds `key` at the end of the layout, absent in every row until [`Rows::set`] gives it a
    /// value. Copies the layout once.
    ///
    /// # Errors
    /// `Programming` when the layout already has `key` or holds [`MAX_KEYS`] keys.
    pub fn insert_key(&mut self, key: impl Into<Key>) -> Result<()> {
        let key = key.into();
        if self.contains_key(key.as_str()) {
            return Err(Error::programming(format!(
                "key '{key}' already exists{}",
                self.0.layout.in_model()
            )));
        }
        let layout = Arc::new(self.0.layout.with_key(key)?);
        let len = self.len();
        let batch = self.batch_mut();
        batch.layout = layout;
        batch.columns.push(Column::absent_everywhere(len));
        Ok(())
    }

    /// Removes `key` from the layout and its column. Copies the layout once.
    ///
    /// # Errors
    /// `Programming` for a key the layout lacks.
    pub fn remove_key(&mut self, key: &str) -> Result<()> {
        let pos = self.position(key)?;
        let layout = Arc::new(self.0.layout.without(pos)?);
        let batch = self.batch_mut();
        if pos >= batch.columns.len() {
            return Err(inconsistent("a layout key has no column"));
        }
        batch.columns.remove(pos);
        batch.layout = layout;
        Ok(())
    }

    /// The whole child level of the relational key `key`, for changing it by key (its row count
    /// cannot change: the parents' positions point into it).
    ///
    /// # Errors
    /// `Programming` for a key the layout lacks or a key that holds no child rows.
    pub fn children_mut(&mut self, key: &str) -> Result<RowsMut<'_>> {
        let pos = self.position(key)?;
        let column = self.column_of(pos)?;
        if !matches!(column.data(), ColumnData::One(_) | ColumnData::Many(_)) {
            return Err(self.column_not_a(key, column, "child rows"));
        }
        self.batch_mut()
            .columns
            .get_mut(pos)
            .and_then(Column::child_mut)
            .map(RowsMut)
            .ok_or_else(|| inconsistent("a relational column lost its child level"))
    }

    /// Every row as a [`Value::Row`] handle (32 bytes each, one vector).
    #[must_use]
    pub fn into_values(self) -> Vec<Value> {
        let mut values = Vec::with_capacity(self.len());
        values.extend(self.iter().map(Value::Row));
        values
    }
}

/// A child level borrowed for changes by key ([`Rows::children_mut`]). Reads through `Deref`;
/// it cannot be replaced, so its row count stays what the parents' positions expect.
pub struct RowsMut<'a>(&'a mut Rows);

impl Deref for RowsMut<'_> {
    type Target = Rows;

    fn deref(&self) -> &Rows {
        self.0
    }
}

impl RowsMut<'_> {
    /// [`Rows::set`] on the child level.
    ///
    /// # Errors
    /// As [`Rows::set`].
    pub fn set(&mut self, row: usize, key: &str, value: impl Into<Value>) -> Result<()> {
        self.0.set(row, key, value)
    }

    /// [`Rows::insert_key`] on the child level.
    ///
    /// # Errors
    /// As [`Rows::insert_key`].
    pub fn insert_key(&mut self, key: impl Into<Key>) -> Result<()> {
        self.0.insert_key(key)
    }

    /// [`Rows::remove_key`] on the child level.
    ///
    /// # Errors
    /// As [`Rows::remove_key`].
    pub fn remove_key(&mut self, key: &str) -> Result<()> {
        self.0.remove_key(key)
    }

    /// [`Rows::children_mut`] on the child level.
    ///
    /// # Errors
    /// As [`Rows::children_mut`].
    pub fn children_mut(&mut self, key: &str) -> Result<RowsMut<'_>> {
        self.0.children_mut(key)
    }
}

impl fmt::Debug for RowsMut<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

/// The model, then every row with every entry, untruncated (as a `List`): a large batch prints in
/// full, so log its length rather than the batch at debug level.
impl fmt::Debug for Rows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.model() {
            Some(model) => write!(f, "Rows<{model}>")?,
            None => f.write_str("Rows")?,
        }
        fmt::Debug::fmt(&RowsRef::from(self.clone()), f)
    }
}

impl PartialEq for Rows {
    /// Same rows with the same entries, in order (keys compared as dicts).
    fn eq(&self, other: &Self) -> bool {
        RowsRef::from(self.clone()) == RowsRef::from(other.clone())
    }
}

fn inconsistent(what: &str) -> Error {
    Error::programming(format!("{what} (internal error in erp_fields::rows)"))
}

// A batch is immutable once shared and crosses threads.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Rows>();
    send_sync::<RowRef>();
    send_sync::<RowsRef>();
};

#[cfg(test)]
mod tests;
