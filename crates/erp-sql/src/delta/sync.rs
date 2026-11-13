// SPDX-License-Identifier: MIT OR Apache-2.0
//! The synchronous guarded apply (slice D2, amendment B5, contract D1 `apply_statement`,
//! D3 `SynchronousGuarded`, D5, D7). Builds statement text and interprets
//! rows; executing is the ORM layer's (`erp_orm::delta::apply_synchronous`).
//!
//! - [`ProjectionShape::net`]: contributions to one grain are summed exactly and independent of
//!   their order (only the final net is range-checked), zero nets are dropped, the rest is in
//!   canonical key order and bounded by [`DeltaLimits::batch_rows`] / [`DeltaLimits::batch_bytes`].
//! - [`SyncApply`]: one H22 upsert statement ([`crate::upsert`]) over a consumer table with
//!   [`MeasureGuard`] floors, evaluated in the database. The update arm refuses a sum that is out
//!   of range or below a floor (the row is locked, left unchanged and returned unwritten); the
//!   insert arm refuses a first delta that violates a floor unless the key already exists.
//! - [`SyncApply::interpret`]: attributes every unwritten or missing row exactly, in Rust, so a
//!   refusal is the consumer's own text or `H27 NumericOutOfRange`, never an anonymous database error.
//!   It also re-checks every row that came back written (range, every guard, and an inserted row
//!   against the net), so a row the database wrote without its guard (a key deleted concurrently,
//!   a key constraint that does not arbitrate the grain) is a failure, never a success.
//!
//! DO UPDATE ... WHERE` evaluated before the update;
//! modified: the guard is data (floors) the database enforces in the same statement, and the
//! refusal text is attributed from the returned rows without parsing any message.

use erp_fields::{ColumnType, DateTime, ErrorKind};
use erp_registry::ModelSpec;

use super::*;
use crate::upsert::{ConflictAction, KeyColumn, SetAction, Upsert, key_match};

/// Result column index of the first measure in the rows of [`SyncApply::statement`]
/// (`ordinal`, `id`, `inserted`, `written`, two version columns, then one column per measure,
/// the transaction timestamp, and last `existed`; see [`SyncApply::result_existed_column`]).
///
/// Only `erp_orm::delta::apply_synchronous` may use it.
#[doc(hidden)]
pub const RESULT_FIRST_MEASURE: usize = 6;

impl DeltaLimits {
    /// Lower the batch caps of contract D7: at most `rows` netted rows and `bytes` canonical
    /// encoded bytes per apply. Zero or above the hard maximum (2,000 rows, 8 MiB) is Programming
    /// `H27 InvalidDefinition: <limits>: ...`.
    pub fn with_batch(self, rows: usize, bytes: usize) -> Result<Self> {
        for (what, value, max) in [
            ("batch rows", rows, MAX_BATCH_ROWS),
            ("batch bytes", bytes, MAX_BATCH_BYTES),
        ] {
            if value == 0 || value > max {
                return Err(invalid("<limits>", format!("{what} must be 1..={max}")));
            }
        }
        Ok(Self {
            batch_rows: rows,
            batch_bytes: bytes,
            ..self
        })
    }

    /// Lower the contribution cap of contract D7: at most `rows` contributions are accepted by
    /// one [`ProjectionShape::net`] (default and hard maximum 32,000). Zero or above the maximum
    /// is Programming `H27 InvalidDefinition: <limits>: contribution rows must be 1..=32000`.
    pub fn with_contributions(self, rows: usize) -> Result<Self> {
        if rows == 0 || rows > MAX_CONTRIBUTION_ROWS {
            return Err(invalid(
                "<limits>",
                format!("contribution rows must be 1..={MAX_CONTRIBUTION_ROWS}"),
            ));
        }
        Ok(Self {
            contribution_rows: rows,
            ..self
        })
    }

    /// Maximum contributions handed to one netting.
    pub fn contribution_rows(&self) -> usize {
        self.contribution_rows
    }

    /// Maximum netted rows of one apply batch.
    pub fn batch_rows(&self) -> usize {
        self.batch_rows
    }

    /// Maximum canonical encoded bytes of one apply batch.
    pub fn batch_bytes(&self) -> usize {
        self.batch_bytes
    }
}

fn limit_exceeded(what: &str) -> Error {
    Error::new(
        ErrorKind::ResourceExhausted,
        format!("H27 LimitExceeded: {what}"),
    )
}

fn unattributed(projection: &str) -> Error {
    Error::internal(format!("H27 ApplyUnattributed: {projection}"))
}

fn as_i128(ty: MeasureType, value: &Stored) -> Result<i128> {
    match (ty, value) {
        (MeasureType::Int8, Stored::Int8(v)) => Ok(i128::from(*v)),
        (MeasureType::Numeric { .. }, Stored::Numeric(m)) => Ok(*m),
        _ => Err(Error::internal(
            "H27 measure vector disagrees with its shape",
        )),
    }
}

fn key_bytes(key: &CanonicalKey) -> usize {
    key.values
        .iter()
        .map(|v| match v {
            GrainValue::Null | GrainValue::Bool(_) => 1,
            GrainValue::Int8(_) | GrainValue::Date(_) | GrainValue::TimestampTz(_) => 8,
            GrainValue::Uuid(_) => 16,
            GrainValue::Text(t) => t.len(),
        })
        .fold(0usize, usize::saturating_add)
}

/// Contributions of one batch netted per grain key: distinct keys in canonical order, no
/// all-zero net, bound to one shape (a [`SyncApply`] of another shape refuses it).
#[derive(Debug, Clone)]
pub struct NettedBatch {
    ident: Arc<ShapeIdent>,
    keys: Vec<CanonicalKey>,
    vectors: Vec<MeasureVector>,
}

impl NettedBatch {
    /// Number of netted rows.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the batch has no row (every net was zero, or nothing was contributed).
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The keys, in canonical order.
    pub fn keys(&self) -> &[CanonicalKey] {
        &self.keys
    }

    /// The net of each key, in the order of [`NettedBatch::keys`].
    pub fn vectors(&self) -> &[MeasureVector] {
        &self.vectors
    }
}

impl ProjectionShape {
    /// Net contributions per grain key.
    ///
    /// Each measure is summed exactly (int8 and numeric as 128-bit mantissas at the declared
    /// scale), in any order: only the final net of a key must fit its measure, so `+max, +1, -1`
    /// is accepted in every order. A net whose measures are all zero is dropped. The result is in
    /// canonical order. Refusals: ResourceExhausted `H27 LimitExceeded: contribution rows` when
    /// more than [`DeltaLimits::contribution_rows`] contributions are given (before anything is
    /// allocated or compared); Validation `H27 NumericOutOfRange: {projection}: {measure}`;
    /// ResourceExhausted `H27 LimitExceeded: batch rows` (distinct keys, counted while grouping and
    /// before a key is cloned, so a key that nets to zero still counts) or `H27 LimitExceeded: batch bytes`
    /// (canonical encoded bytes: int8, date, timestamptz 8; bool and NULL 1; uuid 16; text its
    /// UTF-8 bytes; each int8 measure 8 and each numeric 16), before any SQL; keys or vectors of
    /// another shape are Programming `H27 InvalidDefinition`.
    pub fn net(
        &self,
        contributions: Vec<(CanonicalKey, MeasureVector)>,
        limits: &DeltaLimits,
    ) -> Result<NettedBatch> {
        if contributions.len() > limits.contribution_rows {
            return Err(limit_exceeded("contribution rows"));
        }
        for (key, vector) in &contributions {
            self.check_key(key)?;
            if !self.same_shape(&vector.ident) {
                return Err(invalid(self.name(), "vector belongs to another shape"));
            }
        }
        let mut order: Vec<usize> = (0..contributions.len()).collect();
        let mut failure = None;
        order.sort_by(|a, b| {
            match compare_values(&contributions[*a].0.values, &contributions[*b].0.values) {
                Ok(o) => o,
                Err(e) => {
                    failure.get_or_insert(e);
                    Ordering::Equal
                }
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
        let measures = &self.ident.measures;
        let mut keys: Vec<CanonicalKey> = Vec::new();
        let mut sums: Vec<Vec<i128>> = Vec::new();
        let mut previous: Option<usize> = None;
        for index in order {
            let (key, vector) = &contributions[index];
            let same = match previous {
                Some(p) => {
                    compare_values(&contributions[p].0.values, &key.values)? == Ordering::Equal
                }
                None => false,
            };
            if !same {
                if keys.len() >= limits.batch_rows {
                    return Err(limit_exceeded("batch rows"));
                }
                keys.push(key.clone());
                sums.push(vec![0; measures.len()]);
            }
            previous = Some(index);
            let Some(acc) = sums.last_mut() else {
                return Err(Error::internal("H27 netting lost its accumulator"));
            };
            for ((slot, value), m) in acc.iter_mut().zip(&vector.values).zip(measures) {
                *slot = slot
                    .checked_add(as_i128(m.ty, value)?)
                    .ok_or_else(|| out_of_range(self.name(), &m.name))?;
            }
        }
        let mut batch = NettedBatch {
            ident: Arc::clone(&self.ident),
            keys: Vec::with_capacity(keys.len()),
            vectors: Vec::with_capacity(keys.len()),
        };
        let mut bytes = 0usize;
        for (key, sum) in keys.into_iter().zip(sums) {
            if sum.iter().all(|v| *v == 0) {
                continue;
            }
            let mut values = Vec::with_capacity(sum.len());
            for (m, v) in measures.iter().zip(&sum) {
                let fail = || out_of_range(self.name(), &m.name);
                values.push(match m.ty {
                    MeasureType::Int8 => Stored::Int8(i64::try_from(*v).map_err(|_| fail())?),
                    MeasureType::Numeric { precision, .. } => {
                        let bound = pow10(precision).ok_or_else(fail)?;
                        if v.unsigned_abs() >= bound.unsigned_abs() {
                            return Err(fail());
                        }
                        Stored::Numeric(*v)
                    }
                });
                bytes = bytes.saturating_add(match m.ty {
                    MeasureType::Int8 => 8,
                    MeasureType::Numeric { .. } => 16,
                });
            }
            bytes = bytes.saturating_add(key_bytes(&key));
            batch.keys.push(key);
            batch.vectors.push(MeasureVector {
                ident: Arc::clone(&self.ident),
                values,
            });
        }
        if bytes > limits.batch_bytes {
            return Err(limit_exceeded("batch bytes"));
        }
        Ok(batch)
    }

    fn zero_vector(&self) -> MeasureVector {
        MeasureVector {
            ident: Arc::clone(&self.ident),
            values: self
                .ident
                .measures
                .iter()
                .map(|m| match m.ty {
                    MeasureType::Int8 => Stored::Int8(0),
                    MeasureType::Numeric { .. } => Stored::Numeric(0),
                })
                .collect(),
        }
    }
}

/// A floor or other check the database enforces on the sum of a measure and a consumer's refusal
/// text for it. The text is the consumer's static message (contract D5): it never holds a grain
/// value, so a refusal cannot leak the key it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MeasureGuard {
    /// The measure after the apply must be `>= floor`.
    AtLeast {
        /// The measure name.
        measure: String,
        /// The floor, of the measure's type.
        floor: MeasureValue,
        /// The Validation message of a refusal.
        refusal: &'static str,
    },
}

impl MeasureGuard {
    /// A floor on `measure`; [`SyncApply::new`] validates it.
    pub fn at_least(
        measure: impl Into<String>,
        floor: MeasureValue,
        refusal: &'static str,
    ) -> Self {
        Self::AtLeast {
            measure: measure.into(),
            floor,
            refusal,
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedGuard {
    index: usize,
    floor: Stored,
    floor_sql: String,
    refusal: &'static str,
}

/// The synchronous guarded apply of one projection onto one consumer table: a validated
/// declaration (contract D1 `apply_statement`). The table must hold the grain and measure columns
/// under their declared names, an `id` column, and the grain key constraint
/// ([`ProjectionShape::grain_key_definition`]) under the given name; other columns (a receipt
/// date, addon state) are never read or written by the statement, except a column named by
/// [`SyncApply::with_insert_fill`], which the insert arm writes (amended B7).
///
/// Scope: the statement writes the table directly, without access rules, cache update or write
/// hooks, so it is for tables whose writes the consumer's adapter owns.
#[derive(Debug, Clone)]
pub struct SyncApply {
    shape: ProjectionShape,
    table: String,
    constraint: String,
    guards: Vec<ResolvedGuard>,
    fills: Vec<(String, InsertFill)>,
}

/// The insert-only value of a column outside the grain and measures
/// ([`SyncApply::with_insert_fill`], amendment B7). A `NOT NULL` column without a database
/// default needs one: PostgreSQL checks `NOT NULL` on the proposed row before it resolves the
/// conflict, so without a value the statement fails even for a key that exists, and schema sync refuses
/// a database default set outside the ORM. A closed set: the caller never passes SQL text.
///
/// **Insert-only.** The value is written only when the statement inserts the key; a row that exists
/// keeps its stored value, and the update arm never names the column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InsertFill {
    /// `now()` in UTC: the start time of the transaction, so every row the transaction inserts gets
    /// the same value.
    TransactionTime,
    /// This timestamp (UTC, without time zone), bound as one statement parameter after the arrays.
    Value(DateTime),
}

impl SyncApply {
    /// Validate a declaration. Every refusal is Programming
    /// `H27 InvalidDefinition: {projection}: {reason}`: a table or constraint name that is not a
    /// lowercase identifier of at most 63 bytes, a guard on an unknown measure, two guards on one
    /// measure, a floor of the wrong type or outside the measure's range, or an empty refusal text.
    pub fn new(
        shape: &ProjectionShape,
        table: &str,
        constraint: &str,
        guards: Vec<MeasureGuard>,
    ) -> Result<Self> {
        let name = shape.name();
        if table.len() > MAX_COLUMN_NAME_BYTES || !valid_ident(table, false) {
            return Err(invalid(name, "table name is not valid"));
        }
        if constraint.len() > MAX_COLUMN_NAME_BYTES || !valid_ident(constraint, false) {
            return Err(invalid(name, "constraint name is not valid"));
        }
        let mut resolved: Vec<ResolvedGuard> = Vec::with_capacity(guards.len());
        for guard in guards {
            let MeasureGuard::AtLeast {
                measure,
                floor,
                refusal,
            } = guard;
            let Some(index) = shape.measures().iter().position(|m| m.name == measure) else {
                return Err(invalid(name, "a guard names an unknown measure"));
            };
            let m = &shape.ident.measures[index];
            if resolved.iter().any(|g| g.index == index) {
                return Err(invalid(name, format!("measure {} has two guards", m.name)));
            }
            if refusal.is_empty() {
                return Err(invalid(
                    name,
                    format!("guard refusal text of {} is empty", m.name),
                ));
            }
            let not_admissible =
                || invalid(name, format!("guard floor of {} is not admissible", m.name));
            let mut values: Vec<MeasureValue> = shape
                .ident
                .measures
                .iter()
                .map(|other| match other.ty {
                    MeasureType::Int8 => MeasureValue::Int8(0),
                    MeasureType::Numeric { .. } => MeasureValue::Numeric(Decimal::ZERO),
                })
                .collect();
            values[index] = floor;
            let admitted = shape.admit_measures(values).map_err(|_| not_admissible())?;
            let stored = admitted.values[index];
            let floor_sql = match (m.ty, stored) {
                (MeasureType::Int8, Stored::Int8(v)) => v.to_string(),
                (MeasureType::Numeric { scale, .. }, Stored::Numeric(mantissa)) => {
                    Decimal::try_from_i128_with_scale(mantissa, scale)
                        .map_err(|_| not_admissible())?
                        .to_string()
                }
                _ => return Err(not_admissible()),
            };
            resolved.push(ResolvedGuard {
                index,
                floor: stored,
                floor_sql: format!("({floor_sql})"),
                refusal,
            });
        }
        Ok(Self {
            shape: shape.clone(),
            table: table.to_owned(),
            constraint: constraint.to_owned(),
            guards: resolved,
            fills: Vec::new(),
        })
    }

    /// Fill `column` of `model` on the insert arm only ([`InsertFill`]). Refused (Programming
    /// `H27 InvalidDefinition`) when `model` is not the model of the apply's table, or the column
    /// does not exist on it, is a generated column, is a grain or measure column, is named twice, or
    /// is not a stored `timestamp` column.
    pub fn with_insert_fill(
        mut self,
        model: &ModelSpec,
        column: &str,
        fill: InsertFill,
    ) -> Result<Self> {
        let name = self.shape.name();
        if model.table != self.table {
            return Err(invalid(
                name,
                format!(
                    "insert fill model {} has table {}, not {}",
                    model.name, model.table, self.table
                ),
            ));
        }
        let Some(field) = model.field(column) else {
            return Err(invalid(
                name,
                format!(
                    "insert fill column {column} does not exist on model {}",
                    model.name
                ),
            ));
        };
        if field.is_generated() {
            return Err(invalid(
                name,
                format!("insert fill column {column} is a generated column"),
            ));
        }
        if self.shape.grain().iter().any(|g| g.name == column)
            || self.shape.measures().iter().any(|m| m.name == column)
        {
            return Err(invalid(
                name,
                format!("insert fill column {column} is a grain or measure column"),
            ));
        }
        if self.fills.iter().any(|(c, _)| c == column) {
            return Err(invalid(
                name,
                format!("insert fill column {column} is named twice"),
            ));
        }
        if field.column() != Some(ColumnType::Timestamp) {
            return Err(invalid(
                name,
                format!("insert fill column {column} is not a stored timestamp column"),
            ));
        }
        self.fills.push((column.to_owned(), fill));
        Ok(self)
    }

    /// The values of the [`InsertFill::Value`] fills, in declaration order: the statement's
    /// parameters after the grain and measure arrays.
    ///
    /// Only `erp_orm::delta::apply_synchronous` may use it.
    #[doc(hidden)]
    pub fn insert_fill_values(&self) -> Vec<DateTime> {
        self.fills
            .iter()
            .filter_map(|(_, f)| match f {
                InsertFill::Value(v) => Some(*v),
                InsertFill::TransactionTime => None,
            })
            .collect()
    }

    /// The projection shape.
    pub fn shape(&self) -> &ProjectionShape {
        &self.shape
    }

    /// The consumer table.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Refuse a batch netted against another shape.
    pub fn check_batch(&self, batch: &NettedBatch) -> Result<()> {
        if self.shape.same_shape(&batch.ident) {
            Ok(())
        } else {
            Err(invalid(
                self.shape.name(),
                "batch was netted against another shape",
            ))
        }
    }

    /// The statement text: the H22 upsert over [`SyncApply::table`] with every measure
    /// accumulated without clamping, the update arm guarded by the exact range of the new sum and
    /// every floor, and the insert arm filtered to a first delta that fits and meets every floor
    /// unless the key already exists (so a conflicting row still reaches the guarded update arm).
    ///
    /// Parameters `$1..$n`: one array per grain column then one per measure, in declaration
    /// order, one element per netted row **in canonical order** (the rows are inserted, so locked,
    /// in that order). Result rows, one per written row and per row the update arm refused: see
    /// [`crate::upsert::upsert`]; the measure values start at [`RESULT_FIRST_MEASURE`]. A row the
    /// insert arm filtered is absent. `WHERE` is checked before `SET`, so an int8 sum out of range
    /// is refused by the predicate and never raises `22003`. The last result column is `existed`
    /// ([`SyncApply::result_existed_column`]): whether the key was present in the statement's
    /// snapshot.
    ///
    /// Only `erp_orm::delta::apply_synchronous` may use it: it is the one caller that poisons the
    /// transaction on every failure.
    #[doc(hidden)]
    pub fn statement(&self) -> Result<String> {
        let mut columns: Vec<(&str, ColumnType)> = Vec::new();
        let mut key: Vec<KeyColumn<'_>> = Vec::new();
        for g in self.shape.grain() {
            let ty = match g.ty {
                GrainType::Int8 => ColumnType::Int8,
                GrainType::Bool => ColumnType::Bool,
                GrainType::Date => ColumnType::Date,
                GrainType::TimestampTz => ColumnType::TimestampTz,
                GrainType::Uuid => ColumnType::Uuid,
                GrainType::Text { .. } => ColumnType::Text,
            };
            columns.push((g.name.as_str(), ty));
            key.push(KeyColumn {
                name: g.name.as_str(),
                nullable: g.nullable,
            });
        }
        let mut sets: Vec<(&str, SetAction)> = Vec::new();
        let mut returning: Vec<(&str, ColumnType)> = Vec::new();
        for m in self.shape.measures() {
            let ty = match m.ty {
                MeasureType::Int8 => ColumnType::Int8,
                MeasureType::Numeric { .. } => ColumnType::Numeric,
            };
            columns.push((m.name.as_str(), ty));
            sets.push((m.name.as_str(), SetAction::Accumulate));
            returning.push((m.name.as_str(), ty));
        }
        let target = quote("__t");
        let mut update: Vec<String> = Vec::new();
        let mut insert: Vec<String> = Vec::new();
        for (i, m) in self.shape.measures().iter().enumerate() {
            let col = quote(&m.name);
            let sum = format!("COALESCE({target}.{col}, 0)::numeric + EXCLUDED.{col}::numeric");
            update.push(self.shape.measure_range_predicate(i, &sum)?);
            insert.push(
                self.shape
                    .measure_range_predicate(i, &format!("\"__in\".{col}"))?,
            );
        }
        for g in &self.guards {
            let col = quote(&self.shape.ident.measures[g.index].name);
            update.push(format!(
                "((COALESCE({target}.{col}, 0)::numeric + EXCLUDED.{col}::numeric) >= {})",
                g.floor_sql
            ));
            insert.push(format!("((\"__in\".{col})::numeric >= {})", g.floor_sql));
        }
        let names: Vec<String> = key.iter().map(|k| k.name.to_owned()).collect();
        let exists = format!(
            "EXISTS (SELECT FROM {} AS \"__e\" WHERE {})",
            quote(&self.table),
            key_match(&key, "__in", "__e", &names)
        );
        let guard = update.join(" AND ");
        let filter = format!("(({}) OR {exists})", insert.join(" AND "));
        let mut next = self.shape.grain().len() + self.shape.measures().len();
        let fill_sql: Vec<String> = self
            .fills
            .iter()
            .map(|(_, f)| match f {
                InsertFill::TransactionTime => "(now() AT TIME ZONE 'UTC')".to_owned(),
                InsertFill::Value(_) => {
                    next += 1;
                    format!("${next}::timestamp")
                }
            })
            .collect();
        let fills: Vec<(&str, &str)> = self
            .fills
            .iter()
            .zip(&fill_sql)
            .map(|((c, _), e)| (c.as_str(), e.as_str()))
            .collect();
        Ok(crate::upsert::upsert(
            &Upsert::new(
                &self.table,
                &self.constraint,
                &columns,
                &key,
                ConflictAction::Update {
                    sets: &sets,
                    audit: &[],
                },
            )
            .insert_exprs(&fills)
            .returning(&returning)
            .update_guard(Some(&guard))
            .insert_filter(Some(&filter)),
        ))
    }

    /// Result column index of `existed` in the rows of [`SyncApply::statement`].
    ///
    /// Only `erp_orm::delta::apply_synchronous` may use it.
    #[doc(hidden)]
    pub fn result_existed_column(&self) -> usize {
        RESULT_FIRST_MEASURE + self.shape.ident.measures.len() + 1
    }

    fn failure(&self, error: Error, batch: &NettedBatch, index: usize) -> SyncApplyFailure {
        SyncApplyFailure {
            error,
            refused: batch.keys.get(index).cloned(),
        }
    }

    /// Why row `index` of the batch was refused: the exact sum of the existing values (`None`:
    /// the key is absent) and the net is out of range, or violates the first guard it breaks.
    fn attribute(
        &self,
        batch: &NettedBatch,
        index: usize,
        current: Option<&[MeasureValue]>,
    ) -> SyncApplyFailure {
        let shape = &self.shape;
        let Some(delta) = batch.vectors.get(index) else {
            return self.failure(unattributed(shape.name()), batch, index);
        };
        let base = match current {
            Some(values) => match shape.admit_measures(values.to_vec()) {
                Ok(v) => v,
                Err(e) => return self.failure(e, batch, index),
            },
            None => shape.zero_vector(),
        };
        let proposed = match base.checked_add(shape, delta) {
            Ok(p) => p,
            Err(e) => return self.failure(e, batch, index),
        };
        for g in &self.guards {
            let ty = shape.ident.measures[g.index].ty;
            let compared = match proposed.values.get(g.index) {
                Some(value) => as_i128(ty, value).and_then(|sum| Ok(sum < as_i128(ty, &g.floor)?)),
                None => Err(unattributed(shape.name())),
            };
            let below = match compared {
                Ok(below) => below,
                Err(e) => return self.failure(e, batch, index),
            };
            if below {
                return self.failure(Error::validation(g.refusal), batch, index);
            }
        }
        self.failure(unattributed(shape.name()), batch, index)
    }

    /// Re-check a row the statement reported written (see [`SyncApply::interpret`]).
    fn check_written(
        &self,
        batch: &NettedBatch,
        index: usize,
        row: &SyncReturned,
    ) -> std::result::Result<(), SyncApplyFailure> {
        let shape = &self.shape;
        let Some(delta) = batch.vectors.get(index) else {
            return Err(self.failure(unattributed(shape.name()), batch, index));
        };
        if row.inserted && row.existed {
            return Err(self.failure(unattributed(shape.name()), batch, index));
        }
        let stored = shape
            .admit_measures(row.measures.clone())
            .map_err(|e| self.failure(e, batch, index))?;
        for g in &self.guards {
            let ty = shape.ident.measures[g.index].ty;
            let below = match stored.values.get(g.index) {
                Some(value) => as_i128(ty, value).and_then(|v| Ok(v < as_i128(ty, &g.floor)?)),
                None => Err(unattributed(shape.name())),
            }
            .map_err(|e| self.failure(e, batch, index))?;
            if below {
                return Err(self.failure(Error::validation(g.refusal), batch, index));
            }
        }
        if row.inserted && stored.values != delta.values {
            return Err(self.failure(unattributed(shape.name()), batch, index));
        }
        Ok(())
    }

    /// Interpret the rows the statement returned for `batch`. Every netted row must come back
    /// written; a row returned unwritten, or missing, is attributed against the exact sum
    /// (existing value plus net, or the net alone for a missing row): out of range is Validation
    /// `H27 NumericOutOfRange: {projection}: {measure}`, a floor violated is that guard's refusal
    /// text, and nothing explaining it is Internal `H27 ApplyUnattributed: {projection}`; so is
    /// a duplicate or out-of-range ordinal or a row with the wrong measure count. The failure
    /// carries the refused key as a value; no message holds a grain value.
    ///
    /// Every row that came back written is re-checked against what it now stores: the measures
    /// must be in range and meet every guard (a violation is attributed like a refusal, with the
    /// guard's text or `H27 NumericOutOfRange`), and an inserted row must hold exactly the net.
    /// An inserted row whose key already existed in the statement's snapshot is Internal
    /// `H27 ApplyUnattributed`: either the key constraint does not arbitrate the whole grain
    /// (not `NULLS NOT DISTINCT`, so a second row was created) or a concurrent transaction deleted
    /// the key after the snapshot, and in both cases the insert arm wrote an unguarded delta.
    /// Two concurrent first inserts of one key under a non-arbitrating constraint both see the
    /// key absent and are not detected here; the arbiter must be the shape's
    /// `grain_key_definition` (a catalog check is a slice D3 obligation).
    ///
    /// Only `erp_orm::delta::apply_synchronous` may use it.
    #[doc(hidden)]
    pub fn interpret(
        &self,
        batch: &NettedBatch,
        rows: &[SyncReturned],
    ) -> std::result::Result<SyncApplied, SyncApplyFailure> {
        self.check_batch(batch).map_err(SyncApplyFailure::from)?;
        let n = batch.keys.len();
        let wrong = || SyncApplyFailure::from(unattributed(self.shape.name()));
        let mut by_ordinal: Vec<Option<&SyncReturned>> = vec![None; n];
        for row in rows {
            if row.measures.len() != self.shape.ident.measures.len() {
                return Err(wrong());
            }
            let slot = usize::try_from(row.ordinal)
                .ok()
                .and_then(|o| o.checked_sub(1))
                .and_then(|i| by_ordinal.get_mut(i))
                .ok_or_else(wrong)?;
            if slot.replace(row).is_some() {
                return Err(wrong());
            }
        }
        let mut applied = Vec::with_capacity(n);
        for (index, slot) in by_ordinal.iter().enumerate() {
            match slot {
                Some(row) if row.written => {
                    self.check_written(batch, index, row)?;
                    applied.push(AppliedRow {
                        inserted: row.inserted,
                        measures: row.measures.clone(),
                    });
                }
                Some(row) => return Err(self.attribute(batch, index, Some(&row.measures))),
                None => return Err(self.attribute(batch, index, None)),
            }
        }
        Ok(SyncApplied { rows: applied })
    }
}

/// One row the apply statement returned, as typed values read by the executing layer.
///
/// Only `erp_orm::delta::apply_synchronous` may use it.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReturned {
    ordinal: i64,
    inserted: bool,
    written: bool,
    existed: bool,
    measures: Vec<MeasureValue>,
}

impl SyncReturned {
    /// A returned row: the 1-based canonical position of the netted row, whether the statement
    /// inserted it, whether it was inserted or updated, and the measures now stored (the current
    /// values for a row left unwritten), in declaration order. `existed`: whether the key was
    /// present in the statement's snapshot.
    pub fn new(
        ordinal: i64,
        inserted: bool,
        written: bool,
        existed: bool,
        measures: Vec<MeasureValue>,
    ) -> Self {
        Self {
            ordinal,
            inserted,
            written,
            existed,
            measures,
        }
    }
}

/// A row of a successful apply, in the order of the netted batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRow {
    inserted: bool,
    measures: Vec<MeasureValue>,
}

impl AppliedRow {
    /// Whether the apply inserted the row (its key was new).
    pub fn inserted(&self) -> bool {
        self.inserted
    }

    /// The measures now stored, in declaration order.
    pub fn measures(&self) -> &[MeasureValue] {
        &self.measures
    }
}

/// The outcome of a successful apply: one row per netted key, in canonical order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncApplied {
    rows: Vec<AppliedRow>,
}

impl SyncApplied {
    /// The applied rows, in the order of [`NettedBatch::keys`].
    pub fn rows(&self) -> &[AppliedRow] {
        &self.rows
    }
}

/// A refused or failed apply: the error (what [`From<SyncApplyFailure> for Error`] gives) and,
/// when one netted key caused it, that key as a typed value for the consumer's adapter (the
/// message never holds it).
pub struct SyncApplyFailure {
    error: Error,
    refused: Option<CanonicalKey>,
}

/// Prints `<redacted>` without quotes.
struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The refused key holds grain values, so a log line printing the failure never shows it.
impl std::fmt::Debug for SyncApplyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncApplyFailure")
            .field("error", &self.error)
            .field("refused", &self.refused.as_ref().map(|_| Redacted))
            .finish()
    }
}

impl SyncApplyFailure {
    /// The error.
    pub fn error(&self) -> &Error {
        &self.error
    }

    /// The refused key, when the refusal is about one.
    pub fn refused(&self) -> Option<&CanonicalKey> {
        self.refused.as_ref()
    }

    /// The error alone.
    pub fn into_error(self) -> Error {
        self.error
    }
}

impl From<Error> for SyncApplyFailure {
    fn from(error: Error) -> Self {
        Self {
            error,
            refused: None,
        }
    }
}

impl From<SyncApplyFailure> for Error {
    fn from(failure: SyncApplyFailure) -> Self {
        failure.error
    }
}

#[cfg(test)]
#[path = "test_sync.rs"]
mod tests;
