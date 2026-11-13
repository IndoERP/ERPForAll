//! Building result batches ([`Rows`])
//! straight from cache cells: one typed column per key, filled record by record, with no map per
//! record. A [`Level`] is one level of a `read` or `web_read` while it is still being built: the
//! relational expansion of `web_read` replaces a key's raw ids by a child level and appends rows
//! that carry only an `id`, then [`Level::finish`] turns it into a batch.
//!
//! Values are exactly those [`crate::read::read_value`] gives for the same cells: a column that
//! meets a cell its type cannot hold turns into a dynamic column (`ColumnData::Dyn`) once and keeps
//! the same values, so the representation never changes a result.

use std::sync::Arc;

use erp_fields::rows::{Bitmap, Children, Column, ColumnData, Key, Layout, Rows, RowsBuilder};
use erp_fields::rows::{NO_TARGET, Targets};
use erp_fields::{
    Cell, Date, DateTime, Decimal, Error, FieldType, RangeValue, RecId, Result, Value,
};

use crate::read::Load;

/// How a NULL cell reads (`read_value`; REL §4.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Null {
    /// `False`.
    False,
    /// `Bool(false)` (boolean fields).
    BoolFalse,
    /// `0` (integer and many2one_reference fields).
    IntZero,
    /// `0.0` (float and monetary fields).
    FloatZero,
    /// `[]` (x2many fields).
    EmptyIds,
}

impl Null {
    fn of(ty: &FieldType) -> Self {
        match ty {
            FieldType::Boolean => Self::BoolFalse,
            FieldType::Integer | FieldType::Many2oneReference { .. } => Self::IntZero,
            FieldType::One2many { .. } | FieldType::Many2many { .. } => Self::EmptyIds,
            FieldType::Float { .. } | FieldType::Monetary { .. } => Self::FloatZero,
            FieldType::Id
            | FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Html
            | FieldType::Selection { .. }
            | FieldType::Date
            | FieldType::Datetime
            | FieldType::Uuid
            | FieldType::Xid8
            | FieldType::TimestampTz
            | FieldType::Json
            | FieldType::Many2one { .. }
            | FieldType::Reference { .. }
            | FieldType::RowVersion
            | FieldType::DateRange { .. }
            | FieldType::DatetimeRange { .. } => Self::False,
        }
    }

    fn value(self) -> Value {
        match self {
            Self::False => Value::False,
            Self::BoolFalse => Value::Bool(false),
            Self::IntZero => Value::Int(0),
            Self::FloatZero => Value::Float(0.0),
            Self::EmptyIds => Value::list([]),
        }
    }
}

/// What a column holds while it is built.
#[derive(Debug)]
enum Data {
    /// Values (every kind but child rows).
    Values(ColumnData),
    /// many2one with a sub-specification: the child row of each row, [`NO_TARGET`] for `False`
    /// and for a row that lacks the key.
    One { child: Rows, at: Vec<u32> },
    /// x2many with a sub-specification: row `i` owns positions `offsets[i]..offsets[i + 1]`,
    /// child rows themselves (`at == None`, contiguous) or indices into `at`. A row that lacks
    /// the key owns an empty span.
    Many {
        child: Rows,
        offsets: Vec<u32>,
        at: Option<Vec<u32>>,
    },
}

/// One column while it is built: typed values, which rows read `False` through the bitmap, which
/// rows lack the key. Child columns stay open (positions in vectors) until [`Level::finish`], so a
/// row appended after the expansion (an `id`-only line, [`Level::push_id_only`]) still fits.
#[derive(Debug)]
pub(crate) struct Sink {
    data: Data,
    falsy: Bitmap,
    absent: Bitmap,
    len: usize,
    null: Null,
    /// The one empty id list every NULL x2many row shares (no allocation per row).
    empty_ids: Option<Arc<[RecId]>>,
}

/// Whether `False` is a falsy bit in a column of this data (the others hold `None`).
fn uses_falsy(data: &ColumnData) -> bool {
    matches!(
        data,
        ColumnData::Bool(_)
            | ColumnData::Int(_)
            | ColumnData::Float(_)
            | ColumnData::Decimal(_)
            | ColumnData::Date(_)
            | ColumnData::DateTime(_)
            | ColumnData::Uuid(_)
            | ColumnData::Xid8(_)
            | ColumnData::TimestampTz(_)
            | ColumnData::Label { .. }
    )
}

fn internal(what: &str) -> Error {
    Error::programming(format!("{what} (internal error in erp_orm::rows_build)"))
}

impl Sink {
    fn with(data: ColumnData, null: Null) -> Self {
        let empty_ids = matches!(null, Null::EmptyIds).then(|| Arc::<[RecId]>::from(Vec::new()));
        Self {
            data: Data::Values(data),
            falsy: Bitmap::new(),
            absent: Bitmap::new(),
            len: 0,
            null,
            empty_ids,
        }
    }

    /// A column for the values of a field of type `ty` read with `load`, room for `capacity`
    /// rows.
    pub(crate) fn for_field(ty: &FieldType, load: Load, capacity: usize) -> Self {
        let data = match ty {
            FieldType::Boolean => ColumnData::Bool(Bitmap::new()),
            FieldType::Id
            | FieldType::Integer
            | FieldType::Many2oneReference { .. }
            | FieldType::RowVersion => ColumnData::Int(Vec::with_capacity(capacity)),
            FieldType::Many2one { .. } => match load {
                Load::Classic => ColumnData::Label {
                    ids: Vec::with_capacity(capacity),
                    names: Vec::with_capacity(capacity),
                },
                Load::Raw => ColumnData::Int(Vec::with_capacity(capacity)),
            },
            FieldType::Float { digits: None } => ColumnData::Float(Vec::with_capacity(capacity)),
            FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. } => {
                ColumnData::Decimal(Vec::with_capacity(capacity))
            }
            FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Html
            | FieldType::Selection { .. }
            | FieldType::Reference { .. } => ColumnData::Str(Vec::with_capacity(capacity)),
            FieldType::Date => ColumnData::Date(Vec::with_capacity(capacity)),
            FieldType::Datetime => ColumnData::DateTime(Vec::with_capacity(capacity)),
            FieldType::Uuid => ColumnData::Uuid(Vec::with_capacity(capacity)),
            FieldType::Xid8 => ColumnData::Xid8(Vec::with_capacity(capacity)),
            FieldType::TimestampTz => ColumnData::TimestampTz(Vec::with_capacity(capacity)),
            FieldType::Json => ColumnData::Json(Vec::with_capacity(capacity)),
            FieldType::DateRange { .. } | FieldType::DatetimeRange { .. } => {
                ColumnData::Range(Vec::with_capacity(capacity))
            }
            FieldType::One2many { .. } | FieldType::Many2many { .. } => {
                ColumnData::Ids(Vec::with_capacity(capacity))
            }
        };
        Self::with(data, Null::of(ty))
    }

    /// [`Sink::for_field`] filling the emptied buffers of `spare`, a column of a recycled batch
    /// (`Rows::try_recycle`, design note `perf-stream.md` §3.6), when they are of the kind the
    /// field needs: a stream's windows then reuse one set of buffers. A spare of another kind (a
    /// column that turned dynamic) is dropped and new buffers are made.
    pub(crate) fn for_field_reusing(
        ty: &FieldType,
        load: Load,
        capacity: usize,
        spare: Option<Column>,
    ) -> Self {
        if let Some(spare) = spare {
            // no buffer is allocated for a capacity of 0
            let mut sink = Self::for_field(ty, load, 0);
            let (data, falsy, absent) = spare.into_parts();
            if let Data::Values(fresh) = &sink.data
                && std::mem::discriminant(fresh) == std::mem::discriminant(&data)
            {
                sink.data = Data::Values(data);
                sink.falsy = falsy;
                sink.absent = absent.unwrap_or_default();
                return sink;
            }
        }
        Self::for_field(ty, load, capacity)
    }

    /// [`Sink::ids`] filling the emptied buffer of `spare` (see [`Sink::for_field_reusing`]).
    pub(crate) fn ids_reusing(
        ids: impl ExactSizeIterator<Item = i64>,
        spare: Option<Column>,
    ) -> Self {
        if let Some(spare) = spare
            && let (ColumnData::Int(mut v), falsy, absent) = spare.into_parts()
        {
            v.extend(ids);
            let len = v.len();
            let mut sink = Self::with(ColumnData::Int(v), Null::False);
            sink.falsy = falsy;
            sink.absent = absent.unwrap_or_default();
            sink.len = len;
            return sink;
        }
        Self::ids(ids)
    }

    /// A column of record ids (`id`).
    pub(crate) fn ids(ids: impl ExactSizeIterator<Item = i64>) -> Self {
        let mut v = Vec::with_capacity(ids.len());
        v.extend(ids);
        let len = v.len();
        let mut sink = Self::with(ColumnData::Int(v), Null::False);
        sink.len = len;
        sink
    }

    /// A text column whose NULL reads `False` (labels such as `display_name`).
    pub(crate) fn text(capacity: usize) -> Self {
        Self::with(ColumnData::Str(Vec::with_capacity(capacity)), Null::False)
    }

    /// How many rows.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Appends the value of `cell` as `read_value` reads it.
    pub(crate) fn push_cell(&mut self, cell: Cell) -> Result<()> {
        if cell.is_null() {
            return self.push_null();
        }
        let row = self.len;
        let Data::Values(data) = &mut self.data else {
            return Err(internal("a value appended to a child column"));
        };
        let unfit = match (data, cell) {
            (ColumnData::Bool(bits), Cell::Bool(b)) => {
                if b {
                    bits.set(row, true)?;
                }
                None
            }
            (ColumnData::Int(v), Cell::Int(i)) => {
                v.push(i64::from(i));
                None
            }
            (ColumnData::Int(v), Cell::Id(i)) => {
                v.push(i);
                None
            }
            (ColumnData::Float(v), Cell::Float(f)) => {
                v.push(f);
                None
            }
            (ColumnData::Decimal(v), Cell::Decimal(d)) => {
                v.push(d);
                None
            }
            (ColumnData::Str(v), Cell::Str(s)) => {
                v.push(Some(s));
                None
            }
            (ColumnData::Date(v), Cell::Date(d)) => {
                v.push(d);
                None
            }
            (ColumnData::DateTime(v), Cell::DateTime(d)) => {
                v.push(d);
                None
            }
            (ColumnData::Uuid(v), Cell::Uuid(x)) => {
                v.push(x);
                None
            }
            (ColumnData::Xid8(v), Cell::Xid8(x)) => {
                v.push(x);
                None
            }
            (ColumnData::TimestampTz(v), Cell::TimestampTz(x)) => {
                v.push(x);
                None
            }
            (ColumnData::Json(v), Cell::Json(j)) => {
                v.push(Some(j));
                None
            }
            (ColumnData::Range(v), Cell::DateRange(r)) => {
                v.push(Some(RangeValue::Date(r)));
                None
            }
            (ColumnData::Range(v), Cell::DatetimeRange(r)) => {
                v.push(Some(RangeValue::Datetime(r)));
                None
            }
            (ColumnData::Ids(v), Cell::Ids(ids)) => {
                v.push(Some(ids));
                None
            }
            (ColumnData::Dyn(v), cell) => {
                v.push(Value::from_cell(&cell));
                None
            }
            (_, cell) => Some(cell),
        };
        match unfit {
            None => {
                self.len += 1;
                Ok(())
            }
            // a cell this column's type does not hold: the column keeps the value dynamically
            Some(cell) => self.push_value(Value::from_cell(&cell)),
        }
    }

    /// Appends the NULL value of the column's field.
    fn push_null(&mut self) -> Result<()> {
        let row = self.len;
        let null = self.null;
        let empty = self.empty_ids.clone();
        let Data::Values(data) = &mut self.data else {
            return Err(internal("a NULL appended to a child column"));
        };
        // `false`: the NULL value is of another kind than the column (a NULL decimal reads `0.0`,
        // a float); the column keeps it dynamically below
        let fits = match data {
            ColumnData::Bool(_) => null == Null::BoolFalse,
            ColumnData::Int(v) => match null {
                Null::IntZero => {
                    v.push(0);
                    true
                }
                Null::False => {
                    v.push(0);
                    self.falsy.set(row, true)?;
                    true
                }
                Null::BoolFalse | Null::FloatZero | Null::EmptyIds => false,
            },
            ColumnData::Float(v) => {
                let fits = null == Null::FloatZero;
                if fits {
                    v.push(0.0);
                }
                fits
            }
            ColumnData::Decimal(_) => false,
            ColumnData::Str(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(None);
                }
                fits
            }
            ColumnData::Bytes(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(None);
                }
                fits
            }
            ColumnData::Date(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(Date::from_days(0));
                    self.falsy.set(row, true)?;
                }
                fits
            }
            ColumnData::DateTime(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(DateTime::from_micros(0));
                    self.falsy.set(row, true)?;
                }
                fits
            }
            ColumnData::Uuid(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(erp_fields::Uuid::NIL);
                    self.falsy.set(row, true)?;
                }
                fits
            }
            ColumnData::Xid8(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(erp_fields::Xid8::from_u64(0));
                    self.falsy.set(row, true)?;
                }
                fits
            }
            ColumnData::TimestampTz(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(erp_fields::TimestampTz::UNIX_EPOCH);
                    self.falsy.set(row, true)?;
                }
                fits
            }
            ColumnData::Json(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(None);
                }
                fits
            }
            ColumnData::Range(v) => {
                let fits = null == Null::False;
                if fits {
                    v.push(None);
                }
                fits
            }
            ColumnData::Ids(v) => {
                let fits = null == Null::EmptyIds;
                if fits {
                    v.push(empty);
                }
                fits
            }
            ColumnData::Label { ids, names } => {
                let fits = null == Null::False;
                if fits {
                    ids.push(0);
                    names.push(None);
                    self.falsy.set(row, true)?;
                }
                fits
            }
            ColumnData::Dyn(v) => {
                v.push(null.value());
                true
            }
            ColumnData::One(_) | ColumnData::Many(_) => {
                return Err(internal("a NULL appended to a finished child column"));
            }
        };
        if fits {
            self.len += 1;
            Ok(())
        } else {
            self.push_value(null.value())
        }
    }

    /// Appends a many2one label (`load='_classic_read'`): `[id, name]`, `False` without one.
    pub(crate) fn push_label(&mut self, label: Option<crate::read::Label>) -> Result<()> {
        let row = self.len;
        let Data::Values(data) = &mut self.data else {
            return Err(internal("a label pushed into a child column"));
        };
        match (data, label) {
            (ColumnData::Label { ids, names }, Some((id, name))) => {
                ids.push(id.get());
                names.push(name);
            }
            (ColumnData::Label { ids, names }, None) => {
                ids.push(0);
                names.push(None);
                self.falsy.set(row, true)?;
            }
            _ => return Err(internal("a label pushed into a column of another kind")),
        }
        self.len += 1;
        Ok(())
    }

    /// Appends a dynamic value.
    pub(crate) fn push_value(&mut self, value: Value) -> Result<()> {
        self.retype_dyn()?;
        match &mut self.data {
            Data::Values(ColumnData::Dyn(v)) => v.push(value),
            _ => return Err(internal("a dynamic column is not dynamic")),
        }
        self.len += 1;
        Ok(())
    }

    /// Appends a row that lacks the key. Every column kind takes one, child columns included
    /// (no child row, an empty span), so [`Level::push_id_only`] fits whatever the level holds.
    pub(crate) fn push_absent(&mut self) -> Result<()> {
        let row = self.len;
        match &mut self.data {
            Data::Values(data) => match data {
                ColumnData::Bool(_) => {}
                ColumnData::Int(v) => v.push(0),
                ColumnData::Float(v) => v.push(0.0),
                ColumnData::Decimal(v) => v.push(Decimal::ZERO),
                ColumnData::Str(v) => v.push(None),
                ColumnData::Bytes(v) => v.push(None),
                ColumnData::Date(v) => v.push(Date::from_days(0)),
                ColumnData::DateTime(v) => v.push(DateTime::from_micros(0)),
                ColumnData::Uuid(v) => v.push(erp_fields::Uuid::NIL),
                ColumnData::Xid8(v) => v.push(erp_fields::Xid8::from_u64(0)),
                ColumnData::TimestampTz(v) => v.push(erp_fields::TimestampTz::UNIX_EPOCH),
                ColumnData::Json(v) => v.push(None),
                ColumnData::Range(v) => v.push(None),
                ColumnData::Ids(v) => v.push(None),
                ColumnData::Label { ids, names } => {
                    ids.push(0);
                    names.push(None);
                }
                ColumnData::Dyn(v) => v.push(Value::False),
                // child columns are `Data::One`/`Data::Many` until the level is finished
                ColumnData::One(_) | ColumnData::Many(_) => {
                    return Err(internal("a row appended to a finished child column"));
                }
            },
            Data::One { at, .. } => at.push(NO_TARGET),
            Data::Many { offsets, .. } => {
                let end = *offsets
                    .last()
                    .ok_or_else(|| internal("x2many offsets without a start"))?;
                offsets.push(end);
            }
        }
        self.absent.set(row, true)?;
        self.len += 1;
        Ok(())
    }

    /// The value of `row` (which has the key), read from the column being built.
    fn value_at(&self, row: usize) -> Result<Value> {
        let Data::Values(data) = &self.data else {
            return Err(internal("a child column read as a value"));
        };
        if uses_falsy(data) && self.falsy.get(row) {
            return Ok(Value::False);
        }
        let short = || internal("a column is shorter than its rows");
        Ok(match data {
            ColumnData::Bool(bits) => Value::Bool(bits.get(row)),
            ColumnData::Int(v) => Value::Int(*v.get(row).ok_or_else(short)?),
            ColumnData::Float(v) => Value::Float(*v.get(row).ok_or_else(short)?),
            ColumnData::Decimal(v) => Value::Decimal(*v.get(row).ok_or_else(short)?),
            ColumnData::Str(v) => v
                .get(row)
                .ok_or_else(short)?
                .clone()
                .map_or(Value::False, Value::Str),
            ColumnData::Bytes(v) => v
                .get(row)
                .ok_or_else(short)?
                .clone()
                .map_or(Value::False, Value::Bytes),
            ColumnData::Date(v) => Value::Date(*v.get(row).ok_or_else(short)?),
            ColumnData::DateTime(v) => Value::DateTime(*v.get(row).ok_or_else(short)?),
            ColumnData::Uuid(v) => Value::Uuid(*v.get(row).ok_or_else(short)?),
            ColumnData::Xid8(v) => Value::Xid8(*v.get(row).ok_or_else(short)?),
            ColumnData::TimestampTz(v) => Value::TimestampTz(*v.get(row).ok_or_else(short)?),
            ColumnData::Json(v) => v
                .get(row)
                .ok_or_else(short)?
                .clone()
                .map_or(Value::False, Value::Json),
            ColumnData::Range(v) => v
                .get(row)
                .ok_or_else(short)?
                .map_or(Value::False, Value::Range),
            ColumnData::Ids(v) => v
                .get(row)
                .ok_or_else(short)?
                .as_ref()
                .map_or(Value::False, |ids| {
                    Value::List(ids.iter().map(|id| Value::Int(id.get())).collect())
                }),
            ColumnData::Label { ids, names } => Value::list([
                Value::Int(*ids.get(row).ok_or_else(short)?),
                names
                    .get(row)
                    .ok_or_else(short)?
                    .clone()
                    .map_or(Value::False, Value::Str),
            ]),
            ColumnData::Dyn(v) => v.get(row).ok_or_else(short)?.clone(),
            ColumnData::One(_) | ColumnData::Many(_) => {
                return Err(internal("a finished child column read as a value"));
            }
        })
    }

    /// Turns the column into a dynamic one with the same values (once per column).
    fn retype_dyn(&mut self) -> Result<()> {
        match &self.data {
            Data::Values(ColumnData::Dyn(_)) => return Ok(()),
            Data::Values(_) => {}
            Data::One { .. } | Data::Many { .. } => {
                return Err(internal("a child column cannot hold other values"));
            }
        }
        let mut values = Vec::with_capacity(self.len.max(1));
        for row in 0..self.len {
            values.push(if self.absent.get(row) {
                // the absent bit stays set; the placeholder is never read
                Value::False
            } else {
                self.value_at(row)?
            });
        }
        self.data = Data::Values(ColumnData::Dyn(values));
        self.falsy = Bitmap::new();
        Ok(())
    }

    /// The raw id of `row` in an integer column (`None` when it reads `False`).
    pub(crate) fn int_at(&self, row: usize) -> Result<Option<i64>> {
        match &self.data {
            Data::Values(ColumnData::Int(v)) => {
                let i = *v
                    .get(row)
                    .ok_or_else(|| internal("an id column is shorter than its rows"))?;
                Ok((!self.falsy.get(row) && !self.absent.get(row)).then_some(i))
            }
            Data::Values(ColumnData::Dyn(v)) => match v.get(row) {
                Some(Value::Int(i)) if !self.absent.get(row) => Ok(Some(*i)),
                Some(_) => Ok(None),
                None => Err(internal("an id column is shorter than its rows")),
            },
            _ => Err(internal("an id read from a column that holds no ids")),
        }
    }

    /// The ids of `row` in an x2many column (`None` when it reads `False`).
    pub(crate) fn ids_at(&self, row: usize) -> Result<Option<&Arc<[RecId]>>> {
        match &self.data {
            Data::Values(ColumnData::Ids(v)) => v
                .get(row)
                .map(Option::as_ref)
                .ok_or_else(|| internal("an id-list column is shorter than its rows")),
            _ => Err(internal("an id list read from a column that holds none")),
        }
    }

    /// Replaces the ids of `row` in an x2many column.
    pub(crate) fn set_ids(&mut self, row: usize, ids: Arc<[RecId]>) -> Result<()> {
        match &mut self.data {
            Data::Values(ColumnData::Ids(v)) => {
                *v.get_mut(row)
                    .ok_or_else(|| internal("an id-list column is shorter than its rows"))? =
                    Some(ids);
                Ok(())
            }
            _ => Err(internal("an id list stored into a column that holds none")),
        }
    }

    /// Replaces the value of `row` (which keeps the key). A value the column's type does not
    /// hold turns the column dynamic, once.
    pub(crate) fn set_value(&mut self, row: usize, value: Value) -> Result<()> {
        if row >= self.len {
            return Err(internal("a value stored past the column's rows"));
        }
        let Data::Values(data) = &mut self.data else {
            return Err(internal("a value stored into a child column"));
        };
        self.absent.set(row, false)?;
        if uses_falsy(data) && matches!(value, Value::False) {
            return self.falsy.set(row, true);
        }
        let short = || internal("a value stored past the column's data");
        let unfit = match (data, value) {
            (ColumnData::Int(v), Value::Int(i)) => {
                *v.get_mut(row).ok_or_else(short)? = i;
                self.falsy.set(row, false)?;
                None
            }
            (ColumnData::Str(v), Value::False) => {
                *v.get_mut(row).ok_or_else(short)? = None;
                None
            }
            (ColumnData::Str(v), Value::Str(s)) => {
                *v.get_mut(row).ok_or_else(short)? = Some(s);
                None
            }
            (ColumnData::Dyn(v), value) => {
                *v.get_mut(row).ok_or_else(short)? = value;
                None
            }
            (_, value) => Some(value),
        };
        match unfit {
            None => Ok(()),
            Some(value) => {
                self.retype_dyn()?;
                self.set_value(row, value)
            }
        }
    }

    /// Makes the column the child rows of a many2one: `at[row]` is the child row of each row,
    /// [`NO_TARGET`] for `False`.
    pub(crate) fn set_one(&mut self, child: Rows, at: Vec<u32>) -> Result<()> {
        if at.len() != self.len {
            return Err(internal(
                "many2one positions of another length than the rows",
            ));
        }
        self.data = Data::One { child, at };
        self.falsy = Bitmap::new();
        Ok(())
    }

    /// Makes the column the child rows of an x2many: row `i` owns positions
    /// `offsets[i]..offsets[i + 1]`, which are child rows (`at == None`) or indices into `at`.
    pub(crate) fn set_many(
        &mut self,
        child: Rows,
        offsets: Vec<u32>,
        at: Option<Vec<u32>>,
    ) -> Result<()> {
        if offsets.len() != self.len + 1 {
            return Err(internal("x2many offsets for another number of rows"));
        }
        self.data = Data::Many { child, offsets, at };
        self.falsy = Bitmap::new();
        Ok(())
    }

    fn finish(self) -> Result<Column> {
        let (data, falsy) = match self.data {
            Data::Values(data) => {
                let falsy = if uses_falsy(&data) {
                    self.falsy
                } else {
                    Bitmap::new()
                };
                (data, falsy)
            }
            Data::One { child, at } => (ColumnData::One(Targets::new(child, at)?), Bitmap::new()),
            Data::Many {
                child,
                offsets,
                at: None,
            } => (
                ColumnData::Many(Children::contiguous(child, offsets)?),
                Bitmap::new(),
            ),
            Data::Many {
                child,
                offsets,
                at: Some(at),
            } => (
                ColumnData::Many(Children::indexed(child, offsets, at)?),
                Bitmap::new(),
            ),
        };
        Ok(Column::new(data).with_false(falsy).with_absent(self.absent))
    }
}

#[derive(Debug)]
pub(crate) struct Level {
    model: &'static str,
    keys: Vec<Key>,
    sinks: Vec<Sink>,
    len: usize,
}

impl Level {
    /// A level of `len` rows of `model` with an `id` column (always the first key).
    pub(crate) fn new(model: &'static str, id: Sink) -> Self {
        let len = id.len();
        Self {
            model,
            keys: vec![Key::Field("id")],
            sinks: vec![id],
            len,
        }
    }

    pub(crate) fn ids_only(model: &'static str, ids: &[RecId]) -> Self {
        Self::new(model, Sink::ids(ids.iter().map(|id| id.get())))
    }

    /// How many rows.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// How many keys (the layout position the next column takes).
    pub(crate) fn width(&self) -> usize {
        self.keys.len()
    }

    /// Adds the column of `key` at the end.
    ///
    /// # Errors
    /// `Programming` when the column has another number of rows (or the key is a duplicate,
    /// reported by [`Level::finish`]).
    pub(crate) fn push(&mut self, key: Key, sink: Sink) -> Result<()> {
        if sink.len() != self.len {
            return Err(internal("a column of another length than its level"));
        }
        self.keys.push(key);
        self.sinks.push(sink);
        Ok(())
    }

    /// The position of `key`.
    pub(crate) fn position(&self, key: &str) -> Option<usize> {
        self.keys.iter().position(|k| k.as_str() == key)
    }

    /// The column at `pos`.
    pub(crate) fn sink(&self, pos: usize) -> Result<&Sink> {
        self.sinks
            .get(pos)
            .ok_or_else(|| internal("a key position past the level"))
    }

    /// The column at `pos`, for changes.
    pub(crate) fn sink_mut(&mut self, pos: usize) -> Result<&mut Sink> {
        self.sinks
            .get_mut(pos)
            .ok_or_else(|| internal("a key position past the level"))
    }

    pub(crate) fn sink_or_absent(&mut self, key: Key) -> Result<&mut Sink> {
        let pos = match self.position(key.as_str()) {
            Some(pos) => pos,
            None => {
                let mut sink =
                    Sink::with(ColumnData::Dyn(Vec::with_capacity(self.len)), Null::False);
                for _ in 0..self.len {
                    sink.push_absent()?;
                }
                self.keys.push(key);
                self.sinks.push(sink);
                self.sinks.len() - 1
            }
        };
        self.sink_mut(pos)
    }

    /// The id of `row` (`None` when it reads `False`).
    pub(crate) fn id_at(&self, row: usize) -> Result<Option<i64>> {
        self.sink(0)?.int_at(row)
    }

    /// Every other column gets a row that
    /// lacks its key, whatever it holds: values, or child rows of a many2one or x2many the level
    /// already expanded (no target, an empty span), so the row can come after the expansion.
    pub(crate) fn push_id_only(&mut self, id: i64) -> Result<u32> {
        let row = u32::try_from(self.len)
            .ok()
            .filter(|row| *row != NO_TARGET)
            .ok_or_else(|| Error::programming(format!("too many rows of {}", self.model)))?;
        let mut sinks = self.sinks.iter_mut();
        let id_sink = sinks
            .next()
            .ok_or_else(|| internal("a level without an id column"))?;
        id_sink.set_len_push_int(id)?;
        for sink in sinks {
            sink.push_absent()?;
        }
        self.len += 1;
        Ok(row)
    }

    /// The batch.
    ///
    /// # Errors
    /// `Programming` for a duplicate key or an inconsistent column.
    pub(crate) fn finish(self) -> Result<Rows> {
        self.finish_reusing(None)
    }

    /// [`Level::finish`] into `spare`, the builder of a recycled batch (`Rows::try_recycle`),
    /// when it has this level's model and keys: its layout and batch allocation are kept. The
    /// columns taken out of it were handed to the sinks ([`Sink::for_field_reusing`]).
    pub(crate) fn finish_reusing(self, spare: Option<RowsBuilder>) -> Result<Rows> {
        let same = |builder: &RowsBuilder| {
            let layout = builder.layout();
            layout.model() == Some(self.model)
                && layout.len() == self.keys.len()
                && layout
                    .keys()
                    .zip(&self.keys)
                    .all(|(have, want)| have == want.as_str())
        };
        let mut builder = match spare {
            Some(mut builder) if same(&builder) => {
                builder.set_len(self.len)?;
                builder
            }
            _ => RowsBuilder::new(Layout::for_model(self.model, self.keys)?, self.len)?,
        };
        for (pos, sink) in self.sinks.into_iter().enumerate() {
            builder.column_at(pos, sink.finish()?)?;
        }
        builder.finish()
    }
}

impl Sink {
    /// Appends an integer to an id column.
    fn set_len_push_int(&mut self, id: i64) -> Result<()> {
        match &mut self.data {
            Data::Values(ColumnData::Int(v)) => v.push(id),
            Data::Values(ColumnData::Dyn(v)) => v.push(Value::Int(id)),
            _ => return Err(internal("an id appended to a column that holds no ids")),
        }
        self.len += 1;
        Ok(())
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::{TimestampTz, Uuid, Xid8};

    #[test]
    fn native_fields_rows_preserve_type_and_null_across_reuse() {
        for (kind, cell, expected) in [
            (
                FieldType::Uuid,
                Cell::Uuid(Uuid::NIL),
                Value::Uuid(Uuid::NIL),
            ),
            (
                FieldType::Xid8,
                Cell::Xid8(Xid8::from_u64(u64::MAX)),
                Value::Xid8(Xid8::from_u64(u64::MAX)),
            ),
            (
                FieldType::TimestampTz,
                Cell::TimestampTz(TimestampTz::UNIX_EPOCH),
                Value::TimestampTz(TimestampTz::UNIX_EPOCH),
            ),
        ] {
            let mut sink = Sink::for_field(&kind, Load::Raw, 2);
            sink.push_cell(cell.clone()).unwrap();
            sink.push_cell(Cell::Null).unwrap();
            assert!(!matches!(&sink.data, Data::Values(ColumnData::Dyn(_))));
            let mut level = Level::new("native", Sink::ids([1, 2].into_iter()));
            level.push(Key::Field("native_value"), sink).unwrap();
            let rows = level.finish().unwrap();
            assert_eq!(rows.get(0, "native_value").unwrap(), expected);
            assert_eq!(rows.get(1, "native_value").unwrap(), Value::False);
            let mut spare = rows.try_recycle().unwrap();
            let mut reused = Sink::for_field_reusing(&kind, Load::Raw, 1, spare.take_column(1));
            reused.push_cell(cell).unwrap();
            let mut level = Level::new(
                "native",
                Sink::ids_reusing([3].into_iter(), spare.take_column(0)),
            );
            level.push(Key::Field("native_value"), reused).unwrap();
            let rows = level.finish_reusing(Some(spare)).unwrap();
            assert_eq!(rows.get(0, "native_value").unwrap(), expected);
        }
    }
}
