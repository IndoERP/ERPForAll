//! Bound parameters of generated statements and the conversion of domain literals to them.
//!
//! Statements never inline values: every literal becomes a `$n` placeholder cast to an explicit
//! type (`$1::int8[]`, `$2::text`), so one prepared statement serves every value of a given
//! domain shape (§11). [`Param`] is driver-independent; `erp-orm` binds it.

use std::sync::Arc;

use erp_domain::{PrincipalParam, Value as DomainValue};
use erp_fields::convert::decimal_from_f64;
use erp_fields::{
    Date, DateRange, DateTime, DatetimeRange, Decimal, FieldType, RangeBounds, TimestampTz, Uuid,
    Xid8,
};

/// One scalar parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    /// Exact bigint field operand.
    BigInt(erp_fields::BigInt),
    /// `int8` (ids, many2one, integer columns compared as `int8`).
    Int8(i64),
    /// `float8`.
    Float8(f64),
    /// `numeric`.
    Numeric(Decimal),
    /// `text` (char, text, html, selection).
    Text(Arc<str>),
    /// `date`.
    Date(Date),
    /// `timestamp` (naive UTC).
    Timestamp(DateTime),
    /// Native UUID.
    Uuid(Uuid),
    /// Native unsigned transaction id.
    Xid8(Xid8),
    /// Aware microsecond instant.
    TimestampTz(TimestampTz),
    /// `daterange`, with the bounds of the field it is compared with (`overlaps`).
    DateRange(DateRange, RangeBounds),
    /// `tsrange`, with the bounds of the field it is compared with (`overlaps`).
    TsRange(DatetimeRange, RangeBounds),
}

/// The SQL type of a [`Scalar`] (all members of an array parameter have the same kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarKind {
    /// Exact signed bigint with safe integer/canonical text admission.
    BigInt,
    /// `int8`.
    Int8,
    /// `float8`.
    Float8,
    /// `numeric`.
    Numeric,
    /// `text`.
    Text,
    /// `date`.
    Date,
    /// `timestamp`.
    Timestamp,
    /// Native UUID.
    Uuid,
    /// Native unsigned transaction id.
    Xid8,
    /// Aware microsecond instant.
    TimestampTz,
    /// `daterange`.
    DateRange,
    /// `tsrange`.
    TsRange,
}

impl ScalarKind {
    /// The type name used in placeholder casts.
    pub fn sql(self) -> &'static str {
        match self {
            Self::Int8 | Self::BigInt => "int8",
            Self::Float8 => "float8",
            Self::Numeric => "numeric",
            Self::Text => "text",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::Uuid => "uuid",
            Self::Xid8 => "xid8",
            Self::TimestampTz => "timestamptz",
            Self::DateRange => "daterange",
            Self::TsRange => "tsrange",
        }
    }

    /// The kind a field's values are compared as, or `None` for types compared without parameters
    /// (boolean, JSON). Integer columns are compared with `int8` parameters so that literals beyond
    /// the `int4` range compare instead of failing to bind.
    pub fn of_field(ty: &FieldType) -> Option<Self> {
        Some(match ty {
            FieldType::BigInt => Self::BigInt,
            FieldType::Id
            | FieldType::Many2one { .. }
            | FieldType::Many2oneReference { .. }
            | FieldType::Integer
            | FieldType::RowVersion => Self::Int8,
            FieldType::Reference { .. } => Self::Text,
            FieldType::One2many { .. } | FieldType::Many2many { .. } => return None,
            FieldType::Float { digits: None } => Self::Float8,
            FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. } => Self::Numeric,
            FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Html
            | FieldType::Selection { .. } => Self::Text,
            FieldType::Date => Self::Date,
            FieldType::Datetime => Self::Timestamp,
            FieldType::Uuid => Self::Uuid,
            FieldType::Xid8 => Self::Xid8,
            FieldType::TimestampTz => Self::TimestampTz,
            // ranges are compared by `overlaps`/`contains`, not by value
            FieldType::Boolean
            | FieldType::Json
            | FieldType::DateRange { .. }
            | FieldType::DatetimeRange { .. } => return None,
        })
    }
}

impl Scalar {
    /// The SQL type of the value.
    pub fn kind(&self) -> ScalarKind {
        match self {
            Self::BigInt(_) => ScalarKind::BigInt,
            Self::Int8(_) => ScalarKind::Int8,
            Self::Float8(_) => ScalarKind::Float8,
            Self::Numeric(_) => ScalarKind::Numeric,
            Self::Text(_) => ScalarKind::Text,
            Self::Date(_) => ScalarKind::Date,
            Self::Timestamp(_) => ScalarKind::Timestamp,
            Self::Uuid(_) => ScalarKind::Uuid,
            Self::Xid8(_) => ScalarKind::Xid8,
            Self::TimestampTz(_) => ScalarKind::TimestampTz,
            Self::DateRange(..) => ScalarKind::DateRange,
            Self::TsRange(..) => ScalarKind::TsRange,
        }
    }

    /// `None` for different kinds.
    pub(crate) fn partial_cmp_same(&self, other: &Scalar) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::BigInt(a), Self::BigInt(b)) => a.partial_cmp(b),
            (Self::Int8(a), Self::Int8(b)) => a.partial_cmp(b),
            (Self::Float8(a), Self::Float8(b)) => a.partial_cmp(b),
            (Self::Numeric(a), Self::Numeric(b)) => a.partial_cmp(b),
            (Self::Text(a), Self::Text(b)) => a.partial_cmp(b),
            (Self::Date(a), Self::Date(b)) => a.partial_cmp(b),
            (Self::Timestamp(a), Self::Timestamp(b)) => a.partial_cmp(b),
            (Self::Uuid(a), Self::Uuid(b)) => a.partial_cmp(b),
            (Self::Xid8(a), Self::Xid8(b)) => a.partial_cmp(b),
            (Self::TimestampTz(a), Self::TimestampTz(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}

/// A bound parameter: one value, an array of values of one kind (`= ANY($n)`), a row count, or a
/// principal parameter still to be bound.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    /// A scalar.
    Scalar(Scalar),
    /// An array; every member has the given kind.
    Array(ScalarKind, Vec<Scalar>),
    /// A row count or offset (`LIMIT`/`OFFSET`), bound as `int8`.
    Count(i64),
    /// A value of the principal the statement runs: the ORM replaces it
    /// with a [`Param::Scalar`] or [`Param::Array`] of `kind` from the principal binding before
    /// the statement runs, so the planned text serves every principal.
    Principal {
        /// Which value.
        param: PrincipalParam,
        /// Its SQL type.
        kind: ScalarKind,
        /// Bound as an array (`$n::kind[]`).
        array: bool,
    },
}

impl Param {
    /// The placeholder cast, e.g. `int8[]`.
    pub fn sql_type(&self) -> String {
        match self {
            Self::Scalar(s) => s.kind().sql().to_owned(),
            Self::Array(kind, _) => format!("{}[]", kind.sql()),
            Self::Count(_) => "int8".to_owned(),
            Self::Principal { kind, array, .. } => {
                if *array {
                    format!("{}[]", kind.sql())
                } else {
                    kind.sql().to_owned()
                }
            }
        }
    }
}

/// Other types have none.
pub fn falsy_value(ty: &FieldType) -> Option<Scalar> {
    Some(match ty {
        FieldType::BigInt => Scalar::BigInt(erp_fields::BigInt::from_i64(0)),
        FieldType::Integer | FieldType::Many2oneReference { .. } | FieldType::RowVersion => {
            Scalar::Int8(0)
        }
        FieldType::Float { digits: None } => Scalar::Float8(0.0),
        FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. } => {
            Scalar::Numeric(Decimal::ZERO)
        }
        FieldType::Char { .. } | FieldType::Text | FieldType::Html => Scalar::Text(Arc::from("")),
        _ => return None,
    })
}

/// Largest integer magnitude every `f64` represents exactly (2^53).
const F64_EXACT_INT: u64 = 1 << 53;

/// A domain literal that cannot be converted to the type of the field it is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotConvertible;

/// Lossless conversions are accepted (`'12'` for an integer, `5` for a float, `42` for a
/// string); lossy or impossible ones are refused, as the input converter does. The
/// caller names the condition in its error.
pub fn to_scalar(kind: ScalarKind, value: &DomainValue) -> Result<Scalar, NotConvertible> {
    let scalar = match kind {
        // None becomes NotConvertible below; the caller names the condition.
        ScalarKind::BigInt => match value {
            DomainValue::Int(i) if i.unsigned_abs() < F64_EXACT_INT => {
                Some(Scalar::BigInt(erp_fields::BigInt::from_i64(*i)))
            }
            DomainValue::Str(s) => erp_fields::BigInt::parse(s).ok().map(Scalar::BigInt),
            _ => None,
        },
        ScalarKind::Int8 => match value {
            DomainValue::Int(i) => Some(Scalar::Int8(*i)),
            DomainValue::Bool(b) => Some(Scalar::Int8(i64::from(*b))),
            DomainValue::Float(f)
                if f.is_finite() && f.fract() == 0.0 && f.abs() <= F64_EXACT_INT as f64 =>
            {
                Some(Scalar::Int8(*f as i64))
            }
            DomainValue::Str(s) => s.trim().parse::<i64>().ok().map(Scalar::Int8),
            _ => None,
        },
        ScalarKind::Float8 => match value {
            DomainValue::Int(i) if i.unsigned_abs() <= F64_EXACT_INT => {
                Some(Scalar::Float8(*i as f64))
            }
            DomainValue::Bool(b) => Some(Scalar::Float8(f64::from(u8::from(*b)))),
            DomainValue::Float(f) if f.is_finite() => Some(Scalar::Float8(*f)),
            DomainValue::Str(s) => s
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .map(Scalar::Float8),
            _ => None,
        },
        ScalarKind::Numeric => match value {
            DomainValue::Int(i) => Some(Scalar::Numeric(Decimal::from(*i))),
            DomainValue::Bool(b) => Some(Scalar::Numeric(Decimal::from(u8::from(*b)))),
            DomainValue::Float(f) => decimal_from_f64(*f).map(Scalar::Numeric),
            DomainValue::Str(s) => {
                let t = s.trim();
                t.parse::<Decimal>()
                    .or_else(|_| Decimal::from_scientific(t))
                    .ok()
                    .map(Scalar::Numeric)
            }
            _ => None,
        },
        ScalarKind::Text => match value {
            DomainValue::Str(s) => Some(Scalar::Text(Arc::from(s.as_str()))),
            DomainValue::Int(i) => Some(Scalar::Text(Arc::from(i.to_string()))),
            DomainValue::Float(f) if f.is_finite() => {
                Some(Scalar::Text(Arc::from(format!("{f:?}"))))
            }
            DomainValue::Bool(true) => Some(Scalar::Text(Arc::from("True"))),
            _ => None,
        },
        ScalarKind::Date => match value {
            DomainValue::Date(d) => date_of(*d).map(Scalar::Date),
            DomainValue::Datetime(dt) => date_of(dt.date()).map(Scalar::Date),
            DomainValue::Str(s) => Date::parse_odoo(s).map(Scalar::Date),
            _ => None,
        },
        ScalarKind::Timestamp => match value {
            DomainValue::Datetime(dt) => datetime_of(*dt).map(Scalar::Timestamp),
            DomainValue::Date(d) => date_of(*d)
                .and_then(Date::checked_at_midnight)
                .map(Scalar::Timestamp),
            DomainValue::Str(s) => DateTime::parse_odoo(s).map(Scalar::Timestamp),
            _ => None,
        },
        ScalarKind::Uuid => match value {
            DomainValue::Str(s) => Uuid::parse(s).ok().map(Scalar::Uuid),
            _ => None,
        },
        ScalarKind::Xid8 => match value {
            DomainValue::Str(s) => Xid8::parse(s).ok().map(Scalar::Xid8),
            _ => None,
        },
        ScalarKind::TimestampTz => match value {
            DomainValue::Str(s) => TimestampTz::parse(s).ok().map(Scalar::TimestampTz),
            _ => None,
        },
        // a range operand needs the field's bounds: `crate::filter` builds it end by end
        ScalarKind::DateRange | ScalarKind::TsRange => None,
    };
    scalar.ok_or(NotConvertible)
}

/// A `time` date as an ERPForAll [`Date`].
pub fn date_of(d: time::Date) -> Option<Date> {
    Date::from_ymd(d.year(), u32::from(u8::from(d.month())), u32::from(d.day()))
}

/// A `time` naive datetime (UTC) as an ERPForAll [`DateTime`].
pub fn datetime_of(dt: time::PrimitiveDateTime) -> Option<DateTime> {
    DateTime::from_parts(
        date_of(dt.date())?,
        u32::from(dt.hour()),
        u32::from(dt.minute()),
        u32::from(dt.second()),
        dt.microsecond(),
    )
}

/// An ERPForAll [`DateTime`] as a `time` naive datetime.
pub fn primitive_of(dt: DateTime) -> Option<time::PrimitiveDateTime> {
    let (y, m, d) = dt.date().ymd();
    let (h, mi, s, us) = dt.time();
    let date = time::Date::from_calendar_date(
        y,
        time::Month::try_from(u8::try_from(m).ok()?).ok()?,
        u8::try_from(d).ok()?,
    )
    .ok()?;
    let time = time::Time::from_hms_micro(
        u8::try_from(h).ok()?,
        u8::try_from(mi).ok()?,
        u8::try_from(s).ok()?,
        us,
    )
    .ok()?;
    Some(time::PrimitiveDateTime::new(date, time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_conversions_only() {
        assert_eq!(
            to_scalar(ScalarKind::Int8, &DomainValue::from("12")),
            Ok(Scalar::Int8(12))
        );
        assert_eq!(
            to_scalar(ScalarKind::Int8, &DomainValue::Float(3.0)),
            Ok(Scalar::Int8(3))
        );
        assert!(to_scalar(ScalarKind::Int8, &DomainValue::Float(1.5)).is_err());
        assert!(to_scalar(ScalarKind::Int8, &DomainValue::from("abc")).is_err());
        assert_eq!(
            to_scalar(ScalarKind::Text, &DomainValue::Int(42)),
            Ok(Scalar::Text(Arc::from("42")))
        );
        assert_eq!(
            to_scalar(ScalarKind::Numeric, &DomainValue::Float(2.675)),
            Ok(Scalar::Numeric("2.675".parse().unwrap()))
        );
        assert!(to_scalar(ScalarKind::Date, &DomainValue::from("2024-13-01")).is_err());
    }

    #[test]
    fn datetimes_round_trip() {
        let dt = DateTime::parse_odoo("2024-01-05 11:06:02").unwrap();
        assert_eq!(datetime_of(primitive_of(dt).unwrap()), Some(dt));
    }
}
