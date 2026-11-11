//! The compiled `ParentAccess` rule (a SQL predicate on every search) is the follow-up
//! `the BASE-B access follow-up`; until it lands the generic ORM read of `ir.attachment` is limited to the
//! creator's own rows and to the system group (see `model.rs` and `security/ir.access.csv`).
//!
//! The rules, in our words:
//! - a record-bound attachment is allowed when the caller may read (or, for write, create and
//!   unlink, write) the record it is attached to; the parent's own rules decide, archived
//!   parents included;
//! - a field-bound one (`res_field`) also needs access to that field of the parent, except for
//!   the system group; a field the model does not have denies;
//! - a standalone one (no `res_id`) is allowed to the system group and to its creator;
//! - a parent whose model is not installed, a record that no longer exists and a parent on the
//!   deny list are denied;
//! - the caller may write the attachments of their own `res.users` record;
//! - superuser mode passes.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::prelude::*;
use erp_sdk::{Env, FieldOp, Operation, Transaction};

/// Models that never carry attachments: security-internal storage.
pub(crate) const DENIED_PARENT_MODELS: &[&str] = &[
    "res.users.apikeys",
    "ir.audit.entry",
    "ir.event.outbox",
    "ir.config.setting.value",
    "ir.config.setting.scope",
    "ir.config.setting.audit",
];

/// One attachment (existing or about to be created) as the check sees it.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub id: Option<i64>,
    pub res_model: Option<String>,
    pub res_id: Option<i64>,
    pub res_field: Option<String>,
    pub create_uid: i64,
}

impl Target {
    pub(crate) fn parent(&self) -> Option<(&str, i64)> {
        match (&self.res_model, self.res_id) {
            (Some(model), Some(id)) if id > 0 && !model.is_empty() => Some((model, id)),
            _ => None,
        }
    }
}

/// The text of every refusal: the same for a parent the caller cannot reach and for one that does
/// not exist, so the error tells nothing about other records.
pub(crate) fn denied(op: Operation, ids: &[i64], uid: i64) -> Error {
    let records = if ids.is_empty() {
        "the new attachment".to_owned()
    } else {
        format!("{:?}", ids.iter().take(6).collect::<Vec<_>>())
    };
    Error::access(format!(
        "Sorry, you are not allowed to access this document. Please contact your system \
         administrator.\n\n(Operation: {})\n\nRecords: {records}, User: {uid}",
        operation_name(op)
    ))
}

fn operation_name(op: Operation) -> &'static str {
    match op {
        Operation::Read => "read",
        Operation::Write => "write",
        Operation::Create => "create",
        Operation::Unlink => "unlink",
    }
}

/// Which targets the caller may `op`, in order.
///
/// `op` selects the parent check: reading needs read on the parent, every other operation write.
pub(crate) async fn allowed(
    tx: &mut Transaction,
    env: &Env,
    targets: &[Target],
    op: Operation,
) -> Result<Vec<bool>> {
    if env.su() {
        return Ok(vec![true; targets.len()]);
    }
    let uid = env.uid();
    let parent_op = if op == Operation::Read {
        Operation::Read
    } else {
        Operation::Write
    };
    let field_op = if op == Operation::Read {
        FieldOp::Read
    } else {
        FieldOp::Write
    };
    // the system group is asked only when a target needs it (a standalone attachment of
    // another user, a field-bound one): a plain create on a record never asks
    let mut is_system: Option<bool> = None;
    let mut ok = vec![true; targets.len()];
    let mut by_model: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    for (index, target) in targets.iter().enumerate() {
        let needs_group =
            (target.parent().is_none() && target.create_uid != uid) || target.res_field.is_some();
        let system = match (needs_group, is_system) {
            (false, _) => false,
            (true, Some(known)) => known,
            (true, None) => {
                let known = tx.has_group(env, "base.group_system").await?;
                is_system = Some(known);
                known
            }
        };
        if !system {
            if target.parent().is_none() && target.create_uid != uid {
                ok[index] = false;
                continue;
            }
            if let Some(field) = &target.res_field {
                let may = match &target.res_model {
                    Some(model) if !model.is_empty() => {
                        tx.has_field_access(env, model, field, field_op).await?
                    }
                    _ => false,
                };
                if !may {
                    ok[index] = false;
                    continue;
                }
            }
        }
        if let Some((model, id)) = target.parent() {
            by_model.entry(model).or_default().insert(id);
        }
    }
    let mut permitted: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    for (model, ids) in &by_model {
        let known = env
            .registry()
            .model(model)
            .is_some_and(|m| !m.is_abstract());
        if DENIED_PARENT_MODELS.contains(model) || !known {
            permitted.insert(model, BTreeSet::new());
            continue;
        }
        // a user may write the attachments of their own user record, whatever the user rules say
        if *model == "res.users"
            && parent_op == Operation::Write
            && ids.len() == 1
            && ids.contains(&uid)
        {
            permitted.insert(model, ids.clone());
            continue;
        }
        let records = env.records(model)?.browse(ids.iter().copied())?;
        let allowed_records = records.filtered_access(tx, parent_op).await?;
        permitted.insert(model, allowed_records.ids().into_iter().collect());
    }
    for (index, target) in targets.iter().enumerate() {
        if !ok[index] {
            continue;
        }
        if let Some((model, id)) = target.parent()
            && !permitted.get(model).is_some_and(|set| set.contains(&id))
        {
            ok[index] = false;
        }
    }
    Ok(ok)
}

/// Refuse the call unless every target is allowed.
///
/// # Errors
///
/// `Access` naming the operation and the first attachments that are refused.
pub(crate) async fn check(
    tx: &mut Transaction,
    env: &Env,
    targets: &[Target],
    op: Operation,
) -> Result<()> {
    let verdict = allowed(tx, env, targets, op).await?;
    if verdict.iter().all(|ok| *ok) {
        return Ok(());
    }
    let refused: Vec<i64> = targets
        .iter()
        .zip(&verdict)
        .filter(|(_, ok)| !**ok)
        .filter_map(|(t, _)| t.id)
        .collect();
    Err(denied(op, &refused, env.uid()))
}

/// Load the access-relevant columns of existing attachments, in the order of `ids` (duplicates
/// dropped). A missing id is not returned.
pub(crate) async fn load_targets(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<Target>> {
    targets_by(
        tx,
        "SELECT id, res_model, res_id, res_field, create_uid FROM ir_attachment \
         WHERE id = ANY($1::int8[]) ORDER BY id",
        ids,
    )
    .await
}

/// [`load_targets`] with a `FOR NO KEY UPDATE` row lock, for a path that decides from the rows
/// and then writes in the same transaction (`adopt`): the rows cannot be deleted or re-parented
/// by a concurrent transaction between the decision and the move. The lock is exclusive on
/// purpose: a shared lock followed by the `UPDATE` lets two concurrent claims of one file each
/// hold the shared lock and wait for the other's update (a deadlock, 40P01); an exclusive lock
/// makes the second claim wait for the first. Rows are locked in id order. It writes locks, so it
/// is not for the read-only transactions the shared loader also serves.
pub(crate) async fn load_targets_locked(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<Target>> {
    targets_by(
        tx,
        "SELECT id, res_model, res_id, res_field, create_uid FROM ir_attachment \
         WHERE id = ANY($1::int8[]) ORDER BY id FOR NO KEY UPDATE",
        ids,
    )
    .await
}

async fn targets_by(tx: &mut Transaction, sql: &str, ids: &[i64]) -> Result<Vec<Target>> {
    let rows = tx.query(sql, &[&ids]).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let read = |e: &dyn std::fmt::Display| {
            Error::db(format!("an attachment row could not be read: {e}"))
        };
        out.push(Target {
            id: Some(row.try_get(0).map_err(|e| read(&e))?),
            res_model: row.try_get(1).map_err(|e| read(&e))?,
            res_id: row.try_get(2).map_err(|e| read(&e))?,
            res_field: row.try_get(3).map_err(|e| read(&e))?,
            create_uid: row
                .try_get::<_, Option<i64>>(4)
                .map_err(|e| read(&e))?
                .unwrap_or(0),
        });
    }
    Ok(out)
}
