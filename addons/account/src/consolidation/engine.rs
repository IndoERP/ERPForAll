// SPDX-License-Identifier: MIT OR Apache-2.0
//! Consolidation Execution Monitor (`account.consolidation.run`), Top-Side Group Adjustments
//! (`account.consolidation.adjustment`), and Posting-Level Lines (`account.consolidation.line`):
//! 4-stage Multi-Rate Translation + CTA, IC Eliminations, Manual Adjustments, and NCI Carve-Out.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RateBook, RoundingTable};
use erp_sdk::Operation;
use erp_sdk::money::RateDateRule;
use erp_sdk::prelude::*;

use super::{AccountConsolidationGroup, db, default_fs_code_for_account_type};

fn default_year_start(_ctx: &erp_sdk::DefaultContext) -> Value {
    let (y, _, _) = Date::today().ymd();
    Value::Date(Date::from_ymd(y, 1, 1).unwrap_or_else(Date::today))
}

fn default_today(_ctx: &erp_sdk::DefaultContext) -> Value {
    Value::Date(Date::today())
}

/// Computes the exact Cumulative Translation Adjustment (CTA) balancing entry (in signed
/// debit-positive / credit-negative accounting convention) so that:
/// `sum(all translated BS balances + translated P&L net balance + CTA) == 0.00`.
pub fn compute_cta_plug(translated_signed_sum: Decimal) -> Decimal {
    (-translated_signed_sum).round_dp(2)
}

/// Computes the Non-Controlling Interest (Minority Interest) share for a `full`-consolidated
/// subsidiary with `ownership_pct` (e.g. 80% ownership -> 20% NCI).
pub fn compute_nci_share(subsidiary_amount: Decimal, ownership_pct: Decimal) -> Decimal {
    let nci_pct = (Decimal::from(100) - ownership_pct).max(Decimal::ZERO);
    (subsidiary_amount * nci_pct / Decimal::from(100)).round_dp(2)
}

/// Pairs an intercompany debit balance (`ar_or_exp > 0`) with an intercompany credit balance
/// (`ap_or_rev < 0` expressed as positive magnitude `credit_mag > 0`), returning
/// `(matched_eliminated, net_variance)` where `net_variance = debit_mag - credit_mag`.
pub fn pair_ic_balances(debit_mag: Decimal, credit_mag: Decimal) -> (Decimal, Decimal) {
    let matched = debit_mag.min(credit_mag).max(Decimal::ZERO).round_dp(2);
    let variance = (debit_mag - credit_mag).round_dp(2);
    (matched, variance)
}

/// `account.consolidation.run`: persisted, auditable group financial consolidation run for a
/// Consolidation Group and reporting period.
#[model(
    name = "account.consolidation.run",
    description = "Consolidation Run",
    order = "date_to desc, id desc"
)]
pub struct AccountConsolidationRun {
    /// Run name (e.g. "FY2026 Consolidated Financial Statements").
    #[field(char, required = true, string = "Run Name")]
    pub name: Option<Arc<str>>,
    /// Consolidation Group.
    #[field(
        many2one,
        comodel = "account.consolidation.group",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Consolidation Group"
    )]
    pub group_id: Option<RecId>,
    /// Group presentation currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "group_id.currency_id",
        store = true,
        string = "Group Currency"
    )]
    pub currency_id: Option<RecId>,
    /// P&L period start date (inclusive).
    #[field(date, required = true, default_fn = default_year_start, string = "Period Start")]
    pub date_from: Option<Date>,
    /// Balance Sheet & P&L period end date (inclusive).
    #[field(date, required = true, default_fn = default_today, string = "Period End")]
    pub date_to: Option<Date>,
    /// Run workflow status.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("executed", "Executed (Review)"),
            ("locked", "Locked (Final)"),
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Consolidated Total Assets (positive).
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Consolidated Assets"
    )]
    pub total_assets: Decimal,
    /// Consolidated Total Liabilities + Equity + Retained/Net Profit + CTA + NCI (positive).
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Consolidated Liabilities & Equity"
    )]
    pub total_liabilities_equity: Decimal,
    /// Accounting equation check (`total_assets - total_liabilities_equity`; must be 0.00 to lock).
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Balance Check Diff"
    )]
    pub balance_check_diff: Decimal,
    /// Total Cumulative Translation Adjustment (CTA) across foreign subsidiaries.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Total CTA"
    )]
    pub total_cta: Decimal,
    /// Total Non-Controlling Interest carved out in Equity.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "NCI (Equity)"
    )]
    pub total_nci_equity: Decimal,
    /// Non-Controlling Interest share of period Net Profit.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "NCI (Net Profit)"
    )]
    pub total_nci_profit: Decimal,
    /// Consolidated Group Net Profit (before NCI split).
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Group Net Profit"
    )]
    pub group_net_profit: Decimal,
    /// Top-side manual consolidation journal adjustments attached to this run.
    #[field(
        one2many,
        comodel = "account.consolidation.adjustment",
        inverse_name = "run_id",
        string = "Manual Adjustments"
    )]
    pub adjustment_ids: Vec<RecId>,
    /// Generated consolidation ledger lines across Posting Levels 00, 10, 20, and 30.
    #[field(
        one2many,
        comodel = "account.consolidation.line",
        inverse_name = "run_id",
        readonly = true,
        string = "Consolidation Lines"
    )]
    pub line_ids: Vec<RecId>,
}

#[methods(AccountConsolidationRun, addon = "account")]
impl AccountConsolidationRun {
    /// Executes the full 4-stage consolidation pipeline deterministically inside one transaction:
    /// - Level 00: Local balance extraction + multi-rate FX translation (Closing / Average / Historical) + exact CTA
    /// - Level 20: Automatic IC Eliminations (AR/AP, Revenue/Expense, Investment vs Share Capital + Goodwill)
    /// - Level 10: Posted Top-Side Manual Consolidation Adjustments
    /// - Level 30: Non-Controlling Interest (NCI) Profit & Equity Carve-Out
    #[method]
    #[rpc(effect = write)]
    pub async fn action_execute_run(
        this: &RecordSet<AccountConsolidationRun>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let run = this.ensure_one()?;
        run.as_records().check_access(tx, Operation::Write).await?;
        let run_id = run.as_records().ids()[0];
        let state = run.get(tx, AccountConsolidationRun::STATE).await?;
        if state.as_deref() == Some("locked") {
            return Err(Error::user(
                "A locked consolidation run cannot be re-executed.",
            ));
        }
        let group_id = run
            .get(tx, AccountConsolidationRun::GROUP_ID)
            .await?
            .ok_or_else(|| Error::validation("Select a Consolidation Group."))?
            .get();
        let date_from = run
            .get(tx, AccountConsolidationRun::DATE_FROM)
            .await?
            .ok_or_else(|| Error::validation("Period Start is required."))?;
        let date_to = run
            .get(tx, AccountConsolidationRun::DATE_TO)
            .await?
            .ok_or_else(|| Error::validation("Period End is required."))?;
        if date_to.days() < date_from.days() {
            return Err(Error::validation(
                "Period End cannot be earlier than Period Start.",
            ));
        }

        // Read group config.
        let grp_rows = tx
            .query(
                "SELECT currency_id::int8, closing_rate_type_code::text, \
                        average_rate_type_code::text, historical_rate_type_code::text, \
                        COALESCE(fallback_to_spot, true) \
                 FROM account_consolidation_group WHERE id = $1",
                &[&group_id],
            )
            .await?;
        let Some(grp_row) = grp_rows.first() else {
            return Err(Error::missing("Consolidation Group does not exist."));
        };
        let group_currency_id: i64 = grp_row.try_get(0).map_err(db)?;
        let closing_code: String = grp_row.try_get(1).map_err(db)?;
        let average_code: String = grp_row.try_get(2).map_err(db)?;
        let historical_code: String = grp_row.try_get(3).map_err(db)?;
        let fallback_to_spot: bool = grp_row.try_get(4).map_err(db)?;

        // Ensure Group FS Chart exists.
        let grp_rec: RecordSet<AccountConsolidationGroup> = run
            .env()
            .records("account.consolidation.group")?
            .browse([group_id])?
            .try_into()?;
        let fs_count: i64 = tx
            .query(
                "SELECT count(*)::int8 FROM account_consolidation_account WHERE group_id = $1",
                &[&group_id],
            )
            .await?[0]
            .try_get(0)
            .map_err(db)?;
        if fs_count == 0 {
            AccountConsolidationGroup::action_bootstrap_default_chart(&grp_rec, tx).await?;
        }

        // Load units and authorize caller access to every member company.
        struct UnitInfo {
            company_id: i64,
            company_name: String,
            local_currency_id: i64,
            is_parent: bool,
            method: String,
            ownership_pct: Decimal,
            investment_account_id: Option<i64>,
        }
        let unit_rows = tx
            .query(
                "SELECT u.company_id::int8, c.name::text, c.currency_id::int8, \
                        COALESCE(u.is_parent, false), u.consolidation_method::text, \
                        u.ownership_pct, u.investment_account_id::int8 \
                 FROM account_consolidation_unit u \
                 JOIN res_company c ON c.id = u.company_id \
                 WHERE u.group_id = $1 ORDER BY u.is_parent DESC, u.id",
                &[&group_id],
            )
            .await?;
        if unit_rows.is_empty() {
            return Err(Error::user(
                "Add at least one Consolidation Unit (Company) to this group before executing.",
            ));
        }

        let allowed_companies: BTreeSet<i64> = tx
            .companies(run.env())
            .await?
            .into_iter()
            .map(|c| c.get())
            .collect();
        let mut units = Vec::with_capacity(unit_rows.len());
        let mut group_company_ids = BTreeSet::new();
        for urow in unit_rows {
            let cid: i64 = urow.try_get(0).map_err(db)?;
            if !run.env().su() && !allowed_companies.contains(&cid) {
                return Err(Error::access(format!(
                    "You do not have access to member company {cid} in this consolidation group."
                )));
            }
            group_company_ids.insert(cid);
            units.push(UnitInfo {
                company_id: cid,
                company_name: urow.try_get(1).map_err(db)?,
                local_currency_id: urow.try_get(2).map_err(db)?,
                is_parent: urow.try_get(3).map_err(db)?,
                method: urow.try_get(4).map_err(db)?,
                ownership_pct: urow.try_get(5).map_err(db)?,
                investment_account_id: urow.try_get(6).map_err(db)?,
            });
        }

        // Load Group FS items.
        #[derive(Clone)]
        struct FsItem {
            id: i64,
            code: String,
            section: String,
            rate_method: String,
            ic_role: String,
        }
        let fs_rows = tx
            .query(
                "SELECT id::int8, code::text, section::text, rate_method::text, ic_role::text \
                 FROM account_consolidation_account WHERE group_id = $1 ORDER BY sequence, code, id",
                &[&group_id],
            )
            .await?;
        let mut fs_by_id = BTreeMap::<i64, FsItem>::new();
        let mut fs_by_code = BTreeMap::<String, i64>::new();
        let mut fs_by_role = BTreeMap::<String, i64>::new();
        let mut fs_by_section = BTreeMap::<String, i64>::new();
        for r in fs_rows {
            let item = FsItem {
                id: r.try_get(0).map_err(db)?,
                code: r.try_get(1).map_err(db)?,
                section: r.try_get(2).map_err(db)?,
                rate_method: r.try_get(3).map_err(db)?,
                ic_role: r.try_get(4).map_err(db)?,
            };
            fs_by_code.insert(item.code.clone(), item.id);
            if item.ic_role != "none" {
                fs_by_role.entry(item.ic_role.clone()).or_insert(item.id);
            }
            fs_by_section.entry(item.section.clone()).or_insert(item.id);
            fs_by_id.insert(item.id, item);
        }

        let cta_fs_id = fs_by_section.get("equity_cta").copied().ok_or_else(|| {
            Error::validation("Group FS Chart requires an equity_cta (CTA) item.")
        })?;
        let nci_eq_fs_id = fs_by_section
            .get("equity_nci")
            .copied()
            .ok_or_else(|| Error::validation("Group FS Chart requires an equity_nci item."))?;
        let nci_pl_fs_id = fs_by_section
            .get("nci_profit")
            .copied()
            .ok_or_else(|| Error::validation("Group FS Chart requires an nci_profit item."))?;
        let goodwill_fs_id = fs_by_section.get("goodwill").copied().unwrap_or(cta_fs_id);
        let retained_fs_id = fs_by_section
            .get("equity_retained")
            .copied()
            .unwrap_or(cta_fs_id);
        let ic_var_fs_id = fs_by_role
            .get("ic_variance")
            .copied()
            .unwrap_or(retained_fs_id);
        let ic_ar_fs_id = fs_by_role.get("ic_receivable").copied();
        let ic_ap_fs_id = fs_by_role.get("ic_payable").copied();
        let ic_rev_fs_id = fs_by_role.get("ic_revenue").copied();
        let ic_exp_fs_id = fs_by_role.get("ic_expense").copied();

        // Load explicit entity account mappings.
        let explicit_map: BTreeMap<(i64, i64), i64> = tx
            .query(
                "SELECT company_id::int8, account_id::int8, consolidation_account_id::int8 \
                 FROM account_consolidation_account_map WHERE group_id = $1",
                &[&group_id],
            )
            .await?
            .into_iter()
            .map(|r| {
                Ok((
                    (r.try_get(0).map_err(db)?, r.try_get(1).map_err(db)?),
                    r.try_get(2).map_err(db)?,
                ))
            })
            .collect::<Result<_>>()?;

        // Clear any prior generated consolidation lines for this run (idempotent re-execution).
        tx.execute(
            "DELETE FROM account_consolidation_line WHERE run_id = $1",
            &[&run_id],
        )
        .await?;

        // Prepare currency rounding table for all unit currencies + group currency.
        let real_id = |id: i64| -> Result<RecId> {
            RecId::from_real(id).ok_or_else(|| Error::invariant(format!("Invalid record id {id}.")))
        };
        let mut all_currencies = BTreeSet::new();
        all_currencies.insert(real_id(group_currency_id)?);
        for u in &units {
            all_currencies.insert(real_id(u.local_currency_id)?);
        }
        let currency_vec: Vec<RecId> = all_currencies.into_iter().collect();
        let rounding_table = RoundingTable::load(run.env(), tx, &currency_vec).await?;

        struct OutLine {
            fs_id: i64,
            company_id: Option<i64>,
            trading_partner_company_id: Option<i64>,
            posting_level: &'static str,
            local_currency_id: i64,
            local_amount: Decimal,
            exchange_rate: Decimal,
            group_amount: Decimal,
            label: String,
        }
        let mut out_lines: Vec<OutLine> = Vec::new();

        // Per-unit accumulators for Stage 2 (Investment vs Equity) and Stage 4 (NCI).
        let mut unit_translated_pnl = BTreeMap::<i64, Decimal>::new(); // signed: credit negative => profit is -sum
        let mut unit_translated_bs_equity = BTreeMap::<i64, Decimal>::new(); // signed equity + cta
        let mut unit_share_capital_group = BTreeMap::<i64, (i64, Decimal)>::new(); // (fs_id, credit magnitude > 0)
        let mut parent_investment_total = Decimal::ZERO;
        let mut parent_investment_fs_id: Option<i64> = None;
        let mut parent_company_id: Option<i64> = None;

        // Intercompany pair buckets keyed by (min_co, max_co, kind) -> (debit_fs_id, debit_co, debit_mag, credit_fs_id, credit_co, credit_mag)
        let mut ic_ar_by_pair = BTreeMap::<(i64, i64), (i64, Decimal)>::new(); // (holder_co, tp_co) -> (fs_id, debit_mag)
        let mut ic_ap_by_pair = BTreeMap::<(i64, i64), (i64, Decimal)>::new(); // (holder_co, tp_co) -> (fs_id, credit_mag)
        let mut ic_rev_by_pair = BTreeMap::<(i64, i64), (i64, Decimal)>::new(); // (seller_co, buyer_co) -> (fs_id, credit_mag)
        let mut ic_exp_by_pair = BTreeMap::<(i64, i64), (i64, Decimal)>::new(); // (buyer_co, seller_co) -> (fs_id, debit_mag)

        let rate_company_id = units
            .iter()
            .find(|u| u.is_parent)
            .map(|u| u.company_id)
            .unwrap_or(units[0].company_id);

        // Helper to translate local_amount -> (group_amount, effective_rate).
        let group_cur_rec = real_id(group_currency_id)?;
        let rate_co_rec = real_id(rate_company_id)?;

        // Load RateBooks once for closing, average, historical, and spot fallback.
        let spot_book = RateBook::load(
            run.env(),
            tx,
            rate_co_rec,
            date_to,
            RateDateRule::OnOrBefore,
            &currency_vec,
        )
        .await?;
        let closing_book = RateBook::load_typed(
            run.env(),
            tx,
            rate_co_rec,
            date_to,
            &closing_code,
            &currency_vec,
        )
        .await
        .ok();
        let average_book = RateBook::load_typed(
            run.env(),
            tx,
            rate_co_rec,
            date_to,
            &average_code,
            &currency_vec,
        )
        .await
        .ok();
        let historical_book = RateBook::load_typed(
            run.env(),
            tx,
            rate_co_rec,
            date_to,
            &historical_code,
            &currency_vec,
        )
        .await
        .ok();

        let translate_amount = |local_cur_id: i64,
                                amount: Decimal,
                                rate_method: &str|
         -> Result<(Decimal, Decimal)> {
            if local_cur_id == group_currency_id || amount.is_zero() {
                return Ok((amount.round_dp(2), Decimal::ONE));
            }
            let from_cur = real_id(local_cur_id)?;
            let typed_book = match rate_method {
                "closing" => closing_book.as_ref(),
                "average" => average_book.as_ref(),
                "historical" => historical_book.as_ref(),
                _ => closing_book.as_ref(),
            };
            let snap = if let Some(book) = typed_book {
                match book.snapshot(from_cur, group_cur_rec) {
                    Ok(s) => s,
                    Err(_) if fallback_to_spot => spot_book.snapshot(from_cur, group_cur_rec)?,
                    Err(e) => return Err(e),
                }
            } else if fallback_to_spot {
                spot_book.snapshot(from_cur, group_cur_rec)?
            } else {
                return Err(Error::user(format!(
                    "Missing '{rate_method}' currency rate for conversion on {date_to}."
                )));
            };
            let exact = snap.convert_exact(amount)?;
            let converted = exact.quantize(
                rounding_table.get(group_cur_rec)?.increment(),
                CURRENCY_ROUNDING_MODE,
            )?;
            let eff_rate = (converted / amount).round_dp(6);
            Ok((converted.round_dp(2), eff_rate))
        };

        // =====================================================================
        // STAGE 1 (Posting Level 00): Local Extraction, Multi-Rate Translation & CTA
        // =====================================================================
        for unit in &units {
            if unit.is_parent {
                parent_company_id = Some(unit.company_id);
            }
            if unit.method == "equity" {
                // Equity method associates do not consolidate line-by-line (handled via manual or investment).
                continue;
            }
            let scale_factor = if unit.method == "proportional" {
                unit.ownership_pct / Decimal::from(100)
            } else {
                Decimal::ONE
            };

            // Read posted journal item balances for this unit:
            // - Balance sheet accounts (include_initial_balance = true): cumulative <= date_to
            // - P&L accounts (include_initial_balance = false): period [date_from, date_to]
            // - Prior-year P&L (date < date_from) rolls into Retained Earnings!
            let rows = tx
                .query(
                    "SELECT l.account_id::int8, a.account_type::text, \
                            COALESCE(a.include_initial_balance, true), \
                            COALESCE(l.trading_partner_company_id, p.trading_partner_company_id, rc.id)::int8, \
                            SUM(CASE WHEN COALESCE(a.include_initial_balance, true) THEN l.balance \
                                     WHEN l.date >= $2 THEN l.balance ELSE 0 END), \
                            SUM(CASE WHEN NOT COALESCE(a.include_initial_balance, true) AND l.date < $2 \
                                     THEN l.balance ELSE 0 END) \
                     FROM account_move_line l \
                     JOIN account_account a ON a.id = l.account_id \
                     LEFT JOIN res_partner p ON p.id = l.partner_id \
                     LEFT JOIN res_company rc ON rc.partner_id = l.partner_id \
                     WHERE l.company_id = $1 AND l.parent_state = 'posted' \
                       AND l.date <= $3 AND a.account_type <> 'off_balance' \
                     GROUP BY l.account_id, a.account_type, a.include_initial_balance, \
                              COALESCE(l.trading_partner_company_id, p.trading_partner_company_id, rc.id)",
                    &[&unit.company_id, &date_from, &date_to],
                )
                .await?;

            let mut unit_translated_sum = Decimal::ZERO;
            let mut prior_retained_local = Decimal::ZERO;

            for row in rows {
                let acc_id: i64 = row.try_get(0).map_err(db)?;
                let atype: String = row.try_get(1).map_err(db)?;
                let _is_bs: bool = row.try_get(2).map_err(db)?;
                let raw_tp: Option<i64> = row.try_get(3).map_err(db)?;
                let main_local: Decimal = row.try_get(4).map_err(db)?;
                let prior_pl_local: Decimal = row.try_get(5).map_err(db)?;

                prior_retained_local += prior_pl_local;
                if main_local.is_zero() {
                    continue;
                }
                let scaled_local = (main_local * scale_factor).round_dp(2);

                // Resolve Trading Partner only when it is another company inside this consolidation group.
                let tp_co =
                    raw_tp.filter(|tp| *tp != unit.company_id && group_company_ids.contains(tp));

                // Determine Group FS Item (auto-routing intercompany counterparties to IC FS items).
                let base_fs_id = if Some(acc_id) == unit.investment_account_id {
                    fs_by_role
                        .get("investment_in_sub")
                        .copied()
                        .or_else(|| fs_by_code.get("1550").copied())
                } else {
                    explicit_map
                        .get(&(unit.company_id, acc_id))
                        .copied()
                        .or_else(|| {
                            fs_by_code
                                .get(default_fs_code_for_account_type(&atype))
                                .copied()
                        })
                };
                let mut fs_id = base_fs_id.unwrap_or(retained_fs_id);

                if tp_co.is_some() {
                    if atype == "asset_receivable"
                        && let Some(id) = ic_ar_fs_id
                    {
                        fs_id = id;
                    } else if atype == "liability_payable"
                        && let Some(id) = ic_ap_fs_id
                    {
                        fs_id = id;
                    } else if matches!(atype.as_str(), "income" | "income_other")
                        && let Some(id) = ic_rev_fs_id
                    {
                        fs_id = id;
                    } else if matches!(
                        atype.as_str(),
                        "expense" | "expense_direct_cost" | "expense_other"
                    ) && let Some(id) = ic_exp_fs_id
                    {
                        fs_id = id;
                    }
                }

                let fs_item = fs_by_id
                    .get(&fs_id)
                    .ok_or_else(|| Error::invariant("Missing Group FS item in map"))?;
                let (grp_amt, eff_rate) =
                    translate_amount(unit.local_currency_id, scaled_local, &fs_item.rate_method)?;

                unit_translated_sum += grp_amt;

                // Track P&L vs Equity for NCI & Investment Elimination.
                if matches!(fs_item.section.as_str(), "income" | "cogs" | "expense") {
                    *unit_translated_pnl
                        .entry(unit.company_id)
                        .or_insert(Decimal::ZERO) += grp_amt;
                }
                if matches!(
                    fs_item.section.as_str(),
                    "equity_capital" | "equity_retained" | "equity_cta"
                ) {
                    *unit_translated_bs_equity
                        .entry(unit.company_id)
                        .or_insert(Decimal::ZERO) += grp_amt;
                }
                if fs_item.section == "equity_capital" && !unit.is_parent {
                    let entry = unit_share_capital_group
                        .entry(unit.company_id)
                        .or_insert((fs_id, Decimal::ZERO));
                    entry.1 += -grp_amt; // credit magnitude
                }
                if fs_item.ic_role == "investment_in_sub" && unit.is_parent {
                    parent_investment_total += grp_amt;
                    parent_investment_fs_id = Some(fs_id);
                }

                // Collect Intercompany Elimination buckets.
                if let Some(tp) = tp_co {
                    match fs_item.ic_role.as_str() {
                        "ic_receivable" if grp_amt > Decimal::ZERO => {
                            let e = ic_ar_by_pair
                                .entry((unit.company_id, tp))
                                .or_insert((fs_id, Decimal::ZERO));
                            e.1 += grp_amt;
                        }
                        "ic_payable" if grp_amt < Decimal::ZERO => {
                            let e = ic_ap_by_pair
                                .entry((unit.company_id, tp))
                                .or_insert((fs_id, Decimal::ZERO));
                            e.1 += -grp_amt;
                        }
                        "ic_revenue" if grp_amt < Decimal::ZERO => {
                            let e = ic_rev_by_pair
                                .entry((unit.company_id, tp))
                                .or_insert((fs_id, Decimal::ZERO));
                            e.1 += -grp_amt;
                        }
                        "ic_expense" if grp_amt > Decimal::ZERO => {
                            let e = ic_exp_by_pair
                                .entry((unit.company_id, tp))
                                .or_insert((fs_id, Decimal::ZERO));
                            e.1 += grp_amt;
                        }
                        _ => {}
                    }
                }

                out_lines.push(OutLine {
                    fs_id,
                    company_id: Some(unit.company_id),
                    trading_partner_company_id: tp_co,
                    posting_level: "00_translated",
                    local_currency_id: unit.local_currency_id,
                    local_amount: scaled_local,
                    exchange_rate: eff_rate,
                    group_amount: grp_amt,
                    label: format!("{} ({})", unit.company_name, fs_item.code),
                });
            }

            // Prior-year P&L rolls into Retained Earnings at historical rate.
            if !prior_retained_local.is_zero() {
                let scaled_prior = (prior_retained_local * scale_factor).round_dp(2);
                let (grp_amt, eff_rate) =
                    translate_amount(unit.local_currency_id, scaled_prior, "historical")?;
                unit_translated_sum += grp_amt;
                *unit_translated_bs_equity
                    .entry(unit.company_id)
                    .or_insert(Decimal::ZERO) += grp_amt;
                out_lines.push(OutLine {
                    fs_id: retained_fs_id,
                    company_id: Some(unit.company_id),
                    trading_partner_company_id: None,
                    posting_level: "00_translated",
                    local_currency_id: unit.local_currency_id,
                    local_amount: scaled_prior,
                    exchange_rate: eff_rate,
                    group_amount: grp_amt,
                    label: format!("{} Prior-Year Retained Earnings", unit.company_name),
                });
            }

            // Compute exact CTA balancing plug for foreign-currency subsidiaries.
            let cta = compute_cta_plug(unit_translated_sum);
            if !cta.is_zero() {
                *unit_translated_bs_equity
                    .entry(unit.company_id)
                    .or_insert(Decimal::ZERO) += cta;
                out_lines.push(OutLine {
                    fs_id: cta_fs_id,
                    company_id: Some(unit.company_id),
                    trading_partner_company_id: None,
                    posting_level: "00_translated",
                    local_currency_id: unit.local_currency_id,
                    local_amount: Decimal::ZERO,
                    exchange_rate: Decimal::ONE,
                    group_amount: cta,
                    label: format!("{} Translation Reserve (CTA)", unit.company_name),
                });
            }
        }

        // =====================================================================
        // STAGE 2 (Posting Level 20): Automatic Intercompany & Investment Eliminations
        // =====================================================================
        // 2a. IC Receivable vs IC Payable Elimination
        let mut ar_ap_pairs = BTreeSet::new();
        for &(a, b) in ic_ar_by_pair.keys() {
            ar_ap_pairs.insert((a, b));
        }
        for &(b, a) in ic_ap_by_pair.keys() {
            ar_ap_pairs.insert((a, b));
        }
        for (creditor_co, debtor_co) in ar_ap_pairs {
            let (ar_fs, ar_mag) = ic_ar_by_pair
                .get(&(creditor_co, debtor_co))
                .copied()
                .unwrap_or((ic_ar_fs_id.unwrap_or(retained_fs_id), Decimal::ZERO));
            let (ap_fs, ap_mag) = ic_ap_by_pair
                .get(&(debtor_co, creditor_co))
                .copied()
                .unwrap_or((ic_ap_fs_id.unwrap_or(retained_fs_id), Decimal::ZERO));
            let (matched, variance) = pair_ic_balances(ar_mag, ap_mag);
            if matched > Decimal::ZERO {
                // Eliminate AR (credit = -matched) & AP (debit = +matched)
                out_lines.push(OutLine {
                    fs_id: ar_fs,
                    company_id: Some(creditor_co),
                    trading_partner_company_id: Some(debtor_co),
                    posting_level: "20_ic_elimination",
                    local_currency_id: group_currency_id,
                    local_amount: -matched,
                    exchange_rate: Decimal::ONE,
                    group_amount: -matched,
                    label: format!("IC AR Elimination (Co {creditor_co} vs Co {debtor_co})"),
                });
                out_lines.push(OutLine {
                    fs_id: ap_fs,
                    company_id: Some(debtor_co),
                    trading_partner_company_id: Some(creditor_co),
                    posting_level: "20_ic_elimination",
                    local_currency_id: group_currency_id,
                    local_amount: matched,
                    exchange_rate: Decimal::ONE,
                    group_amount: matched,
                    label: format!("IC AP Elimination (Co {debtor_co} vs Co {creditor_co})"),
                });
            }
            if !variance.is_zero() {
                // Eliminate remaining un-paired IC balance to explicit IC Elimination Variance
                if variance > Decimal::ZERO {
                    out_lines.push(OutLine {
                        fs_id: ar_fs,
                        company_id: Some(creditor_co),
                        trading_partner_company_id: Some(debtor_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: -variance,
                        exchange_rate: Decimal::ONE,
                        group_amount: -variance,
                        label: format!(
                            "IC AR Variance Clearing (Co {creditor_co} vs Co {debtor_co})"
                        ),
                    });
                    out_lines.push(OutLine {
                        fs_id: ic_var_fs_id,
                        company_id: Some(creditor_co),
                        trading_partner_company_id: Some(debtor_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: variance,
                        exchange_rate: Decimal::ONE,
                        group_amount: variance,
                        label: format!(
                            "IC AR/AP Reconciliation Variance (Co {creditor_co} vs Co {debtor_co})"
                        ),
                    });
                } else {
                    let abs_v = variance.abs();
                    out_lines.push(OutLine {
                        fs_id: ap_fs,
                        company_id: Some(debtor_co),
                        trading_partner_company_id: Some(creditor_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: abs_v,
                        exchange_rate: Decimal::ONE,
                        group_amount: abs_v,
                        label: format!(
                            "IC AP Variance Clearing (Co {debtor_co} vs Co {creditor_co})"
                        ),
                    });
                    out_lines.push(OutLine {
                        fs_id: ic_var_fs_id,
                        company_id: Some(debtor_co),
                        trading_partner_company_id: Some(creditor_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: -abs_v,
                        exchange_rate: Decimal::ONE,
                        group_amount: -abs_v,
                        label: format!(
                            "IC AR/AP Reconciliation Variance (Co {debtor_co} vs Co {creditor_co})"
                        ),
                    });
                }
            }
        }

        // 2b. IC Revenue vs IC Cost/Expense Elimination
        let mut rev_exp_pairs = BTreeSet::new();
        for &(seller, buyer) in ic_rev_by_pair.keys() {
            rev_exp_pairs.insert((seller, buyer));
        }
        for &(buyer, seller) in ic_exp_by_pair.keys() {
            rev_exp_pairs.insert((seller, buyer));
        }
        for (seller_co, buyer_co) in rev_exp_pairs {
            let (rev_fs, rev_mag) = ic_rev_by_pair
                .get(&(seller_co, buyer_co))
                .copied()
                .unwrap_or((ic_rev_fs_id.unwrap_or(retained_fs_id), Decimal::ZERO));
            let (exp_fs, exp_mag) = ic_exp_by_pair
                .get(&(buyer_co, seller_co))
                .copied()
                .unwrap_or((ic_exp_fs_id.unwrap_or(retained_fs_id), Decimal::ZERO));
            let (matched, _var) = pair_ic_balances(exp_mag, rev_mag);
            if matched > Decimal::ZERO {
                // Eliminate IC Revenue (debit = +matched) & IC Expense (credit = -matched)
                out_lines.push(OutLine {
                    fs_id: rev_fs,
                    company_id: Some(seller_co),
                    trading_partner_company_id: Some(buyer_co),
                    posting_level: "20_ic_elimination",
                    local_currency_id: group_currency_id,
                    local_amount: matched,
                    exchange_rate: Decimal::ONE,
                    group_amount: matched,
                    label: format!("IC Revenue Elimination (Co {seller_co} -> Co {buyer_co})"),
                });
                out_lines.push(OutLine {
                    fs_id: exp_fs,
                    company_id: Some(buyer_co),
                    trading_partner_company_id: Some(seller_co),
                    posting_level: "20_ic_elimination",
                    local_currency_id: group_currency_id,
                    local_amount: -matched,
                    exchange_rate: Decimal::ONE,
                    group_amount: -matched,
                    label: format!("IC Expense Elimination (Co {buyer_co} <- Co {seller_co})"),
                });
            }
        }

        // 2c. Parent Investment in Subsidiary vs Subsidiary Share Capital Elimination + Goodwill
        if parent_investment_total > Decimal::ZERO
            && let Some(inv_fs) = parent_investment_fs_id
        {
            let mut total_parent_share_cap = Decimal::ZERO;
            for unit in &units {
                if unit.is_parent || unit.method != "full" {
                    continue;
                }
                if let Some(&(cap_fs, cap_mag)) = unit_share_capital_group.get(&unit.company_id)
                    && cap_mag > Decimal::ZERO
                {
                    let parent_portion =
                        (cap_mag * unit.ownership_pct / Decimal::from(100)).round_dp(2);
                    if parent_portion > Decimal::ZERO {
                        total_parent_share_cap += parent_portion;
                        // Debit Subsidiary Share Capital (eliminate parent's share)
                        out_lines.push(OutLine {
                            fs_id: cap_fs,
                            company_id: Some(unit.company_id),
                            trading_partner_company_id: parent_company_id,
                            posting_level: "20_ic_elimination",
                            local_currency_id: group_currency_id,
                            local_amount: parent_portion,
                            exchange_rate: Decimal::ONE,
                            group_amount: parent_portion,
                            label: format!(
                                "Subsidiary Share Capital Elimination ({}, {}%)",
                                unit.company_name, unit.ownership_pct
                            ),
                        });
                    }
                }
            }
            if total_parent_share_cap > Decimal::ZERO {
                // Credit Parent Investment in Subsidiary
                out_lines.push(OutLine {
                    fs_id: inv_fs,
                    company_id: parent_company_id,
                    trading_partner_company_id: None,
                    posting_level: "20_ic_elimination",
                    local_currency_id: group_currency_id,
                    local_amount: -parent_investment_total,
                    exchange_rate: Decimal::ONE,
                    group_amount: -parent_investment_total,
                    label: "Investment in Subsidiaries Elimination".to_owned(),
                });
                let goodwill = (parent_investment_total - total_parent_share_cap).round_dp(2);
                if !goodwill.is_zero() {
                    let target_fs = if goodwill > Decimal::ZERO {
                        goodwill_fs_id
                    } else {
                        retained_fs_id
                    };
                    out_lines.push(OutLine {
                        fs_id: target_fs,
                        company_id: parent_company_id,
                        trading_partner_company_id: None,
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: goodwill,
                        exchange_rate: Decimal::ONE,
                        group_amount: goodwill,
                        label: if goodwill > Decimal::ZERO {
                            "Goodwill on Consolidation".to_owned()
                        } else {
                            "Bargain Purchase / Consolidation Reserve".to_owned()
                        },
                    });
                }
            }
        }

        // =====================================================================
        // STAGE 3 (Posting Level 10): Posted Manual / Top-Side Adjustments
        // =====================================================================
        let adj_rows = tx
            .query(
                "SELECT a.name::text, l.consolidation_account_id::int8, l.company_id::int8, \
                        COALESCE(l.amount, 0), COALESCE(l.name, a.name)::text \
                 FROM account_consolidation_adjustment_line l \
                 JOIN account_consolidation_adjustment a ON a.id = l.adjustment_id \
                 WHERE a.run_id = $1 AND a.state = 'posted' \
                 ORDER BY a.id, l.id",
                &[&run_id],
            )
            .await?;
        for arow in adj_rows {
            let fs_id: i64 = arow.try_get(1).map_err(db)?;
            let co_id: Option<i64> = arow.try_get(2).map_err(db)?;
            let amt: Decimal = arow.try_get(3).map_err(db)?;
            let lbl: String = arow.try_get(4).map_err(db)?;
            if amt.is_zero() {
                continue;
            }
            out_lines.push(OutLine {
                fs_id,
                company_id: co_id,
                trading_partner_company_id: None,
                posting_level: "10_manual_adjustment",
                local_currency_id: group_currency_id,
                local_amount: amt,
                exchange_rate: Decimal::ONE,
                group_amount: amt,
                label: format!("Adjustment: {lbl}"),
            });
        }

        // =====================================================================
        // STAGE 4 (Posting Level 30): Non-Controlling Interest (NCI) Carve-Out
        // =====================================================================
        let mut total_nci_profit = Decimal::ZERO;
        let mut total_nci_equity = Decimal::ZERO;

        for unit in &units {
            if unit.is_parent || unit.method != "full" || unit.ownership_pct >= Decimal::from(100) {
                continue;
            }
            // Subsidiary translated profit (credit-negative in accounting -> flip sign for profit)
            let sub_pnl_signed = unit_translated_pnl
                .get(&unit.company_id)
                .copied()
                .unwrap_or(Decimal::ZERO);
            let sub_profit = -sub_pnl_signed;
            let nci_profit_share = compute_nci_share(sub_profit, unit.ownership_pct);
            if !nci_profit_share.is_zero() {
                total_nci_profit += nci_profit_share;
                // Carve out NCI Profit: Dr NCI Share of Profit (+), Cr NCI Equity (-)
                // (On consolidated P&L, NCI Share of Profit reduces Parent Attributable Profit;
                //  on consolidated BS, Retained/Current Earnings is reduced by NCI profit which moves into NCI Equity!)
                out_lines.push(OutLine {
                    fs_id: nci_pl_fs_id,
                    company_id: Some(unit.company_id),
                    trading_partner_company_id: None,
                    posting_level: "30_nci",
                    local_currency_id: group_currency_id,
                    local_amount: nci_profit_share,
                    exchange_rate: Decimal::ONE,
                    group_amount: nci_profit_share,
                    label: format!(
                        "NCI Profit Carve-Out ({}, {}% NCI)",
                        unit.company_name,
                        Decimal::from(100) - unit.ownership_pct
                    ),
                });
                out_lines.push(OutLine {
                    fs_id: nci_eq_fs_id,
                    company_id: Some(unit.company_id),
                    trading_partner_company_id: None,
                    posting_level: "30_nci",
                    local_currency_id: group_currency_id,
                    local_amount: -nci_profit_share,
                    exchange_rate: Decimal::ONE,
                    group_amount: -nci_profit_share,
                    label: format!("NCI Current-Year Profit to Equity ({})", unit.company_name),
                });
                total_nci_equity += nci_profit_share;
            }

            // Subsidiary net equity (Share Capital + Retained Earnings + CTA)
            let sub_eq_signed = unit_translated_bs_equity
                .get(&unit.company_id)
                .copied()
                .unwrap_or(Decimal::ZERO);
            let sub_eq_credit = -sub_eq_signed;
            let nci_eq_share = compute_nci_share(sub_eq_credit, unit.ownership_pct);
            if !nci_eq_share.is_zero() {
                total_nci_equity += nci_eq_share;
                // Reclassify NCI portion from Retained/Share Capital -> NCI Equity
                let src_eq_fs = unit_share_capital_group
                    .get(&unit.company_id)
                    .map(|(fs, _)| *fs)
                    .unwrap_or(retained_fs_id);
                out_lines.push(OutLine {
                    fs_id: src_eq_fs,
                    company_id: Some(unit.company_id),
                    trading_partner_company_id: None,
                    posting_level: "30_nci",
                    local_currency_id: group_currency_id,
                    local_amount: nci_eq_share,
                    exchange_rate: Decimal::ONE,
                    group_amount: nci_eq_share,
                    label: format!("NCI Equity Reclassification ({})", unit.company_name),
                });
                out_lines.push(OutLine {
                    fs_id: nci_eq_fs_id,
                    company_id: Some(unit.company_id),
                    trading_partner_company_id: None,
                    posting_level: "30_nci",
                    local_currency_id: group_currency_id,
                    local_amount: -nci_eq_share,
                    exchange_rate: Decimal::ONE,
                    group_amount: -nci_eq_share,
                    label: format!(
                        "Non-Controlling Interest in Equity ({}, {}% NCI)",
                        unit.company_name,
                        Decimal::from(100) - unit.ownership_pct
                    ),
                });
            }
        }

        // Persist all consolidation lines and compute KPI summaries.
        let mut vals_list = Vec::with_capacity(out_lines.len());
        let mut total_assets = Decimal::ZERO;
        let mut total_liab_eq_signed = Decimal::ZERO; // credit-negative -> flip sign
        let mut total_cta_signed = Decimal::ZERO;
        let mut total_pnl_signed = Decimal::ZERO;

        for ol in &out_lines {
            let fs = fs_by_id
                .get(&ol.fs_id)
                .ok_or_else(|| Error::invariant("Missing FS item"))?;
            match fs.section.as_str() {
                "asset_current" | "asset_non_current" | "goodwill" => {
                    total_assets += ol.group_amount;
                }
                "liability_current"
                | "liability_non_current"
                | "equity_capital"
                | "equity_retained"
                | "equity_cta"
                | "equity_nci" => {
                    total_liab_eq_signed += ol.group_amount;
                    if fs.section == "equity_cta" {
                        total_cta_signed += ol.group_amount;
                    }
                }
                "income" | "cogs" | "expense" | "nci_profit" => {
                    // Net income attributable to Parent flows into Consolidated Equity!
                    total_liab_eq_signed += ol.group_amount;
                    if fs.section != "nci_profit" {
                        total_pnl_signed += ol.group_amount;
                    }
                }
                _ => {}
            }

            let mut v = RawVals::new();
            v.insert("run_id".into(), Cell::Id(run_id));
            v.insert("consolidation_account_id".into(), Cell::Id(ol.fs_id));
            if let Some(cid) = ol.company_id {
                v.insert("company_id".into(), Cell::Id(cid));
            }
            if let Some(tp) = ol.trading_partner_company_id {
                v.insert("trading_partner_company_id".into(), Cell::Id(tp));
            }
            v.insert("posting_level".into(), Cell::Str(ol.posting_level.into()));
            v.insert("local_currency_id".into(), Cell::Id(ol.local_currency_id));
            v.insert("local_amount".into(), Cell::Decimal(ol.local_amount));
            v.insert("exchange_rate".into(), Cell::Decimal(ol.exchange_rate));
            v.insert("group_amount".into(), Cell::Decimal(ol.group_amount));
            v.insert("label".into(), Cell::Str(ol.label.as_str().into()));
            vals_list.push(v);
        }

        if !vals_list.is_empty() {
            let clean_env = run.env().with_context_replaced(erp_sdk::Context::default());
            clean_env
                .records("account.consolidation.line")?
                .create(tx, vals_list)
                .await?;
        }

        let total_liabilities_equity = (-total_liab_eq_signed).round_dp(2);
        let total_assets = total_assets.round_dp(2);
        let balance_check_diff = (total_assets - total_liabilities_equity).round_dp(2);
        let total_cta = (-total_cta_signed).round_dp(2);
        let group_net_profit = (-total_pnl_signed).round_dp(2);

        tx.execute(
            "UPDATE account_consolidation_run \
             SET state = 'executed', total_assets = $2, total_liabilities_equity = $3, \
                 balance_check_diff = $4, total_cta = $5, total_nci_equity = $6, \
                 total_nci_profit = $7, group_net_profit = $8, \
                 write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[
                &run_id,
                &total_assets,
                &total_liabilities_equity,
                &balance_check_diff,
                &total_cta,
                &total_nci_equity,
                &total_nci_profit,
                &group_net_profit,
            ],
        )
        .await?;
        tx.invalidate_records(
            run.as_records(),
            Some(&[
                "state",
                "total_assets",
                "total_liabilities_equity",
                "balance_check_diff",
                "total_cta",
                "total_nci_equity",
                "total_nci_profit",
                "group_net_profit",
                "line_ids",
            ]),
        )
        .await
    }

    /// Locks an executed consolidation run after verifying exact double-entry balance (`diff == 0.00`).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_lock_run(
        this: &RecordSet<AccountConsolidationRun>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let run = this.ensure_one()?;
        run.as_records().check_access(tx, Operation::Write).await?;
        let state = run.get(tx, AccountConsolidationRun::STATE).await?;
        if state.as_deref() != Some("executed") {
            return Err(Error::user(
                "Execute the consolidation run before locking it.",
            ));
        }
        let diff = run
            .get(tx, AccountConsolidationRun::BALANCE_CHECK_DIFF)
            .await?;
        if !diff.is_zero() {
            return Err(Error::user(format!(
                "Cannot lock consolidation run: Consolidated Balance Sheet is out of balance by {diff}."
            )));
        }
        let id = run.as_records().ids()[0];
        tx.execute(
            "UPDATE account_consolidation_run SET state = 'locked', write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
            &[&id],
        )
        .await?;
        tx.invalidate_records(run.as_records(), Some(&["state"]))
            .await
    }
}

/// `account.consolidation.adjustment`: top-side manual consolidation journal entry (e.g.
/// unrealized profit in inventory elimination, PPA amortization).
#[model(
    name = "account.consolidation.adjustment",
    description = "Consolidation Top-Side Adjustment",
    order = "date desc, id desc"
)]
pub struct AccountConsolidationAdjustment {
    /// Adjustment title.
    #[field(char, required = true, string = "Reference")]
    pub name: Option<Arc<str>>,
    /// Consolidation run.
    #[field(
        many2one,
        comodel = "account.consolidation.run",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Consolidation Run"
    )]
    pub run_id: Option<RecId>,
    /// Group currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "run_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Adjustment date.
    #[field(date, required = true, default_fn = default_today, string = "Date")]
    pub date: Option<Date>,
    /// Memo / audit note.
    #[field(text, string = "Memo")]
    pub memo: Option<Arc<str>>,
    /// State (`draft` -> `posted`).
    #[field(
        selection,
        selection = [("draft", "Draft"), ("posted", "Posted")],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Balanced adjustment lines.
    #[field(
        one2many,
        comodel = "account.consolidation.adjustment.line",
        inverse_name = "adjustment_id",
        string = "Adjustment Lines"
    )]
    pub line_ids: Vec<RecId>,
}

#[methods(AccountConsolidationAdjustment, addon = "account")]
impl AccountConsolidationAdjustment {
    /// Validates exact debit == credit balance (`sum(amount) == 0.00`) and posts the adjustment.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_post_adjustment(
        this: &RecordSet<AccountConsolidationAdjustment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let adj = this.ensure_one()?;
        adj.as_records().check_access(tx, Operation::Write).await?;
        let aid = adj.as_records().ids()[0];
        let state = adj.get(tx, AccountConsolidationAdjustment::STATE).await?;
        if state.as_deref() != Some("draft") {
            return Err(Error::user(
                "This consolidation adjustment is already posted.",
            ));
        }
        let rows = tx
            .query(
                "SELECT count(*)::int8, COALESCE(SUM(amount), 0) \
                 FROM account_consolidation_adjustment_line WHERE adjustment_id = $1",
                &[&aid],
            )
            .await?;
        let count: i64 = rows[0].try_get(0).map_err(db)?;
        let net: Decimal = rows[0].try_get(1).map_err(db)?;
        if count < 2 {
            return Err(Error::user(
                "A consolidation adjustment requires at least two balancing lines.",
            ));
        }
        if !net.round_dp(2).is_zero() {
            return Err(Error::user(format!(
                "Consolidation adjustment is unbalanced by {net} (debit positive, credit negative must sum to 0.00)."
            )));
        }
        tx.execute(
            "UPDATE account_consolidation_adjustment SET state = 'posted', write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
            &[&aid],
        )
        .await?;
        tx.invalidate_records(adj.as_records(), Some(&["state"]))
            .await
    }
}

/// `account.consolidation.adjustment.line`: one line of a top-side consolidation adjustment.
#[model(
    name = "account.consolidation.adjustment.line",
    description = "Consolidation Adjustment Line",
    order = "id",
    company_access = unrestricted(reason = "Adjustment lines inherit scope from their parent consolidation run.")
)]
pub struct AccountConsolidationAdjustmentLine {
    /// Parent adjustment.
    #[field(
        many2one,
        comodel = "account.consolidation.adjustment",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Adjustment"
    )]
    pub adjustment_id: Option<RecId>,
    /// Group currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "adjustment_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Target Group FS Item.
    #[field(
        many2one,
        comodel = "account.consolidation.account",
        required = true,
        ondelete = "restrict",
        string = "Group FS Item"
    )]
    pub consolidation_account_id: Option<RecId>,
    /// Optional member entity attributed.
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "set null",
        string = "Entity"
    )]
    pub company_id: Option<RecId>,
    /// Line description.
    #[field(char, string = "Description")]
    pub name: Option<Arc<str>>,
    /// Signed amount in Group Currency (debit positive, credit negative).
    #[field(monetary, currency_field = "currency_id", string = "Signed Amount")]
    pub amount: Decimal,
}

#[methods(AccountConsolidationAdjustmentLine, addon = "account")]
impl AccountConsolidationAdjustmentLine {}

/// `account.consolidation.line`: one persisted line of a Consolidation Run tagged by Posting Level
/// (`00_translated`, `10_manual_adjustment`, `20_ic_elimination`, `30_nci`).
#[model(
    name = "account.consolidation.line",
    description = "Consolidation Ledger Line",
    order = "consolidation_account_id, posting_level, id",
    company_access = unrestricted(reason = "Consolidation lines are generated from authorized unit reads inside a consolidation run."),
    indexes = [
        ("_run_level", "(run_id, posting_level)"),
        ("_run_account", "(run_id, consolidation_account_id)"),
    ]
)]
pub struct AccountConsolidationLine {
    /// Parent consolidation run.
    #[field(
        many2one,
        comodel = "account.consolidation.run",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Consolidation Run"
    )]
    pub run_id: Option<RecId>,
    /// Consolidation group.
    #[field(
        many2one,
        comodel = "account.consolidation.group",
        related = "run_id.group_id",
        store = true,
        string = "Group"
    )]
    pub group_id: Option<RecId>,
    /// Group currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "run_id.currency_id",
        store = true,
        string = "Group Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Group FS Item.
    #[field(
        many2one,
        comodel = "account.consolidation.account",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Group FS Item"
    )]
    pub consolidation_account_id: Option<RecId>,
    /// Source legal entity.
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "set null",
        string = "Entity"
    )]
    pub company_id: Option<RecId>,
    /// Counterpart Trading Partner company (when intercompany).
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "set null",
        string = "Trading Partner"
    )]
    pub trading_partner_company_id: Option<RecId>,
    /// Consolidation posting level.
    #[field(
        selection,
        selection = [
            ("00_translated", "PL 00 — Translated Entity Data"),
            ("10_manual_adjustment", "PL 10 — Manual Top-Side Adjustment"),
            ("20_ic_elimination", "PL 20 — Intercompany Elimination"),
            ("30_nci", "PL 30 — Non-Controlling Interest (NCI)"),
        ],
        required = true,
        index = true,
        string = "Posting Level"
    )]
    pub posting_level: Option<Arc<str>>,
    /// Source local currency.
    #[field(
        many2one,
        comodel = "res.currency",
        ondelete = "restrict",
        string = "Local Currency"
    )]
    pub local_currency_id: Option<RecId>,
    /// Local currency balance before translation.
    #[field(
        monetary,
        currency_field = "local_currency_id",
        string = "Local Amount"
    )]
    pub local_amount: Decimal,
    /// Effective FX rate applied (`group / local`).
    #[field(float, digits = (16, 6), default = 1, string = "FX Rate")]
    pub exchange_rate: Decimal,
    /// Signed amount in Group Currency (debit positive, credit negative).
    #[field(monetary, currency_field = "currency_id", string = "Group Amount")]
    pub group_amount: Decimal,
    /// Audit label.
    #[field(char, string = "Label")]
    pub label: Option<Arc<str>>,
}

#[methods(AccountConsolidationLine, addon = "account")]
impl AccountConsolidationLine {}

#[cfg(test)]
mod tests {
    use super::{compute_cta_plug, compute_nci_share, pair_ic_balances};
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn multi_rate_translation_cta_restores_exact_balance_sheet_identity() {
        // Subsidiary IDR balances translated to USD:
        // Assets at Closing (1/15000): Cash 150,000,000 IDR => +10,000.00 USD
        // Share Capital at Historical (1/14000): -100,000,000 IDR => -7,142.86 USD
        // Net Profit at Average (1/14500): -50,000,000 IDR => -3,448.28 USD
        let assets = Decimal::from_str("10000.00").unwrap();
        let equity_cap = Decimal::from_str("-7142.86").unwrap();
        let pnl = Decimal::from_str("-3448.28").unwrap();
        let cta = compute_cta_plug(assets + equity_cap + pnl);
        assert_eq!(cta, Decimal::from_str("591.14").unwrap());
        assert_eq!(assets + equity_cap + pnl + cta, Decimal::ZERO);
    }

    #[test]
    fn nci_carves_out_exact_20_percent_on_80_percent_subsidiary() {
        let sub_profit = Decimal::from_str("5000.00").unwrap();
        let nci = compute_nci_share(sub_profit, Decimal::from_str("80.00").unwrap());
        assert_eq!(nci, Decimal::from_str("1000.00").unwrap());
        assert_eq!(sub_profit - nci, Decimal::from_str("4000.00").unwrap());
    }

    #[test]
    fn ic_pairing_eliminates_matched_and_isolates_fx_variance() {
        let (matched, variance) = pair_ic_balances(
            Decimal::from_str("1025.00").unwrap(),
            Decimal::from_str("1000.00").unwrap(),
        );
        assert_eq!(matched, Decimal::from_str("1000.00").unwrap());
        assert_eq!(variance, Decimal::from_str("25.00").unwrap());
    }
}
