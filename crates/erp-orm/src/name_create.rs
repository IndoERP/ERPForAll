//! `name_create` (CRUD §4.3).
//!
//! Create a new record by providing its display name. Requires `_rec_name`.

use std::any::Any;
use std::sync::Arc;

use erp_fields::{Cell, Error, Result, Value};

use crate::Transaction;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::records::{Model, RecordSet, Records};

impl Records {
    /// Create a record using the model's name field and return its id and display name.
    ///
    /// Returns a programming error when the model has no name field, or propagates create,
    /// access and display-name errors. Addons may override this method.
    ///
    /// ```no_run
    /// # async fn example(model: erp_orm::Records, mut tx: erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let (_id, _display_name) = model.name_create(&mut tx, "New record").await?;
    /// # Ok(()) }
    /// ```
    pub async fn name_create(&self, tx: &mut Transaction, name: &str) -> Result<(i64, Arc<str>)> {
        call_method::<(String,), (i64, Arc<str>)>(tx, self, "name_create", (name.to_owned(),)).await
    }
}

impl<M: Model> RecordSet<M> {
    /// Create a new record by providing its display name (see [`Records::name_create`]).
    pub async fn name_create(&self, tx: &mut Transaction, name: &str) -> Result<(i64, Arc<str>)> {
        self.as_records().name_create(tx, name).await
    }
}

/// The `base` implementation of `name_create`.
async fn base_name_create(
    tx: &mut Transaction,
    records: &Records,
    name: &str,
) -> Result<(i64, Arc<str>)> {
    let spec = records.spec();
    let Some(rec_name_id) = spec.rec_name else {
        return Err(Error::programming(format!(
            "Cannot name_create on {}",
            records.model_name()
        )));
    };
    let rec_name = spec.field_by_id(rec_name_id).name;
    let mut vals = erp_fields::ValueMap::default();
    vals.insert(rec_name.to_owned(), Value::str(name));
    let raw = records.vals_from_values(vals.iter())?;
    let created = records.create(tx, vec![raw]).await?;
    let id = match created.ids().as_slice() {
        [id] => *id,
        _ => {
            return Err(Error::programming(
                "name_create did not create exactly one record",
            ));
        }
    };
    let display_cells = tx.display_names(&created).await?;
    let display_name = match display_cells.first() {
        Some(Cell::Str(s)) => Arc::clone(s),
        Some(other) => {
            return Err(Error::programming(format!(
                "name_create: expected string display_name on {}, got {other:?}",
                records.model_name()
            )));
        }
        None => {
            return Err(Error::programming(format!(
                "name_create: no display_name returned for {}",
                records.model_name()
            )));
        }
    };
    Ok((id, display_name))
}

type NameCreateFn = MethodFn<(String,), (i64, Arc<str>)>;

fn base_name_create_impl<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (String,),
    _sup: Super<(String,), (i64, Arc<str>)>,
) -> BoxFuture<'a, Result<(i64, Arc<str>)>> {
    Box::pin(async move { base_name_create(tx, records, &args.0).await })
}

fn name_create_entry() -> &'static (dyn Any + Send + Sync) {
    static F: NameCreateFn = base_name_create_impl;
    &F
}

static BASE_NAME_CREATE: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "name_create",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<NameCreateFn>,
    entry: name_create_entry,
    file: file!(),
    line: line!(),
};

/// The `base` implementation of `name_create`, registered by [`crate::Dispatch::build`].
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 1] = [&BASE_NAME_CREATE];
