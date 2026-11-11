//! The `orm-host` imports, with the guest-side checks of their answers.
//!
//! Every function runs as the user and company of the current call; the host takes them from its
//! own store, never from an argument. Each call is set-based: one call per batch.
//! Outside a component (native builds and tests) the imports are not linked and must not be called.

use crate::bindings::erpforall::addon::orm_host;
use crate::error::{Result, error};
use crate::{BoundaryErrorKind, CallContext, ColumnStore, RecordId, ScalarValue, Writes};

pub use orm_host::LogLevel;

/// Ids of the records of `model` matching `domain_json`, with the access rules applied.
pub fn search(
    model: &str,
    domain_json: &str,
    limit: Option<u32>,
    offset: Option<u32>,
    order: Option<&str>,
) -> Result<Vec<RecordId>> {
    orm_host::search(model, domain_json, limit, offset, order)
}

/// `fields` of the records `ids` of `model`, held as a checked [`ColumnStore`] under the call's
/// `cache-epoch`.
///
/// The host must answer for the model and the ids asked for, in that order, with one column per
/// requested field; any other answer is refused with `internal-error` rather than read.
pub fn read(
    ctx: &CallContext,
    model: &str,
    ids: &[RecordId],
    fields: &[String],
) -> Result<ColumnStore> {
    let batch = orm_host::read_columns(model, ids, fields)?;
    if batch.model != model || batch.ids != ids {
        return Err(error(
            BoundaryErrorKind::InternalError,
            format!(
                "read-columns of model '{model}' for {} ids answered for model '{}' with {} ids",
                ids.len(),
                batch.model,
                batch.ids.len()
            ),
        ));
    }
    let store = ColumnStore::new(batch, ctx.cache_epoch)?;
    for field in fields {
        store.column(field).map_err(|_| {
            error(
                BoundaryErrorKind::InternalError,
                format!("read-columns of model '{model}' did not answer field '{field}'"),
            )
        })?;
    }
    Ok(store)
}

/// Create one record of `model` per entry of `values`; the new ids, in the same order.
pub fn create(model: &str, values: &[Vec<(String, ScalarValue)>]) -> Result<Vec<RecordId>> {
    let ids = orm_host::create_batch(model, values)?;
    if ids.len() != values.len() {
        return Err(error(
            BoundaryErrorKind::InternalError,
            format!(
                "create-batch of model '{model}' for {} records answered {} ids",
                values.len(),
                ids.len()
            ),
        ));
    }
    Ok(ids)
}

/// Write the same `values` to every record of `ids`.
pub fn write(model: &str, ids: &[RecordId], values: &[(String, ScalarValue)]) -> Result<()> {
    orm_host::write_batch(model, ids, values)
}

/// Send the calls of a [`WriteBuffer`](crate::WriteBuffer): its batch writes, then its column
/// writes. The first failure stops the sending and is returned; the host rolls the call back.
pub fn flush(writes: &Writes) -> Result<()> {
    for batch in &writes.batches {
        orm_host::write_batch(&writes.model, &batch.ids, &batch.values)?;
    }
    for columns in &writes.columns {
        orm_host::write_columns(&writes.model, &columns.ids, &columns.columns)?;
    }
    Ok(())
}

/// Delete the records `ids` of `model`.
pub fn unlink(model: &str, ids: &[RecordId]) -> Result<()> {
    orm_host::unlink_batch(model, ids)
}

/// Call `method` of `model` through its whole override chain. Arguments and result are JSON text.
pub fn call_method(model: &str, method: &str, ids: &[RecordId], args_json: &str) -> Result<String> {
    orm_host::call_method(model, method, ids, args_json)
}

pub fn super_call(model: &str, method: &str, ids: &[RecordId], args_json: &str) -> Result<String> {
    orm_host::super_call(model, method, ids, args_json)
}

/// Send one event to the host's log.
pub fn log(level: LogLevel, target: &str, message: &str) {
    orm_host::log(level, target, message);
}
