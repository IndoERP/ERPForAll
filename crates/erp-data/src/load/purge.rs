//! Called by the module system after the modules' uninstall hooks and before it removes their
//! schema (the registry diff). The records the modules own in models that stay
//! installed are deleted; records of models that go away with the modules die with their tables,
//! and the schema records (`ir.model`, `ir.model.fields`, ...) are the module system's.
//!
//! After the records, the views users copied from the modules' views are deleted: views keyed
//! `<module>.…` that no real module staying installed names by external id
//! (`_remove_copied_views`,, called at), with the
//! views built on them.
//! The
//! pseudo-modules (`__export__`, `__import__`, `__cloc_exclude__`: `__…__` names) are not
//! modules: their external ids keep no copy.
//!
//! After the views, the selection `ondelete` policies of values the uninstalled modules added
//! (`selection_add`) are processed: for
//! remaining records of tables that stay, `set null`, `set default`, literal values (`'set <val>'`),
//! `cascade` (which unlinks matching records, removing their external IDs via the unlink hook), and
//! registered callbacks are executed. A `safe_write` savepoint attempts an ORM write and falls back
//! to direct SQL update when write overrides fail. Company-dependent selection fields are processed
//! across all company keys in their JSONB columns.

use std::collections::HashSet;

use erp_fields::{Error, Value};
use erp_orm::{PrincipalKind, SavepointOutcome, Transaction};

use super::{LoadError, LoadErrorKind};
use crate::model_data;

/// Models of the module system's own schema reflection: their external ids and
/// records are the module system's, not the data loader's.
pub const SCHEMA_MODELS: [&str; 5] = [
    "ir.model",
    "ir.model.fields",
    "ir.model.fields.selection",
    "ir.model.constraint",
    "ir.model.relation",
];

const CLOC_EXCLUDE: &str = "__cloc_exclude__";

/// What a purge did. `#[must_use]`: its lists name records a staying module lost, which the
/// caller reports (review T-5).
///
/// This is a plain output record, not a validated input or authorization token (DT-6).
/// Public fields let callers inspect and aggregate the result; `purge_module_data` maintains
/// the count relationships in reports it returns. Mutating a report does not change the purge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
#[must_use = "the report names views of staying modules the purge deleted; report them"]
pub struct PurgeReport {
    /// Records deleted, per run of one model, in deletion order (a model can appear more than once).
    /// A view run includes owned descendants its cascade deletes before their scheduled run.
    pub deleted: Vec<(String, usize)>,
    /// Records kept because an external id of a module that stays names them.
    pub kept_shared: usize,
    pub unavailable_models: Vec<String>,
    /// External-id rows the final statement removed (rows of kept records, of records that were
    /// already gone, and of unavailable models).
    pub xids_removed: usize,
    /// Views deleted as copies of the modules' views (keyed `<module>.…`, named by no staying
    /// module), the views built on them included.
    pub copied_views_removed: usize,
    /// Views keyed `<module>.…` kept because an external id of a staying module names them (a
    /// named view built on a removed copy is removed with it and counted in
    /// [`Self::copied_views_removed`]).
    pub copied_views_kept: usize,
    /// Of the views removed with a copy, those an external id of a staying module named, keyed
    /// `<module>.…` or not (a form view has no key), one entry per such external id, in view id
    /// order, recorded before the cascade (reviews A3-S3, A4-S1, R-5).
    pub copied_views_named_removed: Vec<CascadedNamedView>,
    /// Views deleted with a removed module's own view they were built on (the view model's
    /// `force_delete` cascade) that the purge would not have deleted
    /// otherwise: views of staying modules, without external id, or shared with a staying module
    /// (review S-4).
    pub views_cascaded: usize,
    /// Of [`Self::views_cascaded`], those an external id of a staying module named, one entry per
    /// such external id, in view id order, recorded before the cascade (review S-4). A shared
    /// view listed here is not counted in [`Self::kept_shared`]: it did not stay.
    pub views_named_cascaded: Vec<CascadedNamedView>,
}

/// A view an external id of a staying module named, deleted with the copied view or the removed
/// module's view it was built on ([`PurgeReport::copied_views_named_removed`],
/// [`PurgeReport::views_named_cascaded`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CascadedNamedView {
    /// The deleted view.
    pub view_id: i64,
    /// The external id (`module.name`) of the staying module that named it.
    pub xmlid: String,
}

fn shape(e: &tokio_postgres::Error) -> Error {
    Error::programming(format!("unexpected ir_model_data row: {e}"))
}

/// Delete the records `modules` own in models that stay installed, before the module system
/// removes their schema. See the module documentation.
///
/// # Errors
/// A [`LoadError`]: `Access` (kind kept through [`LoadError::orm_kind`]) unless the transaction
/// acts as the system principal with full elevation; the ORM's error of a record
/// that cannot be deleted; a database error.
pub async fn purge_module_data(
    tx: &mut Transaction,
    modules: &[&str],
) -> Result<PurgeReport, LoadError> {
    let principal = tx.env().principal().clone();
    if principal.kind() != PrincipalKind::System || !principal.elevation().is_full() {
        return Err(orm_error(Error::access(
            "Administrator access is required to uninstall a module",
        )));
    }
    let registry = std::sync::Arc::clone(tx.registry());
    let Some(imd) = registry
        .model(model_data::MODEL)
        .map(|s| erp_sql::quote(&s.table))
    else {
        // no external ids can exist; copied views and selection ondelete still can
        let mut report = PurgeReport::default();
        remove_copied_views(tx, &registry, modules, None, &mut report).await?;
        process_selection_ondelete(tx, &registry, modules, &mut report).await?;
        return Ok(report);
    };
    let names: Vec<&str> = modules.to_vec();
    let mut report = PurgeReport::default();

    // 1. the modules' external ids with a record, newest first (`:2562`)
    let sql = format!(
        "SELECT id, model, res_id FROM {imd} \
         WHERE module = ANY($1) AND res_id IS NOT NULL ORDER BY id DESC"
    );
    let mut xid_rows: Vec<i64> = Vec::new();
    // the records in newest-external-id-first order
    let mut ordered: Vec<(String, i64)> = Vec::new();
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    for row in &tx.query(&sql, &[&names]).await? {
        let id: i64 = row.try_get(0).map_err(|e| orm_error(shape(&e)))?;
        let model: String = row.try_get(1).map_err(|e| orm_error(shape(&e)))?;
        let res_id: i64 = row.try_get(2).map_err(|e| orm_error(shape(&e)))?;
        if SCHEMA_MODELS.contains(&model.as_str()) {
            continue;
        }
        // A model stays when a declaration of an addon that is not being removed is still behind
        // it; the core addons are always there, but they only add to models other addons define.
        let Some(spec) = registry.model(&model) else {
            if !report.unavailable_models.contains(&model) {
                tracing::warn!(
                    target: "erp::data",
                    "uninstall: external ids of model '{model}' skipped, the model is not in the registry"
                );
                report.unavailable_models.push(model.clone());
            }
            xid_rows.push(id);
            continue;
        };
        if !spec.stays_without_addons(&names) {
            // the table goes with the module
            continue;
        }
        if !spec.has_table() {
            if !report.unavailable_models.contains(&model) {
                tracing::warn!(
                    target: "erp::data",
                    "uninstall: external ids of model '{model}' skipped, the model has no table"
                );
                report.unavailable_models.push(model.clone());
            }
            xid_rows.push(id);
            continue;
        }
        xid_rows.push(id);
        if seen.insert((model.clone(), res_id)) {
            ordered.push((model, res_id));
        }
    }

    // 2. shared ownership: a record another module (that stays) names is kept (`:2573-2579`)
    let mut shared: HashSet<(String, i64)> = HashSet::new();
    if !seen.is_empty() {
        let pair_models: Vec<&str> = seen.iter().map(|(m, _)| m.as_str()).collect();
        let pair_ids: Vec<i64> = seen.iter().map(|(_, r)| *r).collect();
        let sql = format!(
            "SELECT DISTINCT model, res_id FROM {imd} \
             WHERE (model, res_id) IN (SELECT * FROM unnest($1::text[], $2::int8[])) \
             AND module <> ALL($3) AND module <> $4"
        );
        for row in &tx
            .query(&sql, &[&pair_models, &pair_ids, &names, &CLOC_EXCLUDE])
            .await?
        {
            let model: String = row.try_get(0).map_err(|e| orm_error(shape(&e)))?;
            let res_id: i64 = row.try_get(1).map_err(|e| orm_error(shape(&e)))?;
            shared.insert((model, res_id));
        }
    }

    let mut runs: Vec<(String, Vec<i64>)> = Vec::new();
    // the views the purge keeps (shared) and those it deletes itself: a view cascade reports
    // the views it takes beyond these (review S-4)
    let mut kept_views: HashSet<i64> = HashSet::new();
    let mut purged_views: Vec<i64> = Vec::new();
    for (model, id) in ordered {
        if shared.contains(&(model.clone(), id)) {
            report.kept_shared += 1;
            if model == VIEW_MODEL {
                kept_views.insert(id);
            }
            continue;
        }
        if model == VIEW_MODEL {
            purged_views.push(id);
        }
        match runs.last_mut() {
            Some((last, ids)) if *last == model => ids.push(id),
            _ => runs.push((model, vec![id])),
        }
    }
    let env = tx.env();
    for (model, ids) in runs {
        let records = env
            .records(&model)
            .map_err(orm_error)?
            .with_context([("force_delete", Value::Bool(true))])
            .browse(ids)
            .map_err(orm_error)?;
        let alive = records.exists(tx).await.map_err(orm_error)?;
        if alive.is_empty() {
            continue;
        }
        let mut count = alive.ids().len();
        if model == VIEW_MODEL {
            let owned_descendants = cascaded_views(
                tx,
                &registry,
                CascadeScope {
                    imd: &imd,
                    modules: &names,
                    ids: &alive.ids(),
                    purged: &purged_views,
                    kept: &kept_views,
                },
                &mut report,
            )
            .await?;
            count = count.checked_add(owned_descendants).ok_or_else(|| {
                orm_error(Error::invariant("the purge's owned-view count overflowed"))
            })?;
        }
        alive.unlink(tx).await.map_err(orm_error)?;
        report.deleted.push((model, count));
    }

    // 3b. the views copied from the modules' views, after their records (`ondelete="restrict"`
    // of `inherit_id` would refuse otherwise) and before their schema goes
    remove_copied_views(tx, &registry, modules, Some(&imd), &mut report).await?;

    // 3c. process selection ondelete policies of values added by the removed modules
    process_selection_ondelete(tx, &registry, modules, &mut report).await?;

    // 4. the modules' external ids that pointed at kept records, at records that were already
    // gone, or at models that are not available
    if !xid_rows.is_empty() {
        let sql = format!("DELETE FROM {imd} WHERE id = ANY($1)");
        let removed = tx.execute(&sql, &[&xid_rows]).await?;
        report.xids_removed = usize::try_from(removed).map_err(|_| {
            orm_error(Error::programming(format!(
                "the purge removed {removed} external-id rows, more than this platform counts"
            )))
        })?;
        tx.invalidate_model(model_data::MODEL, None)
            .await
            .map_err(orm_error)?;
    }
    Ok(report)
}

/// Process `ondelete` policies of selection values added by uninstalled modules.
/// Runs after the module's own records and copied views are deleted; operates on the remaining
/// records in tables that stay.
async fn process_selection_ondelete(
    tx: &mut Transaction,
    registry: &erp_registry::Registry,
    modules: &[&str],
    report: &mut PurgeReport,
) -> Result<(), LoadError> {
    tx.flush().await.map_err(orm_error)?;

    for spec in registry.models() {
        // a table that leaves with the modules takes its values with it
        if spec.is_abstract() || !spec.has_table() || !spec.stays_without_addons(modules) {
            continue;
        }

        // A stored selection_add field of an uninstalled module with no merged field spec means
        // the registry is inconsistent: `selection_additions_from_addons` skips it, and skipping
        // would leave stale values in a staying table while the uninstall reports success.
        for decl in spec.mro.iter().filter(|d| modules.contains(&d.addon)) {
            for field in decl.fields {
                let erp_registry::KindDecl::Selection { add, .. } = &field.kind else {
                    continue;
                };
                if !add.is_empty() && spec.field(field.name).is_none() {
                    return Err(orm_error(Error::programming(format!(
                        "selection_add field {}.{} declared by an uninstalled module has no merged field spec",
                        spec.name, field.name
                    ))));
                }
            }
        }

        for (field_name, values) in spec.selection_additions_from_addons(modules) {
            let field_spec = spec.field(field_name).ok_or_else(|| {
                orm_error(Error::programming(format!(
                    "selection_add field {}.{field_name} declared by an uninstalled module has no merged field spec",
                    spec.name
                )))
            })?;
            for val in values {
                // A value a staying module still declares is not removed.
                let is_kept = spec
                    .mro
                    .iter()
                    .filter(|d| !modules.contains(&d.addon))
                    .any(|d| {
                        d.fields.iter().any(|f| {
                            f.name == field_name
                                && match f.kind {
                                    erp_registry::KindDecl::Selection { values, add, .. } => {
                                        values.is_some_and(|vs| vs.iter().any(|(v, _)| *v == val))
                                            || add.iter().any(|(v, _)| *v == val)
                                    }
                                    _ => false,
                                }
                        })
                    });
                if is_kept {
                    continue;
                }

                // Look up policy for val directly on field_spec (SEL-D5).
                let policy = field_spec
                    .selection_ondelete
                    .get(val)
                    .cloned()
                    .unwrap_or(erp_fields::SelectionOnDelete::SetNull);

                if field_spec.company_dependent {
                    process_company_dependent_selection(tx, spec, field_spec, val, policy, report)
                        .await?;
                } else {
                    process_normal_selection(tx, spec, field_spec, val, policy, report).await?;
                }
            }
        }
    }
    Ok(())
}

async fn process_normal_selection(
    tx: &mut Transaction,
    spec: &erp_registry::ModelSpec,
    field_spec: &erp_registry::FieldSpec,
    val: &str,
    policy: erp_fields::SelectionOnDelete,
    report: &mut PurgeReport,
) -> Result<(), LoadError> {
    tx.flush().await.map_err(orm_error)?;

    let sql = format!(
        "SELECT id FROM {} WHERE {} = $1",
        erp_sql::quote(&spec.table),
        erp_sql::quote(field_spec.name),
    );
    let rows = tx.query(&sql, &[&val]).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut ids: Vec<i64> = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = row.try_get(0).map_err(|e| orm_error(shape(&e)))?;
        ids.push(id);
    }
    let env = tx.env();

    match policy {
        erp_fields::SelectionOnDelete::Cascade => {
            let records = env
                .records(spec.name)
                .map_err(orm_error)?
                .with_context([("force_delete", Value::Bool(true))])
                .browse(ids)
                .map_err(orm_error)?;
            let alive = records.exists(tx).await.map_err(orm_error)?;
            if !alive.is_empty() {
                let count = alive.ids().len();
                alive.unlink(tx).await.map_err(orm_error)?;
                report.deleted.push((spec.name.to_owned(), count));
            }
        }
        erp_fields::SelectionOnDelete::Callback(cb_name) => {
            let rec_ids = ids
                .iter()
                .map(|id| {
                    erp_fields::RecId::from_real(*id)
                        .ok_or_else(|| orm_error(Error::programming("invalid id")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            tx.call_selection_callback(&cb_name, spec.name, &rec_ids)
                .await
                .map_err(orm_error)?;
        }
        erp_fields::SelectionOnDelete::SetNull => {
            safe_write(tx, spec, field_spec.name, &ids, Value::False, None).await?;
        }
        erp_fields::SelectionOnDelete::SetDefault => {
            let empty = env
                .records(spec.name)
                .map_err(orm_error)?
                .browse(Vec::<i64>::new())
                .map_err(orm_error)?;
            let defaults = empty
                .default_get(tx, &[field_spec.name])
                .await
                .map_err(orm_error)?;
            let def_val = defaults.get(field_spec.name).cloned().ok_or_else(|| {
                orm_error(Error::programming(format!(
                    "selection field {}.{} uses 'set default' but default_get returned no default",
                    spec.name, field_spec.name
                )))
            })?;
            safe_write(tx, spec, field_spec.name, &ids, def_val, None).await?;
        }
        erp_fields::SelectionOnDelete::SetValue(target) => {
            safe_write(tx, spec, field_spec.name, &ids, Value::str(&target), None).await?;
        }
        _ => {
            return Err(orm_error(Error::programming(
                "unsupported selection ondelete policy",
            )));
        }
    }
    Ok(())
}

async fn safe_write(
    tx: &mut Transaction,
    spec: &erp_registry::ModelSpec,
    field_name: &str,
    ids: &[i64],
    value: Value,
    company_id: Option<i64>,
) -> Result<(), LoadError> {
    if ids.is_empty() {
        return Ok(());
    }

    let env = match company_id {
        Some(cid) => tx.env().with_company(cid).map_err(orm_error)?,
        None => tx.env(),
    };
    let records = env
        .records(spec.name)
        .map_err(orm_error)?
        .browse(ids.to_vec())
        .map_err(orm_error)?;
    let val_clone = value.clone();
    let res = tx
        .savepoint_outcome(async |stx: &mut Transaction| {
            let mut map = std::collections::BTreeMap::new();
            map.insert(field_name.to_owned(), val_clone);
            records.write_values(stx, &map).await
        })
        .await;

    match res {
        SavepointOutcome::Committed(()) => Ok(()),
        SavepointOutcome::RolledBack(err) => {
            tracing::warn!(
                target: "erp::data",
                "Could not fulfill ondelete action for field {}.{} via ORM write ({:?}: {}), attempting ORM bypass...",
                spec.name, field_name, err.kind(), err.message()
            );
            match company_id {
                None => {
                    let sql = format!(
                        "UPDATE {} SET {} = $1 WHERE id = ANY($2)",
                        erp_sql::quote(&spec.table),
                        erp_sql::quote(field_name),
                    );
                    match &value {
                        Value::False => {
                            let null_str: Option<&str> = None;
                            tx.execute(&sql, &[&null_str, &ids]).await?;
                        }
                        Value::Str(s) => {
                            let s_ref = s.as_ref();
                            tx.execute(&sql, &[&s_ref, &ids]).await?;
                        }
                        other => {
                            return Err(orm_error(Error::programming(format!(
                                "unexpected selection ondelete value {other:?}"
                            ))));
                        }
                    }
                }
                Some(cid) => {
                    let cid_str = cid.to_string();
                    let json_val = match &value {
                        Value::False => serde_json::Value::Null,
                        Value::Str(s) => serde_json::Value::String(s.to_string()),
                        other => {
                            return Err(orm_error(Error::programming(format!(
                                "unexpected company-dependent selection ondelete value {other:?}"
                            ))));
                        }
                    };
                    let sql = format!(
                        "UPDATE {} SET {} = jsonb_set(COALESCE({}, '{{}}'::jsonb), ARRAY[$1], $2, true) WHERE id = ANY($3)",
                        erp_sql::quote(&spec.table),
                        erp_sql::quote(field_name),
                        erp_sql::quote(field_name),
                    );
                    tx.execute(&sql, &[&cid_str, &json_val, &ids]).await?;
                }
            }
            tx.invalidate_model(spec.name, None)
                .await
                .map_err(orm_error)?;
            Ok(())
        }
        SavepointOutcome::SetupFailed(err) => {
            // Setup failure: transaction was already cancelled, poisoned, or pre-flush failed.
            // Never enter SQL fallback.
            Err(orm_error(err))
        }
        SavepointOutcome::RollbackFailed {
            work_error,
            rollback_error,
        } => {
            // Savepoint rollback failed: transaction is unusable. Never enter SQL fallback.
            let primary = if let Some(we) = work_error {
                Error::new(
                    rollback_error.kind(),
                    format!("{} (after: {})", rollback_error.message(), we.message()),
                )
            } else {
                rollback_error
            };
            Err(orm_error(primary))
        }
    }
}

async fn process_company_dependent_selection(
    tx: &mut Transaction,
    spec: &erp_registry::ModelSpec,
    field_spec: &erp_registry::FieldSpec,
    val: &str,
    policy: erp_fields::SelectionOnDelete,
    report: &mut PurgeReport,
) -> Result<(), LoadError> {
    tx.flush().await.map_err(orm_error)?;

    // Check for malformed JSONB (SEL-D7)
    let malformed_check = format!(
        "SELECT id FROM {} WHERE {} IS NOT NULL AND jsonb_typeof({}) != 'object' LIMIT 1",
        erp_sql::quote(&spec.table),
        erp_sql::quote(field_spec.name),
        erp_sql::quote(field_spec.name),
    );
    let bad_rows = tx.query(&malformed_check, &[]).await?;
    if !bad_rows.is_empty() {
        return Err(orm_error(Error::programming(format!(
            "malformed JSONB in company-dependent field {}.{}: expected JSON object",
            spec.name, field_spec.name
        ))));
    }

    // Find companies with matching values using SQL jsonb_each_text (SEL-D7)
    let comp_sql = format!(
        "SELECT DISTINCT j.key FROM {}, jsonb_each_text({}) AS j WHERE j.value = $1 ORDER BY j.key",
        erp_sql::quote(&spec.table),
        erp_sql::quote(field_spec.name),
    );
    let comp_rows = tx.query(&comp_sql, &[&val]).await?;
    if comp_rows.is_empty() {
        return Ok(());
    }

    let env = tx.env();
    for comp_row in comp_rows {
        let comp_key: String = comp_row.try_get(0).map_err(|e| orm_error(shape(&e)))?;
        let company_id: i64 = comp_key.parse().map_err(|_| {
            orm_error(Error::programming(format!(
                "invalid company id key '{comp_key}' in company-dependent field {}.{}",
                spec.name, field_spec.name
            )))
        })?;

        // Flush before each per-company raw id lookup (SEL-CLOSE-F2)
        tx.flush().await.map_err(orm_error)?;

        let id_sql = format!(
            "SELECT id FROM {} WHERE {} ->> $1 = $2",
            erp_sql::quote(&spec.table),
            erp_sql::quote(field_spec.name),
        );
        let id_rows = tx.query(&id_sql, &[&comp_key, &val]).await?;
        if id_rows.is_empty() {
            continue;
        }
        let mut ids: Vec<i64> = Vec::with_capacity(id_rows.len());
        for r in id_rows {
            let id: i64 = r.try_get(0).map_err(|e| orm_error(shape(&e)))?;
            ids.push(id);
        }

        match &policy {
            erp_fields::SelectionOnDelete::Cascade => {
                let records = env
                    .with_company(company_id)
                    .map_err(orm_error)?
                    .records(spec.name)
                    .map_err(orm_error)?
                    .with_context([("force_delete", Value::Bool(true))])
                    .browse(ids)
                    .map_err(orm_error)?;
                let alive = records.exists(tx).await.map_err(orm_error)?;
                if !alive.is_empty() {
                    let count = alive.ids().len();
                    alive.unlink(tx).await.map_err(orm_error)?;
                    report.deleted.push((spec.name.to_owned(), count));
                }
            }
            erp_fields::SelectionOnDelete::Callback(cb_name) => {
                let rec_ids = ids
                    .iter()
                    .map(|id| {
                        erp_fields::RecId::from_real(*id)
                            .ok_or_else(|| orm_error(Error::programming("invalid id")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                tx.call_selection_callback_in_company(
                    cb_name,
                    spec.name,
                    &rec_ids,
                    Some(company_id),
                )
                .await
                .map_err(orm_error)?;
            }
            erp_fields::SelectionOnDelete::SetNull => {
                safe_write(
                    tx,
                    spec,
                    field_spec.name,
                    &ids,
                    Value::False,
                    Some(company_id),
                )
                .await?;
            }
            erp_fields::SelectionOnDelete::SetDefault => {
                let empty = env
                    .with_company(company_id)
                    .map_err(orm_error)?
                    .records(spec.name)
                    .map_err(orm_error)?
                    .browse(Vec::<i64>::new())
                    .map_err(orm_error)?;
                let defaults = empty
                    .default_get(tx, &[field_spec.name])
                    .await
                    .map_err(orm_error)?;
                let def_val = defaults.get(field_spec.name).cloned().ok_or_else(|| {
                    orm_error(Error::programming(format!(
                        "selection field {}.{} uses 'set default' but default_get returned no default",
                        spec.name, field_spec.name
                    )))
                })?;
                match def_val {
                    Value::Str(_) | Value::False => {
                        safe_write(tx, spec, field_spec.name, &ids, def_val, Some(company_id))
                            .await?;
                    }
                    other => {
                        return Err(orm_error(Error::programming(format!(
                            "company-dependent selection field {}.{} has non-string default {:?}",
                            spec.name, field_spec.name, other
                        ))));
                    }
                }
            }
            erp_fields::SelectionOnDelete::SetValue(target) => {
                safe_write(
                    tx,
                    spec,
                    field_spec.name,
                    &ids,
                    Value::str(target),
                    Some(company_id),
                )
                .await?;
            }
            _ => {
                return Err(orm_error(Error::programming(
                    "unsupported selection ondelete policy",
                )));
            }
        }
    }

    tx.flush().await.map_err(orm_error)?;
    Ok(())
}

/// What [`cascaded_views`] looks at: the run of views about to be deleted, and the views the
/// purge deletes itself or keeps.
struct CascadeScope<'a> {
    /// The quoted `ir.model.data` table.
    imd: &'a str,
    /// The removed modules.
    modules: &'a [&'a str],
    /// The views of the run.
    ids: &'a [i64],
    /// Every view the purge deletes as a record of the modules.
    purged: &'a [i64],
    /// The views of the modules the purge keeps (shared with a staying module).
    kept: &'a HashSet<i64>,
}

/// Count and list the views the `force_delete` cascade of the views `scope.ids` will delete
/// beyond the purge's own (review S-4): every view built on them, at any depth, that is not one
/// of `scope.ids`; each external id of a staying real module naming one is listed. Descendants
/// the purge owns are returned as an additional deleted count for this run, not `views_cascaded`:
/// a later run's `exists` will find them already gone. A shared view the cascade takes is no
/// longer counted as kept.
/// One statement, before the unlink; the walk is a `UNION` recursion, so a cycle in `inherit_id`
/// ends.
async fn cascaded_views(
    tx: &mut Transaction,
    registry: &erp_registry::Registry,
    scope: CascadeScope<'_>,
    report: &mut PurgeReport,
) -> Result<usize, LoadError> {
    let Some(view) = registry.model(VIEW_MODEL) else {
        return Err(orm_error(Error::invariant(
            "the purge deletes ir.ui.view records, but the registry has no ir.ui.view model",
        )));
    };
    let table = erp_sql::quote(&view.table);
    let imd = scope.imd;
    let sql = format!(
        "WITH RECURSIVE below(id) AS ( \
           SELECT c.id FROM {table} c WHERE c.inherit_id = ANY($1) AND NOT (c.id = ANY($1)) \
           UNION \
           SELECT c.id FROM {table} c JOIN below b ON c.inherit_id = b.id \
            WHERE NOT (c.id = ANY($1))) \
         SELECT b.id, b.id = ANY($4), ARRAY(SELECT d.module || '.' || d.name FROM {imd} d WHERE d.model = $2 \
                AND d.res_id = b.id AND d.module <> ALL($3) \
                AND d.module NOT LIKE '\\_\\_%\\_\\_' ORDER BY d.id) \
           FROM below b ORDER BY b.id"
    );
    let rows = tx
        .query(
            &sql,
            &[&scope.ids, &VIEW_MODEL, &scope.modules, &scope.purged],
        )
        .await?;
    let mut owned = 0usize;
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(|e| orm_error(shape(&e)))?;
        let is_owned: bool = row.try_get(1).map_err(|e| orm_error(shape(&e)))?;
        if is_owned {
            owned = owned.checked_add(1).ok_or_else(|| {
                orm_error(Error::invariant(
                    "the purge's owned-descendant count overflowed",
                ))
            })?;
            continue;
        }
        let names: Vec<String> = row.try_get(2).map_err(|e| orm_error(shape(&e)))?;
        report.views_cascaded += 1;
        if scope.kept.contains(&id) {
            report.kept_shared = report.kept_shared.checked_sub(1).ok_or_else(|| {
                orm_error(Error::invariant(format!(
                    "the purge counted shared view {id} as kept without counting it"
                )))
            })?;
        }
        for xmlid in names {
            report
                .views_named_cascaded
                .push(CascadedNamedView { view_id: id, xmlid });
        }
    }
    Ok(owned)
}

fn key_patterns(modules: &[&str]) -> Vec<String> {
    modules
        .iter()
        .map(|m| {
            let mut pattern = String::with_capacity(m.len() + 2);
            for c in m.chars() {
                if matches!(c, '\\' | '%' | '_') {
                    pattern.push('\\');
                }
                pattern.push(c);
            }
            pattern.push_str(".%");
            pattern
        })
        .collect()
}

/// Delete the views keyed `<module>.…` of `modules` that no external id of a staying real module
/// names, archived ones included, with the views built on them (the view model's `force_delete`
/// cascade): one statement for the ids, the counts and the named views the cascade takes (keyed
/// or not), one `unlink`.
async fn remove_copied_views(
    tx: &mut Transaction,
    registry: &erp_registry::Registry,
    modules: &[&str],
    imd: Option<&str>,
    report: &mut PurgeReport,
) -> Result<(), LoadError> {
    // no view model, or one without a table: no view is stored, so none was copied
    let Some(view) = registry.model(VIEW_MODEL) else {
        return Ok(());
    };
    if !view.has_table() {
        return Ok(());
    }
    // a stored view model without `key` cannot tell the copies: refused, never skipped
    // (review F-5)
    if !view.field("key").is_some_and(|f| f.store) {
        return Err(orm_error(Error::programming(format!(
            "{VIEW_MODEL} has a table but no stored 'key' field: the views copied from the \
             removed modules' views cannot be found"
        ))));
    }
    let patterns = key_patterns(modules);
    let table = erp_sql::quote(&view.table);
    // `names`: the external ids of modules that stay (not a pseudo-module `__…__`) naming it;
    // `named` when there is one
    let names = match imd {
        Some(imd) => format!(
            "ARRAY(SELECT d.module || '.' || d.name FROM {imd} d WHERE d.model = $2 \
             AND d.res_id = v.id AND d.module <> ALL($3) \
             AND d.module NOT LIKE '\\_\\_%\\_\\_' ORDER BY d.id)"
        ),
        None => "ARRAY[]::text[]".to_owned(),
    };
    // the copies, whether each is named, whether the cascade deletes it anyway (a named view
    // built on a copy goes with it: review A2-D3), and how many views the cascade deletes; then
    // every other view the cascade deletes that a staying module names, whatever its key (a
    // form view has none; review R-5), so each deleted named view is listed
    let sql = format!(
        "WITH RECURSIVE keyed AS ( \
           SELECT k.id, cardinality(k.names) > 0 AS named, k.names FROM ( \
             SELECT v.id, {names} AS names FROM {table} v WHERE v.\"key\" LIKE ANY($1)) k), \
         gone(id) AS ( \
           SELECT id FROM keyed WHERE NOT named \
           UNION \
           SELECT c.id FROM {table} c JOIN gone g ON c.inherit_id = g.id), \
         below AS ( \
           SELECT v.id, {names} AS names FROM {table} v \
            WHERE v.id IN (SELECT id FROM gone) AND v.id NOT IN (SELECT id FROM keyed)) \
         SELECT r.id, r.named, r.cascaded, (SELECT count(*) FROM gone), r.names FROM ( \
           SELECT k.id, k.named, k.id IN (SELECT id FROM gone) AS cascaded, k.names FROM keyed k \
           UNION ALL \
           SELECT b.id, true, true, b.names FROM below b WHERE cardinality(b.names) > 0) r \
          ORDER BY r.id"
    );
    let rows = match imd {
        Some(_) => tx.query(&sql, &[&patterns, &VIEW_MODEL, &modules]).await?,
        None => tx.query(&sql, &[&patterns]).await?,
    };
    let mut ids: Vec<i64> = Vec::new();
    let mut gone: i64 = 0;
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(|e| orm_error(shape(&e)))?;
        let named: bool = row.try_get(1).map_err(|e| orm_error(shape(&e)))?;
        let cascaded: bool = row.try_get(2).map_err(|e| orm_error(shape(&e)))?;
        gone = row.try_get(3).map_err(|e| orm_error(shape(&e)))?;
        match (named, cascaded) {
            // named, and not built on a copy: kept
            (true, false) => report.copied_views_kept += 1,
            // named, but under a copy (keyed or not): the cascade deletes it (counted in `gone`);
            // each external id that named it is reported, so it is not silent (reviews A3-S3,
            // A4-S1, R-5)
            (true, true) => {
                let names: Vec<String> = row.try_get(4).map_err(|e| orm_error(shape(&e)))?;
                for xmlid in names {
                    report
                        .copied_views_named_removed
                        .push(CascadedNamedView { view_id: id, xmlid });
                }
            }
            (false, _) => ids.push(id),
        }
    }
    if ids.is_empty() {
        return Ok(());
    }
    let views = tx
        .env()
        .records(VIEW_MODEL)
        .map_err(orm_error)?
        .with_context([
            ("force_delete", Value::Bool(true)),
            ("active_test", Value::Bool(false)),
        ])
        .browse(ids.iter().copied())
        .map_err(orm_error)?;
    views.unlink(tx).await.map_err(orm_error)?;
    report.copied_views_removed = usize::try_from(gone).map_err(|_| {
        orm_error(Error::programming(format!(
            "the purge counted {gone} copied views, which this platform cannot count"
        )))
    })?;
    Ok(())
}

/// The view model.
const VIEW_MODEL: &str = "ir.ui.view";

fn orm_error(error: Error) -> LoadError {
    LoadError::without_place(LoadErrorKind::Orm(error))
}
