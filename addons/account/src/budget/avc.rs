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

/// Pure exact-decimal Availability Control evaluation (default 2 decimal places).
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
    evaluate_avc_with_scale(
        budget_label,
        released,
        committed,
        actual,
        delta,
        avc_mode,
        warning_pct,
        blocking_pct,
        tolerance,
        2,
    )
}

/// Pure exact-decimal Availability Control evaluation using the company currency's decimal scale.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_avc_with_scale(
    budget_label: &str,
    released: Decimal,
    committed: Decimal,
    actual: Decimal,
    delta: Decimal,
    avc_mode: &str,
    warning_pct: Decimal,
    blocking_pct: Decimal,
    tolerance: Decimal,
    scale: u32,
) -> AvcVerdict {
    if delta <= Decimal::ZERO || avc_mode == "none" {
        return AvcVerdict::Ok;
    }
    let base = released.abs();
    let projected = committed + actual + delta;
    let warn_limit = (base * warning_pct / Decimal::from(100)).round_dp(scale);
    let block_limit = (base * blocking_pct / Decimal::from(100)).round_dp(scale) + tolerance;

    if avc_mode == "hard_stop" && projected > block_limit {
        let shortfall = (projected - block_limit).round_dp(scale);
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
    #[field(bigint, required = true, string = "Source Record ID")]
    pub source_id: erp_sdk::BigInt,
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
impl AccountBudgetCommitment {
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountBudgetCommitment>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            return Err(Error::user(
                "Budget commitments are managed automatically by Purchase Order confirmation and Vendor Bill posting and cannot be edited manually.",
            ));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountBudgetCommitment>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            return Err(Error::user(
                "Budget commitments cannot be deleted manually; cancel the originating Purchase Order to release commitments.",
            ));
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

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
    scale: u32,
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
                    COALESCE(l.released_amount, 0), \
                    COALESCE(l.committed_amount, 0), \
                    COALESCE(l.practical_amount, 0), \
                    COALESCE(p.avc_mode, 'none')::text, \
                    COALESCE(p.warning_threshold_pct, 80), \
                    COALESCE(p.blocking_threshold_pct, 100), \
                    COALESCE(p.tolerance_amount, 0), \
                    COALESCE(p.check_commitments, true), \
                    COALESCE(cur.decimal_places, 2)::int4 \
             FROM account_budget_line l \
             JOIN account_budget b ON b.id = l.budget_id \
             JOIN res_currency cur ON cur.id = l.currency_id \
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
        let scale_i32: i32 = row.try_get(10).map_err(db)?;
        let scale = u32::try_from(scale_i32)
            .map_err(|_| Error::validation(format!("Invalid currency scale {scale_i32}.")))?;
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
            scale,
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

    // Release prior open commitments for the exact same source lines first so budget line locks
    // and refreshed totals reflect the clean baseline before evaluating the whole batch.
    for req in requests {
        if req.amount <= Decimal::ZERO {
            continue;
        }
        let old_lines = tx
            .query(
                "UPDATE account_budget_commitment \
                 SET state = 'cancelled', open_amount = 0, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE source_model = $1 AND source_id = $2::int8 AND state IN ('open', 'partially_billed') \
                 RETURNING budget_line_id::int8",
                &[&req.source_model, &req.source_id],
            )
            .await?;
        for ol in old_lines {
            let bl_id: i64 = ol.try_get(0).map_err(db)?;
            if !touched_budget_lines.contains(&bl_id) {
                touched_budget_lines.push(bl_id);
            }
        }
    }
    if !touched_budget_lines.is_empty() {
        touched_budget_lines.sort_unstable();
        touched_budget_lines.dedup();
        refresh_budget_lines(tx, &touched_budget_lines).await?;
    }

    // Pre-lock all candidate budget lines for the companies/dates in sorted canonical ID order to
    // prevent cross-request deadlocks on multi-line POs.
    let mut candidate_company_ids: Vec<i64> = requests.iter().map(|r| r.company_id).collect();
    candidate_company_ids.sort_unstable();
    candidate_company_ids.dedup();
    tx.query(
        "SELECT l.id::int8 FROM account_budget_line l \
         JOIN account_budget b ON b.id = l.budget_id \
         WHERE b.company_id = ANY($1::int8[]) AND b.state = 'validate' \
           AND COALESCE(b.budget_type, 'opex') IN ('opex', 'capex') \
         ORDER BY l.id FOR UPDATE OF l",
        &[&candidate_company_ids],
    )
    .await?;

    for req in requests {
        if req.amount <= Decimal::ZERO {
            continue;
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
            let verdict = evaluate_avc_with_scale(
                &bl.label,
                bl.released,
                bl.committed + prior_delta,
                bl.practical,
                req.amount,
                &bl.avc_mode,
                bl.warning_pct,
                bl.blocking_pct,
                bl.tolerance,
                bl.scale,
            );
            let mut audit_suffix = "";
            match verdict {
                AvcVerdict::Blocked(msg) => {
                    if can_override {
                        audit_suffix = " [AVC Override]";
                    } else {
                        blocks.push(msg);
                        continue;
                    }
                }
                AvcVerdict::Warning(_) => {
                    audit_suffix = " [AVC Warning]";
                }
                AvcVerdict::Ok => {}
            }
            *running_delta_by_line.entry(bl.id).or_insert(Decimal::ZERO) += req.amount;
            if !touched_budget_lines.contains(&bl.id) {
                touched_budget_lines.push(bl.id);
            }
            let mut v = RawVals::new();
            let commit_name = if audit_suffix.is_empty() {
                req.source_ref.clone()
            } else {
                format!("{}{audit_suffix}", req.source_ref)
            };
            v.insert("name".into(), Cell::Str(commit_name.as_str().into()));
            v.insert("company_id".into(), Cell::Id(req.company_id));
            v.insert("budget_line_id".into(), Cell::Id(bl.id));
            v.insert(
                "source_model".into(),
                Cell::Str(req.source_model.as_str().into()),
            );
            v.insert(
                "source_id".into(),
                Cell::BigInt(erp_sdk::BigInt::from_i64(req.source_id)),
            );
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
        let clean_env = crate::moves::post::strip_default_context(env).sudo(true);
        clean_env
            .records("account.budget.commitment")?
            .create(tx, new_commitments)
            .await?;
    }
    if !touched_budget_lines.is_empty() {
        touched_budget_lines.sort_unstable();
        touched_budget_lines.dedup();
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
    let rows = tx
        .query(
            "UPDATE account_budget_commitment \
             SET state = 'cancelled', open_amount = 0, write_date = now() AT TIME ZONE 'UTC' \
             WHERE source_model = $1 AND source_id = ANY($2::int8[]) \
               AND state IN ('open', 'partially_billed') \
             RETURNING budget_line_id::int8",
            &[&source_model, &source_ids],
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
        touched.sort_unstable();
        touched.dedup();
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
    if tx.registry().model("purchase.direct.bill.line").is_none() {
        return Ok(liquidated_by_line);
    }
    let po_lines = tx
        .query(
            "SELECT purchase_line_id::int8 FROM purchase_direct_bill_line WHERE bill_line_id = $1",
            &[&move_line_id],
        )
        .await?;
    if po_lines.is_empty() {
        return Ok(liquidated_by_line);
    }
    let po_line_ids: Vec<i64> = po_lines
        .iter()
        .map(|r| r.try_get(0).map_err(db))
        .collect::<Result<_>>()?;

    let commits = tx
        .query(
            "SELECT id::int8, budget_line_id::int8, liquidated_amount, open_amount \
             FROM account_budget_commitment \
             WHERE source_model = 'purchase.order.line' \
               AND source_id = ANY($1::int8[]) \
               AND state IN ('open', 'partially_billed') \
             ORDER BY id FOR UPDATE",
            &[&po_line_ids],
        )
        .await?;

    let mut remaining = billed_amount;
    for crow in commits {
        if remaining <= Decimal::ZERO {
            break;
        }
        let cid: i64 = crow.try_get(0).map_err(db)?;
        let bl_id: i64 = crow.try_get(1).map_err(db)?;
        let cur_liq: Decimal = crow.try_get(2).map_err(db)?;
        let cur_open: Decimal = crow.try_get(3).map_err(db)?;
        let take = cur_open.min(remaining);
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
        remaining -= take;
        *liquidated_by_line.entry(bl_id).or_insert(Decimal::ZERO) += take;
        if !touched_budget_lines.contains(&bl_id) {
            touched_budget_lines.push(bl_id);
        }
    }
    Ok(liquidated_by_line)
}

/// Restores any open commitments liquidated by `move_ids` when those moves are reset to draft,
/// and refreshes all budget lines matching the reset moves so actuals stay accurate.
pub async fn restore_commitments_and_refresh_for_reset_moves(
    tx: &mut Transaction,
    move_ids: &[i64],
) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    let mut touched_budget_lines: Vec<i64> = Vec::new();
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
        return Ok(());
    }

    let lines = tx
        .query(
            "SELECT l.id::int8, l.company_id::int8, l.date, l.account_id::int8, \
                    l.analytic_account_id::int8, l.asset_id::int8, COALESCE(l.balance, 0) \
             FROM account_move_line l \
             WHERE l.move_id = ANY($1::int8[]) AND l.account_id IS NOT NULL \
             ORDER BY l.id",
            &[&move_ids],
        )
        .await?;

    let has_direct_bill = tx.registry().model("purchase.direct.bill.line").is_some();
    for row in lines {
        let ml_id: i64 = row.try_get(0).map_err(db)?;
        let company_id: i64 = row.try_get(1).map_err(db)?;
        let date: Date = row.try_get(2).map_err(db)?;
        let account_id: i64 = row.try_get(3).map_err(db)?;
        let analytic_id: Option<i64> = row.try_get(4).map_err(db)?;
        let asset_id: Option<i64> = row.try_get(5).map_err(db)?;
        let spend: Decimal = row.try_get(6).map_err(db)?;

        if has_direct_bill && spend > Decimal::ZERO {
            let po_lines = tx
                .query(
                    "SELECT purchase_line_id::int8 FROM purchase_direct_bill_line WHERE bill_line_id = $1",
                    &[&ml_id],
                )
                .await?;
            let po_line_ids: Vec<i64> = po_lines
                .iter()
                .map(|r| r.try_get(0).map_err(db))
                .collect::<Result<_>>()?;
            if !po_line_ids.is_empty() {
                let commits = tx
                    .query(
                        "SELECT id::int8, budget_line_id::int8, ordered_amount, liquidated_amount \
                         FROM account_budget_commitment \
                         WHERE source_model = 'purchase.order.line' \
                           AND source_id = ANY($1::int8[]) \
                           AND state IN ('partially_billed', 'closed') \
                         ORDER BY id FOR UPDATE",
                        &[&po_line_ids],
                    )
                    .await?;
                let mut remaining = spend;
                for crow in commits {
                    if remaining <= Decimal::ZERO {
                        break;
                    }
                    let cid: i64 = crow.try_get(0).map_err(db)?;
                    let bl_id: i64 = crow.try_get(1).map_err(db)?;
                    let ordered: Decimal = crow.try_get(2).map_err(db)?;
                    let cur_liq: Decimal = crow.try_get(3).map_err(db)?;
                    let restore = cur_liq.min(remaining);
                    if restore <= Decimal::ZERO {
                        continue;
                    }
                    let next_liq = cur_liq - restore;
                    let next_open = (ordered - next_liq).max(Decimal::ZERO);
                    let next_state = if next_liq <= Decimal::ZERO {
                        "open"
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
                    remaining -= restore;
                    if !touched_budget_lines.contains(&bl_id) {
                        touched_budget_lines.push(bl_id);
                    }
                }
            }
        }

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
        }
    }

    if !touched_budget_lines.is_empty() {
        refresh_budget_lines(tx, &touched_budget_lines).await?;
    }
    Ok(())
}

/// Evaluates real-time AVC on journal entries / vendor bills being posted (`post_moves`),
/// liquidating any linked PO commitments atomically (zero double-counting), and returns
/// the matched budget line IDs so the caller refreshes their utilization metrics right after
/// `set_state("posted")` and analytic line generation.
pub async fn check_and_apply_move_avc(
    tx: &mut Transaction,
    env: &Env,
    move_dates: &[(i64, Date)],
) -> Result<Vec<i64>> {
    if move_dates.is_empty() {
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

    let (move_ids, dates): (Vec<i64>, Vec<Date>) = move_dates.iter().copied().unzip();
    let lines = tx
        .query(
            "WITH md AS (SELECT * FROM unnest($1::int8[], $2::date[]) AS t(move_id, eff_date)) \
             SELECT l.id::int8, l.company_id::int8, md.eff_date, l.account_id::int8, \
                    l.analytic_account_id::int8, l.asset_id::int8, COALESCE(l.balance, 0), \
                    COALESCE(a.internal_group, '')::text, l.move_id::int8 \
             FROM md \
             JOIN account_move_line l ON l.move_id = md.move_id \
             JOIN account_account a ON a.id = l.account_id \
             WHERE l.account_id IS NOT NULL \
               AND COALESCE(l.balance, 0) <> 0 \
               AND (a.internal_group IN ('expense', 'asset') OR l.asset_id IS NOT NULL) \
             ORDER BY l.id",
            &[&move_ids, &dates],
        )
        .await?;
    if lines.is_empty() {
        return Ok(Vec::new());
    }

    // Pre-lock all active OPEX/CAPEX budget lines for the companies in this batch in canonical ID
    // order so concurrent multi-line postings cannot deadlock.
    let mut batch_companies: Vec<i64> = lines
        .iter()
        .map(|r| r.try_get(1).map_err(db))
        .collect::<Result<_>>()?;
    batch_companies.sort_unstable();
    batch_companies.dedup();
    tx.query(
        "SELECT l.id::int8 FROM account_budget_line l \
         JOIN account_budget b ON b.id = l.budget_id \
         WHERE b.company_id = ANY($1::int8[]) AND b.state = 'validate' \
           AND COALESCE(b.budget_type, 'opex') IN ('opex', 'capex') \
         ORDER BY l.id FOR UPDATE OF l",
        &[&batch_companies],
    )
    .await?;

    let can_override = has_override_right(tx, env).await?;
    let mut touched_budget_lines = Vec::new();
    let mut running_actual_by_line = std::collections::BTreeMap::<i64, Decimal>::new();
    let mut running_liq_by_line = std::collections::BTreeMap::<i64, Decimal>::new();
    let mut blocks = Vec::new();
    let mut warnings_by_move = std::collections::BTreeMap::<i64, Vec<String>>::new();

    for row in lines {
        let ml_id: i64 = row.try_get(0).map_err(db)?;
        let company_id: i64 = row.try_get(1).map_err(db)?;
        let date: Date = row.try_get(2).map_err(db)?;
        let account_id: i64 = row.try_get(3).map_err(db)?;
        let analytic_id: Option<i64> = row.try_get(4).map_err(db)?;
        let asset_id: Option<i64> = row.try_get(5).map_err(db)?;
        let spend: Decimal = row.try_get(6).map_err(db)?;
        let move_id: i64 = row.try_get(8).map_err(db)?;

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
            let liq = liquidated_map.get(&bl.id).copied().unwrap_or(Decimal::ZERO);
            let prior_liq = running_liq_by_line
                .get(&bl.id)
                .copied()
                .unwrap_or(Decimal::ZERO);
            let prior_actual = running_actual_by_line
                .get(&bl.id)
                .copied()
                .unwrap_or(Decimal::ZERO);

            if spend > Decimal::ZERO {
                // Subtract liquidated commitment from `committed` ONCE and pass full `spend`
                // (never subtract `liq` from both `committed` and `spend`!).
                let effective_committed = if bl.check_commitments {
                    (bl.committed - prior_liq - liq).max(Decimal::ZERO)
                } else {
                    Decimal::ZERO
                };
                let verdict = evaluate_avc_with_scale(
                    &bl.label,
                    bl.released,
                    effective_committed,
                    bl.practical + prior_actual,
                    spend,
                    &bl.avc_mode,
                    bl.warning_pct,
                    bl.blocking_pct,
                    bl.tolerance,
                    bl.scale,
                );
                match verdict {
                    AvcVerdict::Blocked(msg) if !can_override => {
                        blocks.push(msg);
                    }
                    AvcVerdict::Blocked(msg) => {
                        warnings_by_move
                            .entry(move_id)
                            .or_default()
                            .push(format!("[AVC Override] {msg}"));
                    }
                    AvcVerdict::Warning(msg) => {
                        warnings_by_move.entry(move_id).or_default().push(msg);
                    }
                    AvcVerdict::Ok => {}
                }
            }
            *running_liq_by_line.entry(bl.id).or_insert(Decimal::ZERO) += liq;
            *running_actual_by_line.entry(bl.id).or_insert(Decimal::ZERO) += spend;
        }
    }

    if !blocks.is_empty() {
        return Err(Error::user(blocks.join("\n")));
    }
    if !warnings_by_move.is_empty() && tx.registry().model("mail.message").is_some() {
        let escape_html = |text: &str| {
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let clean_env = crate::moves::post::strip_default_context(env).sudo(true);
        let msg_model = clean_env.records("mail.message")?;
        let mut msg_vals = Vec::with_capacity(warnings_by_move.len());
        for (mid, msgs) in warnings_by_move {
            let body_html = msgs
                .iter()
                .map(|m| format!("<p>{}</p>", escape_html(m)))
                .collect::<String>();
            let mut v = RawVals::new();
            v.insert("model".into(), Cell::Str("account.move".into()));
            v.insert("res_id".into(), Cell::Id(mid));
            v.insert("message_type".into(), Cell::Str("notification".into()));
            v.insert("body".into(), Cell::Str(body_html.into()));
            msg_vals.push(v);
        }
        msg_model.create(tx, msg_vals).await?;
    }
    touched_budget_lines.sort_unstable();
    touched_budget_lines.dedup();
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
