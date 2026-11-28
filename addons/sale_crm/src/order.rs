// SPDX-License-Identifier: MIT OR Apache-2.0
use std::collections::BTreeMap;

use crate::{SaleOrder, open_record};
use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::{fence_linked_opportunities, optional_id, required_id};

#[extend(SaleOrder, addon = "sale_crm")]
pub struct SalesOpportunity {
    #[field(
        many2one,
        comodel = "crm.lead",
        ondelete = "restrict",
        index = "btree_not_null",
        copy = false,
        string = "Opportunity"
    )]
    pub opportunity_id: Option<RecId>,
    #[field(
        integer,
        compute = "_compute_readable_opportunity",
        string = "Opportunity"
    )]
    pub opportunity_count: i32,
}

#[methods(SaleOrder, addon = "sale_crm")]
impl SalesOpportunity {
    #[compute(depends = ["opportunity_id", "opportunity_id.user_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_readable_opportunity(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        let links = this.mapped(tx, Self::OPPORTUNITY_ID).await?;
        let visible = this
            .env()
            .records("crm.lead")?
            .browse_ids(links.iter().flatten().copied())
            .filtered_access(tx, Operation::Read)
            .await?;
        let ids: std::collections::BTreeSet<_> = visible.rec_ids().iter().copied().collect();
        for (i, id) in links.into_iter().enumerate() {
            out.set(
                i,
                Self::OPPORTUNITY_COUNT,
                i32::from(id.is_some_and(|id| ids.contains(&id))),
            )?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_view_opportunity(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let order = this.as_records();
        order
            .ensure_one()?
            .check_access(tx, Operation::Read)
            .await?;
        let lead = order.related(tx, "opportunity_id").await?;
        lead.check_access(tx, Operation::Read).await?;
        open_record("Opportunity", &lead)
    }

    #[constrains("opportunity_id", "company_id", "partner_id")]
    async fn _check_opportunity_link(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for order in this.as_records().iter() {
            let lead = order.related(tx, "opportunity_id").await?;
            if lead.is_empty() {
                continue;
            }
            fence_linked_opportunities(tx, &lead).await?;
            if lead.cell(tx, "kind").await? != Cell::Str("opportunity".into())
                || lead.cell(tx, "state").await? == Cell::Str("lost".into())
            {
                return Err(Error::validation(
                    "Only an open or won opportunity can be linked to a quotation.",
                ));
            }
            for field in ["partner_id", "company_id"] {
                if optional_id(tx, &lead, field).await?
                    != Some(required_id(tx, &order, field).await?)
                {
                    return Err(Error::validation(
                        "The quotation must use the opportunity's customer and company.",
                    ));
                }
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        values: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for row in &values {
            crate::booking::refuse_override(row, this.env().context())?;
        }
        sup.call(tx, this.as_records(), (values,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let orders = this.as_records();
        crate::booking::refuse_override(&vals, this.env().context())?;
        if vals.contains("opportunity_id") {
            orders.check_access(tx, Operation::Write).await?;
            orders.lock_for_update(tx, true).await?;
            for order in orders.iter() {
                if !matches!(order.cell(tx, "state").await?, Cell::Str(state) if matches!(state.as_ref(), "draft" | "sent"))
                    || order.cell(tx, "locked").await? == Cell::Bool(true)
                {
                    return Err(Error::user(
                        "Only editable quotations can change their linked opportunity.",
                    ));
                }
                let previous = order.related(tx, "opportunity_id").await?;
                if !previous.is_empty() {
                    fence_linked_opportunities(tx, &previous).await?;
                }
            }
        }
        sup.call(tx, orders, (vals,)).await
    }

    #[method(override)]
    async fn _action_confirm(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let orders = this.as_records();
        let (): () = sup.call(tx, orders, ()).await?;
        let links = confirmed_links(tx, orders).await?;
        if links.is_empty() {
            return Ok(());
        }
        let leads = orders
            .env()
            .records("crm.lead")?
            .browse(links.keys().copied())?;
        fence_linked_opportunities(tx, &leads).await?;
        for (id, (reference, salesperson)) in links {
            let (): () = leads
                .browse([id])?
                .call(tx, "_win_from_order", (reference, salesperson))
                .await?;
        }
        Ok(())
    }

    #[method(override)]
    async fn _action_cancel(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let orders = this.as_records();
        tx.savepoint(async move |tx| {
            let links = confirmed_links(tx, orders).await?;
            if links.is_empty() {
                return sup.call(tx, orders, ()).await;
            }
            let leads = orders
                .env()
                .records("crm.lead")?
                .browse(links.keys().copied())?;
            fence_linked_opportunities(tx, &leads).await?;
            let (): () = sup.call(tx, orders, ()).await?;
            schedule_reviews(tx, &leads).await
        })
        .await
    }
}

/// Called after confirmation and before cancellation: ignore draft-only cancellations and
/// choose one order reference deterministically for each opportunity in a batch.
async fn confirmed_links(
    tx: &mut Transaction,
    orders: &Records,
) -> Result<BTreeMap<i64, (String, Option<i64>)>> {
    orders.check_access(tx, Operation::Write).await?;
    tx.flush().await?;
    tx.query(
        "SELECT DISTINCT ON (opportunity_id) opportunity_id::int8, name, user_id::int8 FROM sale_order
              WHERE id = ANY($1) AND opportunity_id IS NOT NULL AND state = 'sale'
              ORDER BY opportunity_id, user_id IS NULL, id",
        &[&orders.ids()],
    )
    .await?
    .iter()
    .map(|row| {
        Ok((
            row.try_get(0).map_err(decode)?,
            (row.try_get(1).map_err(decode)?, row.try_get(2).map_err(decode)?),
        ))
    })
    .collect()
}

fn decode(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Reading linked opportunity bookings failed.").with_source(error)
}

async fn schedule_reviews(tx: &mut Transaction, leads: &Records) -> Result<()> {
    tx.flush().await?;
    let rows = tx.query("SELECT res_id::int8 FROM ir_model_data
            WHERE module = 'sale_crm' AND name = 'activity_review_cancelled_booking' AND model = 'mail.activity.type'", &[]).await?;
    let review_type: i64 = rows
        .first()
        .ok_or_else(|| Error::invariant("The cancelled booking review activity type is missing."))?
        .try_get(0)
        .map_err(decode)?;
    // Count all linked bookings, independent of individual order visibility. Only already
    // authorized opportunities can produce a task. Serialize multiple cancellations on the
    // lead and keep a single pending review when a deal changes hands.
    let due = tx.query("SELECT l.id::int8, COALESCE(l.user_id,l.won_user_id,t.user_id)::int8, a.id::int8, a.user_id::int8
            FROM crm_lead l LEFT JOIN crm_team t ON t.id=l.team_id
            LEFT JOIN LATERAL (
                SELECT a.id,a.user_id FROM mail_activity a WHERE a.res_model='crm.lead'
                AND a.res_id=l.id AND a.activity_type_id=$2 AND a.active AND a.automated
                ORDER BY a.id LIMIT 1
            ) a ON true
            WHERE l.id=ANY($1) AND NOT EXISTS (
                SELECT 1 FROM sale_order o WHERE o.opportunity_id=l.id AND o.state='sale'
            ) ORDER BY l.id", &[&leads.ids(), &review_type]).await?;
    let env = leads.env().without_default_overrides();
    let activity_model = env.records("mail.activity")?;
    let mut activities = Vec::with_capacity(due.len());
    for row in due {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let owner = row.try_get::<_, Option<i64>>(1).map_err(decode)?
            .ok_or_else(|| Error::user("Assign the opportunity or its sales team a responsible salesperson before cancelling its final booking."))?;
        if let Some(existing) = row.try_get::<_, Option<i64>>(2).map_err(decode)? {
            if row.try_get::<_, Option<i64>>(3).map_err(decode)? != Some(owner) {
                activity_model
                    .browse([existing])?
                    .write(tx, RawVals::new().set_cell("user_id", Cell::Id(owner)))
                    .await?;
            }
            continue;
        }
        activities.push(
            RawVals::new()
                .set_cell("res_model", Cell::Str("crm.lead".into()))
                .set_cell("res_id", Cell::Id(id))
                .set_cell("activity_type_id", Cell::Id(review_type))
                .set_cell("date_deadline", Cell::Date(env.context_today()?))
                .set_cell("user_id", Cell::Id(owner))
                .set_cell("automated", Cell::Bool(true)),
        );
    }
    if !activities.is_empty() {
        activity_model.create(tx, activities).await?;
    }
    Ok(())
}
