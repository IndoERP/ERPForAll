// SPDX-License-Identifier: MIT OR Apache-2.0
//! Transaction-local destructive admission. No cached readiness value authorizes removal.
use super::runtime::{blob_error, col_error, runtime};
use erp_sdk::blob::{BlobError, Namespace, OwnerIdentity, OwnerMarker, OwnerVersion, StoreKind};
use erp_sdk::{Error, Result, Transaction};

fn refused(reason: &'static str) -> Error {
    blob_error(BlobError::CleanupRefused { reason })
}
fn column<T>(value: std::result::Result<T, impl std::fmt::Display>) -> Result<T> {
    value.map_err(|e| col_error("attachment cleanup", e))
}

/// Coordinator -> store metadata -> owner rows -> GC holds -> blob/upload rows.
/// All store ids are int2 and locked in order. This locks metadata, never scans content.
pub(super) async fn start(tx: &mut Transaction) -> Result<()> {
    let isolation = tx
        .query("SELECT current_setting('transaction_isolation')", &[])
        .await?;
    let isolation: String = column(isolation[0].try_get(0))?;
    if isolation != "read committed" {
        return Err(Error::programming(
            "The garbage collection sweep runs in a READ COMMITTED transaction (its job kinds do): a REPEATABLE READ snapshot predates the hold lock and could miss a hold.",
        ));
    }
    super::ownership::lock(tx).await?;
    tx.query("SELECT id FROM ir_blob_store ORDER BY id FOR SHARE", &[])
        .await?;
    tx.query(
        "SELECT store_id FROM ir_blob_owner_state ORDER BY store_id FOR UPDATE",
        &[],
    )
    .await?;
    Ok(())
}

/// Check each store of one batch once, under `start`'s locks. A store refused by the store layer
/// (ownership, observation, capability, contest or an unreachable store) is left out of the batch
/// and logged at error level with its reason, so one store never stops cleanup of the others;
/// database and configuration errors still fail the transaction. Returns (admitted, refused);
/// each refused store carries the fixed reason of its `BlobError::CleanupRefused`, or
/// `store_error` for any other store-layer failure (its text goes only to the log).
pub(super) async fn admit(
    tx: &mut Transaction,
    stores: impl IntoIterator<Item = i16>,
) -> Result<(Vec<i16>, Vec<(i16, &'static str)>)> {
    let (mut admitted, mut refused) = (Vec::new(), Vec::new());
    for store in stores {
        match check(tx, store).await {
            Ok(()) => admitted.push(store),
            Err(e) => {
                let reason = match std::error::Error::source(&e)
                    .and_then(|s| s.downcast_ref::<BlobError>())
                {
                    Some(BlobError::CleanupRefused { reason }) => *reason,
                    // a configuration error concerns every store: it fails the transaction
                    Some(BlobError::Configuration { .. }) | None => return Err(e),
                    Some(_) => "store_error",
                };
                tracing::error!(target: "erp::attachments", store, reason, error = %e, "attachment cleanup is not admitted for this store; its rows are kept for the next run");
                refused.push((store, reason));
            }
        }
    }
    Ok((admitted, refused))
}

/// Check under `start`'s locks immediately before each destructive store call. Database clock
/// and store state are read again on every invocation, including within the same transaction.
pub(super) async fn check(tx: &mut Transaction, store_id: i16) -> Result<()> {
    let rt = runtime(&tx.env())?;
    let rows = tx.query(
        "SELECT s.store_key,s.namespace::text,s.role,s.fingerprint,o.mode, \
         o.namespace::text,o.fingerprint,o.confirmed_marker,o.confirmed_etag,o.confirmed_version, \
         coalesce(o.last_success_at>clock_timestamp()-make_interval(secs=>$2::float8),false), \
         coalesce(o.observation_id IS NOT NULL AND o.observation_not_before<=clock_timestamp(),false), \
         o.probe_protocol,i.system_identifier::text,i.database_oid,i.incarnation::text, \
         p.system_identifier::text,(SELECT oid::int8 FROM pg_database WHERE datname=current_database()) \
         FROM ir_blob_store s LEFT JOIN ir_blob_owner_state o ON o.store_id=s.id \
         LEFT JOIN ir_blob_owner_identity i ON i.singleton CROSS JOIN pg_control_system() p \
         WHERE s.id=$1", &[&store_id, &(super::ownership::FRESH_SECS as f64)]).await?;
    let row = rows
        .first()
        .ok_or_else(|| refused("store registration is missing"))?;
    let key: String = column(row.try_get(0))?;
    let ns: String = column(row.try_get(1))?;
    let role: String = column(row.try_get(2))?;
    if role != "own" {
        return Err(refused("borrowed namespaces cannot be removed"));
    }
    let fingerprint: String = column(row.try_get(3))?;
    let mode: Option<String> = column(row.try_get(4))?;
    let fresh: bool = column(row.try_get(10))?;
    let probe: Option<i16> = column(row.try_get(12))?;
    if mode.as_deref() != Some("healthy") || !fresh || probe != Some(1) {
        return Err(refused("ownership is not admitted"));
    }
    let bound_ns: String = column(row.try_get(5))?;
    let bound_fp: String = column(row.try_get(6))?;
    let store = rt
        .store(&key)
        .ok_or_else(|| refused("store is not configured"))?;
    if bound_ns != ns
        || bound_fp != fingerprint
        || store.fingerprint().as_deref() != Some(fingerprint.as_str())
    {
        return Err(refused("store binding changed"));
    }
    store
        .verify_fingerprint(&fingerprint)
        .await
        .map_err(blob_error)?;
    let caps = store.capabilities();
    for (present, capability) in [
        (caps.create_only, "create-only put"),
        (caps.if_match_put, "If-Match put"),
    ] {
        if !present {
            return Err(blob_error(BlobError::OwnerCapabilityMissing { capability }));
        }
    }
    // Shared local stores are refused whatever the peer-probe evidence: `peer_probe::admit` is
    // not wired into cleanup yet, so a recorded probe admits nothing (status note).
    if store.kind() == StoreKind::Local && !rt.unshared_local_stores().contains(&key) {
        return Err(blob_error(BlobError::OwnerCapabilityMissing {
            capability: "shared-filesystem peer evidence (a later release)",
        }));
    }
    super::topology::verify(tx, &rt).await?;
    let bytes: Vec<u8> = column(row.try_get(7))?;
    let marker = OwnerMarker::decode(&bytes).map_err(blob_error)?;
    let system: String = column(row.try_get(13))?;
    let oid: i64 = column(row.try_get(14))?;
    let incarnation: String = column(row.try_get(15))?;
    let actual_system: String = column(row.try_get(16))?;
    let actual_oid: i64 = column(row.try_get(17))?;
    if system != actual_system || oid != actual_oid {
        return Err(refused("database identity changed"));
    }
    let identity = OwnerIdentity::new(
        system
            .parse()
            .map_err(|_| Error::db("Invalid cleanup system identifier"))?,
        u32::try_from(oid).map_err(|_| Error::db("Invalid cleanup database OID"))?,
        Namespace::parse(&incarnation).map_err(blob_error)?,
    )
    .map_err(blob_error)?;
    if marker.identity() != &identity {
        return Err(refused("database identity changed"));
    }
    let ready: bool = column(row.try_get(11))?;
    if !ready {
        return Err(refused("observation is incomplete"));
    }
    let namespace = Namespace::parse(&ns).map_err(blob_error)?;
    let etag: String = column(row.try_get(8))?;
    let version: Option<String> = column(row.try_get(9))?;
    let expected = OwnerVersion::new(&etag, version.as_deref()).map_err(blob_error)?;
    if store
        .contested(&namespace)
        .await
        .map_err(blob_error)?
        .is_some()
    {
        return Err(refused("namespace is contested"));
    }
    store
        .check_owner(&namespace, &expected)
        .await
        .map_err(blob_error)?;
    if store
        .contested(&namespace)
        .await
        .map_err(blob_error)?
        .is_some()
    {
        return Err(refused("namespace is contested"));
    }
    Ok(())
}
