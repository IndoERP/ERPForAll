// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit schema-1 transport: SQL types are mapped independently of JSON token spelling.
use super::{CanonicalValue as Value, RowDigest, SealerProblem, json, problem::codec};
use erp_fields::{Error, Result};
use serde_json::value::RawValue;

// UUIDs/bytea use hex transport, xid8 and exact UTC microseconds use decimal strings.
// row_to_json is transport only; its text is never hashed directly.
pub(super) const PROJECTION: &str = "a.id,pg_catalog.encode(pg_catalog.uuid_send(a.entry_id),'hex') AS entry_id,
 CASE WHEN pg_catalog.isfinite(a.create_date) THEN pg_catalog.trunc(EXTRACT(EPOCH FROM a.create_date)*1000000)::text END AS create_date,
 a.xid::text AS xid,a.schema_version,a.entry_kind,a.origin,a.outcome,a.error_class,a.attempt,
 pg_catalog.encode(pg_catalog.uuid_send(a.operation_id),'hex') AS operation_id,
 pg_catalog.encode(pg_catalog.uuid_send(a.correlation_id),'hex') AS correlation_id,
 pg_catalog.encode(pg_catalog.uuid_send(a.causation_id),'hex') AS causation_id,
 pg_catalog.encode(pg_catalog.uuid_send(a.original_operation_id),'hex') AS original_operation_id,
 a.principal_kind,a.subject_id,a.actor_kind,a.actor_id,a.delegating_user_id,a.company_ids,
 a.registry_generation,a.policy_generation,a.model,a.method,a.effect,a.target_ids,
 pg_catalog.encode(a.args_digest,'hex') AS args_digest,a.redaction_version,a.events,a.encoded_bytes,
 (pg_catalog.cardinality(a.company_ids)=0 OR (pg_catalog.array_ndims(a.company_ids)=1 AND pg_catalog.array_lower(a.company_ids,1)=1)) AS _company_shape";

pub(super) fn digest(payload: &str) -> Result<RowDigest> {
    let json::Members(members) = json::members(payload)?;
    if members.len() != 31 {
        return Err(SealerProblem::RowValue.into());
    }
    let mut columns = Vec::with_capacity(30);
    let mut shape = false;
    for (name, raw) in &members {
        if name == "_company_shape" {
            if raw.get() != "true" {
                return Err(SealerProblem::RowValue.into());
            }
            shape = true;
        } else {
            columns.push((name.as_str(), value(name, raw)?));
        }
    }
    if !shape {
        return Err(SealerProblem::RowValue.into());
    }
    RowDigest::schema1(&columns).map_err(codec)
}
fn value(name: &str, raw: &RawValue) -> Result<Value> {
    if name == "schema_version" && raw.get() != "1" {
        return Err(SealerProblem::RowSchema.into());
    }
    if raw.get() == "null" {
        let nullable = super::super::AUDIT_TABLE
            .columns
            .iter()
            .find(|c| c.name == name)
            .is_some_and(|c| !c.not_null);
        return if nullable {
            Ok(Value::null())
        } else {
            Err(SealerProblem::RowValue.into())
        };
    }
    match name {
        "xid" => Ok(Value::xid(
            json::string(raw)?
                .parse::<u64>()
                .map_err(|_| Error::from(SealerProblem::RowValue))?,
        )),
        "create_date" => Ok(Value::timestamp_micros(
            json::string(raw)?
                .parse()
                .map_err(|_| Error::from(SealerProblem::RowValue))?,
        )),
        "id"
        | "schema_version"
        | "attempt"
        | "subject_id"
        | "actor_id"
        | "delegating_user_id"
        | "registry_generation"
        | "policy_generation"
        | "redaction_version"
        | "encoded_bytes" => Ok(Value::integer(json::integer(raw)?)),
        "entry_id"
        | "operation_id"
        | "correlation_id"
        | "causation_id"
        | "original_operation_id" => {
            let bytes = hex(&json::string(raw)?)?;
            Ok(Value::uuid(
                bytes
                    .try_into()
                    .map_err(|_| Error::from(SealerProblem::RowValue))?,
            ))
        }
        "args_digest" => Value::bytes(&hex(&json::string(raw)?)?).map_err(codec),
        "company_ids" => {
            let values = json::array(raw)?
                .into_iter()
                .map(|r| json::integer(r).map(Value::integer))
                .collect::<Result<Vec<_>>>()?;
            Value::array(&values).map_err(codec)
        }
        "target_ids" | "events" => json::canonical(raw),
        "entry_kind" | "origin" | "outcome" | "error_class" | "principal_kind" | "actor_kind"
        | "model" | "method" | "effect" => Value::text(&json::string(raw)?).map_err(codec),
        _ => Err(SealerProblem::RowValue.into()),
    }
}
fn hex(text: &str) -> Result<Vec<u8>> {
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(SealerProblem::RowValue.into());
    }
    pairs
        .iter()
        .map(|pair| {
            let nibble = |b: u8| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => Err(Error::from(SealerProblem::RowValue)),
            };
            Ok((nibble(pair[0])? << 4) | nibble(pair[1])?)
        })
        .collect()
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
    #[test]
    fn mapping_has_no_unhandled_schema_column() {
        let names = super::super::super::AUDIT_TABLE.columns;
        assert_eq!(names.len(), 30);
        let n: &RawValue = serde_json::from_str("1").unwrap();
        // Required integer columns exercise exact integer mapping; other kinds have explicit
        // transport projections and are covered by the real-row integration test.
        for name in [
            "id",
            "schema_version",
            "attempt",
            "subject_id",
            "actor_id",
            "delegating_user_id",
            "registry_generation",
            "policy_generation",
            "redaction_version",
            "encoded_bytes",
        ] {
            assert!(names.iter().any(|c| c.name == name));
            assert_eq!(value(name, n).unwrap(), Value::integer(1));
        }
    }
    #[test]
    fn exact_unsigned_xid_and_negative_timestamp() {
        let xid: &RawValue = serde_json::from_str(r#""18446744073709551615""#).unwrap();
        assert_eq!(value("xid", xid).unwrap(), Value::xid(u64::MAX));
        let ts: &RawValue = serde_json::from_str(r#""-1""#).unwrap();
        assert_eq!(
            value("create_date", ts).unwrap(),
            Value::timestamp_micros(-1)
        );
    }
}
