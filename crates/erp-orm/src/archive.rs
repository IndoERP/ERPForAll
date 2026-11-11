//! `action_archive` and `action_unarchive`.
//!
//! Archive and unarchive recordsets by writing to the `active` field.

use std::any::Any;

use erp_fields::{Error, Result, Value};

use crate::Transaction;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::records::{Model, RecordSet, Records};

impl Records {
    /// Archive currently active records by writing their active field to false.
    ///
    /// Returns a programming error if the model has no active field; read/write access and
    /// validation errors propagate. Already inactive records are not written.
    ///
    /// ```no_run
    /// # async fn example(records: erp_orm::Records, mut tx: erp_orm::Transaction) -> erp_fields::Result<()> {
    /// records.action_archive(&mut tx).await?;
    /// # Ok(()) }
    /// ```
    pub async fn action_archive(&self, tx: &mut Transaction) -> Result<()> {
        call_method::<(), ()>(tx, self, "action_archive", ()).await
    }

    /// Unarchive currently inactive records by writing their active field to true.
    ///
    /// Errors follow [`Records::action_archive`]; already active records are not written.
    ///
    /// ```no_run
    /// # async fn example(records: erp_orm::Records, mut tx: erp_orm::Transaction) -> erp_fields::Result<()> {
    /// records.action_unarchive(&mut tx).await?;
    /// # Ok(()) }
    /// ```
    pub async fn action_unarchive(&self, tx: &mut Transaction) -> Result<()> {
        call_method::<(), ()>(tx, self, "action_unarchive", ()).await
    }
}

impl<M: Model> RecordSet<M> {
    /// Archive active records in this recordset (see [`Records::action_archive`]).
    pub async fn action_archive(&self, tx: &mut Transaction) -> Result<()> {
        self.as_records().action_archive(tx).await
    }

    /// Unarchive inactive records in this recordset (see [`Records::action_unarchive`]).
    pub async fn action_unarchive(&self, tx: &mut Transaction) -> Result<()> {
        self.as_records().action_unarchive(tx).await
    }
}

async fn base_toggle_active(
    tx: &mut Transaction,
    records: &Records,
    target_active: bool,
) -> Result<()> {
    let spec = records.spec();
    let Some(active_field_id) = spec.active_name else {
        return Err(Error::programming(format!(
            "No 'active' field on model {}",
            records.model_name()
        )));
    };
    if records.is_empty() {
        return Ok(());
    }
    let active_name = spec.field_by_id(active_field_id).name;
    let predicate = |v: &Value| {
        if target_active {
            !v.is_truthy()
        } else {
            v.is_truthy()
        }
    };
    let recs_to_update = records.filtered_values(tx, active_name, predicate).await?;
    if !recs_to_update.is_empty() {
        let mut vals = erp_fields::ValueMap::default();
        vals.insert(active_name.to_owned(), Value::Bool(target_active));
        recs_to_update.write_values(tx, vals.iter()).await?;
    }
    Ok(())
}

/// The `base` implementation of `action_archive`.
async fn base_action_archive(tx: &mut Transaction, records: &Records) -> Result<()> {
    base_toggle_active(tx, records, false).await
}

/// The `base` implementation of `action_unarchive`.
async fn base_action_unarchive(tx: &mut Transaction, records: &Records) -> Result<()> {
    base_toggle_active(tx, records, true).await
}

type ArchiveFn = MethodFn<(), ()>;

fn base_action_archive_impl<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    _args: (),
    _sup: Super<(), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move { base_action_archive(tx, records).await })
}

fn action_archive_entry() -> &'static (dyn Any + Send + Sync) {
    static F: ArchiveFn = base_action_archive_impl;
    &F
}

static BASE_ACTION_ARCHIVE: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "action_archive",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<ArchiveFn>,
    entry: action_archive_entry,
    file: file!(),
    line: line!(),
};

fn base_action_unarchive_impl<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    _args: (),
    _sup: Super<(), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move { base_action_unarchive(tx, records).await })
}

fn action_unarchive_entry() -> &'static (dyn Any + Send + Sync) {
    static F: ArchiveFn = base_action_unarchive_impl;
    &F
}

static BASE_ACTION_UNARCHIVE: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "action_unarchive",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<ArchiveFn>,
    entry: action_unarchive_entry,
    file: file!(),
    line: line!(),
};

/// The `base` implementations of `action_archive` and `action_unarchive`, registered by
/// [`crate::Dispatch::build`].
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 2] = [&BASE_ACTION_ARCHIVE, &BASE_ACTION_UNARCHIVE];
