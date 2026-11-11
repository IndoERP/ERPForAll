//! Rows decoded straight from the received bytes (PERF.STREAM-RAW).
//!
//! The vendored driver hands over each row of a stream as its `DataRow` body
//! (`RowStream::poll_next_body`), already checked as `Row::new` checks it (every field's range
//! inside the body, one field per column). [`RowRanges`] reads the field ranges of one row into a
//! buffer the transaction keeps, so no allocation is made per row (`Row::new` collects them into
//! a new vector per row), and lends the row as a [`RawRow`]: the columns, the body and the ranges.
//! Values decode with `FromSql` on the slices, through the same rules and with the same error
//! texts as `Row::try_get`.
//!
//! [`CellSource`] lets one decoder read either a `RawRow` (every cache-filling fetch) or a
//! driver `Row` (collected results: `RETURNING`, [`Transaction::query`](super::Transaction::query)).

use std::error::Error as StdError;
use std::fmt;
use std::ops::Range;

use erp_fields::{Error, ErrorKind, Result};
use postgres_protocol::message::backend::DataRowBody;
use tokio_postgres::fallible_iterator::FallibleIterator;
use tokio_postgres::types::{FromSql, Type, WrongType};
use tokio_postgres::{Column, Row};

/// The field ranges of the row being decoded, kept by the transaction and reused for every row:
/// its capacity grows only for a statement wider than every earlier one (lead
/// decision 6).
#[derive(Debug, Default)]
pub(crate) struct RowRanges {
    ranges: Vec<Option<Range<usize>>>,
    /// Test seam: move every value's range one byte on, so the byte-identity test can show it
    /// catches a wrong mapping ([`Transaction::raw_row_identity_for_tests`](super::Transaction)).
    #[cfg(feature = "test-faults")]
    skew: bool,
    /// Test seam: refuse the next row as malformed, to test how a stream ends on a row that does
    /// not parse ([`Transaction::refuse_next_row_for_tests`](super::Transaction)).
    #[cfg(feature = "test-faults")]
    refuse_next: bool,
}

impl RowRanges {
    /// Read the field ranges of `body` and lend the row.
    ///
    /// # Errors
    ///
    /// `Database` when the body is malformed or has another field count than `columns`. The
    /// driver refuses such bodies before handing them over, so this is a second line only.
    pub(crate) fn read<'a>(
        &'a mut self,
        columns: &'a [Column],
        body: &'a DataRowBody,
    ) -> Result<RawRow<'a>> {
        self.ranges.clear();
        #[cfg(feature = "test-faults")]
        if std::mem::take(&mut self.refuse_next) {
            return Err(Error::new(
                ErrorKind::Database,
                "error parsing response from server",
            ));
        }
        let mut ranges = body.ranges();
        self.ranges.reserve(ranges.size_hint().0);
        loop {
            match ranges.next() {
                Ok(Some(range)) => self.ranges.push(range),
                Ok(None) => break,
                Err(e) => {
                    return Err(Error::new(
                        ErrorKind::Database,
                        "error parsing response from server",
                    )
                    .with_source(e));
                }
            }
        }
        if self.ranges.len() != columns.len() {
            return Err(Error::new(
                ErrorKind::Database,
                format!(
                    "a row's field count {} differs from its column count {}",
                    self.ranges.len(),
                    columns.len()
                ),
            ));
        }
        #[cfg(feature = "test-faults")]
        if self.skew {
            for range in self.ranges.iter_mut().flatten() {
                *range = range.start + 1..range.end + 1;
            }
        }
        Ok(RawRow {
            columns,
            body,
            ranges: &self.ranges,
        })
    }
}

/// One row of a stream, lent while its sink decodes it: the statement's columns, the row's
/// `DataRow` body and its field ranges. Nothing in it is owned.
#[derive(Clone, Copy)]
pub struct RawRow<'a> {
    columns: &'a [Column],
    body: &'a DataRowBody,
    ranges: &'a [Option<Range<usize>>],
}

impl fmt::Debug for RawRow<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawRow")
            .field("columns", &self.columns)
            .finish_non_exhaustive()
    }
}

impl RawRow<'_> {
    /// The value of column `idx` as `T`, with `Row::try_get`'s rules: SQL NULL only into a type
    /// that accepts it (`Option`), a column type `T` does not accept is refused before decoding.
    ///
    /// # Errors
    ///
    /// [`CellError`] with `Row::try_get`'s text: "invalid column `idx`" past the last column,
    /// "error deserializing column idx" (the cause as its source) for a type `T` does not accept
    /// or a value that does not decode.
    pub fn try_get<'s, T: FromSql<'s>>(&'s self, idx: usize) -> Result<T, CellError> {
        let column = self.columns.get(idx).ok_or(CellError::Column(idx))?;
        let ty = column.type_();
        if !T::accepts(ty) {
            return Err(CellError::from_sql(
                idx,
                Box::new(WrongType::new::<T>(ty.clone())),
            ));
        }
        let raw = match self.ranges.get(idx) {
            Some(Some(range)) => match self.body.buffer().get(range.clone()) {
                Some(raw) => Some(raw),
                None => {
                    return Err(CellError::from_sql(
                        idx,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "a field range lies outside its row",
                        )),
                    ));
                }
            },
            Some(None) => None,
            None => return Err(CellError::Column(idx)),
        };
        T::from_sql_nullable(ty, raw).map_err(|e| CellError::from_sql(idx, e))
    }
}

impl RawRow<'_> {
    /// The bytes of the row's `DataRow` body as received (a stream's window size follows the sum,
    /// `perf-stream.md` §3.4).
    pub(crate) fn wire_len(&self) -> usize {
        self.body.buffer().len()
    }
}

/// One row of a statement run with [`Transaction::query_each`](super::Transaction::query_each),
/// lent while the closure reads it: values by column index, column names and type names. It is
/// the ORM's own row type (lead decision 4, D3): nothing of the driver's row, types or
/// traits is handed out, and nothing in it is owned or allocated per row.
#[derive(Clone, Copy)]
pub struct SqlRow<'a> {
    raw: &'a RawRow<'a>,
}

impl fmt::Debug for SqlRow<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlRow")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl<'a> SqlRow<'a> {
    pub(crate) fn new(raw: &'a RawRow<'a>) -> Self {
        Self { raw }
    }

    /// The number of columns.
    pub fn len(&self) -> usize {
        self.raw.columns.len()
    }

    /// Whether the statement returns no column.
    pub fn is_empty(&self) -> bool {
        self.raw.columns.is_empty()
    }

    /// The name of column `idx`, `None` past the last column.
    pub fn column_name(&self, idx: usize) -> Option<&str> {
        self.raw.columns.get(idx).map(Column::name)
    }

    /// The PostgreSQL name of column `idx`'s type (`int4`, `text`, `numeric`, ...), `None` past
    /// the last column.
    pub fn column_type_name(&self, idx: usize) -> Option<&str> {
        self.raw.columns.get(idx).map(|c| c.type_().name())
    }

    /// The value of column `idx` as `T` (one of the [`SqlValue`] types): SQL NULL only into an
    /// `Option`, a column type `T` does not accept is refused before decoding. Borrowed types
    /// (`&str`, `&[u8]`) borrow the received bytes for as long as the row is lent.
    ///
    /// # Errors
    ///
    /// `Database`: "invalid column `idx`" past the last column, "error deserializing column idx"
    /// for a type `T` does not accept or a value that does not decode (the cause, e.g. the
    /// wrong-type text, is the source).
    pub fn try_get<T: SqlValue<'a>>(&self, idx: usize) -> Result<T> {
        T::read(*self, idx)
    }
}

mod sealed {
    /// Only the ORM implements [`super::SqlValue`].
    pub trait Sealed {}
}

/// A Rust type a [`SqlRow`] column reads into (the ORM's own bound, not the
/// driver's trait): `bool`, `i16`, `i32`, `i64`, `u32` (`oid`), `f32`, `f64`, `String`, `&str`,
/// `Vec<u8>`, `&[u8]`, [`serde_json::Value`] (`json`, `jsonb`), [`erp_fields::Decimal`]
/// (`numeric`), [`erp_fields::Date`] and [`erp_fields::DateTime`], and an `Option` of each for a
/// column that may be NULL. Sealed: the list grows here.
pub trait SqlValue<'a>: Sized + sealed::Sealed {
    #[doc(hidden)]
    fn read(row: SqlRow<'a>, idx: usize) -> Result<Self>;
}

macro_rules! sql_values {
    ($($ty:ty),* $(,)?) => {$(
        impl sealed::Sealed for $ty {}
        impl<'a> SqlValue<'a> for $ty {
            fn read(row: SqlRow<'a>, idx: usize) -> Result<Self> {
                row.raw.try_get(idx).map_err(cell_error)
            }
        }
        impl sealed::Sealed for Option<$ty> {}
        impl<'a> SqlValue<'a> for Option<$ty> {
            fn read(row: SqlRow<'a>, idx: usize) -> Result<Self> {
                row.raw.try_get(idx).map_err(cell_error)
            }
        }
    )*};
}

sql_values!(
    bool,
    i16,
    i32,
    i64,
    u32,
    f32,
    f64,
    String,
    Vec<u8>,
    serde_json::Value,
    erp_fields::Decimal,
    erp_fields::Date,
    erp_fields::DateTime,
);

impl sealed::Sealed for &str {}
impl<'a> SqlValue<'a> for &'a str {
    fn read(row: SqlRow<'a>, idx: usize) -> Result<Self> {
        row.raw.try_get(idx).map_err(cell_error)
    }
}
impl sealed::Sealed for Option<&str> {}
impl<'a> SqlValue<'a> for Option<&'a str> {
    fn read(row: SqlRow<'a>, idx: usize) -> Result<Self> {
        row.raw.try_get(idx).map_err(cell_error)
    }
}
impl sealed::Sealed for &[u8] {}
impl<'a> SqlValue<'a> for &'a [u8] {
    fn read(row: SqlRow<'a>, idx: usize) -> Result<Self> {
        row.raw.try_get(idx).map_err(cell_error)
    }
}
impl sealed::Sealed for Option<&[u8]> {}
impl<'a> SqlValue<'a> for Option<&'a [u8]> {
    fn read(row: SqlRow<'a>, idx: usize) -> Result<Self> {
        row.raw.try_get(idx).map_err(cell_error)
    }
}

/// The ORM error of a value [`SqlRow::try_get`] could not read: the cell error's text, its cause
/// as the source.
fn cell_error(e: CellError) -> Error {
    let message = e.to_string();
    match e {
        CellError::FromSql { cause, .. } => {
            Error::new(ErrorKind::Database, message).with_source(Cause(cause))
        }
        CellError::Driver(driver) => Error::new(ErrorKind::Database, message).with_source(driver),
        CellError::Column(_) => Error::new(ErrorKind::Database, message),
    }
}

/// A boxed cause carried as an error source (a `Box<dyn Error>` is not itself an `Error`).
#[derive(Debug)]
struct Cause(Box<dyn StdError + Send + Sync>);

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl StdError for Cause {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.0.source()
    }
}

/// A value that could not be read from a row: the same texts and sources as the driver's
/// `Row::try_get` errors, whichever row kind it came from.
#[derive(Debug)]
pub enum CellError {
    /// From a driver `Row`.
    Driver(tokio_postgres::Error),
    /// Column index past the last column of a [`RawRow`].
    Column(usize),
    /// Column of a [`RawRow`] whose type the target does not accept, or whose value does not
    /// decode; the cause is the source.
    FromSql {
        /// The column index.
        idx: usize,
        /// Why it does not decode.
        cause: Box<dyn StdError + Send + Sync>,
    },
}

impl CellError {
    fn from_sql(idx: usize, cause: Box<dyn StdError + Send + Sync>) -> Self {
        CellError::FromSql { idx, cause }
    }
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellError::Driver(e) => fmt::Display::fmt(e, f),
            CellError::Column(idx) => write!(f, "invalid column `{idx}`"),
            CellError::FromSql { idx, .. } => write!(f, "error deserializing column {idx}"),
        }
    }
}

impl StdError for CellError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            // the driver error's own text is this error's text, so its source comes next
            CellError::Driver(e) => e.source(),
            CellError::Column(_) => None,
            CellError::FromSql { cause, .. } => Some(&**cause as &(dyn StdError + 'static)),
        }
    }
}

/// A row the ORM's decoders read: a [`RawRow`] on the fetch path, a driver `Row` for collected
/// results.
pub(crate) trait CellSource {
    /// The type of column `idx`, `None` past the last column.
    fn column_type(&self, idx: usize) -> Option<&Type>;

    /// The value of column `idx` as `T` (see [`RawRow::try_get`]).
    fn cell<'s, T: FromSql<'s>>(&'s self, idx: usize) -> Result<T, CellError>;
}

impl CellSource for RawRow<'_> {
    fn column_type(&self, idx: usize) -> Option<&Type> {
        self.columns.get(idx).map(Column::type_)
    }

    fn cell<'s, T: FromSql<'s>>(&'s self, idx: usize) -> Result<T, CellError> {
        self.try_get(idx)
    }
}

impl CellSource for Row {
    fn column_type(&self, idx: usize) -> Option<&Type> {
        self.columns().get(idx).map(Column::type_)
    }

    fn cell<'s, T: FromSql<'s>>(&'s self, idx: usize) -> Result<T, CellError> {
        self.try_get(idx).map_err(CellError::Driver)
    }
}

/// How [`Transaction::raw_row_identity_for_tests`](super::Transaction) runs the statement on the
/// raw path. Test seam (`test-faults`).
#[cfg(feature = "test-faults")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMode {
    /// A named prepared statement (`query_raw`).
    Prepared,
    /// An unnamed statement with typed parameters (`query_typed_raw`).
    PerCall,
    /// A flight of two statements, one of each form ([`Transaction::query_flight`](super::Transaction)):
    /// the statement as given (prepared), then a different one (per call) with an extra
    /// trailing column and the first row left out, so rows handed to the wrong statement or
    /// read with the other statement's columns cannot compare equal.
    Flight,
}

/// What [`Transaction::raw_row_identity_for_tests`](super::Transaction) compared. Test seam.
#[cfg(feature = "test-faults")]
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct IdentityReport {
    /// Rows read on the raw path.
    pub rows: usize,
    /// Cells whose bytes were compared.
    pub cells: usize,
    /// Of those, the cells that are not NULL on the driver side.
    pub values: usize,
    /// Decodes compared (group decoder on every cell, cache decoder on every requested read).
    pub decodes: usize,
    /// Decodes that failed on both paths with the same error.
    pub errors: usize,
    /// Every difference, one line each; empty when both paths agree.
    pub mismatches: Vec<String>,
}

/// Takes a value of any type as its bytes (test seam).
#[cfg(feature = "test-faults")]
struct AnyBytes<'a>(&'a [u8]);

#[cfg(feature = "test-faults")]
impl<'a> FromSql<'a> for AnyBytes<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(AnyBytes(raw))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

/// A decode outcome as text: the value's `Debug` form, or the error's kind, message and every
/// source's text.
#[cfg(feature = "test-faults")]
fn outcome<T: fmt::Debug>(result: &Result<T>) -> (bool, String) {
    match result {
        Ok(value) => (false, format!("ok {value:?}")),
        Err(e) => {
            let mut text = format!("err {:?} {}", e.kind(), e.message());
            let mut source = StdError::source(e);
            while let Some(cause) = source {
                text.push_str(" | ");
                text.push_str(&cause.to_string());
                source = cause.source();
            }
            (true, text)
        }
    }
}

#[cfg(feature = "test-faults")]
fn bytes_outcome(result: Result<Option<AnyBytes<'_>>, CellError>) -> String {
    match result {
        Ok(value) => format!("ok {:?}", value.map(|b| b.0)),
        Err(e) => format!("err {e}"),
    }
}

/// Compare row `k` of the raw path with the collected driver row `k`: every cell's bytes, the
/// group decoder on every cell (not with `skewed` ranges: an array header read from the wrong
/// bytes can ask for any element count), the cache decoder on every requested `(column, type)`.
#[cfg(feature = "test-faults")]
fn compare_row(
    report: &mut IdentityReport,
    label: &str,
    k: usize,
    raw: &RawRow<'_>,
    rows: &[Row],
    reads: &[(usize, erp_fields::ColumnType)],
    skewed: bool,
) {
    report.rows += 1;
    let Some(row) = rows.get(k) else {
        report.mismatches.push(format!(
            "{label} row {k}: the raw path read more rows than the driver"
        ));
        return;
    };
    if row.columns().len() != raw.columns.len() {
        report.mismatches.push(format!(
            "{label} row {k}: {} columns against {}",
            raw.columns.len(),
            row.columns().len()
        ));
        return;
    }
    for i in 0..row.columns().len() {
        report.cells += 1;
        let ours = bytes_outcome(raw.try_get::<Option<AnyBytes<'_>>>(i));
        let theirs = row.try_get::<_, Option<AnyBytes<'_>>>(i);
        if matches!(theirs, Ok(Some(_))) {
            report.values += 1;
        }
        let theirs = bytes_outcome(theirs.map_err(CellError::Driver));
        if ours != theirs {
            report.mismatches.push(format!(
                "{label} row {k} column {i} bytes: raw {ours} / row {theirs}"
            ));
        }
        if skewed {
            continue;
        }
        let ours = outcome(&crate::read_group::decode::raw(raw, i));
        let theirs = outcome(&crate::read_group::decode::raw(row, i));
        report.decodes += 1;
        if ours != theirs {
            report.mismatches.push(format!(
                "{label} row {k} column {i} group decode: raw {} / row {}",
                ours.1, theirs.1
            ));
        } else if ours.0 {
            report.errors += 1;
        }
    }
    for (i, ty) in reads {
        let ours = outcome(&crate::read::decode(raw, *i, *ty));
        let theirs = outcome(&crate::read::decode(row, *i, *ty));
        report.decodes += 1;
        if ours != theirs {
            report.mismatches.push(format!(
                "{label} row {k} column {i} as {ty:?}: raw {} / row {}",
                ours.1, theirs.1
            ));
        } else if ours.0 {
            report.errors += 1;
        }
    }
}

#[cfg(feature = "test-faults")]
impl super::Transaction {
    /// Make the next fetched row read as malformed, as if the driver had let a bad `DataRow`
    /// through (test seam for the second-line check; `test-faults` only).
    #[doc(hidden)]
    pub fn refuse_next_row_for_tests(&mut self) {
        self.row_ranges.refuse_next = true;
    }

    /// Byte-identity check of the raw read: run `sql` collected through the driver
    /// (`Row`) and on the raw path in `mode`, and compare every row: each cell's bytes, the group
    /// decoder on each cell, and the cache decoder on each `(column, type)` of `reads` (columns
    /// past the last and types the column does not have included, to compare errors). With
    /// `skew`, the raw path moves every value's range one byte on, a wrong mapping the check must
    /// report. Only with the `test-faults` feature.
    ///
    /// # Errors
    ///
    /// The statement's errors, on either path.
    #[doc(hidden)]
    pub async fn raw_row_identity_for_tests(
        &mut self,
        sql: &str,
        mode: IdentityMode,
        reads: &[(usize, erp_fields::ColumnType)],
        skew: bool,
    ) -> Result<IdentityReport> {
        use crate::search::PlanPolicy;
        let second = format!("SELECT s.*, 1::int4 AS flight_second FROM ({sql}) AS s OFFSET 1");
        let rows = match mode {
            IdentityMode::Prepared | IdentityMode::Flight => self.query(sql, &[]).await?,
            IdentityMode::PerCall => self.query_per_call(sql, &[]).await?,
        };
        let second_rows = match mode {
            IdentityMode::Flight => self.query_per_call(&second, &[]).await?,
            IdentityMode::Prepared | IdentityMode::PerCall => Vec::new(),
        };
        let mut report = IdentityReport::default();
        self.row_ranges.skew = skew;
        let counts = match mode {
            IdentityMode::Prepared | IdentityMode::PerCall => {
                let policy = if mode == IdentityMode::Prepared {
                    PlanPolicy::Cached
                } else {
                    PlanPolicy::PerCall
                };
                let mut k = 0usize;
                let label = format!("{mode:?}");
                self.query_each_with(sql, &[], policy, |_, raw| {
                    compare_row(&mut report, &label, k, raw, &rows, reads, skew);
                    k += 1;
                    Ok(())
                })
                .await
                .map(|()| vec![(k, rows.len())])
            }
            IdentityMode::Flight => {
                let statements = [
                    super::pipelined::PipelinedQuery::new(sql, &[], PlanPolicy::Cached)?,
                    super::pipelined::PipelinedQuery::new(&second, &[], PlanPolicy::PerCall)?,
                ];
                let collected = [&rows, &second_rows];
                let mut seen = [0usize; 2];
                self.query_flight(&statements, |i, _, raw| {
                    let k = seen.get(i).copied().ok_or_else(|| {
                        Error::programming(format!("a flight row named statement {i}"))
                    })?;
                    let own = collected.get(i).copied().ok_or_else(|| {
                        Error::programming(format!("a flight row named statement {i}"))
                    })?;
                    compare_row(
                        &mut report,
                        &format!("Flight[{i}]"),
                        k,
                        raw,
                        own,
                        reads,
                        skew,
                    );
                    if let Some(n) = seen.get_mut(i) {
                        *n += 1;
                    }
                    Ok(())
                })
                .await
                .map(|()| vec![(seen[0], rows.len()), (seen[1], second_rows.len())])
            }
        };
        self.row_ranges.skew = false;
        for (i, (read, collected)) in counts?.into_iter().enumerate() {
            if read != collected {
                report.mismatches.push(format!(
                    "{mode:?} statement {i}: {read} rows on the raw path, {collected} through the \
                     driver"
                ));
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use postgres_protocol::message::backend::Message;

    use super::*;

    fn body(field_count: u16, fields: &[u8]) -> DataRowBody {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(b"D");
        buf.extend_from_slice(&(i32::try_from(fields.len()).unwrap() + 6).to_be_bytes());
        buf.extend_from_slice(&field_count.to_be_bytes());
        buf.extend_from_slice(fields);
        match Message::parse(&mut buf) {
            Ok(Some(Message::DataRow(body))) => body,
            _ => panic!("not a DataRow"),
        }
    }

    /// A malformed body is refused (the driver refuses it first; this is the second line), and
    /// the buffer is reused: a narrower row after a wider one allocates nothing.
    #[test]
    fn malformed_bodies_are_refused_and_the_buffer_is_reused() {
        let mut ranges = RowRanges::default();
        let truncated = body(1, &[&10i32.to_be_bytes()[..], &b"ab"[..]].concat());
        let error = ranges.read(&[], &truncated).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database);
        assert_eq!(error.message(), "error parsing response from server");
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("unexpected EOF")
        );

        let fields = [
            &1i32.to_be_bytes()[..],
            &b"a"[..],
            &(-1i32).to_be_bytes()[..],
        ]
        .concat();
        let two = body(2, &fields);
        let error = ranges.read(&[], &two).unwrap_err();
        assert_eq!(
            error.message(),
            "a row's field count 2 differs from its column count 0"
        );
        let capacity = ranges.ranges.capacity();
        assert!(capacity >= 2);
        let one = body(1, &(-1i32).to_be_bytes());
        let error = ranges.read(&[], &one).unwrap_err();
        assert_eq!(
            error.message(),
            "a row's field count 1 differs from its column count 0"
        );
        assert_eq!(
            ranges.ranges.capacity(),
            capacity,
            "a narrower row reuses the buffer"
        );
    }
}
