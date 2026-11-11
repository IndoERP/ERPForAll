//! PostgreSQL binary conversions for dates, datetimes and cells (feature `pg`).

use std::error::Error as StdError;

use bytes::{BufMut, BytesMut};
use postgres_types::{FromSql, IsNull, ToSql, Type, accepts, to_sql_checked};

use crate::cell::Cell;
use crate::range::{Range, RangeBounds, StoredEnd, from_stored};
use crate::temporal::{Date, DateTime};
use crate::{TimestampTz, Uuid, Xid8};

type BoxError = Box<dyn StdError + Sync + Send>;

/// Days between 1970-01-01 and PostgreSQL's epoch 2000-01-01.
const PG_EPOCH_DAYS: i32 = 10_957;
/// Microseconds between the Unix epoch and PostgreSQL's epoch.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;
/// PostgreSQL's first and last valid date as days since 2000-01-01: 4714-11-24 BC and
/// 5874897-12-31 (`IS_VALID_DATE`, `datatype/timestamp.h`: Julian days 0 to 2,147,483,493).
const PG_DATE_MIN: i32 = -2_451_545;
const PG_DATE_MAX: i32 = 2_145_031_948;
/// PostgreSQL's valid timestamps as microseconds since 2000-01-01: `MIN_TIMESTAMP` (4714-11-24
/// 00:00:00 BC) inclusive to `END_TIMESTAMP` (294277-01-01 00:00:00) exclusive
/// (`IS_VALID_TIMESTAMP`, `datatype/timestamp.h`).
const PG_TIMESTAMP_MIN: i64 = -211_813_488_000_000_000;
const PG_TIMESTAMP_END: i64 = 9_223_371_331_200_000_000;

fn read_i32(raw: &[u8]) -> Result<i32, BoxError> {
    let bytes: [u8; 4] = raw.try_into().map_err(|_| "invalid date length")?;
    Ok(i32::from_be_bytes(bytes))
}

fn read_i64(raw: &[u8]) -> Result<i64, BoxError> {
    let bytes: [u8; 8] = raw.try_into().map_err(|_| "invalid timestamp length")?;
    Ok(i64::from_be_bytes(bytes))
}

// The driver contract observed in postgres-types 0.2.14's UUID adapter uses
// native UUID OIDs and exactly sixteen bytes. Keep application UUID policy out
// of this purpose-neutral codec; additionally refuse direct wrong-type calls.
impl<'a> FromSql<'a> for Uuid {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        if *ty != Type::UUID {
            return Err(crate::Error::programming("UUID SQL decoder expected uuid").into());
        }
        let bytes = <[u8; 16]>::try_from(raw).map_err(|_| {
            crate::Error::validation("UUID binary value must contain exactly 16 bytes")
        })?;
        Ok(Self::from_bytes(bytes))
    }
    accepts!(UUID);
}

impl ToSql for Uuid {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        if *ty != Type::UUID {
            return Err(crate::Error::programming("UUID SQL encoder expected uuid").into());
        }
        out.extend_from_slice(self.as_bytes());
        Ok(IsNull::No)
    }
    accepts!(UUID);
    to_sql_checked!();
}

impl<'a> FromSql<'a> for Xid8 {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        if *ty != Type::XID8 {
            return Err(crate::Error::programming("xid8 SQL decoder expected xid8").into());
        }
        let bytes = <[u8; 8]>::try_from(raw).map_err(|_| {
            crate::Error::validation("xid8 binary value must contain exactly 8 bytes")
        })?;
        Ok(Self::from_u64(u64::from_be_bytes(bytes)))
    }
    accepts!(XID8);
}

impl ToSql for Xid8 {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        if *ty != Type::XID8 {
            return Err(crate::Error::programming("xid8 SQL encoder expected xid8").into());
        }
        out.extend_from_slice(&self.get().to_be_bytes());
        Ok(IsNull::No)
    }
    accepts!(XID8);
    to_sql_checked!();
}

impl<'a> FromSql<'a> for TimestampTz {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        if *ty != Type::TIMESTAMPTZ {
            return Err(
                crate::Error::programming("timestamptz SQL decoder expected timestamptz").into(),
            );
        }
        let bytes = <[u8; 8]>::try_from(raw).map_err(|_| {
            crate::Error::validation("timestamptz binary value must contain exactly 8 bytes")
        })?;
        let micros = i64::from_be_bytes(bytes);
        if micros == i64::MIN || micros == i64::MAX {
            return Err(
                crate::Error::validation("infinite timestamptz values are not supported").into(),
            );
        }
        let unix = micros.checked_add(PG_EPOCH_MICROS).ok_or_else(|| {
            crate::Error::validation("timestamptz outside supported finite instant range")
        })?;
        Ok(Self::from_unix_micros(unix)?)
    }
    accepts!(TIMESTAMPTZ);
}

impl ToSql for TimestampTz {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        if *ty != Type::TIMESTAMPTZ {
            return Err(
                crate::Error::programming("timestamptz SQL encoder expected timestamptz").into(),
            );
        }
        let micros = self
            .unix_micros()
            .checked_sub(PG_EPOCH_MICROS)
            .filter(|value| (PG_TIMESTAMP_MIN..PG_TIMESTAMP_END).contains(value))
            .ok_or_else(|| {
                crate::Error::validation("timestamptz outside supported finite instant range")
            })?;
        out.put_i64(micros);
        Ok(IsNull::No)
    }
    accepts!(TIMESTAMPTZ);
    to_sql_checked!();
}

impl<'a> FromSql<'a> for Date {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        let days = read_i32(raw)?;
        if days == i32::MAX || days == i32::MIN {
            return Err("infinite dates are not supported".into());
        }
        // Every date PostgreSQL sends fits: its last, 5874897-12-31, is 2,145,031,948 days after
        // 2000-01-01, so 2,145,042,905 after 1970. The check refuses a corrupt value instead of
        // overflowing.
        let days = days
            .checked_add(PG_EPOCH_DAYS)
            .ok_or("date out of the supported range")?;
        Ok(Date::from_days(days))
    }
    accepts!(DATE);
}

impl ToSql for Date {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        // Outside PostgreSQL's range a value is refused, not sent: `date_recv` would refuse it, or
        // take i32::MIN and i32::MAX as -infinity and infinity, which this type cannot read back.
        let days = self
            .days()
            .checked_sub(PG_EPOCH_DAYS)
            .filter(|days| (PG_DATE_MIN..=PG_DATE_MAX).contains(days))
            .ok_or("date outside PostgreSQL's range (4714-11-24 BC to 5874897-12-31)")?;
        out.put_i32(days);
        Ok(IsNull::No)
    }
    accepts!(DATE);
    to_sql_checked!();
}

impl<'a> FromSql<'a> for DateTime {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        let micros = read_i64(raw)?;
        if micros == i64::MAX || micros == i64::MIN {
            return Err("infinite timestamps are not supported".into());
        }
        // PostgreSQL's last timestamps (year 294276) lie past i64 microseconds since 1970
        let micros = micros
            .checked_add(PG_EPOCH_MICROS)
            .ok_or("timestamp out of the supported range")?;
        Ok(DateTime::from_micros(micros))
    }
    accepts!(TIMESTAMP);
}

impl ToSql for DateTime {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        // As for dates: i64::MIN and i64::MAX would be stored as -infinity and infinity.
        let micros = self
            .micros()
            .checked_sub(PG_EPOCH_MICROS)
            .filter(|micros| (PG_TIMESTAMP_MIN..PG_TIMESTAMP_END).contains(micros))
            .ok_or("timestamp outside PostgreSQL's range (4714-11-24 BC to 294276-12-31)")?;
        out.put_i64(micros);
        Ok(IsNull::No)
    }
    accepts!(TIMESTAMP);
    to_sql_checked!();
}

impl ToSql for Cell {
    /// Writes the cell in the binary format of `ty`, refusing combinations that do not match (a
    /// statement always casts its placeholders to the column type, so a mismatch is a bug). Range
    /// cells are refused too: their bounds belong to the field, so they are sent as [`RangeSql`].
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        match self {
            Cell::Null => Ok(IsNull::Yes),
            Cell::Bool(b) if *ty == Type::BOOL => b.to_sql(ty, out),
            Cell::Int(i) if *ty == Type::INT4 => i.to_sql(ty, out),
            Cell::Id(i) if *ty == Type::INT8 => i.to_sql(ty, out),
            Cell::Float(f) if *ty == Type::FLOAT8 => f.to_sql(ty, out),
            Cell::Decimal(d) if *ty == Type::NUMERIC => d.to_sql(ty, out),
            Cell::Str(s) if *ty == Type::VARCHAR || *ty == Type::TEXT => s.as_ref().to_sql(ty, out),
            Cell::Date(d) if *ty == Type::DATE => d.to_sql(ty, out),
            Cell::DateTime(d) if *ty == Type::TIMESTAMP => d.to_sql(ty, out),
            Cell::Uuid(value) if *ty == Type::UUID => value.to_sql(ty, out),
            Cell::Xid8(value) if *ty == Type::XID8 => value.to_sql(ty, out),
            Cell::TimestampTz(value) if *ty == Type::TIMESTAMPTZ => value.to_sql(ty, out),
            Cell::Uuid(_) => {
                Err(crate::Error::programming("native uuid cell requires SQL type uuid").into())
            }
            Cell::Xid8(_) => {
                Err(crate::Error::programming("native xid8 cell requires SQL type xid8").into())
            }
            Cell::TimestampTz(_) => Err(crate::Error::programming(
                "native timestamptz cell requires SQL type timestamptz",
            )
            .into()),
            Cell::Json(j) if *ty == Type::JSONB => j.as_ref().to_sql(ty, out),
            cell @ (Cell::DateRange(_) | Cell::DatetimeRange(_)) => Err(format!(
                "cannot send {cell:?} as {ty}: a range is sent with its field's bounds (RangeSql)"
            )
            .into()),
            cell => Err(format!("cannot send {cell:?} as {ty}").into()),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

// ----- ranges ---------------------------------------------------------------------
//
// PostgreSQL's binary range format (`range_send`/`range_recv`): one flag byte, then each bounded end
// as a 4-byte length and the element's own binary value.

const RANGE_EMPTY: u8 = 0x01;
const RANGE_LB_INC: u8 = 0x02;
const RANGE_UB_INC: u8 = 0x04;
const RANGE_LB_INF: u8 = 0x08;
const RANGE_UB_INF: u8 = 0x10;

/// A range sent to PostgreSQL with the bounds of the field or period it belongs to: a `daterange`
/// for [`crate::DateRange`], a `tsrange` for [`crate::DatetimeRange`]. An unbounded end is sent
/// unbounded (PostgreSQL's infinite flag, not the `infinity` value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSql<T> {
    range: Range<T>,
    bounds: RangeBounds,
}

impl<T: Copy + Ord> RangeSql<T> {
    /// `range` with `bounds`.
    pub fn new(range: Range<T>, bounds: RangeBounds) -> Self {
        Self { range, bounds }
    }
}

fn put_end<T: ToSql>(value: &T, ty: &Type, out: &mut BytesMut) -> Result<(), BoxError> {
    let at = out.len();
    out.put_i32(0);
    value.to_sql(ty, out)?;
    let len = i32::try_from(out.len() - at - 4).map_err(|_| "range bound too large")?;
    out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

fn range_to_sql<T: ToSql + Copy + Ord>(
    range: &RangeSql<T>,
    element: &Type,
    out: &mut BytesMut,
) -> Result<IsNull, BoxError> {
    let mut flags = 0u8;
    match range.range.lower() {
        None => flags |= RANGE_LB_INF,
        // both declared bounds include the lower end
        Some(_) => flags |= RANGE_LB_INC,
    }
    match (range.range.upper(), range.bounds) {
        (None, _) => flags |= RANGE_UB_INF,
        (Some(_), RangeBounds::Closed) => flags |= RANGE_UB_INC,
        (Some(_), RangeBounds::HalfOpen) => {}
    }
    out.put_u8(flags);
    if let Some(lower) = range.range.lower() {
        put_end(&lower, element, out)?;
    }
    if let Some(upper) = range.range.upper() {
        put_end(&upper, element, out)?;
    }
    Ok(IsNull::No)
}

impl ToSql for RangeSql<Date> {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        range_to_sql(self, &Type::DATE, out)
    }
    accepts!(DATE_RANGE);
    to_sql_checked!();
}

impl ToSql for RangeSql<DateTime> {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        range_to_sql(self, &Type::TIMESTAMP, out)
    }
    accepts!(TS_RANGE);
    to_sql_checked!();
}

/// A range as PostgreSQL returns it (`daterange` into `StoredRange<Date>`, `tsrange` into
/// `StoredRange<DateTime>`): which ends are included, and whether it is empty. Turn it into a
/// [`Range`] with the field's bounds with [`StoredRange::into_declared`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredRange<T> {
    empty: bool,
    lower: StoredEnd<T>,
    upper: StoredEnd<T>,
}

impl StoredRange<Date> {
    /// The range with the declared `bounds` (see [`crate::Range`]); PostgreSQL stores every
    /// `daterange` as `[)`, so a `[]` field reads its upper end one day earlier.
    ///
    /// # Errors
    ///
    /// A message when the stored range is empty.
    pub fn into_declared(self, bounds: RangeBounds) -> Result<Range<Date>, String> {
        from_stored(self.empty, self.lower, self.upper, bounds)
    }
}

impl StoredRange<DateTime> {
    /// The range with the declared `bounds` (see [`crate::Range`]).
    ///
    /// # Errors
    ///
    /// A message when the stored range is empty or its inclusivity differs from `bounds` (a
    /// `tsrange` written by raw SQL with other bounds: a continuous range cannot be shifted).
    pub fn into_declared(self, bounds: RangeBounds) -> Result<Range<DateTime>, String> {
        from_stored(self.empty, self.lower, self.upper, bounds)
    }
}

fn take_end<'a, T: FromSql<'a>>(raw: &mut &'a [u8], element: &Type) -> Result<T, BoxError> {
    if raw.len() < 4 {
        return Err("truncated range bound".into());
    }
    let (len, rest) = raw.split_at(4);
    let len = usize::try_from(read_i32(len)?).map_err(|_| "negative range bound length")?;
    if rest.len() < len {
        return Err("truncated range bound".into());
    }
    let (value, rest) = rest.split_at(len);
    *raw = rest;
    T::from_sql(element, value)
}

fn range_from_sql<'a, T: FromSql<'a>>(
    element: &Type,
    raw: &'a [u8],
) -> Result<StoredRange<T>, BoxError> {
    let (&flags, mut rest) = raw.split_first().ok_or("empty range value")?;
    if flags & RANGE_EMPTY != 0 {
        return Ok(StoredRange {
            empty: true,
            lower: None,
            upper: None,
        });
    }
    let lower = if flags & RANGE_LB_INF != 0 {
        None
    } else {
        Some((take_end(&mut rest, element)?, flags & RANGE_LB_INC != 0))
    };
    let upper = if flags & RANGE_UB_INF != 0 {
        None
    } else {
        Some((take_end(&mut rest, element)?, flags & RANGE_UB_INC != 0))
    };
    if !rest.is_empty() {
        return Err("trailing bytes after a range value".into());
    }
    Ok(StoredRange {
        empty: false,
        lower,
        upper,
    })
}

impl<'a> FromSql<'a> for StoredRange<Date> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        range_from_sql(&Type::DATE, raw)
    }
    accepts!(DATE_RANGE);
}

impl<'a> FromSql<'a> for StoredRange<DateTime> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        range_from_sql(&Type::TIMESTAMP, raw)
    }
    accepts!(TS_RANGE);
}

#[cfg(test)]
mod range_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use crate::range::{DateRange, DatetimeRange};

    fn d(day: u32) -> Date {
        Date::from_ymd(2026, 1, day).unwrap()
    }

    fn encode<T>(value: &T, ty: &Type) -> BytesMut
    where
        T: ToSql,
    {
        let mut out = BytesMut::new();
        assert!(matches!(value.to_sql(ty, &mut out).unwrap(), IsNull::No));
        out
    }

    /// PostgreSQL's last timestamp (294276-12-31 23:59:59.999999) does not fit the 1970-based
    /// microsecond count: an error, never an overflow (found by the raw-row identity test,
    /// PERF.STREAM-RAW). Every PostgreSQL date fits; a corrupt day count past i32 is refused.
    #[test]
    fn values_past_the_supported_range_are_errors() {
        let last_date = PG_DATE_MAX.to_be_bytes();
        assert_eq!(
            Date::from_sql(&Type::DATE, &last_date).unwrap().ymd(),
            (5_874_897, 12, 31)
        );
        let corrupt = (i32::MAX - 1).to_be_bytes();
        assert_eq!(
            Date::from_sql(&Type::DATE, &corrupt)
                .unwrap_err()
                .to_string(),
            "date out of the supported range"
        );
        let last_ts = (PG_TIMESTAMP_END - 1).to_be_bytes();
        assert_eq!(
            DateTime::from_sql(&Type::TIMESTAMP, &last_ts)
                .unwrap_err()
                .to_string(),
            "timestamp out of the supported range"
        );
    }

    /// Sending refuses everything outside PostgreSQL's ranges, the infinity sentinels included,
    /// and accepts both ends of the range (PERF.STREAM-RAW review).
    #[test]
    fn values_outside_postgres_ranges_are_refused_on_send() {
        let sent = |value: &dyn ToSql, ty: &Type| {
            let mut out = BytesMut::new();
            match value.to_sql_checked(ty, &mut out) {
                Ok(_) => Ok(out.to_vec()),
                Err(e) => {
                    assert!(out.is_empty(), "nothing written on refusal");
                    Err(e.to_string())
                }
            }
        };
        let date_refused =
            Err("date outside PostgreSQL's range (4714-11-24 BC to 5874897-12-31)".to_owned());
        let date = |pg_days: i32| Date::from_days(pg_days + PG_EPOCH_DAYS);
        assert_eq!(
            sent(&date(PG_DATE_MIN), &Type::DATE),
            Ok(PG_DATE_MIN.to_be_bytes().to_vec())
        );
        assert_eq!(
            sent(&date(PG_DATE_MAX), &Type::DATE),
            Ok(PG_DATE_MAX.to_be_bytes().to_vec())
        );
        assert_eq!(sent(&date(PG_DATE_MIN - 1), &Type::DATE), date_refused);
        assert_eq!(sent(&date(PG_DATE_MAX + 1), &Type::DATE), date_refused);
        // what PostgreSQL would store as -infinity and infinity
        assert_eq!(sent(&date(i32::MIN), &Type::DATE), date_refused);
        assert_eq!(sent(&Date::from_days(i32::MAX), &Type::DATE), date_refused);
        assert_eq!(sent(&Date::from_days(i32::MIN), &Type::DATE), date_refused);

        let ts_refused =
            Err("timestamp outside PostgreSQL's range (4714-11-24 BC to 294276-12-31)".to_owned());
        let ts = |pg_micros: i64| DateTime::from_micros(pg_micros + PG_EPOCH_MICROS);
        assert_eq!(
            sent(&ts(PG_TIMESTAMP_MIN), &Type::TIMESTAMP),
            Ok(PG_TIMESTAMP_MIN.to_be_bytes().to_vec())
        );
        assert_eq!(
            sent(&ts(PG_TIMESTAMP_MIN - 1), &Type::TIMESTAMP),
            ts_refused
        );
        // i64::MIN would be stored as -infinity
        assert_eq!(sent(&ts(i64::MIN), &Type::TIMESTAMP), ts_refused);
        assert_eq!(
            sent(&DateTime::from_micros(i64::MIN), &Type::TIMESTAMP),
            ts_refused
        );
        // the largest 1970-based count is below PostgreSQL's end: it is sent
        assert_eq!(
            sent(&DateTime::from_micros(i64::MAX), &Type::TIMESTAMP),
            Ok((i64::MAX - PG_EPOCH_MICROS).to_be_bytes().to_vec())
        );
    }

    #[test]
    fn date_ranges_round_trip_through_the_binary_format() {
        let ty = Type::DATE_RANGE;
        for (range, bounds) in [
            (
                DateRange::new(Some(d(1)), Some(d(5))),
                RangeBounds::HalfOpen,
            ),
            (DateRange::new(Some(d(1)), None), RangeBounds::HalfOpen),
            (DateRange::new(None, Some(d(5))), RangeBounds::Closed),
            (DateRange::new(None, None), RangeBounds::Closed),
            (DateRange::new(Some(d(3)), Some(d(3))), RangeBounds::Closed),
        ] {
            let bytes = encode(&RangeSql::new(range, bounds), &ty);
            let stored = StoredRange::<Date>::from_sql(&ty, &bytes).unwrap();
            assert_eq!(
                stored.into_declared(bounds).unwrap(),
                range,
                "{range:?} {bounds}"
            );
        }
        // flags of `[2026-01-01,)`: lower included, upper infinite
        let bytes = encode(
            &RangeSql::new(DateRange::new(Some(d(1)), None), RangeBounds::HalfOpen),
            &ty,
        );
        assert_eq!(bytes[0], RANGE_LB_INC | RANGE_UB_INF);
        assert_eq!(bytes.len(), 1 + 4 + 4);
    }

    #[test]
    fn datetime_ranges_round_trip_with_both_bounds() {
        let ty = Type::TS_RANGE;
        let a = DateTime::from_parts(d(1), 10, 0, 0, 0).unwrap();
        let b = DateTime::from_parts(d(1), 11, 0, 0, 0).unwrap();
        for bounds in [RangeBounds::HalfOpen, RangeBounds::Closed] {
            let range = DatetimeRange::new(Some(a), Some(b));
            let bytes = encode(&RangeSql::new(range, bounds), &ty);
            let stored = StoredRange::<DateTime>::from_sql(&ty, &bytes).unwrap();
            assert_eq!(stored.into_declared(bounds).unwrap(), range);
        }
    }

    #[test]
    fn malformed_range_values_are_errors() {
        let ty = Type::DATE_RANGE;
        assert_eq!(
            StoredRange::<Date>::from_sql(&ty, &[])
                .unwrap_err()
                .to_string(),
            "empty range value"
        );
        // lower bound announced, no bytes
        assert_eq!(
            StoredRange::<Date>::from_sql(&ty, &[RANGE_LB_INC | RANGE_UB_INF])
                .unwrap_err()
                .to_string(),
            "truncated range bound"
        );
        let mut bytes = encode(
            &RangeSql::new(DateRange::new(None, None), RangeBounds::HalfOpen),
            &ty,
        );
        bytes.put_u8(0);
        assert_eq!(
            StoredRange::<Date>::from_sql(&ty, &bytes)
                .unwrap_err()
                .to_string(),
            "trailing bytes after a range value"
        );
        let empty = StoredRange::<Date>::from_sql(&ty, &[RANGE_EMPTY]).unwrap();
        assert_eq!(
            empty.into_declared(RangeBounds::HalfOpen).unwrap_err(),
            "the database holds an empty range, which a range field never stores"
        );
    }

    #[test]
    fn range_cells_are_not_sent_without_bounds() {
        let mut out = BytesMut::new();
        let Err(err) =
            Cell::DateRange(DateRange::new(None, None)).to_sql(&Type::DATE_RANGE, &mut out)
        else {
            panic!("a range cell was sent without bounds");
        };
        assert_eq!(
            err.to_string(),
            "cannot send DateRange(Range { lower: None, upper: None }) as daterange: a range is \
             sent with its field's bounds (RangeSql)"
        );
    }
}
