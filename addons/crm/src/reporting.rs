// SPDX-License-Identifier: MIT OR Apache-2.0
//! Deal valuation, monthly quotas and the underlying records for native CRM reports.
use erp_sdk::Operation;
use erp_sdk::prelude::*;

#[model(name = "crm.quota", description = "Monthly Sales Quota", order = "month DESC, team_id, user_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        ("_unique_target", "UNIQUE(company_id, user_id, month)", "A salesperson has one quota per company and month."),
        ("_target_amount", "CHECK(amount >= 0)", "A quota must be zero or positive."),
        ("_month_start", "CHECK(EXTRACT(DAY FROM month) = 1)", "Use the first day of the quota month.")
    ])]
pub struct Quota {
    #[field(char, related = "user_id.name", string = "Salesperson")]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.team",
        required = true,
        index = true,
        ondelete = "restrict"
    )]
    pub team_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.users",
        required = true,
        index = true,
        ondelete = "restrict"
    )]
    pub user_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "crm.team.member",
        ondelete = "set null",
        copy = false,
        string = "Salesperson"
    )]
    pub salesperson_member_id: Option<RecId>,
    #[field(date, required = true, index = true, string = "Month Starting")]
    pub month: Option<Date>,
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true
    )]
    pub currency_id: Option<RecId>,
    #[field(
        monetary,
        required = true,
        currency_field = "currency_id",
        string = "Sales Target"
    )]
    pub amount: Decimal,
    #[field(
        monetary,
        compute = "_compute_attainment",
        currency_field = "currency_id",
        string = "Achieved"
    )]
    pub achieved_amount: Decimal,
    #[field(
        monetary,
        compute = "_compute_attainment",
        currency_field = "currency_id",
        string = "Remaining to Target"
    )]
    pub remaining_amount: Decimal,
}

#[methods(Quota, addon = "crm")]
impl Quota {
    #[onchange("team_id")]
    async fn _onchange_quota_team(
        this: &RecordSet<Quota>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let member = this
            .as_records()
            .related(tx, "salesperson_member_id")
            .await?;
        if !member.is_empty()
            && member.related(tx, "crm_team_id").await?.rec_ids()
                != this.as_records().related(tx, "team_id").await?.rec_ids()
        {
            return Ok(OnchangeResult::none().value("salesperson_member_id", Value::False));
        }
        Ok(OnchangeResult::none())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<Quota>,
        tx: &mut Transaction,
        mut rows: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let model = this.as_records();
        model.check_access(tx, Operation::Create).await?;
        let keys = ["company_id", "team_id", "user_id", "salesperson_member_id"];
        let defaults = model.vals_from_values(&model.default_get(tx, &keys).await?)?;
        for vals in &mut rows {
            for key in keys {
                if !vals.contains(key)
                    && !(key == "user_id" && vals.contains("salesperson_member_id")
                        || key == "salesperson_member_id" && vals.contains("user_id"))
                    && let Some(cell) = defaults.get(key)
                {
                    vals.insert(key.into(), cell.clone());
                }
            }
            crate::routing::apply_member(this.env(), tx, None, vals).await?;
        }
        sup.call(tx, model, (rows,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<Quota>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let rows = this.as_records();
        if !["salesperson_member_id", "team_id", "user_id"]
            .iter()
            .any(|key| vals.contains(key))
            || rows.is_empty()
        {
            return sup.call(tx, rows, (vals,)).await;
        }
        tx.savepoint(async move |tx| {
            rows.check_access(tx, Operation::Write).await?;
            rows.lock_for_update(tx, true).await?;
            if rows.len() > 1 {
                for row in rows.iter() {
                    row.write(tx, vals.clone()).await?;
                }
                return Ok(());
            }
            let mut changed = vals;
            crate::routing::apply_member(this.env(), tx, Some(rows), &mut changed).await?;
            sup.call(tx, rows, (changed,)).await
        })
        .await
    }

    #[constrains("team_id", "user_id", "company_id")]
    async fn _check_quota_team(this: &RecordSet<Quota>, tx: &mut Transaction) -> Result<()> {
        for quota in this.as_records().iter() {
            let company = required_id(&quota.cell(tx, "company_id").await?, "Quota company")?;
            let team = quota.related(tx, "team_id").await?;
            crate::pipeline::assert_company(tx, &team, company.get()).await?;
            let user = required_id(&quota.cell(tx, "user_id").await?, "Quota salesperson")?;
            if !crate::routing::user_in_company(this.env(), tx, user.get(), company.get()).await? {
                return Err(Error::validation(
                    "The salesperson must have access to the quota's company.",
                ));
            }
        }
        Ok(())
    }
}

fn required_id(cell: &Cell, label: &str) -> Result<RecId> {
    if let Cell::Id(n) = cell
        && let Some(id) = RecId::from_real(*n)
    {
        return Ok(id);
    }
    Err(Error::validation(format!("{label} is required.")))
}

async fn effective(
    tx: &mut Transaction,
    record: Option<&Records>,
    vals: &RawVals,
    key: &str,
) -> Result<Cell> {
    if let Some(value) = vals.get(key) {
        return Ok(value.clone());
    }
    if let Some(record) = record {
        return record.cell(tx, key).await;
    }
    Ok(Cell::Null)
}

async fn convert(
    env: &Env,
    tx: &mut Transaction,
    company: RecId,
    from: RecId,
    to: RecId,
    amount: Decimal,
) -> Result<(Decimal, String)> {
    env.records("res.currency")?
        .browse_ids([from])
        .call(
            tx,
            "_convert_with_factor",
            (to, company, env.context_today()?, amount),
        )
        .await
}

fn decimal(value: &Cell) -> Result<Decimal> {
    match value {
        Cell::Decimal(v) if *v >= Decimal::ZERO => Ok(*v),
        Cell::Null => Ok(Decimal::ZERO),
        _ => Err(Error::validation("Enter a non-negative deal amount.")),
    }
}

pub(crate) async fn prepare_amounts(
    tx: &mut Transaction,
    env: &Env,
    record: Option<&Records>,
    vals: &mut RawVals,
) -> Result<()> {
    if record.is_some()
        && !["deal_currency_id", "deal_amount", "expected_revenue"]
            .iter()
            .any(|key| vals.contains(key))
    {
        return Ok(());
    }
    let company = required_id(
        &effective(tx, record, vals, "company_id").await?,
        "Deal company",
    )?;
    let currency = required_id(
        &env.records("res.company")?
            .browse_ids([company])
            .cell(tx, "currency_id")
            .await?,
        "Company currency",
    )?;
    let original = match effective(tx, record, vals, "deal_currency_id").await? {
        Cell::Null => currency,
        other => required_id(&other, "Deal currency")?,
    };
    let mut desired = vals.get("expected_revenue").map(decimal).transpose()?;
    let mut amount = match (vals.contains("deal_amount"), desired) {
        (false, Some(value)) if original == currency => value,
        (false, Some(_)) => {
            return Err(Error::validation(
                "Enter the original deal amount when using a foreign currency.",
            ));
        }
        _ => decimal(&effective(tx, record, vals, "deal_amount").await?)?,
    };
    // Convert exactly the source amount that the monetary field will persist.
    // A second conversion at the same rate on win must reproduce the open forecast.
    if amount != Decimal::ZERO || desired.is_some_and(|n| n != Decimal::ZERO) {
        let mut inputs = vec![(original, amount)];
        inputs.extend(desired.map(|n| (currency, n)));
        let rounded: Vec<Decimal> = env
            .records("res.currency")?
            .call(tx, "_round_monetary", (inputs,))
            .await?;
        match rounded.as_slice() {
            [source] if desired.is_none() => amount = *source,
            [source, expected] if desired.is_some() => {
                amount = *source;
                desired = Some(*expected);
            }
            _ => {
                return Err(Error::invariant(
                    "Currency rounding returned an unexpected number of amounts.",
                ));
            }
        }
    }
    let revenue = if original == currency {
        amount
    } else {
        convert(env, tx, company, original, currency, amount)
            .await?
            .0
    };
    if desired.is_some_and(|value| value != revenue) {
        return Err(Error::validation(
            "Deal amount and expected revenue disagree at the current exchange rate.",
        ));
    }
    vals.insert("deal_currency_id".into(), Cell::Id(original.get()));
    vals.insert("deal_amount".into(), Cell::Decimal(amount));
    vals.insert("expected_revenue".into(), Cell::Decimal(revenue));
    Ok(())
}

pub(crate) async fn snapshot_rate(
    tx: &mut Transaction,
    record: &Records,
    vals: &mut RawVals,
) -> Result<()> {
    let company = required_id(&record.cell(tx, "company_id").await?, "Deal company")?;
    let from = required_id(&record.cell(tx, "deal_currency_id").await?, "Deal currency")?;
    let to = required_id(&record.cell(tx, "currency_id").await?, "Company currency")?;
    let original = decimal(&record.cell(tx, "deal_amount").await?)?;
    let (amount, rate) = convert(record.env(), tx, company, from, to, original).await?;
    vals.insert("won_amount".into(), Cell::Decimal(amount));
    vals.insert("won_rate".into(), Cell::Str(rate.into()));
    Ok(())
}

#[model(name = "crm.deal.change", description = "Opportunity Forecast Change", order = "id DESC",
    company_access = parent(edge = "lead_id"))]
pub struct DealChange {
    #[field(char, related = "lead_id.name")]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "crm.lead",
        required = true,
        index = true,
        ondelete = "cascade"
    )]
    pub lead_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.currency",
        required = true,
        ondelete = "restrict"
    )]
    pub currency_id: Option<RecId>,
    #[field(monetary, currency_field = "currency_id")]
    pub previous_amount: Decimal,
    #[field(monetary, currency_field = "currency_id")]
    pub new_amount: Decimal,
    #[field(date)]
    pub previous_close: Option<Date>,
    #[field(date)]
    pub new_close: Option<Date>,
    #[field(
        many2one,
        comodel = "res.users",
        required = true,
        ondelete = "restrict"
    )]
    pub changed_by: Option<RecId>,
}

pub(crate) async fn track_changes(
    tx: &mut Transaction,
    record: &Records,
    vals: &RawVals,
) -> Result<()> {
    if !vals.contains("date_deadline") && !vals.contains("expected_revenue") {
        return Ok(());
    }
    if record.cell(tx, "kind").await? != Cell::Str("opportunity".into()) {
        return Ok(());
    }
    let before_amount = record.cell(tx, "expected_revenue").await?;
    let before_close = record.cell(tx, "date_deadline").await?;
    let after_amount = vals.get("expected_revenue").unwrap_or(&before_amount);
    let after_close = vals.get("date_deadline").unwrap_or(&before_close);
    if before_amount == *after_amount && before_close == *after_close {
        return Ok(());
    }
    record.check_access(tx, Operation::Write).await?;
    let data = RawVals::new()
        .set_cell("lead_id", Cell::Id(record.ensure_one()?.ids()[0]))
        .set_cell("changed_by", Cell::Id(record.env().uid()))
        .set_cell("currency_id", record.cell(tx, "currency_id").await?)
        .set_cell("previous_amount", before_amount.clone())
        .set_cell("new_amount", after_amount.clone())
        .set_cell("previous_close", before_close.clone())
        .set_cell("new_close", after_close.clone());
    crate::lifecycle::clean_env(record.env())
        .sudo(true)
        .records("crm.deal.change")?
        .create(tx, vec![data])
        .await?;
    Ok(())
}
