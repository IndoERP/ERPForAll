// SPDX-License-Identifier: MIT OR Apache-2.0
//! Snapshot-safe, transactional admission for attachments awaiting a parent record.
use super::{NewAttachment, service, types::MAX_BATCH};
use erp_sdk::prelude::*;

/// The pending batch would exceed the caller's placeholder limit. Carried as the source of a
/// Validation error, so a consumer can provide its own recovery message without matching text.
#[derive(Debug)]
#[non_exhaustive]
pub struct PendingLimitExceeded;
impl std::fmt::Display for PendingLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pending attachment limit exceeded")
    }
}
impl std::error::Error for PendingLimitExceeded {}

/// Create a batch awaiting a record, bounded by `limit` for this subject and placeholder.
///
/// Callers check permission for their intended operation first. Items must have no parent,
/// field, or placeholder; this operation assigns `placeholder`. A generation write serializes
/// admission across nodes and rejects stale repeatable-read snapshots with SQLSTATE 40001.
/// The batch and guard update share a savepoint and the caller's transaction.
///
/// # Errors
/// Validation for invalid shape/bounds or a full limit (source [`PendingLimitExceeded`]); the
/// attachment service's errors; Concurrency for a stale snapshot, retried by the RPC service.
pub async fn create_pending(
    tx: &mut Transaction,
    placeholder: &str,
    limit: usize,
    items: Vec<NewAttachment>,
) -> Result<Vec<RecId>> {
    if placeholder.is_empty()
        || placeholder.len() > 128
        || !(1..=MAX_BATCH).contains(&limit)
        || items.len() > MAX_BATCH
    {
        return Err(Error::validation(
            "Pending attachments need a placeholder of 1 to 128 bytes and a limit and batch of at most 1000.",
        ));
    }
    if items
        .iter()
        .any(|i| i.parent.is_some() || i.res_field.is_some() || i.pending_model.is_some())
    {
        return Err(Error::validation(
            "A pending attachment batch supplies no parent, field or placeholder on its items.",
        ));
    }
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let subject = tx.env().principal().subject().get();
    tx.savepoint(async |tx| {
        tx.execute("INSERT INTO ir_attachment_pending_guard(subject_user,placeholder_model) VALUES ($1,$2) ON CONFLICT(subject_user,placeholder_model) DO UPDATE SET generation=NOT ir_attachment_pending_guard.generation", &[&subject,&placeholder]).await?;
        tx.flush().await?;
        let rows = tx.query("SELECT count(*)::int8 FROM ir_attachment WHERE create_uid=$1 AND res_model=$2 AND coalesce(res_id,0)=0", &[&subject,&placeholder]).await?;
        let held: i64 = rows.first().ok_or_else(|| Error::db("Pending attachment count returned no row."))?.try_get(0).map_err(|e| Error::db("Pending attachment count is unreadable.").with_source(e))?;
        let held = usize::try_from(held).map_err(|_| Error::db("Pending attachment count is outside the supported range."))?;
        if held.checked_add(items.len()).is_none_or(|n| n>limit) {
            return Err(Error::validation(format!("At most {limit} pending attachments are allowed for {placeholder}.")).with_source(PendingLimitExceeded));
        }
        service::create(tx, items.into_iter().map(|i| i.pending_in(placeholder)).collect()).await
    }).await
}
