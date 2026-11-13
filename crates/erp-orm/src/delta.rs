// SPDX-License-Identifier: MIT OR Apache-2.0
//! Transactional deltas, ORM layer (slice D2, contract D1, D3, D5; case
//! ): the executor of the synchronous guarded apply of [`erp_sql::delta`].
//!
//! The declaration types of the SQL layer ([`ProjectionShape`], [`SyncApply`], [`NettedBatch`] and
//! the rest) are re-exported here, so an addon needs one path. [`apply_synchronous`] runs one
//! statement inside the caller's [`Transaction`] and poisons it on every failure, so the source
//! writes of the same transaction and the sibling keys the statement already wrote can never commit.
//!
//! Scope: the statement writes the consumer's table directly. It does not check model or
//! record-rule access, update the ORM cache, run computes, constraints or write hooks, and does not
//! move the mutation counter; it is for tables whose writes the consumer's reviewed adapter owns
//! (H27 projection tables, or a table whose access proof and cache invalidation that adapter states).
//! A key committed by a concurrent transaction after the statement's snapshot is treated as absent
//! by the insert arm, so a negative first delta is refused (conservative); the update arm still
//! guards a conflicting row. A key deleted concurrently, or a key constraint that does not
//! arbitrate the whole grain, can make the insert arm write a row without its guard; the
//! interpretation re-checks every written row and refuses an inserted row whose key existed in
//! the snapshot, so those cases poison the transaction. It cannot detect two concurrent first
//! inserts of one key under a constraint that is not `NULLS NOT DISTINCT` over the grain (both
//! see the key absent and both insert): the consumer must declare the key with
//! `ProjectionShape::grain_key_definition` (contract D1). A catalog check of the arbiter is a
//! slice D3 obligation (declaration resolution through SQLOBJ). A concurrent delete surfaces as
//! Internal `H27 ApplyUnattributed`; the whole unit may be retried safely, nothing committed.
//!
//! Only `REPEATABLE READ` transactions are accepted: at `READ COMMITTED` the statement could
//! attribute a refusal against values another transaction has since changed, so
//! [`apply_synchronous`] refuses it before sending anything.

pub use erp_sql::delta::*;

use erp_fields::{Date, Decimal, Error, Result, TimestampTz, Uuid};
use tokio_postgres::types::{ToSql, Type};

use crate::{Isolation, Transaction};

type Bound = Box<dyn ToSql + Sync>;

fn wrong_value(what: &str) -> Error {
    Error::internal(format!(
        "H27 apply: a {what} value does not match its column"
    ))
}

/// The Julian day number of 1970-01-01, the Unix epoch.
const UNIX_EPOCH_JULIAN_DAY: i32 = 2_440_588;

fn grain_column(batch: &NettedBatch, index: usize, ty: GrainType) -> Result<(Bound, Type)> {
    fn collect<T: ToSql + Sync + 'static>(
        batch: &NettedBatch,
        index: usize,
        convert: impl Fn(&GrainValue) -> Result<Option<T>>,
    ) -> Result<Bound> {
        let mut out: Vec<Option<T>> = Vec::with_capacity(batch.len());
        for key in batch.keys() {
            let value = key
                .values()
                .get(index)
                .ok_or_else(|| wrong_value("missing grain"))?;
            out.push(convert(value)?);
        }
        Ok(Box::new(out))
    }
    Ok(match ty {
        GrainType::Int8 => (
            collect(batch, index, |v| match v {
                GrainValue::Null => Ok(None),
                GrainValue::Int8(i) => Ok(Some(*i)),
                _ => Err(wrong_value("int8")),
            })?,
            Type::INT8_ARRAY,
        ),
        GrainType::Bool => (
            collect(batch, index, |v| match v {
                GrainValue::Null => Ok(None),
                GrainValue::Bool(b) => Ok(Some(*b)),
                _ => Err(wrong_value("boolean")),
            })?,
            Type::BOOL_ARRAY,
        ),
        GrainType::Date => (
            collect(batch, index, |v| match v {
                GrainValue::Null => Ok(None),
                GrainValue::Date(d) => Ok(Some(Date::from_days(
                    d.to_julian_day() - UNIX_EPOCH_JULIAN_DAY,
                ))),
                _ => Err(wrong_value("date")),
            })?,
            Type::DATE_ARRAY,
        ),
        GrainType::TimestampTz => (
            collect(batch, index, |v| match v {
                GrainValue::Null => Ok(None),
                GrainValue::TimestampTz(t) => {
                    let micros = i64::try_from(t.unix_timestamp_nanos() / 1_000)
                        .map_err(|_| wrong_value("timestamptz"))?;
                    Ok(Some(TimestampTz::from_unix_micros(micros)?))
                }
                _ => Err(wrong_value("timestamptz")),
            })?,
            Type::TIMESTAMPTZ_ARRAY,
        ),
        GrainType::Uuid => (
            collect(batch, index, |v| match v {
                GrainValue::Null => Ok(None),
                GrainValue::Uuid(u) => Ok(Some(Uuid::from_bytes(*u.as_bytes()))),
                _ => Err(wrong_value("uuid")),
            })?,
            Type::UUID_ARRAY,
        ),
        GrainType::Text { .. } => (
            collect(batch, index, |v| match v {
                GrainValue::Null => Ok(None),
                GrainValue::Text(t) => Ok(Some(t.clone())),
                _ => Err(wrong_value("text")),
            })?,
            Type::TEXT_ARRAY,
        ),
        _ => return Err(Error::internal("H27 apply: unsupported grain type")),
    })
}

fn measure_columns(apply: &SyncApply, batch: &NettedBatch) -> Result<Vec<(Bound, Type)>> {
    let shape = apply.shape();
    let mut rows = Vec::with_capacity(batch.len());
    for vector in batch.vectors() {
        rows.push(shape.measure_values(vector)?);
    }
    let mut out = Vec::with_capacity(shape.measures().len());
    for (i, m) in shape.measures().iter().enumerate() {
        out.push(match m.ty() {
            MeasureType::Int8 => {
                let mut values: Vec<i64> = Vec::with_capacity(rows.len());
                for row in &rows {
                    match row.get(i) {
                        Some(MeasureValue::Int8(v)) => values.push(*v),
                        _ => return Err(wrong_value("int8 measure")),
                    }
                }
                (Box::new(values) as Bound, Type::INT8_ARRAY)
            }
            MeasureType::Numeric { .. } => {
                let mut values: Vec<Decimal> = Vec::with_capacity(rows.len());
                for row in &rows {
                    match row.get(i) {
                        Some(MeasureValue::Numeric(v)) => values.push(*v),
                        _ => return Err(wrong_value("numeric measure")),
                    }
                }
                (Box::new(values) as Bound, Type::NUMERIC_ARRAY)
            }
            _ => return Err(Error::internal("H27 apply: unsupported measure type")),
        });
    }
    Ok(out)
}

/// The column index and type only: the driver's text is not carried into a message.
fn unreadable<T>(column: usize, _: &tokio_postgres::Error) -> Error {
    Error::internal(format!(
        "H27 apply: result column {column} is unreadable as {}",
        std::any::type_name::<T>()
    ))
}

async fn run(
    tx: &mut Transaction,
    apply: &SyncApply,
    batch: &NettedBatch,
) -> std::result::Result<SyncApplied, SyncApplyFailure> {
    apply.check_batch(batch)?;
    if batch.is_empty() {
        return apply.interpret(batch, &[]);
    }
    let sql = apply.statement()?;
    let shape = apply.shape();
    let mut bound: Vec<(Bound, Type)> = Vec::new();
    for (i, g) in shape.grain().iter().enumerate() {
        bound.push(grain_column(batch, i, g.ty())?);
    }
    bound.extend(measure_columns(apply, batch)?);
    for value in apply.insert_fill_values() {
        bound.push((Box::new(value) as Bound, Type::TIMESTAMP));
    }
    let params: Vec<(&(dyn ToSql + Sync), Type)> = bound
        .iter()
        .map(|(value, ty)| (value.as_ref(), ty.clone()))
        .collect();
    let rows = tx.query_typed(&sql, &params).await?;
    let mut returned = Vec::with_capacity(rows.len());
    for row in &rows {
        let ordinal: i64 = row.try_get(0).map_err(|e| unreadable::<i64>(0, &e))?;
        let inserted: bool = row.try_get(2).map_err(|e| unreadable::<bool>(2, &e))?;
        let written: bool = row.try_get(3).map_err(|e| unreadable::<bool>(3, &e))?;
        let existed_column = apply.result_existed_column();
        let existed: bool = row
            .try_get(existed_column)
            .map_err(|e| unreadable::<bool>(existed_column, &e))?;
        let mut measures = Vec::with_capacity(shape.measures().len());
        for (i, m) in shape.measures().iter().enumerate() {
            let column = RESULT_FIRST_MEASURE + i;
            measures.push(match m.ty() {
                MeasureType::Int8 => MeasureValue::Int8(
                    row.try_get(column)
                        .map_err(|e| unreadable::<i64>(column, &e))?,
                ),
                MeasureType::Numeric { .. } => MeasureValue::Numeric(
                    row.try_get::<_, Decimal>(column)
                        .map_err(|e| unreadable::<Decimal>(column, &e))?,
                ),
                _ => return Err(Error::internal("H27 apply: unsupported measure type").into()),
            });
        }
        returned.push(SyncReturned::new(
            ordinal, inserted, written, existed, measures,
        ));
    }
    apply.interpret(batch, &returned)
}

/// Apply a netted batch to the consumer's table in one statement, inside `tx`.
///
/// An empty batch sends nothing. Otherwise the grain and measure arrays of the batch, in canonical
/// order, are bound to [`SyncApply::statement`] and the rows it returns are interpreted
/// ([`SyncApply::interpret`]): on success one [`AppliedRow`] per key; on a guard refusal the
/// consumer's refusal text (Validation), `H27 NumericOutOfRange` for an exact sum out of range, or
/// the database error with its kind and SQLSTATE unchanged. **Every** failure poisons `tx`
/// (commit then refuses with the same error), because the statement may already have written
/// sibling keys and the caller's source writes would otherwise commit without their projection.
/// The refused key, when one key caused the refusal, is in [`SyncApplyFailure::refused`].
///
/// # Errors
///
/// As described above; a batch netted against another shape is Programming. A transaction not at
/// `REPEATABLE READ` is Programming `H27 Unsupported: synchronous apply at READ COMMITTED; requires,
/// before anything is sent: nothing was written, so that refusal does not poison.
pub async fn apply_synchronous(
    tx: &mut Transaction,
    apply: &SyncApply,
    batch: &NettedBatch,
) -> std::result::Result<SyncApplied, SyncApplyFailure> {
    if tx.isolation_level() != Isolation::RepeatableRead {
        return Err(Error::programming(
            "H27 Unsupported: synchronous apply at READ COMMITTED; requires a later release",
        )
        .into());
    }
    let outcome = run(tx, apply, batch).await;
    if let Err(failure) = &outcome {
        tx.poison_with(failure.error());
    }
    outcome
}
