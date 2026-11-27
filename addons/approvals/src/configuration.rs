// SPDX-License-Identifier: MIT OR Apache-2.0
//! Configuration writes go through the base policy's ordinary create and archive methods.

use std::collections::BTreeSet;

use erp_sdk::prelude::*;
use erp_sdk::rpc::{Effect, Exposure};
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{Operation, SUPERUSER_ID, ValueMap};

erp_sdk::model_ref!(pub Policy = "ir.approval.policy");

#[extend(Policy, addon = "approvals")]
pub struct WorkflowPolicy {}

const MAX_STEPS: usize = 16;
const MAX_RECIPIENTS: usize = 64;
const MAX_NAME: usize = 128;
const MAX_IDENTITY: usize = 256;

struct Workflow {
    values: RawVals,
    active: bool,
    users: BTreeSet<i64>,
    groups: BTreeSet<i64>,
}

#[methods(Policy, addon = "approvals")]
impl WorkflowPolicy {
    #[method(override)]
    async fn create(
        this: &RecordSet<Policy>,
        tx: &mut Transaction,
        values: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let created: Records = sup.call(tx, this.as_records(), (values,)).await?;
        crate::workflow::sync_policies(tx, &created, true).await?;
        Ok(created)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<Policy>,
        tx: &mut Transaction,
        values: RawVals,
        sup: Super,
    ) -> Result<()> {
        let archive_changes = values.get("active").is_some();
        sup.call(tx, this.as_records(), (values,)).await?;
        if archive_changes {
            crate::workflow::sync_policies(tx, this.as_records(), false).await?;
        }
        Ok(())
    }

    /// Return currently exposed methods for an installed model and effect.
    #[method]
    #[rpc(effect = read, model)]
    async fn erp_workflow_methods(
        this: &RecordSet<Policy>,
        tx: &mut Transaction,
        model: String,
        effect: String,
        query: String,
    ) -> Result<Value> {
        this.as_records().check_access(tx, Operation::Read).await?;
        let effect =
            Effect::parse(&effect).ok_or_else(|| Error::validation("Unknown approval effect."))?;
        if !matches!(effect, Effect::Write | Effect::Destructive) {
            return Err(Error::validation(
                "Choose Write or Destructive for an approval workflow.",
            ));
        }
        if model.len() > MAX_IDENTITY || query.len() > MAX_IDENTITY {
            return Err(Error::validation("The model or method search is too long."));
        }
        let model = this
            .env()
            .registry()
            .model_id(&model)
            .ok_or_else(|| Error::validation("Choose an installed model."))?;
        let query = query.trim().to_ascii_lowercase();
        let methods = tx
            .exposure()
            .methods(model)
            .into_iter()
            .filter_map(|(name, exposure)| match exposure {
                Exposure::Exposed(method) if method.effect() == effect && name.contains(&query) => {
                    Some(Value::str(name))
                }
                _ => None,
            })
            .take(20);
        Ok(Value::list(methods))
    }

    /// Save one immutable workflow. Publishing a replacement archives the old revision and
    /// creates its successor in the same transaction under the base policy fence.
    #[method]
    #[rpc(effect = write, model)]
    async fn erp_publish_workflow(
        this: &RecordSet<Policy>,
        tx: &mut Transaction,
        workflow: Value,
        replaces_id: Option<i64>,
    ) -> Result<Value> {
        let policies = this.as_records();
        policies.check_access(tx, Operation::Create).await?;
        let workflow = parse_workflow(&workflow)?;
        check_recipients(tx, this.env(), &workflow).await?;
        if let Some(id) = replaces_id {
            if id <= 0 || !workflow.active {
                return Err(Error::validation(
                    "A replacement needs an active source and must be published.",
                ));
            }
            let () = policies
                .browse([id])?
                .call(tx, "erp_archive_for_workflow_replacement", ())
                .await?;
            // The base selector check queries SQL directly, so it must see the staged archive.
            tx.flush_model(policies.model_id()).await?;
        }
        let created = policies.create(tx, vec![workflow.values]).await?;
        let ids = created.ids();
        let [id] = ids.as_slice() else {
            return Err(Error::invariant(
                "Saving a workflow must create one revision.",
            ));
        };
        Ok(Value::Int(*id))
    }
}

fn document<'a>(value: &'a Value, allowed: &[&str], label: &str) -> Result<&'a ValueMap> {
    let Value::Map(map) = value else {
        return Err(Error::validation(format!("{label} must be an object.")));
    };
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::validation(format!(
            "{label} contains an unsupported field: {}.",
            key.chars().take(64).collect::<String>()
        )));
    }
    Ok(map)
}

fn name(value: Option<&Value>, label: &str) -> Result<Arc<str>> {
    match value {
        Some(Value::Str(value))
            if !value.trim().is_empty()
                && value.trim().chars().count() <= MAX_NAME
                && !value.contains('\0') =>
        {
            Ok(Arc::from(value.trim()))
        }
        _ => Err(Error::validation(format!(
            "{label} must contain between 1 and {MAX_NAME} characters."
        ))),
    }
}

fn identity(value: Option<&Value>, label: &str) -> Result<Option<Arc<str>>> {
    match value {
        None | Some(Value::False | Value::Bool(false)) => Ok(None),
        Some(Value::Str(value)) if value.is_empty() => Ok(None),
        Some(Value::Str(value)) if value.len() <= MAX_IDENTITY && !value.contains('\0') => {
            Ok(Some(Arc::clone(value)))
        }
        _ => Err(Error::validation(format!(
            "{label} must be a technical name of at most {MAX_IDENTITY} bytes."
        ))),
    }
}

fn recipient_ids(value: Option<&Value>, label: &str) -> Result<BTreeSet<i64>> {
    let Some(value) = value else {
        return Ok(BTreeSet::new());
    };
    let Value::List(values) = value else {
        return Err(Error::validation(format!(
            "{label} must be a list of record IDs."
        )));
    };
    if values.len() > MAX_RECIPIENTS {
        return Err(Error::validation(format!(
            "{label} may contain at most {MAX_RECIPIENTS} records."
        )));
    }
    let mut ids = BTreeSet::new();
    for value in values.iter() {
        match value {
            Value::Int(id) if *id > 0 && ids.insert(*id) => {}
            _ => {
                return Err(Error::validation(format!(
                    "{label} must contain distinct positive record IDs."
                )));
            }
        }
    }
    Ok(ids)
}

fn parse_workflow(value: &Value) -> Result<Workflow> {
    let body = document(
        value,
        &[
            "name",
            "active",
            "principal_kind",
            "effect",
            "company_id",
            "model",
            "method",
            "review_steps",
        ],
        "The workflow",
    )?;
    let title = name(body.get("name"), "The workflow name")?;
    let principal = identity(body.get("principal_kind"), "The requester type")?;
    let effect = identity(body.get("effect"), "The effect")?;
    match (principal.as_deref(), effect.as_deref()) {
        (Some("user" | "agent"), Some("write" | "destructive")) => {}
        _ => {
            return Err(Error::validation(
                "Choose a supported requester type and operation effect.",
            ));
        }
    }
    let active = match body.get("active") {
        Some(Value::Bool(active)) => *active,
        Some(Value::False) => false,
        _ => {
            return Err(Error::validation(
                "Choose whether to activate the workflow.",
            ));
        }
    };
    let company = match body.get("company_id") {
        None | Some(Value::False | Value::Bool(false)) => Cell::Null,
        Some(Value::Int(id)) if *id > 0 => Cell::Id(*id),
        _ => {
            return Err(Error::validation(
                "Choose an active company or all companies.",
            ));
        }
    };
    let model = identity(body.get("model"), "The model")?;
    let method = identity(body.get("method"), "The method")?;
    if model.is_none() && method.is_some() {
        return Err(Error::validation(
            "Select a model before choosing a method.",
        ));
    }
    let Some(Value::List(steps)) = body.get("review_steps") else {
        return Err(Error::validation("Add at least one review step."));
    };
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(Error::validation(format!(
            "Add between 1 and {MAX_STEPS} review steps."
        )));
    }
    let mut configured = Vec::<Json>::with_capacity(steps.len());
    let mut users = BTreeSet::new();
    let mut groups = BTreeSet::new();
    for (index, step) in steps.iter().enumerate() {
        let label = format!("Step {}", index + 1);
        let step = document(step, &["name", "user_ids", "group_ids"], &label)?;
        let title = name(step.get("name"), &format!("{label} name"))?;
        let user_ids = recipient_ids(step.get("user_ids"), &format!("{label} users"))?;
        let group_ids = recipient_ids(step.get("group_ids"), &format!("{label} groups"))?;
        let total = user_ids.len() + group_ids.len();
        if !(1..=MAX_RECIPIENTS).contains(&total) {
            return Err(Error::validation(format!(
                "{label} needs between 1 and {MAX_RECIPIENTS} approver users or groups."
            )));
        }
        users.extend(user_ids.iter().copied());
        groups.extend(group_ids.iter().copied());
        configured
            .push(json!({"name": title.as_ref(), "user_ids": user_ids, "group_ids": group_ids}));
    }
    let text = |value: Option<Arc<str>>| value.map_or(Cell::Null, Cell::Str);
    let values = RawVals::new()
        .set_cell("name", Cell::Str(title))
        .set_cell("principal_kind", text(principal))
        .set_cell("effect", text(effect))
        .set_cell("active", Cell::Bool(active))
        .set_cell("company_id", company)
        .set_cell("model", text(model))
        .set_cell("method", text(method))
        .set_cell("outcome", Cell::Str(Arc::from("approve")))
        .set_cell(
            "review_steps",
            Cell::Json(Arc::new(Json::Array(configured))),
        );
    Ok(Workflow {
        values,
        active,
        users,
        groups,
    })
}

async fn check_recipients(tx: &mut Transaction, env: &Env, workflow: &Workflow) -> Result<()> {
    if !workflow.users.is_empty() {
        let domain = Domain::condition(
            "id",
            Operator::In,
            workflow.users.iter().copied().collect::<Vec<_>>(),
        )? & Domain::condition("id", Operator::Ne, SUPERUSER_ID.get())?
            & Domain::condition("active", Operator::Eq, true)?
            & Domain::condition("share", Operator::Eq, false)?;
        let available = env.records("res.users")?.search(tx, &domain).await?;
        if available.len() != workflow.users.len() {
            return Err(Error::validation(
                "Choose active internal users who are available to you.",
            ));
        }
    }
    if !workflow.groups.is_empty() {
        let domain = Domain::condition(
            "id",
            Operator::In,
            workflow.groups.iter().copied().collect::<Vec<_>>(),
        )?;
        let available = env.records("res.groups")?.search(tx, &domain).await?;
        if available.len() != workflow.groups.len() {
            return Err(Error::validation(
                "Choose approver groups that are available to you.",
            ));
        }
    }
    Ok(())
}
