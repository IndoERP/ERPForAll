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

/// Refuse mutation families whose capture boundary remains deferred beyond S7.
pub(crate) fn refuse_uncaptured_mutation(spec: &ModelSpec, operation: &str) -> Result<()> {
    if spec.eventful {
        return Err(Error::programming(format!(
            "outbox: eventful {operation} capture for model '{}' is not implemented until S7-S9",
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
    if rows.iter().any(|row| {
        row.iter_commands().next().is_some()
            || row.iter().any(|(name, _)| {
                records
                    .spec()
                    .field(name)
                    .is_some_and(|field| field.ty.is_x2many())
            })
    }) {
        refuse_uncaptured_mutation(records.spec(), "relation")?;
    }
    Ok(())
}
