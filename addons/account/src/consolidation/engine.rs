// SPDX-License-Identifier: MIT OR Apache-2.0
//! Consolidation Execution Monitor (`account.consolidation.run`), Top-Side Group Adjustments
//! (`account.consolidation.adjustment`), and Posting-Level Lines (`account.consolidation.line`):
//! 4-stage Multi-Rate Translation + CTA, IC Eliminations, Manual Adjustments, and NCI Carve-Out.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RateBook, RoundingTable};
use erp_sdk::money::{Increment, RateDateRule};
use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation};

use super::{
    AccountConsolidationGroup, db, default_fs_code_for_account_type, ensure_group_company_access,
    strip_default_context,
};

fn quantize_inc(value: Decimal, inc: &Increment) -> Result<Decimal> {
    Ok(inc.quantize(value, CURRENCY_ROUNDING_MODE)?)
}

async fn resolve_default_today(tx: &mut Transaction, env: &Env) -> Result<Date> {
    if let Some(cid) = env.company_id()? {
        erp_addon_base::company_calendar::business_date(tx, cid).await
    } else {
        env.context_today()
    }
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

/// Currency-increment-aware NCI share calculation.
pub fn compute_nci_share_inc(
    subsidiary_amount: Decimal,
    ownership_pct: Decimal,
    inc: &Increment,
) -> Result<Decimal> {
    let nci_pct = (Decimal::from(100) - ownership_pct).max(Decimal::ZERO);
    quantize_inc(subsidiary_amount * nci_pct / Decimal::from(100), inc)
}

/// Pairs an intercompany debit balance (`ar_or_exp > 0`) with an intercompany credit balance
/// (`ap_or_rev < 0` expressed as positive magnitude `credit_mag > 0`), returning
/// `(matched_eliminated, net_variance)` where `net_variance = debit_mag - credit_mag`.
pub fn pair_ic_balances(debit_mag: Decimal, credit_mag: Decimal) -> (Decimal, Decimal) {
    let matched = debit_mag.min(credit_mag).max(Decimal::ZERO).round_dp(2);
    let variance = (debit_mag - credit_mag).round_dp(2);
    (matched, variance)
}

/// Currency-increment-aware IC pairing supporting signed (including contra/credit-note) net balances.
pub fn pair_ic_balances_inc(
    debit_net: Decimal,
    credit_net: Decimal,
    inc: &Increment,
) -> Result<(Decimal, Decimal)> {
    let matched = if debit_net > Decimal::ZERO && credit_net > Decimal::ZERO {
        quantize_inc(debit_net.min(credit_net), inc)?
    } else if debit_net < Decimal::ZERO && credit_net < Decimal::ZERO {
        // Both sides hold contra/credit-note net balances -> match the shared contra magnitude.
        quantize_inc(debit_net.max(credit_net), inc)?
    } else {
        Decimal::ZERO
    };
    let variance = quantize_inc(debit_net - credit_net, inc)?;
    Ok((matched, variance))
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
    #[field(date, required = true, string = "Period Start")]
    pub date_from: Option<Date>,
    /// Balance Sheet & P&L period end date (inclusive).
    #[field(date, required = true, string = "Period End")]
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
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountConsolidationRun>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let need_from = names.iter().any(|n| n == "date_from") && !values.contains_key("date_from");
        let need_to = names.iter().any(|n| n == "date_to") && !values.contains_key("date_to");
        if need_from || need_to {
            let today = resolve_default_today(tx, this.env()).await?;
            if need_from {
                let (y, _, _) = today.ymd();
                let start = Date::from_ymd(y, 1, 1)
                    .ok_or_else(|| Error::invariant("Invalid year start date."))?;
                values.insert("date_from".to_owned(), Value::Date(start));
            }
            if need_to {
                values.insert("date_to".to_owned(), Value::Date(today));
            }
        }
        Ok(values)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<AccountConsolidationRun>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if !this.env().su() {
            for vals in &vals_list {
                if let Some(Cell::Str(st)) = vals.get("state")
                    && st.as_ref() != "draft"
                {
                    return Err(Error::validation(
                        "Consolidation runs must start in Draft status.",
                    ));
                }
                if let Some(Cell::Id(gid)) = vals.get("group_id") {
                    ensure_group_company_access(tx, this.env(), *gid).await?;
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountConsolidationRun>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            if vals.contains("state") {
                return Err(Error::validation(
                    "Consolidation run status must be changed through Execute or Lock actions.",
                ));
            }
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let rows = tx
                    .query(
                        "SELECT id::int8, group_id::int8, state::text \
                         FROM account_consolidation_run WHERE id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                let mut gids = BTreeSet::new();
                for r in rows {
                    let gid: i64 = r.try_get(1).map_err(db)?;
                    let st: String = r.try_get(2).map_err(db)?;
                    gids.insert(gid);
                    if st == "locked" {
                        return Err(Error::user(
                            "A locked consolidation run cannot be modified.",
                        ));
                    }
                }
                if let Some(Cell::Id(new_gid)) = vals.get("group_id") {
                    gids.insert(*new_gid);
                }
                for gid in gids {
                    ensure_group_company_access(tx, this.env(), gid).await?;
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountConsolidationRun>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let rows = tx
                    .query(
                        "SELECT group_id::int8, state::text \
                         FROM account_consolidation_run WHERE id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                let mut gids = BTreeSet::new();
                for r in rows {
                    let gid: i64 = r.try_get(0).map_err(db)?;
                    let st: String = r.try_get(1).map_err(db)?;
                    gids.insert(gid);
                    if st == "locked" {
                        return Err(Error::user("A locked consolidation run cannot be deleted."));
                    }
                }
                for gid in gids {
                    ensure_group_company_access(tx, this.env(), gid).await?;
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

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
        tx.flush().await?;
        let run_id = run.as_records().ids()[0];

        // Lock run row FOR UPDATE to serialize concurrent executions / locks.
        let lock_rows = tx
            .query(
                "SELECT state::text, group_id::int8, date_from, date_to \
                 FROM account_consolidation_run WHERE id = $1 FOR UPDATE",
                &[&run_id],
            )
            .await?;
        let Some(lock_row) = lock_rows.first() else {
            return Err(Error::missing("Consolidation run does not exist."));
        };
        let state: String = lock_row.try_get(0).map_err(db)?;
        if state == "locked" {
            return Err(Error::user(
                "A locked consolidation run cannot be re-executed.",
            ));
        }
        let group_id: i64 = lock_row.try_get(1).map_err(db)?;
        let date_from: Date = lock_row.try_get(2).map_err(db)?;
        let date_to: Date = lock_row.try_get(3).map_err(db)?;
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
                        COALESCE(fallback_to_spot, true), COALESCE(ic_tolerance, 0) \
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
        let ic_tolerance: Decimal = grp_row.try_get(5).map_err(db)?;
        if ic_tolerance < Decimal::ZERO {
            return Err(Error::validation(
                "IC Variance Tolerance cannot be negative.",
            ));
        }

        // Load units and authorize caller access to every member company first.
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
            let method: String = urow.try_get(4).map_err(db)?;
            if !matches!(method.as_str(), "full" | "proportional" | "equity") {
                return Err(Error::validation(format!(
                    "Unsupported consolidation method '{method}' for company {cid}."
                )));
            }
            group_company_ids.insert(cid);
            units.push(UnitInfo {
                company_id: cid,
                company_name: urow.try_get(1).map_err(db)?,
                local_currency_id: urow.try_get(2).map_err(db)?,
                is_parent: urow.try_get(3).map_err(db)?,
                method,
                ownership_pct: urow.try_get(5).map_err(db)?,
                investment_account_id: urow.try_get(6).map_err(db)?,
            });
        }

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
            .await?
            .first()
            .ok_or_else(|| Error::invariant("Missing count row for consolidation FS chart."))?
            .try_get(0)
            .map_err(db)?;
        if fs_count == 0 {
            AccountConsolidationGroup::action_bootstrap_default_chart(&grp_rec, tx).await?;
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
            if !matches!(
                item.rate_method.as_str(),
                "closing" | "average" | "historical" | "computed_cta"
            ) {
                return Err(Error::validation(format!(
                    "Unsupported translation rate method '{}' on Group FS Item '{}'.",
                    item.rate_method, item.code
                )));
            }
            if !matches!(
                item.section.as_str(),
                "asset_current"
                    | "asset_non_current"
                    | "goodwill"
                    | "liability_current"
                    | "liability_non_current"
                    | "equity_capital"
                    | "equity_retained"
                    | "equity_cta"
                    | "equity_nci"
                    | "income"
                    | "cogs"
                    | "expense"
                    | "nci_profit"
            ) {
                return Err(Error::validation(format!(
                    "Unsupported FS section '{}' on Group FS Item '{}'.",
                    item.section, item.code
                )));
            }
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
        let goodwill_fs_id = fs_by_section
            .get("goodwill")
            .copied()
            .ok_or_else(|| Error::validation("Group FS Chart requires a goodwill item."))?;
        let retained_fs_id = fs_by_section
            .get("equity_retained")
            .copied()
            .ok_or_else(|| Error::validation("Group FS Chart requires an equity_retained item."))?;
        let ic_var_fs_id = fs_by_role.get("ic_variance").copied().ok_or_else(|| {
            Error::validation("Group FS Chart requires an ic_variance elimination item.")
        })?;
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
        let group_cur_rec = real_id(group_currency_id)?;
        let group_inc = rounding_table.get(group_cur_rec)?.increment();

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
        let mut unit_equity_components = BTreeMap::<i64, BTreeMap<i64, Decimal>>::new(); // company_id -> (eq_fs_id -> signed sum)
        let mut unit_share_capital_group = BTreeMap::<i64, (i64, Decimal)>::new(); // (fs_id, credit magnitude > 0)
        // Parent investments keyed by optional subsidiary company_id -> (inv_fs_id, amount)
        let mut parent_inv_by_sub = BTreeMap::<i64, (i64, Decimal)>::new();
        let mut parent_inv_untagged = Decimal::ZERO;
        let mut parent_investment_fs_id: Option<i64> = None;
        let mut parent_company_id: Option<i64> = None;

        // Intercompany pair buckets (signed net balances so credit notes / contra entries net cleanly).
        let mut ic_ar_by_pair = BTreeMap::<(i64, i64), (i64, Decimal)>::new(); // (holder_co, tp_co) -> (fs_id, debit_net)
        let mut ic_ap_by_pair = BTreeMap::<(i64, i64), (i64, Decimal)>::new(); // (holder_co, tp_co) -> (fs_id, credit_net)
        let mut ic_rev_by_pair = BTreeMap::<(i64, i64), (i64, Decimal)>::new(); // (seller_co, buyer_co) -> (fs_id, credit_net)
        let mut ic_exp_by_pair = BTreeMap::<(i64, i64), (i64, Decimal)>::new(); // (buyer_co, seller_co) -> (fs_id, debit_net)

        let rate_company_id = units
            .iter()
            .find(|u| u.is_parent)
            .map(|u| u.company_id)
            .unwrap_or(units[0].company_id);
        let rate_co_rec = real_id(rate_company_id)?;

        // Load RateBooks once for closing, average, historical, and spot fallback.
        // Propagate real errors loudly; only tolerate Missing rate type when fallback_to_spot is enabled.
        let spot_book = RateBook::load(
            run.env(),
            tx,
            rate_co_rec,
            date_to,
            RateDateRule::OnOrBefore,
            &currency_vec,
        )
        .await?;
        let load_typed_opt = |res: Result<RateBook>| -> Result<Option<RateBook>> {
            match res {
                Ok(book) => Ok(Some(book)),
                Err(err) if fallback_to_spot && err.kind() == ErrorKind::Missing => Ok(None),
                Err(err) => Err(err),
            }
        };
        let closing_book = load_typed_opt(
            RateBook::load_typed(
                run.env(),
                tx,
                rate_co_rec,
                date_to,
                &closing_code,
                &currency_vec,
            )
            .await,
        )?;
        let average_book = load_typed_opt(
            RateBook::load_typed(
                run.env(),
                tx,
                rate_co_rec,
                date_to,
                &average_code,
                &currency_vec,
            )
            .await,
        )?;
        let historical_book = load_typed_opt(
            RateBook::load_typed(
                run.env(),
                tx,
                rate_co_rec,
                date_to,
                &historical_code,
                &currency_vec,
            )
            .await,
        )?;

        let translate_amount = |local_cur_id: i64,
                                amount: Decimal,
                                rate_method: &str|
         -> Result<(Decimal, Decimal)> {
            if local_cur_id == group_currency_id || amount.is_zero() {
                return Ok((quantize_inc(amount, group_inc)?, Decimal::ONE));
            }
            let from_cur = real_id(local_cur_id)?;
            let typed_book = match rate_method {
                "closing" => closing_book.as_ref(),
                "average" => average_book.as_ref(),
                "historical" => historical_book.as_ref(),
                "computed_cta" => {
                    return Err(Error::validation(
                        "Cannot directly translate a local GL balance using rate method 'computed_cta'.",
                    ));
                }
                other => {
                    return Err(Error::validation(format!(
                        "Unsupported translation rate method '{other}'."
                    )));
                }
            };
            let snap = if let Some(book) = typed_book {
                match book.snapshot(from_cur, group_cur_rec) {
                    Ok(s) => s,
                    Err(e)
                        if fallback_to_spot
                            && matches!(e.kind(), ErrorKind::Missing | ErrorKind::UserError) =>
                    {
                        spot_book.snapshot(from_cur, group_cur_rec)?
                    }
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
            let converted = exact.quantize(group_inc, CURRENCY_ROUNDING_MODE)?;
            let rate_inc = Increment::new(Decimal::new(1, 6))?;
            let unit_rate = snap
                .convert_exact(Decimal::ONE)?
                .quantize(&rate_inc, CURRENCY_ROUNDING_MODE)?;
            Ok((converted, unit_rate))
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
            let local_inc = rounding_table
                .get(real_id(unit.local_currency_id)?)?
                .increment();

            // Read posted journal item balances for this unit:
            // - Exclude journal groups (e.g. tax-only/excluded journals) via j.journal_group_id IS NULL
            // - Use LATERAL LIMIT 1 on res_company.partner_id to prevent duplicate-row fan-out
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
                     JOIN account_move m ON m.id = l.move_id \
                     JOIN account_journal j ON j.id = m.journal_id \
                     JOIN account_account a ON a.id = l.account_id \
                     LEFT JOIN res_partner p ON p.id = l.partner_id \
                     LEFT JOIN LATERAL ( \
                         SELECT id FROM res_company WHERE partner_id = l.partner_id ORDER BY id LIMIT 1 \
                     ) rc ON TRUE \
                     WHERE l.company_id = $1 AND l.parent_state = 'posted' \
                       AND j.journal_group_id IS NULL \
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
                let scaled_local = quantize_inc(main_local * scale_factor, local_inc)?;

                // Resolve Trading Partner only when it is another company inside this consolidation group.
                let tp_co =
                    raw_tp.filter(|tp| *tp != unit.company_id && group_company_ids.contains(tp));

                // Determine Group FS Item. Respect explicit IC/investment mappings; auto-route
                // standard third-party AR/AP/Rev/Exp accounts when tagged with an intercompany TP.
                let explicit_id = explicit_map.get(&(unit.company_id, acc_id)).copied();
                let base_fs_id = if Some(acc_id) == unit.investment_account_id {
                    fs_by_role
                        .get("investment_in_sub")
                        .copied()
                        .or_else(|| fs_by_code.get("1550").copied())
                } else if let Some(eid) = explicit_id {
                    Some(eid)
                } else {
                    let def_code = default_fs_code_for_account_type(&atype)?;
                    fs_by_code.get(def_code).copied()
                };
                let mut fs_id = base_fs_id.ok_or_else(|| {
                    Error::validation(format!(
                        "No Group FS Item mapping found for account {acc_id} (type '{atype}') in company '{}'.",
                        unit.company_name
                    ))
                })?;

                let explicit_has_role = explicit_id
                    .and_then(|id| fs_by_id.get(&id))
                    .is_some_and(|item| item.ic_role != "none");

                if tp_co.is_some()
                    && !explicit_has_role
                    && Some(acc_id) != unit.investment_account_id
                {
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

                // Track P&L vs Equity components for NCI & Investment Elimination.
                if matches!(fs_item.section.as_str(), "income" | "cogs" | "expense") {
                    *unit_translated_pnl
                        .entry(unit.company_id)
                        .or_insert(Decimal::ZERO) += grp_amt;
                }
                if matches!(
                    fs_item.section.as_str(),
                    "equity_capital" | "equity_retained" | "equity_cta"
                ) {
                    *unit_equity_components
                        .entry(unit.company_id)
                        .or_default()
                        .entry(fs_id)
                        .or_insert(Decimal::ZERO) += grp_amt;
                }
                if fs_item.section == "equity_capital" && !unit.is_parent {
                    let entry = unit_share_capital_group
                        .entry(unit.company_id)
                        .or_insert((fs_id, Decimal::ZERO));
                    entry.1 += -grp_amt; // credit magnitude
                }
                if fs_item.ic_role == "investment_in_sub" && unit.is_parent {
                    parent_investment_fs_id = Some(fs_id);
                    if let Some(tp) = tp_co {
                        let e = parent_inv_by_sub
                            .entry(tp)
                            .or_insert((fs_id, Decimal::ZERO));
                        e.1 += grp_amt;
                    } else {
                        parent_inv_untagged += grp_amt;
                    }
                }

                // Collect Intercompany Elimination buckets (all non-zero balances including contra notes).
                if let Some(tp) = tp_co {
                    match fs_item.ic_role.as_str() {
                        "ic_receivable" if !grp_amt.is_zero() => {
                            let e = ic_ar_by_pair
                                .entry((unit.company_id, tp))
                                .or_insert((fs_id, Decimal::ZERO));
                            e.1 += grp_amt;
                        }
                        "ic_payable" if !grp_amt.is_zero() => {
                            let e = ic_ap_by_pair
                                .entry((unit.company_id, tp))
                                .or_insert((fs_id, Decimal::ZERO));
                            e.1 += -grp_amt;
                        }
                        "ic_revenue" if !grp_amt.is_zero() => {
                            let e = ic_rev_by_pair
                                .entry((unit.company_id, tp))
                                .or_insert((fs_id, Decimal::ZERO));
                            e.1 += -grp_amt;
                        }
                        "ic_expense" if !grp_amt.is_zero() => {
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
                let scaled_prior = quantize_inc(prior_retained_local * scale_factor, local_inc)?;
                let (grp_amt, eff_rate) =
                    translate_amount(unit.local_currency_id, scaled_prior, "historical")?;
                unit_translated_sum += grp_amt;
                *unit_equity_components
                    .entry(unit.company_id)
                    .or_default()
                    .entry(retained_fs_id)
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

            // Compute exact CTA balancing plug ONLY for foreign-currency subsidiaries.
            // Same-currency proportional JV rounding drift routes to Retained Earnings, never FX CTA.
            let plug = quantize_inc(-unit_translated_sum, group_inc)?;
            if !plug.is_zero() {
                if unit.local_currency_id != group_currency_id {
                    *unit_equity_components
                        .entry(unit.company_id)
                        .or_default()
                        .entry(cta_fs_id)
                        .or_insert(Decimal::ZERO) += plug;
                    out_lines.push(OutLine {
                        fs_id: cta_fs_id,
                        company_id: Some(unit.company_id),
                        trading_partner_company_id: None,
                        posting_level: "00_translated",
                        local_currency_id: unit.local_currency_id,
                        local_amount: Decimal::ZERO,
                        exchange_rate: Decimal::ONE,
                        group_amount: plug,
                        label: format!("{} Translation Reserve (CTA)", unit.company_name),
                    });
                } else if unit.method == "proportional" {
                    *unit_equity_components
                        .entry(unit.company_id)
                        .or_default()
                        .entry(retained_fs_id)
                        .or_insert(Decimal::ZERO) += plug;
                    out_lines.push(OutLine {
                        fs_id: retained_fs_id,
                        company_id: Some(unit.company_id),
                        trading_partner_company_id: None,
                        posting_level: "00_translated",
                        local_currency_id: unit.local_currency_id,
                        local_amount: Decimal::ZERO,
                        exchange_rate: Decimal::ONE,
                        group_amount: plug,
                        label: format!(
                            "{} Proportional Consolidation Rounding Adjustment",
                            unit.company_name
                        ),
                    });
                } else {
                    return Err(Error::invariant(format!(
                        "Same-currency unit '{}' translated trial balance is out of balance by {}.",
                        unit.company_name, unit_translated_sum
                    )));
                }
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
            let (ar_fs, ar_net) = ic_ar_by_pair
                .get(&(creditor_co, debtor_co))
                .copied()
                .unwrap_or((ic_ar_fs_id.unwrap_or(ic_var_fs_id), Decimal::ZERO));
            let (ap_fs, ap_net) = ic_ap_by_pair
                .get(&(debtor_co, creditor_co))
                .copied()
                .unwrap_or((ic_ap_fs_id.unwrap_or(ic_var_fs_id), Decimal::ZERO));
            let (matched, variance) = pair_ic_balances_inc(ar_net, ap_net, group_inc)?;
            if variance.abs() > ic_tolerance {
                return Err(Error::user(format!(
                    "Intercompany AR/AP variance between company {creditor_co} and company {debtor_co} ({variance}) exceeds IC Variance Tolerance ({ic_tolerance})."
                )));
            }
            if !matched.is_zero() {
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
                // Eliminate remaining within-tolerance IC balance to explicit IC Elimination Variance
                let ar_rem = quantize_inc(ar_net - matched, group_inc)?;
                let ap_rem = quantize_inc(ap_net - matched, group_inc)?;
                if !ar_rem.is_zero() {
                    out_lines.push(OutLine {
                        fs_id: ar_fs,
                        company_id: Some(creditor_co),
                        trading_partner_company_id: Some(debtor_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: -ar_rem,
                        exchange_rate: Decimal::ONE,
                        group_amount: -ar_rem,
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
                        local_amount: ar_rem,
                        exchange_rate: Decimal::ONE,
                        group_amount: ar_rem,
                        label: format!(
                            "IC AR/AP Reconciliation Variance (Co {creditor_co} vs Co {debtor_co})"
                        ),
                    });
                }
                if !ap_rem.is_zero() {
                    out_lines.push(OutLine {
                        fs_id: ap_fs,
                        company_id: Some(debtor_co),
                        trading_partner_company_id: Some(creditor_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: ap_rem,
                        exchange_rate: Decimal::ONE,
                        group_amount: ap_rem,
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
                        local_amount: -ap_rem,
                        exchange_rate: Decimal::ONE,
                        group_amount: -ap_rem,
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
            let (rev_fs, rev_net) = ic_rev_by_pair
                .get(&(seller_co, buyer_co))
                .copied()
                .unwrap_or((ic_rev_fs_id.unwrap_or(ic_var_fs_id), Decimal::ZERO));
            let (exp_fs, exp_net) = ic_exp_by_pair
                .get(&(buyer_co, seller_co))
                .copied()
                .unwrap_or((ic_exp_fs_id.unwrap_or(ic_var_fs_id), Decimal::ZERO));
            let (matched, variance) = pair_ic_balances_inc(exp_net, rev_net, group_inc)?;
            if variance.abs() > ic_tolerance {
                return Err(Error::user(format!(
                    "Intercompany Revenue/Expense variance between seller company {seller_co} and buyer company {buyer_co} ({variance}) exceeds IC Variance Tolerance ({ic_tolerance})."
                )));
            }
            if !matched.is_zero() {
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
            if !variance.is_zero() {
                let exp_rem = quantize_inc(exp_net - matched, group_inc)?;
                let rev_rem = quantize_inc(rev_net - matched, group_inc)?;
                if !exp_rem.is_zero() {
                    out_lines.push(OutLine {
                        fs_id: exp_fs,
                        company_id: Some(buyer_co),
                        trading_partner_company_id: Some(seller_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: -exp_rem,
                        exchange_rate: Decimal::ONE,
                        group_amount: -exp_rem,
                        label: format!(
                            "IC Expense Variance Clearing (Co {buyer_co} <- Co {seller_co})"
                        ),
                    });
                    out_lines.push(OutLine {
                        fs_id: ic_var_fs_id,
                        company_id: Some(buyer_co),
                        trading_partner_company_id: Some(seller_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: exp_rem,
                        exchange_rate: Decimal::ONE,
                        group_amount: exp_rem,
                        label: format!(
                            "IC Rev/Exp Reconciliation Variance (Co {buyer_co} <- Co {seller_co})"
                        ),
                    });
                }
                if !rev_rem.is_zero() {
                    out_lines.push(OutLine {
                        fs_id: rev_fs,
                        company_id: Some(seller_co),
                        trading_partner_company_id: Some(buyer_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: rev_rem,
                        exchange_rate: Decimal::ONE,
                        group_amount: rev_rem,
                        label: format!(
                            "IC Revenue Variance Clearing (Co {seller_co} -> Co {buyer_co})"
                        ),
                    });
                    out_lines.push(OutLine {
                        fs_id: ic_var_fs_id,
                        company_id: Some(seller_co),
                        trading_partner_company_id: Some(buyer_co),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: -rev_rem,
                        exchange_rate: Decimal::ONE,
                        group_amount: -rev_rem,
                        label: format!(
                            "IC Rev/Exp Reconciliation Variance (Co {seller_co} -> Co {buyer_co})"
                        ),
                    });
                }
            }
        }

        // 2c. Parent Investment in Subsidiary vs Subsidiary Share Capital Elimination + Goodwill
        // Eliminate per subsidiary so Goodwill of one entity never nets against Bargain Purchase of another
        // and Equity-method associates are never wiped out.
        let full_subs: Vec<&UnitInfo> = units
            .iter()
            .filter(|u| !u.is_parent && u.method == "full")
            .collect();
        let mut eliminated_subs = BTreeSet::<i64>::new();
        for unit in &full_subs {
            let Some(&(inv_fs, sub_inv)) = parent_inv_by_sub.get(&unit.company_id) else {
                continue;
            };
            if sub_inv <= Decimal::ZERO {
                continue;
            }
            let Some(&(cap_fs, cap_mag)) = unit_share_capital_group.get(&unit.company_id) else {
                continue;
            };
            if cap_mag <= Decimal::ZERO {
                continue;
            }
            let parent_portion =
                quantize_inc(cap_mag * unit.ownership_pct / Decimal::from(100), group_inc)?;
            if parent_portion <= Decimal::ZERO {
                continue;
            }
            eliminated_subs.insert(unit.company_id);
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
            out_lines.push(OutLine {
                fs_id: inv_fs,
                company_id: parent_company_id,
                trading_partner_company_id: Some(unit.company_id),
                posting_level: "20_ic_elimination",
                local_currency_id: group_currency_id,
                local_amount: -sub_inv,
                exchange_rate: Decimal::ONE,
                group_amount: -sub_inv,
                label: format!(
                    "Investment in Subsidiary Elimination ({})",
                    unit.company_name
                ),
            });
            let goodwill = quantize_inc(sub_inv - parent_portion, group_inc)?;
            if !goodwill.is_zero() {
                let target_fs = if goodwill > Decimal::ZERO {
                    goodwill_fs_id
                } else {
                    retained_fs_id
                };
                out_lines.push(OutLine {
                    fs_id: target_fs,
                    company_id: parent_company_id,
                    trading_partner_company_id: Some(unit.company_id),
                    posting_level: "20_ic_elimination",
                    local_currency_id: group_currency_id,
                    local_amount: goodwill,
                    exchange_rate: Decimal::ONE,
                    group_amount: goodwill,
                    label: if goodwill > Decimal::ZERO {
                        format!("Goodwill on Consolidation ({})", unit.company_name)
                    } else {
                        format!(
                            "Bargain Purchase / Consolidation Reserve ({})",
                            unit.company_name
                        )
                    },
                });
            }
        }
        if parent_inv_untagged > Decimal::ZERO
            && let Some(inv_fs) = parent_investment_fs_id
        {
            let remaining_subs: Vec<&UnitInfo> = full_subs
                .iter()
                .copied()
                .filter(|u| {
                    !eliminated_subs.contains(&u.company_id)
                        && unit_share_capital_group
                            .get(&u.company_id)
                            .is_some_and(|(_, m)| *m > Decimal::ZERO)
                })
                .collect();
            if remaining_subs.len() == 1 {
                let unit = remaining_subs[0];
                let (cap_fs, cap_mag) = unit_share_capital_group[&unit.company_id];
                let parent_portion =
                    quantize_inc(cap_mag * unit.ownership_pct / Decimal::from(100), group_inc)?;
                if parent_portion > Decimal::ZERO {
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
                    out_lines.push(OutLine {
                        fs_id: inv_fs,
                        company_id: parent_company_id,
                        trading_partner_company_id: Some(unit.company_id),
                        posting_level: "20_ic_elimination",
                        local_currency_id: group_currency_id,
                        local_amount: -parent_inv_untagged,
                        exchange_rate: Decimal::ONE,
                        group_amount: -parent_inv_untagged,
                        label: format!(
                            "Investment in Subsidiary Elimination ({})",
                            unit.company_name
                        ),
                    });
                    let goodwill = quantize_inc(parent_inv_untagged - parent_portion, group_inc)?;
                    if !goodwill.is_zero() {
                        let target_fs = if goodwill > Decimal::ZERO {
                            goodwill_fs_id
                        } else {
                            retained_fs_id
                        };
                        out_lines.push(OutLine {
                            fs_id: target_fs,
                            company_id: parent_company_id,
                            trading_partner_company_id: Some(unit.company_id),
                            posting_level: "20_ic_elimination",
                            local_currency_id: group_currency_id,
                            local_amount: goodwill,
                            exchange_rate: Decimal::ONE,
                            group_amount: goodwill,
                            label: if goodwill > Decimal::ZERO {
                                format!("Goodwill on Consolidation ({})", unit.company_name)
                            } else {
                                format!(
                                    "Bargain Purchase / Consolidation Reserve ({})",
                                    unit.company_name
                                )
                            },
                        });
                    }
                }
            } else if remaining_subs.len() > 1 {
                return Err(Error::user(
                    "Multiple full-consolidated subsidiaries have untagged Parent Investment balances; tag each Parent Investment journal line with the subsidiary's Trading Partner Company so Goodwill / Bargain Purchase is calculated per subsidiary.",
                ));
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
            let amt = quantize_inc(arow.try_get(3).map_err(db)?, group_inc)?;
            let lbl: String = arow.try_get(4).map_err(db)?;
            if amt.is_zero() {
                continue;
            }
            let fs = fs_by_id.get(&fs_id).ok_or_else(|| {
                Error::validation(format!(
                    "Manual adjustment references Group FS Item {fs_id} which does not belong to this group."
                ))
            })?;
            // Reflect entity-attributed top-side adjustments in the subsidiary's NCI base.
            if let Some(cid) = co_id {
                if matches!(fs.section.as_str(), "income" | "cogs" | "expense") {
                    *unit_translated_pnl.entry(cid).or_insert(Decimal::ZERO) += amt;
                } else if matches!(
                    fs.section.as_str(),
                    "equity_capital" | "equity_retained" | "equity_cta"
                ) {
                    *unit_equity_components
                        .entry(cid)
                        .or_default()
                        .entry(fs_id)
                        .or_insert(Decimal::ZERO) += amt;
                }
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
            let nci_profit_share =
                compute_nci_share_inc(sub_profit, unit.ownership_pct, group_inc)?;
            if !nci_profit_share.is_zero() {
                total_nci_profit += nci_profit_share;
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

            // Carve NCI proportionally from EACH equity component (Share Capital, Retained Earnings, CTA)
            // so Share Capital is never over-debited for Retained Earnings or CTA reserves.
            if let Some(comp_map) = unit_equity_components.get(&unit.company_id) {
                for (&eq_fs_id, &eq_signed) in comp_map {
                    let eq_credit = -eq_signed;
                    let comp_nci = compute_nci_share_inc(eq_credit, unit.ownership_pct, group_inc)?;
                    if comp_nci.is_zero() {
                        continue;
                    }
                    total_nci_equity += comp_nci;
                    let fs_code = fs_by_id
                        .get(&eq_fs_id)
                        .map(|f| f.code.as_str())
                        .unwrap_or("");
                    out_lines.push(OutLine {
                        fs_id: eq_fs_id,
                        company_id: Some(unit.company_id),
                        trading_partner_company_id: None,
                        posting_level: "30_nci",
                        local_currency_id: group_currency_id,
                        local_amount: comp_nci,
                        exchange_rate: Decimal::ONE,
                        group_amount: comp_nci,
                        label: format!(
                            "NCI Equity Reclassification ({}, FS {fs_code})",
                            unit.company_name
                        ),
                    });
                    out_lines.push(OutLine {
                        fs_id: nci_eq_fs_id,
                        company_id: Some(unit.company_id),
                        trading_partner_company_id: None,
                        posting_level: "30_nci",
                        local_currency_id: group_currency_id,
                        local_amount: -comp_nci,
                        exchange_rate: Decimal::ONE,
                        group_amount: -comp_nci,
                        label: format!(
                            "Non-Controlling Interest in Equity ({}, FS {fs_code}, {}% NCI)",
                            unit.company_name,
                            Decimal::from(100) - unit.ownership_pct
                        ),
                    });
                }
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
                    total_liab_eq_signed += ol.group_amount;
                    if fs.section != "nci_profit" {
                        total_pnl_signed += ol.group_amount;
                    }
                }
                other => {
                    return Err(Error::invariant(format!(
                        "Unsupported FS section '{other}' during consolidation total computation."
                    )));
                }
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
            let sudo_env = strip_default_context(run.env()).sudo(true);
            sudo_env
                .records("account.consolidation.line")?
                .create(tx, vals_list)
                .await?;
        }

        let total_liabilities_equity = quantize_inc(-total_liab_eq_signed, group_inc)?;
        let total_assets = quantize_inc(total_assets, group_inc)?;
        let balance_check_diff = quantize_inc(total_assets - total_liabilities_equity, group_inc)?;
        let total_cta = quantize_inc(-total_cta_signed, group_inc)?;
        let group_net_profit = quantize_inc(-total_pnl_signed, group_inc)?;
        let total_nci_equity = quantize_inc(total_nci_equity, group_inc)?;
        let total_nci_profit = quantize_inc(total_nci_profit, group_inc)?;
        let uid = run.env().uid();

        let updated = tx
            .execute(
                "UPDATE account_consolidation_run \
                 SET state = 'executed', total_assets = $2, total_liabilities_equity = $3, \
                     balance_check_diff = $4, total_cta = $5, total_nci_equity = $6, \
                     total_nci_profit = $7, group_net_profit = $8, \
                     write_uid = $9, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state <> 'locked'",
                &[
                    &run_id,
                    &total_assets,
                    &total_liabilities_equity,
                    &balance_check_diff,
                    &total_cta,
                    &total_nci_equity,
                    &total_nci_profit,
                    &group_net_profit,
                    &uid,
                ],
            )
            .await?;
        if updated != 1 {
            return Err(Error::user(
                "Consolidation run was locked concurrently and cannot be re-executed.",
            ));
        }
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
        tx.flush().await?;
        let id = run.as_records().ids()[0];
        let rows = tx
            .query(
                "SELECT group_id::int8, state::text, COALESCE(balance_check_diff, 0) \
                 FROM account_consolidation_run WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Consolidation run does not exist."));
        };
        let group_id: i64 = row.try_get(0).map_err(db)?;
        let state: String = row.try_get(1).map_err(db)?;
        let diff: Decimal = row.try_get(2).map_err(db)?;
        ensure_group_company_access(tx, run.env(), group_id).await?;
        if state != "executed" {
            return Err(Error::user(
                "Execute the consolidation run before locking it.",
            ));
        }
        if !diff.is_zero() {
            return Err(Error::user(format!(
                "Cannot lock consolidation run: Consolidated Balance Sheet is out of balance by {diff}."
            )));
        }
        let uid = run.env().uid();
        let updated = tx
            .execute(
                "UPDATE account_consolidation_run \
                 SET state = 'locked', write_uid = $2, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state = 'executed'",
                &[&id, &uid],
            )
            .await?;
        if updated != 1 {
            return Err(Error::user(
                "Consolidation run state changed concurrently; reload and retry.",
            ));
        }
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
    #[field(date, required = true, string = "Date")]
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
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountConsolidationAdjustment>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "date") && !values.contains_key("date") {
            let today = resolve_default_today(tx, this.env()).await?;
            values.insert("date".to_owned(), Value::Date(today));
        }
        Ok(values)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<AccountConsolidationAdjustment>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if !this.env().su() {
            let mut run_ids = BTreeSet::new();
            for vals in &vals_list {
                if let Some(Cell::Str(st)) = vals.get("state")
                    && st.as_ref() != "draft"
                {
                    return Err(Error::validation(
                        "Consolidation adjustments must start in Draft status.",
                    ));
                }
                if let Some(Cell::Id(rid)) = vals.get("run_id") {
                    run_ids.insert(*rid);
                }
            }
            if !run_ids.is_empty() {
                let ids: Vec<i64> = run_ids.into_iter().collect();
                let rows = tx
                    .query(
                        "SELECT group_id::int8, state::text FROM account_consolidation_run WHERE id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                for r in rows {
                    let gid: i64 = r.try_get(0).map_err(db)?;
                    let st: String = r.try_get(1).map_err(db)?;
                    ensure_group_company_access(tx, this.env(), gid).await?;
                    if st == "locked" {
                        return Err(Error::user(
                            "Cannot add consolidation adjustments to a locked run.",
                        ));
                    }
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountConsolidationAdjustment>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            if vals.contains("state") {
                return Err(Error::validation(
                    "Consolidation adjustment status must be changed through the Post action.",
                ));
            }
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let rows = tx
                    .query(
                        "SELECT a.state::text, r.group_id::int8, r.state::text \
                         FROM account_consolidation_adjustment a \
                         JOIN account_consolidation_run r ON r.id = a.run_id \
                         WHERE a.id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                for r in rows {
                    let ast: String = r.try_get(0).map_err(db)?;
                    let gid: i64 = r.try_get(1).map_err(db)?;
                    let rst: String = r.try_get(2).map_err(db)?;
                    ensure_group_company_access(tx, this.env(), gid).await?;
                    if ast != "draft" {
                        return Err(Error::user(
                            "A posted consolidation adjustment cannot be modified.",
                        ));
                    }
                    if rst == "locked" {
                        return Err(Error::user(
                            "Cannot modify adjustments belonging to a locked consolidation run.",
                        ));
                    }
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountConsolidationAdjustment>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let rows = tx
                    .query(
                        "SELECT a.state::text, r.group_id::int8, r.state::text \
                         FROM account_consolidation_adjustment a \
                         JOIN account_consolidation_run r ON r.id = a.run_id \
                         WHERE a.id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                for r in rows {
                    let ast: String = r.try_get(0).map_err(db)?;
                    let gid: i64 = r.try_get(1).map_err(db)?;
                    let rst: String = r.try_get(2).map_err(db)?;
                    ensure_group_company_access(tx, this.env(), gid).await?;
                    if ast != "draft" {
                        return Err(Error::user(
                            "A posted consolidation adjustment cannot be deleted.",
                        ));
                    }
                    if rst == "locked" {
                        return Err(Error::user(
                            "Cannot delete adjustments belonging to a locked consolidation run.",
                        ));
                    }
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Validates exact debit == credit balance (`sum(amount) == 0.00`) and posts the adjustment.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_post_adjustment(
        this: &RecordSet<AccountConsolidationAdjustment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let adj = this.ensure_one()?;
        adj.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let aid = adj.as_records().ids()[0];

        // Lock adjustment and parent run FOR UPDATE.
        let arows = tx
            .query(
                "SELECT a.state::text, a.run_id::int8, r.group_id::int8, r.state::text, r.currency_id::int8 \
                 FROM account_consolidation_adjustment a \
                 JOIN account_consolidation_run r ON r.id = a.run_id \
                 WHERE a.id = $1 FOR UPDATE OF a, r",
                &[&aid],
            )
            .await?;
        let Some(arow) = arows.first() else {
            return Err(Error::missing("Consolidation adjustment does not exist."));
        };
        let state: String = arow.try_get(0).map_err(db)?;
        let group_id: i64 = arow.try_get(2).map_err(db)?;
        let run_state: String = arow.try_get(3).map_err(db)?;
        let currency_id: i64 = arow.try_get(4).map_err(db)?;
        ensure_group_company_access(tx, adj.env(), group_id).await?;
        if run_state == "locked" {
            return Err(Error::user(
                "Cannot post a consolidation adjustment on a locked run.",
            ));
        }
        if state != "draft" {
            return Err(Error::user(
                "This consolidation adjustment is already posted.",
            ));
        }

        let cur_rec = RecId::from_real(currency_id)
            .ok_or_else(|| Error::invariant("Invalid group currency id."))?;
        let rtable = RoundingTable::load(adj.env(), tx, &[cur_rec]).await?;
        let inc = rtable.get(cur_rec)?.increment();

        let rows = tx
            .query(
                "SELECT count(*)::int8, COALESCE(SUM(amount), 0) \
                 FROM account_consolidation_adjustment_line WHERE adjustment_id = $1",
                &[&aid],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::invariant("Missing aggregation row for adjustment lines."))?;
        let count: i64 = row.try_get(0).map_err(db)?;
        let net: Decimal = row.try_get(1).map_err(db)?;
        if count < 2 {
            return Err(Error::user(
                "A consolidation adjustment requires at least two balancing lines.",
            ));
        }
        if !quantize_inc(net, inc)?.is_zero() {
            return Err(Error::user(format!(
                "Consolidation adjustment is unbalanced by {net} (debit positive, credit negative must sum to 0)."
            )));
        }
        let uid = adj.env().uid();
        let updated = tx
            .execute(
                "UPDATE account_consolidation_adjustment \
                 SET state = 'posted', write_uid = $2, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state = 'draft'",
                &[&aid, &uid],
            )
            .await?;
        if updated != 1 {
            return Err(Error::user(
                "Consolidation adjustment state changed concurrently; reload and retry.",
            ));
        }
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
impl AccountConsolidationAdjustmentLine {
    #[constrains("adjustment_id", "consolidation_account_id")]
    async fn _check_line_group(
        this: &RecordSet<AccountConsolidationAdjustmentLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        tx.flush().await?;
        let ids: Vec<i64> = this.as_records().ids().to_vec();
        if ids.is_empty() {
            return Ok(());
        }
        let bad = tx
            .query(
                "SELECT l.id::int8 \
                 FROM account_consolidation_adjustment_line l \
                 JOIN account_consolidation_adjustment a ON a.id = l.adjustment_id \
                 JOIN account_consolidation_run r ON r.id = a.run_id \
                 JOIN account_consolidation_account fs ON fs.id = l.consolidation_account_id \
                 WHERE l.id = ANY($1::int8[]) AND fs.group_id <> r.group_id \
                 LIMIT 1",
                &[&ids],
            )
            .await?;
        if !bad.is_empty() {
            return Err(Error::validation(
                "Adjustment line Group FS Item must belong to the run's Consolidation Group.",
            ));
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<AccountConsolidationAdjustmentLine>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if !this.env().su() {
            let mut adj_ids = BTreeSet::new();
            for vals in &vals_list {
                if let Some(Cell::Id(aid)) = vals.get("adjustment_id") {
                    adj_ids.insert(*aid);
                }
            }
            if !adj_ids.is_empty() {
                let ids: Vec<i64> = adj_ids.into_iter().collect();
                let rows = tx
                    .query(
                        "SELECT a.state::text, r.group_id::int8, r.state::text \
                         FROM account_consolidation_adjustment a \
                         JOIN account_consolidation_run r ON r.id = a.run_id \
                         WHERE a.id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                for r in rows {
                    let ast: String = r.try_get(0).map_err(db)?;
                    let gid: i64 = r.try_get(1).map_err(db)?;
                    let rst: String = r.try_get(2).map_err(db)?;
                    ensure_group_company_access(tx, this.env(), gid).await?;
                    if ast != "draft" || rst == "locked" {
                        return Err(Error::user(
                            "Cannot add lines to a posted adjustment or locked consolidation run.",
                        ));
                    }
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountConsolidationAdjustmentLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let rows = tx
                    .query(
                        "SELECT a.state::text, r.group_id::int8, r.state::text \
                         FROM account_consolidation_adjustment_line l \
                         JOIN account_consolidation_adjustment a ON a.id = l.adjustment_id \
                         JOIN account_consolidation_run r ON r.id = a.run_id \
                         WHERE l.id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                for r in rows {
                    let ast: String = r.try_get(0).map_err(db)?;
                    let gid: i64 = r.try_get(1).map_err(db)?;
                    let rst: String = r.try_get(2).map_err(db)?;
                    ensure_group_company_access(tx, this.env(), gid).await?;
                    if ast != "draft" || rst == "locked" {
                        return Err(Error::user(
                            "Cannot edit lines of a posted adjustment or locked consolidation run.",
                        ));
                    }
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountConsolidationAdjustmentLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let rows = tx
                    .query(
                        "SELECT a.state::text, r.group_id::int8, r.state::text \
                         FROM account_consolidation_adjustment_line l \
                         JOIN account_consolidation_adjustment a ON a.id = l.adjustment_id \
                         JOIN account_consolidation_run r ON r.id = a.run_id \
                         WHERE l.id = ANY($1::int8[])",
                        &[&ids],
                    )
                    .await?;
                for r in rows {
                    let ast: String = r.try_get(0).map_err(db)?;
                    let gid: i64 = r.try_get(1).map_err(db)?;
                    let rst: String = r.try_get(2).map_err(db)?;
                    ensure_group_company_access(tx, this.env(), gid).await?;
                    if ast != "draft" || rst == "locked" {
                        return Err(Error::user(
                            "Cannot delete lines of a posted adjustment or locked consolidation run.",
                        ));
                    }
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

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
impl AccountConsolidationLine {
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountConsolidationLine>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if !this.env().su() {
            return Err(Error::access(
                "Consolidation ledger lines are generated by executing a Consolidation Run and cannot be created manually.",
            ));
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountConsolidationLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            return Err(Error::access(
                "Consolidation ledger lines are read-only outputs of a Consolidation Run.",
            ));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountConsolidationLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            return Err(Error::access(
                "Consolidation ledger lines cannot be deleted manually; re-execute or delete the draft run.",
            ));
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

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
