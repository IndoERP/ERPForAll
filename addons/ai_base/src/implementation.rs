// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company implementation projects; a plan documents work without executing live test transactions.
mod catalog;
mod contacts;
mod import;
mod settings;
pub use catalog::{Coverage, Scenario, coverage};
use erp_sdk::{Operation, prelude::*};
pub use import::{StepImport, import_fingerprint, import_fingerprint_iter};
use serde_json::json;
use std::collections::BTreeSet;

#[model(name = "ai.implementation.project", description = "Implementation Project", order = "name, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [("_company_name", "UNIQUE(company_id, name)", "This company already has an implementation project with this name.")])]
pub struct Project {
    #[field(char, required = true, size = 160)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "set null",
        string = "Project lead"
    )]
    pub owner_id: Option<RecId>,
    #[field(text, string = "Business requirements")]
    pub requirements: Option<Arc<str>>,
    #[field(
        one2many,
        comodel = "ai.implementation.step",
        inverse_name = "project_id",
        string = "Implementation steps"
    )]
    pub step_ids: Vec<RecId>,
}

#[model(name = "ai.implementation.step", description = "Implementation Step", order = "sequence, id",
    company_access = parent(edge = "project_id"),
    constraints = [("_project_addon", "UNIQUE(project_id, addon)", "This project already has a step for this addon.")])]
pub struct Step {
    #[field(
        many2one,
        comodel = "ai.implementation.project",
        required = true,
        index = true,
        ondelete = "cascade"
    )]
    pub project_id: Option<RecId>,
    #[field(char, size = 128)]
    pub addon: Option<Arc<str>>,
    #[field(boolean, compute = "_compute_has_records")]
    pub has_records: bool,
    #[field(char, required = true, size = 160)]
    pub name: Option<Arc<str>>,
    #[field(integer, default = 10)]
    pub sequence: i32,
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "set null",
        string = "Responsible"
    )]
    pub owner_id: Option<RecId>,
    #[field(selection, required = true, default = "planned", selection = [("planned","Planned"), ("in_progress","In progress"), ("waiting","Awaiting decision"), ("gap","Fit-gap"), ("done","Verified")])]
    pub state: Option<Arc<str>>,
    #[field(text, string = "Prerequisite addons")]
    pub prerequisites: Option<Arc<str>>,
    #[field(text, string = "Setup and master data")]
    pub setup: Option<Arc<str>>,
    #[field(text, string = "Acceptance criteria")]
    pub acceptance: Option<Arc<str>>,
    #[field(text, string = "Evidence or decision reference")]
    pub evidence: Option<Arc<str>>,
    #[field(text, string = "Customer requirement")]
    pub requirement: Option<Arc<str>>,
    #[field(text, string = "Closest standard feature")]
    pub standard_feature: Option<Arc<str>>,
    #[field(text, string = "Unmet behaviour")]
    pub gap: Option<Arc<str>>,
    #[field(text, string = "Example business case")]
    pub example: Option<Arc<str>>,
    #[field(
        one2many,
        comodel = "ai.implementation.import",
        inverse_name = "step_id",
        string = "Imports"
    )]
    pub import_ids: Vec<RecId>,
}

fn company(env: &Env) -> Result<i64> {
    env.company()?
        .map(|company| company.get())
        .ok_or_else(|| Error::access("Select a company before opening its implementation."))
}

fn form(project_id: i64) -> Value {
    Value::Json(Arc::new(
        json!({"type":"ir.actions.act_window", "name":"Implementation Project",
        "res_model":"ai.implementation.project","res_id":project_id,"target":"current","views":[[false,"form"]]}),
    ))
}

async fn same_company(tx: &mut Transaction, project: &Records) -> Result<()> {
    if project.cell(tx, "company_id").await? != Cell::Id(company(project.env())?) {
        return Err(Error::access(
            "Open the implementation in its active company.",
        ));
    }
    Ok(())
}

async fn assigned_owner(tx: &mut Transaction, record: &Records, scope: i64) -> Result<()> {
    let owner = match record.cell(tx, "owner_id").await? {
        Cell::Null => return Ok(()),
        Cell::Id(id) => id,
        _ => return Err(Error::invariant("Invalid implementation assignee.")),
    };
    let allowed = Domain::field("id")?.eq(owner)
        & (Domain::field("company_id")?.eq(scope) | Domain::field("company_ids")?.in_(vec![scope]));
    if record
        .env()
        .records("res.users")?
        .search_count(tx, &allowed, Some(1))
        .await?
        != 1
    {
        return Err(Error::validation(
            "Choose a project lead or step owner who belongs to the project company.",
        ));
    }
    Ok(())
}

async fn refresh(tx: &mut Transaction, project: &Records) -> Result<Value> {
    project.ensure_one()?;
    project.check_access(tx, Operation::Write).await?;
    same_company(tx, project).await?;
    project.lock_for_update(tx, true).await?;
    let project_id = project.int()?;
    let env = project.env();
    let steps = env.records("ai.implementation.step")?;
    let installed: Vec<_> = env
        .registry()
        .addons()
        .iter()
        .map(|addon| addon.name)
        .collect();
    let bound_addons =
        Domain::field("project_id")?.eq(project_id) & Domain::field("addon")?.in_(installed);
    let existing = steps.search(tx, &bound_addons).await?;
    let mut known = BTreeSet::new();
    for row in existing.read(tx, &["addon"]).await? {
        if let Some(Value::Str(addon)) = row.get("addon") {
            known.insert(addon.to_string());
        }
    }
    let mut missing = Vec::new();
    for (index, descriptor) in env.registry().addons().iter().enumerate() {
        if known.contains(descriptor.name) {
            continue;
        }
        let scenario = match coverage(descriptor.name) {
            Coverage::Platform => continue,
            Coverage::Scenario(rule) => rule,
            Coverage::Unknown => Scenario {
                title: descriptor.name,
                model: "",
                setup: "Interview the process owner and document this addon's configuration, master data and permissions.",
                acceptance: "Agree a representative business case with the owner, run it with test data and record the result.",
            },
        };
        let sequence = i32::try_from(index + 1)
            .map_err(|_| Error::validation("Too many installed addons."))?;
        let prerequisites = descriptor
            .depends
            .iter()
            .copied()
            .filter(|name| !matches!(coverage(name), Coverage::Platform))
            .collect::<Vec<_>>()
            .join(", ");
        missing.push(
            RawVals::new()
                .set_cell("project_id", Cell::Id(project_id))
                .set_cell("addon", Cell::Str(descriptor.name.into()))
                .set_cell("name", Cell::Str(scenario.title.into()))
                .set_cell("sequence", Cell::Int(sequence))
                .set_cell("prerequisites", Cell::Str(prerequisites.into()))
                .set_cell("setup", Cell::Str(scenario.setup.into()))
                .set_cell("acceptance", Cell::Str(scenario.acceptance.into())),
        );
    }
    if !missing.is_empty() {
        steps.create(tx, missing).await?;
    }
    Ok(form(project_id))
}

#[methods(Project, addon = "ai_base")]
impl Project {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<Project>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let wants_company = fields.iter().any(|field| field == "company_id");
        let mut values: erp_sdk::ValueMap = sup.call(tx, this.as_records(), (fields,)).await?;
        if wants_company && let Some(company) = this.env().company_id()? {
            values.insert("company_id".into(), Value::Int(company));
        }
        Ok(values)
    }

    #[constrains("company_id", "owner_id")]
    async fn _check_project_scope(this: &RecordSet<Project>, tx: &mut Transaction) -> Result<()> {
        let scope = company(this.env())?;
        for project in this.as_records().iter() {
            same_company(tx, &project).await?;
            assigned_owner(tx, &project, scope).await?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = write, model, assistant = offer)]
    async fn start_implementation(
        this: &RecordSet<Project>,
        tx: &mut Transaction,
        name: String,
        requirements: String,
    ) -> Result<Value> {
        let name = name.trim();
        if name.is_empty() || name.len() > 160 || requirements.len() > 8192 {
            return Err(Error::validation(
                "Enter a project name of 1–160 bytes and requirements of at most 8192 bytes.",
            ));
        }
        let scope = company(this.env())?;
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        // Serialize concurrent starts, including the first one when there is no project row to lock.
        tx.query("SELECT pg_advisory_xact_lock(hashtextextended('ai.implementation.' || $1::bigint::text || '.' || $2::text, 0))",
            &[&scope, &name]).await?;
        let domain = Domain::field("company_id")?.eq(scope) & Domain::field("name")?.eq(name);
        let project = this.as_records().search(tx, &domain).await?;
        let project = if project.is_empty() {
            this.as_records()
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("company_id", Cell::Id(scope))
                            .set_cell("owner_id", Cell::Id(this.env().user_id().get()))
                            .set_cell("requirements", Cell::Str(requirements.into())),
                    ],
                )
                .await?
        } else {
            if !requirements.trim().is_empty()
                && project.cell(tx, "requirements").await? != Cell::Str(requirements.into())
            {
                return Err(Error::validation(
                    "An existing project has this name with different requirements. Open that project to update its scope.",
                ));
            }
            project
        };
        refresh(tx, &project).await
    }

    #[method]
    #[rpc(effect = write, assistant = offer)]
    async fn action_refresh_steps(
        this: &RecordSet<Project>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        refresh(tx, this.as_records()).await
    }

    /// Open the standard configuration page under its normal Settings menu permission.
    #[method]
    #[rpc(effect = read, assistant = offer)]
    async fn action_open_settings(
        this: &RecordSet<Project>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let project = this.ensure_one()?.as_records();
        project.check_access(tx, Operation::Read).await?;
        same_company(tx, project).await?;
        if !tx.has_group(this.env(), "base.group_system").await? {
            return Err(Error::access(
                "Opening Settings requires a settings administrator.",
            ));
        }
        let view = tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name='settings_form' AND model='ir.ui.view'", &[]).await?;
        let id: i64 = view
            .first()
            .ok_or_else(|| Error::user("The native Settings form is unavailable."))?
            .try_get(0)
            .map_err(|error| Error::db("Invalid native Settings view.").with_source(error))?;
        Ok(Value::Json(Arc::new(
            json!({"type":"ir.actions.act_window","name":"Settings","res_model":"ir.config.setting",
            "views":[[id,"form"]],"target":"current"}),
        )))
    }

    /// Return the active project's playbooks and progress in bounded pages.
    #[method]
    #[rpc(effect = read, assistant = offer)]
    async fn implementation_steps(
        this: &RecordSet<Project>,
        tx: &mut Transaction,
        offset: i64,
        limit: i64,
    ) -> Result<Value> {
        let project = this.ensure_one()?.as_records();
        project.check_access(tx, Operation::Read).await?;
        same_company(tx, project).await?;
        if !(0..=100_000).contains(&offset) || !(1..=25).contains(&limit) {
            return Err(Error::validation(
                "Use an offset of 0–100000 and a page size from 1 to 25.",
            ));
        }
        let steps = this.env().records("ai.implementation.step")?;
        let domain = Domain::field("project_id")?.eq(project.int()?);
        let steps = steps
            .search_with(
                tx,
                &domain,
                &erp_sdk::SearchOptions::new()
                    .offset(offset.unsigned_abs())
                    .limit(limit.unsigned_abs() + 1),
            )
            .await?;
        let has_more = steps.len() > limit.unsigned_abs() as usize;
        let page = steps.browse(steps.ids().into_iter().take(limit.unsigned_abs() as usize))?;
        let rows = page
            .read(
                tx,
                &[
                    "id",
                    "name",
                    "addon",
                    "state",
                    "prerequisites",
                    "setup",
                    "acceptance",
                ],
            )
            .await?;
        Ok(Value::Map(Arc::new(erp_sdk::ValueMap::from([
            (
                "steps".to_owned(),
                Value::list(
                    rows.into_iter()
                        .map(|row| Value::Map(Arc::new(row.into_iter().collect()))),
                ),
            ),
            ("has_more".to_owned(), Value::Bool(has_more)),
        ]))))
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<Project>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(company) = vals.get("company_id") {
            for project in this.as_records().iter() {
                if project.cell(tx, "company_id").await? != *company {
                    return Err(Error::validation(
                        "An implementation project's company is fixed.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}

async fn update_step(tx: &mut Transaction, step: &Records, values: RawVals) -> Result<bool> {
    step.check_access(tx, Operation::Write).await?;
    let project = step.related(tx, "project_id").await?;
    same_company(tx, &project).await?;
    step.write(tx, values).await?;
    Ok(true)
}

#[methods(Step, addon = "ai_base")]
impl Step {
    #[compute(depends = ["addon"])]
    async fn _compute_has_records(
        this: &RecordSet<Step>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Step>,
    ) -> Result<()> {
        for (index, step) in this.as_records().iter().enumerate() {
            let available = match step.cell(tx, "addon").await? {
                Cell::Str(addon) => {
                    matches!(coverage(&addon), Coverage::Scenario(_))
                        && this
                            .env()
                            .registry()
                            .addons()
                            .iter()
                            .any(|descriptor| descriptor.name == addon.as_ref())
                }
                _ => false,
            };
            out.set(index, Self::HAS_RECORDS, available)?;
        }
        Ok(())
    }

    #[constrains("project_id", "owner_id")]
    async fn _check_step_scope(this: &RecordSet<Step>, tx: &mut Transaction) -> Result<()> {
        let scope = company(this.env())?;
        for step in this.as_records().iter() {
            let project = step.related(tx, "project_id").await?;
            same_company(tx, &project).await?;
            assigned_owner(tx, &step, scope).await?;
        }
        Ok(())
    }

    /// Record an unresolved requirement with a business example and its acceptance test.
    #[method]
    #[rpc(effect = write, assistant = offer)]
    async fn record_fit_gap(
        this: &RecordSet<Step>,
        tx: &mut Transaction,
        requirement: String,
        standard_feature: String,
        gap: String,
        example: String,
        acceptance: String,
    ) -> Result<bool> {
        let mut values = RawVals::new().set_cell("state", Cell::Str("gap".into()));
        for (field, value) in [
            ("requirement", requirement),
            ("standard_feature", standard_feature),
            ("gap", gap),
            ("example", example),
            ("acceptance", acceptance),
        ] {
            if value.trim().is_empty() || value.len() > 2000 {
                return Err(Error::validation(format!(
                    "Enter 1–2000 bytes for {field}."
                )));
            }
            values = values.set_cell(field, Cell::Str(value.trim().into()));
        }
        update_step(tx, this.ensure_one()?.as_records(), values).await
    }

    /// Set progress and evidence. A verified step requires acceptance criteria and a decision or test reference.
    #[method]
    #[rpc(effect = write, assistant = offer)]
    async fn record_progress(
        this: &RecordSet<Step>,
        tx: &mut Transaction,
        state: String,
        evidence: String,
    ) -> Result<bool> {
        if !matches!(
            state.as_str(),
            "planned" | "in_progress" | "waiting" | "done"
        ) || evidence.len() > 2000
        {
            return Err(Error::validation(
                "Choose planned, in_progress, waiting or done and provide at most 2000 bytes of evidence.",
            ));
        }
        let values = RawVals::new()
            .set_cell("state", Cell::Str(state.into()))
            .set_cell("evidence", Cell::Str(evidence.trim().into()));
        update_step(tx, this.ensure_one()?.as_records(), values).await
    }

    #[constrains(
        "state",
        "requirement",
        "standard_feature",
        "gap",
        "example",
        "acceptance",
        "evidence"
    )]
    async fn _check_outcome(this: &RecordSet<Step>, tx: &mut Transaction) -> Result<()> {
        for row in this.as_records().iter() {
            let fields: &[&str] =
                match crate::models::text(row.cell(tx, "state").await?, "state")?.as_str() {
                    "done" => &["acceptance", "evidence"],
                    "gap" => &[
                        "requirement",
                        "standard_feature",
                        "gap",
                        "example",
                        "acceptance",
                    ],
                    _ => continue,
                };
            for &field in fields {
                if !matches!(row.cell(tx, field).await?, Cell::Str(ref value) if !value.trim().is_empty())
                {
                    return Err(Error::validation(format!(
                        "Complete {field} before saving this implementation outcome."
                    )));
                }
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<Step>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        for field in ["addon", "project_id"] {
            if let Some(new) = vals.get(field) {
                for step in this.as_records().iter() {
                    if step.cell(tx, field).await? != *new {
                        return Err(Error::validation(
                            "The implementation step's project and addon are fixed.",
                        ));
                    }
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// Open the standard list, which uses the normal guided import and model permissions.
    #[method]
    #[rpc(effect = read, assistant = offer)]
    async fn action_open_data(this: &RecordSet<Step>, tx: &mut Transaction) -> Result<Value> {
        let step = this.ensure_one()?.as_records();
        step.check_access(tx, Operation::Read).await?;
        let project = step.related(tx, "project_id").await?;
        same_company(tx, &project).await?;
        let addon = crate::models::text(step.cell(tx, "addon").await?, "Addon")?;
        let Coverage::Scenario(rule) = coverage(&addon) else {
            return Err(Error::user(
                "This step has no registered master data view. Record the required entry point in its blueprint.",
            ));
        };
        if !this
            .env()
            .registry()
            .addons()
            .iter()
            .any(|installed| installed.name == addon)
        {
            return Err(Error::user(
                "The addon for this implementation step is no longer installed.",
            ));
        }
        this.env()
            .records(rule.model)?
            .check_access(tx, Operation::Read)
            .await?;
        Ok(Value::Json(Arc::new(
            json!({"type":"ir.actions.act_window","name":rule.title,
            "res_model":rule.model,"views":[[false,"list"],[false,"form"]],"target":"current",
                "context":{"ai_implementation_step_id":step.int()?}}),
        )))
    }
}
