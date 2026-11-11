//! The deployment shape check: attachment bytes have no
//! node-local authority. A node-local directory is valid for one node; a second live node makes
//! it a misconfiguration, because that node would not see the bytes the first one wrote.
//!
//! How it is detected, in two independent ways:
//! - **the node registry** (`erp_node`): more than one live node while a configured
//!   `local` store is not declared `shared_filesystem = true` fails this check with an error that
//!   names the store and the two ways out;
//! - **the store fingerprint**: a node that mounts another directory than the one the database
//!   registered (`ir_blob_store.fingerprint` against the directory's `.store-id`) refuses to read
//!   or write at all (`Unavailable`, not retryable), so even a deployment that never set the flag
//!   cannot silently split its files across two disks.
//!
//! The check runs when an upload starts and when the attachment schedules are ensured.

use erp_sdk::blob::AttachmentRuntime;
use erp_sdk::prelude::*;

/// Refuse a multi-node deployment whose attachment store is an unshared local directory.
///
/// # Errors
///
/// `Programming` (a configuration error, with the operator's remedy in the message) when more
/// than one node is live and a `local` store is not declared shared.
pub(crate) async fn verify(tx: &mut Transaction, rt: &AttachmentRuntime) -> Result<()> {
    let unshared = rt.unshared_local_stores();
    if unshared.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT count(*) FROM erp_node WHERE state <> 'stopped' AND expires_at > now()",
            &[],
        )
        .await?;
    let live: i64 = rows
        .first()
        .ok_or_else(|| Error::db("the live node count is missing"))?
        .try_get(0)
        .map_err(|e| Error::db(format!("the live node count could not be read: {e}")))?;
    if live > 1 {
        return Err(Error::programming(format!(
            "The attachment store {} is a node-local directory but {live} nodes are live in this \
             database. Every node must reach the same bytes: set \
             attachments.stores.<id>.shared_filesystem = true on a path every node mounts, or \
             use an s3 store.",
            unshared.join(", ")
        )));
    }
    Ok(())
}

/// The same check as a public call, for a composition root that wants it at startup, before the
/// node takes traffic.
///
/// # Errors
///
/// As the private check; `Programming` when no attachment runtime is attached.
pub async fn verify_topology(tx: &mut Transaction) -> Result<()> {
    let env = tx.env();
    let rt = super::runtime::runtime(&env)?;
    verify(tx, &rt).await
}
