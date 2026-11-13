//! Candidate computes: async computes that may join same-call monetary admission
//! ("Async candidate currency computes").
//!
//! A `#[compute(candidate, ...)]` function receives a [`ReadTx`] instead of `&mut Transaction`.
//! The view offers ORM reads and SQL `SELECT` only. It has no mutation, no `extension<T>()`, no
//! `after_commit` and no way back to the mutable transaction, so those operations do not compile.
//! That is what lets the ORM run the function once, on staged inputs, before the mutation that
//! needs its result is admitted.
//!
//! Visibility, by design (the SQL guard is a lexical deny-list, see [`ReadTx::query`]):
//!
//! - ORM reads of the compute's own batch (`get`, `mapped`) see the staged values of its declared
//!   `depends` and `reads` fields, the same operation-local values a synchronous compute sees.
//!   Reading another field of the batch is a `Programming` error: declare it.
//! - ORM reads of other records, and SQL, see the transaction as it is before the mutation.
//!   SQL never sees staged values or unflushed cache writes.
//!
//! ```compile_fail,E0616
//! // A candidate compute cannot reach the mutable transaction through its view.
//! fn escape<'a>(view: &'a mut erp_orm::ReadTx<'_>) -> &'a mut erp_orm::Transaction {
//!     &mut *view.tx
//! }
//! ```
//!
//! ```compile_fail,E0599
//! // Nor can it mutate: the view has no write, extension or hook methods.
//! async fn mutate(view: &mut erp_orm::ReadTx<'_>) {
//!     view.execute("DELETE FROM res_partner", &[]).await.ok();
//! }
//! ```
//!
//! ```compile_fail,E0599
//! // No extension state and no commit hooks either.
//! fn hook(view: &mut erp_orm::ReadTx<'_>) {
//!     view.after_commit(|| {});
//! }
//! ```
//!
//! ```compile_fail,E0599
//! // `extension::<T>()` would hand out addon state that no rollback restores.
//! fn extension(view: &mut erp_orm::ReadTx<'_>) {
//!     let _ = view.extension::<u32>();
//! }
//! ```

use std::sync::Arc;

use erp_fields::{Cell, Error, FieldValue, RecId, Result};
use erp_registry::{FieldId, ModelId};
use tokio_postgres::Row;
use tokio_postgres::types::ToSql;

use crate::Transaction;
use crate::env::Env;
use crate::records::{Field, Inherits, Model, RecordSet, Records};

/// What a candidate compute may read of its own batch, and the values it sees.
pub(crate) struct ReadScope {
    model: ModelId,
    ids: Arc<[RecId]>,
    /// The declared `depends`/`reads` fields (and the group's own fields, old values).
    fields: Vec<FieldId>,
    /// Staged values, `values[k][i]` for `fields[k]` and `ids[i]`. `None`: the transaction
    /// holds the values (the lazy recompute after publication).
    values: Option<Vec<Vec<Cell>>>,
}

impl ReadScope {
    /// Reads of the batch go to the transaction; only the declared fields may be read.
    pub(crate) fn stored(model: ModelId, ids: Arc<[RecId]>, fields: &[FieldId]) -> Self {
        Self {
            model,
            ids,
            fields: fields.to_vec(),
            values: None,
        }
    }

    /// Reads of the batch see `columns` (one value per record, in `ids` order).
    pub(crate) fn staged(
        model: ModelId,
        ids: Arc<[RecId]>,
        columns: Vec<(FieldId, Vec<Cell>)>,
    ) -> Self {
        let (fields, values) = columns.into_iter().unzip();
        Self {
            model,
            ids,
            fields,
            values: Some(values),
        }
    }
}

/// The read-only view of the transaction a candidate compute receives.
///
/// See the [module documentation](self) for what it shows. It deliberately does not implement
/// `Deref` to [`Transaction`].
pub struct ReadTx<'t> {
    tx: &'t mut Transaction,
    scope: ReadScope,
}

impl<'t> ReadTx<'t> {
    pub(crate) fn new(tx: &'t mut Transaction, scope: ReadScope) -> Self {
        Self { tx, scope }
    }

    /// The environment the transaction was opened with (its base environment: not the
    /// `compute_sudo` or user environment of the batch, which `this.as_records().env()` holds).
    pub fn env(&self) -> Env {
        self.tx.env()
    }

    /// A field value of a singleton; the type's null value on an empty set; the singleton error
    /// on several records. Records of the compute's own batch show their staged values.
    ///
    /// # Errors
    /// `Programming` when `records` holds several records, or names a field of the compute's own
    /// batch that is not among its declared dependencies; the errors of an ordinary read.
    pub async fn get<M, P: Model, T: FieldValue>(
        &mut self,
        records: &RecordSet<M>,
        field: Field<P, T>,
    ) -> Result<T>
    where
        M: Model + Inherits<P>,
    {
        let cells = self.cells(records.as_records(), field.name()).await?;
        match cells.as_slice() {
            [] => T::from_cell(&Cell::Null),
            [cell] => T::from_cell(cell),
            _ => Err(Error::programming(format!(
                "Expected singleton: {}",
                records.as_records()
            ))),
        }
    }

    /// One value per record, in order. Records of the compute's own batch show their staged
    /// values.
    ///
    /// # Errors
    /// As [`ReadTx::get`], without the singleton rule.
    pub async fn mapped<M, P: Model, T: FieldValue>(
        &mut self,
        records: &RecordSet<M>,
        field: Field<P, T>,
    ) -> Result<Vec<T>>
    where
        M: Model + Inherits<P>,
    {
        self.cells(records.as_records(), field.name())
            .await?
            .iter()
            .map(T::from_cell)
            .collect()
    }

    /// Run a `SELECT` (also `WITH`, `SHOW`, `EXPLAIN` without `ANALYZE`). The database state is
    /// the one before the mutation in progress: staged values are not visible to SQL.
    ///
    /// # Errors
    /// `Programming` for a statement that is not certainly a read (data-changing keywords,
    /// `SELECT ... INTO`, locking clauses, effectful session functions, calls of `nextval`,
    /// `setval`, `pg_sleep` and `lo_*` large-object functions, text the scanner cannot read
    /// with certainty). This is a lexical deny-list, not a proof of read-only; the errors of an ordinary query.
    pub async fn query(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        self.tx.require_select_only(sql)?;
        self.tx.query(sql, params).await
    }

    async fn cells(&mut self, records: &Records, name: &str) -> Result<Vec<Cell>> {
        let spec = records.spec();
        let fid = spec.field_id(name).ok_or_else(|| {
            Error::programming(format!("Invalid field '{name}' on '{}'", spec.name))
        })?;
        let mut out: Vec<Option<Cell>> = vec![None; records.len()];
        let mut outside: Vec<usize> = Vec::new();
        for (k, id) in records.rec_ids().iter().enumerate() {
            let position = (records.model_id() == self.scope.model)
                .then(|| self.scope.ids.iter().position(|b| b == id))
                .flatten();
            let Some(position) = position else {
                outside.push(k);
                continue;
            };
            let Some(column) = self.scope.fields.iter().position(|f| *f == fid) else {
                return Err(Error::programming(format!(
                    "a candidate compute read {}.{name} of a record in its own batch, which is \
                     not among its declared depends or reads: declare it",
                    spec.name
                )));
            };
            match &self.scope.values {
                Some(values) => out[k] = Some(values[column][position].clone()),
                None => outside.push(k),
            }
        }
        if !outside.is_empty() {
            let ids: Vec<RecId> = outside.iter().map(|k| records.rec_ids()[*k]).collect();
            let rest = records.browse_ids(ids);
            self.tx.admit_records(&rest).await?;
            let cells = rest.cells(self.tx, name).await?;
            if cells.len() != outside.len() {
                return Err(Error::programming(
                    "a candidate compute read returned an incomplete column",
                ));
            }
            for (k, cell) in outside.into_iter().zip(cells) {
                out[k] = Some(cell);
            }
        }
        out.into_iter()
            .map(|cell| {
                cell.ok_or_else(|| {
                    Error::programming("a candidate compute read lost a record's value")
                })
            })
            .collect()
    }
}
