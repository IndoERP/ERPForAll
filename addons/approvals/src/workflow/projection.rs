// SPDX-License-Identifier: MIT OR Apache-2.0
//! Keep the native read-only history in the same transaction as the authoritative policy.

use super::*;
use erp_sdk::PrincipalKind;
use erp_sdk::serde_json::Value as Json;

const POLICY_FIELDS: &[&str] = &[
    "id",
    "name",
    "active",
    "company_id",
    "principal_kind",
    "effect",
    "model",
    "method",
    "outcome",
    "review_steps",
    "approver_user_ids",
    "approver_group_ids",
];

pub(super) async fn backfill(tx: &mut Transaction, env: &Env) -> Result<()> {
    let mut after = 0_i64;
    loop {
        // This runs as System during model initialization. The unique policy link makes this
        // idempotent on an upgrade and preserves native draft or published identities.
        let rows = tx.query(
            "SELECT id::bigint FROM ir_approval_policy
             WHERE outcome='approve' AND principal_kind IN ('user','agent') AND effect IN ('write','destructive')
               AND id>$1::bigint ORDER BY id LIMIT 100",
            &[&after],
        ).await?;
        let ids = rows
            .iter()
            .map(|row| {
                row.try_get::<_, i64>(0)
                    .map_err(|_| Error::invariant("Invalid approval policy identity."))
            })
            .collect::<Result<Vec<_>>>()?;
        let Some(last) = ids.last() else {
            return Ok(());
        };
        after = *last;
        sync_policies(tx, &env.records(POLICY)?.browse(ids)?, false).await?;
    }
}

pub(crate) async fn sync_policies(
    tx: &mut Transaction,
    policies: &Records,
    creating: bool,
) -> Result<()> {
    let env = policies.env();
    if policies.is_empty() || env.registry().model(WORKFLOW).is_none() {
        return Ok(());
    }
    // During initial bootstrap, base policy data may be applied before the dependent
    // addon's tables exist. The native model's init hook backfills those revisions.
    if env.principal().kind() == PrincipalKind::System {
        let tables = tx
            .query("SELECT to_regclass('approval_workflow') IS NOT NULL", &[])
            .await?;
        if !tables
            .first()
            .ok_or_else(|| Error::invariant("Cannot check workflow storage."))?
            .try_get::<_, bool>(0)
            .map_err(|_| Error::invariant("Cannot check workflow storage."))?
        {
            return Ok(());
        }
    }
    tx.flush_model(policies.model_id()).await?;
    let projection = std::mem::replace(&mut tx.extension::<Edit>().projection, true);
    let result = sync(tx, policies, creating).await;
    tx.extension::<Edit>().projection = projection;
    result
}

async fn sync(tx: &mut Transaction, policies: &Records, creating: bool) -> Result<()> {
    let workflows = policies.env().records(WORKFLOW)?;
    let publishing = if creating {
        tx.extension::<Edit>().publishing
    } else {
        None
    };
    let rows = policies.read_with(tx, POLICY_FIELDS, Load::Raw).await?;
    if rows.len() != policies.len() {
        return Err(Error::invariant(
            "A policy revision was unavailable for its workflow history.",
        ));
    }
    if publishing.is_some() && rows.len() != 1 {
        return Err(Error::invariant(
            "Publishing a workflow must create one policy revision.",
        ));
    }
    for row in rows {
        if text(&row, "outcome").as_deref() != Some("approve") || supported(&row).is_err() {
            if publishing.is_some() {
                return Err(Error::invariant(
                    "A workflow must publish an approval policy.",
                ));
            }
            continue;
        }
        let policy_id = record_id(&row, "id")
            .ok_or_else(|| Error::invariant("Expected an approval policy ID."))?;
        let state = match row.get("active") {
            Some(Value::Bool(true)) => "active",
            Some(Value::Bool(false) | Value::False) => "inactive",
            _ => {
                return Err(Error::invariant(
                    "Expected the approval policy archive state.",
                ));
            }
        };
        let existing = workflows
            .search(
                tx,
                &Domain::condition("policy_id", Operator::Eq, policy_id)?,
            )
            .await?;
        if !existing.is_empty() {
            if publishing.is_some() {
                return Err(Error::invariant(
                    "The published policy already has a workflow.",
                ));
            }
            let current = data(tx, &existing, &["state"]).await?;
            if text(&current, "state").as_deref() != Some(state) {
                existing
                    .write(
                        tx,
                        RawVals::new().set_cell("state", Cell::Str(Arc::from(state))),
                    )
                    .await?;
            }
            continue;
        }
        let technical = text(&row, "model");
        let model_id = if let Some(name) = &technical {
            let names = policies
                .env()
                .records("ir.model")?
                .search(
                    tx,
                    &Domain::condition("model", Operator::Eq, name.as_ref())?,
                )
                .await?;
            names.ids().first().copied()
        } else {
            None
        };
        let mut step_commands = if publishing.is_some() {
            vec![Command::clear()]
        } else {
            Vec::new()
        };
        step_commands.extend(read_steps(tx, policies.env(), &row).await?);
        let values = RawVals::new()
            .set_cell("state", Cell::Str(Arc::from(state)))
            .set_cell("name", text_cell(text(&row, "name")))
            .set_cell("company_id", id_cell(record_id(&row, "company_id")))
            .set_cell("principal_kind", text_cell(text(&row, "principal_kind")))
            .set_cell("effect", text_cell(text(&row, "effect")))
            .set_cell("model_id", id_cell(model_id))
            .set_cell("model", text_cell(technical))
            .set_cell("method", text_cell(text(&row, "method")))
            .set_cell("policy_id", Cell::Id(policy_id))
            .set_commands("step_ids", step_commands);
        if let Some(id) = publishing {
            workflows.browse([id])?.write(tx, values).await?;
        } else {
            workflows
                .create(tx, vec![values.set_cell("replaces_policy_id", Cell::Null)])
                .await?;
        }
    }
    Ok(())
}

fn json_ids(step: &Json, key: &str) -> Result<Vec<i64>> {
    let Some(value) = step.get(key) else {
        return Ok(Vec::new());
    };
    let ids = value
        .as_array()
        .ok_or_else(|| Error::invariant("Invalid stored workflow reviewers."))?;
    ids.iter()
        .map(|id| {
            id.as_i64()
                .filter(|id| *id > 0)
                .ok_or_else(|| Error::invariant("Invalid stored workflow reviewer ID."))
        })
        .collect()
}

async fn read_steps(tx: &mut Transaction, env: &Env, row: &Data) -> Result<Vec<Command>> {
    let stages = match row.get("review_steps") {
        Some(Value::False | Value::Bool(false)) => &[][..],
        Some(Value::Json(json)) if json.is_null() => &[][..],
        Some(Value::Json(json)) => json
            .as_array()
            .ok_or_else(|| Error::invariant("Invalid stored approval workflow."))?
            .as_slice(),
        _ => return Err(Error::invariant("Invalid stored approval workflow.")),
    };
    if stages.is_empty() {
        return Ok(vec![Command::create(step_values(
            text(row, "name"),
            sequence(0)?,
            refs(row, "approver_user_ids")?,
            refs(row, "approver_group_ids")?,
        ))]);
    }
    let mut commands = Vec::with_capacity(stages.len());
    for (index, step) in stages.iter().enumerate() {
        let label = step
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invariant("Invalid stored approval step name."))?;
        let users = existing_references(tx, env, "res.users", json_ids(step, "user_ids")?).await?;
        let groups =
            existing_references(tx, env, "res.groups", json_ids(step, "group_ids")?).await?;
        commands.push(Command::create(step_values(
            Some(Arc::from(label)),
            sequence(index)?,
            users,
            groups,
        )));
    }
    Ok(commands)
}

async fn existing_references(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: Vec<i64>,
) -> Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(ids);
    }
    // Legacy JSON can outlive a deleted user or group. Only existing references can appear
    // in the native relationship. The original policy remains the immutable engine record.
    let records = env
        .records(model)?
        .search_with(
            tx,
            &Domain::condition("id", Operator::In, ids)?,
            &SearchOptions::new().active_test(false),
        )
        .await?;
    Ok(records.ids())
}
