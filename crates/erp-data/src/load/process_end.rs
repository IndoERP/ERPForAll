//! After every module of an operation has loaded, the records of the updated modules whose
//! external ids the data no longer names are deleted: the data of a new version replaces the old
//! one. An external id is **obsolete** when it belongs to an updated module, points at a record,
//! is not `noupdate`, was not loaded by the session and was not written by this transaction
//! (a hook or a data-callable method binding it through `update_xmlids`: `write_date` is the
//! transaction's `now()`), and its model is in the registry.
//!
//! Two classes never count: the schema records the module system's reflection writes
//! (`ir.model`, `ir.model.fields`, `ir.model.fields.selection`, `ir.model.constraint`,
//! `ir.model.relation`): their external ids carry no `write_date` and no data file names them,
//! and the module system removes a module's schema itself. And a row without
//! `write_date` is judged like any other: the test is `IS DISTINCT FROM now()`, which keeps NULL
//! in the candidates, never `write_date < now()`, which would skip it.
//!
//! `_inherits` parents of children that were loaded stay (`:2744-2768`),
//! with their external id.

use std::collections::{HashMap, HashSet};

use erp_fields::{Error, Value};
use erp_orm::{PrincipalKind, Transaction};

use super::{LoadError, LoadSession};
use crate::model_data;

use super::purge::SCHEMA_MODELS;

/// A run of consecutive records (newest external id first) of one model and module prefix.
struct Run<'a> {
    model: &'a str,
    module: &'a str,
    ids: Vec<i64>,
}

/// An external id that is a candidate for removal.
struct Candidate {
    id: i64,
    module: String,
    model: String,
    res_id: i64,
}

fn shape(e: &tokio_postgres::Error) -> Error {
    Error::programming(format!("unexpected ir_model_data row: {e}"))
}

fn cell<'a, T: tokio_postgres::types::FromSql<'a>>(
    row: &'a tokio_postgres::Row,
    idx: usize,
) -> Result<T, Error> {
    row.try_get(idx).map_err(|e| shape(&e))
}

/// Delete the obsolete data of `updated`.
pub(crate) async fn run(
    session: &mut LoadSession,
    tx: &mut Transaction,
    updated: &[&str],
) -> Result<(), LoadError> {
    // the deletes run with full elevation: only the System principal may ask for them, like the
    // purge
    let principal = tx.env().principal().clone();
    if principal.kind() != PrincipalKind::System || !principal.elevation().is_full() {
        return Err(LoadError::from(Error::access(
            "Administrator access is required to complete a module update",
        )));
    }
    if updated.is_empty() {
        return Ok(());
    }
    delete_obsolete(session, tx, updated)
        .await
        .map_err(LoadError::from)
}

#[allow(
    clippy::too_many_lines,
    reason = "the steps of one cleanup, in the reference ERP's order"
)]
async fn delete_obsolete(
    session: &LoadSession,
    tx: &mut Transaction,
    updated: &[&str],
) -> Result<(), Error> {
    let registry = std::sync::Arc::clone(tx.registry());
    // without the model there is no external id to read: nothing was ever loaded
    let Some(imd_spec) = registry.model(model_data::MODEL) else {
        return Ok(());
    };
    let imd = erp_sql::quote(&imd_spec.table);
    let now = erp_sql::NOW_UTC;
    let modules: Vec<&str> = updated.to_vec();
    let schema: Vec<&str> = SCHEMA_MODELS.to_vec();

    // 1. the candidates, newest first
    tx.flush().await?;
    let sql = format!(
        "SELECT id, module, name, model, res_id FROM {imd} \
         WHERE module = ANY($1) AND res_id IS NOT NULL AND NOT COALESCE(noupdate, false) \
         AND write_date IS DISTINCT FROM {now} \
         AND model <> ALL($2) ORDER BY id DESC"
    );
    let rows = tx.query(&sql, &[&modules, &schema]).await?;
    let mut candidates: Vec<Candidate> = Vec::new();
    for row in &rows {
        let module: String = cell(row, 1)?;
        let name: String = cell(row, 2)?;
        let model: String = cell(row, 3)?;
        if registry.model(&model).is_none_or(|m| !m.has_table()) {
            continue;
        }
        if session.is_loaded_key(&module, &name) {
            continue;
        }
        candidates.push(Candidate {
            id: cell(row, 0)?,
            module,
            model,
            res_id: cell(row, 4)?,
        });
    }
    if candidates.is_empty() {
        return Ok(());
    }

    // 2. `_inherits` parents of children that were loaded stay
    let mut children: HashMap<&str, Vec<(&erp_registry::ModelSpec, &str)>> = HashMap::new();
    for child in registry.models() {
        if child.is_abstract() || !child.has_table() {
            continue;
        }
        for &(parent, link) in &child.inherits {
            let parent = registry.model_by_id(parent).name;
            children
                .entry(parent)
                .or_default()
                .push((child, child.field_by_id(link).name));
        }
    }
    let mut parent_ids: HashMap<&str, Vec<i64>> = HashMap::new();
    for c in &candidates {
        if children.contains_key(c.model.as_str()) {
            parent_ids
                .entry(c.model.as_str())
                .or_default()
                .push(c.res_id);
        }
    }
    let mut kept: HashSet<(String, i64)> = HashSet::new();
    for (parent, ids) in &parent_ids {
        for (child, link) in &children[parent] {
            let (link, child_table) = (erp_sql::quote(link), erp_sql::quote(&child.table));
            let sql = format!(
                "SELECT c.{link}, d.module, d.name, (d.write_date IS NOT DISTINCT FROM {now}) \
                 FROM {child_table} c JOIN {imd} d ON d.model = $1 AND d.res_id = c.id \
                 WHERE c.{link} = ANY($2)"
            );
            for row in &tx.query(&sql, &[&child.name, ids]).await? {
                let module: String = cell(row, 1)?;
                let name: String = cell(row, 2)?;
                let fresh: Option<bool> = cell(row, 3)?;
                if fresh == Some(true) || session.is_loaded_key(&module, &name) {
                    let parent_id: i64 = cell(row, 0)?;
                    kept.insert(((*parent).to_owned(), parent_id));
                }
            }
        }
    }
    candidates.retain(|c| !kept.contains(&(c.model.clone(), c.res_id)));
    if candidates.is_empty() {
        return Ok(());
    }

    // 3. a record that has an external id which is not a candidate loses only its candidate ids
    let candidate_ids: HashSet<i64> = candidates.iter().map(|c| c.id).collect();
    let models: Vec<&str> = candidates.iter().map(|c| c.model.as_str()).collect();
    let res_ids: Vec<i64> = candidates.iter().map(|c| c.res_id).collect();
    let sql = format!(
        "SELECT id, model, res_id FROM {imd} \
         WHERE (model, res_id) IN (SELECT * FROM unnest($1::text[], $2::int8[]))"
    );
    let mut owned_elsewhere: HashSet<(String, i64)> = HashSet::new();
    for row in &tx.query(&sql, &[&models, &res_ids]).await? {
        let id: i64 = cell(row, 0)?;
        if !candidate_ids.contains(&id) {
            owned_elsewhere.insert((cell(row, 1)?, cell(row, 2)?));
        }
    }

    let mut stale_ids: Vec<i64> = Vec::new();
    let mut runs: Vec<Run<'_>> = Vec::new();
    let mut seen: HashSet<(&str, i64)> = HashSet::new();
    for c in &candidates {
        stale_ids.push(c.id);
        if owned_elsewhere.contains(&(c.model.clone(), c.res_id)) {
            continue;
        }
        if !seen.insert((c.model.as_str(), c.res_id)) {
            continue;
        }
        match runs.last_mut() {
            Some(run) if run.model == c.model && run.module == c.module => run.ids.push(c.res_id),
            _ => runs.push(Run {
                model: c.model.as_str(),
                module: c.module.as_str(),
                ids: vec![c.res_id],
            }),
        }
    }

    // 4. delete the records run by run, then the external-id rows
    let env = tx.env().sudo(true);
    for run in runs {
        let records = env.records(run.model)?;
        // a record that is already gone only loses its external id
        let alive = records.browse(run.ids)?.exists(tx).await?;
        if alive.is_empty() {
            continue;
        }
        records
            .with_context([
                ("module", Value::str(run.module)),
                ("force_delete", Value::Bool(true)),
            ])
            .browse(alive.ids())?
            .unlink(tx)
            .await?;
    }
    // `model_data::UNLINK_HOOK` removes the external ids of unlinked records when it is registered;
    // this statement also covers the rows of records that were already gone and ORMs without it
    let sql = format!("DELETE FROM {imd} WHERE id = ANY($1)");
    tx.execute(&sql, &[&stale_ids]).await?;
    tx.invalidate_model(model_data::MODEL, None).await?;
    Ok(())
}
