// SPDX-License-Identifier: MIT OR Apache-2.0
//! Sales Team Dashboard (`crm.team` extension): live `dashboard_card` v1 JSON showing
//! Quotations, Confirmed Sales Orders, To Invoice, and Target vs Confirmed Sales.

use std::collections::BTreeMap;

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RoundingTable, convert};
use erp_addon_sales_team::CrmTeam;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};

/// Pure exact-decimal achievement percentage against a team's invoicing target.
pub fn compute_target_achievement(confirmed_amount: Decimal, target: Decimal) -> Decimal {
    if target > Decimal::ZERO {
        ((confirmed_amount * Decimal::from(100)) / target).round_dp(1)
    } else {
        Decimal::ZERO
    }
}

/// Extension of `crm.team` providing Sales KPIs and a Kanban dashboard card.
#[extend(CrmTeam, addon = "sale")]
pub struct CrmTeamSaleDashboard {
    /// Contract v1 JSON rendered by `widget="dashboard_card"` on the Sales Teams Kanban board.
    #[field(
        text,
        compute = "_compute_sale_dashboard_card",
        string = "Sales Dashboard"
    )]
    pub sale_dashboard_card: Option<Arc<str>>,
}

fn db(e: impl std::fmt::Display) -> Error {
    Error::db(format!("sales team dashboard: {e}"))
}

#[methods(CrmTeam, addon = "sale")]
impl CrmTeamSaleDashboard {
    #[compute(
        depends = ["name", "invoiced_target", "company_id"],
        depends_context = ["uid", "allowed_company_ids"]
    )]
    async fn _compute_sale_dashboard_card(
        this: &RecordSet<CrmTeam>,
        tx: &mut Transaction,
        out: &mut Assign<'_, CrmTeam>,
    ) -> Result<()> {
        if this.is_empty() {
            return Ok(());
        }
        tx.flush().await?;
        let ids = this.as_records().ids();
        let targets = this.mapped(tx, CrmTeam::INVOICED_TARGET).await?;
        let companies = this.mapped(tx, CrmTeam::COMPANY_ID).await?;
        let env = this.env().clone();

        // Resolve company currency (or current company's currency for shared teams).
        let fallback_company = env.company_id()?;
        let mut currency_cache: BTreeMap<i64, Option<i64>> = BTreeMap::new();
        let mut team_companies: Vec<Option<i64>> = Vec::with_capacity(ids.len());
        let mut team_currencies: Vec<Option<i64>> = Vec::with_capacity(ids.len());
        let mut currency_rec_ids: std::collections::BTreeSet<RecId> =
            std::collections::BTreeSet::new();
        for comp in &companies {
            let cid = comp.map(RecId::get).or(fallback_company);
            team_companies.push(cid);
            let cur = if let Some(cid) = cid {
                if let Some(&cached) = currency_cache.get(&cid) {
                    cached
                } else {
                    let c = match env
                        .sudo(true)
                        .records("res.company")?
                        .browse([cid])?
                        .cell(tx, "currency_id")
                        .await?
                    {
                        Cell::Id(cur_id) => Some(cur_id),
                        Cell::Null => None,
                        other => {
                            return Err(Error::programming(format!(
                                "res.company.currency_id returned {other:?}, expected id or null"
                            )));
                        }
                    };
                    currency_cache.insert(cid, c);
                    c
                }
            } else {
                None
            };
            if let Some(rec) = cur.and_then(RecId::from_real) {
                currency_rec_ids.insert(rec);
            }
            team_currencies.push(cur);
        }
        let cur_list: Vec<RecId> = currency_rec_ids.into_iter().collect();
        let rounding = if cur_list.is_empty() {
            None
        } else {
            Some(RoundingTable::load(&env, tx, &cur_list).await?)
        };

        // Enforce sale.order read access, personal record rules, and allowed_company_ids.
        let visible_orders = env
            .records("sale.order")?
            .search(
                tx,
                &Domain::condition("team_id", Operator::In, ids.clone())?,
            )
            .await?;
        let visible_ids = visible_orders.ids();

        // Aggregate only visible sales orders per team and currency, converting foreign currencies
        // into each team's target currency so mixed currencies are never blindly summed.
        let mut metrics: BTreeMap<i64, (i64, Decimal, i64, Decimal, i64, Decimal)> =
            BTreeMap::new();
        if !visible_ids.is_empty() {
            let rows = tx
                .query(
                    "SELECT team_id::int8, currency_id::int8, \
                            COUNT(*) FILTER (WHERE state IN ('draft', 'sent'))::int8, \
                            COALESCE(SUM(amount_untaxed) FILTER (WHERE state IN ('draft', 'sent')), 0), \
                            COUNT(*) FILTER (WHERE state = 'sale')::int8, \
                            COALESCE(SUM(amount_untaxed) FILTER (WHERE state = 'sale'), 0), \
                            COUNT(*) FILTER (WHERE state = 'sale' AND invoice_status = 'to invoice')::int8, \
                            COALESCE(SUM(amount_untaxed) FILTER (WHERE state = 'sale' AND invoice_status = 'to invoice'), 0) \
                     FROM sale_order \
                     WHERE id = ANY($1) \
                     GROUP BY team_id, currency_id",
                    &[&visible_ids],
                )
                .await?;
            let team_target: BTreeMap<i64, (Option<i64>, Option<i64>)> = ids
                .iter()
                .zip(&team_companies)
                .zip(&team_currencies)
                .map(|((&tid, &cid), &cur)| (tid, (cid, cur)))
                .collect();
            let today = env.context_today()?;
            for row in rows {
                let tid: i64 = row.try_get(0).map_err(db)?;
                let src_cur: Option<i64> = row.try_get(1).map_err(db)?;
                let q_cnt: i64 = row.try_get(2).map_err(db)?;
                let mut q_amt: Decimal = row.try_get(3).map_err(db)?;
                let s_cnt: i64 = row.try_get(4).map_err(db)?;
                let mut s_amt: Decimal = row.try_get(5).map_err(db)?;
                let inv_cnt: i64 = row.try_get(6).map_err(db)?;
                let mut inv_amt: Decimal = row.try_get(7).map_err(db)?;
                if let Some(&(Some(comp_id), Some(dst_cur))) = team_target.get(&tid)
                    && let Some(sc) = src_cur
                    && sc != dst_cur
                {
                    let from = RecId::from_real(sc)
                        .ok_or_else(|| Error::invariant("Invalid source currency id."))?;
                    let to = RecId::from_real(dst_cur)
                        .ok_or_else(|| Error::invariant("Invalid target currency id."))?;
                    let comp = RecId::from_real(comp_id)
                        .ok_or_else(|| Error::invariant("Invalid company id."))?;
                    if !q_amt.is_zero() {
                        q_amt = convert(&env, tx, q_amt, from, to, comp, today).await?;
                    }
                    if !s_amt.is_zero() {
                        s_amt = convert(&env, tx, s_amt, from, to, comp, today).await?;
                    }
                    if !inv_amt.is_zero() {
                        inv_amt = convert(&env, tx, inv_amt, from, to, comp, today).await?;
                    }
                }
                let entry = metrics.entry(tid).or_insert((
                    0,
                    Decimal::ZERO,
                    0,
                    Decimal::ZERO,
                    0,
                    Decimal::ZERO,
                ));
                entry.0 += q_cnt;
                entry.1 += q_amt;
                entry.2 += s_cnt;
                entry.3 += s_amt;
                entry.4 += inv_cnt;
                entry.5 += inv_amt;
            }
        }

        let fmt_amt = |amt: Decimal, cur: Option<i64>| -> Result<String> {
            if let (Some(table), Some(rec)) = (&rounding, cur.and_then(RecId::from_real)) {
                let inc = table.get(rec)?.increment();
                Ok(inc.quantize(amt, CURRENCY_ROUNDING_MODE)?.to_string())
            } else {
                Ok(amt.round_dp(2).to_string())
            }
        };

        for (index, ((&tid, &target), &currency)) in
            ids.iter().zip(&targets).zip(&team_currencies).enumerate()
        {
            let (q_cnt, q_amt, s_cnt, s_amt, inv_cnt, inv_amt) = metrics
                .get(&tid)
                .copied()
                .unwrap_or((0, Decimal::ZERO, 0, Decimal::ZERO, 0, Decimal::ZERO));
            let pct = compute_target_achievement(s_amt, target);

            let buttons = vec![json!({
                "label": "New Quotation",
                "action": "sale.action_quotations",
                "view_mode": "form",
                "context": {"default_team_id": tid},
                "primary": true,
            })];
            let stats = vec![
                json!({
                    "label": "Quotations",
                    "count": q_cnt,
                    "amount": fmt_amt(q_amt, currency)?,
                    "currency_id": currency,
                    "action": "sale.action_quotations",
                    "domain": [["team_id", "=", tid], ["state", "in", ["draft", "sent"]]],
                    "context": {"default_team_id": tid},
                    "tone": "neutral",
                }),
                json!({
                    "label": "Sales Orders",
                    "count": s_cnt,
                    "amount": fmt_amt(s_amt, currency)?,
                    "currency_id": currency,
                    "action": "sale.action_orders",
                    "domain": [["team_id", "=", tid], ["state", "=", "sale"]],
                    "context": {"default_team_id": tid},
                    "tone": "neutral",
                }),
                json!({
                    "label": "To Invoice",
                    "count": inv_cnt,
                    "amount": fmt_amt(inv_amt, currency)?,
                    "currency_id": currency,
                    "action": "sale.action_orders",
                    "domain": [["team_id", "=", tid], ["state", "=", "sale"], ["invoice_status", "=", "to invoice"]],
                    "context": {"default_team_id": tid},
                    "tone": if inv_cnt > 0 { "warning" } else { "neutral" },
                }),
            ];
            let chart = if target > Decimal::ZERO || s_amt > Decimal::ZERO {
                json!({
                    "type": "bar",
                    "label": format!("Target ({pct}%)"),
                    "points": [
                        {"label": "Sales", "value": fmt_amt(s_amt, currency)?, "tone": "warning"},
                        {"label": "Target", "value": fmt_amt(target, currency)?, "tone": "neutral"},
                    ],
                })
            } else {
                Json::Null
            };
            let card = json!({
                "v": 1,
                "buttons": buttons,
                "stats": stats,
                "chart": chart,
            });
            out.set(
                index,
                CrmTeamSaleDashboard::SALE_DASHBOARD_CARD,
                Some(Arc::from(card.to_string())),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::compute_target_achievement;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn target_achievement_is_exact_decimal() {
        assert_eq!(
            compute_target_achievement(
                Decimal::from_str("7500.00").unwrap(),
                Decimal::from_str("10000.00").unwrap(),
            ),
            Decimal::from_str("75.0").unwrap()
        );
        assert_eq!(
            compute_target_achievement(Decimal::from(500), Decimal::ZERO),
            Decimal::ZERO
        );
    }
}
