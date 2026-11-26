//! The generic ORM never touches a model of the closed event namespace (A2-D5,
//! plan S2).
//!
//! The outbox and audit tables live in their own schema. Their rows are written by the capture
//! path only, so every generic operation (create, write, unlink, upsert, copy, read, search,
//! grouping, export, import, RPC) is refused before it sends a statement. The guard runs where a
//! transaction accepts a recordset ([`crate::Transaction::check_records`]), which every record
//! entry point passes first; the model-level access check is not used because superuser mode skips
//! it on the read side.

use erp_fields::{Error, Result};
use erp_registry::{ModelSpec, TableNamespace};

/// Refuse `op` on `spec` when the model is in the closed event namespace.
///
/// # Errors
///
/// `Programming` naming the model, the refused operation and the slice whose capture path is the
/// only writer.
pub(crate) fn refuse_event_namespace(spec: &ModelSpec, op: &str) -> Result<()> {
    if spec.namespace == TableNamespace::Event {
        return Err(Error::programming(format!(
            "model '{}' is in the closed event namespace: the generic ORM refuses {op}; the \
             a later release capture path is the only writer and nothing reads it through the ORM",
            spec.name
        )));
    }
    Ok(())
}

/// Check before method overrides or delegated-parent work can change the database.
pub(crate) fn check_create_capture(
    records: &crate::Records,
    rows: &[crate::RawVals],
) -> Result<()> {
    if rows.is_empty() || !records.spec().eventful {
        return Ok(());
    }
    if records.env().operation_identity().is_none() {
        return Err(Error::programming(
            "outbox: eventful mutation requires an OperationIdentity",
        ));
    }
    Ok(())
}
