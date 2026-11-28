// SPDX-License-Identifier: MIT OR Apache-2.0
//! `project.project`: a body of work for a customer, split into tasks that move through the
//! project's own stages.

use erp_sdk::prelude::*;
use erp_sdk::{SearchOptions, ValueMap};

use crate::counts::{as_field, count_by};
use crate::{CLOSED_STATES, MailActivityMixin, MailThread};

/// A project. A project with a company is seen only in that company; a project without one is
/// shared by every company.
#[model(
    name = "project.project",
    description = "Project",
    inherit = [MailThread, MailActivityMixin],
    order = "sequence, name, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow")
)]
pub struct ProjectProject {
    /// Project name.
    #[field(char, required = true, index = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// What the project is about.
    #[field(html, string = "Description")]
    pub description: Option<Arc<str>>,
    /// Archived projects leave the default lists and boards.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Display order.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// Colour of the project card: an index into the client's palette, 0 is no colour.
    #[field(integer, default = 0, string = "Color")]
    pub color: i32,
    /// The customer the work is for.
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "set null",
        index = true,
        tracking = true,
        string = "Customer"
    )]
    pub partner_id: Option<RecId>,
    /// The person responsible for the project.
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "set null",
        index = true,
        tracking = true,
        string = "Project Manager"
    )]
    pub user_id: Option<RecId>,
    /// Labels.
    #[field(many2many, comodel = "project.tags", string = "Tags")]
    pub tag_ids: Vec<RecId>,
    /// Planned start.
    #[field(date, string = "Start Date")]
    pub date_start: Option<Date>,
    /// Planned end.
    #[field(date, string = "End Date")]
    pub date: Option<Date>,
    /// Empty: the project is shared by every company.
    #[field(many2one, comodel = "res.company", index = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// The project's tasks.
    #[field(
        one2many,
        comodel = "project.task",
        inverse_name = "project_id",
        string = "Tasks"
    )]
    pub task_ids: Vec<RecId>,
    /// The project's own stages, copied from the template stages when the project is created. A
    /// copy of the project gets copies of these stages.
    #[field(
        one2many,
        comodel = "project.task.type",
        inverse_name = "project_id",
        copy = true,
        string = "Stages"
    )]
    pub type_ids: Vec<RecId>,
    /// Active tasks of the project the user may read.
    #[field(integer, compute = "_compute_task_counts", string = "Task Count")]
    pub task_count: i32,
    /// Active tasks the user may read that are neither done nor cancelled.
    #[field(integer, compute = "_compute_task_counts", string = "Open Tasks")]
    pub open_task_count: i32,
}

/// The fields a template stage passes to its copies.
const STAGE_COPY_FIELDS: [&str; 4] = ["name", "sequence", "fold", "description"];

/// Whether project values give stages of their own. No commands, an empty command list (what a
/// form sends for a project whose stage page was left alone) and commands that only clear the
/// list give none: the project then gets copies of the template stages.
fn gives_stages(vals: &RawVals) -> bool {
    vals.commands("type_ids").is_some_and(|commands| {
        commands.iter().any(|command| match command {
            Command::Clear => false,
            Command::Set(ids) => !ids.is_empty(),
            _ => true,
        })
    })
}

#[methods(ProjectProject, addon = "project")]
impl ProjectProject {
    /// Create the projects, then give every project whose values carry no stages its own copies
    /// of the template stages, in board order. Without template stages the project starts with
    /// none.
    #[method(override)]
    async fn create(
        this: &RecordSet<ProjectProject>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let wants_templates: Vec<bool> = vals_list.iter().map(|vals| !gives_stages(vals)).collect();
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        let ids = created.ids();
        if ids.len() != wants_templates.len() {
            return Err(Error::invariant(format!(
                "project create returned {} records for {} value sets",
                ids.len(),
                wants_templates.len()
            )));
        }
        let targets: Vec<i64> = ids
            .iter()
            .zip(&wants_templates)
            .filter(|(_, wants)| **wants)
            .map(|(id, _)| *id)
            .collect();
        if targets.is_empty() {
            return Ok(created);
        }
        let stages = this.env().records("project.task.type")?;
        let templates = stages
            .search_with(
                tx,
                &Domain::field("project_id")?.eq(false),
                &SearchOptions::new()
                    .order("sequence, id")
                    .fetch_fields(STAGE_COPY_FIELDS),
            )
            .await?;
        if templates.is_empty() {
            return Ok(created);
        }
        let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(templates.len());
        for template in templates.iter() {
            let mut row = Vec::with_capacity(STAGE_COPY_FIELDS.len());
            for field in STAGE_COPY_FIELDS {
                row.push(template.cell(tx, field).await?);
            }
            rows.push(row);
        }
        let mut copies = Vec::with_capacity(targets.len() * rows.len());
        for project in targets {
            for row in &rows {
                let mut vals = RawVals::new().set_cell("project_id", Cell::Id(project));
                for (field, value) in STAGE_COPY_FIELDS.iter().zip(row) {
                    vals.insert((*field).into(), value.clone());
                }
                copies.push(vals);
            }
        }
        stages.create(tx, copies).await?;
        Ok(created)
    }

    /// Write the projects; a new company is passed on to every task of the projects, archived
    /// ones included, so the tasks stay visible exactly where their project is.
    #[method(override)]
    async fn write(
        this: &RecordSet<ProjectProject>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let projects = this.as_records();
        let company = match vals.get("company_id") {
            Some(Cell::Id(id)) => Some(Cell::Id(*id)),
            Some(Cell::Null | Cell::Bool(false)) => Some(Cell::Null),
            Some(other) => {
                return Err(Error::validation(format!(
                    "company_id must be a company id or empty, not {other:?}."
                ))
                .with_fields(["company_id"]));
            }
            None => None,
        };
        sup.call(tx, projects, (vals,)).await?;
        let Some(company) = company else {
            return Ok(());
        };
        let ids: Vec<i64> = projects.rec_ids().iter().map(|id| id.get()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let tasks = this
            .env()
            .records("project.task")?
            .with_context([("active_test", Value::Bool(false))])
            .search(tx, &Domain::field("project_id")?.in_(ids))
            .await?;
        if !tasks.is_empty() {
            tasks
                .write(tx, RawVals::new().set_cell("company_id", company))
                .await?;
        }
        Ok(())
    }

    #[compute(
        depends = ["task_ids", "task_ids.state", "task_ids.active"],
        depends_context = ["uid", "allowed_company_ids"]
    )]
    async fn _compute_task_counts(
        this: &RecordSet<ProjectProject>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProjectProject>,
    ) -> Result<()> {
        let ids: Vec<i64> = this
            .as_records()
            .rec_ids()
            .iter()
            .map(|id| id.get())
            .collect();
        let env = this.env();
        let all = count_by(tx, env, "project.task", "project_id", &ids, None).await?;
        let open = count_by(
            tx,
            env,
            "project.task",
            "project_id",
            &ids,
            Some(Domain::field("state")?.not_in(CLOSED_STATES.to_vec())),
        )
        .await?;
        for (index, id) in ids.iter().enumerate() {
            out.set(
                index,
                ProjectProject::TASK_COUNT,
                as_field(all.get(id).copied().unwrap_or(0), "task")?,
            )?;
            out.set(
                index,
                ProjectProject::OPEN_TASK_COUNT,
                as_field(open.get(id).copied().unwrap_or(0), "open task")?,
            )?;
        }
        Ok(())
    }

    /// Open the tasks of this project as a list (and form), creating new ones in this project.
    #[method]
    #[rpc(effect = read)]
    async fn action_view_tasks(
        this: &RecordSet<ProjectProject>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = this.as_records().ensure_one()?.int()?;
        let label = match this.as_records().cell(tx, "name").await? {
            Cell::Str(name) => format!("{name} — Tasks"),
            _ => "Tasks".to_owned(),
        };
        Ok(Value::Map(Arc::new(ValueMap::from([
            ("type".into(), Value::str("ir.actions.act_window")),
            ("target".into(), Value::str("current")),
            ("name".into(), Value::str(label)),
            ("res_model".into(), Value::str("project.task")),
            (
                "views".into(),
                Value::list([
                    Value::list([Value::Bool(false), Value::str("list")]),
                    Value::list([Value::Bool(false), Value::str("form")]),
                ]),
            ),
            (
                "domain".into(),
                Value::list([Value::list([
                    Value::str("project_id"),
                    Value::str("="),
                    Value::Int(id),
                ])]),
            ),
            (
                "context".into(),
                Value::Map(Arc::new(ValueMap::from([(
                    "default_project_id".into(),
                    Value::Int(id),
                )]))),
            ),
        ]))))
    }
}
