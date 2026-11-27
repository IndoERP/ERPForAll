// SPDX-License-Identifier: MIT OR Apache-2.0
//! Editable native drafts. Published rows project immutable base policy revisions.

use std::collections::BTreeMap;

use erp_sdk::prelude::*;
use erp_sdk::rpc::{Effect, Exposure};
use erp_sdk::serde_json::json;
use erp_sdk::{Load, Operation, SearchOptions, ValueMap};

mod projection;
pub(super) use projection::sync_policies;

pub(super) const WORKFLOW: &str = "approval.workflow";
const STEP: &str = "approval.workflow.step";
const POLICY: &str = "ir.approval.policy";
const DRAFT: &str = "draft";
const IMMUTABLE: &str =
    "Published approval workflows are read-only. Create a revision to change a workflow.";
const FIELDS: &[&str] = &[
    "name",
    "state",
    "company_id",
    "principal_kind",
    "effect",
    "model_id",
    "model",
    "method",
    "policy_id",
    "replaces_policy_id",
];
const STEP_FIELDS: &[&str] = &["name", "sequence", "user_ids", "group_ids"];

#[model(
    name = "approval.workflow",
    description = "Approval Workflow",
    addon = "approvals",
    order = "id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    keys = [key(name = "_policy_revision", fields = ["policy_id"], nulls_distinct = true, message = "A policy revision can appear only once in Approvals.")],
    constraints = [("_publication_link", "CHECK ((state = 'draft') = (policy_id IS NULL))", "A published workflow must reference its policy revision.")]
)]
pub struct ApprovalWorkflow {
    #[field(char, required, default = "New workflow")]
    pub name: Option<Arc<str>>,
    #[field(selection, required, readonly, copy = false, default = "draft", selection = [
        ("draft", "Draft"), ("active", "Active"), ("inactive", "Archived")
    ])]
    pub state: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company", ondelete = "restrict")]
    pub company_id: Option<RecId>,
    #[field(selection, default = "user", selection = [("user", "User"), ("agent", "Agent")])]
    pub principal_kind: Option<Arc<str>>,
    #[field(selection, default = "write", selection = [("write", "Write"), ("destructive", "Destructive")])]
    pub effect: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "ir.model",
        ondelete = "set null",
        string = "Model"
    )]
    pub model_id: Option<RecId>,
    #[field(char, readonly, copy = false, string = "Technical Model")]
    pub model: Option<Arc<str>>,
    #[field(char)]
    pub method: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "ir.approval.policy",
        ondelete = "restrict",
        readonly,
        copy = false
    )]
    pub policy_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "ir.approval.policy",
        ondelete = "restrict",
        readonly,
        copy = false,
        string = "Replaces Policy"
    )]
    pub replaces_policy_id: Option<RecId>,
    #[field(
        one2many,
        comodel = "approval.workflow.step",
        inverse_name = "workflow_id",
        copy = true,
        string = "Review Steps"
    )]
    pub step_ids: Vec<RecId>,
}

#[model(
    name = "approval.workflow.step",
    description = "Approval Workflow Step",
    addon = "approvals",
    order = "sequence, id",
    company_access = parent(edge = "workflow_id")
)]
pub struct ApprovalWorkflowStep {
    #[field(
        many2one,
        comodel = "approval.workflow",
        required,
        ondelete = "cascade",
        index = true
    )]
    pub workflow_id: Option<RecId>,
    #[field(integer, default = 10)]
    pub sequence: i32,
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "res.users",
        relation = "approval_workflow_step_user_rel",
        column1 = "step_id",
        column2 = "user_id",
        string = "Approver Users"
    )]
    pub user_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "approval_workflow_step_group_rel",
        column1 = "step_id",
        column2 = "group_id",
        string = "Approver Groups"
    )]
    pub group_ids: Vec<RecId>,
}

type Data = BTreeMap<String, Value>;

#[derive(Default)]
struct Edit {
    projection: bool,
    publishing: Option<i64>,
}

fn projecting(tx: &mut Transaction) -> bool {
    tx.extension::<Edit>().projection
}

fn refuse_link_fields(vals: &RawVals) -> Result<()> {
    const PROTECTED: &[&str] = &["state", "policy_id", "replaces_policy_id", "model"];
    if vals.iter().any(|(name, _)| PROTECTED.contains(&name))
        || vals
            .iter_commands()
            .any(|(name, _)| PROTECTED.contains(&name))
    {
        return Err(Error::validation(
            "Use workflow actions to manage the publication state.",
        ));
    }
    if vals.commands("step_ids").is_some_and(|commands| {
        commands
            .iter()
            .any(|command| matches!(command, Command::Link(_) | Command::Set(_)))
    }) || matches!(vals.get("step_ids"), Some(Cell::Ids(_)))
    {
        return Err(Error::validation(
            "Review steps belong to their original workflow.",
        ));
    }
    Ok(())
}

/// All aggregate changes, including child CRUD and reorder, take the same draft lock.
/// A no-op update detects stale writers under REPEATABLE READ even when only children changed.
async fn lock_drafts(tx: &mut Transaction, records: &Records) -> Result<()> {
    records.check_access(tx, Operation::Write).await?;
    let mut ids = records.ids();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    // Acquire multiple workflow locks in deterministic order.
    let changed = tx
        .query(
            "WITH drafts AS MATERIALIZED (
            SELECT id FROM approval_workflow WHERE id=ANY($1::bigint[]) AND state='draft'
            ORDER BY id FOR UPDATE
         )
         UPDATE approval_workflow w SET state=w.state FROM drafts d WHERE w.id=d.id RETURNING w.id",
            &[&ids],
        )
        .await?;
    if changed.len() != ids.len() {
        return Err(Error::validation(IMMUTABLE));
    }
    Ok(())
}

fn cell_id(value: Option<&Cell>) -> Option<i64> {
    match value {
        Some(Cell::Id(id)) if *id > 0 => Some(*id),
        Some(Cell::Int(id)) if *id > 0 => Some(i64::from(*id)),
        _ => None,
    }
}

fn text(row: &Data, field: &str) -> Option<Arc<str>> {
    match row.get(field) {
        Some(Value::Str(text)) => Some(Arc::clone(text)),
        _ => None,
    }
}

fn record_id(row: &Data, field: &str) -> Option<i64> {
    match row.get(field) {
        Some(Value::Int(id)) if *id > 0 => Some(*id),
        _ => None,
    }
}

fn refs(row: &Data, field: &str) -> Result<Vec<i64>> {
    let Some(Value::List(values)) = row.get(field) else {
        return Err(Error::invariant(format!(
            "Expected workflow relation {field}."
        )));
    };
    values
        .iter()
        .map(|value| match value {
            Value::Int(id) if *id > 0 => Ok(*id),
            _ => Err(Error::invariant(format!(
                "Invalid ID in workflow relation {field}."
            ))),
        })
        .collect()
}

fn text_cell(value: Option<Arc<str>>) -> Cell {
    value.map_or(Cell::Null, Cell::Str)
}

fn id_cell(value: Option<i64>) -> Cell {
    value.map_or(Cell::Null, Cell::Id)
}

async fn data(tx: &mut Transaction, records: &Records, fields: &[&str]) -> Result<Data> {
    records.ensure_one()?;
    records
        .read_with(tx, fields, Load::Raw)
        .await?
        .pop()
        .ok_or_else(|| Error::access("The approval workflow is unavailable."))
}

async fn technical_model(
    tx: &mut Transaction,
    env: &Env,
    id: Option<i64>,
) -> Result<Option<Arc<str>>> {
    let Some(id) = id else {
        return Ok(None);
    };
    let records = env.records("ir.model")?.browse([id])?;
    let row = data(tx, &records, &["model"]).await?;
    text(&row, "model")
        .map(Some)
        .ok_or_else(|| Error::validation("Choose an installed model."))
}

async fn steps(tx: &mut Transaction, record: &Records) -> Result<Vec<Data>> {
    let id = record.ensure_one()?.int()?;
    let selected = record
        .env()
        .records(STEP)?
        .search_with(
            tx,
            &Domain::condition("workflow_id", Operator::Eq, id)?,
            &SearchOptions::new().order("sequence, id").limit(17),
        )
        .await?;
    selected.read_with(tx, STEP_FIELDS, Load::Raw).await
}

fn step_values(
    name: Option<Arc<str>>,
    sequence: i32,
    users: Vec<i64>,
    groups: Vec<i64>,
) -> RawVals {
    RawVals::new()
        .set_cell("name", text_cell(name))
        .set_cell("sequence", Cell::Int(sequence))
        .set_commands(
            "user_ids",
            [Command::set(users.into_iter().filter_map(RecId::from_real))],
        )
        .set_commands(
            "group_ids",
            [Command::set(
                groups.into_iter().filter_map(RecId::from_real),
            )],
        )
}

fn supported(row: &Data) -> Result<()> {
    if !matches!(
        text(row, "principal_kind").as_deref(),
        Some("user" | "agent")
    ) || !matches!(
        text(row, "effect").as_deref(),
        Some("write" | "destructive")
    ) {
        return Err(Error::validation(
            "Choose User or Agent and the Write or Destructive effect.",
        ));
    }
    Ok(())
}

#[methods(ApprovalWorkflow, addon = "approvals")]
impl ApprovalWorkflow {
    #[method(override)]
    async fn init(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        projection::backfill(tx, this.env()).await
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if !projecting(tx) {
            for vals in &mut vals_list {
                refuse_link_fields(vals)?;
                *vals = std::mem::take(vals)
                    .set_cell("state", Cell::Str(Arc::from(DRAFT)))
                    .set_cell("policy_id", Cell::Null)
                    .set_cell("replaces_policy_id", Cell::Null)
                    .set_cell("model", Cell::Null);
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !projecting(tx) {
            refuse_link_fields(&vals)?;
            lock_drafts(tx, this.as_records()).await?;
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !projecting(tx) {
            this.as_records()
                .check_access(tx, Operation::Unlink)
                .await?;
            lock_drafts(tx, this.as_records()).await?;
        }
        sup.call(tx, this.as_records(), ()).await
    }

    #[onchange("model_id", "effect")]
    async fn _onchange_workflow_target(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let method = this.get(tx, ApprovalWorkflow::METHOD).await?;
        let Some(method) = method else {
            return Ok(OnchangeResult::none());
        };
        let model = this
            .get(tx, ApprovalWorkflow::MODEL_ID)
            .await?
            .and_then(|id| id.as_real().map(|real| real.get()));
        let technical = technical_model(tx, this.env(), model).await?;
        let effect = this.get(tx, ApprovalWorkflow::EFFECT).await?;
        let exposed = technical
            .as_deref()
            .and_then(|name| this.env().registry().model_id(name))
            .and_then(|model| tx.exposure().get(model, &method));
        if matches!(exposed, Some(Exposure::Exposed(method)) if Some(method.effect()) == effect.as_deref().and_then(Effect::parse))
        {
            return Ok(OnchangeResult::none());
        }
        Ok(OnchangeResult::none().value("method", Value::False))
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_publish(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let record = this.as_records();
        let id = record.ensure_one()?.int()?;
        lock_drafts(tx, record).await?;
        let row = data(tx, record, FIELDS).await?;
        supported(&row)?;
        let model = technical_model(tx, this.env(), record_id(&row, "model_id")).await?;
        let mut configured = Vec::new();
        for step in steps(tx, record).await? {
            configured.push(Value::Map(Arc::new(ValueMap::from_iter([
                (
                    "name".to_owned(),
                    step.get("name").cloned().unwrap_or(Value::False),
                ),
                (
                    "user_ids".to_owned(),
                    Value::list(refs(&step, "user_ids")?.into_iter().map(Value::Int)),
                ),
                (
                    "group_ids".to_owned(),
                    Value::list(refs(&step, "group_ids")?.into_iter().map(Value::Int)),
                ),
            ]))));
        }
        let value = |key: &str| row.get(key).cloned().unwrap_or(Value::False);
        let workflow = Value::Map(Arc::new(ValueMap::from_iter([
            ("name".to_owned(), value("name")),
            ("principal_kind".to_owned(), value("principal_kind")),
            ("effect".to_owned(), value("effect")),
            ("company_id".to_owned(), value("company_id")),
            ("model".to_owned(), model.map_or(Value::False, Value::Str)),
            ("method".to_owned(), value("method")),
            ("active".to_owned(), Value::Bool(true)),
            ("review_steps".to_owned(), Value::list(configured)),
        ])));
        let policies = this.env().records(POLICY)?;
        let replaces = record_id(&row, "replaces_policy_id");
        tx.extension::<Edit>().publishing = Some(id);
        let outcome: Result<Value> = policies
            .call(tx, "erp_publish_workflow", (workflow, replaces))
            .await;
        tx.extension::<Edit>().publishing = None;
        outcome?;
        Ok(())
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_create_revision(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let record = this.as_records();
        let source = data(tx, record, FIELDS).await?;
        supported(&source)?;
        let Some(policy_id) = record_id(&source, "policy_id") else {
            return Err(Error::validation(
                "Publish the workflow before creating a revision.",
            ));
        };
        let successor = if text(&source, "state").as_deref() == Some("active") {
            Some(policy_id)
        } else {
            None
        };
        let mut commands = Vec::new();
        for (index, step) in steps(tx, record).await?.into_iter().enumerate() {
            commands.push(Command::create(step_values(
                text(&step, "name"),
                sequence(index)?,
                refs(&step, "user_ids")?,
                refs(&step, "group_ids")?,
            )));
        }
        let draft = RawVals::new()
            .set_cell("name", text_cell(text(&source, "name")))
            .set_cell("state", Cell::Str(Arc::from(DRAFT)))
            .set_cell("company_id", id_cell(record_id(&source, "company_id")))
            .set_cell("principal_kind", text_cell(text(&source, "principal_kind")))
            .set_cell("effect", text_cell(text(&source, "effect")))
            .set_cell("model_id", id_cell(record_id(&source, "model_id")))
            .set_cell("model", Cell::Null)
            .set_cell("method", text_cell(text(&source, "method")))
            .set_cell("policy_id", Cell::Null)
            .set_cell("replaces_policy_id", id_cell(successor))
            .set_commands("step_ids", commands);
        let previous = std::mem::replace(&mut tx.extension::<Edit>().projection, true);
        let created = this.env().records(WORKFLOW)?.create(tx, vec![draft]).await;
        tx.extension::<Edit>().projection = previous;
        let id = created?.ensure_one()?.int()?;
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window", "name": "Workflow revision", "res_model": WORKFLOW,
            "res_id": id, "target": "current", "views": [[false, "form"]],
        }))))
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_archive_workflow(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
    ) -> Result<()> {
        set_active(tx, this.as_records(), false).await
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_activate(
        this: &RecordSet<ApprovalWorkflow>,
        tx: &mut Transaction,
    ) -> Result<()> {
        set_active(tx, this.as_records(), true).await
    }
}

fn sequence(index: usize) -> Result<i32> {
    let position =
        i32::try_from(index + 1).map_err(|_| Error::invariant("Too many workflow steps."))?;
    Ok(position * 10)
}

async fn set_active(tx: &mut Transaction, record: &Records, active: bool) -> Result<()> {
    record.check_access(tx, Operation::Write).await?;
    let source = data(tx, record, FIELDS).await?;
    supported(&source)?;
    let Some(policy) = record_id(&source, "policy_id") else {
        return Err(Error::validation(
            "Publish the draft before changing its archive status.",
        ));
    };
    record
        .env()
        .records(POLICY)?
        .browse([policy])?
        .write(tx, RawVals::new().set_cell("active", Cell::Bool(active)))
        .await
}

#[methods(ApprovalWorkflowStep, addon = "approvals")]
impl ApprovalWorkflowStep {
    #[method(override)]
    async fn create(
        this: &RecordSet<ApprovalWorkflowStep>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if !projecting(tx) {
            this.as_records()
                .check_access(tx, Operation::Create)
                .await?;
            let parents = vals_list
                .iter()
                .map(|vals| {
                    cell_id(vals.get("workflow_id"))
                        .ok_or_else(|| Error::validation("Save review steps on a draft workflow."))
                })
                .collect::<Result<Vec<_>>>()?;
            lock_drafts(tx, &this.env().records(WORKFLOW)?.browse(parents)?).await?;
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ApprovalWorkflowStep>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !projecting(tx) {
            if vals.get("workflow_id").is_some() {
                return Err(Error::validation(
                    "Review steps belong to their original workflow.",
                ));
            }
            let records = this.as_records();
            records.check_access(tx, Operation::Write).await?;
            let parents = records.related(tx, "workflow_id").await?;
            lock_drafts(tx, &parents).await?;
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<ApprovalWorkflowStep>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !projecting(tx) {
            let records = this.as_records();
            records.check_access(tx, Operation::Unlink).await?;
            let parents = records.related(tx, "workflow_id").await?;
            lock_drafts(tx, &parents).await?;
        }
        sup.call(tx, this.as_records(), ()).await
    }
}
