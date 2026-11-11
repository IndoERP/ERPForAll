//! Decoding grouped rows (GRP §4.6): each column is read with the type PostgreSQL reports for it
//! (aggregates change types: `SUM(int4)` is `int8`, `AVG(int4)` is `numeric`, `date_part` is
//! `float8`), then post-processed column by column.

use std::sync::Arc;

use erp_fields::{Date, DateTime, Decimal, Error, ErrorKind, RecId, Result};
use erp_registry::ModelId;
use postgres_types::Type;
use rustc_hash::FxHashSet;

use super::build::{AggOut, GroupOut};
use super::value::GroupValue;
use crate::env::Env;
use crate::records::{Ids, Records};
use crate::transaction::raw_row::{CellError, CellSource};

fn decode_error(e: CellError) -> Error {
    Error::new(ErrorKind::Database, "could not decode a grouped value").with_source(e)
}

fn get<'a, T: postgres_types::FromSql<'a>>(
    row: &'a (impl CellSource + ?Sized),
    idx: usize,
) -> Result<Option<T>> {
    row.cell::<Option<T>>(idx).map_err(decode_error)
}

fn list<'a, T: postgres_types::FromSql<'a>>(
    row: &'a (impl CellSource + ?Sized),
    idx: usize,
    wrap: impl Fn(T) -> GroupValue,
) -> Result<GroupValue> {
    Ok(match get::<Vec<Option<T>>>(row, idx)? {
        None => GroupValue::Null,
        Some(items) => GroupValue::List(
            items
                .into_iter()
                .map(|v| v.map_or(GroupValue::Null, &wrap))
                .collect(),
        ),
    })
}

/// The value of column `idx` as PostgreSQL typed it; SQL NULL is [`GroupValue::Null`].
pub(crate) fn raw<R: CellSource + ?Sized>(row: &R, idx: usize) -> Result<GroupValue> {
    let ty = row
        .column_type(idx)
        .cloned()
        .ok_or_else(|| Error::programming(format!("grouped row has no column {idx}")))?;
    let null = GroupValue::Null;
    Ok(match ty {
        Type::BOOL => get::<bool>(row, idx)?.map_or(null, GroupValue::Bool),
        Type::INT2 => get::<i16>(row, idx)?.map_or(null, |v| GroupValue::Int(i64::from(v))),
        Type::INT4 => get::<i32>(row, idx)?.map_or(null, |v| GroupValue::Int(i64::from(v))),
        Type::INT8 => get::<i64>(row, idx)?.map_or(null, GroupValue::Int),
        Type::FLOAT4 => get::<f32>(row, idx)?.map_or(null, |v| GroupValue::Float(f64::from(v))),
        Type::FLOAT8 => get::<f64>(row, idx)?.map_or(null, GroupValue::Float),
        Type::NUMERIC => get::<Decimal>(row, idx)?.map_or(null, GroupValue::Decimal),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
            get::<&str>(row, idx)?.map_or(null, GroupValue::str)
        }
        Type::DATE => get::<Date>(row, idx)?.map_or(null, GroupValue::Date),
        Type::TIMESTAMP => get::<DateTime>(row, idx)?.map_or(null, GroupValue::DateTime),
        Type::JSONB | Type::JSON => get::<erp_fields::serde_json::Value>(row, idx)?
            .map_or(null, |j| GroupValue::Json(Arc::new(j))),
        Type::BOOL_ARRAY => list::<bool>(row, idx, GroupValue::Bool)?,
        Type::INT2_ARRAY => list::<i16>(row, idx, |v| GroupValue::Int(i64::from(v)))?,
        Type::INT4_ARRAY => list::<i32>(row, idx, |v| GroupValue::Int(i64::from(v)))?,
        Type::INT8_ARRAY => list::<i64>(row, idx, GroupValue::Int)?,
        Type::FLOAT4_ARRAY => list::<f32>(row, idx, |v| GroupValue::Float(f64::from(v)))?,
        Type::FLOAT8_ARRAY => list::<f64>(row, idx, GroupValue::Float)?,
        Type::NUMERIC_ARRAY => list::<Decimal>(row, idx, GroupValue::Decimal)?,
        Type::TEXT_ARRAY | Type::VARCHAR_ARRAY | Type::BPCHAR_ARRAY => {
            list::<&str>(row, idx, GroupValue::str)?
        }
        Type::DATE_ARRAY => list::<Date>(row, idx, GroupValue::Date)?,
        Type::TIMESTAMP_ARRAY => list::<DateTime>(row, idx, GroupValue::DateTime)?,
        Type::JSONB_ARRAY => {
            list::<erp_fields::serde_json::Value>(row, idx, |j| GroupValue::Json(Arc::new(j)))?
        }
        other => {
            return Err(Error::programming(format!(
                "grouped column {idx} has the unsupported type {other}"
            )));
        }
    })
}

fn id_of(value: &GroupValue) -> Option<RecId> {
    match value {
        GroupValue::Int(i) => RecId::from_real(*i),
        _ => None,
    }
}

fn dedup(ids: impl IntoIterator<Item = RecId>) -> Vec<RecId> {
    let mut seen = FxHashSet::default();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

pub(crate) fn group_column(
    env: &Env,
    out: GroupOut,
    raw: Vec<GroupValue>,
    what: &str,
) -> Result<Vec<GroupValue>> {
    Ok(match out {
        GroupOut::Scalar => raw
            .into_iter()
            .map(|v| match v {
                GroupValue::Null => GroupValue::False,
                other => other,
            })
            .collect(),
        GroupOut::DatePart => raw
            .into_iter()
            .map(|v| match v {
                GroupValue::Null => GroupValue::False,
                // `date_part` is `float8`: whole numbers are returned as integers (GRP §7 D2)
                GroupValue::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => {
                    GroupValue::Int(f as i64)
                }
                other => other,
            })
            .collect(),
        GroupOut::Translated => raw
            .into_iter()
            .map(|v| match v {
                GroupValue::Json(arc) => {
                    let cell = erp_fields::Cell::Json(arc);
                    Ok(
                        match crate::translated::project("read_group", what, &cell, env.lang())? {
                            erp_fields::Cell::Str(s) => GroupValue::str(&s),
                            _ => GroupValue::False,
                        },
                    )
                }
                GroupValue::Null => Ok(GroupValue::False),
                other => Ok(other),
            })
            .collect::<Result<Vec<_>>>()?,
        GroupOut::Records(model) => {
            let prefetch: Ids = dedup(raw.iter().filter_map(id_of)).into();
            raw.iter()
                .map(|v| records(env, model, id_of(v).into_iter().collect(), &prefetch))
                .collect()
        }
    })
}

fn records(env: &Env, model: ModelId, ids: Vec<RecId>, prefetch: &Ids) -> GroupValue {
    GroupValue::Records(Records::from_parts(
        env.clone(),
        model,
        ids.into(),
        Arc::clone(prefetch),
    ))
}

pub(crate) fn aggregate_column(env: &Env, out: AggOut, raw: Vec<GroupValue>) -> Vec<GroupValue> {
    match out {
        AggOut::Count => raw
            .into_iter()
            .map(|v| match v {
                GroupValue::Null => GroupValue::Int(0),
                other => other,
            })
            .collect(),
        AggOut::Scalar => raw
            .into_iter()
            .map(|v| match v {
                GroupValue::Null => GroupValue::False,
                other => other,
            })
            .collect(),
        AggOut::Array => raw
            .into_iter()
            .map(|v| match v {
                GroupValue::Null => GroupValue::List(Vec::new()),
                other => other,
            })
            .collect(),
        AggOut::Records(model) => {
            let items = |v: &GroupValue| -> Vec<RecId> {
                match v {
                    GroupValue::List(items) => items.iter().filter_map(id_of).collect(),
                    _ => Vec::new(),
                }
            };
            let prefetch: Ids = dedup(raw.iter().flat_map(items)).into();
            raw.iter()
                .map(|v| records(env, model, dedup(items(v)), &prefetch))
                .collect()
        }
    }
}

/// The empty value of an aggregate column.
pub(crate) fn empty_aggregate(env: &Env, out: AggOut) -> GroupValue {
    match out {
        AggOut::Count => GroupValue::Int(0),
        AggOut::Scalar => GroupValue::False,
        AggOut::Array => GroupValue::List(Vec::new()),
        AggOut::Records(model) => GroupValue::Records(Records::empty(env.clone(), model)),
    }
}
