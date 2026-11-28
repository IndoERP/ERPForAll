// SPDX-License-Identifier: MIT OR Apache-2.0
//! Salesperson quota credit is the sum of won deals in the month they closed.
use crate::reporting::Quota;
use erp_sdk::prelude::*;
use erp_sdk::{Operation, serde_json::json};
use std::collections::{BTreeMap, BTreeSet};

fn next_month(start: Date) -> Result<Date> {
    let (y, m, _) = start.ymd();
    let (year, month) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    Date::from_ymd(year, month, 1).ok_or_else(|| Error::validation("Quota month is out of range."))
}

fn credited_deals(
    companies: Vec<i64>,
    owners: Vec<i64>,
    first: Date,
    last: Date,
) -> Result<Domain> {
    Ok(Domain::field("state")?.eq("won")
        & Domain::field("kind")?.eq("opportunity")
        & Domain::field("company_id")?.in_(companies)
        & Domain::field("won_user_id")?.in_(owners)
        & Domain::field("closed_date")?.ge(first.to_string())
        & Domain::field("closed_date")?.lt(next_month(last)?.to_string()))
}

#[methods(Quota, addon = "crm")]
impl Quota {
    #[compute(depends = ["month", "user_id", "company_id", "amount"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_attainment(
        this: &RecordSet<Quota>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Quota>,
    ) -> Result<()> {
        let companies = this.mapped(tx, Quota::COMPANY_ID).await?;
        let owners = this.mapped(tx, Quota::USER_ID).await?;
        let months = this.mapped(tx, Quota::MONTH).await?;
        let targets = this.mapped(tx, Quota::AMOUNT).await?;
        let mut credits = BTreeMap::new();
        if let (Some(first), Some(last)) =
            (months.iter().flatten().min(), months.iter().flatten().max())
        {
            let ids = |values: &[Option<RecId>]| {
                values
                    .iter()
                    .flatten()
                    .map(|id| id.get())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            };
            let domain = credited_deals(ids(&companies), ids(&owners), *first, *last)?;
            let groups = this
                .env()
                .records("crm.lead")?
                ._read_group(
                    tx,
                    ReadGroup::new(domain)
                        .groupby(["company_id", "won_user_id", "closed_date:month"])
                        .aggregates(["won_amount:sum"]),
                )
                .await?;
            for row in 0..groups.len() {
                let (
                    Some(company),
                    Some(owner),
                    GroupValue::Date(month),
                    GroupValue::Decimal(amount),
                ) = (
                    groups.column(0)[row].record_id(),
                    groups.column(1)[row].record_id(),
                    &groups.column(2)[row],
                    &groups.column(3)[row],
                )
                else {
                    return Err(Error::invariant("Invalid monthly won-deal aggregate."));
                };
                credits.insert((company, owner, *month), *amount);
            }
        }
        for (index, target) in targets.into_iter().enumerate() {
            let key = companies[index].zip(owners[index]).zip(months[index]);
            let achieved = key
                .and_then(|((company, owner), month)| credits.get(&(company, owner, month)))
                .copied()
                .unwrap_or(Decimal::ZERO);
            out.set(index, Self::ACHIEVED_AMOUNT, achieved)?;
            out.set(
                index,
                Self::REMAINING_AMOUNT,
                target.checked_sub(achieved).ok_or_else(|| {
                    Error::validation("Quota difference exceeds the supported range.")
                })?,
            )?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_view_credited_deals(
        this: &RecordSet<Quota>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let quota = this.ensure_one()?;
        quota.as_records().check_access(tx, Operation::Read).await?;
        let (Some(company), Some(owner), Some(month)) = (
            quota.get(tx, Quota::COMPANY_ID).await?,
            quota.get(tx, Quota::USER_ID).await?,
            quota.get(tx, Quota::MONTH).await?,
        ) else {
            return Err(Error::validation(
                "Save a salesperson, company and month to review quota credit.",
            ));
        };
        let domain = credited_deals(vec![company.get()], vec![owner.get()], month, month)?;
        Ok(Value::Json(Arc::new(json!({
            "type":"ir.actions.act_window", "res_model":"crm.lead", "name":"Credited Opportunities",
            "views":[[false,"list"],[false,"form"]], "target":"current", "domain":domain.to_json()?
        }))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_include_the_entire_closing_month_across_leap_days_and_year_boundaries() -> Result<()>
    {
        for (start, end) in [("2028-02-01", "2028-03-01"), ("2026-12-01", "2027-01-01")] {
            let month = Date::parse_odoo(start).ok_or_else(|| Error::invariant("test date"))?;
            let domain = credited_deals(vec![2], vec![3], month, month)?.to_json()?;
            let terms = domain
                .as_array()
                .ok_or_else(|| Error::invariant("domain terms"))?;
            for expected in [
                json!(["closed_date", ">=", start]),
                json!(["closed_date", "<", end]),
                json!(["won_user_id", "in", [3]]),
                json!(["company_id", "in", [2]]),
                json!(["state", "=", "won"]),
            ] {
                assert!(terms.contains(&expected), "{domain}");
            }
        }
        Ok(())
    }
}
