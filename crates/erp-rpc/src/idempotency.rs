//! Idempotency keys (design note §10 and lead rulings 2 and 4).
//!
//! The key row is claimed by the first statement of the business transaction and the response is
//! stored by an `UPDATE` pipelined with `COMMIT`, so the effect and its record commit or roll back
//! together. A failed attempt stores nothing.

use std::fmt;

use erp_fields::{Error, Result};
use sha2::{Digest, Sha256};
use tokio_postgres::types::Type;

use crate::error::{ProtocolError, RpcFailure};
use crate::json::{self, Json};

/// The table's DDL (design note §10.2), installed by [`erp_orm::Orm::sync_schema`] through
/// [`erp_orm::CORE_DDL`]. Unpartitioned, `int8` identity id, `uuid` key, no
/// foreign keys (lead ruling 2).
pub const IDEMPOTENCY_DDL: &str = "\
CREATE TABLE IF NOT EXISTS ir_idempotency_key (
    id                int8        GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    key               uuid        NOT NULL,
    principal_kind    text        NOT NULL CHECK (principal_kind IN ('user', 'agent', 'service')),
    subject_uid       int8        NOT NULL,
    api_key_id        int8,
    model             text        NOT NULL,
    method            text        NOT NULL,
    request_digest    bytea       NOT NULL CHECK (octet_length(request_digest) = 32),
    status            smallint,
    response          bytea,
    binding_digest    bytea       NOT NULL CHECK (octet_length(binding_digest) = 32),
    policy_generation int8        NOT NULL,
    resolution_epoch  int8        NOT NULL,
    correlation_id    uuid,
    created_at        timestamptz NOT NULL DEFAULT now(),
    expires_at        timestamptz NOT NULL,
    CONSTRAINT ir_idempotency_key_scope
        UNIQUE NULLS NOT DISTINCT (principal_kind, subject_uid, api_key_id, key)
);
CREATE INDEX IF NOT EXISTS ir_idempotency_key_expires_at ON ir_idempotency_key (expires_at);
CREATE INDEX IF NOT EXISTS ir_idempotency_key_stored ON ir_idempotency_key (created_at)
    WHERE response IS NOT NULL";

/// Rows the claim deletes (expired) and responses it drops (past the replay window), at most
/// each, so storage stays bounded without a job (until) and a claim's work stays small.
pub const MAINTENANCE_BATCH: i64 = 16;

erp_orm::core_ddl!(
    IDEMPOTENCY_TABLE,
    owner = "erp-rpc",
    name = "ir_idempotency_key",
    purpose = "idempotency keys",
    tables = ["ir_idempotency_key"],
    sql = IDEMPOTENCY_DDL
);

/// The claim (design note §10.3): insert the key row, or take over an expired one, or return the
/// existing row. `expired` tells whether the stored result is past the replay window. In the same
/// statement it deletes up to [`MAINTENANCE_BATCH`] other expired rows and drops up to as many
/// responses past the replay window (`SKIP LOCKED`: a claim never waits on another's
/// maintenance; under `REPEATABLE READ` a row another claim removed after this snapshot raises
/// `40001`, which the whole-call retry handles).
pub(crate) const CLAIM_SQL: &str = "\
WITH purged AS (
    DELETE FROM ir_idempotency_key
     WHERE id IN (SELECT id FROM ir_idempotency_key
                   WHERE expires_at <= now()
                     AND NOT (principal_kind = $2 AND subject_uid = $3
                              AND api_key_id IS NOT DISTINCT FROM $4 AND key = $1::uuid)
                   ORDER BY expires_at LIMIT $14 FOR UPDATE SKIP LOCKED)
), trimmed AS (
    UPDATE ir_idempotency_key SET response = NULL
     WHERE id IN (SELECT id FROM ir_idempotency_key
                   WHERE response IS NOT NULL
                     AND created_at <= now() - $13 * interval '1 second'
                     AND expires_at > now()
                     AND NOT (principal_kind = $2 AND subject_uid = $3
                              AND api_key_id IS NOT DISTINCT FROM $4 AND key = $1::uuid)
                   ORDER BY created_at LIMIT $14 FOR UPDATE SKIP LOCKED)
), claimed AS (
    INSERT INTO ir_idempotency_key AS k
           (key, principal_kind, subject_uid, api_key_id, model, method, request_digest,
            binding_digest, policy_generation, resolution_epoch, correlation_id, expires_at)
    VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::uuid,
            now() + $12 * interval '1 second')
    ON CONFLICT ON CONSTRAINT ir_idempotency_key_scope DO UPDATE
       SET model = EXCLUDED.model, method = EXCLUDED.method,
           request_digest = EXCLUDED.request_digest, status = NULL, response = NULL,
           binding_digest = EXCLUDED.binding_digest,
           policy_generation = EXCLUDED.policy_generation,
           resolution_epoch = EXCLUDED.resolution_epoch,
           correlation_id = EXCLUDED.correlation_id, created_at = now(),
           expires_at = EXCLUDED.expires_at
     WHERE k.expires_at <= now()
    RETURNING k.id
)
SELECT id, true AS claimed, NULL::bytea AS digest, NULL::smallint AS status,
       NULL::bytea AS response, NULL::bytea AS binding_digest, NULL::int8 AS policy_generation,
       NULL::int8 AS resolution_epoch, false AS expired
  FROM claimed
UNION ALL
SELECT k.id, false, k.request_digest, k.status, k.response, k.binding_digest,
       k.policy_generation, k.resolution_epoch,
       k.created_at + $13 * interval '1 second' <= now()
  FROM ir_idempotency_key k
 WHERE NOT EXISTS (SELECT 1 FROM claimed)
   AND k.principal_kind = $2 AND k.subject_uid = $3
   AND k.api_key_id IS NOT DISTINCT FROM $4 AND k.key = $1::uuid";

/// Parameter types of [`CLAIM_SQL`].
pub(crate) const CLAIM_TYPES: [Type; 14] = [
    Type::TEXT,
    Type::TEXT,
    Type::INT8,
    Type::INT8,
    Type::TEXT,
    Type::TEXT,
    Type::BYTEA,
    Type::BYTEA,
    Type::INT8,
    Type::INT8,
    Type::TEXT,
    Type::FLOAT8,
    Type::FLOAT8,
    Type::INT8,
];

/// The store, pipelined with `COMMIT`.
pub(crate) const STORE_SQL: &str =
    "UPDATE ir_idempotency_key SET status = $2, response = $3 WHERE id = $1";

/// Parameter types of [`STORE_SQL`].
pub(crate) const STORE_TYPES: [Type; 3] = [Type::INT8, Type::INT2, Type::BYTEA];

/// An `Idempotency-Key` header value: an RFC 9562 UUID in text form (any version; v7 is
/// recommended for index locality), kept in canonical lower case.
///
/// ```
/// use erp_rpc::IdempotencyKey;
/// let key = IdempotencyKey::parse("018F3A1C-7B2D-7C4E-9A10-3F2B1C0D9E8A").unwrap();
/// assert_eq!(key.to_string(), "018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a");
/// assert_eq!(
///     IdempotencyKey::parse("not-a-uuid").unwrap_err().to_string(),
///     "werkzeug.exceptions.BadRequest (400): Idempotency-Key must be a UUID (RFC 9562)"
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(Box<str>);

impl IdempotencyKey {
    /// Parse a header value (the value is never echoed in the error).
    ///
    /// # Errors
    ///
    /// 400 `werkzeug.exceptions.BadRequest` "Idempotency-Key must be a UUID (RFC 9562)".
    pub fn parse(value: &str) -> std::result::Result<Self, RpcFailure> {
        parse_uuid(value).map(Self).ok_or_else(|| {
            RpcFailure::Protocol(ProtocolError::BadRequest(
                "Idempotency-Key must be a UUID (RFC 9562)".to_owned(),
            ))
        })
    }

    /// The key from every `Idempotency-Key` header of a request: none → `None`, one → parsed,
    /// more → 400.
    ///
    /// # Errors
    ///
    /// 400 `werkzeug.exceptions.BadRequest` "Idempotency-Key must be sent at most once" for
    /// two headers or more; [`IdempotencyKey::parse`]'s error for a malformed value.
    pub fn from_headers(values: &[&str]) -> std::result::Result<Option<Self>, RpcFailure> {
        match values {
            [] => Ok(None),
            [one] => Self::parse(one).map(Some),
            _ => Err(RpcFailure::Protocol(ProtocolError::BadRequest(
                "Idempotency-Key must be sent at most once".to_owned(),
            ))),
        }
    }

    /// The canonical text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// Kept at the original path for RPC source compatibility; ORM owns propagation.
pub use erp_orm::CorrelationId;

/// `8-4-4-4-12` hex digits, case-insensitive, returned in lower case.
fn parse_uuid(value: &str) -> Option<Box<str>> {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        let dash = matches!(i, 8 | 13 | 18 | 23);
        if dash != (*b == b'-') || (!dash && !b.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(value.to_ascii_lowercase().into())
}

/// SHA-256 of the RFC 8785 canonical JSON of
/// `{"model", "method", "ids", "context", "kwargs"}` as sent (after parsing, before binding
/// defaults): key order and whitespace do not matter, values do.
pub(crate) fn request_digest(
    model: &str,
    method: &str,
    ids: &Json,
    context: &Json,
    kwargs: &Json,
) -> [u8; 32] {
    let doc = Json::Object(vec![
        ("model".into(), Json::Str(model.into())),
        ("method".into(), Json::Str(method.into())),
        ("ids".into(), ids.clone()),
        ("context".into(), context.clone()),
        ("kwargs".into(), kwargs.clone()),
    ]);
    Sha256::digest(json::canonical(&doc)).into()
}

/// SHA-256 of a principal's binding bytes.
pub(crate) fn binding_digest(binding: &[u8]) -> [u8; 32] {
    Sha256::digest(binding).into()
}

/// What the claim found.
#[derive(Debug)]
pub(crate) enum Claim {
    /// This transaction owns the (new or taken-over) row.
    Claimed { id: i64 },
    /// A committed row exists.
    Existing(Existing),
}

/// A committed row returned by the claim.
#[derive(Debug)]
pub(crate) struct Existing {
    pub(crate) id: i64,
    pub(crate) digest: Vec<u8>,
    pub(crate) status: Option<i16>,
    pub(crate) response: Option<Vec<u8>>,
    pub(crate) binding_digest: Vec<u8>,
    pub(crate) policy_generation: i64,
    pub(crate) resolution_epoch: i64,
    pub(crate) expired: bool,
}

/// Decode the claim's row.
pub(crate) fn decode_claim(rows: &[tokio_postgres::Row]) -> Result<Claim> {
    let [row] = rows else {
        return Err(Error::programming(format!(
            "the idempotency claim returned {} rows instead of one",
            rows.len()
        )));
    };
    let get = |e: tokio_postgres::Error| {
        Error::programming(format!(
            "the idempotency claim row could not be decoded: {e}"
        ))
    };
    let id: i64 = row.try_get(0).map_err(get)?;
    let claimed: bool = row.try_get(1).map_err(get)?;
    if claimed {
        return Ok(Claim::Claimed { id });
    }
    let required =
        |v: Option<Vec<u8>>, what: &str| {
            v.ok_or_else(|| Error::programming({
            tracing::error!(target: "erp::rpc", row = id, "idempotency key row has no {what}");
            format!("an idempotency key row has no {what}")
        }))
        };
    let required_i64 =
        |v: Option<i64>, what: &str| {
            v.ok_or_else(|| Error::programming({
            tracing::error!(target: "erp::rpc", row = id, "idempotency key row has no {what}");
            format!("an idempotency key row has no {what}")
        }))
        };
    Ok(Claim::Existing(Existing {
        id,
        digest: required(row.try_get(2).map_err(get)?, "request digest")?,
        status: row.try_get(3).map_err(get)?,
        response: row.try_get(4).map_err(get)?,
        binding_digest: required(row.try_get(5).map_err(get)?, "binding digest")?,
        policy_generation: required_i64(row.try_get(6).map_err(get)?, "policy generation")?,
        resolution_epoch: required_i64(row.try_get(7).map_err(get)?, "resolution epoch")?,
        expired: row
            .try_get::<_, Option<bool>>(8)
            .map_err(get)?
            .ok_or_else(|| {
                {
                tracing::error!(target: "erp::rpc", row = id, "idempotency key row has no expiry flag");
                Error::programming("an idempotency key row has no expiry flag")
            }
            })?,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn keys_are_uuids_in_canonical_form() {
        for good in [
            "00000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
            "018F3A1C-7B2D-7C4E-9A10-3F2B1C0D9E8A",
        ] {
            assert_eq!(
                IdempotencyKey::parse(good).unwrap().as_str(),
                good.to_ascii_lowercase()
            );
        }
        for bad in [
            "",
            "018f3a1c7b2d7c4e9a103f2b1c0d9e8a",
            "{018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a}",
            "018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8",
            "018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8az",
            "018f3a1c_7b2d-7c4e-9a10-3f2b1c0d9e8a",
            "018f3a1g-7b2d-7c4e-9a10-3f2b1c0d9e8a",
            " 018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8",
        ] {
            let err = IdempotencyKey::parse(bad).unwrap_err();
            assert_eq!(err.status(), 400, "{bad:?}");
            assert_eq!(err.message(), "Idempotency-Key must be a UUID (RFC 9562)");
        }
        assert_eq!(IdempotencyKey::from_headers(&[]).unwrap(), None);
        let two = IdempotencyKey::from_headers(&[
            "018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a",
            "018f3a1c-7b2d-7c4e-9a10-3f2b1c0d9e8a",
        ])
        .unwrap_err();
        assert_eq!(two.message(), "Idempotency-Key must be sent at most once");
        assert_eq!(
            CorrelationId::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef).as_str(),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
    }

    #[test]
    fn legacy_uuid_parsers_remain_case_insensitive_after_orm_reexport() {
        let sent = "01234567-89AB-CDEF-0123-456789ABCDEF";
        let expected = "01234567-89ab-cdef-0123-456789abcdef";
        let correlation: erp_orm::CorrelationId = crate::CorrelationId::parse(sent).unwrap();
        assert_eq!(correlation.as_str(), expected);
        assert_eq!(IdempotencyKey::parse(sent).unwrap().as_str(), expected);
        assert_eq!(
            IdempotencyKey::from_headers(&[sent])
                .unwrap()
                .unwrap()
                .as_str(),
            expected
        );
        // The native scalar's canonical policy does not alter these established APIs.
        let error = erp_fields::Uuid::parse(sent).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "invalid UUID: expected canonical lowercase 8-4-4-4-12 hexadecimal text"
        );
    }

    #[test]
    fn the_request_digest_ignores_key_order_but_not_values() {
        let parse = |s: &str| crate::json::parse(s.as_bytes(), 64).unwrap();
        let a = request_digest(
            "m",
            "f",
            &parse("[1,2]"),
            &parse(r#"{"lang":"fr_FR","tz":"UTC"}"#),
            &parse(r#"{"vals":{"a":1,"b":[true,null]}}"#),
        );
        let b = request_digest(
            "m",
            "f",
            &parse(" [ 1 , 2 ] "),
            &parse(r#"{"tz":"UTC","lang":"fr_FR"}"#),
            &parse(r#"{"vals":{"b":[true,null],"a":1}}"#),
        );
        assert_eq!(a, b);
        let c = request_digest(
            "m",
            "f",
            &parse("[2,1]"),
            &parse(r#"{"lang":"fr_FR","tz":"UTC"}"#),
            &parse(r#"{"vals":{"a":1,"b":[true,null]}}"#),
        );
        assert_ne!(a, c, "ids order matters");
        let d = request_digest(
            "m",
            "g",
            &parse("[1,2]"),
            &parse(r#"{"lang":"fr_FR","tz":"UTC"}"#),
            &parse(r#"{"vals":{"a":1,"b":[true,null]}}"#),
        );
        assert_ne!(a, d, "the method matters");
    }
}
