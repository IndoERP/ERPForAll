// SPDX-License-Identifier: MIT OR Apache-2.0
//! Editable sales processes. Stages belong to one pipeline for their lifetime.
use erp_sdk::Operation;
use erp_sdk::prelude::*;

erp_sdk::model_ref!(pub Team = "crm.team");

#[model(name = "crm.pipeline", description = "Sales Pipeline", order = "sequence, id",
company_access = direct(field = "company_id", branch = "exact", empty = "allow"))]
pub struct Pipeline {
    #[field(char, required = true, size = 128, string = "Pipeline")]
    pub name: Option<Arc<str>>,
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(
        many2one,
        comodel = "res.company",
        string = "Company",
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    #[field(
        one2many,
        comodel = "crm.stage",
        inverse_name = "pipeline_id",
        string = "Stages"
    )]
    pub stage_ids: Vec<RecId>,
}

#[model(name = "crm.stage", description = "Pipeline Stage", order = "sequence, id", company_access = parent(edge = "pipeline_id"),
constraints = [("_probability", "CHECK(probability BETWEEN 0 AND 100)", "Use a probability between 0 and 100.")])]
pub struct Stage {
    #[field(char, required = true, size = 128, string = "Stage")]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "crm.pipeline",
        required = true,
        index = true,
        ondelete = "restrict",
        string = "Pipeline"
    )]
    pub pipeline_id: Option<RecId>,
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    #[field(integer, default = 10, string = "Default Probability (%)")]
    pub probability: i32,
    #[field(selection, selection = [("pipeline","Pipeline"),("best_case","Best Case"),("commit","Commit"),("omitted","Omitted")], default = "pipeline", required = true, string = "Forecast Category")]
    pub forecast_category: Option<Arc<str>>,
    #[field(boolean, string = "Require Customer")]
    pub require_partner: bool,
    #[field(boolean, string = "Require Close Date")]
    pub require_close_date: bool,
    #[field(boolean, string = "Require Positive Value")]
    pub require_value: bool,
    #[field(boolean, string = "Require Next Step")]
    pub require_next_step: bool,
}

#[extend(Team, addon = "crm")]
pub struct TeamPipelines {
    #[field(
        many2one,
        comodel = "crm.pipeline",
        ondelete = "restrict",
        string = "Default Pipeline"
    )]
    pub crm_pipeline_id: Option<RecId>,
    #[field(
        many2many,
        comodel = "crm.pipeline",
        relation = "crm_team_pipeline_rel",
        column1 = "team_id",
        column2 = "pipeline_id",
        string = "Additional Pipelines"
    )]
    pub crm_pipeline_ids: Vec<RecId>,
}

#[methods(Team, addon = "crm")]
impl TeamPipelines {
    #[constrains("company_id", "crm_pipeline_id", "crm_pipeline_ids")]
    async fn _check_pipeline_scope(this: &RecordSet<Team>, tx: &mut Transaction) -> Result<()> {
        for team in this.as_records().iter() {
            let company = team.cell(tx, "company_id").await?;
            let default = team.related(tx, "crm_pipeline_id").await?;
            let allowed = team.related(tx, "crm_pipeline_ids").await?;
            for pipe in default.iter().chain(allowed.iter()) {
                let scope = pipe.cell(tx, "company_id").await?;
                if !scope.is_null() && scope != company {
                    return Err(Error::validation(
                        "A team's pipelines must be shared or belong to the team's company.",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[methods(Pipeline, addon = "crm")]
impl Pipeline {
    #[method(override)]
    async fn write(
        this: &RecordSet<Pipeline>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(company) = vals.get("company_id") {
            for pipe in this.as_records().iter() {
                if pipe.cell(tx, "company_id").await? != *company {
                    return Err(Error::validation(
                        "A pipeline's company is fixed. Create a pipeline for the destination company.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}

#[methods(Stage, addon = "crm")]
impl Stage {
    #[method(override)]
    async fn write(
        this: &RecordSet<Stage>,
        tx: &mut Transaction,
        values: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(new) = values.get("pipeline_id") {
            for stage in this.as_records().iter() {
                if stage.cell(tx, "pipeline_id").await? != *new {
                    return Err(Error::validation(
                        "A stage belongs to one pipeline. Create a new stage in the destination pipeline.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (values,)).await
    }
}

pub(crate) async fn first(
    tx: &mut Transaction,
    set: &Records,
    domain: &Domain,
) -> Result<Option<i64>> {
    Ok(set
        .search_with(tx, domain, &erp_sdk::SearchOptions::new().limit(1))
        .await?
        .ids()
        .first()
        .copied())
}

pub(crate) async fn assert_company(
    tx: &mut Transaction,
    rec: &Records,
    company: i64,
) -> Result<()> {
    rec.check_access(tx, Operation::Read).await?;
    if matches!(rec.cell(tx, "company_id").await?, Cell::Id(other) if other != company) {
        return Err(Error::validation(
            "The selected record belongs to another company.",
        ));
    }
    Ok(())
}

pub(crate) async fn default_team(
    env: &Env,
    tx: &mut Transaction,
    company: i64,
) -> Result<Option<i64>> {
    // The generic sales-team selector may fall back to a team from another enabled
    // company. A CRM pipeline is company-specific, including in mixed-company imports.
    env.with_context([("allowed_company_ids", Value::list([Value::Int(company)]))])
        .records("crm.team")?
        .call(
            tx,
            "_get_default_team_id",
            (None::<i64>, Some(company), None::<i64>),
        )
        .await
}

pub(crate) async fn default_pipeline(
    env: &Env,
    tx: &mut Transaction,
    company: i64,
    team: Option<i64>,
    selected: Option<i64>,
) -> Result<i64> {
    let set = env.records("crm.pipeline")?;
    let active = Domain::field("active")?.eq(true);
    let own = Domain::field("company_id")?.eq(company);
    let shared = Domain::field("company_id")?.eq(false);
    let eligible = active.clone() & (own.clone() | shared.clone());
    if let Some(team) = team {
        let rec = env
            .records("crm.team")?
            .browse([team])?
            .with_context([("active_test", Value::Bool(false))]);
        let preferred = rec
            .related(tx, "crm_pipeline_id")
            .await?
            .ids()
            .first()
            .copied();
        let mut configured = rec.related(tx, "crm_pipeline_ids").await?.ids();
        configured.extend(preferred);
        if !configured.is_empty() {
            let permitted = set
                .search(tx, &(eligible & Domain::field("id")?.in_(configured)))
                .await?
                .ids();
            return selected
                .filter(|id| permitted.contains(id))
                .or_else(|| preferred.filter(|id| permitted.contains(id)))
                .or_else(|| permitted.first().copied())
                .ok_or_else(|| Error::user("Configure an active pipeline for this sales team."));
        }
    }
    if let Some(id) = selected
        && first(tx, &set, &(eligible & Domain::field("id")?.eq(id)))
            .await?
            .is_some()
    {
        return Ok(id);
    }
    if let Some(id) = first(tx, &set, &(active.clone() & own)).await? {
        return Ok(id);
    }
    first(tx, &set, &(active & shared))
        .await?
        .ok_or_else(|| Error::user("Configure an active sales pipeline first."))
}

pub(crate) async fn first_stage(env: &Env, tx: &mut Transaction, pipeline: i64) -> Result<i64> {
    first(
        tx,
        &env.records("crm.stage")?,
        &Domain::condition("pipeline_id", Operator::Eq, pipeline)?,
    )
    .await?
    .ok_or_else(|| Error::user("Add a stage to the selected pipeline first."))
}
