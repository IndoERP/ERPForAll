//! Transactional-delta primitives (slice D1,
//! D1, D3, D5, D7). This module builds SQL text and validates values;
//! it executes nothing and owns no lifecycle (the ORM layer does, in later slices).
//!
//! - [`ProjectionShape`]: a validated, immutable declaration of one projection's typed grain and
//!   exact measures. Every refusal is Programming `H27 InvalidDefinition: {projection}: {reason}`.
//! - Canonical keys ([`ProjectionShape::admit_key`], [`ProjectionShape::compare`]): lexicographic,
//!   ascending, NULLS LAST, text by bytes (C collation). The order belongs to a shape, never to a
//!   global `Ord`, so keys of different shapes cannot be compared by accident.
//! - Exact measures ([`ProjectionShape::admit_measures`], [`MeasureVector::checked_add`]): no
//!   rounding, no wraparound; failures are Validation `H27 NumericOutOfRange: {projection}: {measure}`.
//! - SQL fragments: column definitions, the named range `CHECK` of each measure, canonical
//!   `ORDER BY` / `GROUP BY` and the `NULLS NOT DISTINCT` key. Measures are unconstrained
//!   `numeric` (never `numeric(p,s)`, whose typmod would round input); the range predicate is
//!   what bounds them, and it is false (never NULL) for NaN, the infinities and NULL.

use std::cmp::Ordering;
use std::num::NonZeroU32;
use std::sync::Arc;

use erp_fields::{Decimal, Error, Result};
use time::{Date, OffsetDateTime, UtcOffset};

use crate::{quote, shorten_identifier, upsert};

mod sync;
pub use sync::*;

/// Column names a declaration may not use: they belong to the pending/base objects D3 generates.
pub const RESERVED_COLUMNS: &[&str] = &[
    "id",
    "generation",
    "event_id",
    "ordinal",
    "source_model",
    "xid",
];

/// Hard maxima of contract D7 for the grain column count.
const MAX_GRAIN_COLUMNS: usize = 16;
/// Hard maximum of the measure count.
const MAX_MEASURES: usize = 16;
/// Hard maximum of a text grain value in bytes.
const MAX_TEXT_BYTES: usize = 1024;
/// Hard maximum of a declaration name in bytes.
const MAX_NAME_BYTES: usize = 128;
/// Hard maximum of netted rows in one synchronous apply batch (contract D7).
const MAX_BATCH_ROWS: usize = 2_000;
/// Hard maximum of the canonical encoded bytes of one synchronous apply batch (contract D7).
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Hard maximum of the contributions handed to one netting (contract D7, statement capture
/// contributions).
const MAX_CONTRIBUTION_ROWS: usize = 32_000;
/// PostgreSQL's identifier limit in bytes.
const MAX_COLUMN_NAME_BYTES: usize = 63;
/// Largest numeric precision a measure may declare (`rust_decimal` holds 28 exact digits).
const MAX_PRECISION: u32 = 28;
/// `timestamptz` upper bound of PostgreSQL (year 294276 AD).
const PG_TIMESTAMP_MAX_YEAR: i32 = 294_276;
/// `date` upper bound of PostgreSQL (year 5874897 AD).
const PG_DATE_MAX_YEAR: i32 = 5_874_897;

fn invalid(projection: &str, reason: impl std::fmt::Display) -> Error {
    Error::programming(format!("H27 InvalidDefinition: {projection}: {reason}"))
}

fn out_of_range(projection: &str, measure: &str) -> Error {
    Error::validation(format!("H27 NumericOutOfRange: {projection}: {measure}"))
}

/// Resource caps of contract D7. The default equals the hard maximum; a host may lower a cap with
/// [`DeltaLimits::lower`], never set zero or raise one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeltaLimits {
    grain_columns: usize,
    measures: usize,
    text_value_bytes: usize,
    name_bytes: usize,
    batch_rows: usize,
    batch_bytes: usize,
    contribution_rows: usize,
}

impl Default for DeltaLimits {
    fn default() -> Self {
        Self {
            grain_columns: MAX_GRAIN_COLUMNS,
            measures: MAX_MEASURES,
            text_value_bytes: MAX_TEXT_BYTES,
            name_bytes: MAX_NAME_BYTES,
            batch_rows: MAX_BATCH_ROWS,
            batch_bytes: MAX_BATCH_BYTES,
            contribution_rows: MAX_CONTRIBUTION_ROWS,
        }
    }
}

impl DeltaLimits {
    /// Lower the caps. A zero or a value above the hard maximum (16 grain columns, 16 measures,
    /// 1,024 text bytes, 128 name bytes) is refused.
    pub fn lower(
        grain_columns: usize,
        measures: usize,
        text_value_bytes: usize,
        name_bytes: usize,
    ) -> Result<Self> {
        for (what, value, max) in [
            ("grain columns", grain_columns, MAX_GRAIN_COLUMNS),
            ("measures", measures, MAX_MEASURES),
            ("text value bytes", text_value_bytes, MAX_TEXT_BYTES),
            ("name bytes", name_bytes, MAX_NAME_BYTES),
        ] {
            if value == 0 || value > max {
                return Err(invalid("<limits>", format!("{what} must be 1..={max}")));
            }
        }
        Ok(Self {
            grain_columns,
            measures,
            text_value_bytes,
            name_bytes,
            ..Self::default()
        })
    }

    /// Maximum grain columns of a shape.
    pub fn grain_columns(&self) -> usize {
        self.grain_columns
    }

    /// Maximum measures of a shape.
    pub fn measures(&self) -> usize {
        self.measures
    }

    /// Maximum bytes of a text grain value.
    pub fn text_value_bytes(&self) -> usize {
        self.text_value_bytes
    }

    /// Maximum bytes of a projection name.
    pub fn name_bytes(&self) -> usize {
        self.name_bytes
    }
}

/// The type of a grain column. No float, JSON, array or custom collation exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GrainType {
    /// `int8`.
    Int8,
    /// `boolean`.
    Bool,
    /// `date`.
    Date,
    /// `timestamptz` (microsecond precision).
    TimestampTz,
    /// `uuid`.
    Uuid,
    /// `text COLLATE "C"` of at most `max_bytes` UTF-8 bytes.
    Text {
        /// Byte cap of a value, `1..=DeltaLimits::text_value_bytes`.
        max_bytes: usize,
    },
}

/// One ordered grain column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrainColumn {
    name: String,
    ty: GrainType,
    nullable: bool,
}

impl GrainColumn {
    /// A grain column; [`ProjectionShape::new`] validates it.
    pub fn new(name: impl Into<String>, ty: GrainType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable,
        }
    }

    /// The column name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The column type.
    pub fn ty(&self) -> GrainType {
        self.ty
    }

    /// Whether the column may hold NULL.
    pub fn nullable(&self) -> bool {
        self.nullable
    }
}

/// The type of a measure. Measures are never nullable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MeasureType {
    /// Signed `int8`.
    Int8,
    /// Exact `numeric`: `precision` significant digits (1..=28), `scale` of them after the point
    /// (0..=precision). Never rounded.
    Numeric {
        /// Significant digits.
        precision: u32,
        /// Digits after the decimal point.
        scale: u32,
    },
}

/// One ordered measure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasureColumn {
    name: String,
    ty: MeasureType,
}

impl MeasureColumn {
    /// A measure; [`ProjectionShape::new`] validates it.
    pub fn new(name: impl Into<String>, ty: MeasureType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }

    /// The measure name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The measure type.
    pub fn ty(&self) -> MeasureType {
        self.ty
    }
}

/// A 16-byte UUID grain value. Any 16 bytes are a valid UUID; ordering is bytewise, as PostgreSQL's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uuid16([u8; 16]);

impl Uuid16 {
    /// A UUID from its 16 bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The 16 bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// One grain value before admission.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GrainValue {
    /// NULL (only for a nullable column; sorts after every value, never a sentinel).
    Null,
    /// `int8`.
    Int8(i64),
    /// `boolean`.
    Bool(bool),
    /// `date`.
    Date(Date),
    /// `timestamptz`, normalized to UTC at admission.
    TimestampTz(OffsetDateTime),
    /// `uuid`.
    Uuid(Uuid16),
    /// `text`.
    Text(String),
}

/// One measure value before admission.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MeasureValue {
    /// `int8`.
    Int8(i64),
    /// Exact `numeric`.
    Numeric(Decimal),
}

#[derive(Debug, PartialEq, Eq)]
struct ShapeIdent {
    name: String,
    version: NonZeroU32,
    grain: Vec<GrainColumn>,
    measures: Vec<MeasureColumn>,
}

/// A grain key admitted against one [`ProjectionShape`]; only that shape can compare it.
#[derive(Debug, Clone)]
pub struct CanonicalKey {
    ident: Arc<ShapeIdent>,
    values: Vec<GrainValue>,
}

impl CanonicalKey {
    /// The normalized values (timestamps in UTC).
    pub fn values(&self) -> &[GrainValue] {
        &self.values
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stored {
    Int8(i64),
    /// Mantissa at the declared scale.
    Numeric(i128),
}

/// Measure values admitted against one shape; sums are exact and checked.
#[derive(Debug, Clone)]
pub struct MeasureVector {
    ident: Arc<ShapeIdent>,
    values: Vec<Stored>,
}

/// A validated, immutable projection declaration. See the module documentation.
#[derive(Debug, Clone)]
pub struct ProjectionShape {
    ident: Arc<ShapeIdent>,
    /// `(measure index, constraint name)` of every numeric measure.
    constraint_names: Vec<(usize, String)>,
}

fn valid_ident(name: &str, allow_dot: bool) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || (allow_dot && b == b'.')
        })
}

fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

impl ProjectionShape {
    /// Validate a declaration. Every refusal is Programming
    /// `H27 InvalidDefinition: {projection}: {reason}`; when the projection name itself is invalid
    /// the projection reads `<unnamed>` so no unvalidated text is echoed.
    pub fn new(
        name: &str,
        version: NonZeroU32,
        grain: Vec<GrainColumn>,
        measures: Vec<MeasureColumn>,
        limits: &DeltaLimits,
    ) -> Result<Self> {
        if name.is_empty() || name.len() > limits.name_bytes || !valid_ident(name, true) {
            return Err(invalid("<unnamed>", "projection name is not valid"));
        }
        if grain.is_empty() || grain.len() > limits.grain_columns {
            return Err(invalid(name, "grain column count out of range"));
        }
        if measures.is_empty() || measures.len() > limits.measures {
            return Err(invalid(name, "measure count out of range"));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(grain.len() + measures.len());
        let names = grain
            .iter()
            .map(|g| g.name.as_str())
            .chain(measures.iter().map(|m| m.name.as_str()));
        for column in names {
            if column.len() > MAX_COLUMN_NAME_BYTES || !valid_ident(column, false) {
                return Err(invalid(name, "a column name is not valid"));
            }
            if RESERVED_COLUMNS.contains(&column) {
                return Err(invalid(name, format!("column {column} is reserved")));
            }
            if seen.contains(&column) {
                return Err(invalid(name, format!("column {column} is declared twice")));
            }
            seen.push(column);
        }
        for g in &grain {
            if let GrainType::Text { max_bytes } = g.ty
                && (max_bytes == 0 || max_bytes > limits.text_value_bytes)
            {
                return Err(invalid(
                    name,
                    format!("column {} text byte cap out of range", g.name),
                ));
            }
        }
        for m in &measures {
            if let MeasureType::Numeric { precision, scale } = m.ty
                && (precision == 0 || precision > MAX_PRECISION || scale > precision)
            {
                return Err(invalid(
                    name,
                    format!("measure {} precision or scale out of range", m.name),
                ));
            }
        }
        let flat = name.replace('.', "_");
        let mut constraint_names: Vec<(usize, String)> = Vec::with_capacity(measures.len());
        for (index, m) in measures.iter().enumerate() {
            if m.ty == MeasureType::Int8 {
                continue;
            }
            let generated = shorten_identifier(&format!("{flat}__{}_h27_range", m.name));
            if constraint_names.iter().any(|(_, n)| *n == generated) {
                return Err(invalid(
                    name,
                    format!("measure {} constraint name collides", m.name),
                ));
            }
            constraint_names.push((index, generated));
        }
        Ok(Self {
            ident: Arc::new(ShapeIdent {
                name: name.to_owned(),
                version,
                grain,
                measures,
            }),
            constraint_names,
        })
    }

    /// The projection name.
    pub fn name(&self) -> &str {
        &self.ident.name
    }

    /// The positive declaration version.
    pub fn version(&self) -> NonZeroU32 {
        self.ident.version
    }

    /// The ordered grain columns.
    pub fn grain(&self) -> &[GrainColumn] {
        &self.ident.grain
    }

    /// The ordered measures.
    pub fn measures(&self) -> &[MeasureColumn] {
        &self.ident.measures
    }

    fn same_shape(&self, ident: &Arc<ShapeIdent>) -> bool {
        Arc::ptr_eq(&self.ident, ident) || *self.ident == **ident
    }

    fn bad_value(&self, column: &str, why: &str) -> Error {
        invalid(
            self.name(),
            format!("grain value for {column} is not admissible: {why}"),
        )
    }

    /// Admit a grain key: arity and types must match, NULL only for nullable columns, text without
    /// NUL and within its byte cap, timestamps normalized to UTC without sub-microsecond digits,
    /// and dates/timestamps inside PostgreSQL's range. (`time`'s own default range is narrower
    /// than PostgreSQL's, so the upper bounds only matter with its `large-dates` feature.)
    pub fn admit_key(&self, values: Vec<GrainValue>) -> Result<CanonicalKey> {
        if values.len() != self.ident.grain.len() {
            return Err(invalid(self.name(), "grain key arity does not match"));
        }
        let mut out = Vec::with_capacity(values.len());
        for (col, value) in self.ident.grain.iter().zip(values) {
            let name = col.name.as_str();
            let admitted = match (col.ty, value) {
                (_, GrainValue::Null) if col.nullable => GrainValue::Null,
                (_, GrainValue::Null) => return Err(self.bad_value(name, "null not allowed")),
                (GrainType::Int8, v @ GrainValue::Int8(_))
                | (GrainType::Bool, v @ GrainValue::Bool(_))
                | (GrainType::Uuid, v @ GrainValue::Uuid(_)) => v,
                (GrainType::Date, GrainValue::Date(d)) => {
                    if d.to_julian_day() < 0 || d.year() > PG_DATE_MAX_YEAR {
                        return Err(self.bad_value(name, "date out of range"));
                    }
                    GrainValue::Date(d)
                }
                (GrainType::TimestampTz, GrainValue::TimestampTz(t)) => {
                    let Some(utc) = t.checked_to_offset(UtcOffset::UTC) else {
                        return Err(self.bad_value(name, "timestamp out of range"));
                    };
                    if utc.nanosecond() % 1000 != 0 {
                        return Err(self.bad_value(name, "sub-microsecond precision"));
                    }
                    if utc.date().to_julian_day() < 0 || utc.year() > PG_TIMESTAMP_MAX_YEAR {
                        return Err(self.bad_value(name, "timestamp out of range"));
                    }
                    GrainValue::TimestampTz(utc)
                }
                (GrainType::Text { max_bytes }, GrainValue::Text(s)) => {
                    if s.contains('\0') {
                        return Err(self.bad_value(name, "text contains NUL"));
                    }
                    if s.len() > max_bytes {
                        return Err(self.bad_value(name, "text above its byte cap"));
                    }
                    GrainValue::Text(s)
                }
                _ => return Err(self.bad_value(name, "wrong type")),
            };
            out.push(admitted);
        }
        Ok(CanonicalKey {
            ident: Arc::clone(&self.ident),
            values: out,
        })
    }

    fn check_key(&self, key: &CanonicalKey) -> Result<()> {
        if self.same_shape(&key.ident) {
            Ok(())
        } else {
            Err(invalid(
                self.name(),
                "key was admitted against another shape",
            ))
        }
    }

    /// Compare two keys admitted against this shape: lexicographic over the grain, ascending,
    /// NULL after every value, text by bytes, `false < true`, UUID by bytes, timestamps by instant.
    pub fn compare(&self, a: &CanonicalKey, b: &CanonicalKey) -> Result<Ordering> {
        self.check_key(a)?;
        self.check_key(b)?;
        compare_values(&a.values, &b.values)
    }

    /// Sort keys admitted against this shape into canonical order (stable).
    pub fn sort_keys(&self, keys: &mut [CanonicalKey]) -> Result<()> {
        for key in keys.iter() {
            self.check_key(key)?;
        }
        // Admission guarantees matching types, so the error arm is unreachable in practice; it is
        // carried out of the comparator instead of being hidden as `Equal`.
        let mut failure = None;
        keys.sort_by(|a, b| match compare_values(&a.values, &b.values) {
            Ok(order) => order,
            Err(e) => {
                failure.get_or_insert(e);
                Ordering::Equal
            }
        });
        failure.map_or(Ok(()), Err)
    }

    /// Admit one value per measure. Numeric values keep their exact value: a scale above the
    /// declared one (after dropping trailing zeros) or a magnitude of `10^(precision-scale)` or
    /// more is refused, never rounded.
    pub fn admit_measures(&self, values: Vec<MeasureValue>) -> Result<MeasureVector> {
        if values.len() != self.ident.measures.len() {
            return Err(invalid(self.name(), "measure vector arity does not match"));
        }
        let mut out = Vec::with_capacity(values.len());
        for (m, value) in self.ident.measures.iter().zip(values) {
            out.push(match (m.ty, value) {
                (MeasureType::Int8, MeasureValue::Int8(v)) => Stored::Int8(v),
                (MeasureType::Numeric { precision, scale }, MeasureValue::Numeric(d)) => {
                    let fail = || out_of_range(self.name(), &m.name);
                    let n = d.normalize();
                    if n.scale() > scale {
                        return Err(fail());
                    }
                    let factor = pow10(scale - n.scale()).ok_or_else(fail)?;
                    let mantissa = n.mantissa().checked_mul(factor).ok_or_else(fail)?;
                    if mantissa.unsigned_abs() >= pow10(precision).ok_or_else(fail)?.unsigned_abs()
                    {
                        return Err(fail());
                    }
                    Stored::Numeric(mantissa)
                }
                _ => {
                    return Err(invalid(
                        self.name(),
                        format!("measure {} has the wrong value type", m.name),
                    ));
                }
            });
        }
        Ok(MeasureVector {
            ident: Arc::clone(&self.ident),
            values: out,
        })
    }

    /// The values of a vector admitted against this shape, exactly (numerics at the declared scale).
    pub fn measure_values(&self, vector: &MeasureVector) -> Result<Vec<MeasureValue>> {
        if !self.same_shape(&vector.ident) {
            return Err(invalid(self.name(), "vector belongs to another shape"));
        }
        self.ident
            .measures
            .iter()
            .zip(&vector.values)
            .map(|(m, v)| match (m.ty, v) {
                (MeasureType::Numeric { scale, .. }, Stored::Numeric(mantissa)) => {
                    Decimal::try_from_i128_with_scale(*mantissa, scale)
                        .map(MeasureValue::Numeric)
                        .map_err(|_| out_of_range(self.name(), &m.name))
                }
                (_, Stored::Int8(v)) => Ok(MeasureValue::Int8(*v)),
                _ => Err(Error::internal(
                    "H27 measure vector disagrees with its shape",
                )),
            })
            .collect()
    }

    // ---- SQL fragments -------------------------------------------------------------------

    /// Column definitions of the grain: `"c" int8`, … , text as
    /// `"c" text COLLATE "C" CHECK (octet_length("c") <= N)`, plus `NOT NULL` when not nullable.
    pub fn grain_column_defs(&self) -> Vec<String> {
        self.ident
            .grain
            .iter()
            .map(|g| {
                let q = quote(&g.name);
                let ty = match g.ty {
                    GrainType::Int8 => "int8".to_owned(),
                    GrainType::Bool => "boolean".to_owned(),
                    GrainType::Date => "date".to_owned(),
                    GrainType::TimestampTz => "timestamptz".to_owned(),
                    GrainType::Uuid => "uuid".to_owned(),
                    GrainType::Text { max_bytes } => {
                        format!("text COLLATE \"C\" CHECK (octet_length({q}) <= {max_bytes})")
                    }
                };
                let null = if g.nullable { "" } else { " NOT NULL" };
                format!("{q} {ty}{null}")
            })
            .collect()
    }

    /// Column definitions of the measures: `"m" int8 NOT NULL` or `"m" numeric NOT NULL`
    /// (deliberately without a typmod, which would round input).
    pub fn measure_column_defs(&self) -> Vec<String> {
        self.ident
            .measures
            .iter()
            .map(|m| {
                let ty = match m.ty {
                    MeasureType::Int8 => "int8",
                    MeasureType::Numeric { .. } => "numeric",
                };
                format!("{} {ty} NOT NULL", quote(&m.name))
            })
            .collect()
    }

    /// The range predicate of measure `index` over `operand_sql`, evaluated as `numeric` so that
    /// no int8 cast or addition can raise before it runs. TRUE only for a value that fits (and for
    /// numeric measures has no digits beyond the declared scale); FALSE, never NULL, for NULL,
    /// NaN, `Infinity` and `-Infinity`. An `index` past the measures is Programming
    /// `H27 InvalidDefinition: {projection}: measure index out of range`.
    ///
    /// `operand_sql` is repeated several times in the text, so it must be a deterministic
    /// expression built from resolved declarations (a quoted column, a `sum(...)` of one), never
    /// caller text. For an int8 measure no named `CHECK` exists (the int8 cast of a bad sum raises
    /// an anonymous 22003 first); SQL-side attribution of int8 sums comes from slice D2's guarded
    /// conflict arm and slice D3's raise, which use this predicate.
    pub fn measure_range_predicate(&self, index: usize, operand_sql: &str) -> Result<String> {
        let Some(m) = self.ident.measures.get(index) else {
            return Err(invalid(self.name(), "measure index out of range"));
        };
        let x = format!("(({operand_sql})::numeric)");
        Ok(match m.ty {
            // NaN sorts above every number and the infinities are outside, so the comparisons are
            // FALSE; `scale()` is NULL for them, which `AND` absorbs after a FALSE.
            MeasureType::Int8 => format!(
                "({x} IS NOT NULL AND {x} BETWEEN -9223372036854775808 AND 9223372036854775807 \
                 AND scale({x}) = 0)"
            ),
            MeasureType::Numeric { precision, scale } => {
                let bound = format!("1{}", "0".repeat((precision - scale) as usize));
                format!(
                    "({x} IS NOT NULL AND abs({x}) < {bound} AND min_scale({x}) IS NOT NULL \
                     AND min_scale({x}) <= {scale})"
                )
            }
        })
    }

    /// One named `CHECK (...)` per numeric measure over its bare column: `(constraint name,
    /// check text)`. Int8 measures get none (see [`Self::measure_range_predicate`]).
    pub fn measure_check_constraints(&self) -> Result<Vec<(String, String)>> {
        self.constraint_names
            .iter()
            .map(|(index, name)| {
                let Some(m) = self.ident.measures.get(*index) else {
                    return Err(invalid(self.name(), "measure index out of range"));
                };
                let column = quote(&m.name);
                let predicate = self.measure_range_predicate(*index, &column)?;
                Ok((name.clone(), format!("CHECK ({predicate})")))
            })
            .collect()
    }

    /// Map a violated constraint name back to its numeric measure by exact lookup (never by
    /// parsing): Validation `H27 NumericOutOfRange: {projection}: {measure}`. Generated names
    /// flatten `.` to `_`, so `a.b` and `a_b` produce the same names: the D3 mapping must key on
    /// the table plus the constraint name, not the constraint name alone.
    pub fn measure_for_constraint(&self, constraint_name: &str) -> Option<Error> {
        let (index, _) = self
            .constraint_names
            .iter()
            .find(|(_, n)| n == constraint_name)?;
        Some(out_of_range(
            self.name(),
            &self.ident.measures.get(*index)?.name,
        ))
    }

    fn qualified(alias: &str, column: &str) -> String {
        if alias.is_empty() {
            quote(column)
        } else {
            format!("{}.{}", quote(alias), quote(column))
        }
    }

    /// The grain expressions of `alias` (text with `COLLATE "C"`), in declaration order.
    pub fn grain_exprs(&self, alias: &str) -> Vec<String> {
        self.ident
            .grain
            .iter()
            .map(|g| {
                let c = Self::qualified(alias, &g.name);
                match g.ty {
                    GrainType::Text { .. } => format!("{c} COLLATE \"C\""),
                    _ => c,
                }
            })
            .collect()
    }

    /// `ORDER BY` items in canonical order: every grain column `ASC NULLS LAST`, text `COLLATE "C"`.
    pub fn order_by(&self, alias: &str) -> String {
        self.grain_exprs(alias)
            .into_iter()
            .map(|e| format!("{e} ASC NULLS LAST"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `GROUP BY` items: the grain expressions, text with `COLLATE "C"`.
    pub fn group_by(&self, alias: &str) -> String {
        self.grain_exprs(alias).join(", ")
    }

    /// `UNIQUE NULLS NOT DISTINCT (grain...)`, the one-row-per-grain key.
    pub fn grain_key_definition(&self) -> String {
        let names: Vec<&str> = self.ident.grain.iter().map(|g| g.name.as_str()).collect();
        upsert::key_definition(&names, false)
    }
}

impl MeasureVector {
    /// Whether every measure is zero.
    pub fn is_zero(&self) -> bool {
        self.values
            .iter()
            .all(|v| matches!(v, Stored::Int8(0) | Stored::Numeric(0)))
    }

    /// Exact sum: int8 with `checked_add`, numeric on mantissas at the declared scale against
    /// `10^precision`. Overflow or a result out of range is Validation
    /// `H27 NumericOutOfRange: {projection}: {measure}`; vectors of another shape are refused.
    pub fn checked_add(
        &self,
        shape: &ProjectionShape,
        other: &MeasureVector,
    ) -> Result<MeasureVector> {
        if !shape.same_shape(&self.ident) || !shape.same_shape(&other.ident) {
            return Err(invalid(shape.name(), "vector belongs to another shape"));
        }
        let mut out = Vec::with_capacity(self.values.len());
        for ((m, a), b) in shape
            .ident
            .measures
            .iter()
            .zip(&self.values)
            .zip(&other.values)
        {
            let fail = || out_of_range(shape.name(), &m.name);
            out.push(match (m.ty, a, b) {
                (_, Stored::Int8(a), Stored::Int8(b)) => {
                    Stored::Int8(a.checked_add(*b).ok_or_else(fail)?)
                }
                (
                    MeasureType::Numeric { precision, .. },
                    Stored::Numeric(a),
                    Stored::Numeric(b),
                ) => {
                    let sum = a.checked_add(*b).ok_or_else(fail)?;
                    let bound = pow10(precision).ok_or_else(fail)?;
                    if sum.unsigned_abs() >= bound.unsigned_abs() {
                        return Err(fail());
                    }
                    Stored::Numeric(sum)
                }
                _ => {
                    return Err(Error::internal(
                        "H27 measure vector disagrees with its shape",
                    ));
                }
            });
        }
        Ok(MeasureVector {
            ident: Arc::clone(&self.ident),
            values: out,
        })
    }
}

fn compare_values(a: &[GrainValue], b: &[GrainValue]) -> Result<Ordering> {
    for (x, y) in a.iter().zip(b) {
        let ord = match (x, y) {
            (GrainValue::Null, GrainValue::Null) => Ordering::Equal,
            (GrainValue::Null, _) => Ordering::Greater,
            (_, GrainValue::Null) => Ordering::Less,
            (GrainValue::Int8(x), GrainValue::Int8(y)) => x.cmp(y),
            (GrainValue::Bool(x), GrainValue::Bool(y)) => x.cmp(y),
            (GrainValue::Date(x), GrainValue::Date(y)) => x.to_julian_day().cmp(&y.to_julian_day()),
            (GrainValue::TimestampTz(x), GrainValue::TimestampTz(y)) => {
                x.unix_timestamp_nanos().cmp(&y.unix_timestamp_nanos())
            }
            (GrainValue::Uuid(x), GrainValue::Uuid(y)) => x.0.cmp(&y.0),
            (GrainValue::Text(x), GrainValue::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
            _ => return Err(Error::internal("H27 internal: grain value type mismatch")),
        };
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;
    use erp_fields::ErrorKind;

    fn v1() -> NonZeroU32 {
        NonZeroU32::new(1).unwrap()
    }

    fn g(name: &str, ty: GrainType, nullable: bool) -> GrainColumn {
        GrainColumn::new(name, ty, nullable)
    }

    fn money(precision: u32, scale: u32) -> MeasureType {
        MeasureType::Numeric { precision, scale }
    }

    fn shape() -> ProjectionShape {
        ProjectionShape::new(
            "acct.balance",
            v1(),
            vec![
                g("account", GrainType::Int8, true),
                g("label", GrainType::Text { max_bytes: 16 }, true),
                g("day", GrainType::Date, true),
                g("posted", GrainType::Bool, true),
            ],
            vec![
                MeasureColumn::new("lines", MeasureType::Int8),
                MeasureColumn::new("amount", money(10, 2)),
            ],
            &DeltaLimits::default(),
        )
        .unwrap()
    }

    fn refuse(r: Result<ProjectionShape>, message: &str) {
        let e = r.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(e.message(), message);
    }

    fn decl(
        name: &str,
        grain: Vec<GrainColumn>,
        measures: Vec<MeasureColumn>,
    ) -> Result<ProjectionShape> {
        ProjectionShape::new(name, v1(), grain, measures, &DeltaLimits::default())
    }

    fn one_grain() -> Vec<GrainColumn> {
        vec![g("k", GrainType::Int8, false)]
    }

    fn one_measure() -> Vec<MeasureColumn> {
        vec![MeasureColumn::new("n", MeasureType::Int8)]
    }

    #[test]
    fn invalid_declarations_are_refused_with_the_complete_message() {
        decl("p", one_grain(), one_measure()).unwrap();
        refuse(
            decl("", one_grain(), one_measure()),
            "H27 InvalidDefinition: <unnamed>: projection name is not valid",
        );
        refuse(
            decl("Bad-Name", one_grain(), one_measure()),
            "H27 InvalidDefinition: <unnamed>: projection name is not valid",
        );
        refuse(
            decl(&"a".repeat(129), one_grain(), one_measure()),
            "H27 InvalidDefinition: <unnamed>: projection name is not valid",
        );
        decl(&"a".repeat(128), one_grain(), one_measure()).unwrap();
        refuse(
            decl("p", vec![], one_measure()),
            "H27 InvalidDefinition: p: grain column count out of range",
        );
        let many = (0..17)
            .map(|i| g(&format!("c{i}"), GrainType::Int8, false))
            .collect();
        refuse(
            decl("p", many, one_measure()),
            "H27 InvalidDefinition: p: grain column count out of range",
        );
        refuse(
            decl("p", one_grain(), vec![]),
            "H27 InvalidDefinition: p: measure count out of range",
        );
        let many = (0..17)
            .map(|i| MeasureColumn::new(format!("m{i}"), MeasureType::Int8))
            .collect();
        refuse(
            decl("p", one_grain(), many),
            "H27 InvalidDefinition: p: measure count out of range",
        );
        refuse(
            decl("p", vec![g("K", GrainType::Int8, false)], one_measure()),
            "H27 InvalidDefinition: p: a column name is not valid",
        );
        refuse(
            decl(
                "p",
                vec![g(&"k".repeat(64), GrainType::Int8, false)],
                one_measure(),
            ),
            "H27 InvalidDefinition: p: a column name is not valid",
        );
        for reserved in RESERVED_COLUMNS {
            refuse(
                decl(
                    "p",
                    vec![g(reserved, GrainType::Int8, false)],
                    one_measure(),
                ),
                &format!("H27 InvalidDefinition: p: column {reserved} is reserved"),
            );
        }
        refuse(
            decl(
                "p",
                one_grain(),
                vec![MeasureColumn::new("k", MeasureType::Int8)],
            ),
            "H27 InvalidDefinition: p: column k is declared twice",
        );
        refuse(
            decl(
                "p",
                vec![g("t", GrainType::Text { max_bytes: 0 }, false)],
                one_measure(),
            ),
            "H27 InvalidDefinition: p: column t text byte cap out of range",
        );
        refuse(
            decl(
                "p",
                vec![g("t", GrainType::Text { max_bytes: 1025 }, false)],
                one_measure(),
            ),
            "H27 InvalidDefinition: p: column t text byte cap out of range",
        );
        for (precision, scale) in [(0, 0), (29, 0), (5, 6)] {
            refuse(
                decl(
                    "p",
                    one_grain(),
                    vec![MeasureColumn::new("a", money(precision, scale))],
                ),
                "H27 InvalidDefinition: p: measure a precision or scale out of range",
            );
        }
        decl(
            "p",
            one_grain(),
            vec![MeasureColumn::new("a", money(28, 28))],
        )
        .unwrap();
        // Long measure names that differ only past the shortening cut stay distinct (the CRC of
        // the full name differs); the collision refusal itself guards a CRC-32 clash.
        let long = "m".repeat(60);
        decl(
            "p",
            one_grain(),
            vec![
                MeasureColumn::new(format!("{long}a"), MeasureType::Int8),
                MeasureColumn::new(format!("{long}b"), MeasureType::Int8),
            ],
        )
        .unwrap();
    }

    #[test]
    fn limits_can_be_lowered_never_zeroed_or_raised() {
        assert_eq!(DeltaLimits::default().grain_columns(), 16);
        assert_eq!(DeltaLimits::default().measures(), 16);
        assert_eq!(DeltaLimits::default().text_value_bytes(), 1024);
        assert_eq!(DeltaLimits::default().name_bytes(), 128);
        let low = DeltaLimits::lower(1, 1, 8, 8).unwrap();
        assert_eq!(low.grain_columns(), 1);
        for (bad, reason) in [
            (
                DeltaLimits::lower(0, 1, 1, 1),
                "grain columns must be 1..=16",
            ),
            (
                DeltaLimits::lower(17, 1, 1, 1),
                "grain columns must be 1..=16",
            ),
            (DeltaLimits::lower(1, 17, 1, 1), "measures must be 1..=16"),
            (
                DeltaLimits::lower(1, 1, 1025, 1),
                "text value bytes must be 1..=1024",
            ),
            (
                DeltaLimits::lower(1, 1, 1, 129),
                "name bytes must be 1..=128",
            ),
            (
                DeltaLimits::lower(1, 1, 0, 1),
                "text value bytes must be 1..=1024",
            ),
        ] {
            let e = bad.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Programming);
            assert_eq!(
                e.message(),
                format!("H27 InvalidDefinition: <limits>: {reason}")
            );
        }
        // A lowered limit binds the shape.
        let two = vec![
            g("a", GrainType::Int8, false),
            g("b", GrainType::Int8, false),
        ];
        refuse(
            ProjectionShape::new("p", v1(), two, one_measure(), &low),
            "H27 InvalidDefinition: p: grain column count out of range",
        );
    }

    fn key(s: &ProjectionShape, v: [GrainValue; 4]) -> CanonicalKey {
        s.admit_key(v.to_vec()).unwrap()
    }

    fn text(t: &str) -> GrainValue {
        GrainValue::Text(t.to_owned())
    }

    #[test]
    fn comparator_is_lexicographic_nulls_last_bytewise() {
        use GrainValue::{Int8, Null};
        let s = shape();
        let n = || Null;
        // Null after every value in each position, and the first position dominates.
        let a = key(&s, [Int8(5), n(), n(), n()]);
        let b = key(&s, [n(), text("a"), n(), n()]);
        assert_eq!(s.compare(&a, &b).unwrap(), Ordering::Less);
        let c = key(&s, [Int8(5), text("zz"), n(), n()]);
        assert_eq!(s.compare(&c, &a).unwrap(), Ordering::Less);
        let d = key(
            &s,
            [
                Int8(5),
                text("zz"),
                GrainValue::Date(Date::from_calendar_date(2000, time::Month::January, 1).unwrap()),
                n(),
            ],
        );
        let e = key(&s, [Int8(5), text("zz"), n(), n()]);
        assert_eq!(s.compare(&d, &e).unwrap(), Ordering::Less);
        let f = key(&s, [Int8(5), text("zz"), n(), GrainValue::Bool(false)]);
        let h = key(&s, [Int8(5), text("zz"), n(), GrainValue::Bool(true)]);
        let i = key(&s, [Int8(5), text("zz"), n(), n()]);
        assert_eq!(s.compare(&f, &h).unwrap(), Ordering::Less);
        assert_eq!(s.compare(&h, &i).unwrap(), Ordering::Less);
        // Bytewise text.
        let t = |x: &str| key(&s, [n(), text(x), n(), n()]);
        assert_eq!(s.compare(&t("B"), &t("a")).unwrap(), Ordering::Less);
        assert_eq!(s.compare(&t("a"), &t("\u{e1}")).unwrap(), Ordering::Less);
        assert_eq!(s.compare(&t("Z"), &t("\u{c4}")).unwrap(), Ordering::Less);
        // Repeated keys are equal; sorting is stable and canonical.
        assert_eq!(s.compare(&t("a"), &t("a")).unwrap(), Ordering::Equal);
        let mut keys = vec![t("\u{e1}"), t("a"), t("B"), a.clone(), t("a")];
        s.sort_keys(&mut keys).unwrap();
        let order: Vec<_> = keys.iter().map(|k| k.values()[1].clone()).collect();
        assert_eq!(order[0], Null);
        assert_eq!(
            &order[1..],
            &[text("B"), text("a"), text("a"), text("\u{e1}")]
        );
    }

    #[test]
    fn keys_of_another_shape_are_refused() {
        let s = shape();
        let other = ProjectionShape::new(
            "other",
            v1(),
            one_grain(),
            one_measure(),
            &DeltaLimits::default(),
        )
        .unwrap();
        let k = other.admit_key(vec![GrainValue::Int8(1)]).unwrap();
        let own = key(
            &s,
            [
                GrainValue::Null,
                GrainValue::Null,
                GrainValue::Null,
                GrainValue::Null,
            ],
        );
        let e = s.compare(&own, &k).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "H27 InvalidDefinition: acct.balance: key was admitted against another shape"
        );
        let e = s.sort_keys(&mut [k]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "H27 InvalidDefinition: acct.balance: key was admitted against another shape"
        );
        // An identically declared shape accepts the key.
        let twin = shape();
        assert!(twin.compare(&own, &own).is_ok());
    }

    #[test]
    fn key_admission_refuses_what_postgres_would_store_differently() {
        let t = ProjectionShape::new(
            "p",
            v1(),
            vec![
                g("ts", GrainType::TimestampTz, false),
                g("txt", GrainType::Text { max_bytes: 4 }, false),
            ],
            one_measure(),
            &DeltaLimits::default(),
        )
        .unwrap();
        let ts = |nanos: i128| {
            GrainValue::TimestampTz(OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap())
        };
        let msg = |why: &str, col: &str| {
            format!("H27 InvalidDefinition: p: grain value for {col} is not admissible: {why}")
        };
        let err = |v: Vec<GrainValue>| t.admit_key(v).unwrap_err();
        let e = err(vec![ts(1_000_000_001), text("a")]);
        assert_eq!(
            (e.kind(), e.message()),
            (
                ErrorKind::Programming,
                msg("sub-microsecond precision", "ts").as_str()
            )
        );
        assert_eq!(
            err(vec![ts(1_000), text("a\0")]).message(),
            msg("text contains NUL", "txt")
        );
        assert_eq!(
            err(vec![ts(1_000), text("abcde")]).message(),
            msg("text above its byte cap", "txt")
        );
        assert_eq!(
            err(vec![ts(1_000), GrainValue::Null]).message(),
            msg("null not allowed", "txt")
        );
        assert_eq!(
            err(vec![ts(1_000), GrainValue::Int8(1)]).message(),
            msg("wrong type", "txt")
        );
        assert_eq!(
            err(vec![ts(1_000)]).message(),
            "H27 InvalidDefinition: p: grain key arity does not match"
        );
        // An offset timestamp is normalized to UTC and equals the same instant.
        let plus = OffsetDateTime::from_unix_timestamp(10)
            .unwrap()
            .to_offset(UtcOffset::from_hms(5, 0, 0).unwrap());
        let a = t
            .admit_key(vec![GrainValue::TimestampTz(plus), text("a")])
            .unwrap();
        let b = t.admit_key(vec![ts(10_000_000_000), text("a")]).unwrap();
        assert_eq!(t.compare(&a, &b).unwrap(), Ordering::Equal);
        // Before PostgreSQL's lowest timestamp and date (4714-11-24 BC).
        let early = OffsetDateTime::new_utc(Date::MIN, time::Time::MIDNIGHT);
        let e = shape()
            .admit_key(vec![
                GrainValue::Null,
                GrainValue::Null,
                GrainValue::Date(Date::MIN),
                GrainValue::Null,
            ])
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "H27 InvalidDefinition: acct.balance: grain value for day is not admissible: date out of range"
        );
        let e = t
            .admit_key(vec![GrainValue::TimestampTz(early), text("a")])
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(e.message(), msg("timestamp out of range", "ts"));
        // Normalizing to UTC would leave time's range: refused, not a panic.
        let edge = time::Time::from_hms(23, 30, 0).unwrap();
        let edge = Date::MAX
            .with_time(edge)
            .assume_offset(UtcOffset::from_hms(-1, 0, 0).unwrap());
        let e = t
            .admit_key(vec![GrainValue::TimestampTz(edge), text("a")])
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(e.message(), msg("timestamp out of range", "ts"));
    }

    fn vector(s: &ProjectionShape, lines: i64, amount: &str) -> Result<MeasureVector> {
        s.admit_measures(vec![
            MeasureValue::Int8(lines),
            MeasureValue::Numeric(amount.parse().unwrap()),
        ])
    }

    fn range_error(r: Result<MeasureVector>, measure: &str) {
        let e = r.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(
            e.message(),
            format!("H27 NumericOutOfRange: acct.balance: {measure}")
        );
    }

    #[test]
    fn numeric_admission_is_exact_at_each_bound() {
        let s = shape();
        // (10, 2): below 10^8, two decimals.
        assert!(vector(&s, 0, "99999999.99").is_ok());
        assert!(vector(&s, 0, "-99999999.99").is_ok());
        range_error(vector(&s, 0, "100000000.00"), "amount");
        range_error(vector(&s, 0, "-100000000"), "amount");
        range_error(vector(&s, 0, "1.005"), "amount");
        // Trailing zeros are exact, not excess scale.
        let v = vector(&s, 0, "1.500").unwrap();
        assert_eq!(
            s.measure_values(&v).unwrap(),
            vec![
                MeasureValue::Int8(0),
                MeasureValue::Numeric("1.50".parse().unwrap())
            ]
        );
        assert_eq!(
            s.measure_values(&v).unwrap()[1],
            MeasureValue::Numeric(Decimal::new(150, 2))
        );
        // (28, 0): 28 nines fit, 10^28 does not.
        let wide = ProjectionShape::new(
            "acct.balance",
            v1(),
            one_grain(),
            vec![MeasureColumn::new("amount", money(28, 0))],
            &DeltaLimits::default(),
        )
        .unwrap();
        let adm = |t: &str| wide.admit_measures(vec![MeasureValue::Numeric(t.parse().unwrap())]);
        assert!(adm("9999999999999999999999999999").is_ok());
        range_error(adm("10000000000000000000000000000"), "amount");
        range_error(adm("79228162514264337593543950335"), "amount");
        // Wrong arity and type are definition errors.
        let e = s.admit_measures(vec![]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "H27 InvalidDefinition: acct.balance: measure vector arity does not match"
        );
        let e = s
            .admit_measures(vec![
                MeasureValue::Numeric(Decimal::ZERO),
                MeasureValue::Int8(0),
            ])
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "H27 InvalidDefinition: acct.balance: measure lines has the wrong value type"
        );
    }

    #[test]
    fn same_name_and_version_with_another_layout_is_another_shape() {
        let s = shape();
        // Same projection name and version as `shape()`, the `posted` grain is not nullable.
        let changed = ProjectionShape::new(
            "acct.balance",
            v1(),
            vec![
                g("account", GrainType::Int8, true),
                g("label", GrainType::Text { max_bytes: 16 }, true),
                g("day", GrainType::Date, true),
                g("posted", GrainType::Bool, false),
            ],
            vec![
                MeasureColumn::new("lines", MeasureType::Int8),
                MeasureColumn::new("amount", money(10, 2)),
            ],
            &DeltaLimits::default(),
        )
        .unwrap();
        let values = [
            GrainValue::Int8(1),
            GrainValue::Null,
            GrainValue::Null,
            GrainValue::Bool(true),
        ];
        let theirs = key(&changed, values.clone());
        let own = key(&s, values);
        for e in [
            s.compare(&own, &theirs).unwrap_err(),
            s.sort_keys(&mut [theirs]).unwrap_err(),
        ] {
            assert_eq!(e.kind(), ErrorKind::Programming);
            assert_eq!(
                e.message(),
                "H27 InvalidDefinition: acct.balance: key was admitted against another shape"
            );
        }
        let foreign = vector(&changed, 1, "1.00").unwrap();
        let mine = vector(&s, 1, "1.00").unwrap();
        for e in [
            mine.checked_add(&s, &foreign).unwrap_err(),
            s.measure_values(&foreign).unwrap_err(),
        ] {
            assert_eq!(e.kind(), ErrorKind::Programming);
            assert_eq!(
                e.message(),
                "H27 InvalidDefinition: acct.balance: vector belongs to another shape"
            );
        }
    }

    #[test]
    fn sums_are_exact_and_overflow_is_refused() {
        let s = shape();
        let a = vector(&s, i64::MAX - 1, "99999999.98").unwrap();
        let b = vector(&s, 1, "0.01").unwrap();
        let sum = a.checked_add(&s, &b).unwrap();
        assert_eq!(
            s.measure_values(&sum).unwrap(),
            vec![
                MeasureValue::Int8(i64::MAX),
                MeasureValue::Numeric("99999999.99".parse().unwrap())
            ]
        );
        assert!(!sum.is_zero());
        // int8 overflow names the measure.
        range_error(sum.checked_add(&s, &vector(&s, 1, "0").unwrap()), "lines");
        // numeric overflow names the measure.
        range_error(
            sum.checked_add(&s, &vector(&s, 0, "0.01").unwrap()),
            "amount",
        );
        // Cancelling vectors are zero.
        let neg = vector(&s, -5, "-1.25").unwrap();
        let pos = vector(&s, 5, "1.25").unwrap();
        assert!(neg.checked_add(&s, &pos).unwrap().is_zero());
        // A vector of another shape is refused.
        let other = ProjectionShape::new(
            "other",
            v1(),
            one_grain(),
            vec![
                MeasureColumn::new("lines", MeasureType::Int8),
                MeasureColumn::new("amount", money(10, 2)),
            ],
            &DeltaLimits::default(),
        )
        .unwrap();
        let foreign = other
            .admit_measures(vec![
                MeasureValue::Int8(1),
                MeasureValue::Numeric(Decimal::ONE),
            ])
            .unwrap();
        for e in [
            pos.checked_add(&s, &foreign).unwrap_err(),
            s.measure_values(&foreign).unwrap_err(),
        ] {
            assert_eq!(e.kind(), ErrorKind::Programming);
            assert_eq!(
                e.message(),
                "H27 InvalidDefinition: acct.balance: vector belongs to another shape"
            );
        }
    }

    #[test]
    fn fragments_are_quoted_and_constraints_map_back() {
        let s = shape();
        assert_eq!(
            s.grain_column_defs()[1],
            "\"label\" text COLLATE \"C\" CHECK (octet_length(\"label\") <= 16)"
        );
        assert_eq!(
            s.measure_column_defs(),
            ["\"lines\" int8 NOT NULL", "\"amount\" numeric NOT NULL"]
        );
        assert_eq!(
            s.order_by("a"),
            "\"a\".\"account\" ASC NULLS LAST, \"a\".\"label\" COLLATE \"C\" ASC NULLS LAST, \
             \"a\".\"day\" ASC NULLS LAST, \"a\".\"posted\" ASC NULLS LAST"
        );
        assert!(
            s.group_by("")
                .starts_with("\"account\", \"label\" COLLATE \"C\"")
        );
        assert_eq!(
            s.grain_key_definition(),
            "UNIQUE NULLS NOT DISTINCT (\"account\", \"label\", \"day\", \"posted\")"
        );
        // Only the numeric measure gets a named CHECK; int8 sums are attributed by D2/D3.
        let checks = s.measure_check_constraints().unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].0, "acct_balance__amount_h27_range");
        assert!(
            checks[0]
                .1
                .contains("abs(((\"amount\")::numeric)) < 100000000 ")
        );
        let e = s.measure_for_constraint(&checks[0].0).unwrap();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(e.message(), "H27 NumericOutOfRange: acct.balance: amount");
        assert!(
            s.measure_for_constraint("acct_balance__lines_h27_range")
                .is_none()
        );
        assert!(s.measure_for_constraint("unknown").is_none());
        let e = s.measure_range_predicate(2, "x").unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "H27 InvalidDefinition: acct.balance: measure index out of range"
        );
        assert!(
            s.measure_range_predicate(0, "x")
                .unwrap()
                .contains("scale(")
        );
    }
}
