//! Row versions and write hooks on the ORM side (design note
//! ).
//!
//! - A model declared `row_version = true` has a `row_version` field. Every flush `UPDATE` sets
//!   `row_version = row_version + 1` itself (so the `BEFORE UPDATE` trigger schema sync creates
//!   only runs for other SQL), at no extra statement.
//! - [`Records::expect_versions`] records the versions a caller read; the next flush `UPDATE` of
//!   those records also requires `row_version = <expected>`. A row it skips is classified by one
//!   `SELECT`: stale → `Conflict` (never retried), gone → `Missing`. Each expectation is consumed
//!   by the first flush that writes its record in the transaction: later flushes hold the row lock.
//! - [`WriteHook`]s registered on the [`crate::OrmBuilder`] receive the old and new values of the
//!   fields they name for every row a flush `UPDATE` (or an upsert) writes, from the statement's
//!   own `RETURNING` (0 extra statements). Models without hooks keep the plain statement.

use std::sync::Arc;

use erp_fields::{Cell, Error, RealId, RecId, Result};
use erp_registry::{FieldId, ModelId, ModelSpec, Registry};
use rustc_hash::FxHashMap;

use crate::Transaction;
use crate::dispatch::BoxFuture;
use crate::records::Records;

/// Expected versions recorded by [`Records::expect_versions`], until a flush consumes them.
#[derive(Debug, Default, Clone)]
pub(crate) struct Expectations {
    map: FxHashMap<(ModelId, RealId), i64>,
}

impl Expectations {
    /// The expected version of each id (`None` without one), or `None` when no id has one.
    pub(crate) fn for_ids(&self, model: ModelId, ids: &[RealId]) -> Option<Vec<Option<i64>>> {
        if self.map.is_empty() {
            return None;
        }
        let expected: Vec<Option<i64>> = ids
            .iter()
            .map(|id| self.map.get(&(model, *id)).copied())
            .collect();
        expected.iter().any(Option::is_some).then_some(expected)
    }

    /// Forget the expectations of rows a flush wrote.
    pub(crate) fn consume(&mut self, model: ModelId, ids: &[RealId]) {
        for id in ids {
            self.map.remove(&(model, *id));
        }
    }

    /// Whether any expectation of `model` is pending.
    pub(crate) fn has_model(&self, model: ModelId) -> bool {
        self.map.keys().any(|(m, _)| *m == model)
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }
}

impl Records {
    /// Expect these row versions for records of this model (the versions the caller read): the
    /// next flush `UPDATE` that writes one of them fails with `Conflict` when its version moved
    /// (design note §4.3). A later call for the same record replaces its expectation; a record
    /// that is never written is never checked. The only statement sent is the record check that
    /// the caller may read the records (none in superuser mode or under a rule-free policy); the
    /// expectations of a rolled back savepoint go with it.
    ///
    /// ```ignore
    /// quants.expect_versions(&mut tx, [(quant_id, version_read_by_the_client)]).await?;
    /// quant.write(&mut tx, vals).await?;
    /// tx.flush().await?; // Conflict if someone else changed it since
    /// ```
    ///
    /// # Errors
    ///
    /// `Programming` when the model has no row version (`'<model>' has no row version (declare
    /// row_version = true)`), for a new record's id, a version below 1, or one id given twice
    /// with different versions; `Access` when the caller may not read the model or one of the
    /// records (the `Conflict` text then only ever names records the caller can read).
    pub async fn expect_versions(
        &self,
        tx: &mut Transaction,
        versions: impl IntoIterator<Item = (RecId, i64)>,
    ) -> Result<()> {
        tx.check_records(self)?;
        let spec = self.spec();
        if spec.row_version.is_none() {
            return Err(Error::programming(format!(
                "'{}' has no row version (declare row_version = true)",
                spec.name
            )));
        }
        let mut batch: FxHashMap<RealId, i64> = FxHashMap::default();
        for (id, version) in versions {
            let Some(real) = id.as_real() else {
                return Err(Error::programming(format!(
                    "expect_versions on '{}': {id} is a new record, it has no stored version",
                    spec.name
                )));
            };
            if version < 1 {
                return Err(Error::programming(format!(
                    "expect_versions on '{}': record {id} cannot have version {version} (row \
                     versions start at 1)",
                    spec.name
                )));
            }
            match batch.insert(real, version) {
                Some(other) if other != version => {
                    return Err(Error::programming(format!(
                        "expect_versions on '{}': record {id} is given versions {other} and \
                         {version}",
                        spec.name
                    )));
                }
                _ => {}
            }
        }
        // the caller must be able to read the records it expects versions of: a later `Conflict`
        // names them with their current versions (review SEC-S6)
        let env = self.env().clone();
        tx.prepare_access(&env).await?;
        tx.check_model(&env, self.model_id(), crate::access::Operation::Read)?;
        let given = self.browse_ids(batch.keys().map(|id| RecId::real(*id)));
        tx.check_record_access(&given, crate::access::Operation::Read)
            .await?;
        let model = self.model_id();
        for (id, version) in batch {
            tx.versions.map.insert((model, id), version);
        }
        Ok(())
    }
}

/// The `Conflict` error of stale expectations: `(id, expected, current)`, up to 20 listed.
pub(crate) fn conflict_error(spec: &ModelSpec, stale: &[(RealId, i64, i64)]) -> Error {
    const LISTED: usize = 20;
    match stale {
        [(id, expected, current)] => Error::conflict(format!(
            "Record '{}' ({}) {id} was changed by someone else since you read it (version \
             {expected}, now {current}). Reload it and apply your change again.",
            spec.description, spec.name
        )),
        _ => {
            let mut listed: Vec<String> = stale
                .iter()
                .take(LISTED)
                .map(|(id, expected, current)| format!("{id} (version {expected}, now {current})"))
                .collect();
            if stale.len() > LISTED {
                listed.push(format!("and {} more", stale.len() - LISTED));
            }
            Error::conflict(format!(
                "Records '{}' ({}) {} were changed by someone else since you read them. Reload \
                 them and apply your changes again.",
                spec.description,
                spec.name,
                listed.join(", ")
            ))
        }
    }
}

/// The old and new values of the fields a [`WriteHook`] names, for the rows one statement wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteBatch {
    model: &'static str,
    fields: Vec<&'static str>,
    rows: Vec<WrittenRow>,
}

impl WriteBatch {
    pub(crate) fn new(
        model: &'static str,
        fields: Vec<&'static str>,
        rows: Vec<WrittenRow>,
    ) -> Self {
        Self {
            model,
            fields,
            rows,
        }
    }

    /// The model.
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// The fields, in the order of [`WrittenRow::values`].
    pub fn fields(&self) -> &[&'static str] {
        &self.fields
    }

    /// One entry per written row.
    pub fn rows(&self) -> &[WrittenRow] {
        &self.rows
    }
}

/// One written row: its id and `(old, new)` per field of the batch.
#[derive(Debug, Clone, PartialEq)]
pub struct WrittenRow {
    id: RecId,
    values: Vec<(Cell, Cell)>,
}

impl WrittenRow {
    pub(crate) fn new(id: RecId, values: Vec<(Cell, Cell)>) -> Self {
        Self { id, values }
    }

    /// The record.
    pub fn id(&self) -> RecId {
        self.id
    }

    /// `(old, new)` per field of the batch, in [`WriteBatch::fields`] order.
    pub fn values(&self) -> &[(Cell, Cell)] {
        &self.values
    }
}

/// The function of a write hook: called after the statement, inside the transaction.
pub type WriteHookFn = for<'a> fn(&'a mut Transaction, &'a WriteBatch) -> BoxFuture<'a, Result<()>>;

/// A hook on the writes of some fields of a model (design note §4.4; and
/// register theirs): every flush `UPDATE` and upsert update of those fields returns their old
/// values in the same statement and passes them to the hook. Register it with
/// [`crate::OrmBuilder::write_hook`].
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct WriteHook {
    model: &'static str,
    fields: &'static [&'static str],
    call: WriteHookFn,
}

impl WriteHook {
    /// A hook on `fields` of `model` (stored columns, checked when the ORM is built).
    pub const fn new(
        model: &'static str,
        fields: &'static [&'static str],
        call: WriteHookFn,
    ) -> Self {
        Self {
            model,
            fields,
            call,
        }
    }
}

impl std::fmt::Debug for WriteHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteHook")
            .field("model", &self.model)
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

/// The function of an unlink hook: called after the `DELETE` of one `unlink` call, inside the
/// transaction, with the model's name and the ids that were deleted (real ids, each once).
pub type UnlinkHookFn =
    for<'a> fn(&'a mut Transaction, &'a str, &'a [RecId]) -> BoxFuture<'a, Result<()>>;

/// A hook on every `unlink`: the owner of a table that points at records of any model
/// (`erp-data`'s external ids) removes what it holds for the deleted records, in one statement
/// per call. Register it with [`crate::OrmBuilder::unlink_hook`]; a rebuilt ORM keeps it. The
/// hook runs for every model, so it must decide itself whether the model matters, and it must not
/// issue a statement per record.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct UnlinkHook {
    call: UnlinkHookFn,
}

impl UnlinkHook {
    /// A hook that calls `call` after each `unlink`.
    pub const fn new(call: UnlinkHookFn) -> Self {
        Self { call }
    }

    /// Run the hook.
    pub(crate) fn call<'a>(
        &self,
        tx: &'a mut Transaction,
        model: &'a str,
        ids: &'a [RecId],
    ) -> BoxFuture<'a, Result<()>> {
        (self.call)(tx, model, ids)
    }
}

impl std::fmt::Debug for UnlinkHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnlinkHook").finish_non_exhaustive()
    }
}

/// What a selection ondelete callback receives: the transaction, the model name,
/// and the record IDs holding the removed selection value.
pub type SelectionCallbackFn =
    for<'a> fn(&'a mut Transaction, &'a str, &'a [RecId]) -> BoxFuture<'a, Result<()>>;

/// A selection ondelete callback.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct SelectionCallback {
    name: &'static str,
    call: SelectionCallbackFn,
}

impl SelectionCallback {
    /// A selection callback registered under `name`.
    pub const fn new(name: &'static str, call: SelectionCallbackFn) -> Self {
        Self { name, call }
    }

    /// The name of the callback.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Run the callback.
    pub(crate) fn call<'a>(
        &self,
        tx: &'a mut Transaction,
        model: &'a str,
        ids: &'a [RecId],
    ) -> BoxFuture<'a, Result<()>> {
        (self.call)(tx, model, ids)
    }
}

impl std::fmt::Debug for SelectionCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectionCallback")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A hook resolved against a registry.
#[derive(Clone)]
pub(crate) struct ResolvedHook {
    pub(crate) fields: Vec<FieldId>,
    pub(crate) call: WriteHookFn,
}

/// The hooks of an ORM, by model.
#[derive(Clone, Default)]
pub(crate) struct WriteHooks {
    by_model: FxHashMap<ModelId, Vec<ResolvedHook>>,
}

impl WriteHooks {
    /// Resolve `hooks` against `registry`.
    ///
    /// # Errors
    ///
    /// `Programming` for an unknown model, an unknown field, a field without a column of the
    /// model's own table, or an empty field list.
    pub(crate) fn build(registry: &Arc<Registry>, hooks: &[WriteHook]) -> Result<Self> {
        let mut by_model: FxHashMap<ModelId, Vec<ResolvedHook>> = FxHashMap::default();
        for hook in hooks {
            let spec = registry.model(hook.model).ok_or_else(|| {
                Error::programming(format!("write hook on unknown model '{}'", hook.model))
            })?;
            if hook.fields.is_empty() {
                return Err(Error::programming(format!(
                    "write hook on '{}' names no field",
                    hook.model
                )));
            }
            let mut fields = Vec::with_capacity(hook.fields.len());
            for name in hook.fields {
                let field = spec.field(name).ok_or_else(|| {
                    Error::programming(format!(
                        "write hook on '{}': unknown field '{name}'",
                        hook.model
                    ))
                })?;
                if field.inherited.is_some() || field.column().is_none() {
                    return Err(Error::programming(format!(
                        "write hook on '{}': field '{name}' is not a column of the model's own \
                         table",
                        hook.model
                    )));
                }
                fields.push(field.id);
            }
            by_model.entry(spec.id).or_default().push(ResolvedHook {
                fields,
                call: hook.call,
            });
        }
        Ok(Self { by_model })
    }

    /// The hooks of `model`.
    pub(crate) fn of(&self, model: ModelId) -> &[ResolvedHook] {
        self.by_model.get(&model).map_or(&[], Vec::as_slice)
    }
}

/// The fields of `group` that a write hook of `model` names, in group order.
pub(crate) fn hooked_fields(tx: &Transaction, model: ModelId, group: &[FieldId]) -> Vec<FieldId> {
    let hooks = tx.shared.write_hooks.of(model);
    // the tracked columns of the model (D1) travel the same way
    let tracked = &tx.registry().model_by_id(model).tracked;
    if hooks.is_empty() && tracked.is_empty() {
        return Vec::new();
    }
    group
        .iter()
        .copied()
        .filter(|f| {
            (tracked.contains(f)
                && tx
                    .registry()
                    .model_by_id(model)
                    .field_by_id(*f)
                    .has_column())
                || hooks.iter().any(|h| h.fields.contains(f))
        })
        .collect()
}

/// A `22003` of a flush `UPDATE` on a versioned model: the only value the statement computes is
/// `row_version + 1` (every written value was validated before), so it names that column. Other
/// errors are returned as they are.
pub(crate) fn version_overflow(spec: &ModelSpec, error: Error) -> Error {
    if spec.row_version.is_none() || error.sqlstate() != Some("22003") {
        return error;
    }
    let sqlstate = error.sqlstate().map(str::to_owned);
    Error::new(
        erp_fields::ErrorKind::Database,
        format!(
            "the row version ({}.{}) of a '{}' record is out of range",
            spec.table,
            erp_registry::ROW_VERSION,
            spec.name
        ),
    )
    .with_sqlstate(sqlstate)
    .with_source(error)
}

/// The error for rows a flush `UPDATE` with expected versions skipped: one `SELECT` of their
/// current versions tells a stale row (`Conflict`) from a deleted one (`Missing`). Stale rows win
/// when both occur.
pub(crate) async fn stale_or_missing(
    tx: &mut Transaction,
    spec: &ModelSpec,
    ids: &[RealId],
    expected: &[Option<i64>],
    skipped: &[RealId],
) -> Error {
    let sql = erp_sql::version::current_versions(&spec.table);
    let params: Vec<i64> = skipped.iter().map(|id| id.get()).collect();
    let rows = match tx.query(&sql, &[&params]).await {
        Ok(rows) => rows,
        Err(error) => {
            // keep the kind (a serialization failure stays retryable) and the cause: the caller
            // learns that the skipped records could not be classified, not only the database error
            let listed: Vec<String> = skipped.iter().map(ToString::to_string).collect();
            let sqlstate = error.sqlstate().map(str::to_owned);
            return Error::new(
                error.kind(),
                format!(
                    "the flush UPDATE of '{}' skipped records {} (stale or deleted) and the \
                     statement that tells which failed: {}",
                    spec.name,
                    listed.join(", "),
                    error.message()
                ),
            )
            .with_sqlstate(sqlstate)
            .with_source(error);
        }
    };
    let mut current: FxHashMap<i64, i64> = FxHashMap::default();
    for row in &rows {
        let decoded: std::result::Result<(i64, i64), _> = row
            .try_get(0)
            .and_then(|id| row.try_get(1).map(|v| (id, v)));
        match decoded {
            Ok((id, version)) => {
                current.insert(id, version);
            }
            Err(e) => {
                return Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not decode the current row versions",
                )
                .with_source(e);
            }
        }
    }
    // the expectation of each id of the statement (ids without one are absent)
    let mut wanted_of: FxHashMap<RealId, i64> = FxHashMap::default();
    for (id, wanted) in ids.iter().zip(expected) {
        if let Some(version) = wanted {
            wanted_of.insert(*id, *version);
        }
    }
    let mut stale: Vec<(RealId, i64, i64)> = Vec::new();
    let mut missing: Vec<RealId> = Vec::new();
    let mut unexplained: Vec<RealId> = Vec::new();
    for id in skipped {
        let wanted = wanted_of.get(id).copied();
        match (current.get(&id.get()), wanted) {
            (None, _) => missing.push(*id),
            (Some(now), Some(w)) if w != *now => stale.push((*id, w, *now)),
            (Some(_), _) => unexplained.push(*id),
        }
    }
    if !stale.is_empty() {
        stale.sort_unstable();
        return conflict_error(spec, &stale);
    }
    if !missing.is_empty() {
        return crate::write::missing_error(spec, &missing, tx.env().uid());
    }
    let listed: Vec<String> = unexplained.iter().map(ToString::to_string).collect();
    Error::programming(format!(
        "the flush UPDATE of '{}' skipped records {} that exist at their expected version",
        spec.name,
        listed.join(", ")
    ))
}

/// Hand the old and new values one flush `UPDATE` returned to the write hooks of `spec`'s model.
pub(crate) async fn call_write_hooks(
    tx: &mut Transaction,
    spec: &ModelSpec,
    hooked: &[FieldId],
    rows: Vec<(RecId, Vec<(Cell, Cell)>)>,
) -> Result<()> {
    let hooks: Vec<ResolvedHook> = tx.shared.write_hooks.of(spec.id).to_vec();
    for hook in hooks {
        let positions: Vec<usize> = hooked
            .iter()
            .enumerate()
            .filter(|(_, f)| hook.fields.contains(f))
            .map(|(i, _)| i)
            .collect();
        if positions.is_empty() {
            continue;
        }
        let fields = positions
            .iter()
            .map(|i| spec.field_by_id(hooked[*i]).name)
            .collect();
        let mut written = Vec::with_capacity(rows.len());
        for (id, values) in &rows {
            let mut picked = Vec::with_capacity(positions.len());
            for i in &positions {
                let value = values.get(*i).cloned().ok_or_else(|| {
                    Error::programming(format!(
                        "write hook values of '{}' record {id} are incomplete",
                        spec.name
                    ))
                })?;
                picked.push(value);
            }
            written.push(WrittenRow::new(*id, picked));
        }
        let batch = WriteBatch::new(spec.name, fields, written);
        (hook.call)(tx, &batch).await?;
    }
    Ok(())
}
