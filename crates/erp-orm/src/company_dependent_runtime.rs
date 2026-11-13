// SPDX-License-Identifier: MIT OR Apache-2.0
//! ORM-owned property preparation. The codec remains independent of authorization and SQL.
use crate::{CompanyId, Env, Records, Transaction};
use erp_fields::{Cell, Error, Result};
use erp_registry::{FieldId, ModelId, ModelSpec};
use rustc_hash::FxHashMap;
use std::sync::Arc;

#[derive(Debug, Default, Clone)]
pub(crate) struct State {
    pub defaults: FxHashMap<(ModelId, CompanyId), Vec<Cell>>,
    pub baselines: FxHashMap<(ModelId, FieldId, erp_fields::RecId), Cell>,
    pub origins: FxHashMap<(ModelId, erp_fields::RecId), Env>,
}

pub(crate) fn unsupported(model: &str, field: &str, operation: &str) -> Error {
    Error::programming(format!(
        "{model}.{field}: company-dependent {operation} is not supported until a later release"
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
        // Refresh snapshot-local fallbacks even when the raw record maps are warm.
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
            let cell = match defaults.get(field.name) {
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
            cells[field.id.index()] = crate::company_dependent::decode(
                &field.ty,
                &crate::company_dependent::encode(&field.ty, &cell)?,
            )?;
        }
        self.clear_dangling_fallbacks(env, spec, &mut cells).await?;
        self.properties_mut()
            .defaults
            .insert((spec.id, company), cells);
        Ok(())
    }

    /// A many2one fallback whose target is gone reads empty, like a dangling override (
    /// decision 6): the existence of each target id is checked in the database, one statement per
    /// comodel, and only when a fallback names an id. Record rules do not apply.
    async fn clear_dangling_fallbacks(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        cells: &mut [Cell],
    ) -> Result<()> {
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
        for (comodel, ids) in by_comodel {
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
        Ok(())
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
            .and_then(|cells| cells.get(field.index()))
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
        refuse_draft_link(spec, field, scalar)?;
        // prepares the fallbacks and copies the origin's map (or the default) first
        self.ensure(record, field.id).await?;
        let base = self
            .cache
            .get(spec.id, field.id, id)
            .ok_or_else(|| Error::programming("a draft company map was not cached by ensure"))?;
        let (company, fallback) = self.property_context(record.env(), spec.id, field.id)?;
        crate::company_dependent::apply(&field.ty, &base, company, scalar, &fallback)
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

/// A company map stores database ids only: a many2one to an unsaved record has no id to store.
pub(crate) fn refuse_draft_link(
    spec: &ModelSpec,
    field: &erp_registry::FieldSpec,
    value: &Cell,
) -> Result<()> {
    if matches!(value, Cell::Id(raw) if *raw < 0) {
        return Err(Error::validation(format!(
            "{}.{}: a company-dependent many2one to an unsaved record is not supported until \
             a later release",
            spec.name, field.name
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
