//! The write rules of `ir.actions.server` (design m2-s3 §7.2, brief §2.4 and §2.8): who may set
//! `run_as_superuser` or change an action tree that holds it, that a flagged action keeps groups,
//! and that `parent_id`/`child_ids` make no loop.

use erp_sdk::prelude::*;
use erp_sdk::{Env, FieldType, Registry};

use super::ident;
use crate::server_action::{MAX_ACTIONS, MAX_CHAIN, RunLimit, ServerActionError};

/// The check that `init` removes (its suffix as declared).
const OBSOLETE_CHECK: &str = "_run_as_superuser_not_yet";

/// The database name schema synchronization gave the check on `table`: `<table>_<name
/// without its leading '_'>`, shortened as every table object name (`erp-registry` compose).
fn obsolete_check_name(table: &str) -> String {
    erp_sdk::ident::shorten_identifier(&format!("{table}{OBSOLETE_CHECK}"))
}

/// Drop the check on `table` in the current schema (the one schema synchronization
/// works in) when the catalog has it. One statement when it is gone (nothing is locked), two
/// when it is there. The names are bound as parameters and quoted as identifiers: no DO block,
/// no dollar quoting.
pub(super) async fn drop_obsolete_check(tx: &mut Transaction, table: &str) -> Result<()> {
    let name = obsolete_check_name(table);
    let rows = tx
        .query(
            "SELECT n.nspname::text FROM pg_constraint k \
               JOIN pg_class c ON c.oid = k.conrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = current_schema() AND c.relname = $1 AND k.conname = $2",
            &[&table, &name],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let schema: String = row
        .try_get(0)
        .map_err(|e| crate::server_action::column_error("pg_namespace", "nspname", e))?;
    tx.batch(&format!(
        "ALTER TABLE {}.{} DROP CONSTRAINT {}",
        ident(&schema),
        ident(table),
        ident(&name)
    ))
    .await
}

/// The table and the group relation of `ir.actions.server`.
struct Tables {
    /// The table's name, for messages.
    name: String,
    /// The quoted table.
    server: String,
    rel: String,
    col1: String,
}

fn tables(registry: &Registry) -> Result<Tables> {
    let spec = registry
        .model("ir.actions.server")
        .ok_or_else(|| Error::programming("the registry has no model 'ir.actions.server'"))?;
    let Some(FieldType::Many2many {
        relation: Some(relation),
        ..
    }) = spec.field("group_ids").map(|f| &f.ty)
    else {
        return Err(Error::programming(
            "ir.actions.server.group_ids is not a stored many2many",
        ));
    };
    Ok(Tables {
        name: spec.table.clone(),
        server: ident(&spec.table),
        rel: ident(&relation.table),
        col1: ident(&relation.column1),
    })
}

/// An administrator: superuser mode (the loader, `sudo`), or a user in `base.group_system`.
async fn is_admin(tx: &mut Transaction, env: &Env) -> Result<bool> {
    if env.su() {
        return Ok(true);
    }
    tx.has_group(env, "base.group_system").await
}

/// Which part of the tree around the given actions a change reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reach {
    /// The actions and their ancestors: a write or a create changes what each ancestor runs.
    Ancestors,
    /// Also their descendants: an unlink deletes them (`parent_id` is `ondelete=cascade`).
    AncestorsAndDescendants,
}

/// Enumerate the actions `$1`, their ancestors and optionally descendants without locking.
/// Each recursive result is consumed only up to `$2` distinct ids (the limit plus one);
/// no sort or join above these LIMITs can force the complete recursive result to materialize.
/// UNION inside each recursion deduplicates ids, including cycles and overlapping roots.
/// The final UNION operates on at most twice `$2` ids, never the unbounded subtree.
fn tree_statement(t: &Tables, descendants: bool) -> String {
    let server = &t.server;
    let down = if descendants {
        format!(
            ", down(id) AS ( \
                 SELECT a.id FROM {server} a WHERE a.id = ANY($1) \
               UNION \
                 SELECT c.id FROM down JOIN {server} c ON c.parent_id = down.id \
             )"
        )
    } else {
        String::new()
    };
    let down_ids = if descendants {
        " UNION SELECT id FROM (SELECT id FROM down LIMIT $2) bounded_down"
    } else {
        ""
    };
    format!(
        "WITH RECURSIVE up(id) AS ( \
             SELECT a.id FROM {server} a WHERE a.id = ANY($1) \
           UNION \
             SELECT a.parent_id FROM up JOIN {server} a ON a.id = up.id \
              WHERE a.parent_id IS NOT NULL \
         ){down} \
         SELECT id FROM (SELECT id FROM up LIMIT $2) bounded_up{down_ids}"
    )
}

/// A caller outside superuser mode changes the actions `ids` (and, for `Reach::Ancestors`, the
/// tree above them): refused unless the caller is an administrator when any of those actions,
/// one of their ancestors, or (`AncestorsAndDescendants`) one of their descendants has
/// `run_as_superuser` (review SEC-1: a flagged action's tree is part of what it runs). The rows
/// read are locked (review SEC-3) after the whole guard's cardinality is validated. Enumeration
/// and locking use separate query calls, followed by the group check when flagged (source
/// trace; the complete statement count is not measured here). Superuser mode and no ids return
/// before database work. The transaction's REPEATABLE READ snapshot is shared by enumeration
/// and locking; a concurrent flag update committed after that snapshot causes a serialization
/// error.
///
/// At most [`MAX_ACTIONS`] unique actions may participate in this security check. An oversized
/// ancestor set or subtree is refused before locking, never accepted after a partial check.
pub(super) async fn check_tree(
    tx: &mut Transaction,
    env: &Env,
    ids: &[i64],
    reach: Reach,
) -> Result<()> {
    if env.su() || ids.is_empty() {
        return Ok(());
    }
    let t = tables(env.registry())?;
    tx.flush().await?;
    let limit = MAX_ACTIONS
        .checked_add(1)
        .ok_or_else(|| Error::programming("MAX_ACTIONS overflow"))?;
    let limit = i64::try_from(limit).map_err(|_| Error::programming("MAX_ACTIONS overflow"))?;
    let rows = tx
        .query(
            &tree_statement(&t, reach == Reach::AncestorsAndDescendants),
            &[&ids, &limit],
        )
        .await?;
    if rows.len() > MAX_ACTIONS {
        return Err(ServerActionError::Limit {
            kind: RunLimit::MutationActions,
        }
        .into());
    }
    let tree_ids: Vec<i64> = rows
        .iter()
        .map(|row| {
            row.try_get(0)
                .map_err(|e| crate::server_action::column_error(&t.name, "id", e))
        })
        .collect::<Result<_>>()?;
    // Acquire the complete bounded set in id order, as before, so overlapping trees take
    // their locks in the same order. No row is locked for an oversized traversal.
    let rows = tx
        .query(
            &format!(
                "SELECT a.run_as_superuser FROM {} a WHERE a.id = ANY($1) \
                 ORDER BY a.id FOR NO KEY UPDATE OF a",
                t.server
            ),
            &[&tree_ids],
        )
        .await?;
    let mut flagged = false;
    for row in &rows {
        let read = |what: &str, e| crate::server_action::column_error(&t.name, what, e);
        let flag: Option<bool> = row.try_get(0).map_err(|e| read("run_as_superuser", e))?;
        // a NULL boolean column is false, as the ORM reads it
        flagged |= flag == Some(true);
    }
    if flagged && !is_admin(tx, env).await? {
        return Err(ServerActionError::SuperuserAdminOnly.into());
    }
    Ok(())
}

/// The existing actions a write's values point at: the new `parent_id` (the action joins that
/// tree) and every action named by a `child_ids` command (taken from its tree, or changed through
/// it). Only database ids: a new record is in no stored tree.
pub(super) fn linked_ids(vals: &RawVals, out: &mut Vec<i64>) {
    // a many2one value other than an id is refused by the ORM's write with its own error
    if let Some(Cell::Id(parent)) = vals.get("parent_id")
        && *parent > 0
    {
        out.push(*parent);
    }
    if let Some(commands) = vals.commands("child_ids") {
        for command in commands {
            match command {
                Command::Update(id, _)
                | Command::Delete(id)
                | Command::Unlink(id)
                | Command::Link(id) => out.extend(id.as_real().map(|r| r.get())),
                Command::Set(ids) => {
                    out.extend(ids.iter().filter_map(|id| id.as_real()).map(|r| r.get()))
                }
                // a new child joins the tree of the action written, checked with it
                Command::Create(_) | Command::Clear => {}
            }
        }
    }
    if let Some(Cell::Ids(ids)) = vals.get("child_ids") {
        out.extend(ids.iter().filter_map(|id| id.as_real()).map(|r| r.get()));
    }
}

/// Whether `vals` sets `run_as_superuser` to anything but false or nothing: fails closed (review
/// SEC-6), the ORM refuses a value that is not a boolean anyway.
pub(super) fn sets_flag(vals: &RawVals) -> bool {
    !matches!(
        vals.get("run_as_superuser"),
        None | Some(Cell::Null | Cell::Bool(false))
    )
}

/// After a create or a write of the flag or the groups: the stored flagged actions among `ids`
/// need an administrator (create: a default or a copy sets the flag too) and at least one group.
/// One statement.
pub(super) async fn check_flagged(
    tx: &mut Transaction,
    records: &Records,
    ids: &[i64],
) -> Result<()> {
    let env = records.env().clone();
    let t = tables(env.registry())?;
    tx.flush().await?;
    let rows = tx
        .query(
            &format!(
                "SELECT a.name, EXISTS (SELECT 1 FROM {rel} r WHERE r.{col1} = a.id) \
                   FROM {server} a WHERE a.id = ANY($1) AND a.run_as_superuser ORDER BY a.id",
                rel = t.rel,
                col1 = t.col1,
                server = t.server
            ),
            &[&ids],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    if !is_admin(tx, &env).await? {
        return Err(ServerActionError::SuperuserAdminOnly.into());
    }
    for row in &rows {
        let read = |what: &str, e| crate::server_action::column_error(&t.name, what, e);
        let name: String = row.try_get(0).map_err(|e| read("name", e))?;
        let has_groups: bool = row.try_get(1).map_err(|e| read("group_ids", e))?;
        if !has_groups {
            return Err(ServerActionError::SuperuserNeedsGroups { name }.into());
        }
    }
    Ok(())
}

/// After `parent_id`/`child_ids` changed on `ids` (or new actions were created with them): walk
/// up the parents of each, at most [`MAX_CHAIN`] steps.
/// Every action whose parent
/// changed is in `ids` or a child of one of them, so a new loop always passes through `ids`.
/// One statement.
pub(super) async fn check_loops(
    tx: &mut Transaction,
    records: &Records,
    ids: &[i64],
) -> Result<()> {
    let t = tables(records.env().registry())?;
    tx.flush().await?;
    let max = i64::try_from(MAX_CHAIN).map_err(|_| Error::programming("MAX_CHAIN"))?;
    let rows = tx
        .query(
            &format!(
                "WITH RECURSIVE up(start, cur, depth) AS ( \
                     SELECT a.id, a.parent_id, 1::int8 FROM {server} a \
                      WHERE a.id = ANY($1) AND a.parent_id IS NOT NULL \
                   UNION ALL \
                     SELECT up.start, a.parent_id, up.depth + 1 FROM up \
                       JOIN {server} a ON a.id = up.cur \
                      WHERE up.cur <> up.start AND a.parent_id IS NOT NULL AND up.depth < $2 \
                 ) \
                 SELECT EXISTS (SELECT 1 FROM up WHERE cur = start), \
                        EXISTS (SELECT 1 FROM up WHERE depth >= $2 AND cur <> start)",
                server = t.server
            ),
            &[&ids, &max],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::db("the loop check returned no row"))?;
    let read = |e| crate::server_action::column_error(&t.name, "parent_id", e);
    let looped: bool = row.try_get(0).map_err(read)?;
    let too_long: bool = row.try_get(1).map_err(read)?;
    if looped {
        return Err(ServerActionError::ChildLoop.into());
    }
    if too_long {
        return Err(ServerActionError::Limit {
            kind: RunLimit::Chain,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::obsolete_check_name;

    /// The name follows schema synchronization's: `<table>_<suffix>`, shortened past 63 bytes.
    #[test]
    fn obsolete_check_name_is_shortened_as_schema_sync() {
        assert_eq!(
            obsolete_check_name("ir_act_server"),
            "ir_act_server_run_as_superuser_not_yet"
        );
        let long = obsolete_check_name(&"t".repeat(50));
        assert_eq!(long.len(), 63, "{long}");
        assert!(long.starts_with(&"t".repeat(50)));
    }
}
