// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real-Time Budget Availability Control (AVC) & Purchase Order Commitment Ledger
//! (`account.budget.commitment`) with concurrent row-locking (`FOR UPDATE`) and automatic
//! PO-to-Vendor-Bill commitment liquidation (zero double-counting).

use erp_sdk::prelude::*;

use super::{db, refresh_budget_lines};

/// Group allowed to bypass Hard-Stop AVC budget refusals for authorized overrides.
pub const BUDGET_OVERRIDE_GROUP: &str = "account.group_budget_override";

/// Result of evaluating a single spend delta against a budget line's AVC profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvcVerdict {
    /// Within safe threshold.
    Ok,
    /// Exceeded warning threshold (allowed, flagged).
    Warning(String),
    /// Exceeded hard-stop blocking threshold (refused unless override group held).
    Blocked(String),
}

/// Pure exact-decimal Availability Control evaluation.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_avc(
    budget_label: &str,
    released: Decimal,
    committed: Decimal,
    actual: Decimal,
    delta: Decimal,
    avc_mode: &str,
    warning_pct: Decimal,
    blocking_pct: Decimal,
    tolerance: Decimal,
) -> AvcVerdict {
    if delta <= Decimal::ZERO || avc_mode == "none" {
        return AvcVerdict::Ok;
    }
    let base = released.abs();
    let projected = committed + actual + delta;
    let warn_limit = (base * warning_pct / Decimal::from(100)).round_dp(2);
    let block_limit = (base * blocking_pct / Decimal::from(100)).round_dp(2) + tolerance;

    if avc_mode == "hard_stop" && projected > block_limit {
        let shortfall = (projected - block_limit).round_dp(2);
        return AvcVerdict::Blocked(format!(
            "Budget Availability Control exceeded on '{budget_label}': Released {base}, Committed {committed}, Actual {actual}, Requested {delta}, Hard Limit {block_limit} (shortfall {shortfall}). Submit a Budget Supplement or Transfer to proceed."
        ));
    }
    if projected >= warn_limit {
        return AvcVerdict::Warning(format!(
            "Budget warning on '{budget_label}': projected spend {projected} reaches threshold {warn_limit} (Released {base})."
        ));
    }
    AvcVerdict::Ok
}

/// One spend commitment request (e.g. from a Purchase Order line on confirmation).
#[derive(Debug, Clone)]
pub struct CommitmentRequest {
    pub source_model: String,
    pub source_id: i64,
    pub source_ref: String,
    pub company_id: i64,
    pub account_id: Option<i64>,
    pub analytic_account_id: Option<i64>,
    pub asset_id: Option<i64>,
    pub date: Date,
    pub amount: Decimal,
}

/// `account.budget.commitment`: durable encumbrance ledger tracking open Purchase Order
/// commitments and their liquidation as vendor bills are posted.
#[model(
    name = "account.budget.commitment",
    description = "Budget Commitment",
    order = "date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    indexes = [
        ("_source_lookup", "(source_model, source_id)"),
        ("_budget_line_state", "(budget_line_id, state)"),
    ]
)]
pub struct AccountBudgetCommitment {
    /// Commitment reference (e.g. "PO00001 / Laptop").
    #[field(char, required = true, string = "Reference")]
    pub name: Option<Arc<str>>,
    /// Owning company.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Company currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Budget line encumbered by this commitment.
    #[field(
        many2one,
        comodel = "account.budget.line",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Budget Line"
    )]
    pub budget_line_id: Option<RecId>,
    /// Source document model (e.g. "purchase.order.line").
    #[field(char, required = true, string = "Source Model")]
    pub source_model: Option<Arc<str>>,
    /// Source record id.
    #[field(integer, required = true, string = "Source Record ID")]
    pub source_id: i32,
    /// Human-readable source document number.
    #[field(char, string = "Source Document")]
    pub source_ref: Option<Arc<str>>,
    /// GL Account if applicable.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "set null",
        string = "Account"
    )]
    pub account_id: Option<RecId>,
    /// Cost Center if applicable.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        ondelete = "set null",
        string = "Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// CIP Asset if applicable.
    #[field(
        many2one,
        comodel = "account.asset",
        ondelete = "set null",
        string = "CIP Project"
    )]
    pub asset_id: Option<RecId>,
    /// Commitment date.
    #[field(date, required = true, index = true, string = "Date")]
    pub date: Option<Date>,
    /// Original committed amount in company currency.
    #[field(monetary, currency_field = "currency_id", string = "Ordered Amount")]
    pub ordered_amount: Decimal,
    /// Portion already liquidated by posted vendor bills.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Liquidated (Billed)"
    )]
    pub liquidated_amount: Decimal,
    /// Open remaining commitment (`ordered_amount - liquidated_amount`).
    #[field(monetary, currency_field = "currency_id", string = "Open Commitment")]
    pub open_amount: Decimal,
    /// Lifecycle status.
    #[field(
        selection,
        selection = [
            ("open", "Open"),
            ("partially_billed", "Partially Liquidated"),
            ("closed", "Fully Liquidated"),
            ("cancelled", "Released / Cancelled"),
        ],
        required = true,
        default = "open",
        index = true,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
}

#[methods(AccountBudgetCommitment, addon = "account")]
impl AccountBudgetCommitment {}

struct MatchedBudgetLine {
    id: i64,
    label: String,
    released: Decimal,
    committed: Decimal,
    practical: Decimal,
    avc_mode: String,
    warning_pct: Decimal,
    blocking_pct: Decimal,
    tolerance: Decimal,
    check_commitments: bool,
}

async fn find_and_lock_matching_budget_lines(
    tx: &mut Transaction,
    company_id: i64,
    date: Date,
    account_id: Option<i64>,
    analytic_account_id: Option<i64>,
    asset_id: Option<i64>,
) -> Result<Vec<MatchedBudgetLine>> {
    // Match active validated OPEX/CAPEX budget lines covering `date` and matching
    // asset_id, Cost Center (exact or parent group via parent_path), and/or GL account.
    let rows = tx
        .query(
            "SELECT l.id::int8, (b.name || ' / ' || l.name)::text, \
                    COALESCE(l.released_amount, l.planned_amount, 0), \
                    COALESCE(l.committed_amount, 0), \
                    COALESCE(l.practical_amount, 0), \
                    COALESCE(p.avc_mode, 'none')::text, \
                    COALESCE(p.warning_threshold_pct, 80), \
                    COALESCE(p.blocking_threshold_pct, 100), \
                    COALESCE(p.tolerance_amount, 0), \
                    COALESCE(p.check_commitments, true) \
             FROM account_budget_line l \
             JOIN account_budget b ON b.id = l.budget_id \
             LEFT JOIN account_budget_profile p ON p.id = b.profile_id \
             LEFT JOIN account_analytic_account req_cc ON req_cc.id = $4 \
             LEFT JOIN account_analytic_account bl_cc ON bl_cc.id = l.analytic_account_id \
             WHERE b.company_id = $1 \
               AND b.state = 'validate' \
               AND COALESCE(b.budget_type, 'opex') IN ('opex', 'capex') \
               AND l.date_from <= $2 AND l.date_to >= $2 \
               AND (l.account_id IS NOT NULL OR l.analytic_account_id IS NOT NULL OR l.asset_id IS NOT NULL) \
               AND (l.asset_id IS NULL OR l.asset_id = $5) \
               AND (l.account_id IS NULL OR l.account_id = $3) \
               AND (l.analytic_account_id IS NULL \
                    OR (req_cc.id IS NOT NULL AND (req_cc.id = bl_cc.id \
                        OR (bl_cc.parent_path IS NOT NULL AND req_cc.parent_path IS NOT NULL \
                            AND starts_with(req_cc.parent_path, bl_cc.parent_path))))) \
             ORDER BY l.id \
             FOR UPDATE OF l",
            &[&company_id, &date, &account_id, &analytic_account_id, &asset_id],
        )
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(MatchedBudgetLine {
            id: row.try_get(0).map_err(db)?,
            label: row.try_get(1).map_err(db)?,
            released: row.try_get(2).map_err(db)?,
            committed: row.try_get(3).map_err(db)?,
            practical: row.try_get(4).map_err(db)?,
            avc_mode: row.try_get(5).map_err(db)?,
            warning_pct: row.try_get(6).map_err(db)?,
            blocking_pct: row.try_get(7).map_err(db)?,
            tolerance: row.try_get(8).map_err(db)?,
            check_commitments: row.try_get(9).map_err(db)?,
        });
    }
    Ok(out)
}

async fn has_override_right(tx: &mut Transaction, env: &Env) -> Result<bool> {
    if env.su() {
        return Ok(false);
    }
    tx.has_group(env, BUDGET_OVERRIDE_GROUP).await
}

/// Checks AVC and records durable Purchase Commitments for confirmed order lines.
/// Refuses loudly when any matched `hard_stop` budget line would be exceeded (unless the user
/// belongs to `account.group_budget_override`).
pub async fn check_and_record_commitments(
    tx: &mut Transaction,
    env: &Env,
    requests: &[CommitmentRequest],
) -> Result<()> {
    if requests.is_empty() {
        return Ok(());
    }
    let can_override = has_override_right(tx, env).await?;
    let mut touched_budget_lines = Vec::new();
    let mut running_delta_by_line = std::collections::BTreeMap::<i64, Decimal>::new();
    let mut new_commitments = Vec::new();
    let mut blocks = Vec::new();

    for req in requests {
        if req.amount <= Decimal::ZERO {
            continue;
        }
        // Remove any prior open commitments for the exact same source line to stay idempotent.
        let old_lines = tx
            .query(
                "UPDATE account_budget_commitment \
                 SET state = 'cancelled', open_amount = 0, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE source_model = $1 AND source_id = $2 AND state IN ('open', 'partially_billed') \
                 RETURNING budget_line_id::int8",
                &[&req.source_model, &(req.source_id as i32)],
            )
            .await?;
        for ol in old_lines {
            let bl_id: i64 = ol.try_get(0).map_err(db)?;
            if !touched_budget_lines.contains(&bl_id) {
                touched_budget_lines.push(bl_id);
            }
        }
        if !touched_budget_lines.is_empty() {
            refresh_budget_lines(tx, &touched_budget_lines).await?;
        }

        let matched = find_and_lock_matching_budget_lines(
            tx,
            req.company_id,
            req.date,
            req.account_id,
            req.analytic_account_id,
            req.asset_id,
        )
        .await?;

        for bl in matched {
            if !bl.check_commitments {
                continue;
            }
            let prior_delta = running_delta_by_line
                .get(&bl.id)
                .copied()
                .unwrap_or(Decimal::ZERO);
            let verdict = evaluate_avc(
                &bl.label,
                bl.released,
                bl.committed + prior_delta,
                bl.practical,
                req.amount,
                &bl.avc_mode,
                bl.warning_pct,
                bl.blocking_pct,
                bl.tolerance,
            );
            if let AvcVerdict::Blocked(msg) = verdict
                && !can_override
            {
                blocks.push(msg);
                continue;
            }
            *running_delta_by_line.entry(bl.id).or_insert(Decimal::ZERO) += req.amount;
            if !touched_budget_lines.contains(&bl.id) {
                touched_budget_lines.push(bl.id);
            }
            let mut v = RawVals::new();
            v.insert("name".into(), Cell::Str(req.source_ref.as_str().into()));
            v.insert("company_id".into(), Cell::Id(req.company_id));
            v.insert("budget_line_id".into(), Cell::Id(bl.id));
            v.insert(
                "source_model".into(),
                Cell::Str(req.source_model.as_str().into()),
            );
            v.insert("source_id".into(), Cell::Int(req.source_id as i32));
            v.insert(
                "source_ref".into(),
                Cell::Str(req.source_ref.as_str().into()),
            );
            if let Some(acc) = req.account_id {
                v.insert("account_id".into(), Cell::Id(acc));
            }
            if let Some(cc) = req.analytic_account_id {
                v.insert("analytic_account_id".into(), Cell::Id(cc));
            }
            if let Some(aid) = req.asset_id {
                v.insert("asset_id".into(), Cell::Id(aid));
            }
            v.insert("date".into(), Cell::Date(req.date));
            v.insert("ordered_amount".into(), Cell::Decimal(req.amount));
            v.insert("liquidated_amount".into(), Cell::Decimal(Decimal::ZERO));
            v.insert("open_amount".into(), Cell::Decimal(req.amount));
            v.insert("state".into(), Cell::Str("open".into()));
            new_commitments.push(v);
        }
    }

    if !blocks.is_empty() {
        return Err(Error::user(blocks.join("\n")));
    }

    if !new_commitments.is_empty() {
        let clean_env = env
            .sudo(true)
            .with_context_replaced(erp_sdk::Context::default());
        clean_env
            .records("account.budget.commitment")?
            .create(tx, new_commitments)
            .await?;
    }
    if !touched_budget_lines.is_empty() {
        refresh_budget_lines(tx, &touched_budget_lines).await?;
    }
    Ok(())
}

/// Releases open commitments when their source purchase order lines are cancelled.
pub async fn release_commitments_for_source(
    tx: &mut Transaction,
    source_model: &str,
    source_ids: &[i64],
) -> Result<()> {
    if source_ids.is_empty() {
        return Ok(());
    }
    let ids_i32: Vec<i32> = source_ids.iter().map(|id| *id as i32).collect();
    let rows = tx
        .query(
            "UPDATE account_budget_commitment \
             SET state = 'cancelled', open_amount = 0, write_date = now() AT TIME ZONE 'UTC' \
             WHERE source_model = $1 AND source_id = ANY($2::int4[]) \
               AND state IN ('open', 'partially_billed') \
             RETURNING budget_line_id::int8",
            &[&source_model, &ids_i32],
        )
        .await?;
    let mut touched = Vec::new();
    for r in rows {
        let bl_id: i64 = r.try_get(0).map_err(db)?;
        if !touched.contains(&bl_id) {
            touched.push(bl_id);
        }
    }
    if !touched.is_empty() {
        refresh_budget_lines(tx, &touched).await?;
    }
    Ok(())
}

/// Liquidates open purchase commitments linked to a vendor bill line when the bill is posted,
/// returning `(budget_line_id -> liquidated_amount)` so AVC only evaluates net new spending.
async fn liquidate_po_commitments_for_move_line(
    tx: &mut Transaction,
    move_line_id: i64,
    billed_amount: Decimal,
    touched_budget_lines: &mut Vec<i64>,
) -> Result<std::collections::BTreeMap<i64, Decimal>> {
    let mut liquidated_by_line = std::collections::BTreeMap::new();
    if billed_amount <= Decimal::ZERO {
        return Ok(liquidated_by_line);
    }
    // Check if `purchase_direct_bill_line` exists (purchase addon installed) and links this bill line.
    let table_exists: bool = tx
        .query(
            "SELECT to_regclass('public.purchase_direct_bill_line') IS NOT NULL",
            &[],
        )
        .await?[0]
        .try_get(0)
        .map_err(db)?;
    if !table_exists {
        return Ok(liquidated_by_line);
    }
    let po_lines = tx
        .query(
            "SELECT purchase_line_id::int4 FROM purchase_direct_bill_line WHERE bill_line_id = $1",
            &[&move_line_id],
        )
        .await?;
    if po_lines.is_empty() {
        return Ok(liquidated_by_line);
    }
    let po_line_ids: Vec<i32> = po_lines
        .iter()
        .map(|r| r.try_get(0).map_err(db))
        .collect::<Result<_>>()?;

    let commits = tx
        .query(
            "SELECT id::int8, budget_line_id::int8, liquidated_amount, open_amount \
             FROM account_budget_commitment \
             WHERE source_model = 'purchase.order.line' \
               AND source_id = ANY($1::int4[]) \
               AND state IN ('open', 'partially_billed') \
             ORDER BY id FOR UPDATE",
            &[&po_line_ids],
        )
        .await?;

    for crow in commits {
        let cid: i64 = crow.try_get(0).map_err(db)?;
        let bl_id: i64 = crow.try_get(1).map_err(db)?;
        let cur_liq: Decimal = crow.try_get(2).map_err(db)?;
        let cur_open: Decimal = crow.try_get(3).map_err(db)?;
        let take = cur_open.min(billed_amount);
        if take <= Decimal::ZERO {
            continue;
        }
        let next_liq = cur_liq + take;
        let next_open = cur_open - take;
        let next_state = if next_open <= Decimal::ZERO {
            "closed"
        } else {
            "partially_billed"
        };
        tx.execute(
            "UPDATE account_budget_commitment \
             SET liquidated_amount = $2, open_amount = $3, state = $4, \
                 write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[&cid, &next_liq, &next_open, &next_state],
        )
        .await?;
        *liquidated_by_line.entry(bl_id).or_insert(Decimal::ZERO) += take;
        if !touched_budget_lines.contains(&bl_id) {
            touched_budget_lines.push(bl_id);
        }
    }
    Ok(liquidated_by_line)
}

/// Evaluates real-time AVC on journal entries / vendor bills being posted (`post_moves`),
/// liquidating any linked PO commitments atomically (zero double-counting), and returns
/// the matched budget line IDs so the caller refreshes their utilization metrics right after
/// `set_state("posted")` and analytic line generation.
pub async fn check_and_apply_move_avc(
    tx: &mut Transaction,
    env: &Env,
    move_ids: &[i64],
) -> Result<Vec<i64>> {
    if move_ids.is_empty() {
        return Ok(Vec::new());
    }
    // Fast skip when no validated OPEX/CAPEX budget exists.
    let active_budgets: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM account_budget \
             WHERE state = 'validate' AND COALESCE(budget_type, 'opex') IN ('opex', 'capex')",
            &[],
        )
        .await?[0]
        .try_get(0)
        .map_err(db)?;
    if active_budgets == 0 {
        return Ok(Vec::new());
    }

    let lines = tx
        .query(
            "SELECT l.id::int8, l.company_id::int8, l.date, l.account_id::int8, \
                    l.analytic_account_id::int8, l.asset_id::int8, COALESCE(l.balance, 0), \
                    COALESCE(a.internal_group, '')::text \
             FROM account_move_line l \
             JOIN account_account a ON a.id = l.account_id \
             WHERE l.move_id = ANY($1::int8[]) \
               AND l.account_id IS NOT NULL \
               AND COALESCE(l.balance, 0) > 0 \
               AND (a.internal_group = 'expense' OR l.asset_id IS NOT NULL) \
             ORDER BY l.id",
            &[&move_ids],
        )
        .await?;
    if lines.is_empty() {
        return Ok(Vec::new());
    }

    let can_override = has_override_right(tx, env).await?;
    let mut touched_budget_lines = Vec::new();
    let mut blocks = Vec::new();

    for row in lines {
        let ml_id: i64 = row.try_get(0).map_err(db)?;
        let company_id: i64 = row.try_get(1).map_err(db)?;
        let date: Date = row.try_get(2).map_err(db)?;
        let account_id: i64 = row.try_get(3).map_err(db)?;
        let analytic_id: Option<i64> = row.try_get(4).map_err(db)?;
        let asset_id: Option<i64> = row.try_get(5).map_err(db)?;
        let spend: Decimal = row.try_get(6).map_err(db)?;

        let liquidated_map =
            liquidate_po_commitments_for_move_line(tx, ml_id, spend, &mut touched_budget_lines)
                .await?;

        let matched = find_and_lock_matching_budget_lines(
            tx,
            company_id,
            date,
            Some(account_id),
            analytic_id,
            asset_id,
        )
        .await?;

        for bl in matched {
            if !touched_budget_lines.contains(&bl.id) {
                touched_budget_lines.push(bl.id);
            }
            // Subtract the commitment liquidated by this very bill so PO -> Bill has ZERO double-counting.
            let liq = liquidated_map.get(&bl.id).copied().unwrap_or(Decimal::ZERO);
            let net_incremental = (spend - liq).max(Decimal::ZERO);
            if net_incremental.is_zero() {
                continue;
            }
            let effective_committed = if bl.check_commitments {
                (bl.committed - liq).max(Decimal::ZERO)
            } else {
                Decimal::ZERO
            };
            let verdict = evaluate_avc(
                &bl.label,
                bl.released,
                effective_committed,
                bl.practical,
                net_incremental,
                &bl.avc_mode,
                bl.warning_pct,
                bl.blocking_pct,
                bl.tolerance,
            );
            if let AvcVerdict::Blocked(msg) = verdict
                && !can_override
            {
                blocks.push(msg);
            }
        }
    }

    if !blocks.is_empty() {
        return Err(Error::user(blocks.join("\n")));
    }
    Ok(touched_budget_lines)
}

#[cfg(test)]
mod tests {
    use super::{AvcVerdict, evaluate_avc};
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn avc_blocks_when_committed_plus_actual_plus_new_exceeds_released_plus_tolerance() {
        let released = Decimal::from_str("10000.00").unwrap();
        let committed = Decimal::from_str("4000.00").unwrap();
        let actual = Decimal::from_str("5500.00").unwrap();
        let new_po = Decimal::from_str("600.00").unwrap(); // total 10100 > 10000
        let verdict = evaluate_avc(
            "FY26 IT / Cloud",
            released,
            committed,
            actual,
            new_po,
            "hard_stop",
            Decimal::from(80),
            Decimal::from(100),
            Decimal::ZERO,
        );
        assert!(matches!(verdict, AvcVerdict::Blocked(_)));

        // Within $200 tolerance grace buffer -> warning instead of block
        let with_tol = evaluate_avc(
            "FY26 IT / Cloud",
            released,
            committed,
            actual,
            new_po,
            "hard_stop",
            Decimal::from(80),
            Decimal::from(100),
            Decimal::from(200),
        );
        assert!(matches!(with_tol, AvcVerdict::Warning(_)));
    }
}
