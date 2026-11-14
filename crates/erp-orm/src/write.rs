//! `create`, `write` and `flush` (CRUD §4.2, §4.4; ENV §4.6).
//!
//! `create` inserts immediately. The rows of one call share one column set: every column some row
//! gives, NULL where a row gives none (no per-cell `DEFAULT`; columns have no database default,
//! so NULL is what leaving the column out would store). Each bounded batch is one `INSERT …
//! RETURNING` that draws its ids in input order (`nextval` evaluated after `ORDER BY` the
//! ordinality), so ids ascend in input order (F6) without reserving them.
//! One statement shape per (table, column set), one array parameter per column. The cache is then
//! seeded so it equals what a fetch would return.
//!
//! `write` validates and stages values in the cache, marking them dirty. Before a subsequent
//! event-relevant mutation, earlier dirty writes and stored computes settle with their original
//! attribution, preserving operation order even when the identities are equal. `flush` sends one
//! `UPDATE … FROM unnest(…)` per group of records sharing the same dirty fields and
//! reports ids that no longer exist.
//!
//! Every cell is validated against its field ([`validate_cell`]) whichever surface built it.
//!
//! Computed fields (S4a): `create` computes the precomputed fields of the rows before the INSERT,
//! marks the other stored computes to compute, runs constraints and the inverses of
//! the given fields (after the x2many values, when the create has any: [`AfterInsert`]); `write`
//! protects the groups of inversed and editable computed fields, notifies
//! dependents of the values that changed, then runs constraints and inverses (COMP §4.6–4.7).

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::html::input_cell as to_cell;
use erp_fields::convert::{FieldRef, validate_cell};
use erp_fields::{
    Cell, ColumnType, Date, DateTime, Decimal, Error, FieldTranslations, FieldType, FieldValue,
    RealId, RecId, Result, Storage, Value,
};
use erp_registry::{
    DefaultContext, FieldDefault, FieldId, FieldSpec, MAGIC_FIELD_NAMES, ModelId, ModelSpec,
};
use erp_sql::NOW_UTC;
use erp_sql::dml::{IdSource, insert_rows, insert_rows_checked};
use rustc_hash::FxHashSet;
use tokio_postgres::types::ToSql;

use crate::Transaction;
use crate::access::Operation;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::env::Env;
use crate::field_access::FieldOp;
use crate::records::Records;
use crate::vals::RawVals;

fn invalid_field(spec: &ModelSpec, name: &str) -> Error {
    Error::programming(format!("Invalid field '{name}' in '{}'", spec.name))
}

fn is_magic(name: &str) -> bool {
    MAGIC_FIELD_NAMES.contains(&name)
}

fn fref<'a>(spec: &'a ModelSpec, field: &'a FieldSpec) -> FieldRef<'a> {
    FieldRef {
        model: spec.name,
        field: field.name,
        label: &field.string,
    }
}

/// Monetary amounts are applied after other fields in one write (`write_sequence`).
fn write_sequence(ty: &FieldType) -> u8 {
    u8::from(matches!(ty, FieldType::Monetary { .. })) * 10
}

impl Records {
    /// `create(vals_list)` through the method chain (`base` implements it).
    /// A `Validation` error, or any error once rows were inserted or values written (parents of
    /// `_inherits`, lines of x2many commands), poisons the transaction: commit refuses until
    /// rollback (decision E1, F1). Unadapted database-generated inputs (NULL included)
    /// and generated context defaults are refused before ordinary create overrides.
    /// Declared generated inputs first pass their authorized adapter and output checks.
    pub async fn create(&self, tx: &mut Transaction, vals_list: Vec<RawVals>) -> Result<Records> {
        tx.refuse_adapter_effect()?;
        tx.admit_records(self).await?;
        tx.check_capture_create(self, &vals_list)?;
        crate::event_namespace::check_create_capture(self, &vals_list)?;
        let before = tx.mutations();
        let result = async {
            let vals_list = crate::generated_input::create(tx, self, vals_list).await?;
            tx.check_capture_create(self, &vals_list)?;
            match crate::database_generated::check_write_names(
                tx,
                self.spec(),
                vals_list
                    .iter()
                    .flat_map(|v| v.names())
                    .chain(self.env().context().default_field_names()),
            ) {
                Ok(()) => {
                    call_method::<(Vec<RawVals>,), Records>(tx, self, "create", (vals_list,)).await
                }
                Err(error) => Err(error),
            }
        }
        .await;
        if let Err(error) = &result {
            tx.poison_after(before, error);
        }
        result
    }

    /// `write(vals)` through the method chain (`base` implements it). A `Validation` error, or any
    /// error once values were written (in the cache or the database), poisons the transaction
    /// (decision E1, F1). Unadapted database-generated inputs are refused before write
    /// overrides. Declared inputs on persisted records first pass their authorized
    /// adapter; new and empty recordsets retain the explicit assignment refusal.
    /// Event-relevant writes require an [`crate::OperationIdentity`]; deferred captures keep the
    /// writer's actor and effective companies when another environment flushes them. Nested
    /// event-relevant side effects during a mutation or compute are refused by S7.
    pub async fn write(&self, tx: &mut Transaction, vals: RawVals) -> Result<()> {
        tx.refuse_adapter_effect()?;
        tx.admit_records(self).await?;
        tx.check_capture_write(self, &vals)?;
        let before = tx.mutations();
        let result = async {
            if let Some(rows) = crate::generated_input::write(tx, self, &vals).await? {
                for row in &rows {
                    crate::database_generated::check_write_names(tx, self.spec(), row.names())?;
                    tx.check_capture_write(self, row)?;
                }
                if let Some(first) = rows.first()
                    && rows.iter().all(|row| row == first)
                {
                    return call_method::<(RawVals,), ()>(tx, self, "write", (first.clone(),))
                        .await;
                }
                for (id, row) in self.rec_ids().iter().zip(rows) {
                    let record = self.browse_ids(vec![*id]);
                    call_method::<(RawVals,), ()>(tx, &record, "write", (row,)).await?;
                }
                return Ok(());
            }
            crate::database_generated::check_write_names(tx, self.spec(), vals.names())?;
            call_method::<(RawVals,), ()>(tx, self, "write", (vals,)).await
        }
        .await;
        if let Err(error) = &result {
            tx.poison_after(before, error);
        }
        result
    }

    /// Write through the relational base writer, bypassing every model `write` override.
    ///
    /// Trusted native Rust addons may use this when their propagation contract explicitly
    /// requires a base write. The caller's actor and company scope stay in effect:
    /// model access, record rules, field groups, relational validation, constraints, inverses,
    /// audit and recomputation follow the ordinary base writer. This grants no elevation and
    /// is not a sandbox capability. No method-dispatch, RPC, script, server-action or WASM
    /// entry is registered for this API.
    ///
    /// ```no_run
    /// # async fn example(records: &erp_orm::Records, tx: &mut erp_orm::Transaction, values: erp_orm::RawVals) -> erp_fields::Result<()> {
    /// records.write_base(tx, values).await?;
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the ordinary base-write errors, including authorization and read-only refusals.
    /// Record/transaction binding and explicit database-generated assignments are checked before
    /// the writer runs, including generated assignments on empty records. A `Validation` error,
    /// or any error after mutation, poisons the transaction until rollback.
    pub async fn write_base(&self, tx: &mut Transaction, vals: RawVals) -> Result<()> {
        tx.refuse_adapter_effect()?;
        tx.admit_records(self).await?;
        let before = tx.mutations();
        let result =
            match crate::database_generated::check_write_names(tx, self.spec(), vals.names()) {
                Ok(()) => {
                    // Keep the existing base_write entry's heap boundary: embedding the
                    // relational writer's future in native propagation grows its stack frame.
                    Box::pin(crate::relcrud::write(tx, self, vals)).await
                }
                Err(error) => Err(error),
            };
        if let Err(error) = &result {
            tx.poison_after(before, error);
        }
        result
    }

    /// Convert dynamic values with the one converter.
    /// Declared adapted inputs can be represented here for create/write; conversion
    /// grants no assignment eligibility to defaults, new records, write_base or upsert.
    pub fn vals_from_values<'v>(
        &self,
        values: impl IntoIterator<Item = (&'v String, &'v Value)>,
    ) -> Result<RawVals> {
        let spec = self.spec();
        let mut vals = RawVals::new();
        for (name, value) in values {
            if name == "id" {
                continue;
            }
            let field = spec.field(name).ok_or_else(|| invalid_field(spec, name))?;
            if !field.is_input_adapted() {
                crate::database_generated::check_assignment(spec, field)?;
            }
            if let Some(comodel) = field.ty.comodel().filter(|_| field.ty.is_x2many()) {
                let comodel = self.env().records(comodel)?;
                let commands = self.commands_from_value(&comodel, name, value)?;
                vals.insert_commands(name.clone().into(), commands);
                continue;
            }
            let cell = if field.translate {
                match value {
                    Value::Map(m) => {
                        let mut json_map = serde_json::Map::new();
                        for (k, v) in m.as_ref() {
                            let json_v = match v {
                                Value::Str(s) => serde_json::Value::String(s.to_string()),
                                Value::False | Value::Bool(false) => serde_json::Value::Bool(false),
                                // an HTML value is never echoed in an error (review r1 S10)
                                _ if matches!(field.ty, FieldType::Html) => {
                                    return Err(Error::validation(format!(
                                        "Invalid translation for '{k}' in {}.{name}: expected a string or False",
                                        spec.name
                                    )));
                                }
                                other => {
                                    return Err(Error::validation(format!(
                                        "Invalid translation for '{k}' in {}.{name}: expected a string or False, got {other:?}",
                                        spec.name
                                    )));
                                }
                            };
                            json_map.insert(k.clone(), json_v);
                        }
                        Cell::Json(std::sync::Arc::new(serde_json::Value::Object(json_map)))
                    }
                    Value::False | Value::Bool(false) => Cell::Null,
                    Value::Str(s) => Cell::Str(std::sync::Arc::clone(s)),
                    _ => to_cell(fref(spec, field), &field.ty, value)?,
                }
            } else {
                to_cell(fref(spec, field), &field.ty, value)?
            };
            vals.insert(name.clone().into(), cell);
        }
        Ok(vals)
    }

    /// `create` with dynamic values (JSON-2, data files).
    pub async fn create_values(
        &self,
        tx: &mut Transaction,
        values: &[BTreeMap<String, Value>],
    ) -> Result<Records> {
        tx.refuse_adapter_effect()?;
        // a value refused here changed nothing yet, so the transaction stays usable
        let vals = values
            .iter()
            .map(|v| self.vals_from_values(v))
            .collect::<Result<Vec<_>>>()?;
        self.create(tx, vals).await
    }

    /// `write` with dynamic values. Unadapted generated targets are refused;
    /// declared adapted inputs use the same persisted-write boundary as typed values.
    /// On an empty set other names and values are not checked.
    pub async fn write_values<'v>(
        &self,
        tx: &mut Transaction,
        values: impl IntoIterator<Item = (&'v String, &'v Value)>,
    ) -> Result<()> {
        tx.refuse_adapter_effect()?;
        tx.admit_records(self).await?;
        if self.is_empty() {
            crate::database_generated::check_write_names(
                tx,
                self.spec(),
                values.into_iter().map(|(name, _)| name.as_str()),
            )?;
            return Ok(());
        }
        let vals = self.vals_from_values(values)?;
        self.write(tx, vals).await
    }

    /// `records[field] = value` on the dynamic surface: one `write` on the whole set;
    /// `id` cannot be assigned.
    pub async fn set_value(&self, tx: &mut Transaction, field: &str, value: Value) -> Result<()> {
        if field == "id" {
            return Err(Error::programming("field 'id' cannot be assigned"));
        }
        let mut map = BTreeMap::new();
        map.insert(field.to_owned(), value);
        self.write_values(tx, &map).await
    }

    /// Update named translations. `False`, `Bool(false)` and JSON null
    /// remove named non-English entries. Falsy English updates are ignored (including on required
    /// fields); an empty string is still a real update. The final duplicate entry determines the
    /// request, so a final falsy English entry cancels an earlier English update.
    /// Empty input or only a falsy English update returns `false`.
    ///
    /// # Errors
    /// Returns access/field errors, `UserError` for inactive languages or unsupported values,
    /// `Validation` for a missing required English base or excessive Char length, and
    /// `Programming` naming the model/field for corrupt stored translations. No mutation occurs
    /// before validation succeeds.
    pub async fn update_field_translations<K: Into<String>>(
        &self,
        tx: &mut Transaction,
        field_name: &str,
        translations: impl IntoIterator<Item = (K, Value)>,
    ) -> Result<bool> {
        tx.refuse_adapter_effect()?;
        tx.admit_records(self).await?;
        self.ensure_one()?;
        // Authorize before capture settlement can flush earlier permitted work.
        self.check_access(tx, Operation::Write).await?;
        let field = self.spec().field(field_name).ok_or_else(|| {
            Error::programming(format!(
                "Invalid field '{field_name}' on '{}'",
                self.spec().name
            ))
        })?;
        tx.check_fields(self.env(), self.model_id(), [field.id], FieldOp::Write)?;
        let previous_origin = tx.pending_capture.active.clone();
        let relevant = self
            .spec()
            .field(field_name)
            .is_some_and(|field| field.translate && tx.capture_relevant(self.model_id(), field.id));
        let before = tx.mutations();
        let guard = tx.guard();
        if let Err(error) = tx.begin_capture_operation(self, relevant).await {
            guard.disarm();
            return Err(error);
        }
        let result = self
            .update_field_translations_inner(tx, field_name, translations)
            .await;
        tx.pending_capture.active = previous_origin;
        if let Err(error) = &result {
            tx.poison_after(before, error);
        }
        guard.disarm();
        result
    }

    async fn update_field_translations_inner<K: Into<String>>(
        &self,
        tx: &mut Transaction,
        field_name: &str,
        translations: impl IntoIterator<Item = (K, Value)>,
    ) -> Result<bool> {
        self.ensure_one()?;
        self.check_access(tx, Operation::Write).await?;
        let spec = self.spec();
        let field = spec.field(field_name).ok_or_else(|| {
            Error::programming(format!("Invalid field '{field_name}' on '{}'", spec.name))
        })?;
        let fid = field.id;
        tx.check_fields(self.env(), self.model_id(), [fid], FieldOp::Write)?;

        if !field.translate {
            return Ok(false);
        }

        let active = tx.active_languages().await?;
        let id = self.rec_ids()[0];
        let model = self.model_id();
        let storage = field.storage();
        let stored = field.column().is_some();

        let raw_items: Vec<(String, Value)> = translations
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect();
        for (lang, _) in &raw_items {
            if !active.codes().contains(lang) && lang != "en_US" {
                return Err(Error::user(format!(
                    "The following languages are not activated: {lang}"
                )));
            }
        }

        let mut drop_langs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut updates: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for (lang, val) in raw_items {
            match val {
                Value::False | Value::Bool(false) => {
                    updates.remove(&lang);
                    // Lead-confirmed parity: a falsy English entry cancels the English update,
                    // while non-English entries remove their named translation.
                    if lang != "en_US" {
                        drop_langs.insert(lang);
                    }
                }
                Value::Json(v) if v.is_null() => {
                    updates.remove(&lang);
                    // Lead-confirmed parity: a falsy English entry cancels the English update,
                    // while non-English entries remove their named translation.
                    if lang != "en_US" {
                        drop_langs.insert(lang);
                    }
                }
                Value::Str(s) => {
                    if let FieldType::Char { size: Some(limit) } = field.ty {
                        let len = s.chars().count();
                        if len > limit as usize {
                            return Err(Error::validation(format!(
                                "Value too long for {}.{} (at most {limit} characters, got {len}): '{s}'",
                                field.name, lang
                            )));
                        }
                    }
                    drop_langs.remove(&lang);
                    updates.insert(lang, s.to_string());
                }
                other => {
                    if matches!(field.ty, FieldType::Html) {
                        return Err(Error::validation(format!(
                            "{}.{}: HTML translations require text or null",
                            spec.name, field.name
                        )));
                    }
                    return Err(Error::user(format!(
                        "Translations for model translated fields only accept falsy values and str, got {other:?} for {lang}"
                    )));
                }
            }
        }

        if drop_langs.is_empty() && updates.is_empty() {
            return Ok(false);
        }

        if !id.is_new() && stored {
            tx.ensure(self, fid).await?;
        }

        let old_cell = tx.cache.model_mut(model).get(fid, id);
        // Missing cache slots are empty only for new/nonstored fields; stored slots were ensured.
        let mut current_map =
            FieldTranslations::from_cell(old_cell.as_ref().unwrap_or(&Cell::Null))
                .map_err(|err| {
                    Error::programming(format!("{}.{}: {}", spec.name, field.name, err.message()))
                })?
                .into_inner();

        for drop_lang in &drop_langs {
            current_map.remove(drop_lang);
        }
        for (k, v) in updates {
            current_map.insert(k, v);
        }

        if field.required {
            let base_ok = current_map.get("en_US").is_some_and(|s| !s.is_empty());
            if !base_ok {
                return Err(Error::validation(format!(
                    "Missing required field '{}': the base (en_US) value must be non-empty",
                    field.name
                )));
            }
        }

        let new_cell = if current_map.is_empty() {
            Cell::Null
        } else {
            FieldTranslations::new(current_map).into_cell()
        };

        let new_cell = tx.admit_html_cell(spec, field, new_cell)?;
        if old_cell
            .as_ref()
            .is_some_and(|cur| cur.same_value(&new_cell))
        {
            return Ok(false);
        }

        if stored && !id.is_new() && !tx.translated_baselines.contains_key(&(model, fid, id)) {
            tx.translated_baselines
                .insert((model, fid, id), old_cell.clone().unwrap_or(Cell::Null));
        }

        tx.cache
            .model_mut(model)
            .set(fid, storage, id, new_cell, stored && !id.is_new())?;
        tx.note_mutation();
        if stored && !id.is_new() {
            tx.note_database_change(model, &[fid]);
        }
        tx.bind_capture(model, fid, &[id])?;
        tx.modified_prefetch(
            model,
            vec![(fid, vec![id])],
            Some(Arc::from(self.prefetch_ids())),
        )
        .await?;

        Ok(true)
    }

    /// Read a singleton's complete translation map, without changing scalar read types.
    /// NULL and untranslated fields return an empty map; this accessor preserves normal model,
    /// field and record access checks. `prefetch_langs` never changes `value`/`read` return types.
    ///
    /// # Errors
    /// Returns access/field/singleton errors or `Programming` naming the model/field when the
    /// stored translation map contains non-string values or has the wrong shape.
    pub async fn get_field_translations(
        &self,
        tx: &mut Transaction,
        field_name: &str,
    ) -> Result<FieldTranslations> {
        self.ensure_one()?;
        self.check_access(tx, Operation::Read).await?;
        let spec = self.spec();
        let field = spec.field(field_name).ok_or_else(|| {
            Error::programming(format!("Invalid field '{field_name}' on '{}'", spec.name))
        })?;
        let fid = field.id;
        tx.check_fields(self.env(), self.model_id(), [fid], FieldOp::Read)?;
        if !field.translate {
            return Ok(FieldTranslations::default());
        }
        let id = self.rec_ids()[0];
        let model = self.model_id();
        let stored = field.column().is_some();
        if !id.is_new() && stored {
            tx.ensure(self, fid).await?;
        }
        let cell = tx.cache.model_mut(model).get(fid, id).unwrap_or(Cell::Null);
        FieldTranslations::from_cell(&cell).map_err(|err| {
            Error::programming(format!("{}.{}: {}", spec.name, field.name, err.message()))
        })
    }
}

/// The default `Value` of a field on `records` without the `default_get` override chain:
/// context `default_<field>` first, then the field's own default (CRUD §4.5 steps 1 and 4). Only
/// the cache fill of new records' fields uses it (`default_cell`); `create` and `onchange` go
/// through [`Records::default_get`].
fn default_value(records: &Records, field: &FieldSpec) -> Option<Value> {
    // the server fills it at insert; no default may pretend otherwise
    if field.server_set {
        return None;
    }
    if let Some(value) = records
        .env()
        .context()
        .get(&format!("default_{}", field.name))
    {
        return Some(value);
    }
    field_default(records, field)
}

/// The field's own `default`: a static value or a function of the environment.
pub(crate) fn field_default(records: &Records, field: &FieldSpec) -> Option<Value> {
    match &field.default {
        Some(FieldDefault::Value(v)) => Some(v.to_value()),
        Some(FieldDefault::Fn(f)) => Some(f(&DefaultContext {
            uid: records.env().uid(),
        })),
        None => None,
    }
}

/// [`default_value`] as a cell of the field.
pub(crate) fn default_cell(
    records: &Records,
    spec: &ModelSpec,
    field: &FieldSpec,
) -> Result<Option<Cell>> {
    let Some(value) = default_value(records, field) else {
        return Ok(None);
    };
    to_cell(fref(spec, field), &field.ty, &value).map(Some)
}

fn decode_error(e: tokio_postgres::Error) -> Error {
    Error::new(
        erp_fields::ErrorKind::Database,
        "could not decode an INSERT result",
    )
    .with_source(e)
}

/// How the record check of a create runs.
#[derive(Debug)]
pub(crate) enum CreateCheck {
    /// A statement once the create is done ([`Transaction::check_record_access`]).
    After,
    /// Fused into the INSERT: the inserted records whose predicate was false or NULL.
    Fused(Vec<RealId>),
}

/// What the caller of [`create_records`] writes after the INSERT.
#[derive(Clone, Copy)]
pub(crate) enum AfterInsert<'a> {
    /// Nothing: the record check may be fused into the INSERT where the create predicate
    /// qualifies ([`Transaction::fused_create_check`]).
    Nothing,
    /// An
    /// inverse that reads the new record with the caller's rights then sees the relations its
    /// record rules may test (a `res.users` restricted through `company_ids`).
    X2many(&'a FxHashSet<FieldId>),
}

/// The steps of a create that follow the INSERT and may rewrite the new rows: the editable
/// related writes through given relations (COMP §4.8) and the inverses of the given fields
/// (COMP §4.7), both under protection of the given and precomputed groups, then the constraints
/// of the non-stored inversed fields. [`create_records`] runs them itself unless the create has
/// x2many values ([`AfterInsert::X2many`]); then the caller runs them once those are written.
#[must_use = "the inverses and related writes of the create have not run yet"]
pub(crate) struct AfterCreate {
    ids: Vec<RecId>,
    protected: Vec<FxHashSet<FieldId>>,
    related_writes: Vec<(crate::graph::Path, Cell, Vec<RecId>)>,
    inversed: Vec<(&'static str, Vec<FieldId>, Vec<RecId>)>,
    /// Constraints already run (stored fields given or precomputed) or run by the caller
    /// (the x2many fields): skipped among the inversed fields' constraints.
    skip: FxHashSet<FieldId>,
}

impl AfterCreate {
    /// Run the related writes, the inverses and the constraints of the non-stored inversed
    /// fields on `created`.
    pub(crate) async fn run(self, tx: &mut Transaction, created: &Records) -> Result<()> {
        let model = created.model_id();
        let registry = Arc::clone(created.env().registry());
        let spec = registry.model_by_id(model);
        if !self.related_writes.is_empty() {
            let guard = tx.guard();
            let depth = protect_rows(tx, model, &self.ids, &self.protected);
            let mut result = Ok(());
            for (path, cell, recs) in self.related_writes {
                let recs = created.browse_ids(recs);
                result = tx.write_related_targets(&recs, &path, cell).await;
                if result.is_err() {
                    break;
                }
            }
            tx.unprotect_to(depth);
            guard.disarm();
            result?;
        }
        if self.inversed.is_empty() {
            return Ok(());
        }
        let guard = tx.guard();
        let depth = protect_rows(tx, model, &self.ids, &self.protected);
        let mut result = Ok(());
        let mut inversed = self.inversed;
        inversed.sort_by_key(|(_, fields, _)| fields.iter().min().copied());
        for (method, _, recs) in &inversed {
            let recs = created.browse_ids(recs.iter().copied());
            result = call_method::<(), ()>(tx, &recs, method, ()).await;
            if result.is_err() {
                break;
            }
        }
        tx.unprotect_to(depth);
        guard.disarm();
        result?;
        // constraints of non-stored inversed fields
        let names: FxHashSet<FieldId> = inversed
            .iter()
            .flat_map(|(_, fields, _)| fields.iter().copied())
            .filter(|f| !spec.field_by_id(*f).store)
            .collect();
        tx.run_constraints(created, &names, &self.skip).await
    }
}

/// `base.create`: see the module documentation. Also says how the record check runs: fused into
/// the INSERT where the create predicate qualifies ([`Transaction::fused_create_check`]), after
/// the create otherwise; `after` decides ([`AfterInsert`]). With [`AfterInsert::X2many`] the
/// related writes and inverses are returned, not run ([`AfterCreate`]); with
/// [`AfterInsert::Nothing`] they have run and none is returned.
pub(crate) async fn create_records(
    tx: &mut Transaction,
    records: &Records,
    vals_list: Vec<RawVals>,
    after: AfterInsert<'_>,
) -> Result<(Records, CreateCheck, Option<AfterCreate>)> {
    tx.admit_records(records).await?;
    let relevant = !vals_list.is_empty() && tx.capture_model_relevant(records.model_id());
    let before = tx.mutations();
    let guard = tx.guard();
    if let Err(error) = tx.begin_capture_operation(records, relevant).await {
        guard.disarm();
        return Err(error);
    }
    let result = create_records_inner(tx, records, vals_list, after).await;
    if let Err(error) = &result {
        tx.poison_after(before, error);
    }
    guard.disarm();
    result
}

async fn create_records_inner(
    tx: &mut Transaction,
    records: &Records,
    vals_list: Vec<RawVals>,
    after: AfterInsert<'_>,
) -> Result<(Records, CreateCheck, Option<AfterCreate>)> {
    let none = FxHashSet::default();
    let (postponed, fuse) = match after {
        AfterInsert::Nothing => (&none, true),
        AfterInsert::X2many(fields) => (fields, false),
    };
    tx.admit_records(records).await?;
    crate::event_namespace::check_create_capture(records, &vals_list)?;
    let env = records.env().clone();
    let model = records.model_id();
    if vals_list.is_empty() {
        return Ok((records.browse_ids([]), CreateCheck::After, None));
    }
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(model);
    if !spec.has_table() {
        return Err(Error::programming(format!(
            "Cannot create records for abstract model {}",
            spec.name
        )));
    }
    tx.check_model(&env, model, Operation::Create)?;
    let capture = if spec.eventful {
        let companies = tx
            .companies(&env)
            .await?
            .into_iter()
            .map(|id| id.get())
            .collect();
        Some(crate::outbox::CaptureContext::from_env(&env, companies)?)
    } else {
        None
    };
    let shared = Arc::clone(&tx.shared);
    let graph = shared.graph.model(model);

    // One budget for the whole call. The monetary
    // cells the caller supplies are counted before any row buffer is built; precompute charges
    // its own cells against the same budget.
    let budget = crate::monetary::StagingBudget::new(vals_list.len());
    // The record bound holds for every create on a model with a monetary field, whatever the
    // rows supply: its monetary cells may all be precomputed.
    if spec
        .fields
        .iter()
        .any(|f| matches!(f.ty, FieldType::Monetary { .. }))
    {
        budget.check_records()?;
    }
    let supplied_cells = vals_list.iter().try_fold(0usize, |total, vals| {
        let cells = vals
            .iter()
            .filter(|(name, _)| {
                spec.field(name).is_some_and(|field| {
                    matches!(field.ty, FieldType::Monetary { .. })
                        && !graph.compute(field.id).is_some_and(|fc| {
                            fc.precompute() && !fc.editable && field.inverse.is_none()
                        })
                })
            })
            .count();
        total.checked_add(cells)
    });
    match supplied_cells {
        Some(cells) => budget.check(cells)?,
        None => budget.check(usize::MAX)?,
    }

    let n_fields = spec.fields.len();
    let uid_cell = Cell::Id(env.uid());
    let mut rows: Vec<Vec<Option<Cell>>> = Vec::with_capacity(vals_list.len());
    let mut given: Vec<Vec<bool>> = Vec::with_capacity(vals_list.len());
    for vals in &vals_list {
        let mut row: Vec<Option<Cell>> = vec![None; n_fields];
        for (name, cell) in vals.iter() {
            let field = spec.field(name).ok_or_else(|| invalid_field(spec, name))?;
            crate::database_generated::check_assignment(spec, field)?;
            // server-set fields, and `create_date` of a partitioned model (design §7.1, §10)
            if crate::partition::refuses_caller_value(spec, field) {
                return Err(crate::partition::server_set_error(spec, field));
            }
            if name == "id" || (is_magic(name) && spec.log_access) {
                continue;
            }
            // readonly precomputed fields are always computed (COMP §4.9)
            if graph
                .compute(field.id)
                .is_some_and(|fc| fc.precompute() && !fc.editable && field.inverse.is_none())
            {
                continue;
            }
            let validated = if field.translate {
                let active = tx.active_languages().await?;
                crate::translated::apply_write(field, None, cell, env.lang(), &active)?
            } else if matches!(field.ty, FieldType::Html) {
                cell.clone()
            } else {
                validate_cell(fref(spec, field), &field.ty, cell.clone())?
            };
            let validated = tx.admit_html_cell(spec, field, validated)?;
            crate::precision::validate(tx, &env, spec, field, &validated).await?;
            row[field.id.index()] = Some(validated);
        }
        // defaults are already in `vals`: `relcrud::create` asked `default_get` for every field
        // left out (CRUD §4.2 step 3, `defaults::add_missing_defaults`)
        // an editable one-segment related field given at create writes its target (COMP §4.8)
        for field in &spec.fields {
            if let Some(fc) = graph.compute(field.id)
                && let crate::graph::Source::Related(target) = fc.source
                && fc.editable
                && row[target.index()].is_none()
                && let Some(cell) = row[field.id.index()].clone()
            {
                let target_spec = spec.field_by_id(target);
                crate::database_generated::check_assignment(spec, target_spec)?;
                // the target keeps its own HTML policy: a weaker policy on the related field
                // never stores past it (review r1 M2)
                let cell = tx.admit_html_cell(spec, target_spec, cell)?;
                row[target.index()] = Some(cell);
            }
        }
        given.push(row.iter().map(Option::is_some).collect());
        if spec.log_access {
            for name in ["create_uid", "write_uid"] {
                if let Some(f) = spec.field(name) {
                    row[f.id.index()] = Some(uid_cell.clone());
                }
            }
        }
        rows.push(row);
    }

    // A required stored compute that is computed after the INSERT and not given would reach the
    // database as a NOT NULL violation: refuse it now, naming why, before any statement (
    // D7, note).
    refuse_late_required(&registry, spec, graph, &rows)?;

    // Protected per row: groups of given editable computed or inversed fields (COMP §4.6).
    let mut protected: Vec<FxHashSet<FieldId>> = vec![FxHashSet::default(); rows.len()];
    for (r, flags) in given.iter().enumerate() {
        for (i, on) in flags.iter().enumerate() {
            let Some(fs) = spec.fields.get(i) else {
                continue;
            };
            if !*on {
                continue;
            }
            if let Some(fc) = graph.compute(fs.id)
                && (fc.editable || fs.inverse.is_some())
            {
                protected[r].extend(fc.group.iter().copied());
            }
        }
    }
    // Precompute: stored sync computes go into the INSERT.
    let precomputed = precompute_rows(tx, &env, spec, &mut rows, &protected, &budget).await?;
    crate::precision::rows(tx, &env, spec, &rows).await?;
    for (r, set) in precomputed.iter().enumerate() {
        for f in set {
            if let Some(fc) = graph.compute(*f)
                && fc.precompute_declared
            {
                protected[r].extend(fc.group.iter().copied());
            }
        }
    }
    // empty or inverted periods of typed exclusions, before the INSERT
    crate::exclusion::check_rows(spec, &rows)?;

    // Scalar admission (including monetary normalization) is complete. Encode only now.
    tx.encode_create_properties(&env, spec, &mut rows).await?;

    // One column set for the whole call: every column some row gives, NULL where a row gives none
    // (the same as leaving the column out: columns have no database default, schema sync drops the
    // one it uses to fill existing rows). One INSERT shape per batch, ids drawn in input order by
    // the INSERT itself, no id reservation.
    let mut key: Vec<FieldId> = spec
        .column_fields()
        .filter(|f| rows.iter().any(|row| row[f.id.index()].is_some()))
        .map(|f| f.id)
        .collect();
    key.sort_by_key(|f| spec.field_by_id(*f).name);
    let groups: Vec<(Vec<FieldId>, Vec<usize>)> = vec![(key, (0..rows.len()).collect())];
    let mut now_columns: Vec<(&str, &str)> = if spec.log_access {
        ["create_date", "write_date"]
            .into_iter()
            .filter(|n| spec.field(n).is_some())
            .map(|n| (n, NOW_UTC))
            .collect()
    } else {
        Vec::new()
    };
    // server-set fields take the transaction timestamp like `create_date`
    now_columns.extend(
        spec.column_fields()
            .filter(|f| f.server_set)
            .map(|f| (f.name, NOW_UTC)),
    );
    // a new row starts at version 1
    if spec.row_version.is_some() {
        now_columns.push((erp_registry::ROW_VERSION, "1"));
    }
    let batch = tx.config().insert_batch.max(1);

    // The record check fused into the INSERT where the predicate qualifies (decision
    // 11): not when an inverse may rewrite the rows after the INSERT, and never on a column the
    // create writes later (stored computes not given nor precomputed, the parent path).
    let inverse_given = given.iter().zip(&precomputed).any(|(flags, pre)| {
        flags.iter().enumerate().any(|(i, on)| {
            *on && spec
                .fields
                .get(i)
                .is_some_and(|fs| fs.inverse.is_some() && !pre.contains(&fs.id))
        })
    });
    // An editable related field through relations given at create writes its target after the
    // INSERT (COMP §4.8). Only those (`Source::RelatedPath`) can rewrite an input of the new rows
    // after the INSERT: a one-segment editable related field writes its target column into the row
    // before the precompute (above), so the INSERT already carries it; the parent path is written
    // after the INSERT but is always late (`late.extend(spec.parent_path)` below); the sequence
    // hook of `ir.sequence` writes date ranges, not columns of the new rows.
    let related_paths: Vec<FieldId> = graph
        .order
        .iter()
        .copied()
        .filter(|f| {
            graph.compute(*f).is_some_and(|fc| {
                fc.editable && matches!(fc.source, crate::graph::Source::RelatedPath(_))
            })
        })
        .collect();
    let related_given = !related_paths.is_empty()
        && given
            .iter()
            .any(|flags| related_paths.iter().any(|f| flags[f.index()]));
    // Late columns: stored computes some row neither gives (protected) nor computed into the INSERT;
    // and, when an editable related write follows the INSERT, the implicitly precomputed ones too,
    // which that write can mark again (D5). The other steps that write the new rows
    // after the INSERT turn fusing off altogether: an inverse (`inverse_given`, below) and x2many
    // values (`fuse` is false for `AfterInsert::X2many`), so they need no clause here.
    let rewritten_after = related_given;
    let mut late: FxHashSet<FieldId> = graph
        .order
        .iter()
        .copied()
        .filter(|f| {
            graph.compute(*f).is_some_and(|fc| fc.stored)
                && protected
                    .iter()
                    .zip(&precomputed)
                    .any(|(p, pre)| !p.contains(f) && (rewritten_after || !pre.contains(f)))
        })
        .collect();
    late.extend(spec.parent_path);
    let mut fused = None;
    if fuse
        && !inverse_given
        && let Some((fields, _)) = groups.first()
    {
        // the INSERT's own parameters: one array per column, or the row count
        let offset = fields.len().max(1);
        fused = tx.fused_create_check(&env, spec, &late, offset).await?;
    }
    let mut denied: Vec<RealId> = Vec::new();

    let mut ids: Vec<Option<RecId>> = vec![None; rows.len()];
    for (fields, members) in &groups {
        let columns: Vec<(&str, ColumnType)> = fields
            .iter()
            .filter_map(|f| {
                let fs = spec.field_by_id(*f);
                fs.column().map(|c| (fs.name, c))
            })
            .collect();
        // the table's own identity, or the shared sequence it names
        let id_source = spec.id_sequence.map_or(IdSource::Identity, IdSource::Named);
        let sql = match &fused {
            Some(check) => {
                insert_rows_checked(&spec.table, id_source, &columns, &now_columns, &check.sql)
            }
            None => insert_rows(&spec.table, id_source, &columns, &now_columns),
        };
        let check_params = match &fused {
            Some(check) => crate::search::bind(&check.params)?,
            None => Vec::new(),
        };
        for chunk in members.chunks(batch) {
            let mut arrays: Vec<Box<dyn ToSql + Sync + Send>> = Vec::with_capacity(fields.len());
            for f in fields {
                let cells: Vec<Cell> = chunk
                    .iter()
                    .map(|&r| rows[r][f.index()].clone().unwrap_or(Cell::Null))
                    .collect();
                arrays.push(array_param(spec.field_by_id(*f).storage(), cells).map_err(
                    |error| {
                        Error::new(
                            error.kind(),
                            format!(
                                "{}.{}: {}",
                                spec.name,
                                spec.field_by_id(*f).name,
                                error.message()
                            ),
                        )
                    },
                )?);
            }
            if fields.is_empty() {
                arrays.push(Box::new(chunk.len() as i64));
            }
            let params: Vec<&(dyn ToSql + Sync)> = arrays
                .iter()
                .chain(&check_params)
                .map(|a| a.as_ref() as &(dyn ToSql + Sync))
                .collect();
            let result = tx.query(&sql, &params).await?;
            if result.len() != chunk.len() {
                return Err(Error::programming(format!(
                    "INSERT into {} returned {} rows for {}",
                    spec.table,
                    result.len(),
                    chunk.len()
                )));
            }
            for r in &result {
                let raw: i64 = r.try_get(0).map_err(decode_error)?;
                let ord: i64 = r.try_get(1).map_err(decode_error)?;
                let now: DateTime = r.try_get(2).map_err(decode_error)?;
                tx.now = Some(now);
                let position = usize::try_from(ord - 1)
                    .ok()
                    .and_then(|p| chunk.get(p))
                    .copied()
                    .ok_or_else(|| Error::programming(format!("INSERT returned ordinal {ord}")))?;
                let id = RecId::from_real(raw)
                    .ok_or_else(|| Error::programming(format!("INSERT returned id {raw}")))?;
                if fused.is_some() && !r.try_get::<_, bool>(3).map_err(decode_error)? {
                    denied.extend(id.as_real());
                }
                ids[position] = Some(id);
            }
        }
    }
    let ids: Vec<RecId> = ids
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Error::programming("INSERT did not return an id for every row"))?;
    // a created record never reports tracked changes (D2)
    if !spec.tracked.is_empty() {
        let depth = tx.savepoint_depth();
        tx.tracking.created(depth, model, &ids);
    }

    if let Some(context) = capture {
        let depth = tx.savepoint_depth();
        for (fields, members) in &groups {
            let mut names: Vec<String> = fields
                .iter()
                .map(|field| spec.field_by_id(*field).name.to_owned())
                .chain(now_columns.iter().map(|(name, _)| (*name).to_owned()))
                .collect();
            names.sort_unstable();
            names.dedup();
            for chunk in members.chunks(crate::outbox::MAX_ID_ENTRIES) {
                let record_ids = chunk
                    .iter()
                    .map(|&index| {
                        // Every id above was decoded as a real positive INSERT result.
                        ids[index]
                            .as_real()
                            .map(|id| crate::outbox::IdEntry::Single(id.get()))
                            .ok_or_else(|| {
                                Error::programming("outbox: INSERT returned a non-real id")
                            })
                    })
                    .collect::<Result<Vec<_>>>();
                let result = record_ids
                    .and_then(|record_ids| {
                        context.envelope(
                            crate::outbox::Kind::Create,
                            spec.name,
                            record_ids,
                            names.clone(),
                            Vec::new(),
                        )
                    })
                    .and_then(|envelope| tx.outbox.push(envelope, depth));
                if let Err(error) = result {
                    // Base dispatch can reach this function without Records::create's wrapper.
                    tx.poison_with(&error);
                    return Err(error);
                }
            }
        }
    }

    // Seed the cache: given values, NULL for stored fields not inserted, audit values.
    let now = tx.now;
    let cache = tx.cache.model_mut(model);
    for field in &spec.fields {
        if field.is_generated() {
            continue;
        }
        let i = field.id.index();
        let storage = field.storage();
        let is_now = (spec.log_access && matches!(field.name, "create_date" | "write_date"))
            || (field.server_set && field.has_column());
        let is_version = spec.row_version == Some(field.id);
        if !(field.has_column() || rows.iter().any(|r| r[i].is_some())) {
            continue;
        }
        for (row, id) in rows.iter().zip(&ids) {
            let cell = if is_version {
                Cell::Id(1)
            } else if is_now {
                match now {
                    Some(n) => Cell::DateTime(n),
                    None => continue,
                }
            } else {
                row[i].clone().unwrap_or(Cell::Null)
            };
            cache.set(field.id, storage, *id, cell, false)?;
        }
    }
    let ids_arc: crate::records::Ids = ids.clone().into();
    let created = Records::from_parts(env.clone(), model, Arc::clone(&ids_arc), ids_arc);
    // the new rows' x2many values and the one2many values of their many2one targets, before the
    // constraints, related writes and inverses below read them or change what they reach
    crate::relcrud::cache_created_relations(tx, &created).await?;
    // parent paths before anything reads them (constraints below, CRUD §4.11)
    tx.parent_store_create(&created).await?;

    // Stored computed fields that were neither given nor precomputed are computed later
    // (`modified(create=True)`, COMP §4.4).
    let mut pending: Vec<(FieldId, Vec<RecId>)> = Vec::new();
    for &f in &graph.order {
        let Some(fc) = graph.compute(f) else { continue };
        if !fc.stored {
            continue;
        }
        let todo: Vec<RecId> = ids
            .iter()
            .enumerate()
            .filter(|(r, _)| !protected[*r].contains(&f) && !precomputed[*r].contains(&f))
            .map(|(_, id)| *id)
            .collect();
        if !todo.is_empty() {
            pending.push((f, todo));
        }
    }
    for (f, todo) in pending {
        let storage = spec.field_by_id(f).storage();
        tx.bind_capture(model, f, &todo)?;
        tx.cache.model_mut(model).mark_pending(f, storage, &todo);
    }

    // Python constraints of the stored fields given or precomputed.
    let mut stored_names: FxHashSet<FieldId> = FxHashSet::default();
    let mut inversed: Vec<(&'static str, Vec<FieldId>, Vec<RecId>)> = Vec::new();
    for (r, flags) in given.iter().enumerate() {
        for (i, on) in flags.iter().enumerate() {
            let Some(fs) = spec.fields.get(i) else {
                continue;
            };
            if *on && fs.store {
                stored_names.insert(fs.id);
            }
            if *on
                && let Some(method) = fs.inverse
                && !precomputed[r].contains(&fs.id)
            {
                match inversed.iter_mut().find(|(m, _, _)| *m == method) {
                    Some((_, fields, recs)) => {
                        if !fields.contains(&fs.id) {
                            fields.push(fs.id);
                        }
                        if !recs.contains(&ids[r]) {
                            recs.push(ids[r]);
                        }
                    }
                    None => inversed.push((method, vec![fs.id], vec![ids[r]])),
                }
            }
        }
        stored_names.extend(precomputed[r].iter().copied());
    }
    tx.run_constraints(&created, &stored_names, postponed)
        .await?;

    // Editable related fields through relations given at create write their targets (COMP §4.8),
    // under protection of the given groups.
    let mut related_writes: Vec<(crate::graph::Path, Cell, Vec<RecId>)> = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        for field in &spec.fields {
            let Some(fc) = graph.compute(field.id) else {
                continue;
            };
            let crate::graph::Source::RelatedPath(path) = &fc.source else {
                continue;
            };
            if !fc.editable || !given[r][field.id.index()] {
                continue;
            }
            let Some(cell) = row[field.id.index()].clone() else {
                continue;
            };
            match related_writes
                .iter_mut()
                .find(|(p, c, _)| Arc::ptr_eq(p, path) && *c == cell)
            {
                Some((_, _, recs)) => recs.push(ids[r]),
                None => related_writes.push((Arc::clone(path), cell, vec![ids[r]])),
            }
        }
    }
    // The related writes and the inverse methods, under protection of the given and precomputed
    // groups (COMP §4.7, §4.8): now, or after the caller's x2many writes ([`AfterInsert`]).
    let after_create = AfterCreate {
        ids,
        protected,
        related_writes,
        inversed,
        skip: stored_names.union(postponed).copied().collect(),
    };
    let after_create = match after {
        AfterInsert::Nothing => {
            after_create.run(tx, &created).await?;
            None
        }
        AfterInsert::X2many(_) => Some(after_create),
    };
    if spec.name == "ir.sequence" || spec.name == "ir.sequence.date_range" {
        crate::sequence::on_records_created(tx, &created).await?;
    }
    if spec.name == "res.lang" {
        tx.active_langs = None;
        tx.langs_dirty = true;
    }
    let check = if fused.is_some() {
        CreateCheck::Fused(denied)
    } else {
        CreateCheck::After
    };
    Ok((created, check, after_create))
}

/// Refuse rows that give no value for a required stored compute computed after the INSERT
/// : the INSERT would fail on NOT NULL, so the error is raised before any statement,
/// with the text of that violation and the reason the field is late.
///
/// # Errors
///
/// `Validation` naming the first such field, its model and why it is computed after the INSERT.
fn refuse_late_required(
    registry: &erp_registry::Registry,
    spec: &ModelSpec,
    graph: &crate::graph::ModelGraph,
    rows: &[Vec<Option<Cell>>],
) -> Result<()> {
    for &f in &graph.order {
        let Some(fc) = graph.compute(f) else { continue };
        let Some(late) = &fc.late else { continue };
        let field = spec.field_by_id(f);
        if !(fc.stored && field.required && field.has_column()) {
            continue;
        }
        if rows.iter().any(|row| row[f.index()].is_none()) {
            return Err(Error::validation(format!(
                "{}Missing required field '{}' ({}) for model '{}' ({}): it is computed after the \
                 INSERT because {}",
                crate::dberror::PREFIX,
                field.string,
                field.name,
                spec.description,
                spec.name,
                late.describe(registry, spec, graph, f)?
            )));
        }
    }
    Ok(())
}

/// Push one protection scope per distinct protected field set of the created rows.
fn protect_rows(
    tx: &mut Transaction,
    model: ModelId,
    ids: &[RecId],
    protected: &[FxHashSet<FieldId>],
) -> usize {
    let mut by_set: Vec<(Vec<FieldId>, Vec<RecId>)> = Vec::new();
    for (id, set) in ids.iter().zip(protected) {
        if set.is_empty() {
            continue;
        }
        let mut key: Vec<FieldId> = set.iter().copied().collect();
        key.sort();
        match by_set.iter_mut().find(|(k, _)| *k == key) {
            Some((_, recs)) => recs.push(*id),
            None => by_set.push((key, vec![*id])),
        }
    }
    let mut depth = None;
    for (fields, recs) in &by_set {
        let d = tx.protect(model, fields, recs);
        depth.get_or_insert(d);
    }
    depth.unwrap_or_else(|| tx.protect(model, &[], &[]))
}

/// Tracks supplied monetary cells still waiting for a currency prerequisite. Its staging
/// buffers and `pending` mask are as wide as `slots` ([`crate::monetary::staging_slots`] of the
/// model's monetary fields), never as wide as the model.
struct PendingMonetary<'a> {
    spec: &'a ModelSpec,
    slots: &'a [FieldId],
    rows: &'a mut [Vec<Option<Cell>>],
    /// `pending[row][k]`: the cell of `slots[k]` was supplied and is not normalized yet.
    pending: &'a mut [Vec<bool>],
    /// `charged[row][k]`: the cell of `slots[k]` was already charged to the budget in an earlier
    /// round, so a retry round (currency still pending) does not charge it again.
    charged: &'a mut [Vec<bool>],
    budget: &'a crate::monetary::StagingBudget,
}

impl PendingMonetary<'_> {
    fn slot_of(&self, field: FieldId) -> Result<usize> {
        self.slots.binary_search(&field).map_err(|_| {
            Error::programming(format!(
                "Monetary create staging has no slot for {}.{}",
                self.spec.name,
                self.spec.field_by_id(field).name
            ))
        })
    }

    async fn normalize(
        &mut self,
        tx: &mut Transaction,
        env: &Env,
        members: &[usize],
        fields: &[FieldId],
        required: bool,
    ) -> Result<()> {
        let mut monetary_slots = Vec::new();
        for field in fields {
            if matches!(self.spec.field_by_id(*field).ty, FieldType::Monetary { .. }) {
                monetary_slots.push(self.slot_of(*field)?);
            }
        }
        let members: Vec<usize> = members
            .iter()
            .copied()
            .filter(|r| monetary_slots.iter().any(|slot| self.pending[*r][*slot]))
            .collect();
        if members.is_empty() {
            return Ok(());
        }
        let mut rows: Vec<Vec<Option<Cell>>> = members
            .iter()
            .map(|r| {
                self.slots
                    .iter()
                    .map(|f| self.rows[*r][f.index()].clone())
                    .collect()
            })
            .collect();
        let mask: Vec<_> = members.iter().map(|r| self.pending[*r].clone()).collect();
        let precharged: Vec<Vec<bool>> = members.iter().map(|r| self.charged[*r].clone()).collect();
        let completed = crate::monetary::normalize(
            tx,
            env,
            crate::monetary::Stage {
                initial: false,
                spec: self.spec,
                ids: None,
                fields: self.slots,
                rows: &mut rows,
                included: Some(&mask),
                budget: self.budget,
                precharged: Some(&precharged),
                kind: crate::monetary::StageKind::Direct,
            },
            fields,
            false,
            required,
        )
        .await?;
        // every pending cell of a requested monetary field was charged by this round
        for (local, &r) in members.iter().enumerate() {
            for &slot in &monetary_slots {
                if self.pending[r][slot] && rows[local][slot].is_some() {
                    self.charged[r][slot] = true;
                }
            }
        }
        for (local, field) in completed {
            let r = members[local];
            let slot = self.slot_of(field)?;
            self.rows[r][field.index()] = rows[local][slot].take();
            self.pending[r][slot] = false;
        }
        Ok(())
    }
}

/// Compute the precomputed fields of rows about to be inserted, in dependency order, one batch
/// per compute method over every row that needs it (COMP §4.9). Returns the stored
/// fields computed per row. Values of non-stored fields computed on the way are only inputs and
/// are removed from the rows.
async fn precompute_rows(
    tx: &mut Transaction,
    env: &Env,
    spec: &ModelSpec,
    rows: &mut [Vec<Option<Cell>>],
    protected: &[FxHashSet<FieldId>],
    budget: &crate::monetary::StagingBudget,
) -> Result<Vec<FxHashSet<FieldId>>> {
    let shared = Arc::clone(&tx.shared);
    let graph = shared.graph.model(spec.id);
    let mut done: Vec<FxHashSet<FieldId>> = vec![FxHashSet::default(); rows.len()];
    let monetary: Vec<FieldId> = spec
        .fields
        .iter()
        .filter(|f| matches!(f.ty, FieldType::Monetary { .. }))
        .map(|f| f.id)
        .collect();
    let slots = crate::monetary::staging_slots(spec, graph, &monetary);
    // `pending[row][k]`: the cell of `slots[k]` is supplied and not yet normalized.
    let mut pending: Vec<Vec<bool>> = rows
        .iter()
        .map(|row| slots.iter().map(|f| row[f.index()].is_some()).collect())
        .collect();
    let mut charged: Vec<Vec<bool>> = vec![vec![false; slots.len()]; rows.len()];
    let all_rows: Vec<usize> = (0..rows.len()).collect();
    PendingMonetary {
        spec,
        slots: &slots,
        rows,
        pending: &mut pending,
        charged: &mut charged,
        budget,
    }
    .normalize(tx, env, &all_rows, &monetary, false)
    .await?;
    // what each row needs: precomputed stored fields not given, plus the computed fields they read
    let mut need: Vec<FxHashSet<FieldId>> = vec![FxHashSet::default(); rows.len()];
    for (r, row) in rows.iter().enumerate() {
        // A nonstored currency is still a prerequisite of an explicitly supplied amount.
        for &f in &monetary {
            if row[f.index()]
                .as_ref()
                .is_some_and(crate::monetary::needs_currency)
                && let FieldType::Monetary {
                    currency_field: Some(name),
                } = &spec.field_by_id(f).ty
                && let Some(currency) = spec.field_id(name)
                && graph.compute(currency).is_some()
                && row[currency.index()].is_none()
            {
                need[r].insert(currency);
            }
        }
        for &f in graph.order.iter().rev() {
            let Some(fc) = graph.compute(f) else { continue };
            let wanted =
                (fc.precompute() && row[f.index()].is_none() && !protected[r].contains(&f))
                    || need[r].contains(&f);
            if !wanted {
                continue;
            }
            need[r].insert(f);
            for d in &fc.readable {
                if !fc.group.contains(d) && graph.compute(*d).is_some() && row[d.index()].is_none()
                {
                    need[r].insert(*d);
                }
            }
        }
    }
    // The monetary cells precompute will stage are known before any compute runs: refuse an
    // operation they push over a bound first. The supplied cells were charged above; the computed
    // ones are charged again as their group is normalized below, with the same count.
    if !monetary.is_empty() {
        let mut counted: FxHashSet<&'static str> = FxHashSet::default();
        let mut computed_cells = 0usize;
        for &f in &graph.order {
            let Some(fc) = graph.compute(f) else { continue };
            if !fc.method.is_empty() && !counted.insert(fc.method) {
                continue;
            }
            for g in fc.group.iter().filter(|g| monetary.contains(g)) {
                computed_cells = computed_cells.saturating_add(
                    (0..rows.len())
                        .filter(|r| fc.group.iter().any(|m| need[*r].contains(m)))
                        .filter(|r| rows[*r][g.index()].is_none() && !protected[*r].contains(g))
                        .count(),
                );
            }
        }
        budget.check(computed_cells)?;
    }
    let mut row_ids: Vec<Option<RecId>> = vec![None; rows.len()];
    let mut handled: FxHashSet<&'static str> = FxHashSet::default();
    let mut non_stored: FxHashSet<FieldId> = FxHashSet::default();
    for &f in &graph.order {
        let Some(fc) = graph.compute(f) else { continue };
        if !fc.method.is_empty() && !handled.insert(fc.method) {
            continue;
        }
        let all_members: Vec<usize> = (0..rows.len())
            .filter(|r| fc.group.iter().any(|g| need[*r].contains(g)))
            .collect();
        if all_members.is_empty() {
            continue;
        }
        // A candidate compute (read-only view) runs here on the synthetic ids, so a
        // supplied amount is normalized before the INSERT; any other async compute cannot.
        if fc.is_async && !fc.candidate {
            // A supplied amount can demand a late async currency prerequisite even
            // though ordinary async computes are never precomputed. Refuse it before
            // INSERT, naming the flag that lets the compute join admission.
            if monetary.iter().any(|amount| {
                rows.iter().any(|row| {
                    row[amount.index()]
                        .as_ref()
                        .is_some_and(crate::monetary::needs_currency)
                })
            }) {
                return Err(crate::monetary::async_currency_refusal(
                    "resolve",
                    spec.name,
                    spec.field_by_id(f).name,
                    "before create",
                ));
            }
            return Err(Error::programming(format!(
                "{}.{}: an async compute cannot run before the INSERT",
                spec.name,
                spec.field_by_id(f).name
            )));
        }
        // bounded batches
        for chunk in all_members.chunks(tx.config().recompute_batch.max(1)) {
            let members: Vec<usize> = chunk.to_vec();
            let mut ids: Vec<RecId> = Vec::with_capacity(members.len());
            for &r in &members {
                let id = match row_ids[r] {
                    Some(id) => id,
                    None => {
                        let id = tx.next_new_id()?;
                        row_ids[r] = Some(id);
                        id
                    }
                };
                ids.push(id);
            }
            let run_env = if fc.sudo {
                env.engine_sudo()
            } else {
                env.clone()
            };
            // Consumers must never receive an unnormalized monetary input.
            PendingMonetary {
                spec,
                slots: &slots,
                rows,
                pending: &mut pending,
                charged: &mut charged,
                budget,
            }
            .normalize(
                tx,
                env,
                &members,
                &fc.readable
                    .iter()
                    .copied()
                    .filter(|f| !fc.group.contains(f))
                    .collect::<Vec<_>>(),
                true,
            )
            .await?;
            let mut out = crate::compute::Outputs::new(
                run_env.clone(),
                spec.id,
                fc.method,
                &fc.group,
                ids.len(),
            );
            match &fc.source {
                crate::graph::Source::Related(target) => {
                    let name = spec.field_by_id(f).name;
                    for (i, &r) in members.iter().enumerate() {
                        let cell = rows[r][target.index()].clone().unwrap_or(Cell::Null);
                        out.assign(i, name, cell)?;
                    }
                }
                // through many2one fields only (the graph checks it): their targets exist
                crate::graph::Source::RelatedPath(path) => {
                    let name = spec.field_by_id(f).name;
                    let first = path[0].1;
                    let starts: Vec<Option<RecId>> = members
                        .iter()
                        .map(|&r| {
                            rows[r][first.index()]
                                .as_ref()
                                .and_then(crate::relational::id_of)
                        })
                        .collect();
                    let cells = tx.related_from(&run_env, &path[1..], starts, None).await?;
                    for (i, cell) in cells.into_iter().enumerate() {
                        out.assign(i, name, cell)?;
                    }
                }
                // never precomputed (computed after the INSERT)
                crate::graph::Source::DisplayName => {}
                crate::graph::Source::Chain(chain) => {
                    let mut inputs: Vec<Option<Vec<Cell>>> =
                        (0..spec.fields.len()).map(|_| None).collect();
                    for &r in &fc.readable {
                        inputs[r.index()] = Some(
                            members
                                .iter()
                                .map(|&m| rows[m][r.index()].clone().unwrap_or(Cell::Null))
                                .collect(),
                        );
                    }
                    let ids: Arc<[RecId]> = Arc::from(ids);
                    if fc.candidate {
                        let batch_records = Records::from_parts(
                            run_env.clone(),
                            spec.id,
                            Arc::clone(&ids),
                            Arc::clone(&ids),
                        );
                        let columns: Vec<(FieldId, Vec<Cell>)> = fc
                            .readable
                            .iter()
                            .map(|&r| {
                                inputs[r.index()]
                                    .clone()
                                    .map(|column| (r, column))
                                    .ok_or_else(|| {
                                        Error::programming(format!(
                                            "Unresolved candidate compute input {}.{}",
                                            spec.name,
                                            spec.field_by_id(r).name
                                        ))
                                    })
                            })
                            .collect::<Result<_>>()?;
                        let scope =
                            crate::candidate::ReadScope::staged(spec.id, Arc::clone(&ids), columns);
                        crate::compute::run_candidate(chain, tx, &batch_records, &mut out, scope)
                            .await?;
                    } else {
                        let related = if fc.paths.is_empty() && fc.read_paths.is_empty() {
                            Vec::new()
                        } else {
                            let batch_records = Records::from_parts(
                                run_env.clone(),
                                spec.id,
                                Arc::clone(&ids),
                                Arc::clone(&ids),
                            );
                            tx.path_inputs(&batch_records, fc, &inputs).await?
                        };
                        let mut data =
                            crate::compute::BatchData::new(ids, inputs, out).with_related(related);
                        crate::compute::run_sync(chain, 0, &mut data)?;
                        out = data.out;
                    }
                }
            }
            let monetary_fields: Vec<FieldId> = fc
                .group
                .iter()
                .copied()
                .filter(|&f| matches!(spec.field_by_id(f).ty, FieldType::Monetary { .. }))
                .collect();
            // Compact buffers: only the slots a monetary group member can read or resolve.
            let compute_slots = if monetary_fields.is_empty() {
                Vec::new()
            } else {
                crate::monetary::staging_slots(spec, graph, &monetary_fields)
            };
            let mut computed_rows: Vec<Vec<Option<Cell>>> = members
                .iter()
                .map(|r| {
                    compute_slots
                        .iter()
                        .map(|f| rows[*r][f.index()].clone())
                        .collect()
                })
                .collect();
            let mut computed_mask = vec![vec![false; compute_slots.len()]; members.len()];
            let mut computed_values: Vec<(usize, usize, FieldId, Cell)> = Vec::new();
            out.admit_html(tx)?;
            for &g in fc.group.iter() {
                let fs = spec.field_by_id(g);
                let values = out.take(g);
                for (i, &r) in members.iter().enumerate() {
                    if rows[r][g.index()].is_some() || protected[r].contains(&g) {
                        continue;
                    }
                    let value = values.get(i).cloned().flatten();
                    if value.is_none() && !fc.stored && fs.readonly {
                        return Err(Error::programming(format!(
                            "Compute method failed to assign {}(<new>,).{}",
                            spec.name, fs.name
                        )));
                    }
                    let value = value.unwrap_or(Cell::Null);
                    crate::precision::validate(tx, &run_env, spec, fs, &value).await?;
                    if let Ok(slot) = compute_slots.binary_search(&g) {
                        computed_rows[i][slot] = Some(value.clone());
                        computed_mask[i][slot] = true;
                    }
                    computed_values.push((i, r, g, value));
                    if fc.stored {
                        done[r].insert(g);
                    } else {
                        non_stored.insert(g);
                    }
                }
            }
            if !monetary_fields.is_empty() {
                crate::monetary::normalize(
                    tx,
                    &run_env,
                    crate::monetary::Stage {
                        initial: false,
                        spec,
                        ids: None,
                        fields: &compute_slots,
                        rows: &mut computed_rows,
                        included: Some(&computed_mask),
                        budget,
                        precharged: None,
                        kind: crate::monetary::StageKind::Direct,
                    },
                    &monetary_fields,
                    false,
                    true,
                )
                .await?;
            }
            for (i, r, g, value) in computed_values {
                // A slot holds the normalized value; a field outside the slot list is not a
                // monetary one (those are always slots) and keeps what the compute assigned.
                rows[r][g.index()] = Some(match compute_slots.binary_search(&g) {
                    Ok(slot) => computed_rows[i][slot].take().ok_or_else(|| {
                        Error::programming("Monetary precompute staging lost an output")
                    })?,
                    Err(_) if monetary_fields.contains(&g) => {
                        return Err(Error::programming(
                            "Monetary precompute staging has no slot for a computed amount",
                        ));
                    }
                    Err(_) => value,
                });
            }
        }
    }
    PendingMonetary {
        spec,
        slots: &slots,
        rows,
        pending: &mut pending,
        charged: &mut charged,
        budget,
    }
    .normalize(tx, env, &all_rows, &monetary, true)
    .await?;
    for row in rows.iter_mut() {
        for g in &non_stored {
            row[g.index()] = None;
        }
    }
    Ok(done)
}

/// `base.write`: validate, then update the cache, mark stored values dirty, notify dependents,
/// run constraints and inverse methods (COMP §4.6–4.7, CRUD §4.4). `postponed` as for
/// [`create_records`].
pub(crate) async fn write_records(
    tx: &mut Transaction,
    records: &Records,
    vals: RawVals,
    postponed: &FxHashSet<FieldId>,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    tx.admit_records(records).await?;
    let env = records.env().clone();
    let model = records.model_id();
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(model);
    tx.check_model(&env, model, Operation::Write)?;
    let shared = Arc::clone(&tx.shared);
    let graph = shared.graph.model(model);
    let property_input = vals.iter().any(|(name, _)| {
        spec.field(name)
            .is_some_and(|field| field.company_dependent)
    });
    tx.property_write_origin(records, property_input).await?;
    let mut items: Vec<(FieldId, Cell, bool)> = Vec::new();
    // editable related fields through relations: written on their targets (COMP §4.8)
    let mut related_writes: Vec<(crate::graph::Path, Cell)> = Vec::new();
    // new records may point at other new records (ENV §4.9)
    let all_new = records.rec_ids().iter().all(|i| i.is_new());
    for (name, cell) in vals.iter() {
        let field = spec.field(name).ok_or_else(|| invalid_field(spec, name))?;
        crate::database_generated::check_assignment(spec, field)?;
        // records in the database only: a new record's value never reaches a create
        // (`convert_to_write` leaves server-set fields out and its audit values stay dropped)
        if !all_new && crate::partition::refuses_caller_value(spec, field) {
            return Err(crate::partition::server_set_error(spec, field));
        }
        if name == "id" || (is_magic(name) && spec.log_access) {
            continue;
        }
        let mut cell = if field.translate {
            cell.clone()
        } else if matches!(field.ty, FieldType::Html) {
            tx.admit_html_cell(spec, field, cell.clone())?
        } else if all_new {
            crate::newrec::new_record_cell(spec, field, cell.clone(), tx)?
        } else {
            validate_cell(fref(spec, field), &field.ty, cell.clone())?
        };
        if let Some(fc) = graph.compute(field.id)
            && let crate::graph::Source::RelatedPath(path) = &fc.source
            && fc.editable
        {
            if let Some((owner, target)) = path.last() {
                let owner = registry.model_by_id(*owner);
                crate::database_generated::check_assignment(owner, owner.field_by_id(*target))?;
            }
            related_writes.push((Arc::clone(path), cell.clone()));
        }
        // an editable one-segment related field writes its target (COMP §4.8)
        if let Some(fc) = graph.compute(field.id)
            && let crate::graph::Source::Related(target) = fc.source
            && fc.editable
            && !items.iter().any(|(f, _, _)| *f == target)
        {
            let target_spec = spec.field_by_id(target);
            crate::database_generated::check_assignment(spec, target_spec)?;
            // the target keeps its own HTML policy (review r1 M2); the related field's cache
            // holds the stored value, so a read through it shows what was saved
            if matches!(target_spec.ty, FieldType::Html) && !target_spec.translate {
                cell = tx.admit_html_cell(spec, target_spec, cell)?;
            }
            items.push((target, cell.clone(), false));
        }
        items.push((field.id, cell, false));
    }
    for (target, cell, _) in &items {
        crate::precision::validate(tx, &env, spec, spec.field_by_id(*target), cell).await?;
    }
    // write_uid is always rewritten, so the UPDATE happens even when nothing else changed
    if spec.log_access
        && let Some(f) = spec.field("write_uid")
    {
        items.push((f.id, Cell::Id(env.uid()), true));
    }
    items.sort_by_key(|(fid, _, _)| (write_sequence(&spec.field_by_id(*fid).ty), *fid));
    // One budget for the whole call. The monetary cells this write stages are known now: refuse
    // an oversized request before any recompute, protection or staging buffer exists.
    let budget = crate::monetary::StagingBudget::new(records.len());
    let monetary_items = items
        .iter()
        .filter(|(f, _, _)| matches!(spec.field_by_id(*f).ty, FieldType::Monetary { .. }))
        .count();
    budget.check_grid(records.len(), monetary_items)?;
    // empty or inverted periods of typed exclusions, before anything is written
    crate::exclusion::check_write(tx, spec, records.rec_ids(), &items)?;

    // Fields with an inverse and editable computed fields protect their group (COMP §4.6).
    let mut protected: Vec<FieldId> = Vec::new();
    let mut inverses: Vec<(&'static str, Vec<FieldId>)> = Vec::new();
    for (fid, _, _) in &items {
        let fs = spec.field_by_id(*fid);
        let fc = graph.compute(*fid);
        if let Some(method) = fs.inverse {
            match inverses.iter_mut().find(|(m, _)| *m == method) {
                Some((_, fields)) => fields.push(*fid),
                None => inverses.push((method, vec![*fid])),
            }
        }
        if fs.inverse.is_some() || fc.is_some_and(|c| c.editable) {
            for g in fc.map_or_else(|| vec![*fid], |c| c.group.to_vec()) {
                if !protected.contains(&g) {
                    protected.push(g);
                }
            }
        }
    }
    // a pending recompute of a sibling in those groups is not lost
    let siblings: Vec<FieldId> = protected
        .iter()
        .copied()
        .filter(|f| {
            graph.compute(*f).is_some_and(|c| c.stored) && !items.iter().any(|(i, _, _)| i == f)
        })
        .collect();
    if !siblings.is_empty() {
        tx.recompute_fields(&env, model, &siblings).await?;
    }
    let inversed: FxHashSet<FieldId> = inverses
        .iter()
        .flat_map(|(_, fields)| fields.iter().copied())
        .collect();
    let guard = tx.guard();
    let depth = tx.protect(model, &protected, records.rec_ids());
    let skip: FxHashSet<FieldId> = inversed.union(postponed).copied().collect();
    let mut result = write_protected(tx, records, items, &inverses, &skip, &budget).await;
    for (path, cell) in related_writes {
        if result.is_err() {
            break;
        }
        result = tx.write_related_targets(records, &path, cell).await;
    }
    tx.unprotect_to(depth);
    guard.disarm();
    result?;
    tx.run_constraints(records, &inversed, postponed).await?;
    if spec.name == "ir.sequence" || spec.name == "ir.sequence.date_range" {
        crate::sequence::on_records_written(tx, records, &vals).await?;
    }
    if spec.name == "res.lang" {
        tx.active_langs = None;
        tx.langs_dirty = true;
    }
    Ok(())
}

async fn write_protected(
    tx: &mut Transaction,
    records: &Records,
    items: Vec<(FieldId, Cell, bool)>,
    inverses: &[(&'static str, Vec<FieldId>)],
    inversed: &FxHashSet<FieldId>,
    budget: &crate::monetary::StagingBudget,
) -> Result<()> {
    let env = records.env().clone();
    let model = records.model_id();
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(model);
    let monetary: Vec<FieldId> = items
        .iter()
        .map(|(f, _, _)| *f)
        .filter(|f| matches!(spec.field_by_id(*f).ty, FieldType::Monetary { .. }))
        .collect();
    let mut staged = Vec::new();
    let mut candidates: Vec<crate::monetary::StagedCandidate> = Vec::new();
    let mut slots: Vec<FieldId> = Vec::new();
    if !monetary.is_empty() {
        let shared = Arc::clone(&tx.shared);
        slots = crate::monetary::staging_slots(spec, shared.graph.model(model), &monetary);
        staged = vec![vec![None; slots.len()]; records.len()];
        for (field, cell, _) in &items {
            // an item outside the slot list can neither be normalized nor read by a projection
            if let Ok(slot) = slots.binary_search(field) {
                for row in &mut staged {
                    row[slot] = Some(cell.clone());
                }
            }
        }
        // Candidate computes that admission runs are collected here and published
        // with the write below, so each runs once per accepted mutation.
        let mut scope = crate::monetary::CandidateScope::open(tx);
        let normalized = crate::monetary::normalize(
            &mut scope,
            &env,
            crate::monetary::Stage {
                initial: false,
                spec,
                ids: Some(records.rec_ids()),
                fields: &slots,
                rows: &mut staged,
                included: None,
                budget,
                precharged: None,
                kind: crate::monetary::StageKind::Direct,
            },
            &monetary,
            true,
            true,
        )
        .await;
        let collected = scope.finish();
        normalized?;
        candidates = collected.ok_or_else(|| {
            Error::programming("the candidate result collector vanished during admission")
        })?;
    }
    let mut changes: Vec<(FieldId, Vec<RecId>)> = Vec::with_capacity(items.len());
    let mut names: FxHashSet<FieldId> = FxHashSet::default();
    for (fid, cell, force) in items {
        names.insert(fid);
        let field = spec.field_by_id(fid);
        let storage = field.storage();
        let stored = field.has_column();
        let context_slot = match tx.slot(&env, model, fid)? {
            crate::engine::Slot::Plain => None,
            slot => Some(slot),
        };
        // writing a field cancels its pending recomputation (COMP §4.6)
        tx.cache
            .model_mut(model)
            .clear_pending(fid, records.rec_ids());
        let mut changed = Vec::new();
        if field.company_dependent {
            // drafts keep their map in the cache only: never dirty, no baseline
            crate::company_dependent_runtime::refuse_draft_link(spec, field, &cell)?;
            tx.ensure(records, fid).await?;
            let (company, fallback) = tx.property_context(&env, model, fid)?;
            for (i, id) in records.rec_ids().iter().enumerate() {
                let scalar = if monetary.contains(&fid) {
                    let slot = slots.binary_search(&fid).map_err(|_| {
                        Error::programming("Monetary property staging lost a field")
                    })?;
                    staged[i][slot].as_ref().ok_or_else(|| {
                        Error::programming("Monetary property staging lost an input")
                    })?
                } else {
                    &cell
                };
                // a draft may hold no value yet for a required field: checked at save (onchange)
                if !id.is_new() {
                    crate::company_dependent_runtime::check_required(spec, field, scalar)?;
                }
                let old = tx.cache.get(model, fid, *id).ok_or_else(|| {
                    Error::programming("Company-dependent write lost its loaded map")
                })?;
                let new =
                    crate::company_dependent::apply(&field.ty, &old, company, scalar, &fallback)?;
                if !force && old.same_value(&new) {
                    continue;
                }
                if !id.is_new() {
                    tx.properties_mut()
                        .baselines
                        .entry((model, fid, *id))
                        .or_insert(old);
                }
                tx.cache
                    .model_mut(model)
                    .set(fid, storage, *id, new, stored && !id.is_new())?;
                // a cell is dirty from here on: an error in a later field must poison the
                // transaction (`poison_after`), not leave an unbound dirty cell
                tx.note_mutation();
                changed.push(*id);
            }
        } else if field.translate {
            let active = tx.active_languages().await?;
            if !records.rec_ids().iter().all(|id| id.is_new()) && stored {
                tx.ensure(records, fid).await?;
            }
            for id in records.rec_ids() {
                let cache = tx.cache.model_mut(model);
                let old_cell = cache.get(fid, *id);
                let new_json = crate::translated::apply_write(
                    field,
                    old_cell.as_ref(),
                    &cell,
                    env.lang(),
                    &active,
                )?;
                let new_json = tx.admit_html_cell(spec, field, new_json)?;
                if !force
                    && old_cell
                        .as_ref()
                        .is_some_and(|cur| cur.same_value(&new_json))
                {
                    continue;
                }
                if stored
                    && !id.is_new()
                    && !tx.translated_baselines.contains_key(&(model, fid, *id))
                {
                    tx.translated_baselines
                        .insert((model, fid, *id), old_cell.clone().unwrap_or(Cell::Null));
                }
                tx.cache.model_mut(model).set(
                    fid,
                    storage,
                    *id,
                    new_json,
                    stored && !id.is_new(),
                )?;
                tx.note_mutation();
                changed.push(*id);
            }
        } else {
            for (i, id) in records.rec_ids().iter().enumerate() {
                let cell = if monetary.contains(&fid) {
                    slots
                        .binary_search(&fid)
                        .ok()
                        .and_then(|slot| staged[i][slot].as_ref())
                        .ok_or_else(|| Error::programming("Monetary write staging lost an input"))?
                } else {
                    &cell
                };
                if let Some(slot) = &context_slot {
                    tx.slot_set(slot, model, fid, [(*id, cell.clone())])?;
                    tx.note_mutation();
                    changed.push(*id);
                    continue;
                }
                let cache = tx.cache.model_mut(model);
                // Skip records whose cached value already equals the new one; their
                // dependents are not notified either.
                if !force && cache.get(fid, *id).is_some_and(|cur| cur.same_value(cell)) {
                    continue;
                }
                cache.set(fid, storage, *id, cell.clone(), stored && !id.is_new())?;
                tx.note_mutation();
                changed.push(*id);
            }
        }
        changes.push((fid, changed));
    }
    if changes.iter().any(|(_, ids)| !ids.is_empty()) {
        tx.note_mutation();
    }
    // a pending write on a database record may be read by a record rule; one on an in-memory
    // record is not (its decisions come from its origin's row)
    let database_fields: Vec<FieldId> = changes
        .iter()
        .filter(|(_, ids)| ids.iter().any(|id| !id.is_new()))
        .map(|(f, _)| *f)
        .collect();
    if !database_fields.is_empty() {
        tx.note_database_change(model, &database_fields);
    }
    for (field, ids) in &changes {
        tx.bind_capture(model, *field, ids)?;
    }
    tx.modified_prefetch(model, changes, Some(Arc::from(records.prefetch_ids())))
        .await?;
    for candidate in candidates {
        let base = tx.env();
        tx.publish_candidate(
            &base,
            candidate.model,
            candidate.field,
            &candidate.ids,
            candidate.out,
        )
        .await?;
    }
    // constraints of the non-inversed fields first
    tx.run_constraints(records, &names, inversed).await?;
    let real: Vec<RecId> = records
        .rec_ids()
        .iter()
        .copied()
        .filter(|id| !id.is_new())
        .collect();
    if real.is_empty() {
        return Ok(());
    }
    let real = records.browse_ids(real);
    for (method, _) in inverses {
        call_method::<(), ()>(tx, &real, method, ()).await?;
    }
    Ok(())
}

pub(crate) fn array_param(
    storage: Storage,
    cells: Vec<Cell>,
) -> Result<Box<dyn ToSql + Sync + Send>> {
    fn collect<T>(
        cells: Vec<Cell>,
        f: impl Fn(Cell) -> Option<Option<T>>,
    ) -> Result<Vec<Option<T>>> {
        cells
            .into_iter()
            .map(|c| {
                f(c).ok_or_else(|| Error::programming("cache value does not match its column"))
            })
            .collect()
    }
    let boxed: Box<dyn ToSql + Sync + Send> = match storage {
        Storage::Bool => Box::new(collect(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Bool(b) => Some(Some(b)),
            _ => None,
        })?),
        // collect refuses mismatched cells; only explicit Cell::Null becomes None.
        Storage::BigInt => Box::new(collect(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::BigInt(i) => Some(Some(i.get())),
            _ => None,
        })?),
        Storage::Int => Box::new(collect(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Int(i) => Some(Some(i)),
            _ => None,
        })?),
        Storage::Id => Box::new(collect(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Id(i) => Some(Some(i)),
            _ => None,
        })?),
        Storage::Float => Box::new(collect(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Float(x) => Some(Some(x)),
            _ => None,
        })?),
        Storage::Decimal => Box::new(collect::<Decimal>(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Decimal(x) => Some(Some(x)),
            _ => None,
        })?),
        Storage::Str => Box::new(collect::<String>(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Str(s) => Some(Some(s.to_string())),
            _ => None,
        })?),
        Storage::Date => Box::new(collect::<Date>(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Date(d) => Some(Some(d)),
            _ => None,
        })?),
        Storage::DateTime => Box::new(collect::<DateTime>(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::DateTime(d) => Some(Some(d)),
            _ => None,
        })?),
        Storage::Uuid => Box::new(collect::<erp_fields::Uuid>(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Uuid(v) => Some(Some(v)),
            _ => None,
        })?),
        Storage::Xid8 => Box::new(collect::<erp_fields::Xid8>(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Xid8(v) => Some(Some(v)),
            _ => None,
        })?),
        Storage::TimestampTz => Box::new(collect::<erp_fields::TimestampTz>(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::TimestampTz(v) => Some(Some(v)),
            _ => None,
        })?),
        Storage::Json => Box::new(collect(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::Json(j) => Some(Some(erp_fields::serde_json::Value::clone(&j))),
            _ => None,
        })?),
        Storage::Ids => {
            return Err(Error::programming(
                "x2many values have no column: they are written as relation rows",
            ));
        }
        // ranges are sent with their field's bounds
        Storage::DateRange(bounds) => Box::new(collect(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::DateRange(r) => Some(Some(erp_fields::RangeSql::new(r, bounds))),
            _ => None,
        })?),
        Storage::DatetimeRange(bounds) => Box::new(collect(cells, |c| match c {
            Cell::Null => Some(None),
            Cell::DatetimeRange(r) => Some(Some(erp_fields::RangeSql::new(r, bounds))),
            _ => None,
        })?),
    };
    Ok(boxed)
}

/// The `int8[]` parameter of a statement over database ids.
pub(crate) fn id_param(ids: &[RealId]) -> Vec<i64> {
    ids.iter().map(|id| id.get()).collect()
}

impl Transaction {
    /// Recompute and write everything pending, repeating until nothing is left (COMP §4.11).
    /// Recomputation runs in the transaction's base environment ([`Transaction::env`]), whoever
    /// modified the dependencies, and rows it writes get that environment's uid as `write_uid`.
    ///
    /// Fails with a `Programming` error naming what is still pending (to compute or to write)
    /// when computations keep producing work beyond [`crate::OrmConfig::max_flush_rounds`]
    /// rounds, instead of committing a half-computed state (COMP §7 D5). A `Validation` error
    /// (a constraint, a database constraint) poisons the transaction (commit refuses).
    pub async fn flush(&mut self) -> Result<()> {
        self.refuse_adapter_effect()?;
        self.require_record_scope()?;
        self.check_not_cancelled()?;
        let result = self.flush_rounds().await;
        if let Err(error) = &result {
            self.poison_on_validation(error);
        }
        result
    }

    async fn flush_rounds(&mut self) -> Result<()> {
        let env = self.env();
        let rounds = self.config().max_flush_rounds.max(1);
        for _ in 0..rounds {
            if !self.cache.has_dirty() && !self.has_real_pending() {
                self.pending_capture.clear_cells();
                return Ok(());
            }
            self.recompute_all(&env).await?;
            let registry = Arc::clone(self.registry());
            for m in self.cache.dirty_models() {
                if let Some(spec) = registry.models().get(m) {
                    self.write_dirty(spec, None).await?;
                }
            }
        }
        if !self.cache.has_dirty() && !self.has_real_pending() {
            self.pending_capture.clear_cells();
            return Ok(());
        }
        let to_compute = self.fields_to_compute();
        let to_write = self.fields_to_write();
        Err(Error::programming(format!(
            "flush did not converge after {rounds} rounds: still to compute [{}], still to write \
             [{}]",
            to_compute.join(", "),
            to_write.join(", ")
        )))
    }

    /// Recompute the requested fields of `model` that are pending (and what they need), then write
    /// their pending values. Afterwards no requested field has a pending write or recomputation.
    /// Only the requested fields are written (per-field flush), with the pending
    /// audit values of the records written.
    pub async fn flush_fields(&mut self, model: ModelId, fields: &[FieldId]) -> Result<()> {
        self.refuse_adapter_effect()?;
        self.require_record_scope()?;
        self.check_not_cancelled()?;
        let registry = Arc::clone(self.registry());
        let spec = registry.model_by_id(model);
        let mut expanded = fields.to_vec();
        for field in fields {
            let field = spec.field_by_id(*field);
            // Inherited generation metadata carries owning-model dependency ids. Owner query
            // inputs are flushed on that model; never interpret those ids as child fields.
            if field.inherited.is_none()
                && let Some(generated) = field.generated()
            {
                for source in generated.dependencies() {
                    if !expanded.contains(source) {
                        expanded.push(*source);
                    }
                }
            }
        }
        let fields = expanded.as_slice();
        let env = self.env();
        let result = match self.recompute_fields(&env, model, fields).await {
            Ok(()) => {
                let registry = Arc::clone(self.registry());
                let spec = registry.model_by_id(model);
                self.write_dirty(spec, Some(fields)).await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = &result {
            self.poison_on_validation(error);
        }
        result
    }

    /// Recompute every pending field of `model`, then write all its pending values.
    pub async fn flush_model(&mut self, model: ModelId) -> Result<()> {
        self.refuse_adapter_effect()?;
        self.require_record_scope()?;
        self.check_not_cancelled()?;
        let env = self.env();
        let result = match self.recompute_model(&env, model).await {
            Ok(()) => {
                let registry = Arc::clone(self.registry());
                let spec = registry.model_by_id(model);
                self.write_dirty(spec, None).await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = &result {
            self.poison_on_validation(error);
        }
        result
    }

    /// Send the pending writes of `spec`'s fields (all when `fields` is `None`): one `UPDATE` per
    /// set of dirty fields and chunk of [`crate::OrmConfig::recompute_batch`] records, ids in
    /// ascending order (write 5 records = 1 query; a flush of N records costs
    /// O(N / batch) statements and keeps each statement's arrays bounded).
    pub(crate) async fn write_dirty(
        &mut self,
        spec: &ModelSpec,
        fields: Option<&[FieldId]>,
    ) -> Result<()> {
        let previous_origin = self.pending_capture.active.clone();
        let mut origin = None;
        let mut capture_dirty = false;
        if let Some(mc) = self.cache.model(spec.id) {
            for (index, ids) in mc.dirty() {
                let field = spec.fields.get(index).ok_or_else(|| {
                    Error::programming("outbox: dirty field is outside its model")
                })?;
                capture_dirty |= self.capture_relevant(spec.id, field.id);
                for id in ids {
                    if let Some(bound) = self.pending_capture.origin(spec.id, field.id, id) {
                        if origin.as_ref().is_some_and(|old| !Arc::ptr_eq(old, &bound)) {
                            let error = Error::programming(
                                "outbox: dirty batch contains unsettled distinct operations",
                            );
                            self.poison_with(&error);
                            return Err(error);
                        }
                        origin = Some(bound);
                    }
                }
            }
        }
        if let Some(origin) = origin {
            self.pending_capture.active = Some(origin);
        }
        let guard = self.guard();
        let mut ran = false;
        let result = self.write_dirty_run(spec, fields, &mut ran).await;
        self.pending_capture.active = previous_origin;
        guard.disarm();
        // an error after a flush UPDATE ran (a decode failure, a short statement, a hook, a
        // dependent notification) leaves part of the flush written. Event-relevant pre-SQL
        // admission/provenance errors also poison already-staged business work (S7); neither
        // failure may be caught and committed (review SF-M1, SF-N1).
        if let Err(error) = &result
            && (ran || capture_dirty)
        {
            self.poison_with(error);
        }
        result
    }

    async fn write_dirty_run(
        &mut self,
        spec: &ModelSpec,
        fields: Option<&[FieldId]>,
        ran: &mut bool,
    ) -> Result<()> {
        let Some(mc) = self.cache.model(spec.id) else {
            return Ok(());
        };
        if mc.has_dirty() {
            self.require_writable(crate::WriteOp::Flush, spec.name)?;
        }
        let mut per_record: BTreeMap<RecId, Vec<FieldId>> = BTreeMap::new();
        let dirty = mc.dirty();
        for (field_index, ids) in &dirty {
            let Some(field) = spec.fields.get(*field_index) else {
                continue;
            };
            if fields.is_some_and(|fs| !fs.contains(&field.id)) {
                continue;
            }
            for id in ids {
                // only database rows are ever dirty
                if !id.is_new() {
                    per_record.entry(*id).or_default().push(field.id);
                }
            }
        }
        // A record written for the requested fields takes its pending audit values along.
        if fields.is_some() {
            for (field_index, ids) in &dirty {
                let Some(field) = spec.fields.get(*field_index) else {
                    continue;
                };
                if !field.magic || fields.is_some_and(|fs| fs.contains(&field.id)) {
                    continue;
                }
                for id in ids {
                    if let Some(list) = per_record.get_mut(id) {
                        list.push(field.id);
                    }
                }
            }
        }
        if per_record.is_empty() {
            return Ok(());
        }
        // A precision row may have changed after the assignment was staged. Validate the
        // actual pending values again before UPDATE, without publishing any rounded value.
        let env = self.env();
        for (id, fields) in &per_record {
            for fid in fields {
                let field = spec.field_by_id(*fid);
                if crate::precision::usage(field).is_some()
                    && let Some(cell) = self.cache.get(spec.id, *fid, *id)
                {
                    let origin = if field.company_dependent {
                        self.engine
                            .properties
                            .origins
                            .get(&(spec.id, *id))
                            .cloned()
                            .ok_or_else(|| {
                                Error::programming(
                                    "Company-dependent flush is missing its writer origin",
                                )
                            })?
                    } else {
                        env.clone()
                    };
                    let scalar = if field.company_dependent {
                        self.prepare_properties(&origin, spec).await?;
                        let (company, fallback) = self.property_context(&origin, spec.id, *fid)?;
                        crate::company_dependent::project(&field.ty, &cell, company, &fallback)?
                    } else {
                        cell
                    };
                    crate::precision::validate(self, &origin, spec, field, &scalar).await?;
                }
            }
        }
        // Rows written without a pending write_uid (recompute only) get the flushing uid,
        // and the audit values set here notify their dependents.
        let mut audit_changes: Vec<(FieldId, Vec<RecId>)> = Vec::new();
        if spec.log_access
            && let Some(wu) = spec.field("write_uid")
        {
            let uid = Cell::Id(self.env().uid());
            let mut changed = Vec::new();
            let mc = self.cache.model_mut(spec.id);
            for (id, list) in &mut per_record {
                if !list.contains(&wu.id) {
                    list.push(wu.id);
                    if !mc.get(wu.id, *id).is_some_and(|c| c.same_value(&uid)) {
                        changed.push(*id);
                    }
                    mc.set(wu.id, Storage::Id, *id, uid.clone(), true)?;
                }
            }
            if !changed.is_empty() {
                audit_changes.push((wu.id, changed));
            }
        }
        let mut groups: BTreeMap<Vec<FieldId>, Vec<RealId>> = BTreeMap::new();
        for (id, mut fields) in per_record {
            let Some(real) = id.as_real() else { continue };
            fields.sort_by_key(|f| spec.field_by_id(*f).name);
            groups.entry(fields).or_default().push(real);
        }
        let chunk = self.config().recompute_batch.max(1);
        let groups: Vec<(Vec<FieldId>, Vec<RealId>)> = groups
            .into_iter()
            .flat_map(|(fields, ids)| {
                ids.chunks(chunk)
                    .map(|c| (fields.clone(), c.to_vec()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (group_fields, ids) in groups {
            let rec_ids: Vec<RecId> = ids.iter().map(|id| RecId::real(*id)).collect();
            let mut columns: Vec<(&str, ColumnType)> = Vec::with_capacity(group_fields.len());
            let mut arrays: Vec<Box<dyn ToSql + Sync + Send>> =
                Vec::with_capacity(group_fields.len());
            let mut merge_cols: Vec<&str> = Vec::new();
            let mut company_cols: Vec<&str> = Vec::new();
            for fid in &group_fields {
                let field = spec.field_by_id(*fid);
                let Some(col) = field.column() else { continue };
                columns.push((field.name, col));
                if field.company_dependent {
                    company_cols.push(field.name);
                    let deltas = rec_ids
                        .iter()
                        .map(|id| {
                            let current = self.cache.get(spec.id, *fid, *id).ok_or_else(|| {
                                Error::programming("Company-dependent flush lost its map")
                            })?;
                            let before = self
                                .engine
                                .properties
                                .baselines
                                .get(&(spec.id, *fid, *id))
                                .ok_or_else(|| {
                                    Error::programming(
                                        "Company-dependent flush is missing its baseline",
                                    )
                                })?;
                            crate::company_dependent::delta(before, &current)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    arrays.push(array_param(field.storage(), deltas)?);
                } else if field.translate {
                    merge_cols.push(field.name);
                    let delta_cells: Vec<Cell> = rec_ids
                        .iter()
                        .map(|id| {
                            let cur = self.cache.get(spec.id, *fid, *id).unwrap_or(Cell::Null);
                            let base = self.translated_baselines.get(&(spec.id, *fid, *id));
                            let what = format!("{}.{}#{id}", spec.name, field.name);
                            crate::translated::delta(&what, base, &cur)
                        })
                        .collect::<Result<_>>()?;
                    arrays.push(array_param(field.storage(), delta_cells)?);
                } else {
                    let cells: Vec<Cell> = rec_ids
                        .iter()
                        .map(|id| self.cache.get(spec.id, *fid, *id).unwrap_or(Cell::Null))
                        .collect();
                    arrays.push(array_param(field.storage(), cells)?);
                }
            }
            let extra: Vec<(&str, &str)> = if spec.log_access && spec.field("write_date").is_some()
            {
                vec![("write_date", NOW_UTC)]
            } else {
                Vec::new()
            };
            // version bump, expected versions, old values for write hooks (design §4)
            let expected = self.versions.for_ids(spec.id, &ids);
            let hooked = crate::version::hooked_fields(self, spec.id, &group_fields);
            let hooked_names: Vec<&str> =
                hooked.iter().map(|f| spec.field_by_id(*f).name).collect();
            // Database flags include owned generated outputs; automatic audit/version columns
            // alone never manufacture a business write event.
            let mut changed_fields: Vec<FieldId> = if spec.eventful {
                group_fields
                    .iter()
                    .copied()
                    .chain(
                        spec.fields
                            .iter()
                            .filter(|field| {
                                field.generated().is_some() && field.inherited.is_none()
                            })
                            .map(|field| field.id),
                    )
                    .filter(|field| {
                        let fs = spec.field_by_id(*field);
                        fs.has_column() && !fs.magic && spec.row_version != Some(*field)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            changed_fields.sort_by_key(|field| spec.field_by_id(*field).name);
            changed_fields.dedup();
            if changed_fields.len() > erp_registry::MAX_EVENT_FIELD_NAMES {
                return Err(Error::new(
                    erp_fields::ErrorKind::ResourceExhausted,
                    format!(
                        "outbox CaptureLimit: event exceeds {} changed field names",
                        erp_registry::MAX_EVENT_FIELD_NAMES
                    ),
                ));
            }
            let changed_names: Vec<&str> = changed_fields
                .iter()
                .map(|field| spec.field_by_id(*field).name)
                .collect();
            // Validate provenance before SQL, including equal-value updates on a cold cache.
            if spec.eventful {
                for id in &rec_ids {
                    if !group_fields
                        .iter()
                        .any(|field| self.pending_capture.origin(spec.id, *field, *id).is_some())
                    {
                        return Err(Error::programming(
                            "outbox: eventful UPDATE is missing writer provenance",
                        ));
                    }
                }
            }
            let with = erp_sql::version::VersionedUpdate::new()
                .bump(spec.row_version.is_some())
                .expected(expected.is_some())
                .returning_old(&hooked_names)
                .returning_changed(&changed_names)
                .merge_json(&merge_cols)
                .merge_company(&company_cols);
            let sql = erp_sql::version::update_from_arrays_cached(
                self.registry(),
                spec,
                &columns,
                &extra,
                &with,
            );
            let id_values = id_param(&ids);
            let mut params: Vec<&(dyn ToSql + Sync)> = vec![&id_values];
            params.extend(arrays.iter().map(|a| a.as_ref() as &(dyn ToSql + Sync)));
            if let Some(expected) = &expected {
                params.push(expected);
            }
            let mut hook_rows: Vec<(RecId, Vec<(Cell, Cell)>)> = Vec::new();
            let mut captured: BTreeMap<Vec<FieldId>, Vec<crate::outbox::IdEntry>> = BTreeMap::new();
            if with.returns_rows() {
                let rows = self
                    .query(&sql, &params)
                    .await
                    .map_err(|e| crate::version::version_overflow(spec, e))?;
                *ran = true;
                let mut written: FxHashSet<i64> = FxHashSet::default();
                for row in &rows {
                    let raw: i64 = row.try_get(0).map_err(|e| {
                        Error::new(
                            erp_fields::ErrorKind::Database,
                            "could not decode an UPDATE result",
                        )
                        .with_source(e)
                    })?;
                    // Groups preserve the ascending IDs of the ordered per-record map.
                    if id_values.binary_search(&raw).is_err() || !written.insert(raw) {
                        return Err(Error::programming(
                            "outbox: UPDATE returned an unexpected or duplicate id",
                        ));
                    }
                    let id = RecId::from_real(raw).ok_or_else(|| {
                        Error::programming(format!("UPDATE of {} returned id {raw}", spec.table))
                    })?;
                    let mut actual = Vec::new();
                    for (index, field) in changed_fields.iter().enumerate() {
                        let changed: bool =
                            row.try_get(1 + 2 * hooked.len() + index).map_err(|error| {
                                Error::new(
                                    erp_fields::ErrorKind::Database,
                                    "outbox: could not decode an UPDATE change flag",
                                )
                                .with_source(error)
                            })?;
                        if changed {
                            actual.push(*field);
                        }
                    }
                    if !actual.is_empty() {
                        captured
                            .entry(actual)
                            .or_default()
                            .push(crate::outbox::IdEntry::Single(raw));
                    }
                    if hooked.is_empty() {
                        continue;
                    }
                    let mut values = Vec::with_capacity(hooked.len());
                    for (i, f) in hooked.iter().enumerate() {
                        let col = spec.field_by_id(*f).column().ok_or_else(|| {
                            Error::programming(format!(
                                "hooked field {}.{} has no column",
                                spec.name,
                                spec.field_by_id(*f).name
                            ))
                        })?;
                        let old = crate::read::decode(row, 1 + 2 * i, col)?;
                        let new = crate::read::decode(row, 2 + 2 * i, col)?;
                        values.push((old, new));
                    }
                    hook_rows.push((id, values));
                }
                if written.len() != ids.len() {
                    let skipped: Vec<RealId> = ids
                        .iter()
                        .copied()
                        .filter(|id| !written.contains(&id.get()))
                        .collect();
                    let error = match &expected {
                        Some(expected) => {
                            crate::version::stale_or_missing(self, spec, &ids, expected, &skipped)
                                .await
                        }
                        None => self.missing_rows(spec, &skipped).await,
                    };
                    // the statement wrote the other rows of its chunk already: a caller that
                    // catches this error must not commit the partial write (review SF-M1)
                    self.poison_with(&error);
                    return Err(error);
                }
                self.versions.consume(spec.id, &ids);
            } else {
                let updated = self
                    .execute(&sql, &params)
                    .await
                    .map_err(|e| crate::version::version_overflow(spec, e))?;
                *ran = true;
                if usize::try_from(updated).ok() != Some(ids.len()) {
                    // part of the chunk may be written: poison, as above (review SF-M1)
                    let error = self.missing_rows(spec, &ids).await;
                    self.poison_with(&error);
                    return Err(error);
                }
            }
            if !captured.is_empty() {
                let context = self
                    .pending_capture
                    .active
                    .as_ref()
                    .ok_or_else(|| {
                        Error::programming("outbox: eventful UPDATE is missing writer provenance")
                    })?
                    .clone();
                let depth = self.savepoint_depth();
                for (fields, members) in captured {
                    // Schema names stay shared until a group becomes an envelope.
                    let names: Vec<String> = fields
                        .iter()
                        .map(|field| spec.field_by_id(*field).name.to_owned())
                        .collect();
                    for chunk in members.chunks(crate::outbox::MAX_ID_ENTRIES) {
                        let envelope = context.envelope(
                            crate::outbox::Kind::Write,
                            spec.name,
                            chunk.to_vec(),
                            names.clone(),
                            Vec::new(),
                        )?;
                        self.outbox.push(envelope, depth)?;
                    }
                }
            }
            let now = self.now;
            let mc = self.cache.model_mut(spec.id);
            for fid in &group_fields {
                let fs = spec.field_by_id(*fid);
                mc.mark_clean(*fid, &rec_ids);
                if fs.translate || fs.company_dependent {
                    mc.invalidate(Some(&[*fid][..]), Some(&rec_ids));
                    for id in &rec_ids {
                        self.translated_baselines.remove(&(spec.id, *fid, *id));
                    }
                }
            }
            for fid in &group_fields {
                if spec.field_by_id(*fid).company_dependent {
                    for id in &rec_ids {
                        self.properties_mut()
                            .baselines
                            .remove(&(spec.id, *fid, *id));
                    }
                }
            }
            for id in &rec_ids {
                if !self
                    .engine
                    .properties
                    .baselines
                    .keys()
                    .any(|(model, _, record)| *model == spec.id && record == id)
                {
                    self.properties_mut().origins.remove(&(spec.id, *id));
                }
            }
            let mc = self.cache.model_mut(spec.id);
            if let Some(wd) = spec.field("write_date").filter(|_| spec.log_access) {
                match now {
                    Some(n) => {
                        let stamp = Cell::DateTime(n);
                        let mut changed = Vec::new();
                        for id in &rec_ids {
                            if !mc.get(wd.id, *id).is_some_and(|c| c.same_value(&stamp)) {
                                changed.push(*id);
                                mc.set(wd.id, Storage::DateTime, *id, stamp.clone(), false)?;
                            }
                        }
                        if !changed.is_empty() {
                            audit_changes.push((wd.id, changed));
                        }
                    }
                    None => {
                        mc.invalidate(Some(&[wd.id][..]), Some(&rec_ids));
                        audit_changes.push((wd.id, rec_ids.clone()));
                    }
                }
            }
            // the statement bumped the version: the cached one is stale
            if let Some(rv) = spec.row_version {
                mc.invalidate(Some(&[rv][..]), Some(&rec_ids));
            }
            if !hook_rows.is_empty() {
                self.track_capture(spec, &hooked, &hook_rows);
                // the rows are written: a failed hook must not let the transaction commit
                if let Err(error) =
                    crate::version::call_write_hooks(self, spec, &hooked, hook_rows).await
                {
                    self.poison_with(&error);
                    return Err(error);
                }
            }
        }
        self.modified(spec.id, audit_changes).await?;
        Ok(())
    }

    /// The `Missing` error for an UPDATE that matched fewer rows than requested: one query finds
    /// the ids that no longer exist (only on this error path).
    async fn missing_rows(&mut self, spec: &ModelSpec, ids: &[RealId]) -> Error {
        let sql = erp_sql::dml::select_by_ids_cached(self.registry(), spec, &[]);
        let id_values = id_param(ids);
        let rows = match self.query(&sql, &[&id_values]).await {
            Ok(rows) => rows,
            Err(error) => return error,
        };
        // an undecodable row is an error, not a missing record (review SF-S2)
        let mut found: FxHashSet<i64> = FxHashSet::default();
        for row in &rows {
            match row.try_get::<_, i64>(0) {
                Ok(id) => {
                    found.insert(id);
                }
                Err(e) => {
                    return Error::new(
                        erp_fields::ErrorKind::Database,
                        "could not decode the ids found after a short UPDATE",
                    )
                    .with_source(e);
                }
            }
        }
        let missing: Vec<RealId> = ids
            .iter()
            .copied()
            .filter(|id| !found.contains(&id.get()))
            .collect();
        missing_error(spec, &missing, self.env().uid())
    }
}

/// The `Missing` error for records an `UPDATE` did not find.
pub(crate) fn missing_error(spec: &ModelSpec, missing: &[RealId], uid: i64) -> Error {
    let listed: Vec<String> = missing.iter().map(ToString::to_string).collect();
    Error::missing(format!(
        "Record does not exist or has been deleted.\n(Record: {}({}{}), User: {uid})",
        spec.name,
        listed.join(", "),
        if listed.len() == 1 { "," } else { "" },
    ))
}

// ---- base implementations of `create` and `write` -------------------------------------------

type CreateFn = MethodFn<(Vec<RawVals>,), Records>;
type WriteFn = MethodFn<(RawVals,), ()>;

fn base_create<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (Vec<RawVals>,),
    _sup: Super<(Vec<RawVals>,), Records>,
) -> BoxFuture<'a, Result<Records>> {
    Box::pin(crate::relcrud::create(tx, records, args.0))
}

fn base_write<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (RawVals,),
    _sup: Super<(RawVals,), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(crate::relcrud::write(tx, records, args.0))
}

fn create_entry() -> &'static (dyn Any + Send + Sync) {
    static F: CreateFn = base_create;
    &F
}

fn write_entry() -> &'static (dyn Any + Send + Sync) {
    static F: WriteFn = base_write;
    &F
}

/// The core implementations, registered by [`crate::Dispatch::build`] directly: a `linkme` element
/// in the crate that declares the slice would expand `#[link_section]` in this crate, which the
/// workspace's `forbid(unsafe_code)` rejects (guard test 1).
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 2] = [&BASE_CREATE, &BASE_WRITE];

static BASE_CREATE: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "create",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<CreateFn>,
    entry: create_entry,
    file: file!(),
    line: line!(),
};

static BASE_WRITE: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "write",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<WriteFn>,
    entry: write_entry,
    file: file!(),
    line: line!(),
};
