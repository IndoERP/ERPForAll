// SPDX-License-Identifier: MIT OR Apache-2.0
//! `project.task`: one piece of work in a project, assigned to people and moved through the
//! project's stages.
//!
//! The create and write overrides are the authority for what a task takes from its parent and
//! project and for what a stage move changes, so forms, the board, imports and JSON-2 calls behave
//! the same; the onchanges only suggest values in the form.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

use crate::stage::first_stage;
use crate::{CLOSED_STATES, MailActivityMixin, MailThread};

/// Fields only the server writes.
const MANAGED: &[&str] = &["date_last_stage_update"];

/// A task. A task of a project has its project's company; a task without a project has a company
/// of its own, by default the creating user's current one. A task without a company is shared by
/// every company.
#[model(
    name = "project.task",
    description = "Task",
    inherit = [MailThread, MailActivityMixin],
    order = "priority desc, sequence, id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow")
)]
pub struct ProjectTask {
    /// Task title.
    #[field(char, required = true, index = true, string = "Title")]
    pub name: Option<Arc<str>>,
    /// What has to be done.
    #[field(html, string = "Description")]
    pub description: Option<Arc<str>>,
    /// Archived tasks leave the default lists and boards.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Order inside a stage.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// Colour of the task card: an index into the client's palette, 0 is no colour.
    #[field(integer, default = 0, string = "Color")]
    pub color: i32,
    /// A project that still holds tasks cannot be deleted: archive it, or move or delete its
    /// tasks first.
    #[field(
        many2one,
        comodel = "project.project",
        ondelete = "restrict",
        index = true,
        tracking = true,
        string = "Project"
    )]
    pub project_id: Option<RecId>,
    /// One of the project's stages; empty for a task without a project. A stage that tasks still
    /// use cannot be deleted.
    #[field(
        many2one,
        comodel = "project.task.type",
        ondelete = "restrict",
        index = true,
        tracking = true,
        string = "Stage"
    )]
    pub stage_id: Option<RecId>,
    /// The people working on the task.
    #[field(
        many2many,
        comodel = "res.users",
        relation = "project_task_user_rel",
        column1 = "task_id",
        column2 = "user_id",
        tracking = true,
        string = "Assignees"
    )]
    pub user_ids: Vec<RecId>,
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
    /// Labels.
    #[field(many2many, comodel = "project.tags", string = "Tags")]
    pub tag_ids: Vec<RecId>,
    /// Important tasks come first.
    #[field(
        selection,
        selection = [("0", "Normal"), ("1", "Important")],
        default = "0",
        required = true,
        index = true,
        tracking = true,
        string = "Priority"
    )]
    pub priority: Option<Arc<str>>,
    /// Where the work stands. A stage move puts an open task back to In Progress.
    #[field(
        selection,
        selection = [
            ("in_progress", "In Progress"),
            ("changes_requested", "Changes Requested"),
            ("approved", "Approved"),
            ("done", "Done"),
            ("cancelled", "Cancelled")
        ],
        default = "in_progress",
        required = true,
        index = true,
        tracking = true,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Due date.
    #[field(date, index = true, tracking = true, string = "Deadline")]
    pub date_deadline: Option<Date>,
    /// The task this one is a subtask of. Deleting the parent keeps its subtasks as tasks of
    /// their own.
    #[field(
        many2one,
        comodel = "project.task",
        ondelete = "set null",
        index = true,
        string = "Parent Task"
    )]
    pub parent_id: Option<RecId>,
    /// Subtasks.
    #[field(
        one2many,
        comodel = "project.task",
        inverse_name = "parent_id",
        string = "Subtasks"
    )]
    pub child_ids: Vec<RecId>,
    /// The company that sees the task. A task of a project always has its project's company (set
    /// on create, when the task moves to another project and when the project changes company);
    /// a task without a project gets the creating user's current company. Empty: the task is
    /// shared by every company.
    #[field(many2one, comodel = "res.company", index = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// When the task entered its current stage. Set by the server on create and on every stage
    /// move.
    #[field(datetime, readonly = true, copy = false, string = "Last Stage Update")]
    pub date_last_stage_update: Option<DateTime>,
}

/// Refuse values for the fields only the server writes: in `vals`, and on create also as a
/// `default_<field>` context key.
fn refuse_managed(env: &Env, vals: &RawVals, creating: bool) -> Result<()> {
    for field in MANAGED {
        if vals.contains(field)
            || (creating && env.context().get(&format!("default_{field}")).is_some())
        {
            return Err(Error::validation(format!(
                "Projects maintain {field} themselves: it is set when a task enters a stage."
            ))
            .with_fields([*field]));
        }
    }
    Ok(())
}

/// A many2one cell as an optional id: `false` and NULL are both empty.
fn optional_id(cell: &Cell, field: &str) -> Result<Option<i64>> {
    match cell {
        Cell::Id(id) => Ok(Some(*id)),
        Cell::Null | Cell::Bool(false) => Ok(None),
        other => Err(Error::validation(format!(
            "{field} must be a record id or empty, not {other:?}."
        ))
        .with_fields([field])),
    }
}

/// The id a `default_<field>` context key holds, if any.
fn context_id(env: &Env, field: &str) -> Result<Option<i64>> {
    match env.context().get(&format!("default_{field}")) {
        None | Some(Value::False) => Ok(None),
        Some(Value::Int(id)) => Ok(Some(id)),
        Some(other) => Err(Error::validation(format!(
            "The context key default_{field} must be a record id, not {other:?}."
        ))),
    }
}

/// Whether the context carries a `default_<field>` key (even an empty one).
fn has_default(env: &Env, field: &str) -> bool {
    env.context().get(&format!("default_{field}")).is_some()
}

/// Whether a task state is one of [`CLOSED_STATES`].
fn is_closed(state: &Cell) -> bool {
    matches!(state, Cell::Str(s) if CLOSED_STATES.contains(&s.as_ref()))
}

/// The id held by the many2one `field` of one record, if any.
async fn field_id(tx: &mut Transaction, record: &Records, field: &str) -> Result<Option<i64>> {
    optional_id(&record.cell(tx, field).await?, field)
}

/// The company of `project` (empty for a shared project).
async fn project_company(env: &Env, tx: &mut Transaction, project: i64) -> Result<Option<i64>> {
    field_id(
        tx,
        &env.records("project.project")?.browse([project])?,
        "company_id",
    )
    .await
}

/// What a new task takes from its project, read once per project in one `create` call.
#[derive(Clone, Copy)]
struct ProjectFacts {
    first_stage: Option<i64>,
    customer: Option<i64>,
    company: Option<i64>,
}

/// The projects already read during one task `create` call.
#[derive(Default)]
struct ProjectCache(BTreeMap<i64, ProjectFacts>);

impl ProjectCache {
    /// The facts of `project`, read on the first request.
    async fn get(&mut self, env: &Env, tx: &mut Transaction, project: i64) -> Result<ProjectFacts> {
        if let Some(facts) = self.0.get(&project) {
            return Ok(*facts);
        }
        let record = env.records("project.project")?.browse([project])?;
        let facts = ProjectFacts {
            first_stage: first_stage(env, tx, project).await?,
            customer: field_id(tx, &record, "partner_id").await?,
            company: field_id(tx, &record, "company_id").await?,
        };
        self.0.insert(project, facts);
        Ok(facts)
    }
}

/// The fields a new task may take from its parent or project. An empty value given for one of
/// them (as a form sends for a field the user left empty) counts as not given.
const INHERITED: [&str; 3] = ["project_id", "partner_id", "stage_id"];

/// Fill what a new task takes from its parent and project: project (explicit value, else the
/// `default_project_id` context, else the parent's), customer (explicit or context value, else
/// the parent's, else the project's), stage (explicit or context value, else the project's
/// first unfolded stage) and company (the project's; without a project an explicit or context
/// value, else the user's current company).
async fn complete_new_task(
    env: &Env,
    tx: &mut Transaction,
    vals: &mut RawVals,
    cache: &mut ProjectCache,
) -> Result<()> {
    for field in INHERITED {
        if let Some(cell) = vals.get(field)
            && optional_id(cell, field)?.is_none()
        {
            vals.remove(field);
        }
    }
    let parent = match vals.get("parent_id") {
        Some(cell) => optional_id(cell, "parent_id")?,
        None => context_id(env, "parent_id")?,
    };
    let parent = match parent {
        Some(id) => Some(env.records("project.task")?.browse([id])?),
        None => None,
    };
    let project = match vals.get("project_id") {
        Some(cell) => optional_id(cell, "project_id")?,
        None => match context_id(env, "project_id")? {
            Some(id) => Some(id),
            None => match &parent {
                Some(parent) => field_id(tx, parent, "project_id").await?,
                None => None,
            },
        },
    };
    let facts = match project {
        Some(id) => Some(cache.get(env, tx, id).await?),
        None => None,
    };
    if !vals.contains("project_id")
        && let Some(id) = project
    {
        vals.insert("project_id".into(), Cell::Id(id));
    }
    if !vals.contains("partner_id") && !has_default(env, "partner_id") {
        let mut customer = match &parent {
            Some(parent) => field_id(tx, parent, "partner_id").await?,
            None => None,
        };
        if customer.is_none() {
            customer = facts.and_then(|facts| facts.customer);
        }
        if let Some(id) = customer {
            vals.insert("partner_id".into(), Cell::Id(id));
        }
    }
    if !vals.contains("stage_id")
        && !has_default(env, "stage_id")
        && let Some(stage) = facts.and_then(|facts| facts.first_stage)
    {
        vals.insert("stage_id".into(), Cell::Id(stage));
    }
    match facts {
        // an explicit other company is refused by `_check_company_project`
        Some(facts) => {
            let given = match vals.get("company_id") {
                Some(cell) => optional_id(cell, "company_id")?,
                None => None,
            };
            if given.is_none() {
                vals.insert(
                    "company_id".into(),
                    facts.company.map_or(Cell::Null, Cell::Id),
                );
            }
        }
        None => {
            if !vals.contains("company_id")
                && !has_default(env, "company_id")
                && let Some(company) = env.company_id()?
            {
                vals.insert("company_id".into(), Cell::Id(company));
            }
        }
    }
    Ok(())
}

#[methods(ProjectTask, addon = "project")]
impl ProjectTask {
    /// A new task's company in the form: the `default_project_id` project's company, else the
    /// user's current company.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ProjectTask>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut vals: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        let env = this.env();
        if !fields.iter().any(|field| field == "company_id") || vals.contains_key("company_id") {
            return Ok(vals);
        }
        let company = match context_id(env, "project_id")? {
            Some(project) => project_company(env, tx, project).await?,
            None => env.company_id()?,
        };
        vals.insert(
            "company_id".into(),
            company.map_or(Value::False, Value::Int),
        );
        Ok(vals)
    }

    /// Create the tasks after filling what each takes from its parent and project (see
    /// [`complete_new_task`]), and stamp when each entered its stage. The projects are read once
    /// per call, however many tasks share them.
    #[method(override)]
    async fn create(
        this: &RecordSet<ProjectTask>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env();
        let now = DateTime::now();
        let mut cache = ProjectCache::default();
        for vals in &mut vals_list {
            refuse_managed(env, vals, true)?;
            complete_new_task(env, tx, vals, &mut cache).await?;
            vals.insert("date_last_stage_update".into(), Cell::DateTime(now));
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Write the tasks. A move to another project without a stage puts the task in that
    /// project's first unfolded stage (none without a project) and gives it the project's
    /// company; a real stage change stamps the stage time and puts an open task back to In
    /// Progress unless the same write sets the state. Several tasks moving at once are written
    /// one by one inside one savepoint: all or none.
    #[method(override)]
    async fn write(
        this: &RecordSet<ProjectTask>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let tasks = this.as_records();
        refuse_managed(this.env(), &vals, false)?;
        if tasks.is_empty() || !(vals.contains("stage_id") || vals.contains("project_id")) {
            return sup.call(tx, tasks, (vals,)).await;
        }
        let env = this.env();
        tx.savepoint(async move |tx| {
            if tasks.len() > 1 {
                // each task has its own project and stage: move them one by one, all or none
                for task in tasks.iter() {
                    task.write(tx, vals.clone()).await?;
                }
                return Ok(());
            }
            if let Some(cell) = vals.get("project_id") {
                let project = optional_id(cell, "project_id")?;
                if project != field_id(tx, tasks, "project_id").await? {
                    if !vals.contains("stage_id") {
                        let stage = match project {
                            Some(id) => first_stage(env, tx, id).await?,
                            None => None,
                        };
                        vals.insert("stage_id".into(), stage.map_or(Cell::Null, Cell::Id));
                    }
                    let given = match vals.get("company_id") {
                        Some(cell) => optional_id(cell, "company_id")?,
                        None => None,
                    };
                    // an explicit other company is refused by `_check_company_project`; a task
                    // leaving every project keeps its company
                    if given.is_none()
                        && let Some(id) = project
                    {
                        let company = project_company(env, tx, id).await?;
                        vals.insert("company_id".into(), company.map_or(Cell::Null, Cell::Id));
                    }
                }
            }
            if let Some(cell) = vals.get("stage_id") {
                let stage = optional_id(cell, "stage_id")?;
                if stage != field_id(tx, tasks, "stage_id").await? {
                    vals.insert(
                        "date_last_stage_update".into(),
                        Cell::DateTime(DateTime::now()),
                    );
                    if !vals.contains("state") && !is_closed(&tasks.cell(tx, "state").await?) {
                        vals.insert("state".into(), Cell::Str("in_progress".into()));
                    }
                }
            }
            sup.call(tx, tasks, (vals,)).await
        })
        .await
    }

    /// A task of a project uses one of that project's stages; a task without a project has none.
    #[constrains("stage_id", "project_id")]
    async fn _check_stage_project(
        this: &RecordSet<ProjectTask>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let stages = this.env().records("project.task.type")?;
        for task in this.as_records().iter() {
            let Some(stage) = field_id(tx, &task, "stage_id").await? else {
                continue;
            };
            let Some(project) = field_id(tx, &task, "project_id").await? else {
                return Err(Error::validation(
                    "A task without a project has no stage, because every stage belongs to one \
                     project: set the project first or clear the stage (stage_id).",
                )
                .with_fields(["stage_id"]));
            };
            let owner = field_id(tx, &stages.browse([stage])?, "project_id").await?;
            if owner != Some(project) {
                return Err(Error::validation(
                    "The stage (stage_id) of a task must be one of its project's own stages.",
                )
                .with_fields(["stage_id"]));
            }
        }
        Ok(())
    }

    /// A task of a project has the project's company.
    #[constrains("company_id", "project_id")]
    async fn _check_company_project(
        this: &RecordSet<ProjectTask>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let env = this.env();
        let mut companies: BTreeMap<i64, Option<i64>> = BTreeMap::new();
        for task in this.as_records().iter() {
            let Some(project) = field_id(tx, &task, "project_id").await? else {
                continue;
            };
            let expected = match companies.get(&project) {
                Some(company) => *company,
                None => {
                    let company = project_company(env, tx, project).await?;
                    companies.insert(project, company);
                    company
                }
            };
            if field_id(tx, &task, "company_id").await? != expected {
                return Err(Error::validation(
                    "A task of a project has the project's company (company_id): change the \
                     project's company, or move the task out of the project first.",
                )
                .with_fields(["company_id"]));
            }
        }
        Ok(())
    }

    /// A task is never its own subtask, directly or through other subtasks.
    #[constrains("parent_id")]
    async fn _check_parent_recursion(
        this: &RecordSet<ProjectTask>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let tasks = this.env().records("project.task")?;
        for id in this.as_records().rec_ids() {
            let start = id.get();
            let mut seen = BTreeSet::from([start]);
            let mut cursor = start;
            while let Some(parent) = field_id(tx, &tasks.browse([cursor])?, "parent_id").await? {
                if !seen.insert(parent) {
                    return Err(Error::validation(
                        "A task cannot be its own subtask, directly or through other subtasks.",
                    )
                    .with_fields(["parent_id"]));
                }
                cursor = parent;
            }
        }
        Ok(())
    }

    /// Choosing a project suggests its first stage (when the current stage is not one of the
    /// project's), its company, and its customer (when the task has none). Clearing the project
    /// clears the stage.
    #[onchange("project_id")]
    async fn _onchange_project(
        this: &RecordSet<ProjectTask>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let task = this.as_records();
        let env = this.env();
        let stage = task.related(tx, "stage_id").await?;
        let Some(project) = field_id(tx, task, "project_id").await? else {
            if stage.is_empty() {
                return Ok(OnchangeResult::none());
            }
            return Ok(OnchangeResult::none().value("stage_id", Value::False));
        };
        let mut result = OnchangeResult::none();
        if stage.is_empty() || field_id(tx, &stage, "project_id").await? != Some(project) {
            let first = first_stage(env, tx, project).await?;
            result = result.value("stage_id", first.map_or(Value::False, Value::Int));
        }
        let record = env.records("project.project")?.browse([project])?;
        let company = field_id(tx, &record, "company_id").await?;
        result = result.value("company_id", company.map_or(Value::False, Value::Int));
        if field_id(tx, task, "partner_id").await?.is_none()
            && let Some(customer) = field_id(tx, &record, "partner_id").await?
        {
            result = result.value("partner_id", Value::Int(customer));
        }
        Ok(result)
    }

    /// Choosing a parent suggests its project (with that project's first stage and company) when
    /// the task has none, and its customer, else the project's, when the task has none.
    #[onchange("parent_id")]
    async fn _onchange_parent(
        this: &RecordSet<ProjectTask>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let task = this.as_records();
        let env = this.env();
        let parent = task.related(tx, "parent_id").await?;
        if parent.is_empty() {
            return Ok(OnchangeResult::none());
        }
        let mut result = OnchangeResult::none();
        let mut project = field_id(tx, task, "project_id").await?;
        if project.is_none()
            && let Some(id) = field_id(tx, &parent, "project_id").await?
        {
            project = Some(id);
            result = result.value("project_id", Value::Int(id));
            let first = first_stage(env, tx, id).await?;
            result = result.value("stage_id", first.map_or(Value::False, Value::Int));
            let company = project_company(env, tx, id).await?;
            result = result.value("company_id", company.map_or(Value::False, Value::Int));
        }
        if field_id(tx, task, "partner_id").await?.is_none() {
            let mut customer = field_id(tx, &parent, "partner_id").await?;
            if customer.is_none()
                && let Some(id) = project
            {
                let record = env.records("project.project")?.browse([id])?;
                customer = field_id(tx, &record, "partner_id").await?;
            }
            if let Some(id) = customer {
                result = result.value("partner_id", Value::Int(id));
            }
        }
        Ok(result)
    }
}
