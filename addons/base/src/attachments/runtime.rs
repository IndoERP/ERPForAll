//! The configured stores as the transaction sees them: the composition product, the database's
//! store rows (`ir_blob_store`: configured id, namespace, role, fingerprint) and the mapping of
//! store errors to ORM errors.

use std::sync::Arc;

use erp_sdk::blob::{AttachmentRuntime, AttachmentStore, BlobError, Namespace};
use erp_sdk::prelude::*;
use erp_sdk::{Env, ErrorKind};

/// What a caller sees when no composition root attached the store configuration. A user error
/// (HTTP 422), not a programming error: the user reads which setting the operator must set.
pub(crate) const NOT_CONFIGURED: &str = "Attachments are not configured: this server has no attachment \
store, so files cannot be stored. An administrator must set attachments.write_store and a store in \
the [attachments] table of the server configuration";

/// A retryable whole-call failure: a reference raced a removal or a
/// concurrent link. The ORM's retry loop repeats the call.
#[derive(Debug)]
struct Retry(&'static str);

impl std::fmt::Display for Retry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Retry {}

/// The retryable error of a link that raced a garbage collection (SQLSTATE `40001`).
pub(crate) fn retryable(reason: &'static str) -> Error {
    Error::database(
        ErrorKind::Concurrency,
        Some("40001".to_owned()),
        format!("Attachment content changed while it was linked: {reason}; retrying"),
        Retry(reason),
    )
}

/// Map a foreign-key failure of a `blob_id` write to the retryable error (a blob row removed
/// between the lock and the write), pass every other error through.
pub(crate) fn map_link_fk(error: Error) -> Error {
    match error.sqlstate() {
        Some("23503" | "23001") => retryable("a referenced blob row was removed"),
        _ => error,
    }
}

/// The configured stores of this ORM generation.
pub(crate) fn runtime(env: &Env) -> Result<Arc<AttachmentRuntime>> {
    env.generation_attachment::<AttachmentRuntime>()
        .map_err(|_| Error::user(NOT_CONFIGURED))
}

// A failed cleanup diagnostic must not reclassify the original receive failure.
fn receive_kind(error: &BlobError) -> ErrorKind {
    match error {
        BlobError::StagingAbort { receive, .. } => receive_kind(receive),
        BlobError::LimitExceeded { .. }
        | BlobError::InvalidRange { .. }
        | BlobError::UploadExpired => ErrorKind::Validation,
        BlobError::QuotaExceeded { .. } => ErrorKind::ResourceExhausted,
        BlobError::Configuration { .. } => ErrorKind::Programming,
        BlobError::Busy { .. } => ErrorKind::Lock,
        // All remaining storage/integrity/ownership failures already map to Internal below.
        _ => ErrorKind::Internal,
    }
}

/// Map a store error to the ORM error a caller sees.
pub(crate) fn blob_error(error: BlobError) -> Error {
    match &error {
        BlobError::StagingAbort { receive, .. } => {
            Error::new(receive_kind(receive), error.to_string()).with_source(error)
        }
        BlobError::LimitExceeded { .. }
        | BlobError::InvalidRange { .. }
        | BlobError::UploadExpired
        | BlobError::UploadIdleTimeout
        | BlobError::UploadReceiveTimeout => {
            Error::validation(error.to_string()).with_source(error)
        }
        BlobError::QuotaExceeded { .. } | BlobError::PendingUploads { .. } => {
            Error::new(ErrorKind::ResourceExhausted, error.to_string()).with_source(error)
        }
        BlobError::AccountingUnavailable => Error::internal(error.to_string()).with_source(error),
        BlobError::Configuration { .. } => Error::programming(error.to_string()).with_source(error),
        // a full node answers busy at once (409, retry); it never queues
        BlobError::Busy { .. } => {
            Error::new(erp_sdk::ErrorKind::Lock, error.to_string()).with_source(error)
        }
        BlobError::NotFound { .. } => Error::internal(
            "AttachmentContentMissing: the stored content is missing from the store",
        )
        .with_source(error),
        _ => Error::internal(format!("The attachment store failed: {error}")).with_source(error),
    }
}

/// A store row of this database together with the opened store.
#[derive(Clone)]
pub(crate) struct StoreRef {
    pub id: i16,
    pub key: String,
    pub namespace: Namespace,
    pub store: Arc<dyn AttachmentStore>,
}

#[derive(Default)]
struct StoreCache {
    rows: Vec<StoreRef>,
}

fn row_error(error: impl std::fmt::Display) -> Error {
    Error::db(format!("ir_blob_store row could not be read: {error}"))
}

/// The own-namespace row of store `key`, created on first use, checked against the store's
/// fingerprint.
pub(crate) async fn store_by_key(
    tx: &mut Transaction,
    rt: &AttachmentRuntime,
    key: &str,
) -> Result<StoreRef> {
    if let Some(found) = tx
        .extension::<StoreCache>()
        .rows
        .iter()
        .find(|r| r.key == key)
    {
        return Ok(found.clone());
    }
    let store = rt
        .store(key)
        .cloned()
        .ok_or_else(|| Error::programming(format!("attachment store {key} is not configured")))?;
    let fingerprint = store.fingerprint().unwrap_or_default();
    let select = "SELECT id, namespace::text, fingerprint FROM ir_blob_store \
                  WHERE store_key = $1 AND role = 'own'";
    let mut rows = tx.query(select, &[&key]).await?;
    if rows.is_empty() {
        tx.execute(
            "INSERT INTO ir_blob_store (store_key, fingerprint) VALUES ($1, $2) \
             ON CONFLICT DO NOTHING",
            &[&key, &fingerprint],
        )
        .await?;
        rows = tx.query(select, &[&key]).await?;
    }
    let row = rows
        .first()
        .ok_or_else(|| Error::db("ir_blob_store row missing after its insert"))?;
    let id: i16 = row.try_get(0).map_err(row_error)?;
    let namespace: String = row.try_get(1).map_err(row_error)?;
    let registered: String = row.try_get(2).map_err(row_error)?;
    rt.verify_fingerprint(key, &registered)
        .await
        .map_err(blob_error)?;
    let found = StoreRef {
        id,
        key: key.to_owned(),
        namespace: Namespace::parse(&namespace).map_err(blob_error)?,
        store,
    };
    tx.extension::<StoreCache>().rows.push(found.clone());
    Ok(found)
}

/// The store a blob row points at (`ir_blob.store_id`), by its row id.
pub(crate) async fn store_by_id(
    tx: &mut Transaction,
    rt: &AttachmentRuntime,
    id: i16,
) -> Result<StoreRef> {
    if let Some(found) = tx
        .extension::<StoreCache>()
        .rows
        .iter()
        .find(|r| r.id == id)
    {
        return Ok(found.clone());
    }
    let rows = tx
        .query(
            "SELECT store_key, namespace::text, role, fingerprint FROM ir_blob_store WHERE id = $1",
            &[&id],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::db(format!("ir_blob_store row {id} is missing")))?;
    let key: String = row.try_get(0).map_err(row_error)?;
    let namespace: String = row.try_get(1).map_err(row_error)?;
    let role: String = row.try_get(2).map_err(row_error)?;
    let registered: String = row.try_get(3).map_err(row_error)?;
    if role != "own" {
        return Err(blob_error(BlobError::ForeignNamespace { namespace }));
    }
    let store = rt.store(&key).cloned().ok_or_else(|| {
        Error::programming(format!(
            "the database uses attachment store {key}, which is not configured"
        ))
    })?;
    rt.verify_fingerprint(&key, &registered)
        .await
        .map_err(blob_error)?;
    let found = StoreRef {
        id,
        key,
        namespace: Namespace::parse(&namespace).map_err(blob_error)?,
        store,
    };
    tx.extension::<StoreCache>().rows.push(found.clone());
    Ok(found)
}

/// A failed column read of a service statement.
pub(crate) fn col_error(what: &str, error: impl std::fmt::Display) -> Error {
    Error::db(format!("{what}: a column could not be read: {error}"))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod cleanup_errors {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn multipart_abort_diagnostic_keeps_the_original_limit_classification() {
        let error = blob_error(BlobError::StagingAbort {
            receive: Box::new(BlobError::LimitExceeded {
                kind: erp_sdk::blob::LimitKind::ObjectBytes,
                limit: 7,
            }),
            abort: Box::new(BlobError::Unavailable {
                retryable: true,
                message: "abort fault".into(),
                source: None,
            }),
        });
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "object size in bytes exceeds the limit of 7; aborting the incomplete staging upload also failed: the attachment store is unavailable (retryable): abort fault"
        );
        assert!(matches!(
            error.source().unwrap().downcast_ref::<BlobError>().unwrap(),
            BlobError::StagingAbort { .. }
        ));
    }
}
