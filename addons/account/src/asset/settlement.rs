// SPDX-License-Identifier: MIT OR Apache-2.0
//! CIP Multi-Receiver Distribution Rules & Capitalization Settlement Engine
//! (`account.asset.settlement.rule`, `account.asset.settlement`, `account.asset.settlement.line`).

use erp_addon_analytic::split_proportional_exact;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

use super::{AccountAsset, db};
use crate::moves::post_moves;

fn default_today(_ctx: &erp_sdk::DefaultContext) -> Value {
    Value::Date(Date::today())
}

/// Pure exact-decimal settlement distribution calculator.
/// Supports percentage-based rules (must sum to 100% or proportional weights) or explicit fixed
/// amounts whose sum equals `settlement_total`.
pub fn allocate_settlement_amounts(
    settlement_total: Decimal,
    rules: &[(i64, Decimal, Decimal)], // (rule_id, percentage, fixed_amount)
) -> Result<Vec<(i64, Decimal)>> {
    if settlement_total <= Decimal::ZERO {
        return Err(Error::user("Settlement amount must be greater than zero."));
    }
    if rules.is_empty() {
        return Err(Error::user(
            "Configure at least one CIP settlement distribution rule before settling.",
        ));
    }
    let total_fixed: Decimal = rules.iter().map(|(_, _, fixed)| *fixed).sum();
    let total_pct: Decimal = rules.iter().map(|(_, pct, _)| *pct).sum();

    if total_fixed > Decimal::ZERO && total_pct.is_zero() {
        if total_fixed.round_dp(2) != settlement_total.round_dp(2) {
            return Err(Error::user(format!(
                "Fixed settlement rule amounts ({total_fixed}) must equal the total settlement amount ({settlement_total})."
            )));
        }
        return Ok(rules
            .iter()
            .map(|(id, _, fixed)| (*id, fixed.round_dp(2)))
            .collect());
    }
    if total_pct <= Decimal::ZERO {
        return Err(Error::user(
            "Settlement rules must specify positive percentages (summing to 100%) or exact fixed amounts.",
        ));
    }
    if total_pct.round_dp(2) != Decimal::from(100) {
        return Err(Error::user(format!(
            "CIP settlement rule percentages must sum to 100.00% (currently {total_pct}%)."
        )));
    }
    let weights: Vec<(i64, Decimal)> = rules.iter().map(|(id, pct, _)| (*id, *pct)).collect();
    split_proportional_exact(settlement_total, &weights, 2)
}

/// `account.asset.settlement.rule`: distribution rule defining how a CIP asset's accumulated cost
/// is capitalized to a new Fixed Asset, added to an existing Fixed Asset (Post-Capitalization),
/// or expensed to a Cost Center.
#[model(
    name = "account.asset.settlement.rule",
    description = "CIP Settlement Rule",
    order = "sequence, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountAssetSettlementRule {
    /// Parent CIP asset.
    #[field(
        many2one,
        comodel = "account.asset",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "CIP Asset"
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
    /// Rule sequence.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// Rule description / target asset label.
    #[field(char, required = true, string = "Description / Target Name")]
    pub name: Option<Arc<str>>,
    /// Receiver destination type.
    #[field(
        selection,
        selection = [
            ("new_asset", "New Fixed Asset"),
            ("existing_asset", "Existing Fixed Asset (Post-Capitalization)"),
            ("cost_center", "Expense to Cost Center"),
        ],
        required = true,
        default = "new_asset",
        string = "Receiver Type"
    )]
    pub receiver_type: Option<Arc<str>>,
    /// Percentage share of settlement (all percentage rules must total 100%).
    #[field(float, digits = (16, 4), default = 100, string = "Share %")]
    pub percentage: Decimal,
    /// Optional exact fixed amount (used when percentages are 0).
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Fixed Amount"
    )]
    pub fixed_amount: Decimal,
    /// Asset class used when creating a new Fixed Asset (`receiver_type = 'new_asset'`).
    #[field(
        many2one,
        comodel = "account.asset.category",
        check_company = true,
        ondelete = "restrict",
        string = "Target Asset Class"
    )]
    pub target_category_id: Option<RecId>,
    /// Existing Fixed Asset receiving post-capitalization (`receiver_type = 'existing_asset'`).
    #[field(
        many2one,
        comodel = "account.asset",
        check_company = true,
        ondelete = "restrict",
        string = "Target Existing Asset"
    )]
    pub target_asset_id: Option<RecId>,
    /// Expense account when settling non-capitalizable costs (`receiver_type = 'cost_center'`).
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Expense Account"
    )]
    pub expense_account_id: Option<RecId>,
    /// Cost Center attached to a new fixed asset or non-capitalizable expense posting.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "set null",
        string = "Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// Automatically confirm newly created Fixed Assets and build their depreciation schedule.
    #[field(boolean, default = true, string = "Auto-Confirm New Asset")]
    pub auto_confirm_asset: bool,
}

#[methods(AccountAssetSettlementRule, addon = "account")]
impl AccountAssetSettlementRule {}

/// `account.asset.settlement`: auditable CIP capitalization / settlement execution document.
#[model(
    name = "account.asset.settlement",
    description = "CIP Asset Settlement",
    order = "settlement_date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountAssetSettlement {
    /// Settlement reference label.
    #[field(char, required = true, string = "Settlement Reference")]
    pub name: Option<Arc<str>>,
    /// Source CIP asset being settled.
    #[field(
        many2one,
        comodel = "account.asset",
        required = true,
        check_company = true,
        ondelete = "restrict",
        index = true,
        string = "CIP Asset"
    )]
    pub cip_asset_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "cip_asset_id.company_id",
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
    /// Capitalization / settlement posting date.
    #[field(date, required = true, default_fn = default_today, string = "Settlement Date")]
    pub settlement_date: Option<Date>,
    /// Full settlement zeroes remaining CIP balance; Partial settles an explicit amount.
    #[field(
        selection,
        selection = [
            ("full", "Full Final Settlement"),
            ("partial", "Partial Settlement"),
        ],
        required = true,
        default = "full",
        string = "Settlement Mode"
    )]
    pub settlement_mode: Option<Arc<str>>,
    /// Explicit amount to settle when `settlement_mode == 'partial'`.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Partial Amount"
    )]
    pub partial_amount: Decimal,
    /// Document state.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("posted", "Settled"),
            ("reversed", "Reversed"),
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Total amount settled in this document.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Total Settled"
    )]
    pub total_settled: Decimal,
    /// Balanced settlement journal entry posted to the General Ledger.
    #[field(
        many2one,
        comodel = "account.move",
        readonly = true,
        copy = false,
        ondelete = "set null",
        string = "Settlement Journal Entry"
    )]
    pub move_id: Option<RecId>,
    /// Reversal journal entry if reversed.
    #[field(
        many2one,
        comodel = "account.move",
        readonly = true,
        copy = false,
        ondelete = "set null",
        string = "Reversal Entry"
    )]
    pub reversal_move_id: Option<RecId>,
    /// Resulting receiver lines created by this settlement.
    #[field(
        one2many,
        comodel = "account.asset.settlement.line",
        inverse_name = "settlement_id",
        readonly = true,
        string = "Settlement Lines"
    )]
    pub line_ids: Vec<RecId>,
}

#[methods(AccountAssetSettlement, addon = "account")]
impl AccountAssetSettlement {
    /// Executes CIP capitalization according to the CIP asset's distribution rules:
    /// creates/updates receiver assets, posts a balanced GL entry (Dr Target Asset/Expense,
    /// Cr CIP Account), and updates CIP unsettled balances atomically.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_post_settlement(
        this: &RecordSet<AccountAssetSettlement>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let doc = this.ensure_one()?;
        doc.as_records().check_access(tx, Operation::Write).await?;
        let doc_id = doc.as_records().ids()[0];
        let state = doc.get(tx, AccountAssetSettlement::STATE).await?;
        if state.as_deref() != Some("draft") {
            return Err(Error::user("Only a draft CIP settlement can be posted."));
        }
        let cip_id = doc
            .get(tx, AccountAssetSettlement::CIP_ASSET_ID)
            .await?
            .ok_or_else(|| Error::validation("Select a CIP Asset to settle."))?
            .get();
        let settlement_date = doc
            .get(tx, AccountAssetSettlement::SETTLEMENT_DATE)
            .await?
            .unwrap_or_else(Date::today);
        let mode = doc
            .get(tx, AccountAssetSettlement::SETTLEMENT_MODE)
            .await?
            .unwrap_or_else(|| Arc::from("full"));
        let partial_amount = doc.get(tx, AccountAssetSettlement::PARTIAL_AMOUNT).await?;
        let doc_name = doc
            .get(tx, AccountAssetSettlement::NAME)
            .await?
            .unwrap_or_else(|| Arc::from("CIP Settlement"));

        let cip_rows = tx
            .query(
                "SELECT company_id::int8, currency_id::int8, journal_id::int8, \
                        account_asset_id::int8, COALESCE(asset_type, 'normal')::text, \
                        COALESCE(state, 'draft')::text, name::text \
                 FROM account_asset WHERE id = $1 FOR UPDATE",
                &[&cip_id],
            )
            .await?;
        let Some(cip_row) = cip_rows.first() else {
            return Err(Error::missing("CIP Asset does not exist."));
        };
        let company_id: i64 = cip_row.try_get(0).map_err(db)?;
        let currency_id: i64 = cip_row.try_get(1).map_err(db)?;
        let journal_id: i64 = cip_row.try_get(2).map_err(db)?;
        let cip_account_id: i64 = cip_row.try_get(3).map_err(db)?;
        let asset_type: String = cip_row.try_get(4).map_err(db)?;
        let cip_state: String = cip_row.try_get(5).map_err(db)?;
        let cip_name: String = cip_row.try_get(6).map_err(db)?;

        if asset_type != "cip" {
            return Err(Error::user(format!(
                "Asset '{cip_name}' is not a Construction in Progress (CIP) asset."
            )));
        }
        if matches!(cip_state.as_str(), "settled" | "close" | "disposed") {
            return Err(Error::user(format!(
                "CIP asset '{cip_name}' is already {cip_state}."
            )));
        }

        let unsettled: Decimal = tx
            .query(
                "SELECT COALESCE(SUM(amount - settled_amount), 0) \
                 FROM account_asset_cip_line WHERE asset_id = $1",
                &[&cip_id],
            )
            .await?
            .first()
            .and_then(|r| r.try_get(0).ok())
            .unwrap_or(Decimal::ZERO);
        if unsettled <= Decimal::ZERO {
            return Err(Error::user(format!(
                "CIP asset '{cip_name}' has no unsettled accumulated costs to capitalize."
            )));
        }
        let target_total = if mode.as_ref() == "partial" {
            if partial_amount <= Decimal::ZERO || partial_amount > unsettled {
                return Err(Error::user(format!(
                    "Partial settlement amount ({partial_amount}) must be > 0 and <= unsettled CIP balance ({unsettled})."
                )));
            }
            partial_amount.round_dp(2)
        } else {
            unsettled.round_dp(2)
        };

        let rule_rows = tx
            .query(
                "SELECT id::int8, name::text, receiver_type::text, COALESCE(percentage, 0), \
                        COALESCE(fixed_amount, 0), target_category_id::int8, \
                        target_asset_id::int8, expense_account_id::int8, \
                        analytic_account_id::int8, COALESCE(auto_confirm_asset, true) \
                 FROM account_asset_settlement_rule \
                 WHERE asset_id = $1 ORDER BY sequence, id",
                &[&cip_id],
            )
            .await?;

        struct RuleSpec {
            id: i64,
            name: String,
            receiver_type: String,
            pct: Decimal,
            fixed: Decimal,
            target_category_id: Option<i64>,
            target_asset_id: Option<i64>,
            expense_account_id: Option<i64>,
            analytic_account_id: Option<i64>,
            auto_confirm: bool,
        }
        let mut specs = Vec::with_capacity(rule_rows.len());
        let mut alloc_inputs = Vec::with_capacity(rule_rows.len());
        for r in rule_rows {
            let s = RuleSpec {
                id: r.try_get(0).map_err(db)?,
                name: r.try_get(1).map_err(db)?,
                receiver_type: r.try_get(2).map_err(db)?,
                pct: r.try_get(3).map_err(db)?,
                fixed: r.try_get(4).map_err(db)?,
                target_category_id: r.try_get(5).map_err(db)?,
                target_asset_id: r.try_get(6).map_err(db)?,
                expense_account_id: r.try_get(7).map_err(db)?,
                analytic_account_id: r.try_get(8).map_err(db)?,
                auto_confirm: r.try_get(9).map_err(db)?,
            };
            alloc_inputs.push((s.id, s.pct, s.fixed));
            specs.push(s);
        }

        let shares = allocate_settlement_amounts(target_total, &alloc_inputs)?;
        let env = doc.env().with_context_replaced(erp_sdk::Context::default());

        // Create settlement journal entry header.
        let mut mvals = RawVals::new();
        mvals.insert("move_type".into(), Cell::Str("entry".into()));
        mvals.insert("journal_id".into(), Cell::Id(journal_id));
        mvals.insert("company_id".into(), Cell::Id(company_id));
        mvals.insert("currency_id".into(), Cell::Id(currency_id));
        mvals.insert("date".into(), Cell::Date(settlement_date));
        mvals.insert("ref".into(), Cell::Str(doc_name.clone()));

        let unchecked_moves = crate::moves::unchecked(&env.records("account.move")?);
        let move_id = unchecked_moves.create(tx, vec![mvals]).await?.ids()[0];

        let mut gl_lines = Vec::with_capacity(shares.len() + 1);
        // Credit CIP GL account for total settled amount.
        let mut cr_cip = RawVals::new();
        cr_cip.insert("move_id".into(), Cell::Id(move_id));
        cr_cip.insert("account_id".into(), Cell::Id(cip_account_id));
        cr_cip.insert(
            "name".into(),
            Cell::Str(format!("CIP Settlement Credit: {cip_name}").into()),
        );
        cr_cip.insert("currency_id".into(), Cell::Id(currency_id));
        cr_cip.insert("balance".into(), Cell::Decimal(-target_total));
        cr_cip.insert("amount_currency".into(), Cell::Decimal(-target_total));
        gl_lines.push(cr_cip);

        let mut settle_line_vals = Vec::with_capacity(shares.len());

        for (spec, (_, share)) in specs.iter().zip(shares) {
            if share.is_zero() {
                continue;
            }
            let (debit_account_id, created_asset_id, target_asset_id, cc_id) = match spec
                .receiver_type
                .as_str()
            {
                "new_asset" => {
                    let cat_id = spec.target_category_id.ok_or_else(|| {
                        Error::validation(format!(
                            "Settlement rule '{}' (New Fixed Asset) requires a Target Asset Class.",
                            spec.name
                        ))
                    })?;
                    let cat_rows = tx
                            .query(
                                "SELECT journal_id::int8, account_asset_id::int8, \
                                        account_depreciation_id::int8, account_depreciation_expense_id::int8, \
                                        account_disposal_gain_loss_id::int8, method::text, \
                                        method_number, method_period, COALESCE(method_progress_factor, 0.30), \
                                        COALESCE(prorata_temporis, false), COALESCE(asset_type, 'normal')::text \
                                 FROM account_asset_category WHERE id = $1",
                                &[&cat_id],
                            )
                            .await?;
                    let Some(cat) = cat_rows.first() else {
                        return Err(Error::missing("Target Asset Class does not exist."));
                    };
                    let c_journal: i64 = cat.try_get(0).map_err(db)?;
                    let c_asset_acc: i64 = cat.try_get(1).map_err(db)?;
                    let c_dep_acc: Option<i64> = cat.try_get(2).map_err(db)?;
                    let c_exp_acc: Option<i64> = cat.try_get(3).map_err(db)?;
                    let c_disp_acc: Option<i64> = cat.try_get(4).map_err(db)?;
                    let c_method: String = cat.try_get(5).map_err(db)?;
                    let c_num: i32 = cat.try_get(6).map_err(db)?;
                    let c_per: i32 = cat.try_get(7).map_err(db)?;
                    let c_fac: Decimal = cat.try_get(8).map_err(db)?;
                    let c_prorata: bool = cat.try_get(9).map_err(db)?;
                    let c_type: String = cat.try_get(10).map_err(db)?;
                    if c_type != "normal" {
                        return Err(Error::validation(format!(
                            "Target Asset Class on rule '{}' must be a Depreciable Fixed Asset class.",
                            spec.name
                        )));
                    }

                    let mut avals = RawVals::new();
                    avals.insert("name".into(), Cell::Str(spec.name.as_str().into()));
                    avals.insert("company_id".into(), Cell::Id(company_id));
                    avals.insert("category_id".into(), Cell::Id(cat_id));
                    avals.insert("asset_type".into(), Cell::Str("normal".into()));
                    avals.insert("source_cip_id".into(), Cell::Id(cip_id));
                    avals.insert("acquisition_date".into(), Cell::Date(settlement_date));
                    avals.insert("original_value".into(), Cell::Decimal(share));
                    avals.insert("journal_id".into(), Cell::Id(c_journal));
                    avals.insert("account_asset_id".into(), Cell::Id(c_asset_acc));
                    if let Some(d) = c_dep_acc {
                        avals.insert("account_depreciation_id".into(), Cell::Id(d));
                    }
                    if let Some(e) = c_exp_acc {
                        avals.insert("account_depreciation_expense_id".into(), Cell::Id(e));
                    }
                    if let Some(g) = c_disp_acc {
                        avals.insert("account_disposal_gain_loss_id".into(), Cell::Id(g));
                    }
                    avals.insert("method".into(), Cell::Str(c_method.into()));
                    avals.insert("method_number".into(), Cell::Int(c_num));
                    avals.insert("method_period".into(), Cell::Int(c_per));
                    avals.insert("method_progress_factor".into(), Cell::Decimal(c_fac));
                    avals.insert("prorata_temporis".into(), Cell::Bool(c_prorata));
                    if let Some(cc) = spec.analytic_account_id {
                        avals.insert("analytic_account_id".into(), Cell::Id(cc));
                    }

                    let new_asset_rec = env
                        .records("account.asset")?
                        .create(tx, vec![avals])
                        .await?;
                    let new_id = new_asset_rec.ids()[0];
                    if spec.auto_confirm {
                        let typed: RecordSet<AccountAsset> = new_asset_rec.try_into()?;
                        AccountAsset::action_validate(&typed, tx).await?;
                    }
                    (c_asset_acc, Some(new_id), Some(new_id), None)
                }
                "existing_asset" => {
                    let tgt_id = spec.target_asset_id.ok_or_else(|| {
                            Error::validation(format!(
                                "Settlement rule '{}' (Existing Asset) requires a Target Existing Asset.",
                                spec.name
                            ))
                        })?;
                    let tgt_rows = tx
                            .query(
                                "SELECT account_asset_id::int8, COALESCE(original_value, 0), \
                                        COALESCE(asset_type, 'normal')::text, COALESCE(state, 'draft')::text \
                                 FROM account_asset WHERE id = $1 FOR UPDATE",
                                &[&tgt_id],
                            )
                            .await?;
                    let Some(tgt) = tgt_rows.first() else {
                        return Err(Error::missing("Target Existing Asset does not exist."));
                    };
                    let tgt_acc: i64 = tgt.try_get(0).map_err(db)?;
                    let cur_val: Decimal = tgt.try_get(1).map_err(db)?;
                    let tgt_type: String = tgt.try_get(2).map_err(db)?;
                    let tgt_state: String = tgt.try_get(3).map_err(db)?;
                    if tgt_type != "normal" || matches!(tgt_state.as_str(), "disposed" | "close") {
                        return Err(Error::user(format!(
                            "Target Existing Asset on rule '{}' must be a draft or running Fixed Asset.",
                            spec.name
                        )));
                    }
                    let new_orig = cur_val + share;
                    tx.execute(
                        "UPDATE account_asset SET original_value = $2, \
                                    write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                        &[&tgt_id, &new_orig],
                    )
                    .await?;
                    let tgt_rec: RecordSet<AccountAsset> =
                        env.records("account.asset")?.browse([tgt_id])?.try_into()?;
                    tx.invalidate_records(tgt_rec.as_records(), Some(&["original_value"]))
                        .await?;
                    AccountAsset::action_compute_depreciation_board(&tgt_rec, tx).await?;
                    (tgt_acc, None, Some(tgt_id), None)
                }
                "cost_center" => {
                    let exp_acc = spec.expense_account_id.ok_or_else(|| {
                        Error::validation(format!(
                            "Settlement rule '{}' (Cost Center) requires an Expense Account.",
                            spec.name
                        ))
                    })?;
                    let cc = spec.analytic_account_id.ok_or_else(|| {
                        Error::validation(format!(
                            "Settlement rule '{}' (Cost Center) requires a Cost Center.",
                            spec.name
                        ))
                    })?;
                    (exp_acc, None, None, Some(cc))
                }
                other => {
                    return Err(Error::validation(format!(
                        "Unknown settlement receiver type '{other}'."
                    )));
                }
            };

            let mut dr = RawVals::new();
            dr.insert("move_id".into(), Cell::Id(move_id));
            dr.insert("account_id".into(), Cell::Id(debit_account_id));
            dr.insert(
                "name".into(),
                Cell::Str(format!("CIP Capitalization: {}", spec.name).into()),
            );
            dr.insert("currency_id".into(), Cell::Id(currency_id));
            dr.insert("balance".into(), Cell::Decimal(share));
            dr.insert("amount_currency".into(), Cell::Decimal(share));
            if let Some(cc) = cc_id {
                dr.insert("analytic_account_id".into(), Cell::Id(cc));
            }
            gl_lines.push(dr);

            let mut sl = RawVals::new();
            sl.insert("settlement_id".into(), Cell::Id(doc_id));
            sl.insert("name".into(), Cell::Str(spec.name.as_str().into()));
            sl.insert(
                "receiver_type".into(),
                Cell::Str(spec.receiver_type.as_str().into()),
            );
            sl.insert("account_id".into(), Cell::Id(debit_account_id));
            sl.insert("amount".into(), Cell::Decimal(share));
            if let Some(aid) = created_asset_id {
                sl.insert("created_asset_id".into(), Cell::Id(aid));
            }
            if let Some(tid) = target_asset_id {
                sl.insert("target_asset_id".into(), Cell::Id(tid));
            }
            if let Some(cc) = spec.analytic_account_id {
                sl.insert("analytic_account_id".into(), Cell::Id(cc));
            }
            settle_line_vals.push(sl);
        }

        let unchecked_lines = crate::moves::unchecked(&env.records("account.move.line")?);
        unchecked_lines.create(tx, gl_lines).await?;
        post_moves(tx, &env.records("account.move")?.browse([move_id])?).await?;

        if !settle_line_vals.is_empty() {
            env.records("account.asset.settlement.line")?
                .create(tx, settle_line_vals)
                .await?;
        }

        // Allocate settled_amount FIFO across open CIP cost lines.
        let cip_lines = tx
            .query(
                "SELECT id::int8, amount, settled_amount \
                 FROM account_asset_cip_line \
                 WHERE asset_id = $1 AND (amount - settled_amount) <> 0 \
                 ORDER BY date, id",
                &[&cip_id],
            )
            .await?;
        let mut to_mark = target_total;
        for cl in cip_lines {
            if to_mark <= Decimal::ZERO {
                break;
            }
            let cl_id: i64 = cl.try_get(0).map_err(db)?;
            let amt: Decimal = cl.try_get(1).map_err(db)?;
            let cur_settled: Decimal = cl.try_get(2).map_err(db)?;
            let avail = amt - cur_settled;
            if avail <= Decimal::ZERO {
                continue;
            }
            let take = avail.min(to_mark);
            let new_settled = cur_settled + take;
            tx.execute(
                "UPDATE account_asset_cip_line SET settled_amount = $2, \
                        write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                &[&cl_id, &new_settled],
            )
            .await?;
            to_mark -= take;
        }

        // Update settlement document & CIP asset status.
        tx.execute(
            "UPDATE account_asset_settlement \
             SET state = 'posted', total_settled = $2, move_id = $3, \
                 write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[&doc_id, &target_total, &move_id],
        )
        .await?;

        let rem_unsettled: Decimal = tx
            .query(
                "SELECT COALESCE(SUM(amount - settled_amount), 0) \
                 FROM account_asset_cip_line WHERE asset_id = $1",
                &[&cip_id],
            )
            .await?
            .first()
            .and_then(|r| r.try_get(0).ok())
            .unwrap_or(Decimal::ZERO);
        if rem_unsettled <= Decimal::ZERO {
            tx.execute(
                "UPDATE account_asset SET state = 'settled', write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1",
                &[&cip_id],
            )
            .await?;
        }

        tx.invalidate_records(
            doc.as_records(),
            Some(&["state", "total_settled", "move_id", "line_ids"]),
        )
        .await?;
        tx.invalidate_model(
            "account.asset",
            Some(&[
                "state",
                "cip_accumulated_amount",
                "cip_settled_amount",
                "cip_unsettled_amount",
                "value_residual",
            ]),
        )
        .await
    }

    /// Reverses a posted CIP settlement if none of the newly capitalized assets have posted
    /// depreciation entries yet, restoring the CIP unsettled balance cleanly.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_reverse_settlement(
        this: &RecordSet<AccountAssetSettlement>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let doc = this.ensure_one()?;
        doc.as_records().check_access(tx, Operation::Write).await?;
        let doc_id = doc.as_records().ids()[0];
        let state = doc.get(tx, AccountAssetSettlement::STATE).await?;
        if state.as_deref() != Some("posted") {
            return Err(Error::user("Only a posted CIP settlement can be reversed."));
        }
        let cip_id = doc
            .get(tx, AccountAssetSettlement::CIP_ASSET_ID)
            .await?
            .ok_or_else(|| Error::validation("Missing CIP Asset."))?
            .get();
        let move_id = doc
            .get(tx, AccountAssetSettlement::MOVE_ID)
            .await?
            .ok_or_else(|| Error::validation("Missing settlement journal entry."))?
            .get();
        let total_settled = doc.get(tx, AccountAssetSettlement::TOTAL_SETTLED).await?;

        let slines = tx
            .query(
                "SELECT receiver_type::text, created_asset_id::int8, target_asset_id::int8, amount \
                 FROM account_asset_settlement_line WHERE settlement_id = $1 ORDER BY id",
                &[&doc_id],
            )
            .await?;
        for sl in &slines {
            let created_id: Option<i64> = sl.try_get(1).map_err(db)?;
            if let Some(aid) = created_id {
                let dep_posted: i64 = tx
                    .query(
                        "SELECT count(*)::int8 FROM account_asset_depreciation_line \
                         WHERE asset_id = $1 AND state = 'posted'",
                        &[&aid],
                    )
                    .await?[0]
                    .try_get(0)
                    .map_err(db)?;
                if dep_posted > 0 {
                    return Err(Error::user(
                        "Cannot reverse this CIP settlement because a capitalized Fixed Asset has already posted depreciation.",
                    ));
                }
            }
        }

        let env = doc.env().with_context_replaced(erp_sdk::Context::default());
        let src_move = env.records("account.move")?.browse([move_id])?;
        let rev_id = crate::moves::post::reverse_move_with_options(
            tx,
            &src_move,
            None,
            None,
            Some("CIP Settlement Reversal"),
        )
        .await?;
        post_moves(tx, &env.records("account.move")?.browse([rev_id])?).await?;

        for sl in &slines {
            let rtype: String = sl.try_get(0).map_err(db)?;
            let created_id: Option<i64> = sl.try_get(1).map_err(db)?;
            let target_id: Option<i64> = sl.try_get(2).map_err(db)?;
            let amount: Decimal = sl.try_get(3).map_err(db)?;
            if rtype == "new_asset"
                && let Some(aid) = created_id
            {
                tx.execute(
                    "DELETE FROM account_asset_depreciation_line WHERE asset_id = $1 AND state = 'draft'",
                    &[&aid],
                )
                .await?;
                tx.execute(
                    "UPDATE account_asset SET state = 'close', original_value = 0, \
                            write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                    &[&aid],
                )
                .await?;
            } else if rtype == "existing_asset"
                && let Some(tid) = target_id
            {
                tx.execute(
                    "UPDATE account_asset SET original_value = GREATEST(original_value - $2, 0), \
                            write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                    &[&tid, &amount],
                )
                .await?;
            }
        }

        // Roll back settled_amount LIFO across CIP lines.
        let cip_lines = tx
            .query(
                "SELECT id::int8, settled_amount FROM account_asset_cip_line \
                 WHERE asset_id = $1 AND settled_amount > 0 \
                 ORDER BY date DESC, id DESC",
                &[&cip_id],
            )
            .await?;
        let mut to_restore = total_settled;
        for cl in cip_lines {
            if to_restore <= Decimal::ZERO {
                break;
            }
            let cl_id: i64 = cl.try_get(0).map_err(db)?;
            let cur_settled: Decimal = cl.try_get(1).map_err(db)?;
            let undo = cur_settled.min(to_restore);
            let next_settled = cur_settled - undo;
            tx.execute(
                "UPDATE account_asset_cip_line SET settled_amount = $2, \
                        write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                &[&cl_id, &next_settled],
            )
            .await?;
            to_restore -= undo;
        }

        tx.execute(
            "UPDATE account_asset SET state = 'cip', write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[&cip_id],
        )
        .await?;
        tx.execute(
            "UPDATE account_asset_settlement \
             SET state = 'reversed', reversal_move_id = $2, write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[&doc_id, &rev_id],
        )
        .await?;
        tx.invalidate_records(doc.as_records(), Some(&["state", "reversal_move_id"]))
            .await?;
        tx.invalidate_model(
            "account.asset",
            Some(&[
                "state",
                "original_value",
                "cip_accumulated_amount",
                "cip_settled_amount",
                "cip_unsettled_amount",
                "value_residual",
            ]),
        )
        .await
    }
}

/// `account.asset.settlement.line`: immutable breakdown of one receiver within a CIP settlement.
#[model(
    name = "account.asset.settlement.line",
    description = "CIP Settlement Receiver Line",
    order = "id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountAssetSettlementLine {
    /// Parent settlement document.
    #[field(
        many2one,
        comodel = "account.asset.settlement",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Settlement"
    )]
    pub settlement_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "settlement_id.company_id",
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
    /// Receiver description.
    #[field(char, required = true, string = "Receiver")]
    pub name: Option<Arc<str>>,
    /// Receiver destination kind.
    #[field(
        selection,
        selection = [
            ("new_asset", "New Fixed Asset"),
            ("existing_asset", "Existing Fixed Asset"),
            ("cost_center", "Cost Center Expense"),
        ],
        required = true,
        string = "Receiver Type"
    )]
    pub receiver_type: Option<Arc<str>>,
    /// Debited Fixed Asset or Expense GL account.
    #[field(
        many2one,
        comodel = "account.account",
        required = true,
        ondelete = "restrict",
        string = "Target Account"
    )]
    pub account_id: Option<RecId>,
    /// Newly created fixed asset (when `receiver_type = 'new_asset'`).
    #[field(
        many2one,
        comodel = "account.asset",
        ondelete = "set null",
        string = "Created Asset"
    )]
    pub created_asset_id: Option<RecId>,
    /// Target fixed asset (new or existing).
    #[field(
        many2one,
        comodel = "account.asset",
        ondelete = "set null",
        string = "Target Asset"
    )]
    pub target_asset_id: Option<RecId>,
    /// Cost Center (when expensed or linked to asset).
    #[field(
        many2one,
        comodel = "account.analytic.account",
        ondelete = "set null",
        string = "Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// Settled amount in company currency.
    #[field(monetary, currency_field = "currency_id", string = "Settled Amount")]
    pub amount: Decimal,
}

#[methods(AccountAssetSettlementLine, addon = "account")]
impl AccountAssetSettlementLine {}

#[cfg(test)]
mod tests {
    use super::allocate_settlement_amounts;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn multi_receiver_cip_settlement_allocates_70_20_10_exactly() {
        let cip_total = Decimal::from_str("100000.00").unwrap();
        let rules = vec![
            (1, Decimal::from_str("70.00").unwrap(), Decimal::ZERO),
            (2, Decimal::from_str("20.00").unwrap(), Decimal::ZERO),
            (3, Decimal::from_str("10.00").unwrap(), Decimal::ZERO),
        ];
        let shares = allocate_settlement_amounts(cip_total, &rules).unwrap();
        assert_eq!(shares[0].1, Decimal::from_str("70000.00").unwrap());
        assert_eq!(shares[1].1, Decimal::from_str("20000.00").unwrap());
        assert_eq!(shares[2].1, Decimal::from_str("10000.00").unwrap());
        let sum: Decimal = shares.iter().map(|(_, a)| *a).sum();
        assert_eq!(sum, cip_total);
    }

    #[test]
    fn settlement_rejects_percentages_not_summing_to_100() {
        let cip_total = Decimal::from_str("5000.00").unwrap();
        let rules = vec![
            (1, Decimal::from_str("60.00").unwrap(), Decimal::ZERO),
            (2, Decimal::from_str("30.00").unwrap(), Decimal::ZERO),
        ];
        assert!(allocate_settlement_amounts(cip_total, &rules).is_err());
    }
}
