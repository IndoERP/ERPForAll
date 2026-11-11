//! Durable, transaction-owned reference registration.
//!
//! Consumers register before later inventory locks and use the returned identity for their
//! calculation, then persist the business reference in this same transaction. They must backfill
//! historical references before activating this boundary. Cancellation never clears protection.

use crate::conversion::Unit;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

/// Hard admission ceiling for input and affected graph identities, not a capacity measurement.
pub const MAX_GUARD_IDENTITIES: usize = 4096;

/// A resolved unit identity whose permanent-use marker belongs to the current transaction.
///
/// This is data, not a transferable authorization capability. Rolling the transaction back
/// invalidates any claim of committed registration, including when an enclosing savepoint fails.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReferencedUnit {
    unit: Unit,
}
impl ReferencedUnit {
    /// The requested persistent unit identity.
    pub fn id(&self) -> i64 {
        self.unit.id()
    }
    /// Its protected dimensional root.
    pub fn root(&self) -> i64 {
        self.unit.root()
    }
    /// Its exact protected conversion factor.
    pub fn factor(&self) -> Decimal {
        self.unit.factor()
    }
}

pub(crate) fn map_guard_error(error: Error) -> Error {
    let mapped = match error.sqlstate() {
        Some("UOM01") => {
            Error::user("A referenced unit of measure would change. Create a new unit instead.")
        }
        Some("UOM02") => Error::validation("Unit of measure hierarchy is invalid."),
        Some("UOM03") => Error::programming("Unit reference guard state is missing or corrupt."),
        Some("UOM04") => Error::validation("Unit reference guard limits were exceeded."),
        Some("UOM06") => Error::user("Reference unit of measure is missing."),
        Some("UOM07") => Error::validation("Unit reference factor is not representable exactly."),
        Some("UOM05") => {
            Error::access("Unit reference guard records cannot be modified through ordinary CRUD.")
        }
        // Unrelated database failures retain their original kind, SQLSTATE and source.
        _ => return error,
    };
    mapped
        .with_sqlstate(error.sqlstate().map(str::to_owned))
        .with_source(error)
}

pub(crate) fn validated_ids(ids: &[i64]) -> Result<Vec<i64>> {
    if ids.len() > MAX_GUARD_IDENTITIES || ids.iter().any(|id| *id <= 0) {
        return Err(Error::validation(
            "Unit reference guard input exceeds its limits or has invalid identities.",
        ));
    }
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// Register permanent use and return unique identities in ascending unit order.
///
/// # Errors
/// Invalid/bounded input, missing or unreadable units/ancestry, corrupt guard/graph state,
/// and database errors. Serialization/deadlock failures require whole-unit retry; no local
/// retry is performed. An empty slice is a zero-SQL no-op.
pub async fn register(env: &Env, tx: &mut Transaction, ids: &[i64]) -> Result<Vec<ReferencedUnit>> {
    let ids = validated_ids(ids)?;
    if !env.shares_lineage(&tx.env()) {
        return Err(Error::programming(
            "Unit reference environment and transaction belong to different ORM lineages.",
        ));
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    env.records("uom.uom")?
        .browse(ids.iter().copied())?
        .check_access(tx, Operation::Read)
        .await?;
    // Read identities for authorization only; the privileged registration re-resolves after its fence.
    let rows = tx.query("WITH RECURSIVE a(id) AS (SELECT unnest($1::bigint[]) UNION SELECT u.relative_uom_id::bigint FROM public.uom_uom u JOIN a ON a.id=u.id WHERE u.relative_uom_id IS NOT NULL) SELECT id FROM a LIMIT 4097", &[&ids]).await?;
    if rows.len() > MAX_GUARD_IDENTITIES {
        return Err(Error::validation(
            "Unit reference guard graph exceeds 4096 identities.",
        ));
    }
    let ancestors: Vec<i64> = rows
        .iter()
        .map(|row| {
            row.try_get(0)
                .map_err(|e| Error::db("Cannot decode unit ancestry.").with_source(e))
        })
        .collect::<Result<_>>()?;
    let records = env.records("uom.uom")?.browse(ancestors)?;
    records
        .check_access(tx, Operation::Read)
        .await
        .map_err(|e| {
            if e.kind() == erp_sdk::ErrorKind::Access {
                Error::access(
                    "You are not allowed to read all unit ancestry required by this operation.",
                )
                .with_source(e)
            } else {
                e
            }
        })?;
    for field in [
        "relative_uom_id",
        "relative_factor",
        "factor",
        "parent_path",
    ] {
        records.cells(tx, field).await?;
    }
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT uom_id,root_id,factor::text FROM public.erp_uom_register($1::bigint[])",
            &[&ids],
        )
        .await
        .map_err(map_guard_error)?;
    if rows.len() != ids.len() {
        return Err(Error::programming(
            "Unit reference guard returned an incomplete batch.",
        ));
    }
    rows.into_iter()
        .zip(ids)
        .map(|(row, expected)| {
            let id: i64 = row
                .try_get(0)
                .map_err(|e| Error::db("Cannot decode registered unit identity.").with_source(e))?;
            let root: i64 = row
                .try_get(1)
                .map_err(|e| Error::db("Cannot decode registered unit root.").with_source(e))?;
            let factor: String = row
                .try_get(2)
                .map_err(|e| Error::db("Cannot decode registered unit factor.").with_source(e))?;
            if id != expected {
                return Err(Error::programming(
                    "Unit reference guard returned an unexpected identity.",
                ));
            }
            let factor = Decimal::from_str_exact(&factor).map_err(|e| {
                Error::validation("Unit reference factor is not representable exactly.")
                    .with_source(e)
            })?;
            Ok(ReferencedUnit {
                unit: Unit::new(id, root, factor)?,
            })
        })
        .collect()
}
