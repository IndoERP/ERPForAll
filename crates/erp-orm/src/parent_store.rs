//! `_parent_store` (CRUD §4.11;…058).
//!
//! `parent_path` holds the ids from the root to the record, each followed by `/` (`42/63/84/`).
//! The ORM maintains it in SQL, immediately — never through `create`/`write` values, which it
//! ignores:
//!
//! - **create**: one statement computes the paths of the whole batch, parents of the batch first
//!   (recursive over the batch), from the parents' paths read `FOR SHARE`; the cache takes them
//!   before the create's constraints run (an INSERT plus one statement). A parent
//!   without a path is an error, never a silent root;
//! - **write of the parent field**: the records whose parent really changes are moved.
//! - **schema**: the paths are indexed in byte order (`COLLATE "C"`), so ranges and prefixes use
//!   the index whatever the database collation; a `parent_path` column added to an existing table
//!   is filled from the roots in one recursive statement, and rows left without a path (a cycle
//!   in the data) are an error (a);
//! - **deletion**: nothing specific, the parent many2one's `ondelete` applies.
//!
//! DML runs under a cancellation guard (a dropped future poisons the transaction).
//! Dependents of `parent_path` are notified whenever paths change. `child_of` / `parent_of` on
//! the parent field use the paths (`crate::hierarchy`).

use std::sync::Arc;

use erp_fields::{Cell, Error, RecId, Result, Storage};
use erp_registry::ModelSpec;

use crate::Transaction;
use crate::records::Records;

fn decode_paths(rows: &[tokio_postgres::Row]) -> Result<Vec<(RecId, Option<String>)>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let decode = |e: tokio_postgres::Error| {
            Error::new(
                erp_fields::ErrorKind::Database,
                "could not decode a parent path",
            )
            .with_source(e)
        };
        let raw: i64 = row.try_get(0).map_err(decode)?;
        let path: Option<String> = row.try_get(1).map_err(decode)?;
        if let Some(id) = RecId::from_real(raw) {
            out.push((id, path));
        }
    }
    Ok(out)
}

/// The ids in a parent path (`"42/63/"` → 42, 63).
pub(crate) fn path_ids(path: &str) -> Vec<i64> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

fn no_path(spec: &ModelSpec, ids: &[RecId]) -> Error {
    Error::programming(format!(
        "{}: parent_path is empty for {}({}): the parent store is inconsistent (a cycle or rows \
         written outside the ORM); rebuild it before moving or creating records under them",
        spec.table,
        spec.name,
        ids.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

impl Transaction {
    /// Cache the paths returned by a path statement and notify their dependents; a row without a
    /// path is an error.
    async fn take_paths(
        &mut self,
        spec: &ModelSpec,
        rows: Vec<(RecId, Option<String>)>,
    ) -> Result<()> {
        let Some(path) = spec.parent_path else {
            return Ok(());
        };
        let empty: Vec<RecId> = rows
            .iter()
            .filter(|(_, p)| p.is_none())
            .map(|(id, _)| *id)
            .collect();
        if !empty.is_empty() {
            return Err(no_path(spec, &empty));
        }
        let mut changed: Vec<RecId> = Vec::with_capacity(rows.len());
        let mc = self.cache.model_mut(spec.id);
        for (id, p) in rows {
            let cell = p.map_or(Cell::Null, |p| Cell::Str(p.into()));
            if mc.get(path, id).is_some_and(|c| c.same_value(&cell)) {
                continue;
            }
            mc.set(path, Storage::Str, id, cell, false)?;
            changed.push(id);
        }
        self.modified(spec.id, vec![(path, changed)]).await
    }

    /// The paths of records just created (one statement for the batch, parents of the batch
    /// first).
    pub(crate) async fn parent_store_create(&mut self, created: &Records) -> Result<()> {
        let registry = Arc::clone(created.env().registry());
        let spec = registry.model_by_id(created.model_id());
        let (Some(_), Some(parent)) = (spec.parent_path, spec.parent_name) else {
            return Ok(());
        };
        let ids: Vec<i64> = created
            .rec_ids()
            .iter()
            .filter_map(|i| i.as_real())
            .map(|i| i.get())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let sql =
            erp_sql::relational::parent_paths_of_new(&spec.table, spec.field_by_id(parent).name);
        let guard = self.guard();
        let result = async {
            let rows = self.query(&sql, &[&ids]).await?;
            let rows = decode_paths(&rows)?;
            if rows.len() != ids.len() {
                let got: Vec<RecId> = rows.iter().map(|(id, _)| *id).collect();
                let missing: Vec<RecId> = ids
                    .iter()
                    .filter_map(|i| RecId::from_real(*i))
                    .filter(|i| !got.contains(i))
                    .collect();
                return Err(no_path(spec, &missing));
            }
            self.take_paths(spec, rows).await
        }
        .await;
        guard.disarm();
        result
    }

    /// Before the parent field of `records` becomes `new_parent`: the database records whose
    /// parent really changes. Takes the store's lock and locks the new parent and the moved
    /// records; a move under one of the moved records or their descendants is a `UserError`
    /// (nothing is written yet).
    pub(crate) async fn parent_store_moves(
        &mut self,
        records: &Records,
        new_parent: &Cell,
    ) -> Result<Vec<RecId>> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let (Some(_), Some(parent)) = (spec.parent_path, spec.parent_name) else {
            return Ok(Vec::new());
        };
        let real: Vec<RecId> = records
            .rec_ids()
            .iter()
            .copied()
            .filter(|i| !i.is_new())
            .collect();
        if real.is_empty() {
            return Ok(Vec::new());
        }
        let env = records.env().engine_sudo();
        let current = self.peek_cells(&env, spec.id, parent, &real).await?;
        let target = crate::relational::id_of(new_parent);
        let moved: Vec<RecId> = real
            .iter()
            .zip(current)
            .filter(|(_, cell)| crate::relational::id_of(cell) != target)
            .map(|(id, _)| *id)
            .collect();
        if moved.is_empty() {
            return Ok(moved);
        }
        // serialize the store's structure changes, and lock what this move reads: the paths
        // returned are current (a concurrent move would fail the lock with a serialization
        // error under REPEATABLE READ)
        let key = format!("erp_parent_store:{}", spec.table);
        self.query(erp_sql::relational::PARENT_STORE_LOCK, &[&key])
            .await?;
        let mut locked: Vec<i64> = moved.iter().map(|i| i.get()).collect();
        if let Some(t) = target {
            locked.push(t.get());
        }
        locked.sort_unstable();
        locked.dedup();
        let rows = self
            .query(
                &erp_sql::relational::lock_parent_paths(&spec.table),
                &[&locked],
            )
            .await?;
        let paths = decode_paths(&rows)?;
        let empty: Vec<RecId> = paths
            .iter()
            .filter(|(_, p)| p.is_none())
            .map(|(id, _)| *id)
            .collect();
        if !empty.is_empty() {
            return Err(no_path(spec, &empty));
        }
        if let Some(target) = target {
            // the new parent's path: a moved record in it is a loop
            let mut above: Vec<i64> = paths
                .iter()
                .find(|(id, _)| *id == target)
                .and_then(|(_, p)| p.as_deref())
                .map(path_ids)
                .unwrap_or_default();
            above.push(target.get());
            if moved.iter().any(|m| above.contains(&m.get())) {
                return Err(Error::user(format!(
                    "You are creating a loop in your '{}' records. A record cannot be a child of \
                     itself or one of its own sub-items. Please select a different parent.",
                    spec.description
                )));
            }
        }
        Ok(moved)
    }

    /// After the parent field of `moved` became `new_parent` (in the cache): write it, then
    /// rewrite the paths of the moved records and their descendants. Moved records that are
    /// descendants of other moved records are moved first, in their own statement.
    pub(crate) async fn parent_store_move(
        &mut self,
        records: &Records,
        moved: &[RecId],
        new_parent: &Cell,
    ) -> Result<()> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let (Some(path), Some(parent)) = (spec.parent_path, spec.parent_name) else {
            return Ok(());
        };
        if moved.is_empty() {
            return Ok(());
        }
        let guard = self.guard();
        let result = async {
            self.flush_fields(spec.id, &[parent]).await?;
            let env = records.env().engine_sudo();
            let paths = self.peek_cells(&env, spec.id, path, moved).await?;
            let empty: Vec<RecId> = moved
                .iter()
                .zip(&paths)
                .filter(|(_, c)| !matches!(c, Cell::Str(_)))
                .map(|(id, _)| *id)
                .collect();
            if !empty.is_empty() {
                return Err(no_path(spec, &empty));
            }
            // deepest first when a moved record lies below another one
            let mut nodes: Vec<(RecId, String)> = moved
                .iter()
                .zip(paths)
                .filter_map(|(id, cell)| match cell {
                    Cell::Str(p) => Some((*id, p.to_string())),
                    _ => None,
                })
                .collect();
            nodes.sort_by_key(|(_, p)| std::cmp::Reverse(p.len()));
            let nested = nodes.iter().any(|(id, p)| {
                nodes
                    .iter()
                    .any(|(other, q)| other != id && p.starts_with(q.as_str()))
            });
            let batches: Vec<Vec<i64>> = if nested {
                nodes.iter().map(|(id, _)| vec![id.get()]).collect()
            } else {
                vec![nodes.iter().map(|(id, _)| id.get()).collect()]
            };
            let target: Option<i64> = crate::relational::id_of(new_parent).map(|t| t.get());
            let sql = erp_sql::relational::move_parent_paths(&spec.table);
            for batch in batches {
                let rows = self.query(&sql, &[&batch, &target]).await?;
                let rows = decode_paths(&rows)?;
                self.take_paths(spec, rows).await?;
            }
            Ok(())
        }
        .await;
        guard.disarm();
        result
    }
}
