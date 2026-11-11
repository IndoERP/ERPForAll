//! A record batch held in guest memory and read as typed columns.
//!
//! The batch is checked once, when the store is built: every column holds exactly one value per id
//! and no field appears twice. After that, every read is a slice of guest memory; iterating the
//! records never calls the host.

use crate::error::{Result, error};
use crate::{
    BoundaryErrorKind, ColumnData, ColumnVector, DateRange, DatetimeRange, RecordBatch, RecordId,
};

/// The shape of one column: which `column-data` case it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    Bool,
    OptBool,
    Int,
    OptInt,
    Float,
    OptFloat,
    OptDecimal,
    Str,
    OptStr,
    OptDate,
    OptDatetime,
    OptDateRange,
    OptDatetimeRange,
    OptId,
    Ids,
    OptJson,
    OptBytes,
}

impl ColumnKind {
    /// The kind of `data`.
    pub fn of(data: &ColumnData) -> Self {
        match data {
            ColumnData::ColBool(_) => Self::Bool,
            ColumnData::ColOptBool(_) => Self::OptBool,
            ColumnData::ColInt(_) => Self::Int,
            ColumnData::ColOptInt(_) => Self::OptInt,
            ColumnData::ColFloat(_) => Self::Float,
            ColumnData::ColOptFloat(_) => Self::OptFloat,
            ColumnData::ColOptDecimal(_) => Self::OptDecimal,
            ColumnData::ColString(_) => Self::Str,
            ColumnData::ColOptString(_) => Self::OptStr,
            ColumnData::ColOptDate(_) => Self::OptDate,
            ColumnData::ColOptDatetime(_) => Self::OptDatetime,
            ColumnData::ColOptDateRange(_) => Self::OptDateRange,
            ColumnData::ColOptDatetimeRange(_) => Self::OptDatetimeRange,
            ColumnData::ColOptId(_) => Self::OptId,
            ColumnData::ColIds(_) => Self::Ids,
            ColumnData::ColOptJson(_) => Self::OptJson,
            ColumnData::ColOptBytes(_) => Self::OptBytes,
        }
    }

    /// The case name used by the contract (`col-opt-int`, …), for messages.
    pub fn wit_name(self) -> &'static str {
        match self {
            Self::Bool => "col-bool",
            Self::OptBool => "col-opt-bool",
            Self::Int => "col-int",
            Self::OptInt => "col-opt-int",
            Self::Float => "col-float",
            Self::OptFloat => "col-opt-float",
            Self::OptDecimal => "col-opt-decimal",
            Self::Str => "col-string",
            Self::OptStr => "col-opt-string",
            Self::OptDate => "col-opt-date",
            Self::OptDatetime => "col-opt-datetime",
            Self::OptDateRange => "col-opt-date-range",
            Self::OptDatetimeRange => "col-opt-datetime-range",
            Self::OptId => "col-opt-id",
            Self::Ids => "col-ids",
            Self::OptJson => "col-opt-json",
            Self::OptBytes => "col-opt-bytes",
        }
    }
}

/// Number of values `data` holds.
pub(crate) fn column_len(data: &ColumnData) -> usize {
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

/// One record of a [`ColumnStore`]: its position in every column and its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    index: usize,
    id: RecordId,
}

impl Row {
    /// Position of the record in every column of the store.
    pub fn index(self) -> usize {
        self.index
    }

    /// Id of the record.
    pub fn id(self) -> RecordId {
        self.id
    }
}

/// The columns of one model for a list of records, checked and held in guest memory.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnStore {
    model: String,
    ids: Vec<RecordId>,
    columns: Vec<ColumnVector>,
    epoch: u64,
}

macro_rules! typed_column {
    ($(#[$doc:meta])* $name:ident, $case:ident, $kind:ident, $item:ty) => {
        $(#[$doc])*
        pub fn $name(&self, field: &str) -> Result<&[$item]> {
            match &self.column(field)?.data {
                ColumnData::$case(values) => Ok(values),
                other => Err(self.wrong_kind(field, ColumnKind::of(other), ColumnKind::$kind)),
            }
        }
    };
}

impl ColumnStore {
    /// Check `batch` and keep it. `epoch` is the `cache-epoch` of the call the batch arrived in.
    ///
    /// A column whose length is not the number of ids, or a field that appears twice, is refused with
    /// `invalid-argument`: the contract requires one value per id in every column.
    pub fn new(batch: RecordBatch, epoch: u64) -> Result<Self> {
        let RecordBatch {
            model,
            ids,
            columns,
        } = batch;
        for (position, column) in columns.iter().enumerate() {
            let len = column_len(&column.data);
            if len != ids.len() {
                return Err(error(
                    BoundaryErrorKind::InvalidArgument,
                    format!(
                        "record batch of model '{model}': column '{}' holds {len} values for {} ids",
                        column.field,
                        ids.len()
                    ),
                ));
            }
            if columns[..position]
                .iter()
                .any(|earlier| earlier.field == column.field)
            {
                return Err(error(
                    BoundaryErrorKind::InvalidArgument,
                    format!(
                        "record batch of model '{model}': column '{}' appears twice",
                        column.field
                    ),
                ));
            }
        }
        Ok(Self {
            model,
            ids,
            columns,
            epoch,
        })
    }

    /// Model of the records.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Ids of the records, in column order.
    pub fn ids(&self) -> &[RecordId] {
        &self.ids
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the store holds no record.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The `cache-epoch` the columns were read under.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Fields the store holds, in batch order.
    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.columns.iter().map(|column| column.field.as_str())
    }

    /// The records, in column order. Reading values for a row is indexing a column slice: no call
    /// to the host.
    pub fn rows(&self) -> impl ExactSizeIterator<Item = Row> + '_ {
        self.ids
            .iter()
            .enumerate()
            .map(|(index, &id)| Row { index, id })
    }

    /// Kind of the column of `field`.
    pub fn kind(&self, field: &str) -> Result<ColumnKind> {
        Ok(ColumnKind::of(&self.column(field)?.data))
    }

    /// The whole column of `field`.
    pub fn column(&self, field: &str) -> Result<&ColumnVector> {
        self.columns
            .iter()
            .find(|column| column.field == field)
            .ok_or_else(|| {
                error(
                    BoundaryErrorKind::NotFound,
                    format!(
                        "record batch of model '{}' holds no column '{field}'",
                        self.model
                    ),
                )
            })
    }

    /// Give the batch back, for passing it on to the host.
    pub fn into_batch(self) -> RecordBatch {
        RecordBatch {
            model: self.model,
            ids: self.ids,
            columns: self.columns,
        }
    }

    typed_column!(/// `field` as a `col-bool` column.
        bools, ColBool, Bool, bool);
    typed_column!(/// `field` as a `col-opt-bool` column.
        opt_bools, ColOptBool, OptBool, Option<bool>);
    typed_column!(/// `field` as a `col-int` column.
        ints, ColInt, Int, i64);
    typed_column!(/// `field` as a `col-opt-int` column.
        opt_ints, ColOptInt, OptInt, Option<i64>);
    typed_column!(/// `field` as a `col-float` column.
        floats, ColFloat, Float, f64);
    typed_column!(/// `field` as a `col-opt-float` column.
        opt_floats, ColOptFloat, OptFloat, Option<f64>);
    typed_column!(/// `field` as a `col-opt-decimal` column: canonical decimal text.
        opt_decimals, ColOptDecimal, OptDecimal, Option<String>);
    typed_column!(/// `field` as a `col-string` column.
        strings, ColString, Str, String);
    typed_column!(/// `field` as a `col-opt-string` column.
        opt_strings, ColOptString, OptStr, Option<String>);
    typed_column!(/// `field` as a `col-opt-date` column: day counts since 1970-01-01.
        opt_dates, ColOptDate, OptDate, Option<i32>);
    typed_column!(/// `field` as a `col-opt-datetime` column: microseconds since 1970-01-01 00:00:00 UTC.
        opt_datetimes, ColOptDatetime, OptDatetime, Option<i64>);
    typed_column!(/// `field` as a `col-opt-date-range` column.
        opt_date_ranges, ColOptDateRange, OptDateRange, Option<DateRange>);
    typed_column!(/// `field` as a `col-opt-datetime-range` column.
        opt_datetime_ranges, ColOptDatetimeRange, OptDatetimeRange, Option<DatetimeRange>);
    typed_column!(/// `field` as a `col-opt-id` column: `many2one` target ids.
        opt_ids, ColOptId, OptId, Option<RecordId>);
    typed_column!(/// `field` as a `col-ids` column: `x2many` target ids per record.
        id_lists, ColIds, Ids, Vec<RecordId>);
    typed_column!(/// `field` as a `col-opt-json` column: compact JSON text.
        opt_json, ColOptJson, OptJson, Option<String>);
    typed_column!(/// `field` as a `col-opt-bytes` column.
        opt_bytes, ColOptBytes, OptBytes, Option<Vec<u8>>);

    fn wrong_kind(
        &self,
        field: &str,
        found: ColumnKind,
        wanted: ColumnKind,
    ) -> crate::BoundaryError {
        error(
            BoundaryErrorKind::InvalidArgument,
            format!(
                "column '{field}' of model '{}' is {}, not {}",
                self.model,
                found.wit_name(),
                wanted.wit_name()
            ),
        )
    }
}
