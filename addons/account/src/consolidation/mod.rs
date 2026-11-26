// SPDX-License-Identifier: MIT OR Apache-2.0
//! Group Financial Consolidation Master Data (`account.consolidation.group`,
//! `account.consolidation.unit`, `account.consolidation.account`,
//! `account.consolidation.account.map`) and Trading Partner intercompany tagging.

pub mod engine;

use erp_addon_base::ResPartner;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

pub(crate) fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Consolidation query failed: {err}"))
}

pub(crate) use crate::moves::post::strip_default_context;

/// Ensures the caller is authorized for every legal entity (company) in `group_id`.
pub(crate) async fn ensure_group_company_access(
    tx: &mut Transaction,
    env: &Env,
    group_id: i64,
) -> Result<()> {
    if env.su() {
        return Ok(());
    }
    let unit_rows = tx
        .query(
            "SELECT company_id::int8 FROM account_consolidation_unit WHERE group_id = $1",
            &[&group_id],
        )
        .await?;
    if unit_rows.is_empty() {
        return Ok(());
    }
    let allowed: std::collections::BTreeSet<i64> = tx
        .companies(env)
        .await?
        .into_iter()
        .map(|c| c.get())
        .collect();
    for row in unit_rows {
        let cid: i64 = row.try_get(0).map_err(db)?;
        if !allowed.contains(&cid) {
            return Err(Error::access(format!(
                "You do not have access to member company {cid} in this consolidation group."
            )));
        }
    }
    Ok(())
}

/// Extends `res.partner` with a Trading Partner Company link for automatic intercompany
/// transaction identification and elimination across group entities.
#[extend(ResPartner, addon = "account")]
pub struct ResPartnerTradingPartner {
    /// Group company represented by this contact for intercompany reconciliation & elimination.
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "set null",
        index = "btree_not_null",
        string = "Trading Partner Company"
    )]
    pub trading_partner_company_id: Option<RecId>,
}

/// `account.consolidation.group`: corporate consolidation scope, reporting currency, FX rate
/// types (closing / monthly average / historical), and intercompany tolerance.
#[model(
    name = "account.consolidation.group",
    description = "Consolidation Group",
    order = "code, name, id",
    keys = [key(
        name = "_unique_code",
        fields = ["code"],
        message = "Consolidation Group code must be unique."
    )]
)]
pub struct AccountConsolidationGroup {
    /// Group name (e.g. "Group Holding Consolidated").
    #[field(char, required = true, string = "Group Name")]
    pub name: Option<Arc<str>>,
    /// Short group code (e.g. "GRP-HOLDING").
    #[field(char, required = true, index = true, string = "Code")]
    pub code: Option<Arc<str>>,
    /// Group presentation / reporting currency.
    #[field(
        many2one,
        comodel = "res.currency",
        required = true,
        ondelete = "restrict",
        string = "Group Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Rate type code used for closing Balance Sheet accounts (default `closing`).
    #[field(
        char,
        required = true,
        default = "closing",
        string = "Closing Rate Type"
    )]
    pub closing_rate_type_code: Option<Arc<str>>,
    /// Rate type code used for Profit & Loss accounts (default `monthly_average`).
    #[field(
        char,
        required = true,
        default = "monthly_average",
        string = "Average Rate Type"
    )]
    pub average_rate_type_code: Option<Arc<str>>,
    /// Rate type code used for historical Share Capital / Equity (default `historical`).
    #[field(
        char,
        required = true,
        default = "historical",
        string = "Historical Rate Type"
    )]
    pub historical_rate_type_code: Option<Arc<str>>,
    /// Fall back to `spot` rate if a typed rate row is not yet maintained for a date.
    #[field(boolean, default = true, string = "Fallback to Spot Rate")]
    pub fallback_to_spot: bool,
    /// Intercompany mismatch tolerance threshold in group currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "IC Variance Tolerance"
    )]
    pub ic_tolerance: Decimal,
    /// Active flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Legal entities (parent & subsidiaries) in this consolidation scope.
    #[field(
        one2many,
        comodel = "account.consolidation.unit",
        inverse_name = "group_id",
        string = "Consolidation Units"
    )]
    pub unit_ids: Vec<RecId>,
    /// Unified Group Chart of Accounts (Financial Statement Items).
    #[field(
        one2many,
        comodel = "account.consolidation.account",
        inverse_name = "group_id",
        string = "Group FS Chart"
    )]
    pub account_ids: Vec<RecId>,
    /// Account mappings from local entity charts into Group FS Items.
    #[field(
        one2many,
        comodel = "account.consolidation.account.map",
        inverse_name = "group_id",
        string = "Entity Account Mappings"
    )]
    pub mapping_ids: Vec<RecId>,
    /// Consolidation execution runs.
    #[field(
        one2many,
        comodel = "account.consolidation.run",
        inverse_name = "group_id",
        string = "Consolidation Runs"
    )]
    pub run_ids: Vec<RecId>,
}

/// Standard Group FS Items seeded by `action_bootstrap_default_chart`.
const DEFAULT_FS_ITEMS: &[(&str, &str, i32, &str, &str, &str)] = &[
    // (code, name, seq, section, rate_method, ic_role)
    (
        "1100",
        "Cash and Bank Equivalents",
        10,
        "asset_current",
        "closing",
        "none",
    ),
    (
        "1200",
        "Trade Receivables",
        20,
        "asset_current",
        "closing",
        "none",
    ),
    (
        "1250",
        "Intercompany Receivables",
        25,
        "asset_current",
        "closing",
        "ic_receivable",
    ),
    (
        "1300",
        "Other Current Assets & Inventory",
        30,
        "asset_current",
        "closing",
        "none",
    ),
    (
        "1500",
        "Fixed & Non-Current Assets",
        40,
        "asset_non_current",
        "closing",
        "none",
    ),
    (
        "1550",
        "Investment in Subsidiaries",
        45,
        "asset_non_current",
        "historical",
        "investment_in_sub",
    ),
    (
        "1600",
        "Goodwill on Consolidation",
        50,
        "goodwill",
        "closing",
        "none",
    ),
    (
        "2100",
        "Trade Payables",
        60,
        "liability_current",
        "closing",
        "none",
    ),
    (
        "2150",
        "Intercompany Payables",
        65,
        "liability_current",
        "closing",
        "ic_payable",
    ),
    (
        "2200",
        "Other Current Liabilities",
        70,
        "liability_current",
        "closing",
        "none",
    ),
    (
        "2500",
        "Non-Current Liabilities",
        80,
        "liability_non_current",
        "closing",
        "none",
    ),
    (
        "3100",
        "Share Capital",
        90,
        "equity_capital",
        "historical",
        "none",
    ),
    (
        "3200",
        "Retained Earnings & Reserves",
        100,
        "equity_retained",
        "historical",
        "none",
    ),
    (
        "3300",
        "Cumulative Translation Adjustment (CTA)",
        110,
        "equity_cta",
        "computed_cta",
        "none",
    ),
    (
        "3400",
        "Non-Controlling Interest (Equity)",
        120,
        "equity_nci",
        "closing",
        "none",
    ),
    (
        "4100",
        "Operating Revenue",
        130,
        "income",
        "average",
        "none",
    ),
    (
        "4150",
        "Intercompany Revenue",
        135,
        "income",
        "average",
        "ic_revenue",
    ),
    ("4200", "Other Income", 140, "income", "average", "none"),
    ("5100", "Cost of Revenue", 150, "cogs", "average", "none"),
    (
        "5150",
        "Intercompany Cost & Expenses",
        155,
        "expense",
        "average",
        "ic_expense",
    ),
    (
        "5200",
        "Operating Expenses",
        160,
        "expense",
        "average",
        "none",
    ),
    (
        "5300",
        "Depreciation & Amortization",
        170,
        "expense",
        "average",
        "none",
    ),
    (
        "5900",
        "Intercompany Elimination Variance",
        180,
        "expense",
        "average",
        "ic_variance",
    ),
    (
        "5950",
        "Non-Controlling Interest Share of Profit",
        190,
        "nci_profit",
        "average",
        "none",
    ),
];

#[methods(AccountConsolidationGroup, addon = "account")]
impl AccountConsolidationGroup {
    #[constrains("ic_tolerance")]
    async fn _check_ic_tolerance(
        this: &RecordSet<AccountConsolidationGroup>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for tol in this
            .mapped(tx, AccountConsolidationGroup::IC_TOLERANCE)
            .await?
        {
            if tol < Decimal::ZERO {
                return Err(Error::validation(
                    "IC Variance Tolerance cannot be negative.",
                ));
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountConsolidationGroup>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            for gid in this.as_records().ids() {
                ensure_group_company_access(tx, this.env(), gid).await?;
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountConsolidationGroup>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            for gid in this.as_records().ids() {
                ensure_group_company_access(tx, this.env(), gid).await?;
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountConsolidationGroup>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "currency_id")
            && !values.contains_key("currency_id")
            && let Some(cid) = this.env().company_id()?
        {
            let rows = tx
                .query(
                    "SELECT currency_id::int8 FROM res_company WHERE id = $1",
                    &[&cid],
                )
                .await?;
            if let Some(row) = rows.first() {
                let cur: i64 = row.try_get(0).map_err(db)?;
                values.insert("currency_id".to_owned(), Value::Int(cur));
            }
        }
        Ok(values)
    }

    /// Creates the standard Group Financial Statement Chart (24 IFRS/PSAK consolidation items)
    /// idempotently for this group.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_bootstrap_default_chart(
        this: &RecordSet<AccountConsolidationGroup>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let grp = this.ensure_one()?;
        grp.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let gid = grp.as_records().ids()[0];
        ensure_group_company_access(tx, grp.env(), gid).await?;

        let existing: std::collections::BTreeSet<String> = tx
            .query(
                "SELECT code::text FROM account_consolidation_account WHERE group_id = $1",
                &[&gid],
            )
            .await?
            .into_iter()
            .map(|r| r.try_get(0).map_err(db))
            .collect::<Result<_>>()?;

        let mut vals_list = Vec::new();
        for &(code, name, seq, section, rate_method, ic_role) in DEFAULT_FS_ITEMS {
            if existing.contains(code) {
                continue;
            }
            let mut v = RawVals::new();
            v.insert("group_id".into(), Cell::Id(gid));
            v.insert("code".into(), Cell::Str(code.into()));
            v.insert("name".into(), Cell::Str(name.into()));
            v.insert("sequence".into(), Cell::Int(seq));
            v.insert("section".into(), Cell::Str(section.into()));
            v.insert("rate_method".into(), Cell::Str(rate_method.into()));
            v.insert("ic_role".into(), Cell::Str(ic_role.into()));
            vals_list.push(v);
        }

        if !vals_list.is_empty() {
            let clean_env = strip_default_context(grp.env());
            clean_env
                .records("account.consolidation.account")?
                .create(tx, vals_list)
                .await?;
        }
        tx.invalidate_records(grp.as_records(), Some(&["account_ids"]))
            .await
    }

    /// Automatically maps every member unit's local Chart of Accounts (`account.account`) to
    /// the Group FS Items based on `account_type`, skipping accounts already mapped.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_auto_map_units(
        this: &RecordSet<AccountConsolidationGroup>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let grp = this.ensure_one()?;
        grp.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let gid = grp.as_records().ids()[0];
        ensure_group_company_access(tx, grp.env(), gid).await?;

        // Ensure default chart exists if empty.
        let fs_rows = tx
            .query(
                "SELECT id::int8, code::text FROM account_consolidation_account WHERE group_id = $1",
                &[&gid],
            )
            .await?;
        if fs_rows.is_empty() {
            Self::action_bootstrap_default_chart(grp, tx).await?;
        }
        let fs_map: std::collections::BTreeMap<String, i64> = tx
            .query(
                "SELECT code::text, id::int8 FROM account_consolidation_account WHERE group_id = $1",
                &[&gid],
            )
            .await?
            .into_iter()
            .map(|r| Ok((r.try_get(0).map_err(db)?, r.try_get(1).map_err(db)?)))
            .collect::<Result<_>>()?;

        let units = tx
            .query(
                "SELECT company_id::int8, investment_account_id::int8 \
                 FROM account_consolidation_unit WHERE group_id = $1 ORDER BY id",
                &[&gid],
            )
            .await?;
        if units.is_empty() {
            return Err(Error::user(
                "Add at least one Consolidation Unit (Company) before running Auto-Map.",
            ));
        }

        let existing_pairs: std::collections::BTreeSet<(i64, i64)> = tx
            .query(
                "SELECT company_id::int8, account_id::int8 \
                 FROM account_consolidation_account_map WHERE group_id = $1",
                &[&gid],
            )
            .await?
            .into_iter()
            .map(|r| Ok((r.try_get(0).map_err(db)?, r.try_get(1).map_err(db)?)))
            .collect::<Result<_>>()?;

        let mut vals_list = Vec::new();
        for urow in units {
            let company_id: i64 = urow.try_get(0).map_err(db)?;
            let inv_acc_id: Option<i64> = urow.try_get(1).map_err(db)?;

            let accounts = tx
                .query(
                    "SELECT a.id::int8, a.account_type::text \
                     FROM account_account a \
                     JOIN account_account_res_company_rel rel ON rel.account_account_id = a.id \
                     WHERE rel.res_company_id = $1 \
                       AND a.account_type <> 'off_balance' \
                       AND (a.active = true OR EXISTS ( \
                            SELECT 1 FROM account_move_line l \
                            WHERE l.account_id = a.id AND l.company_id = $1 \
                              AND l.parent_state = 'posted' \
                       )) \
                     ORDER BY a.id",
                    &[&company_id],
                )
                .await?;
            for arow in accounts {
                let acc_id: i64 = arow.try_get(0).map_err(db)?;
                let atype: String = arow.try_get(1).map_err(db)?;
                if existing_pairs.contains(&(company_id, acc_id)) {
                    continue;
                }
                let fs_code = if Some(acc_id) == inv_acc_id {
                    "1550"
                } else {
                    default_fs_code_for_account_type(&atype)?
                };
                if let Some(&fs_id) = fs_map.get(fs_code) {
                    let mut v = RawVals::new();
                    v.insert("group_id".into(), Cell::Id(gid));
                    v.insert("company_id".into(), Cell::Id(company_id));
                    v.insert("account_id".into(), Cell::Id(acc_id));
                    v.insert("consolidation_account_id".into(), Cell::Id(fs_id));
                    vals_list.push(v);
                }
            }
        }

        if !vals_list.is_empty() {
            let clean_env = strip_default_context(grp.env());
            clean_env
                .records("account.consolidation.account.map")?
                .create(tx, vals_list)
                .await?;
        }
        tx.invalidate_records(grp.as_records(), Some(&["mapping_ids"]))
            .await
    }
}

pub(crate) fn default_fs_code_for_account_type(account_type: &str) -> Result<&'static str> {
    match account_type {
        "asset_cash" => Ok("1100"),
        "asset_receivable" => Ok("1200"),
        "asset_current" | "asset_prepayments" => Ok("1300"),
        "asset_fixed" | "asset_non_current" => Ok("1500"),
        "liability_payable" => Ok("2100"),
        "liability_current" | "liability_credit_card" => Ok("2200"),
        "liability_non_current" => Ok("2500"),
        "equity" => Ok("3100"),
        "equity_unaffected" => Ok("3200"),
        "income" => Ok("4100"),
        "income_other" => Ok("4200"),
        "expense_direct_cost" => Ok("5100"),
        "expense" | "expense_other" => Ok("5200"),
        "expense_depreciation" => Ok("5300"),
        other => Err(Error::validation(format!(
            "Unsupported account type '{other}' for default Group FS mapping; add an explicit consolidation account mapping."
        ))),
    }
}

/// `account.consolidation.unit`: one legal entity inside a consolidation group with its
/// consolidation method (`full`, `proportional`, `equity`) and exact ownership percentage.
#[model(
    name = "account.consolidation.unit",
    description = "Consolidation Unit",
    order = "is_parent desc, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_unique_group_company",
        fields = ["group_id", "company_id"],
        message = "Each company may appear only once per Consolidation Group."
    )]
)]
pub struct AccountConsolidationUnit {
    /// Parent consolidation group.
    #[field(
        many2one,
        comodel = "account.consolidation.group",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Consolidation Group"
    )]
    pub group_id: Option<RecId>,
    /// Legal entity company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Local currency of the entity.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Local Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Whether this entity is the holding / parent entity of the group.
    #[field(boolean, default = false, string = "Parent Entity")]
    pub is_parent: bool,
    /// Consolidation method applied to this entity.
    #[field(
        selection,
        selection = [
            ("full", "Full Consolidation (>50% + NCI)"),
            ("proportional", "Proportional Consolidation (JV)"),
            ("equity", "Equity Method (Associate)"),
        ],
        required = true,
        default = "full",
        string = "Method"
    )]
    pub consolidation_method: Option<Arc<str>>,
    /// Group ownership percentage (0 < ownership_pct <= 100).
    #[field(
        float,
        digits = (16, 4),
        required = true,
        default = 100,
        string = "Ownership %"
    )]
    pub ownership_pct: Decimal,
    /// Optional Parent GL Account holding Investment in Subsidiaries (eliminated against equity).
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "set null",
        string = "Investment in Subsidiary Account"
    )]
    pub investment_account_id: Option<RecId>,
}

#[methods(AccountConsolidationUnit, addon = "account")]
impl AccountConsolidationUnit {
    #[constrains(
        "ownership_pct",
        "is_parent",
        "group_id",
        "company_id",
        "investment_account_id",
        "consolidation_method"
    )]
    async fn _check_unit_integrity(
        this: &RecordSet<AccountConsolidationUnit>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for pct in this
            .mapped(tx, AccountConsolidationUnit::OWNERSHIP_PCT)
            .await?
        {
            if pct <= Decimal::ZERO || pct > Decimal::from(100) {
                return Err(Error::validation(
                    "Consolidation ownership percentage must be > 0 and <= 100.",
                ));
            }
        }
        for method in this
            .mapped(tx, AccountConsolidationUnit::CONSOLIDATION_METHOD)
            .await?
        {
            if !matches!(method.as_deref(), Some("full" | "proportional" | "equity")) {
                return Err(Error::validation(format!(
                    "Unsupported consolidation method {:?}.",
                    method
                )));
            }
        }
        tx.flush().await?;
        let ids: Vec<i64> = this.as_records().ids().to_vec();
        if ids.is_empty() {
            return Ok(());
        }
        let multi_parent = tx
            .query(
                "SELECT group_id::int8, count(*)::int8 \
                 FROM account_consolidation_unit \
                 WHERE group_id IN (SELECT DISTINCT group_id FROM account_consolidation_unit WHERE id = ANY($1::int8[])) \
                   AND COALESCE(is_parent, false) = true \
                 GROUP BY group_id HAVING count(*) > 1 LIMIT 1",
                &[&ids],
            )
            .await?;
        if !multi_parent.is_empty() {
            return Err(Error::validation(
                "A Consolidation Group can have at most one Parent Entity.",
            ));
        }
        let bad_inv = tx
            .query(
                "SELECT u.id::int8 \
                 FROM account_consolidation_unit u \
                 WHERE u.id = ANY($1::int8[]) \
                   AND u.investment_account_id IS NOT NULL \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM account_account_res_company_rel rel \
                       WHERE rel.account_account_id = u.investment_account_id \
                         AND rel.res_company_id = u.company_id \
                   ) \
                 LIMIT 1",
                &[&ids],
            )
            .await?;
        if !bad_inv.is_empty() {
            return Err(Error::validation(
                "Investment in Subsidiary Account must belong to the Consolidation Unit's company.",
            ));
        }
        Ok(())
    }
}

/// `account.consolidation.account`: unified Group Chart of Accounts (Financial Statement Item).
#[model(
    name = "account.consolidation.account",
    description = "Group Financial Statement Item",
    order = "sequence, code, id",
    keys = [key(
        name = "_unique_group_code",
        fields = ["group_id", "code"],
        message = "Group FS Item code must be unique per Consolidation Group."
    )]
)]
pub struct AccountConsolidationAccount {
    /// Owning group.
    #[field(
        many2one,
        comodel = "account.consolidation.group",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Consolidation Group"
    )]
    pub group_id: Option<RecId>,
    /// Display order.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// Group account code (e.g. "1100", "3300").
    #[field(char, required = true, index = true, string = "Code")]
    pub code: Option<Arc<str>>,
    /// Group account name.
    #[field(char, required = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Financial statement section.
    #[field(
        selection,
        selection = [
            ("asset_current", "Current Assets"),
            ("asset_non_current", "Non-Current Assets"),
            ("goodwill", "Goodwill"),
            ("liability_current", "Current Liabilities"),
            ("liability_non_current", "Non-Current Liabilities"),
            ("equity_capital", "Share Capital"),
            ("equity_retained", "Retained Earnings & Reserves"),
            ("equity_cta", "Translation Reserve (CTA)"),
            ("equity_nci", "Non-Controlling Interest (Equity)"),
            ("income", "Revenue & Other Income"),
            ("cogs", "Cost of Revenue"),
            ("expense", "Operating Expenses"),
            ("nci_profit", "NCI Share of Profit"),
        ],
        required = true,
        string = "FS Section"
    )]
    pub section: Option<Arc<str>>,
    /// Currency translation method per IAS 21 / PSAK 10.
    #[field(
        selection,
        selection = [
            ("closing", "Closing Rate (Period End)"),
            ("average", "Monthly Average Rate (P&L)"),
            ("historical", "Historical Rate (Equity)"),
            ("computed_cta", "Computed CTA Reserve"),
        ],
        required = true,
        default = "closing",
        string = "Translation Rate Method"
    )]
    pub rate_method: Option<Arc<str>>,
    /// Intercompany / elimination role of this FS item.
    #[field(
        selection,
        selection = [
            ("none", "Standard Third-Party"),
            ("ic_receivable", "Intercompany Receivable"),
            ("ic_payable", "Intercompany Payable"),
            ("ic_revenue", "Intercompany Revenue"),
            ("ic_expense", "Intercompany Cost / Expense"),
            ("ic_variance", "IC Elimination Variance"),
            ("investment_in_sub", "Investment in Subsidiaries"),
        ],
        required = true,
        default = "none",
        string = "Elimination Role"
    )]
    pub ic_role: Option<Arc<str>>,
}

#[methods(AccountConsolidationAccount, addon = "account")]
impl AccountConsolidationAccount {
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountConsolidationAccount>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let group_ids: std::collections::BTreeSet<i64> = this
                .mapped(tx, AccountConsolidationAccount::GROUP_ID)
                .await?
                .into_iter()
                .flatten()
                .map(|id| id.get())
                .collect();
            for gid in group_ids {
                ensure_group_company_access(tx, this.env(), gid).await?;
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountConsolidationAccount>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let group_ids: std::collections::BTreeSet<i64> = this
                .mapped(tx, AccountConsolidationAccount::GROUP_ID)
                .await?
                .into_iter()
                .flatten()
                .map(|id| id.get())
                .collect();
            for gid in group_ids {
                ensure_group_company_access(tx, this.env(), gid).await?;
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

/// `account.consolidation.account.map`: maps a local legal entity GL account into a Group FS Item.
#[model(
    name = "account.consolidation.account.map",
    description = "Consolidation Account Mapping",
    order = "company_id, account_id, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_unique_group_company_account",
        fields = ["group_id", "company_id", "account_id"],
        message = "Each company GL account may map to only one Group FS Item per group."
    )]
)]
pub struct AccountConsolidationAccountMap {
    /// Consolidation group.
    #[field(
        many2one,
        comodel = "account.consolidation.group",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Consolidation Group"
    )]
    pub group_id: Option<RecId>,
    /// Legal entity company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Local GL account.
    #[field(
        many2one,
        comodel = "account.account",
        required = true,
        ondelete = "restrict",
        string = "Local GL Account"
    )]
    pub account_id: Option<RecId>,
    /// Target Group FS Item.
    #[field(
        many2one,
        comodel = "account.consolidation.account",
        required = true,
        ondelete = "cascade",
        string = "Group FS Item"
    )]
    pub consolidation_account_id: Option<RecId>,
}

#[methods(AccountConsolidationAccountMap, addon = "account")]
impl AccountConsolidationAccountMap {
    #[constrains("group_id", "company_id", "account_id", "consolidation_account_id")]
    async fn _check_mapping_integrity(
        this: &RecordSet<AccountConsolidationAccountMap>,
        tx: &mut Transaction,
    ) -> Result<()> {
        tx.flush().await?;
        let ids: Vec<i64> = this.as_records().ids().to_vec();
        if ids.is_empty() {
            return Ok(());
        }
        let bad_fs = tx
            .query(
                "SELECT m.id::int8 \
                 FROM account_consolidation_account_map m \
                 JOIN account_consolidation_account a ON a.id = m.consolidation_account_id \
                 WHERE m.id = ANY($1::int8[]) AND a.group_id <> m.group_id \
                 LIMIT 1",
                &[&ids],
            )
            .await?;
        if !bad_fs.is_empty() {
            return Err(Error::validation(
                "Group FS Item must belong to the same Consolidation Group as the account mapping.",
            ));
        }
        let bad_acc = tx
            .query(
                "SELECT m.id::int8 \
                 FROM account_consolidation_account_map m \
                 WHERE m.id = ANY($1::int8[]) \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM account_account_res_company_rel rel \
                       WHERE rel.account_account_id = m.account_id \
                         AND rel.res_company_id = m.company_id \
                   ) \
                 LIMIT 1",
                &[&ids],
            )
            .await?;
        if !bad_acc.is_empty() {
            return Err(Error::validation(
                "Local GL Account must belong to the mapped Consolidation Unit's company.",
            ));
        }
        Ok(())
    }
}
