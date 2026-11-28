// SPDX-License-Identifier: MIT OR Apache-2.0
//! The same lifecycle guards cover native forms, board moves, imports and JSON-2.
use crate::{CrmLead, history, pipeline, reporting, routing};
use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};

const MANAGED: &[&str] = &[
    "state",
    "originated_as_lead",
    "stage_since",
    "closed_date",
    "close_reason",
    "won_amount",
    "won_currency_id",
    "won_user_id",
    "won_team_id",
    "won_rate",
    "history_ids",
    "weighted_revenue",
    "next_activity_date",
];

pub(crate) fn clean_env(env: &Env) -> Env {
    let mut context = env.context().clone();
    let keys: Vec<_> = context
        .default_field_names()
        .map(|f| format!("default_{f}"))
        .collect();
    for key in keys {
        context.remove(&key);
    }
    env.with_context_replaced(context)
}

pub(crate) async fn locked(tx: &mut Transaction, records: &Records) -> Result<()> {
    records.ensure_one()?;
    records.check_access(tx, Operation::Write).await?;
    records.lock_for_update(tx, true).await?;
    records.check_access(tx, Operation::Write).await
}

fn refuse_managed(env: &Env, values: &RawVals, creating: bool) -> Result<()> {
    for field in MANAGED {
        if values.contains(field)
            || values.commands(field).is_some()
            || (creating && env.context().default_field_names().any(|f| f == *field))
        {
            return Err(Error::validation(format!(
                "CRM maintains {field} through its lifecycle actions."
            )));
        }
    }
    if values.get("forecast_category") == Some(&Cell::Str("closed_won".into()))
        || (creating
            && env.context().get("default_forecast_category") == Some(Value::str("closed_won")))
    {
        return Err(Error::validation(
            "Use Win to record a closed-won opportunity.",
        ));
    }
    Ok(())
}

fn context_id(env: &Env, field: &str) -> Option<i64> {
    match env.context().get(&format!("default_{field}")) {
        Some(Value::Int(id)) => Some(id),
        _ => None,
    }
}

fn selected_id(vals: &RawVals, env: &Env, field: &str) -> Option<i64> {
    match vals.get(field) {
        Some(Cell::Id(id)) => Some(*id),
        Some(_) => None,
        None => context_id(env, field),
    }
}

fn as_id(cell: Cell, field: &str) -> Result<i64> {
    if let Cell::Id(id) = cell {
        Ok(id)
    } else {
        Err(Error::validation(format!("Set {field} before continuing.")))
    }
}

fn reason(value: &str) -> Result<&str> {
    let reason = value.trim();
    if reason.is_empty() || reason.chars().count() > 500 {
        return Err(Error::validation("Enter a reason of 1 to 500 characters."));
    }
    Ok(reason)
}

async fn assign_stage(stage: &Records, tx: &mut Transaction, values: &mut RawVals) -> Result<()> {
    stage.check_access(tx, Operation::Read).await?;
    for name in ["probability", "forecast_category"] {
        if !values.contains(name) {
            values.insert(name.into(), stage.cell(tx, name).await?);
        }
    }
    Ok(())
}

async fn membership(tx: &mut Transaction, env: &Env, team_id: i64, user: i64) -> Result<bool> {
    let team = env.records("crm.team")?.browse([team_id])?;
    if team.cell(tx, "user_id").await? == Cell::Id(user) {
        return Ok(true);
    }
    Ok(pipeline::first(
        tx,
        &env.records("crm.team.member")?,
        &(Domain::condition("crm_team_id", Operator::Eq, team_id)?
            & Domain::condition("user_id", Operator::Eq, user)?
            & Domain::condition("active", Operator::Eq, true)?),
    )
    .await?
    .is_some())
}

pub(crate) async fn validate_lead(tx: &mut Transaction, lead: &Records) -> Result<()> {
    validate_lead_as(tx, lead, lead.env()).await
}

async fn validate_lead_as(tx: &mut Transaction, lead: &Records, env: &Env) -> Result<()> {
    let company = as_id(lead.cell(tx, "company_id").await?, "company")?;
    let pipeline_id = as_id(lead.cell(tx, "pipeline_id").await?, "pipeline")?;
    let pipe = env.records("crm.pipeline")?.browse([pipeline_id])?;
    pipeline::assert_company(tx, &pipe, company).await?;
    if pipe.cell(tx, "active").await? != Cell::Bool(true) {
        return Err(Error::validation("Choose an active pipeline."));
    }
    let stage = env
        .records("crm.stage")?
        .browse([as_id(lead.cell(tx, "stage_id").await?, "stage")?])?;
    stage.check_access(tx, Operation::Read).await?;
    if stage.cell(tx, "pipeline_id").await? != Cell::Id(pipeline_id) {
        return Err(Error::validation(
            "The stage belongs to a different pipeline.",
        ));
    }
    if let Cell::Id(tid) = lead.cell(tx, "team_id").await? {
        let team = env
            .records("crm.team")?
            .browse([tid])?
            .with_context([("active_test", Value::Bool(false))]);
        pipeline::assert_company(tx, &team, company).await?;
        let default = team.cell(tx, "crm_pipeline_id").await?;
        let Cell::Ids(additional) = team.cell(tx, "crm_pipeline_ids").await? else {
            return Err(Error::invariant(
                "The sales team's pipelines must be a list.",
            ));
        };
        if (!default.is_null() || !additional.is_empty())
            && default != Cell::Id(pipeline_id)
            && !additional.iter().any(|p| p.get() == pipeline_id)
        {
            return Err(Error::validation(
                "This sales team does not use the selected pipeline.",
            ));
        }
    }
    if let Cell::Id(id) = lead.cell(tx, "partner_id").await? {
        pipeline::assert_company(tx, &env.records("res.partner")?.browse([id])?, company).await?;
    }
    if let Cell::Id(user) = lead.cell(tx, "user_id").await?
        && !routing::user_in_company(env, tx, user, company).await?
    {
        return Err(Error::validation(
            "The salesperson does not work for this company.",
        ));
    }
    if lead.cell(tx, "kind").await? == Cell::Str("opportunity".into()) {
        for (required, field, label) in [
            ("require_partner", "partner_id", "customer"),
            ("require_close_date", "date_deadline", "expected close date"),
            (
                "require_value",
                "expected_revenue",
                "positive expected revenue",
            ),
            ("require_next_step", "next_step", "next step"),
        ] {
            if stage.cell(tx, required).await? != Cell::Bool(true) {
                continue;
            }
            let value = lead.cell(tx, field).await?;
            if !present_for_stage(field, &value)? {
                return Err(Error::validation(format!("This stage requires a {label}.")));
            }
        }
    }
    Ok(())
}

fn present_for_stage(field: &str, cell: &Cell) -> Result<bool> {
    match (field, cell) {
        (_, Cell::Null) => Ok(false),
        ("partner_id", Cell::Id(id)) => Ok(*id > 0),
        ("date_deadline", Cell::Date(_)) => Ok(true),
        ("expected_revenue", Cell::Decimal(amount)) => Ok(*amount > Decimal::ZERO),
        ("next_step", Cell::Str(text)) => Ok(!text.trim().is_empty()),
        _ => Err(Error::invariant(format!(
            "Unexpected value type for CRM stage requirement {field}."
        ))),
    }
}

#[methods(CrmLead, addon = "crm")]
impl CrmLead {
    #[onchange("salesperson_member_id")]
    async fn _onchange_salesperson_member(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let member = this
            .as_records()
            .related(tx, "salesperson_member_id")
            .await?;
        if member.is_empty() {
            return Ok(OnchangeResult::none());
        }
        member.check_access(tx, Operation::Read).await?;
        let seller = as_id(
            member.cell(tx, "user_id").await?,
            "team member's salesperson",
        )?;
        Ok(OnchangeResult::none().value("user_id", Value::Int(seller)))
    }

    #[onchange("team_id")]
    async fn _onchange_team_pipeline(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let team = this.as_records().related(tx, "team_id").await?;
        let member = this
            .as_records()
            .related(tx, "salesperson_member_id")
            .await?;
        let mut result = OnchangeResult::none();
        if !member.is_empty()
            && member.related(tx, "crm_team_id").await?.rec_ids() != team.rec_ids()
        {
            result = result.value("salesperson_member_id", Value::False);
        }
        if team.is_empty() {
            return Ok(result);
        }
        let selected = this.get(tx, Self::PIPELINE_ID).await?;
        let company = this
            .get(tx, Self::COMPANY_ID)
            .await?
            .map(RecId::get)
            .or(this.env().company_id()?);
        let Some(company) = company else {
            return Ok(result);
        };
        let pipeline = pipeline::default_pipeline(
            this.env(),
            tx,
            company,
            Some(team.int()?),
            selected.map(RecId::get),
        )
        .await?;
        if selected.map(RecId::get) == Some(pipeline) {
            return Ok(result);
        }
        Ok(result.value("pipeline_id", Value::Int(pipeline)))
    }

    #[onchange("pipeline_id")]
    async fn _onchange_pipeline(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let Some(pipe) = this.get(tx, Self::PIPELINE_ID).await? else {
            return Ok(OnchangeResult::none().value("stage_id", Value::False));
        };
        let stage = this.as_records().related(tx, "stage_id").await?;
        if !stage.is_empty() && stage.cell(tx, "pipeline_id").await? == Cell::Id(pipe.get()) {
            return Ok(OnchangeResult::none());
        }
        Ok(OnchangeResult::none().value(
            "stage_id",
            Value::Int(pipeline::first_stage(this.env(), tx, pipe.get()).await?),
        ))
    }

    #[onchange("stage_id")]
    async fn _onchange_stage(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let stage = this.as_records().related(tx, "stage_id").await?;
        if stage.is_empty() {
            return Ok(OnchangeResult::none());
        }
        Ok(OnchangeResult::none()
            .value("probability", stage.value(tx, "probability").await?)
            .value(
                "forecast_category",
                stage.value(tx, "forecast_category").await?,
            ))
    }

    #[method(override)]
    async fn default_get(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut vals: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        let wants = |field: &str| fields.iter().any(|f| f == field);
        let env = this.env();
        if wants("user_id")
            && env.context().get("crm_personal_form") == Some(Value::Bool(true))
            && !env.su()
            && !tx
                .has_group(env, "sales_team.group_sale_salesman_all_leads")
                .await?
        {
            vals.insert("user_id".into(), Value::Int(env.uid()));
        }
        let Some(company) = context_id(env, "company_id").or(env.company_id()?) else {
            return Ok(vals);
        };
        if wants("company_id") {
            vals.entry("company_id".into())
                .or_insert(Value::Int(company));
        }
        if wants("deal_currency_id") && !vals.get("deal_currency_id").is_some_and(Value::is_truthy)
        {
            let currency = as_id(
                env.records("res.company")?
                    .browse([company])?
                    .cell(tx, "currency_id")
                    .await?,
                "company currency",
            )?;
            vals.insert("deal_currency_id".into(), Value::Int(currency));
        }
        let stage_defaults = wants("probability") || wants("forecast_category");
        if !stage_defaults
            && !["team_id", "pipeline_id", "stage_id"]
                .into_iter()
                .any(wants)
        {
            return Ok(vals);
        }
        let team = match context_id(env, "team_id") {
            Some(id) => Some(id),
            None => pipeline::default_team(env, tx, company).await?,
        };
        if wants("team_id")
            && let Some(id) = team
        {
            vals.entry("team_id".into()).or_insert(Value::Int(id));
        }
        if wants("pipeline_id") || wants("stage_id") || stage_defaults {
            let id = match context_id(env, "pipeline_id") {
                Some(id) => id,
                None => pipeline::default_pipeline(env, tx, company, team, None).await?,
            };
            if wants("pipeline_id") {
                vals.entry("pipeline_id".into()).or_insert(Value::Int(id));
            }
            if wants("stage_id") || stage_defaults {
                let stage = match context_id(env, "stage_id") {
                    Some(id) => id,
                    None => pipeline::first_stage(env, tx, id).await?,
                };
                if wants("stage_id") {
                    vals.entry("stage_id".into()).or_insert(Value::Int(stage));
                }
                let config = env.records("crm.stage")?.browse([stage])?;
                for key in ["probability", "forecast_category"] {
                    if wants(key) && env.context().get(&format!("default_{key}")).is_none() {
                        vals.insert(key.into(), config.value(tx, key).await?);
                    }
                }
            }
        }
        Ok(vals)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env();
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        tx.savepoint(async move |tx| {
            let now = DateTime::now();
            let stage_defaults: ValueMap = ["probability", "forecast_category"]
                .into_iter()
                .filter_map(|field| {
                    env.context()
                        .get(&format!("default_{field}"))
                        .map(|v| (field.to_owned(), v))
                })
                .collect();
            let stage_defaults = this.as_records().vals_from_values(&stage_defaults)?;
            routing::apply_intake_defaults(this.as_records(), tx, &mut vals_list).await?;
            for vals in &mut vals_list {
                refuse_managed(env, vals, true)?;
                let company = selected_id(vals, env, "company_id")
                    .or(env.company_id()?)
                    .ok_or_else(|| Error::validation("Select a company before creating a lead."))?;
                vals.insert("company_id".into(), Cell::Id(company));
                for name in ["team_id", "user_id", "salesperson_member_id"] {
                    if !vals.contains(name)
                        && let Some(id) = context_id(env, name)
                    {
                        vals.insert(name.into(), Cell::Id(id));
                    }
                }
                routing::apply_member(env, tx, None, vals).await?;
                let direct = match vals.get("kind") {
                    Some(Cell::Str(v)) => v.as_ref() == "opportunity",
                    None => env.context().get("default_kind") == Some(Value::str("opportunity")),
                    _ => false,
                };
                if !vals.contains("kind") {
                    vals.insert(
                        "kind".into(),
                        Cell::Str(if direct { "opportunity" } else { "lead" }.into()),
                    );
                }
                if let Some(Cell::Id(user)) = vals.get("user_id")
                    && *user != env.uid()
                    && !env.su()
                    && !tx
                        .has_group(env, "sales_team.group_sale_salesman_all_leads")
                        .await?
                {
                    return Err(Error::access(
                        "Ask a sales manager to assign a lead to another salesperson.",
                    ));
                }
                if !direct {
                    routing::route_new(env, tx, vals).await?;
                }
                // A lead without an assignee stays in its queue. The base ORM's second
                // default_get must not replace the intake decision with a form default.
                if !vals.contains("user_id") {
                    vals.insert("user_id".into(), Cell::Null);
                }
                if !vals.contains("team_id")
                    && let Some(team) = pipeline::default_team(env, tx, company).await?
                {
                    vals.insert("team_id".into(), Cell::Id(team));
                }
                let team = selected_id(vals, env, "team_id");
                let pipe = match selected_id(vals, env, "pipeline_id") {
                    Some(id) => id,
                    None => pipeline::default_pipeline(env, tx, company, team, None).await?,
                };
                vals.insert("pipeline_id".into(), Cell::Id(pipe));
                let stage = match selected_id(vals, env, "stage_id") {
                    Some(id) => id,
                    None => pipeline::first_stage(env, tx, pipe).await?,
                };
                vals.insert("stage_id".into(), Cell::Id(stage));
                vals.insert("stage_since".into(), Cell::DateTime(now));
                vals.insert("originated_as_lead".into(), Cell::Bool(!direct));
                for (field, value) in stage_defaults.iter() {
                    if !vals.contains(field) {
                        vals.insert(field.to_owned().into(), value.clone());
                    }
                }
                assign_stage(&env.records("crm.stage")?.browse([stage])?, tx, vals).await?;
                reporting::prepare_amounts(tx, env, None, vals).await?;
            }
            let rows: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
            for lead in rows.iter() {
                // Own-document users can submit a lead routed to a colleague. Only extract
                // the just-created CRM row with elevation; linked records use the caller's rights.
                let created = lead.sudo(true);
                validate_lead_as(tx, &created, env).await?;
                let stage = as_id(created.cell(tx, "stage_id").await?, "stage")?;
                history::open(tx, &lead, stage, now).await?;
            }
            Ok(rows)
        })
        .await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let leads = this.as_records();
        refuse_managed(this.env(), &vals, false)?;
        if leads.is_empty() || vals.is_empty() {
            return sup.call(tx, leads, (vals,)).await;
        }
        tx.savepoint(async move |tx| {
            if leads.len() > 1 {
                leads.check_access(tx, Operation::Write).await?;
                leads.lock_for_update(tx, true).await?;
                for lead in leads.iter() {
                    lead.write(tx, vals.clone()).await?;
                }
                return Ok(());
            }
            locked(tx, leads).await?;
            for field in ["kind", "company_id"] {
                if let Some(value) = vals.remove(field)
                    && value != leads.cell(tx, field).await?
                {
                    return Err(Error::validation(
                        "Company and record type are fixed. Use Qualify to convert a lead.",
                    ));
                }
            }
            if vals.is_empty() {
                return Ok(());
            }
            if leads.cell(tx, "state").await? != Cell::Str("open".into()) {
                return Err(Error::user(
                    "Reopen the opportunity before changing its details.",
                ));
            }
            routing::apply_member(this.env(), tx, Some(leads), &mut vals).await?;
            if let Some(owner) = vals.get("user_id")
                && !this.env().su()
                && !tx
                    .has_group(this.env(), "sales_team.group_sale_salesman_all_leads")
                    .await?
            {
                if owner != &Cell::Id(this.env().uid()) {
                    return Err(Error::access("Ask a sales manager to reassign the lead."));
                }
                if let Cell::Id(team) = leads.cell(tx, "team_id").await?
                    && !membership(tx, this.env(), team, this.env().uid()).await?
                {
                    return Err(Error::access(
                        "Only a member of the sales team can claim the lead.",
                    ));
                }
            }
            if vals.contains("pipeline_id")
                && !vals.contains("stage_id")
                && let Some(Cell::Id(pipe)) = vals.get("pipeline_id")
                && leads.cell(tx, "pipeline_id").await? != Cell::Id(*pipe)
            {
                vals.insert(
                    "stage_id".into(),
                    Cell::Id(pipeline::first_stage(this.env(), tx, *pipe).await?),
                );
            }
            let stage = match vals.get("stage_id") {
                Some(Cell::Id(id)) if leads.cell(tx, "stage_id").await? != Cell::Id(*id) => {
                    Some(*id)
                }
                _ => None,
            };
            let now = DateTime::now();
            if let Some(id) = stage {
                assign_stage(
                    &this.env().records("crm.stage")?.browse([id])?,
                    tx,
                    &mut vals,
                )
                .await?;
                vals.insert("stage_since".into(), Cell::DateTime(now));
            }
            reporting::prepare_amounts(tx, this.env(), Some(leads), &mut vals).await?;
            reporting::track_changes(tx, leads, &vals).await?;
            sup.call(tx, leads, (vals,)).await?;
            validate_lead(tx, leads).await?;
            if let Some(id) = stage {
                history::close(tx, leads, now, None).await?;
                history::open(tx, leads, id, now).await?;
            }
            Ok(())
        })
        .await
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_claim(this: &RecordSet<CrmLead>, tx: &mut Transaction) -> Result<()> {
        let lead = this.as_records();
        locked(tx, lead).await?;
        if lead.cell(tx, "user_id").await? == Cell::Id(this.env().uid()) {
            return Ok(());
        }
        if !lead.cell(tx, "user_id").await?.is_null() {
            return Err(Error::user(
                "Another salesperson has already claimed this lead.",
            ));
        }
        if let Cell::Id(tid) = lead.cell(tx, "team_id").await?
            && !this.env().su()
            && !tx
                .has_group(this.env(), "sales_team.group_sale_manager")
                .await?
            && !membership(tx, this.env(), tid, this.env().uid()).await?
        {
            return Err(Error::access(
                "Only a member of this sales team can claim the lead.",
            ));
        }
        lead.write(
            tx,
            RawVals::new().set_cell("user_id", Cell::Id(this.env().uid())),
        )
        .await
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_win(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        prompt_value: String,
    ) -> Result<()> {
        close(tx, this.as_records(), Outcome::Won, reason(&prompt_value)?).await
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_lose(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        prompt_value: String,
    ) -> Result<()> {
        close(tx, this.as_records(), Outcome::Lost, reason(&prompt_value)?).await
    }

    #[method]
    async fn _win_from_order(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        order_name: String,
        order_user_id: Option<i64>,
    ) -> Result<()> {
        close(
            tx,
            this.as_records(),
            Outcome::Order(order_user_id),
            &format!("Sales order {order_name} confirmed."),
        )
        .await
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_reopen(this: &RecordSet<CrmLead>, tx: &mut Transaction) -> Result<()> {
        let lead = this.as_records();
        tx.savepoint(async |tx| {
            locked(tx, lead).await?;
            let state = lead.cell(tx, "state").await?;
            if state == Cell::Str("open".into()) {
                return Ok(());
            }
            let now = DateTime::now();
            let mut vals = RawVals::new()
                .set_cell("state", Cell::Str("open".into()))
                .set_cell("stage_since", Cell::DateTime(now))
                .set_cell("won_amount", Cell::Decimal(Decimal::ZERO));
            for field in [
                "closed_date",
                "close_reason",
                "won_currency_id",
                "won_user_id",
                "won_team_id",
                "won_rate",
            ] {
                vals.insert(field.into(), Cell::Null);
            }
            let stage = lead.related(tx, "stage_id").await?;
            assign_stage(&stage, tx, &mut vals).await?;
            lead.write_base(tx, vals).await?;
            history::open(tx, lead, stage.int()?, now).await?;
            if state == Cell::Str("won".into()) {
                invalidate_quota_totals(tx).await?;
            }
            Ok(())
        })
        .await
    }
}

enum Outcome {
    Won,
    Lost,
    Order(Option<i64>),
}

async fn invalidate_quota_totals(tx: &mut Transaction) -> Result<()> {
    tx.invalidate_model("crm.quota", Some(&["achieved_amount", "remaining_amount"]))
        .await
}

async fn close(tx: &mut Transaction, lead: &Records, outcome: Outcome, reason: &str) -> Result<()> {
    let won = !matches!(outcome, Outcome::Lost);
    let from_order = matches!(outcome, Outcome::Order(_));
    tx.savepoint(async |tx| {
        locked(tx, lead).await?;
        let target = if won { "won" } else { "lost" };
        let state = lead.cell(tx, "state").await?;
        if state == Cell::Str(target.into()) || (from_order && state == Cell::Str("lost".into())) {
            return Ok(());
        }
        if state != Cell::Str("open".into()) {
            return Err(Error::user("Reopen the deal before changing its outcome."));
        }
        if won
            && (lead.cell(tx, "kind").await? != Cell::Str("opportunity".into())
                || lead.cell(tx, "partner_id").await?.is_null())
        {
            return Err(Error::validation(
                "Qualify the lead and select its customer before recording a win.",
            ));
        }
        let mut vals = RawVals::new()
            .set_cell("state", Cell::Str(target.into()))
            .set_cell("closed_date", Cell::Date(lead.env().context_today()?))
            .set_cell("close_reason", Cell::Str(reason.into()))
            .set_cell(
                "forecast_category",
                Cell::Str(if won { "closed_won" } else { "omitted" }.into()),
            )
            .set_cell("probability", Cell::Int(if won { 100 } else { 0 }));
        if won {
            for (dest, src) in [
                ("won_currency_id", "currency_id"),
                ("won_amount", "expected_revenue"),
                ("won_user_id", "user_id"),
                ("won_team_id", "team_id"),
            ] {
                vals.insert(dest.into(), lead.cell(tx, src).await?);
            }
            if vals.get("won_user_id") == Some(&Cell::Null) {
                let Outcome::Order(Some(seller)) = outcome else {
                    return Err(Error::validation(
                        "Assign a salesperson before recording the win.",
                    ));
                };
                let company = as_id(lead.cell(tx, "company_id").await?, "company")?;
                if !routing::user_in_company(lead.env(), tx, seller, company).await? {
                    return Err(Error::validation(
                        "The order salesperson must belong to the opportunity's company.",
                    ));
                }
                vals.insert("won_user_id".into(), Cell::Id(seller));
                vals.insert("user_id".into(), Cell::Id(seller));
            }
            reporting::snapshot_rate(tx, lead, &mut vals).await?;
        }
        history::close(tx, lead, DateTime::now(), Some(&vals)).await?;
        lead.write_base(tx, vals).await?;
        if won {
            invalidate_quota_totals(tx).await?;
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stage_gates_require_real_values_for_their_specific_fields() -> Result<()> {
        assert!(!present_for_stage("partner_id", &Cell::Null)?);
        assert!(present_for_stage("partner_id", &Cell::Id(5))?);
        assert!(!present_for_stage("next_step", &Cell::Str("   ".into()))?);
        assert!(!present_for_stage(
            "expected_revenue",
            &Cell::Decimal(Decimal::ZERO)
        )?);
        assert!(present_for_stage(
            "expected_revenue",
            &Cell::Decimal(Decimal::ONE)
        )?);
        assert!(present_for_stage(
            "date_deadline",
            &Cell::Date(Date::from_days(1))
        )?);
        assert!(present_for_stage("date_deadline", &Cell::Bool(true)).is_err());
        Ok(())
    }
    #[test]
    fn manual_outcomes_need_a_bounded_reason() {
        assert_eq!(
            reason("  Approved contract  ").ok(),
            Some("Approved contract")
        );
        assert!(reason(" \t ").is_err());
        assert!(reason(&"a".repeat(501)).is_err());
    }
}
