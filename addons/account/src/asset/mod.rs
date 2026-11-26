// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fixed Assets, Asset Classes, Construction in Progress (CIP), Multi-Receiver Settlement,
//! Periodic Depreciation Runs, and Asset Disposal.

pub mod cip;
pub mod settlement;

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::fiscal::calendar::add_months;
use crate::moves::post_moves;

/// One row of a computed depreciation schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepreciationRow {
    pub sequence: i32,
    pub date: Date,
    pub amount: Decimal,
    pub cumulative: Decimal,
    pub remaining: Decimal,
}

/// Pure exact-decimal depreciation schedule generator supporting Straight Line (`degressive = false`),
/// Declining Balance (`degressive = true`), and optional Pro-Rata Temporis first-period proration.
/// Strictly conserves exact total: `sum(row.amount) == depreciable` and `last.remaining == 0`.
pub fn build_depreciation_schedule(
    depreciable: Decimal,
    start_date: Date,
    periods: i32,
    months_per_period: i32,
    degressive: bool,
    factor: Decimal,
) -> Result<Vec<DepreciationRow>> {
    build_depreciation_schedule_with_prorata(
        depreciable,
        start_date,
        periods,
        months_per_period,
        degressive,
        factor,
        false,
    )
}

/// Schedule generator with optional day-based pro-rata temporis for the first period (default 2 decimals).
pub fn build_depreciation_schedule_with_prorata(
    depreciable: Decimal,
    start_date: Date,
    periods: i32,
    months_per_period: i32,
    degressive: bool,
    factor: Decimal,
    prorata_temporis: bool,
) -> Result<Vec<DepreciationRow>> {
    build_depreciation_schedule_scaled(
        depreciable,
        start_date,
        periods,
        months_per_period,
        degressive,
        factor,
        prorata_temporis,
        2,
    )
}

/// Currency-scale-aware depreciation schedule generator.
#[allow(clippy::too_many_arguments)]
pub fn build_depreciation_schedule_scaled(
    depreciable: Decimal,
    start_date: Date,
    periods: i32,
    months_per_period: i32,
    degressive: bool,
    factor: Decimal,
    prorata_temporis: bool,
    scale: u32,
) -> Result<Vec<DepreciationRow>> {
    if depreciable <= Decimal::ZERO {
        return Err(Error::user(
            "Depreciable value (Original Value minus Salvage Value) must be greater than zero.",
        ));
    }
    if periods <= 0 || periods > 600 {
        return Err(Error::user(
            "Number of depreciations must be between 1 and 600.",
        ));
    }
    if months_per_period <= 0 || months_per_period > 120 {
        return Err(Error::user("Months per period must be between 1 and 120."));
    }
    let mut rows = Vec::with_capacity((periods + 1) as usize);
    let mut remaining = depreciable.round_dp(scale);
    let mut cumulative = Decimal::ZERO;
    let linear_base = (remaining / Decimal::from(periods)).round_dp(scale);

    // Optional first-period pro-rata temporis when starting mid-month (day > 1).
    let (_, _, start_day) = start_date.ymd();
    let has_prorata = prorata_temporis && start_day > 1 && periods >= 1;
    let total_rows = if has_prorata { periods + 1 } else { periods };

    for i in 1..=total_rows {
        let dep_date = add_months(start_date, (i - 1) * months_per_period)?;
        let amount = if i == total_rows {
            remaining
        } else if i == 1 && has_prorata {
            let days_used = (31 - start_day as i32).clamp(1, 30);
            let ratio = Decimal::from(days_used) / Decimal::from(30);
            let base = if degressive {
                (remaining * factor).round_dp(scale)
            } else {
                linear_base
            };
            (base * ratio).round_dp(scale).min(remaining)
        } else if degressive {
            let periods_left = total_rows - i + 1;
            let linear_floor = (remaining / Decimal::from(periods_left)).round_dp(scale);
            let deg = (remaining * factor).round_dp(scale);
            deg.max(linear_floor).min(remaining)
        } else {
            linear_base.min(remaining)
        };
        cumulative += amount;
        remaining -= amount;
        rows.push(DepreciationRow {
            sequence: i,
            date: dep_date,
            amount,
            cumulative,
            remaining,
        });
    }
    Ok(rows)
}

pub(crate) fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Asset query failed: {err}"))
}

async fn currency_scale(tx: &mut Transaction, currency_id: i64) -> Result<u32> {
    let rows = tx
        .query(
            "SELECT decimal_places::int4 FROM res_currency WHERE id = $1",
            &[&currency_id],
        )
        .await?;
    let scale_i32: i32 = rows
        .first()
        .ok_or_else(|| Error::missing(format!("Currency {currency_id} not found.")))?
        .try_get(0)
        .map_err(db)?;
    u32::try_from(scale_i32)
        .map_err(|_| Error::validation(format!("Invalid currency decimal_places {scale_i32}.")))
}

/// `account.asset.category`: reusable Asset Class defining accounts, CIP vs depreciable behavior,
/// and default depreciation parameters.
#[model(
    name = "account.asset.category",
    description = "Asset Class",
    order = "code, name, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_unique_company_code",
        fields = ["company_id", "code"],
        message = "Asset Class code must be unique per company."
    )]
)]
pub struct AccountAssetCategory {
    /// Asset class name (e.g. "Machinery & Equipment", "Construction in Progress").
    #[field(char, required = true, string = "Asset Class Name")]
    pub name: Option<Arc<str>>,
    /// Short class code (e.g. "CLS-MACH", "CLS-CIP").
    #[field(char, required = true, index = true, string = "Code")]
    pub code: Option<Arc<str>>,
    /// Owning company.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Depreciable fixed asset class vs Construction in Progress (zero depreciation).
    #[field(
        selection,
        selection = [
            ("normal", "Depreciable Fixed Asset"),
            ("cip", "Construction in Progress (CIP)"),
        ],
        required = true,
        default = "normal",
        string = "Class Type"
    )]
    pub asset_type: Option<Arc<str>>,
    /// Default journal for asset entries.
    #[field(
        many2one,
        comodel = "account.journal",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// Fixed Asset or CIP Balance Sheet GL account.
    #[field(
        many2one,
        comodel = "account.account",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Asset / CIP Account"
    )]
    pub account_asset_id: Option<RecId>,
    /// Accumulated Depreciation account (required for depreciable classes).
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Accumulated Depreciation Account"
    )]
    pub account_depreciation_id: Option<RecId>,
    /// Depreciation Expense account (required for depreciable classes).
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Depreciation Expense Account"
    )]
    pub account_depreciation_expense_id: Option<RecId>,
    /// Gain / Loss on Disposal account.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Disposal Gain/Loss Account"
    )]
    pub account_disposal_gain_loss_id: Option<RecId>,
    /// Default depreciation method.
    #[field(
        selection,
        selection = [
            ("linear", "Straight Line"),
            ("degressive", "Declining Balance"),
        ],
        required = true,
        default = "linear",
        string = "Method"
    )]
    pub method: Option<Arc<str>>,
    /// Default number of depreciation periods.
    #[field(
        integer,
        required = true,
        default = 60,
        string = "Number of Depreciations"
    )]
    pub method_number: i32,
    /// Months per depreciation period.
    #[field(integer, required = true, default = 1, string = "Months per Period")]
    pub method_period: i32,
    /// Declining balance factor.
    #[field(float, digits = (16, 4), default = 0.30, string = "Declining Factor")]
    pub method_progress_factor: Decimal,
    /// Pro-rata temporis first-period proration.
    #[field(boolean, default = false, string = "Pro-Rata Temporis")]
    pub prorata_temporis: bool,
    /// Active flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
}

#[methods(AccountAssetCategory, addon = "account")]
impl AccountAssetCategory {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountAssetCategory>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if let Some(cid) = this.env().company_id()? {
            if names.iter().any(|n| n == "company_id") && !values.contains_key("company_id") {
                values.insert("company_id".to_owned(), Value::Int(cid));
            }
            if names.iter().any(|n| n == "journal_id") && !values.contains_key("journal_id") {
                let rows = tx
                    .query(
                        "SELECT id::int8 FROM account_journal \
                         WHERE company_id = $1 AND type = 'general' AND active = true \
                         ORDER BY sequence, id LIMIT 1",
                        &[&cid],
                    )
                    .await?;
                if let Some(jid) = rows.first().and_then(|r| r.try_get::<_, i64>(0).ok()) {
                    values.insert("journal_id".to_owned(), Value::Int(jid));
                }
            }
        }
        Ok(values)
    }
}

/// `account.asset`: fixed asset or Construction in Progress (CIP) asset with depreciation
/// schedules, CIP cost accumulation, multi-receiver capitalization, and disposal.
#[model(
    name = "account.asset",
    description = "Fixed Asset",
    order = "acquisition_date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountAsset {
    /// Asset name (e.g. "Delivery Van #1", "Factory Line B — CIP").
    #[field(char, required = true, string = "Asset Name")]
    pub name: Option<Arc<str>>,
    /// Optional asset number / tag (e.g. "AST-2026-001", "CIP-2026-001").
    #[field(char, index = true, string = "Asset Number")]
    pub code: Option<Arc<str>>,
    /// Asset class template.
    #[field(
        many2one,
        comodel = "account.asset.category",
        check_company = true,
        ondelete = "restrict",
        string = "Asset Class"
    )]
    pub category_id: Option<RecId>,
    /// Depreciable fixed asset vs Construction in Progress (never depreciates until settled).
    #[field(
        selection,
        selection = [
            ("normal", "Fixed Asset"),
            ("cip", "Construction in Progress (CIP)"),
        ],
        required = true,
        default = "normal",
        index = true,
        string = "Asset Type"
    )]
    pub asset_type: Option<Arc<str>>,
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
    /// Cost Center charged when posting depreciation entries for this asset.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "set null",
        string = "Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// Originating CIP asset when this fixed asset was capitalized via CIP settlement.
    #[field(
        many2one,
        comodel = "account.asset",
        check_company = true,
        readonly = true,
        ondelete = "set null",
        string = "Source CIP Project"
    )]
    pub source_cip_id: Option<RecId>,
    /// Lifecycle status.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("cip", "Under Construction"),
            ("open", "Running"),
            ("settled", "Settled"),
            ("disposed", "Disposed"),
            ("close", "Closed"),
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Acquisition / capitalization / depreciation start date.
    #[field(date, required = true, string = "Acquisition Date")]
    pub acquisition_date: Option<Date>,
    /// Purchase / original cost of the asset.
    #[field(monetary, currency_field = "currency_id", string = "Original Value")]
    pub original_value: Decimal,
    /// Salvage / non-depreciable residual value.
    #[field(
        monetary,
        currency_field = "currency_id",
        string = "Not Depreciable Value"
    )]
    pub salvage_value: Decimal,
    /// Remaining book value (original value minus posted depreciation; for CIP, unsettled balance).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_residual",
        string = "Book Value"
    )]
    pub value_residual: Decimal,
    /// Straight-line or declining-balance method.
    #[field(
        selection,
        selection = [
            ("linear", "Straight Line"),
            ("degressive", "Declining Balance"),
        ],
        required = true,
        default = "linear",
        string = "Method"
    )]
    pub method: Option<Arc<str>>,
    /// Number of depreciation entries.
    #[field(
        integer,
        required = true,
        default = 5,
        string = "Number of Depreciations"
    )]
    pub method_number: i32,
    /// Duration of one period in months (1 = monthly, 12 = yearly).
    #[field(integer, required = true, default = 12, string = "Months per Period")]
    pub method_period: i32,
    /// Declining balance factor (e.g. 0.30 = 30%).
    #[field(float, digits = (16, 4), default = 0.30, string = "Declining Factor")]
    pub method_progress_factor: Decimal,
    /// Prorate the first depreciation period based on acquisition day of month.
    #[field(boolean, default = false, string = "Pro-Rata Temporis")]
    pub prorata_temporis: bool,
    /// Journal used to post depreciation, settlement, and disposal entries.
    #[field(
        many2one,
        comodel = "account.journal",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// Fixed Asset or CIP balance sheet account.
    #[field(
        many2one,
        comodel = "account.account",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Fixed Asset / CIP Account"
    )]
    pub account_asset_id: Option<RecId>,
    /// Accumulated Depreciation account (credited on each depreciation entry; optional for CIP).
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Depreciation Account"
    )]
    pub account_depreciation_id: Option<RecId>,
    /// Depreciation Expense account (debited on each depreciation entry; optional for CIP).
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Expense Account"
    )]
    pub account_depreciation_expense_id: Option<RecId>,
    /// Gain / Loss on Disposal account.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Disposal Gain/Loss Account"
    )]
    pub account_disposal_gain_loss_id: Option<RecId>,
    /// Disposal date when scrapped or sold.
    #[field(date, readonly = true, copy = false, string = "Disposal Date")]
    pub disposal_date: Option<Date>,
    /// Disposal journal entry.
    #[field(
        many2one,
        comodel = "account.move",
        readonly = true,
        copy = false,
        ondelete = "set null",
        string = "Disposal Entry"
    )]
    pub disposal_move_id: Option<RecId>,
    /// Computed depreciation board lines.
    #[field(
        one2many,
        comodel = "account.asset.depreciation.line",
        inverse_name = "asset_id",
        string = "Depreciation Board"
    )]
    pub depreciation_line_ids: Vec<RecId>,
    /// Accumulated construction cost lines (for CIP assets).
    #[field(
        one2many,
        comodel = "account.asset.cip.line",
        inverse_name = "asset_id",
        readonly = true,
        string = "CIP Cost Accumulation"
    )]
    pub cip_line_ids: Vec<RecId>,
    /// Capitalization distribution rules configured for this CIP asset.
    #[field(
        one2many,
        comodel = "account.asset.settlement.rule",
        inverse_name = "asset_id",
        string = "CIP Settlement Rules"
    )]
    pub settlement_rule_ids: Vec<RecId>,
    /// Settlement documents executed from this CIP asset.
    #[field(
        one2many,
        comodel = "account.asset.settlement",
        inverse_name = "cip_asset_id",
        readonly = true,
        string = "CIP Settlements"
    )]
    pub settlement_ids: Vec<RecId>,
    /// Total accumulated CIP costs from posted vendor bills and journal entries.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_cip_totals",
        string = "CIP Accumulated"
    )]
    pub cip_accumulated_amount: Decimal,
    /// Total CIP costs already settled / capitalized to fixed assets or cost centers.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_cip_totals",
        string = "CIP Settled"
    )]
    pub cip_settled_amount: Decimal,
    /// Remaining unsettled CIP balance awaiting capitalization.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_cip_totals",
        string = "CIP Unsettled"
    )]
    pub cip_unsettled_amount: Decimal,
}

#[methods(AccountAsset, addon = "account")]
impl AccountAsset {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let cid = this.env().company_id()?;
        if let Some(cid) = cid {
            if names.iter().any(|n| n == "company_id") && !values.contains_key("company_id") {
                values.insert("company_id".to_owned(), Value::Int(cid));
            }
            if names.iter().any(|n| n == "journal_id") && !values.contains_key("journal_id") {
                let rows = tx
                    .query(
                        "SELECT id::int8 FROM account_journal \
                         WHERE company_id = $1 AND type = 'general' AND active = true \
                         ORDER BY sequence, id LIMIT 1",
                        &[&cid],
                    )
                    .await?;
                if let Some(row) = rows.first() {
                    let jid: i64 = row.try_get(0).map_err(db)?;
                    values.insert("journal_id".to_owned(), Value::Int(jid));
                }
            }
        }
        if names.iter().any(|n| n == "acquisition_date") && !values.contains_key("acquisition_date")
        {
            let today = if let Some(cid) = cid {
                erp_addon_base::company_calendar::business_date(tx, cid).await?
            } else {
                this.env().context_today()?
            };
            values.insert("acquisition_date".to_owned(), Value::Date(today));
        }
        Ok(values)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() && !this.env().su() {
            if vals.contains("state")
                || vals.contains("disposal_date")
                || vals.contains("disposal_move_id")
            {
                return Err(Error::user(
                    "Asset status and disposal fields cannot be modified directly; use asset workflow actions.",
                ));
            }
            let states = this.mapped(tx, AccountAsset::STATE).await?;
            for state in &states {
                if matches!(state.as_deref(), Some("settled" | "disposed" | "close")) {
                    return Err(Error::user(
                        "Settled, disposed, or closed assets cannot be modified.",
                    ));
                }
                if vals.contains("asset_type") && state.as_deref() != Some("draft") {
                    return Err(Error::user(
                        "Asset Type cannot be changed after the asset has left Draft state.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() && !this.env().su() {
            for state in this.mapped(tx, AccountAsset::STATE).await? {
                if state.as_deref() != Some("draft") {
                    return Err(Error::user(
                        "Only draft assets can be deleted; dispose or close active assets instead.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    #[compute(depends = [
        "asset_type",
        "original_value",
        "state",
        "depreciation_line_ids",
        "depreciation_line_ids.amount",
        "depreciation_line_ids.state",
        "cip_line_ids",
        "cip_line_ids.amount",
        "cip_line_ids.settled_amount",
    ])]
    async fn _compute_residual(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountAsset>,
    ) -> Result<()> {
        let originals = this.mapped(tx, AccountAsset::ORIGINAL_VALUE).await?;
        let types = this.mapped(tx, AccountAsset::ASSET_TYPE).await?;
        let states = this.mapped(tx, AccountAsset::STATE).await?;
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|r| r.as_real())
            .map(|r| r.get())
            .collect();
        let mut cip_unsettled = std::collections::HashMap::<i64, Decimal>::new();
        let mut posted_dep = std::collections::HashMap::<i64, Decimal>::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            for row in tx
                .query(
                    "SELECT asset_id::int8, COALESCE(SUM(amount - settled_amount), 0) \
                     FROM account_asset_cip_line \
                     WHERE asset_id = ANY($1::int8[]) GROUP BY asset_id",
                    &[&real_ids],
                )
                .await?
            {
                cip_unsettled.insert(row.try_get(0).map_err(db)?, row.try_get(1).map_err(db)?);
            }
            for row in tx
                .query(
                    "SELECT asset_id::int8, COALESCE(SUM(amount), 0) \
                     FROM account_asset_depreciation_line \
                     WHERE asset_id = ANY($1::int8[]) AND state = 'posted' GROUP BY asset_id",
                    &[&real_ids],
                )
                .await?
            {
                posted_dep.insert(row.try_get(0).map_err(db)?, row.try_get(1).map_err(db)?);
            }
        }
        for (i, ((rid, orig), (atype, state))) in rec_ids
            .iter()
            .zip(originals)
            .zip(types.into_iter().zip(states))
            .enumerate()
        {
            if state.as_deref() == Some("disposed") {
                out.set(i, AccountAsset::VALUE_RESIDUAL, Decimal::ZERO)?;
                continue;
            }
            if atype.as_deref() == Some("cip") {
                let unsettled = rid
                    .as_real()
                    .and_then(|r| cip_unsettled.get(&r.get()).copied())
                    .unwrap_or(Decimal::ZERO);
                out.set(
                    i,
                    AccountAsset::VALUE_RESIDUAL,
                    unsettled.max(Decimal::ZERO),
                )?;
                continue;
            }
            let posted = rid
                .as_real()
                .and_then(|r| posted_dep.get(&r.get()).copied())
                .unwrap_or(Decimal::ZERO);
            out.set(
                i,
                AccountAsset::VALUE_RESIDUAL,
                (orig - posted).max(Decimal::ZERO),
            )?;
        }
        Ok(())
    }

    #[compute(depends = ["cip_line_ids", "cip_line_ids.amount", "cip_line_ids.settled_amount"])]
    async fn _compute_cip_totals(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountAsset>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|r| r.as_real())
            .map(|r| r.get())
            .collect();
        let mut by_asset = std::collections::HashMap::<i64, (Decimal, Decimal)>::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            for row in tx
                .query(
                    "SELECT asset_id::int8, COALESCE(SUM(amount), 0), COALESCE(SUM(settled_amount), 0) \
                     FROM account_asset_cip_line \
                     WHERE asset_id = ANY($1::int8[]) GROUP BY asset_id",
                    &[&real_ids],
                )
                .await?
            {
                by_asset.insert(
                    row.try_get(0).map_err(db)?,
                    (row.try_get(1).map_err(db)?, row.try_get(2).map_err(db)?),
                );
            }
        }
        for (i, rid) in rec_ids.iter().enumerate() {
            let (acc, settled) = rid
                .as_real()
                .and_then(|r| by_asset.get(&r.get()).copied())
                .unwrap_or((Decimal::ZERO, Decimal::ZERO));
            out.set(i, AccountAsset::CIP_ACCUMULATED_AMOUNT, acc)?;
            out.set(i, AccountAsset::CIP_SETTLED_AMOUNT, settled)?;
            out.set(i, AccountAsset::CIP_UNSETTLED_AMOUNT, acc - settled)?;
        }
        Ok(())
    }

    /// Generates (or regenerates) the draft depreciation schedule lines for a fixed asset.
    /// Construction in Progress (CIP) assets are strictly refused from depreciating.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_compute_depreciation_board(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let asset = this.ensure_one()?;
        asset
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        tx.flush().await?;
        let aid = asset.as_records().ids()[0];
        let a_rows = tx
            .query(
                "SELECT COALESCE(asset_type, 'normal')::text, COALESCE(state, 'draft')::text, \
                        original_value, salvage_value, acquisition_date, method_number, \
                        method_period, method::text, COALESCE(method_progress_factor, 0.30), \
                        COALESCE(prorata_temporis, false), currency_id::int8 \
                 FROM account_asset WHERE id = $1 FOR UPDATE",
                &[&aid],
            )
            .await?;
        let Some(arow) = a_rows.first() else {
            return Err(Error::missing("Fixed asset no longer exists."));
        };
        let asset_type: String = arow.try_get(0).map_err(db)?;
        if asset_type == "cip" {
            return Err(Error::user(
                "Construction in Progress (CIP) assets cannot be depreciated until settled to a Fixed Asset.",
            ));
        }
        let state: String = arow.try_get(1).map_err(db)?;
        if matches!(state.as_str(), "close" | "disposed" | "settled") {
            return Err(Error::user(
                "Cannot recompute a closed, settled, or disposed asset.",
            ));
        }
        let orig: Decimal = arow.try_get(2).map_err(db)?;
        let salvage: Decimal = arow.try_get(3).map_err(db)?;
        let start: Date = arow
            .try_get::<_, Option<Date>>(4)
            .map_err(db)?
            .ok_or_else(|| Error::validation("Asset Acquisition Date is required."))?;
        let periods: i32 = arow.try_get(5).map_err(db)?;
        let months: i32 = arow.try_get(6).map_err(db)?;
        let method: String = arow
            .try_get::<_, Option<String>>(7)
            .map_err(db)?
            .ok_or_else(|| Error::validation("Asset depreciation method is required."))?;
        let factor: Decimal = arow.try_get(8).map_err(db)?;
        let prorata: bool = arow.try_get(9).map_err(db)?;
        let currency_id: i64 = arow.try_get(10).map_err(db)?;
        let scale = currency_scale(tx, currency_id).await?;

        // Account for already posted depreciation lines when recomputing (e.g. after post-capitalization).
        let posted_rows = tx
            .query(
                "SELECT count(*)::int4, COALESCE(SUM(amount), 0), MAX(sequence)::int4, MAX(depreciation_date) \
                 FROM account_asset_depreciation_line WHERE asset_id = $1 AND state = 'posted'",
                &[&aid],
            )
            .await?;
        let prow = posted_rows
            .first()
            .ok_or_else(|| Error::invariant("Posted depreciation aggregate returned no row."))?;
        let posted_count: i32 = prow.try_get(0).map_err(db)?;
        let posted_sum: Decimal = prow.try_get(1).map_err(db)?;
        let max_seq: Option<i32> = prow.try_get(2).map_err(db)?;
        let last_posted_date: Option<Date> = prow.try_get(3).map_err(db)?;

        let remaining_depreciable = orig - salvage - posted_sum;
        let remaining_periods = (periods - posted_count).max(1);
        let schedule_start = match last_posted_date {
            Some(d) => add_months(d, months)?,
            None => start,
        };
        let degressive = match method.as_str() {
            "linear" => false,
            "degressive" => true,
            other => {
                return Err(Error::validation(format!(
                    "Unknown depreciation method '{other}'."
                )));
            }
        };
        let schedule = build_depreciation_schedule_scaled(
            remaining_depreciable,
            schedule_start,
            remaining_periods,
            months,
            degressive,
            factor,
            prorata && posted_count == 0,
            scale,
        )?;

        tx.execute(
            "DELETE FROM account_asset_depreciation_line WHERE asset_id = $1 AND state = 'draft'",
            &[&aid],
        )
        .await?;
        let seq_offset = max_seq.unwrap_or(0);
        let mut vals_list = Vec::with_capacity(schedule.len());
        for row in schedule {
            let mut v = RawVals::new();
            v.insert("asset_id".into(), Cell::Id(aid));
            v.insert("sequence".into(), Cell::Int(seq_offset + row.sequence));
            v.insert("depreciation_date".into(), Cell::Date(row.date));
            v.insert("amount".into(), Cell::Decimal(row.amount));
            v.insert(
                "depreciated_value".into(),
                Cell::Decimal(posted_sum + row.cumulative),
            );
            v.insert("remaining_value".into(), Cell::Decimal(row.remaining));
            vals_list.push(v);
        }
        crate::moves::post::strip_default_context(asset.env())
            .records("account.asset.depreciation.line")?
            .create(tx, vals_list)
            .await?;
        tx.invalidate_records(
            asset.as_records(),
            Some(&["depreciation_line_ids", "value_residual"]),
        )
        .await
    }

    /// Confirms the asset: CIP assets enter `cip` (Under Construction) with zero depreciation;
    /// normal fixed assets validate depreciation accounts, compute the schedule if empty, and enter `open`.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_validate(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let asset = this.ensure_one()?;
        asset
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        tx.flush().await?;
        let aid = asset.as_records().ids()[0];
        let a_rows = tx
            .query(
                "SELECT COALESCE(asset_type, 'normal')::text, COALESCE(state, 'draft')::text, \
                        account_depreciation_id::int8, account_depreciation_expense_id::int8 \
                 FROM account_asset WHERE id = $1 FOR UPDATE",
                &[&aid],
            )
            .await?;
        let Some(arow) = a_rows.first() else {
            return Err(Error::missing("Asset no longer exists."));
        };
        let asset_type: String = arow.try_get(0).map_err(db)?;
        let state: String = arow.try_get(1).map_err(db)?;
        let dep_acc: Option<i64> = arow.try_get(2).map_err(db)?;
        let exp_acc: Option<i64> = arow.try_get(3).map_err(db)?;
        if state != "draft" {
            return Err(Error::user(format!(
                "Only a draft asset can be confirmed (current status: '{state}')."
            )));
        }
        let uid = asset.env().uid();
        if asset_type == "cip" {
            tx.execute(
                "UPDATE account_asset SET state = 'cip', write_uid = $2, \
                        write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state = 'draft'",
                &[&aid, &uid],
            )
            .await?;
            return tx
                .invalidate_records(asset.as_records(), Some(&["state"]))
                .await;
        }
        if dep_acc.is_none() || exp_acc.is_none() {
            return Err(Error::validation(
                "Depreciable fixed assets require both an Accumulated Depreciation Account and a Depreciation Expense Account.",
            ));
        }
        let count: i64 = tx
            .query(
                "SELECT count(*)::int8 FROM account_asset_depreciation_line WHERE asset_id = $1",
                &[&aid],
            )
            .await?[0]
            .try_get(0)
            .map_err(db)?;
        if count == 0 {
            Self::action_compute_depreciation_board(asset, tx).await?;
        }
        tx.execute(
            "UPDATE account_asset SET state = 'open', write_uid = $2, \
                    write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1 AND state = 'draft'",
            &[&aid, &uid],
        )
        .await?;
        tx.invalidate_records(asset.as_records(), Some(&["state"]))
            .await
    }

    /// Posts the next unposted depreciation entry to the General Ledger (Debit Expense with
    /// the asset's Cost Center, Credit Accumulated Depreciation) and closes the asset once complete.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_post_next_depreciation(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let asset = this.ensure_one()?;
        asset
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        tx.flush().await?;
        let aid = asset.as_records().ids()[0];
        post_asset_depreciation(tx, asset, aid, None).await
    }

    /// Scraps / disposes a running fixed asset by derecognizing original cost & accumulated
    /// depreciation and posting the remaining net book value to Disposal Gain/Loss.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_dispose_asset(
        this: &RecordSet<AccountAsset>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let asset = this.ensure_one()?;
        asset
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        tx.flush().await?;
        let aid = asset.as_records().ids()[0];
        let a_rows = tx
            .query(
                "SELECT name::text, company_id::int8, currency_id::int8, journal_id::int8, \
                        account_asset_id::int8, account_depreciation_id::int8, \
                        account_disposal_gain_loss_id::int8, analytic_account_id::int8, \
                        original_value, COALESCE(asset_type, 'normal')::text, \
                        COALESCE(state, 'draft')::text \
                 FROM account_asset WHERE id = $1 FOR UPDATE",
                &[&aid],
            )
            .await?;
        let Some(arow) = a_rows.first() else {
            return Err(Error::missing("Asset no longer exists."));
        };
        let name: String = arow.try_get(0).map_err(db)?;
        let company_id: i64 = arow.try_get(1).map_err(db)?;
        let currency_id: i64 = arow.try_get(2).map_err(db)?;
        let journal_id: i64 = arow.try_get(3).map_err(db)?;
        let asset_acc: i64 = arow.try_get(4).map_err(db)?;
        let dep_acc: Option<i64> = arow.try_get(5).map_err(db)?;
        let gain_loss_acc: Option<i64> = arow.try_get(6).map_err(db)?;
        let cost_center_id: Option<i64> = arow.try_get(7).map_err(db)?;
        let original: Decimal = arow.try_get(8).map_err(db)?;
        let asset_type: String = arow.try_get(9).map_err(db)?;
        let state: String = arow.try_get(10).map_err(db)?;

        if asset_type == "cip" {
            return Err(Error::user(
                "Use CIP Settlement (to a Cost Center or Fixed Asset) instead of Asset Disposal for a CIP asset.",
            ));
        }
        if state != "open" {
            return Err(Error::user("Only a running fixed asset can be disposed."));
        }
        if original <= Decimal::ZERO {
            return Err(Error::validation(
                "Asset Original Value must be greater than zero to dispose.",
            ));
        }
        let dep_acc = dep_acc
            .ok_or_else(|| Error::validation("Asset has no Accumulated Depreciation account."))?;
        let dep_agg = tx
            .query(
                "SELECT COALESCE(SUM(amount), 0), MAX(depreciation_date) \
                 FROM account_asset_depreciation_line \
                 WHERE asset_id = $1 AND state = 'posted'",
                &[&aid],
            )
            .await?;
        let accum_dep: Decimal = dep_agg[0].try_get(0).map_err(db)?;
        let last_posted_date: Option<Date> = dep_agg[0].try_get(1).map_err(db)?;
        if accum_dep > original {
            return Err(Error::invariant(format!(
                "Asset '{name}' has accumulated depreciation ({accum_dep}) exceeding original cost ({original})."
            )));
        }
        let nbv = original - accum_dep;
        if !nbv.is_zero() && gain_loss_acc.is_none() {
            return Err(Error::validation(
                "Set an explicit Disposal Gain/Loss Account on the asset before disposing a non-zero Net Book Value.",
            ));
        }
        let disposal_date = erp_addon_base::company_calendar::business_date(tx, company_id).await?;
        if let Some(last_dt) = last_posted_date
            && disposal_date.days() < last_dt.days()
        {
            return Err(Error::user(format!(
                "Disposal date ({disposal_date}) cannot be earlier than the last posted depreciation date ({last_dt})."
            )));
        }

        let env = crate::moves::post::strip_default_context(asset.env());
        let label = format!("Asset Disposal: {name}");
        let mut mvals = RawVals::new();
        mvals.insert("move_type".into(), Cell::Str("entry".into()));
        mvals.insert("journal_id".into(), Cell::Id(journal_id));
        mvals.insert("company_id".into(), Cell::Id(company_id));
        mvals.insert("currency_id".into(), Cell::Id(currency_id));
        mvals.insert("date".into(), Cell::Date(disposal_date));
        mvals.insert("ref".into(), Cell::Str(label.as_str().into()));

        let unchecked_moves = crate::moves::unchecked(&env.records("account.move")?);
        let move_id = unchecked_moves.create(tx, vec![mvals]).await?.ids()[0];

        let mut lines = Vec::new();
        // Cr Fixed Asset original cost
        let mut cr_asset = RawVals::new();
        cr_asset.insert("move_id".into(), Cell::Id(move_id));
        cr_asset.insert("account_id".into(), Cell::Id(asset_acc));
        cr_asset.insert("name".into(), Cell::Str(label.as_str().into()));
        cr_asset.insert("currency_id".into(), Cell::Id(currency_id));
        cr_asset.insert("balance".into(), Cell::Decimal(-original));
        cr_asset.insert("amount_currency".into(), Cell::Decimal(-original));
        lines.push(cr_asset);

        // Dr Accumulated Depreciation (if any)
        if !accum_dep.is_zero() {
            let mut dr_dep = RawVals::new();
            dr_dep.insert("move_id".into(), Cell::Id(move_id));
            dr_dep.insert("account_id".into(), Cell::Id(dep_acc));
            dr_dep.insert("name".into(), Cell::Str(label.as_str().into()));
            dr_dep.insert("currency_id".into(), Cell::Id(currency_id));
            dr_dep.insert("balance".into(), Cell::Decimal(accum_dep));
            dr_dep.insert("amount_currency".into(), Cell::Decimal(accum_dep));
            lines.push(dr_dep);
        }

        // Dr Loss on Disposal for remaining NBV (if any)
        if !nbv.is_zero()
            && let Some(gl_acc) = gain_loss_acc
        {
            let mut dr_loss = RawVals::new();
            dr_loss.insert("move_id".into(), Cell::Id(move_id));
            dr_loss.insert("account_id".into(), Cell::Id(gl_acc));
            dr_loss.insert("name".into(), Cell::Str(label.as_str().into()));
            dr_loss.insert("currency_id".into(), Cell::Id(currency_id));
            dr_loss.insert("balance".into(), Cell::Decimal(nbv));
            dr_loss.insert("amount_currency".into(), Cell::Decimal(nbv));
            if let Some(cc) = cost_center_id {
                dr_loss.insert("analytic_account_id".into(), Cell::Id(cc));
            }
            lines.push(dr_loss);
        }

        let unchecked_lines = crate::moves::unchecked(&env.records("account.move.line")?);
        unchecked_lines.create(tx, lines).await?;
        post_moves(tx, &env.records("account.move")?.browse([move_id])?).await?;

        // Remove unposted future depreciation lines.
        tx.execute(
            "DELETE FROM account_asset_depreciation_line WHERE asset_id = $1 AND state = 'draft'",
            &[&aid],
        )
        .await?;
        let uid = asset.env().uid();
        tx.execute(
            "UPDATE account_asset \
             SET state = 'disposed', disposal_date = $2, disposal_move_id = $3, \
                 write_uid = $4, write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1 AND state = 'open'",
            &[&aid, &disposal_date, &move_id, &uid],
        )
        .await?;
        tx.invalidate_records(
            asset.as_records(),
            Some(&[
                "state",
                "disposal_date",
                "disposal_move_id",
                "value_residual",
                "depreciation_line_ids",
            ]),
        )
        .await
    }

    /// Manually closes a running asset (refuses draft/settled/disposed or CIP with unsettled balance).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_close(this: &RecordSet<AccountAsset>, tx: &mut Transaction) -> Result<()> {
        let asset = this.ensure_one()?;
        asset
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        tx.flush().await?;
        let aid = asset.as_records().ids()[0];
        let rows = tx
            .query(
                "SELECT COALESCE(state, 'draft')::text, COALESCE(asset_type, 'normal')::text \
                 FROM account_asset WHERE id = $1 FOR UPDATE",
                &[&aid],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Asset no longer exists."));
        };
        let state: String = row.try_get(0).map_err(db)?;
        let atype: String = row.try_get(1).map_err(db)?;
        if state != "open" {
            return Err(Error::user(format!(
                "Only a running asset can be closed (current status: '{state}')."
            )));
        }
        if atype == "cip" {
            let unsettled: Decimal = tx
                .query(
                    "SELECT COALESCE(SUM(amount - settled_amount), 0) \
                     FROM account_asset_cip_line WHERE asset_id = $1",
                    &[&aid],
                )
                .await?[0]
                .try_get(0)
                .map_err(db)?;
            if !unsettled.is_zero() {
                return Err(Error::user(format!(
                    "Cannot close a CIP asset with unsettled balance ({unsettled}). Settle it first."
                )));
            }
        }
        let uid = asset.env().uid();
        tx.execute(
            "UPDATE account_asset SET state = 'close', write_uid = $2, \
                    write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1 AND state = 'open'",
            &[&aid, &uid],
        )
        .await?;
        tx.invalidate_records(asset.as_records(), Some(&["state"]))
            .await
    }
}

async fn post_asset_depreciation(
    tx: &mut Transaction,
    asset: &RecordSet<AccountAsset>,
    aid: i64,
    specific_line_id: Option<i64>,
) -> Result<()> {
    let a_rows = tx
        .query(
            "SELECT name::text, company_id::int8, currency_id::int8, journal_id::int8, \
                    account_depreciation_id::int8, account_depreciation_expense_id::int8, \
                    analytic_account_id::int8, COALESCE(asset_type, 'normal')::text, \
                    COALESCE(state, 'draft')::text \
             FROM account_asset WHERE id = $1 FOR UPDATE",
            &[&aid],
        )
        .await?;
    let Some(arow) = a_rows.first() else {
        return Err(Error::missing("Asset no longer exists."));
    };
    let name: String = arow.try_get(0).map_err(db)?;
    let company_id: i64 = arow.try_get(1).map_err(db)?;
    let currency_id: i64 = arow.try_get(2).map_err(db)?;
    let journal_id: i64 = arow.try_get(3).map_err(db)?;
    let dep_acc: Option<i64> = arow.try_get(4).map_err(db)?;
    let exp_acc: Option<i64> = arow.try_get(5).map_err(db)?;
    let cost_center_id: Option<i64> = arow.try_get(6).map_err(db)?;
    let asset_type: String = arow.try_get(7).map_err(db)?;
    let state: String = arow.try_get(8).map_err(db)?;

    if asset_type == "cip" {
        return Err(Error::user(
            "Construction in Progress (CIP) assets cannot post depreciation.",
        ));
    }
    if state != "open" {
        return Err(Error::user(
            "Confirm the asset before posting depreciation entries.",
        ));
    }
    let dep_acc = dep_acc.ok_or_else(|| Error::validation("Asset has no depreciation account."))?;
    let exp_acc =
        exp_acc.ok_or_else(|| Error::validation("Asset has no depreciation expense account."))?;

    let next_rows = if let Some(target_lid) = specific_line_id {
        tx.query(
            "SELECT id::int8, sequence, depreciation_date, COALESCE(amount, 0) \
             FROM account_asset_depreciation_line \
             WHERE id = $1 AND asset_id = $2 AND state = 'draft' FOR UPDATE",
            &[&target_lid, &aid],
        )
        .await?
    } else {
        tx.query(
            "SELECT id::int8, sequence, depreciation_date, COALESCE(amount, 0) \
             FROM account_asset_depreciation_line \
             WHERE asset_id = $1 AND state = 'draft' \
             ORDER BY sequence, id LIMIT 1 FOR UPDATE",
            &[&aid],
        )
        .await?
    };
    let Some(nrow) = next_rows.first() else {
        return Err(Error::user(
            "All depreciation entries for this asset have already been posted.",
        ));
    };
    let line_id: i64 = nrow.try_get(0).map_err(db)?;
    let seq: i32 = nrow.try_get(1).map_err(db)?;
    let dep_date: Date = nrow.try_get(2).map_err(db)?;
    let amount: Decimal = nrow.try_get(3).map_err(db)?;

    let env = crate::moves::post::strip_default_context(asset.env());
    let label = format!("Depreciation {seq}: {name}");
    let mut mvals = RawVals::new();
    mvals.insert("move_type".into(), Cell::Str("entry".into()));
    mvals.insert("journal_id".into(), Cell::Id(journal_id));
    mvals.insert("company_id".into(), Cell::Id(company_id));
    mvals.insert("currency_id".into(), Cell::Id(currency_id));
    mvals.insert("date".into(), Cell::Date(dep_date));
    mvals.insert("ref".into(), Cell::Str(label.as_str().into()));

    let unchecked_moves = crate::moves::unchecked(&env.records("account.move")?);
    let created_move = unchecked_moves.create(tx, vec![mvals]).await?;
    let move_id = created_move.ids()[0];

    let mut debit_line = RawVals::new();
    debit_line.insert("move_id".into(), Cell::Id(move_id));
    debit_line.insert("account_id".into(), Cell::Id(exp_acc));
    debit_line.insert("name".into(), Cell::Str(label.as_str().into()));
    debit_line.insert("currency_id".into(), Cell::Id(currency_id));
    debit_line.insert("balance".into(), Cell::Decimal(amount));
    debit_line.insert("amount_currency".into(), Cell::Decimal(amount));
    if let Some(cc) = cost_center_id {
        debit_line.insert("analytic_account_id".into(), Cell::Id(cc));
    }

    let mut credit_line = RawVals::new();
    credit_line.insert("move_id".into(), Cell::Id(move_id));
    credit_line.insert("account_id".into(), Cell::Id(dep_acc));
    credit_line.insert("name".into(), Cell::Str(label.as_str().into()));
    credit_line.insert("currency_id".into(), Cell::Id(currency_id));
    credit_line.insert("balance".into(), Cell::Decimal(-amount));
    credit_line.insert("amount_currency".into(), Cell::Decimal(-amount));

    let unchecked_lines = crate::moves::unchecked(&env.records("account.move.line")?);
    unchecked_lines
        .create(tx, vec![debit_line, credit_line])
        .await?;
    post_moves(tx, &env.records("account.move")?.browse([move_id])?).await?;

    let uid = asset.env().uid();
    tx.execute(
        "UPDATE account_asset_depreciation_line \
         SET state = 'posted', move_id = $2, write_uid = $3, write_date = now() AT TIME ZONE 'UTC' \
         WHERE id = $1 AND state = 'draft'",
        &[&line_id, &move_id, &uid],
    )
    .await?;
    let remaining: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM account_asset_depreciation_line \
             WHERE asset_id = $1 AND state = 'draft'",
            &[&aid],
        )
        .await?[0]
        .try_get(0)
        .map_err(db)?;
    if remaining == 0 {
        tx.execute(
            "UPDATE account_asset SET state = 'close', write_uid = $2, \
                    write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1 AND state = 'open'",
            &[&aid, &uid],
        )
        .await?;
    }
    tx.invalidate_records(
        asset.as_records(),
        Some(&["state", "value_residual", "depreciation_line_ids"]),
    )
    .await?;
    tx.invalidate_model(
        "account.asset.depreciation.line",
        Some(&["state", "move_id"]),
    )
    .await
}

/// `account.asset.depreciation.line`: one scheduled depreciation installment of an asset.
#[model(
    name = "account.asset.depreciation.line",
    description = "Asset Depreciation Line",
    order = "sequence, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountAssetDepreciationLine {
    /// Parent asset.
    #[field(
        many2one,
        comodel = "account.asset",
        required = true,
        ondelete = "cascade",
        string = "Asset"
    )]
    pub asset_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "asset_id.company_id",
        store = true,
        string = "Company"
    )]
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
    /// Installment number.
    #[field(integer, default = 1, string = "Period")]
    pub sequence: i32,
    /// Scheduled depreciation date.
    #[field(date, required = true, string = "Depreciation Date")]
    pub depreciation_date: Option<Date>,
    /// Depreciation amount for this period.
    #[field(monetary, currency_field = "currency_id", string = "Depreciation")]
    pub amount: Decimal,
    /// Cumulative depreciation after this period.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Cumulative"
    )]
    pub depreciated_value: Decimal,
    /// Remaining depreciable balance after this period.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Remaining"
    )]
    pub remaining_value: Decimal,
    /// Posted journal entry when booked.
    #[field(
        many2one,
        comodel = "account.move",
        readonly = true,
        ondelete = "set null",
        string = "Journal Entry"
    )]
    pub move_id: Option<RecId>,
    /// Draft until posted.
    #[field(
        selection,
        selection = [("draft", "Scheduled"), ("posted", "Posted")],
        required = true,
        readonly = true,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
}

#[methods(AccountAssetDepreciationLine, addon = "account")]
impl AccountAssetDepreciationLine {}

/// `account.asset.depreciation.run`: periodic batch depreciation execution posting all scheduled
/// installments up to `cutoff_date` for a company.
#[model(
    name = "account.asset.depreciation.run",
    description = "Periodic Depreciation Run",
    order = "cutoff_date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountAssetDepreciationRun {
    /// Run label (e.g. "Nov 2026 Depreciation Run").
    #[field(char, required = true, string = "Run Name")]
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
    /// Post all scheduled depreciation lines with date <= cutoff_date.
    #[field(date, required = true, string = "Cutoff Date")]
    pub cutoff_date: Option<Date>,
    /// Run status.
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
    /// Count of depreciation journal entries posted by this run.
    #[field(integer, readonly = true, default = 0, string = "Entries Posted")]
    pub entries_posted: i32,
    /// Total depreciation posted by this run.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Total Posted"
    )]
    pub total_depreciation: Decimal,
}

#[methods(AccountAssetDepreciationRun, addon = "account")]
impl AccountAssetDepreciationRun {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountAssetDepreciationRun>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let cid = this.env().company_id()?;
        if names.iter().any(|n| n == "company_id")
            && !values.contains_key("company_id")
            && let Some(cid) = cid
        {
            values.insert("company_id".to_owned(), Value::Int(cid));
        }
        if names.iter().any(|n| n == "cutoff_date") && !values.contains_key("cutoff_date") {
            let today = if let Some(cid) = cid {
                erp_addon_base::company_calendar::business_date(tx, cid).await?
            } else {
                this.env().context_today()?
            };
            values.insert("cutoff_date".to_owned(), Value::Date(today));
        }
        Ok(values)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountAssetDepreciationRun>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() && !this.env().su() {
            if vals.contains("state")
                || vals.contains("entries_posted")
                || vals.contains("total_depreciation")
            {
                return Err(Error::user(
                    "Depreciation run status and totals cannot be edited directly.",
                ));
            }
            for state in this.mapped(tx, AccountAssetDepreciationRun::STATE).await? {
                if state.as_deref() != Some("draft") {
                    return Err(Error::user("Posted depreciation runs cannot be modified."));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountAssetDepreciationRun>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() && !this.env().su() {
            for state in this.mapped(tx, AccountAssetDepreciationRun::STATE).await? {
                if state.as_deref() != Some("draft") {
                    return Err(Error::user("Posted depreciation runs cannot be deleted."));
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Posts every scheduled depreciation line dated on or before `cutoff_date` for open
    /// depreciable fixed assets in the company (capped per run for bounded transaction work).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_post_due_depreciations(
        this: &RecordSet<AccountAssetDepreciationRun>,
        tx: &mut Transaction,
    ) -> Result<()> {
        const MAX_LINES_PER_RUN: i64 = 500;
        let run = this.ensure_one()?;
        run.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let run_id = run.as_records().ids()[0];
        let run_rows = tx
            .query(
                "SELECT state::text, company_id::int8, cutoff_date \
                 FROM account_asset_depreciation_run WHERE id = $1 FOR UPDATE",
                &[&run_id],
            )
            .await?;
        let Some(rrow) = run_rows.first() else {
            return Err(Error::missing("Depreciation run no longer exists."));
        };
        let state: String = rrow.try_get(0).map_err(db)?;
        if state != "draft" {
            return Err(Error::user(
                "This depreciation run has already been posted.",
            ));
        }
        let company_id: i64 = rrow.try_get(1).map_err(db)?;
        let cutoff: Date = rrow
            .try_get::<_, Option<Date>>(2)
            .map_err(db)?
            .ok_or_else(|| Error::validation("Cutoff Date is required."))?;

        let due_rows = tx
            .query(
                "SELECT l.id::int8, l.asset_id::int8, COALESCE(l.amount, 0) \
                 FROM account_asset_depreciation_line l \
                 JOIN account_asset a ON a.id = l.asset_id \
                 WHERE a.company_id = $1 AND a.state = 'open' \
                   AND COALESCE(a.asset_type, 'normal') = 'normal' \
                   AND l.state = 'draft' AND l.depreciation_date <= $2 \
                 ORDER BY l.asset_id, l.depreciation_date, l.sequence, l.id \
                 LIMIT $3",
                &[&company_id, &cutoff, &(MAX_LINES_PER_RUN + 1)],
            )
            .await?;
        if due_rows.is_empty() {
            return Err(Error::user(format!(
                "No scheduled depreciation entries are due on or before {cutoff}."
            )));
        }
        if due_rows.len() as i64 > MAX_LINES_PER_RUN {
            return Err(Error::user(format!(
                "More than {MAX_LINES_PER_RUN} depreciation entries are due on or before {cutoff}; run depreciation for an earlier cutoff date in smaller batches."
            )));
        }

        let mut posted_count: i32 = 0;
        let mut total = Decimal::ZERO;
        let env = run.env().clone();
        for row in due_rows {
            let line_id: i64 = row.try_get(0).map_err(db)?;
            let aid: i64 = row.try_get(1).map_err(db)?;
            let amt: Decimal = row.try_get(2).map_err(db)?;
            let rec: RecordSet<AccountAsset> =
                env.records("account.asset")?.browse([aid])?.try_into()?;
            post_asset_depreciation(tx, &rec, aid, Some(line_id)).await?;
            posted_count += 1;
            total += amt;
        }

        let uid = run.env().uid();
        let updated = tx
            .execute(
                "UPDATE account_asset_depreciation_run \
                 SET state = 'posted', entries_posted = $2, total_depreciation = $3, \
                     write_uid = $4, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state = 'draft'",
                &[&run_id, &posted_count, &total, &uid],
            )
            .await?;
        if updated != 1 {
            return Err(Error::conflict(
                "Depreciation run state changed concurrently; retry.",
            ));
        }
        tx.invalidate_records(
            run.as_records(),
            Some(&["state", "entries_posted", "total_depreciation"]),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{build_depreciation_schedule, build_depreciation_schedule_with_prorata};
    use erp_sdk::{Date, Decimal};
    use std::str::FromStr;

    #[test]
    fn linear_and_degressive_schedules_conserve_exact_total() {
        let start = Date::from_ymd(2026, 1, 1).unwrap();
        let depreciable = Decimal::from_str("1000.00").unwrap();
        let linear =
            build_depreciation_schedule(depreciable, start, 3, 12, false, Decimal::ZERO).unwrap();
        assert_eq!(linear.len(), 3);
        let sum_linear: Decimal = linear.iter().map(|r| r.amount).sum();
        assert_eq!(sum_linear, depreciable);
        assert_eq!(linear.last().unwrap().remaining, Decimal::ZERO);

        let deg = build_depreciation_schedule(
            depreciable,
            start,
            4,
            12,
            true,
            Decimal::from_str("0.40").unwrap(),
        )
        .unwrap();
        let sum_deg: Decimal = deg.iter().map(|r| r.amount).sum();
        assert_eq!(sum_deg, depreciable);
        assert_eq!(deg[0].amount, Decimal::from_str("400.00").unwrap());
        assert_eq!(deg.last().unwrap().remaining, Decimal::ZERO);
    }

    #[test]
    fn prorata_temporis_prorates_first_period_and_conserves_exact_total() {
        let start = Date::from_ymd(2026, 1, 16).unwrap(); // 15 days left in month => 50%
        let depreciable = Decimal::from_str("1200.00").unwrap();
        let rows = build_depreciation_schedule_with_prorata(
            depreciable,
            start,
            4,
            1,
            false,
            Decimal::ZERO,
            true,
        )
        .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].amount, Decimal::from_str("150.00").unwrap());
        let sum: Decimal = rows.iter().map(|r| r.amount).sum();
        assert_eq!(sum, depreciable);
        assert_eq!(rows.last().unwrap().remaining, Decimal::ZERO);
    }
}
