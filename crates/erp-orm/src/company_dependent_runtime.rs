// SPDX-License-Identifier: MIT OR Apache-2.0
//! ORM-owned property preparation. The codec remains independent of authorization and SQL.
//!
//! # Prepared fallbacks and when they are read again
//!
//! The fallbacks of a model for a company are read once per transaction ([`Prepared`]) and reused
//! by every later create, read, write and query of that model in that company (
//! supplement of 2026-11-15). They never leave the transaction: the record API
//! runs at `REPEATABLE READ`, so only this transaction's own changes can alter what the lookup
//! would return. Each of these marks the entries that rest on the changed rows stale, and a stale
//! entry is read again by the next `prepare_properties`:
//! - a change of `ir.default`, of `ir.model.fields` or of `decimal.precision` rows through the
//!   record API, pending or flushed, and the `ir.default` helper functions (every entry; the
//!   `ir.default` invalidation removes them, as it always did);
//! - rows created or deleted in a model a fallback names as its many2one target, through the
//!   record API or an explicit invalidation of the model without a field list (the entries
//!   naming that target);
//! - a statement that names the table it writes (`INSERT`, `UPDATE`, `DELETE`, `MERGE`), the
//!   ORM's own included, mapped to its models like the write ticks do
//!   (`transaction/write_gens.rs`: the models stored in the table, the delegating models, and for
//!   a delete the models a database cascade reaches); a table no model knows marks every entry;
//! - a statement whose effect is unknown and that is not a data-changing `WITH` (`TRUNCATE`,
//!   `COPY`, `CALL`, `DO`, DDL, a raw `ROLLBACK`, a batch, and every statement once the
//!   connection's reading of string literals is uncertain) marks every entry;
//! - `Transaction::invalidate_all` marks every entry; a rollback to a savepoint, commit and
//!   rollback remove them; an isolated scope restores the entries it started with.
//!
//! Stale, not removed: an operation prepares first and projects values afterwards
//! (`property_context`), and a statement it sends in between (a flush of pending writes before a
//! restricted read, for example) may mark its entry. The operation keeps the fallbacks it started
//! with, as it did when every use prepared again; the next operation reads them again.
//!
//! Not seen: a data-changing `WITH` (also one the word reader is unsure about), a `SELECT`
//! calling a function that writes, and a trigger writing `ir_default` or a target table. The
//! ORM's own record inserts are data-changing `WITH` statements and are covered by the record
//! API rule above; its synchronous aggregate upsert (`delta.rs`) only inserts rows, outside that
//! rule, so a dangling fallback naming an id such an insert takes later stays empty until the
//! entry is read again. Raw code of that kind calls
//! `Transaction::invalidate_ormcache("ir.default")`, `Transaction::invalidate_model` for the
//! target model without a field list, or `Transaction::invalidate_all`.
use crate::transaction::write_gens;
use crate::{CompanyId, Env, Records, Transaction};
use erp_fields::{Cell, Error, Result};
use erp_registry::{FieldId, ModelId, ModelSpec};
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// The fallbacks of one model for one company, as this transaction's snapshot and its own
/// writes show them.
#[derive(Debug, Clone)]
pub(crate) struct Prepared {
    /// One cell per field of the model, by field index; `Null` where the field has no fallback
    /// or is not company-dependent.
    cells: Vec<Cell>,
    /// The comodels whose rows the cells rest on: the target model of every many2one fallback
    /// that named an id, also one found dangling (a later insert can make it live again).
    targets: Vec<ModelId>,
    /// An input changed after the entry was read: the next `prepare_properties` reads it again.
    /// Until then it still serves the operation that prepared it (module documentation).
    stale: bool,
}

/// The models whose rows every fallback lookup reads, whatever the model it is for.
fn is_lookup_input(model: &str) -> bool {
    matches!(
        model,
        "ir.default" | "ir.model.fields" | "decimal.precision"
    )
}

impl State {
    /// Mark the prepared entries `ends` selects stale. Nothing is copied when none is live.
    pub(crate) fn end_prepared(this: &mut Arc<Self>, ends: impl Fn(&Prepared) -> bool) {
        if this
            .defaults
            .values()
            .any(|entry| !entry.stale && ends(entry))
        {
            for entry in Arc::make_mut(this).defaults.values_mut() {
                if ends(entry) {
                    entry.stale = true;
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct State {
    pub defaults: FxHashMap<(ModelId, CompanyId), Prepared>,
    pub baselines: FxHashMap<(ModelId, FieldId, erp_fields::RecId), Cell>,
    pub origins: FxHashMap<(ModelId, erp_fields::RecId), Env>,
}

/// The board task that lifts a refusal of this module's callers. Each refusal names its own
/// task; the ids of this crate's refusals exist once, here. Two refusals of crates below this one
/// carry their task id in their own message (`erp-registry`, the declaration refusal in
/// `compose.rs`); a test pins each of those messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owner {
    StoredCompute,
}

impl Owner {
    const fn task(self) -> &'static str {
        match self {
            Self::StoredCompute => "a later release",
        }
    }
}

pub(crate) fn unsupported(model: &str, field: &str, operation: &str, owner: Owner) -> Error {
    Error::programming(format!(
        "{model}.{field}: company-dependent {operation} is not supported until {}",
        owner.task()
    ))
}

impl Transaction {
    pub(crate) fn properties_mut(&mut self) -> &mut State {
        Arc::make_mut(&mut self.engine.properties)
    }

    /// An absent property on create has no company ownership.
    pub(crate) async fn encode_create_properties(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        rows: &mut [Vec<Option<Cell>>],
    ) -> Result<()> {
        let fields: Vec<_> = spec
            .fields
            .iter()
            .filter(|field| field.company_dependent && field.inherited.is_none())
            .collect();
        if fields.is_empty() {
            return Ok(());
        }
        let company = self.companies(env).await?.first().copied();
        if company.is_none() {
            // Presence matters: explicit null/false/zero still needs a company. Check the
            // entire batch before encoding or inserting any row. Defaults are already resolved.
            if fields
                .iter()
                .any(|field| rows.iter().any(|row| row[field.id.index()].is_some()))
            {
                return Err(Error::validation(
                    "company-dependent fields require a current company",
                ));
            }
            for field in fields {
                check_required(spec, field, &Cell::Null)?;
            }
            // SQL NULL means no override for any company; do not prepare or project a fallback.
            return Ok(());
        }
        self.prepare_properties(env, spec).await?;
        for field in fields {
            let (company, fallback) = self.property_context(env, spec.id, field.id)?;
            for row in rows.iter_mut() {
                // Absent values inherit the fallback; explicit null is preserved.
                let scalar = row[field.id.index()].as_ref().unwrap_or(&fallback);
                refuse_draft_link(spec, field, scalar)?;
                check_required(spec, field, scalar)?;
                row[field.id.index()] = Some(crate::company_dependent::apply(
                    &field.ty,
                    &Cell::Null,
                    company,
                    scalar,
                    &fallback,
                )?);
            }
        }
        Ok(())
    }

    pub(crate) async fn prepare_properties(&mut self, env: &Env, spec: &ModelSpec) -> Result<()> {
        if !spec
            .fields
            .iter()
            .any(|f| f.company_dependent && f.inherited.is_none())
        {
            return Ok(());
        }
        let company = self.companies(env).await?.first().copied().ok_or_else(|| {
            Error::validation("company-dependent fields require a current company")
        })?;
        // Reuse what this transaction prepared: every change that could alter the lookup ended
        // the entry (module documentation). One snapshot is the premise, so a transaction that
        // takes a new snapshot per statement prepares again each time.
        if self.isolation_level() == crate::transaction::Isolation::RepeatableRead
            && self
                .engine
                .properties
                .defaults
                .get(&(spec.id, company))
                .is_some_and(|prepared| {
                    !prepared.stale && prepared.cells.len() == spec.fields.len()
                })
        {
            return Ok(());
        }
        let defaults = if crate::ir_default::is_available(env.registry()) {
            crate::ir_default::get_property_defaults(self, env, spec.name).await?
        } else {
            FxHashMap::default()
        };
        let mut cells = vec![Cell::Null; spec.fields.len()];
        for field in spec
            .fields
            .iter()
            .filter(|f| f.company_dependent && f.inherited.is_none())
        {
            cells[field.id.index()] = self
                .property_fallback_cell(env, spec, field, defaults.get(field.name))
                .await?;
        }
        let targets = self.clear_dangling_fallbacks(env, spec, &mut cells).await?;
        self.properties_mut().defaults.insert(
            (spec.id, company),
            Prepared {
                cells,
                targets,
                stale: false,
            },
        );
        Ok(())
    }

    /// The transaction changed rows of `model` (`rows`: rows were created or deleted, or may have
    /// been): the prepared fallbacks that rest on them are read again on their next use (module
    /// documentation).
    pub(crate) fn property_inputs_written(&mut self, model: ModelId, rows: bool) {
        if self.engine.properties.defaults.is_empty() {
            return;
        }
        if is_lookup_input(self.shared.registry.model_by_id(model).name) {
            State::end_prepared(&mut self.engine.properties, |_| true);
        } else if rows {
            State::end_prepared(&mut self.engine.properties, |entry| {
                entry.targets.contains(&model)
            });
        }
    }

    /// A statement was sent whose text the classifier read as `class`: the prepared fallbacks
    /// that may rest on what it changed are read again on their next use (module documentation).
    pub(crate) fn property_statement_sent(
        &mut self,
        sql: &str,
        class: &write_gens::Classified<'_>,
    ) {
        if self.engine.properties.defaults.is_empty() {
            return;
        }
        match class.target {
            write_gens::Target::Reads => {}
            write_gens::Target::Rows { table, delete } => {
                match self.shared.tables.models(table, delete) {
                    Some(models) => {
                        let registry = &self.shared.registry;
                        let lookup = models
                            .iter()
                            .any(|model| is_lookup_input(registry.model_by_id(*model).name));
                        State::end_prepared(&mut self.engine.properties, |entry| {
                            lookup || entry.targets.iter().any(|target| models.contains(target))
                        });
                    }
                    // a table no model knows: fail closed
                    None => State::end_prepared(&mut self.engine.properties, |_| true),
                }
            }
            // The ORM's own record inserts are data-changing `WITH` statements; the record API
            // marks the entries for them. Anything else unknown fails closed.
            write_gens::Target::Unknown
                if class.data_change && starts_with_keyword(sql, "WITH") => {}
            write_gens::Target::Unknown => {
                State::end_prepared(&mut self.engine.properties, |_| true);
            }
        }
    }

    /// A many2one fallback whose target is gone reads empty, like a dangling override (
    /// decision 6): the existence of each target id is checked in the database, one statement per
    /// comodel, and only when a fallback names an id. Record rules do not apply. Returns the
    /// comodels checked.
    async fn clear_dangling_fallbacks(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        cells: &mut [Cell],
    ) -> Result<Vec<ModelId>> {
        let mut by_comodel: Vec<(&str, Vec<i64>)> = Vec::new();
        for field in spec
            .fields
            .iter()
            .filter(|f| f.company_dependent && f.inherited.is_none())
        {
            let (Some(comodel), Cell::Id(id)) = (field.ty.comodel(), &cells[field.id.index()])
            else {
                continue;
            };
            match by_comodel.iter_mut().find(|(name, _)| *name == comodel) {
                Some((_, ids)) => ids.push(*id),
                None => by_comodel.push((comodel, vec![*id])),
            }
        }
        let mut targets = Vec::with_capacity(by_comodel.len());
        for (comodel, ids) in by_comodel {
            let target = env
                .registry()
                .model(comodel)
                .ok_or_else(|| Error::programming(format!("unknown property comodel {comodel}")))?;
            targets.push(target.id);
            let live = self.existing_property_targets(env, comodel, &ids).await?;
            for field in spec
                .fields
                .iter()
                .filter(|f| f.company_dependent && f.inherited.is_none())
                .filter(|f| f.ty.comodel() == Some(comodel))
            {
                if matches!(&cells[field.id.index()], Cell::Id(id) if !live.contains(id)) {
                    cells[field.id.index()] = Cell::Null;
                }
            }
        }
        Ok(targets)
    }

    /// The fallback cell of company-dependent `field` from its `ir.default` value (`None`: no
    /// default): type conversion with the JSON `false` sentinel restored, HTML admission,
    /// precision and the company-map codec round trip. Shared by the fallback of the current
    /// company ([`Self::prepare_properties`]) and by the company fields that mirror a default
    /// (`crate::company_default_for`), so both read one value the same way.
    pub(crate) async fn property_fallback_cell(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        field: &erp_registry::FieldSpec,
        value: Option<&erp_fields::Value>,
    ) -> Result<Cell> {
        let cell = match value {
            Some(value) => crate::html::input_cell(
                erp_fields::convert::FieldRef {
                    model: spec.name,
                    field: field.name,
                    label: &field.string,
                },
                &field.ty,
                // ir.default serializes the public empty value as JSON false. Restore
                // that sentinel before strict date/relation conversion (FIELD-075/085).
                match value {
                    erp_fields::Value::Bool(false) => &erp_fields::Value::False,
                    other => other,
                },
            )
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!(
                        "{}.{}: invalid company-dependent fallback",
                        spec.name, field.name
                    ),
                )
            })?,
            None => Cell::Null,
        };
        let cell = self.admit_html_cell(spec, field, cell)?;
        crate::precision::validate(self, env, spec, field, &cell).await?;
        crate::company_dependent::decode(
            &field.ty,
            &crate::company_dependent::encode(&field.ty, &cell)?,
        )
    }

    /// The ids of `ids` that have a row in `comodel`'s table, in one statement. Record rules do
    /// not apply (a dangling many2one default reads empty).
    pub(crate) async fn existing_property_targets(
        &mut self,
        env: &Env,
        comodel: &str,
        ids: &[i64],
    ) -> Result<Vec<i64>> {
        let target = env
            .registry()
            .model(comodel)
            .ok_or_else(|| Error::programming(format!("unknown property comodel {comodel}")))?;
        let sql = format!(
            "SELECT \"id\" FROM {} WHERE \"id\" = ANY($1::int8[])",
            erp_sql::quote(&target.table)
        );
        let mut live: Vec<i64> = Vec::with_capacity(ids.len());
        for row in self.query(&sql, &[&ids]).await? {
            live.push(row.try_get(0).map_err(|error| {
                Error::new(erp_fields::ErrorKind::Database, "could not decode an id")
                    .with_source(error)
            })?);
        }
        Ok(live)
    }

    pub(crate) fn property_context(
        &self,
        env: &Env,
        model: ModelId,
        field: FieldId,
    ) -> Result<(CompanyId, Cell)> {
        let company = self.companies_now(env)?.first().copied().ok_or_else(|| {
            Error::validation("company-dependent fields require a current company")
        })?;
        let spec = self.registry().model_by_id(model);
        let fallback = self
            .engine
            .properties
            .defaults
            .get(&(model, company))
            .and_then(|prepared| prepared.cells.get(field.index()))
            .cloned()
            .ok_or_else(|| {
                Error::programming(format!(
                    "{}.{}: company-dependent read requires prepared defaults",
                    spec.name,
                    spec.field_by_id(field).name
                ))
            })?;
        Ok((company, fallback))
    }

    /// The ids among `ids` that have a row in the table of `comodel`, as this transaction sees
    /// it: the existence rule of a many2one fallback, for a creation default that names a
    /// record. One statement; record rules do not apply.
    pub(crate) async fn live_ids(
        &mut self,
        env: &Env,
        comodel: &str,
        ids: &[i64],
    ) -> Result<Vec<i64>> {
        let target = env
            .registry()
            .model(comodel)
            .ok_or_else(|| Error::programming(format!("unknown property comodel {comodel}")))?;
        let sql = format!(
            "SELECT \"id\" FROM {} WHERE \"id\" = ANY($1::int8[])",
            erp_sql::quote(&target.table)
        );
        let mut live = Vec::with_capacity(ids.len());
        for row in self.query(&sql, &[&ids]).await? {
            live.push(row.try_get(0).map_err(|error| {
                Error::new(erp_fields::ErrorKind::Database, "could not decode an id")
                    .with_source(error)
            })?);
        }
        Ok(live)
    }

    /// The company map draft `record` holds once `scalar` is assigned to `field` for the
    /// environment's company: its current map (copied from its origin, or its default) with that
    /// company's entry set and every other company's entry untouched. The draft
    /// holds a map like a stored record, so a read in another company
    /// projects that company's value. Nothing is written to the database.
    pub(crate) async fn draft_property_map(
        &mut self,
        record: &Records,
        field: &erp_registry::FieldSpec,
        scalar: &Cell,
    ) -> Result<Cell> {
        let spec = record.spec();
        let &[id] = record.rec_ids() else {
            return Err(Error::programming(format!(
                "{}.{}: a draft company value is assigned to one record at a time",
                spec.name, field.name
            )));
        };
        if !id.is_new() {
            return Err(Error::programming(format!(
                "{}.{}: draft company values belong to new records only",
                spec.name, field.name
            )));
        }
        if matches!(scalar, Cell::Id(id) if *id < 0) {
            crate::newrec::new_record_cell(spec, field, scalar.clone(), self)?;
        }
        // prepares the fallbacks and copies the origin's map (or the default) first
        self.ensure(record, field.id).await?;
        let base = self
            .cache
            .get(spec.id, field.id, id)
            .ok_or_else(|| Error::programming("a draft company map was not cached by ensure"))?;
        let (company, fallback) = self.property_context(record.env(), spec.id, field.id)?;
        crate::company_dependent::apply_draft(&field.ty, &base, company, scalar, &fallback)
    }

    /// Never attribute a previous company's dirty map to a later writer. Flush at origin changes.
    pub(crate) async fn property_write_origin(
        &mut self,
        records: &Records,
        property_input: bool,
    ) -> Result<()> {
        let spec = records.spec();
        if !spec.fields.iter().any(|f| f.company_dependent) {
            return Ok(());
        }
        let env = records.env();
        let current = self.companies(env).await?.first().copied();
        let mut changed = false;
        for id in records.rec_ids() {
            if let Some(old) = self.engine.properties.origins.get(&(spec.id, *id)) {
                changed |=
                    old.uid() != env.uid() || old.su() != env.su() || old.company()? != current;
            }
        }
        if changed {
            self.flush_model(spec.id).await?;
        }
        if property_input {
            self.prepare_properties(env, spec).await?;
            for id in records.rec_ids().iter().filter(|id| !id.is_new()) {
                self.properties_mut()
                    .origins
                    .insert((spec.id, *id), env.clone());
            }
        }
        Ok(())
    }
}

/// Whether the first word of `sql` is `keyword` (ASCII case-insensitive), leading white space
/// skipped. A statement that opens with a comment does not match.
fn starts_with_keyword(sql: &str, keyword: &str) -> bool {
    let rest = sql.trim_start();
    rest.get(..keyword.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(keyword))
        && rest[keyword.len()..]
            .chars()
            .next()
            .is_none_or(|next| !(next.is_alphanumeric() || next == '_'))
}

/// A persisted company map stores database ids only.
pub(crate) fn refuse_draft_link(
    spec: &ModelSpec,
    field: &erp_registry::FieldSpec,
    value: &Cell,
) -> Result<()> {
    if matches!(value, Cell::Id(raw) if *raw < 0) {
        return Err(Error::validation(format!(
            "{}.{}: save the related record before writing a company-dependent many2one",
            spec.name, field.name,
        )));
    }
    Ok(())
}

pub(crate) fn check_required(
    spec: &ModelSpec,
    field: &erp_registry::FieldSpec,
    value: &Cell,
) -> Result<()> {
    let normalized = crate::company_dependent::decode(
        &field.ty,
        &crate::company_dependent::encode(&field.ty, value)?,
    )?;
    let value = &normalized;
    if field.required
        && (matches!(value, Cell::Null) || matches!(value, Cell::Str(s) if s.is_empty()))
    {
        return Err(Error::validation(format!(
            "{}.{}: required company-dependent value is missing",
            spec.name, field.name
        )));
    }
    Ok(())
}

impl Transaction {
    /// Build a selection catalog's company document without implicit writes or native method
    /// dispatch. Only the catalog's own, validated scalar dependencies are prepared.
    pub(crate) async fn catalog_company_values(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        fields: &[FieldId],
    ) -> Result<Option<erp_sql::company::CompanyValues>> {
        let company_fields: Vec<_> = fields
            .iter()
            .map(|id| spec.field_by_id(*id))
            .filter(|field| field.company_dependent)
            .collect();
        if company_fields.is_empty() {
            return Ok(None);
        }
        let company = self.companies_now(env)?.first().copied().ok_or_else(|| {
            Error::validation("company-dependent fields require a current company")
        })?;
        let defaults = if crate::ir_default::is_available(env.registry()) {
            crate::ir_default::catalog_property_defaults(self, env, spec.name).await?
        } else {
            FxHashMap::default()
        };
        let mut values = erp_sql::company::CompanyValues::new(company.get())?;
        for field in company_fields {
            if crate::precision::usage(field).is_some() && defaults.contains_key(field.name) {
                return Err(Error::programming(format!(
                    "selection catalog {}.{} cannot resolve a named precision default",
                    spec.name, field.name,
                )));
            }
            let fallback = self
                .property_fallback_cell(env, spec, field, defaults.get(field.name))
                .await?;
            values.insert(spec, field, &fallback)?;
        }
        Ok(Some(values))
    }

    /// Prepare only the physical property dependencies of a query component. Fallback identity,
    /// precision admission and snapshot invalidation stay shared with ordinary property reads.
    pub(crate) async fn company_query_values(
        &mut self,
        env: &Env,
        fields: &[(ModelId, FieldId)],
    ) -> Result<Option<erp_sql::company::CompanyValues>> {
        let registry = Arc::clone(env.registry());
        let mut models = Vec::new();
        for &(model, field) in fields {
            if registry
                .model_by_id(model)
                .field_by_id(field)
                .company_dependent
                && !models.contains(&model)
            {
                models.push(model);
            }
        }
        if models.is_empty() {
            return Ok(None);
        }
        let company = self.companies(env).await?.first().copied().ok_or_else(|| {
            Error::validation("company-dependent fields require a current company")
        })?;
        let mut values = erp_sql::company::CompanyValues::new(company.get())?;
        for model in models {
            let spec = registry.model_by_id(model);
            self.prepare_properties(env, spec).await?;
            for &(m, field) in fields.iter().filter(|(m, _)| *m == model) {
                let field = spec.field_by_id(field);
                if field.company_dependent {
                    let (_, fallback) = self.property_context(env, m, field.id)?;
                    values.insert(spec, field, &fallback)?;
                }
            }
        }
        Ok(Some(values))
    }
}

#[cfg(test)]
mod tests {
    use super::starts_with_keyword;

    #[test]
    fn a_statement_is_recognised_by_its_first_word_only() {
        for sql in [
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
            " \n\twith x as (select 1) select 1",
            "WITH",
            "WITH(x) AS (SELECT 1) SELECT 1",
        ] {
            assert!(starts_with_keyword(sql, "WITH"), "{sql}");
        }
        for sql in [
            "",
            "WI",
            "WITHIN",
            "with_x",
            "with9",
            "\"with\" x",
            "INSERT INTO t VALUES (1)",
            "/* c */ WITH x AS (SELECT 1) SELECT 1",
            "-- c\nWITH x AS (SELECT 1) SELECT 1",
            "wit\u{e9}",
        ] {
            assert!(!starts_with_keyword(sql, "WITH"), "{sql}");
        }
    }
}
