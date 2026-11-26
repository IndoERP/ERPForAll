// SPDX-License-Identifier: MIT OR Apache-2.0
//! Financial & Controlling Budgets (`account.budget.profile`, `account.budget`,
//! `account.budget.line`) with Real-Time Availability Control (AVC), Purchase Commitments,
//! Budget Release, and Budgeting Workbench Amendments.

pub mod amendment;
pub mod avc;

use erp_sdk::Operation;
use erp_sdk::prelude::*;

/// Pure exact-decimal budget line calculation: returns `(theoretical_amount, achievement_pct)`.
/// Theoretical amount is linearly pro-rated over `[date_from, date_to]` as of `today`.
pub fn compute_budget_line_metrics(
    planned: Decimal,
    practical: Decimal,
    date_from: Date,
    date_to: Date,
    today: Date,
) -> (Decimal, Decimal) {
    let total_days = (date_to.days() - date_from.days() + 1).max(1);
    let theoretical = if today.days() < date_from.days() {
        Decimal::ZERO
    } else if today.days() >= date_to.days() {
        planned
    } else {
        let elapsed = today.days() - date_from.days() + 1;
        (planned * Decimal::from(elapsed) / Decimal::from(total_days)).round_dp(2)
    };
    let pct = if !theoretical.is_zero() {
        (practical / theoretical * Decimal::from(100)).round_dp(2)
    } else if !planned.is_zero() {
        (practical / planned * Decimal::from(100)).round_dp(2)
    } else {
        Decimal::ZERO
    };
    (theoretical, pct)
}

pub(crate) fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Budget query failed: {err}"))
}

pub(crate) fn default_year_start(_ctx: &erp_sdk::DefaultContext) -> Value {
    let (y, _, _) = Date::today().ymd();
    Value::Date(Date::from_ymd(y, 1, 1).unwrap_or_else(Date::today))
}

pub(crate) fn default_year_end(_ctx: &erp_sdk::DefaultContext) -> Value {
    let (y, _, _) = Date::today().ymd();
    Value::Date(Date::from_ymd(y, 12, 31).unwrap_or_else(Date::today))
}

/// `account.budget.profile`: Availability Control (AVC) tolerance and enforcement profile.
#[model(
    name = "account.budget.profile",
    description = "Budget Control Profile",
    order = "code, name, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"),
    keys = [key(
        name = "_unique_company_code",
        fields = ["company_id", "code"],
        message = "Budget control profile code must be unique per company."
    )]
)]
pub struct AccountBudgetProfile {
    /// Profile name (e.g. "Strict OPEX Hard-Stop 80/100").
    #[field(char, required = true, string = "Profile Name")]
    pub name: Option<Arc<str>>,
    /// Short code (e.g. "AVC-STRICT").
    #[field(char, required = true, index = true, string = "Code")]
    pub code: Option<Arc<str>>,
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
    /// Enforcement level when consumed budget exceeds thresholds.
    #[field(
        selection,
        selection = [
            ("none", "Passive / Informational Only"),
            ("warning", "Soft Warning Only"),
            ("hard_stop", "Hard Stop (Block Exceeding)"),
        ],
        required = true,
        default = "hard_stop",
        string = "Availability Control Mode"
    )]
    pub avc_mode: Option<Arc<str>>,
    /// Utilization percentage triggering Warning status (default 80%).
    #[field(
        float,
        digits = (16, 2),
        required = true,
        default = 80,
        string = "Warning Threshold (%)"
    )]
    pub warning_threshold_pct: Decimal,
    /// Utilization percentage triggering Hard Stop refusal (default 100%).
    #[field(
        float,
        digits = (16, 2),
        required = true,
        default = 100,
        string = "Blocking Threshold (%)"
    )]
    pub blocking_threshold_pct: Decimal,
    /// Optional absolute grace buffer in company currency above the percentage threshold.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Tolerance Amount"
    )]
    pub tolerance_amount: Decimal,
    /// Whether open Purchase Order commitments reduce available budget in real time.
    #[field(boolean, default = true, string = "Include PO Commitments")]
    pub check_commitments: bool,
    /// Active flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
}

#[methods(AccountBudgetProfile, addon = "account")]
impl AccountBudgetProfile {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountBudgetProfile>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "company_id")
            && !values.contains_key("company_id")
            && let Some(cid) = this.env().company_id()?
        {
            values.insert("company_id".to_owned(), Value::Int(cid));
        }
        Ok(values)
    }

    #[constrains("warning_threshold_pct", "blocking_threshold_pct", "tolerance_amount")]
    async fn _check_thresholds(
        this: &RecordSet<AccountBudgetProfile>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let warns = this
            .mapped(tx, AccountBudgetProfile::WARNING_THRESHOLD_PCT)
            .await?;
        let blocks = this
            .mapped(tx, AccountBudgetProfile::BLOCKING_THRESHOLD_PCT)
            .await?;
        let tols = this
            .mapped(tx, AccountBudgetProfile::TOLERANCE_AMOUNT)
            .await?;
        for ((w, b), t) in warns.into_iter().zip(blocks).zip(tols) {
            if w <= Decimal::ZERO || b <= Decimal::ZERO {
                return Err(Error::validation(
                    "Warning and Blocking threshold percentages must be greater than zero.",
                ));
            }
            if b < w {
                return Err(Error::validation(
                    "Blocking threshold percentage cannot be lower than Warning threshold percentage.",
                ));
            }
            if t < Decimal::ZERO {
                return Err(Error::validation("Tolerance amount cannot be negative."));
            }
        }
        Ok(())
    }
}

/// `account.budget`: periodic financial & controlling budget with AVC profile, release control,
/// live PO commitments, and actual spend tracking.
#[model(
    name = "account.budget",
    description = "Budget",
    order = "date_from desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountBudget {
    /// Budget name.
    #[field(char, required = true, string = "Budget Name")]
    pub name: Option<Arc<str>>,
    /// Budget version tag (e.g. "V0", "REV-Q2").
    #[field(char, default = "V0", string = "Version")]
    pub version_code: Option<Arc<str>>,
    /// Budget classification driving sign and AVC enforcement.
    #[field(
        selection,
        selection = [
            ("opex", "Operating Expense (OPEX)"),
            ("capex", "Capital / CIP Investment (CAPEX)"),
            ("revenue", "Revenue Target"),
        ],
        required = true,
        default = "opex",
        string = "Budget Type"
    )]
    pub budget_type: Option<Arc<str>>,
    /// Availability Control tolerance profile applied when this budget is validated.
    #[field(
        many2one,
        comodel = "account.budget.profile",
        check_company = true,
        ondelete = "restrict",
        string = "AVC Control Profile"
    )]
    pub profile_id: Option<RecId>,
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
    /// Budget start date.
    #[field(date, required = true, default_fn = default_year_start, string = "Start Date")]
    pub date_from: Option<Date>,
    /// Budget end date.
    #[field(date, required = true, default_fn = default_year_end, string = "End Date")]
    pub date_to: Option<Date>,
    /// Workflow status.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("confirm", "Confirmed"),
            ("validate", "Approved (AVC Active)"),
            ("done", "Closed"),
            ("cancel", "Cancelled"),
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Budget lines.
    #[field(
        one2many,
        comodel = "account.budget.line",
        inverse_name = "budget_id",
        string = "Budget Lines"
    )]
    pub line_ids: Vec<RecId>,
    /// Total original baseline across lines.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_totals",
        string = "Total Original"
    )]
    pub total_original: Decimal,
    /// Net revisions (supplements, returns, transfers, carryforwards).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_totals",
        string = "Total Revisions"
    )]
    pub total_revision: Decimal,
    /// Total current budget (`original + revision`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_totals",
        string = "Total Current Budget"
    )]
    pub total_planned: Decimal,
    /// Total released spendable budget.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_totals",
        string = "Total Released"
    )]
    pub total_released: Decimal,
    /// Total open PO commitments.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_totals",
        string = "Total Committed"
    )]
    pub total_committed: Decimal,
    /// Total actual posted spend/revenue.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_totals",
        string = "Total Actual"
    )]
    pub total_practical: Decimal,
    /// Total remaining available budget (`released - committed - practical`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_totals",
        string = "Total Available"
    )]
    pub total_available: Decimal,
}

#[methods(AccountBudget, addon = "account")]
impl AccountBudget {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountBudget>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "company_id")
            && !values.contains_key("company_id")
            && let Some(cid) = this.env().company_id()?
        {
            values.insert("company_id".to_owned(), Value::Int(cid));
        }
        Ok(values)
    }

    #[compute(depends = [
        "line_ids",
        "line_ids.original_amount",
        "line_ids.revision_amount",
        "line_ids.planned_amount",
        "line_ids.released_amount",
        "line_ids.committed_amount",
        "line_ids.practical_amount",
    ])]
    async fn _compute_totals(
        this: &RecordSet<AccountBudget>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountBudget>,
    ) -> Result<()> {
        for (i, rid) in this.as_records().rec_ids().iter().enumerate() {
            let (orig, rev, planned, released, committed, practical) =
                if let Some(real) = rid.as_real() {
                    let rows = tx
                        .query(
                            "SELECT COALESCE(SUM(original_amount), 0), \
                                    COALESCE(SUM(revision_amount), 0), \
                                    COALESCE(SUM(planned_amount), 0), \
                                    COALESCE(SUM(released_amount), 0), \
                                    COALESCE(SUM(committed_amount), 0), \
                                    COALESCE(SUM(practical_amount), 0) \
                             FROM account_budget_line WHERE budget_id = $1",
                            &[&real.get()],
                        )
                        .await?;
                    if let Some(row) = rows.first() {
                        (
                            row.try_get::<_, Decimal>(0).map_err(db)?,
                            row.try_get::<_, Decimal>(1).map_err(db)?,
                            row.try_get::<_, Decimal>(2).map_err(db)?,
                            row.try_get::<_, Decimal>(3).map_err(db)?,
                            row.try_get::<_, Decimal>(4).map_err(db)?,
                            row.try_get::<_, Decimal>(5).map_err(db)?,
                        )
                    } else {
                        (
                            Decimal::ZERO,
                            Decimal::ZERO,
                            Decimal::ZERO,
                            Decimal::ZERO,
                            Decimal::ZERO,
                            Decimal::ZERO,
                        )
                    }
                } else {
                    (
                        Decimal::ZERO,
                        Decimal::ZERO,
                        Decimal::ZERO,
                        Decimal::ZERO,
                        Decimal::ZERO,
                        Decimal::ZERO,
                    )
                };
            out.set(i, AccountBudget::TOTAL_ORIGINAL, orig)?;
            out.set(i, AccountBudget::TOTAL_REVISION, rev)?;
            out.set(i, AccountBudget::TOTAL_PLANNED, planned)?;
            out.set(i, AccountBudget::TOTAL_RELEASED, released)?;
            out.set(i, AccountBudget::TOTAL_COMMITTED, committed)?;
            out.set(i, AccountBudget::TOTAL_PRACTICAL, practical)?;
            out.set(
                i,
                AccountBudget::TOTAL_AVAILABLE,
                released - committed - practical,
            )?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_budget_confirm(
        this: &RecordSet<AccountBudget>,
        tx: &mut Transaction,
    ) -> Result<()> {
        set_budget_state(this, tx, "confirm").await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_budget_validate(
        this: &RecordSet<AccountBudget>,
        tx: &mut Transaction,
    ) -> Result<()> {
        // Ensure original/released defaults are normalized on every line before activating AVC.
        let ids = this.as_records().ids();
        tx.execute(
            "UPDATE account_budget_line \
             SET original_amount = CASE WHEN COALESCE(original_amount, 0) = 0 AND COALESCE(planned_amount, 0) <> 0 \
                                        THEN planned_amount ELSE COALESCE(original_amount, 0) END, \
                 revision_amount = COALESCE(revision_amount, 0), \
                 planned_amount = CASE WHEN COALESCE(original_amount, 0) = 0 AND COALESCE(planned_amount, 0) <> 0 \
                                       THEN planned_amount ELSE COALESCE(original_amount, 0) + COALESCE(revision_amount, 0) END, \
                 release_pct = CASE WHEN COALESCE(release_pct, 0) <= 0 THEN 100 ELSE release_pct END \
             WHERE budget_id = ANY($1::int8[])",
            &[&ids],
        )
        .await?;
        tx.execute(
            "UPDATE account_budget_line \
             SET released_amount = CASE WHEN COALESCE(released_amount, 0) = 0 AND COALESCE(planned_amount, 0) <> 0 \
                                        THEN ROUND(planned_amount * release_pct / 100.0, 2) \
                                        ELSE released_amount END \
             WHERE budget_id = ANY($1::int8[])",
            &[&ids],
        )
        .await?;
        set_budget_state(this, tx, "validate").await?;
        Self::action_refresh_practical(this, tx).await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_budget_done(
        this: &RecordSet<AccountBudget>,
        tx: &mut Transaction,
    ) -> Result<()> {
        Self::action_refresh_practical(this, tx).await?;
        set_budget_state(this, tx, "done").await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_budget_cancel(
        this: &RecordSet<AccountBudget>,
        tx: &mut Transaction,
    ) -> Result<()> {
        set_budget_state(this, tx, "cancel").await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_budget_draft(
        this: &RecordSet<AccountBudget>,
        tx: &mut Transaction,
    ) -> Result<()> {
        set_budget_state(this, tx, "draft").await
    }

    /// Recomputes `committed_amount`, `practical_amount`, `theoretical_amount`, `percentage`,
    /// and `avc_status` on every line of this budget from open PO commitments and posted GL /
    /// analytic items (respecting Cost Center hierarchy subtrees).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_refresh_practical(
        this: &RecordSet<AccountBudget>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        let bids = this.as_records().ids();
        let line_rows = tx
            .query(
                "SELECT id::int8 FROM account_budget_line WHERE budget_id = ANY($1::int8[]) ORDER BY id",
                &[&bids],
            )
            .await?;
        let line_ids: Vec<i64> = line_rows
            .iter()
            .map(|r| r.try_get(0).map_err(db))
            .collect::<Result<_>>()?;
        refresh_budget_lines(tx, &line_ids).await?;
        tx.invalidate_records(
            this.as_records(),
            Some(&[
                "line_ids",
                "total_original",
                "total_revision",
                "total_planned",
                "total_released",
                "total_committed",
                "total_practical",
                "total_available",
            ]),
        )
        .await?;
        tx.invalidate_model(
            "account.budget.line",
            Some(&[
                "committed_amount",
                "practical_amount",
                "assigned_amount",
                "available_amount",
                "theoretical_amount",
                "percentage",
                "avc_status",
            ]),
        )
        .await
    }
}

/// Refreshes stored `committed_amount`, `practical_amount`, `theoretical_amount`, `percentage`,
/// and `avc_status` for the given `account.budget.line` ids.
pub async fn refresh_budget_lines(tx: &mut Transaction, line_ids: &[i64]) -> Result<()> {
    if line_ids.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT l.id::int8, l.company_id::int8, COALESCE(b.budget_type, 'opex')::text, \
                    l.account_id::int8, l.analytic_account_id::int8, l.asset_id::int8, \
                    l.date_from, l.date_to, COALESCE(l.planned_amount, 0), \
                    COALESCE(l.released_amount, l.planned_amount, 0), \
                    COALESCE(p.warning_threshold_pct, 80), \
                    COALESCE(p.blocking_threshold_pct, 100), \
                    COALESCE(p.tolerance_amount, 0) \
             FROM account_budget_line l \
             JOIN account_budget b ON b.id = l.budget_id \
             LEFT JOIN account_budget_profile p ON p.id = b.profile_id \
             WHERE l.id = ANY($1::int8[]) ORDER BY l.id",
            &[&line_ids],
        )
        .await?;

    for row in rows {
        let lid: i64 = row.try_get(0).map_err(db)?;
        let company_id: i64 = row.try_get(1).map_err(db)?;
        let budget_type: String = row.try_get(2).map_err(db)?;
        let account_id: Option<i64> = row.try_get(3).map_err(db)?;
        let analytic_id: Option<i64> = row.try_get(4).map_err(db)?;
        let asset_id: Option<i64> = row.try_get(5).map_err(db)?;
        let d_from: Date = row.try_get(6).map_err(db)?;
        let d_to: Date = row.try_get(7).map_err(db)?;
        let planned: Decimal = row.try_get(8).map_err(db)?;
        let released: Decimal = row.try_get(9).map_err(db)?;
        let warn_pct: Decimal = row.try_get(10).map_err(db)?;
        let block_pct: Decimal = row.try_get(11).map_err(db)?;
        let tol_amt: Decimal = row.try_get(12).map_err(db)?;

        let today = erp_addon_base::company_calendar::business_date(tx, company_id)
            .await
            .unwrap_or_else(|_| Date::today());

        let committed: Decimal = tx
            .query(
                "SELECT COALESCE(SUM(open_amount), 0) FROM account_budget_commitment \
                 WHERE budget_line_id = $1 AND state IN ('open', 'partially_billed')",
                &[&lid],
            )
            .await?
            .first()
            .and_then(|r| r.try_get(0).ok())
            .unwrap_or(Decimal::ZERO);

        let is_revenue = budget_type == "revenue";
        let practical: Decimal = if let Some(aid) = asset_id {
            // CAPEX / CIP project budget: sum posted CIP/Asset debits on this asset in period.
            tx.query(
                "SELECT COALESCE(SUM(balance), 0) FROM account_move_line \
                 WHERE asset_id = $1 AND company_id = $2 \
                   AND parent_state = 'posted' AND date >= $3 AND date <= $4",
                &[&aid, &company_id, &d_from, &d_to],
            )
            .await?
            .first()
            .and_then(|r| r.try_get(0).ok())
            .unwrap_or(Decimal::ZERO)
        } else if let Some(cc_id) = analytic_id {
            // Cost Center subtree rollup + optional GL account filter.
            if let Some(acc) = account_id {
                let sign_expr = if is_revenue {
                    "-l.balance"
                } else {
                    "l.balance"
                };
                let sql = format!(
                    "SELECT COALESCE(SUM({sign_expr}), 0) \
                     FROM account_move_line l \
                     JOIN account_analytic_account cc ON cc.id = l.analytic_account_id \
                     JOIN account_analytic_account p ON p.id = $1 \
                     WHERE l.account_id = $2 AND l.company_id = $3 \
                       AND l.parent_state = 'posted' AND l.date >= $4 AND l.date <= $5 \
                       AND (cc.id = p.id OR (p.parent_path IS NOT NULL AND cc.parent_path IS NOT NULL \
                            AND starts_with(cc.parent_path, p.parent_path)))"
                );
                tx.query(&sql, &[&cc_id, &acc, &company_id, &d_from, &d_to])
                    .await?
                    .first()
                    .and_then(|r| r.try_get(0).ok())
                    .unwrap_or(Decimal::ZERO)
            } else {
                let sign_expr = if is_revenue { "l.amount" } else { "-l.amount" };
                let sql = format!(
                    "SELECT COALESCE(SUM({sign_expr}), 0) \
                     FROM account_analytic_line l \
                     JOIN account_analytic_account cc ON cc.id = l.account_id \
                     JOIN account_analytic_account p ON p.id = $1 \
                     WHERE l.date >= $2 AND l.date <= $3 \
                       AND (cc.id = p.id OR (p.parent_path IS NOT NULL AND cc.parent_path IS NOT NULL \
                            AND starts_with(cc.parent_path, p.parent_path)))"
                );
                tx.query(&sql, &[&cc_id, &d_from, &d_to])
                    .await?
                    .first()
                    .and_then(|r| r.try_get(0).ok())
                    .unwrap_or(Decimal::ZERO)
            }
        } else if let Some(acc) = account_id {
            let sign_expr = if is_revenue { "-balance" } else { "balance" };
            let sql = format!(
                "SELECT COALESCE(SUM({sign_expr}), 0) FROM account_move_line \
                 WHERE account_id = $1 AND company_id = $2 \
                   AND parent_state = 'posted' AND date >= $3 AND date <= $4"
            );
            tx.query(&sql, &[&acc, &company_id, &d_from, &d_to])
                .await?
                .first()
                .and_then(|r| r.try_get(0).ok())
                .unwrap_or(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };

        let (theoretical, pct) =
            compute_budget_line_metrics(planned, practical, d_from, d_to, today);
        let assigned = committed + practical;
        let avc_status = if is_revenue {
            "ok"
        } else {
            let base = released.abs();
            let block_limit = (base * block_pct / Decimal::from(100)).round_dp(2) + tol_amt;
            let warn_limit = (base * warn_pct / Decimal::from(100)).round_dp(2);
            if !base.is_zero() && assigned > block_limit {
                "exceeded"
            } else if !base.is_zero() && assigned >= warn_limit {
                "warning"
            } else {
                "ok"
            }
        };

        tx.execute(
            "UPDATE account_budget_line \
             SET committed_amount = $2, practical_amount = $3, theoretical_amount = $4, \
                 percentage = $5, avc_status = $6, write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[
                &lid,
                &committed,
                &practical,
                &theoretical,
                &pct,
                &avc_status,
            ],
        )
        .await?;
    }
    Ok(())
}

async fn set_budget_state(
    this: &RecordSet<AccountBudget>,
    tx: &mut Transaction,
    target: &str,
) -> Result<()> {
    this.as_records().check_access(tx, Operation::Write).await?;
    let ids = this.as_records().ids();
    tx.execute(
        "UPDATE account_budget SET state = $2, write_date = now() AT TIME ZONE 'UTC' \
         WHERE id = ANY($1::int8[])",
        &[&ids, &target],
    )
    .await?;
    tx.invalidate_records(this.as_records(), Some(&["state"]))
        .await
}

/// `account.budget.line`: one budgetary control line keyed by GL account, Cost Center (or parent
/// Cost Center Group), and/or CIP Asset with original/revised/released/committed/actual columns.
#[model(
    name = "account.budget.line",
    description = "Budget Line",
    order = "date_from, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountBudgetLine {
    /// Owning budget.
    #[field(
        many2one,
        comodel = "account.budget",
        required = true,
        ondelete = "cascade",
        string = "Budget"
    )]
    pub budget_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "budget_id.company_id",
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
    /// Budgetary position label.
    #[field(char, required = true, string = "Budgetary Position")]
    pub name: Option<Arc<str>>,
    /// General ledger account tracked.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Account"
    )]
    pub account_id: Option<RecId>,
    /// Cost Center (or Cost Center Group parent) tracked.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "restrict",
        string = "Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// Optional CIP / Capital Project asset tracked (for CAPEX budgets).
    #[field(
        many2one,
        comodel = "account.asset",
        check_company = true,
        ondelete = "restrict",
        string = "CIP Project"
    )]
    pub asset_id: Option<RecId>,
    /// Line period start date.
    #[field(date, required = true, default_fn = default_year_start, string = "Start Date")]
    pub date_from: Option<Date>,
    /// Line period end date.
    #[field(date, required = true, default_fn = default_year_end, string = "End Date")]
    pub date_to: Option<Date>,
    /// Initial approved baseline budget.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Original Budget"
    )]
    pub original_amount: Decimal,
    /// Net cumulative approved amendments (supplements, returns, transfers, carryforwards).
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        default = 0,
        string = "Revisions"
    )]
    pub revision_amount: Decimal,
    /// Current total budget (`original_amount + revision_amount`).
    #[field(monetary, currency_field = "currency_id", string = "Current Budget")]
    pub planned_amount: Decimal,
    /// Percentage of Current Budget released for spending (default 100%).
    #[field(float, digits = (16, 2), default = 100, string = "Release %")]
    pub release_pct: Decimal,
    /// Spendable budget released for Availability Control checks.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Released Budget"
    )]
    pub released_amount: Decimal,
    /// Open unbilled Purchase Order commitments consuming this budget line.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        default = 0,
        string = "Committed (PO)"
    )]
    pub committed_amount: Decimal,
    /// Actual posted spend/revenue achieved in period.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        default = 0,
        string = "Actual (Posted)"
    )]
    pub practical_amount: Decimal,
    /// Total consumed budget (`committed_amount + practical_amount`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_availability",
        string = "Assigned"
    )]
    pub assigned_amount: Decimal,
    /// Remaining spendable budget (`released_amount - committed_amount - practical_amount`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_availability",
        string = "Available"
    )]
    pub available_amount: Decimal,
    /// Pro-rata theoretical target as of today.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Theoretical Amount"
    )]
    pub theoretical_amount: Decimal,
    /// Utilization / achievement percentage.
    #[field(float, digits = (16, 2), readonly = true, string = "Utilization %")]
    pub percentage: Decimal,
    /// Current AVC health status.
    #[field(
        selection,
        selection = [
            ("ok", "Within Budget"),
            ("warning", "Warning"),
            ("exceeded", "Exceeded"),
        ],
        required = true,
        readonly = true,
        default = "ok",
        string = "AVC Status"
    )]
    pub avc_status: Option<Arc<str>>,
}

#[methods(AccountBudgetLine, addon = "account")]
impl AccountBudgetLine {
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountBudgetLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for vals in &mut vals_list {
            normalize_budget_line_vals(vals);
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[compute(depends = ["released_amount", "planned_amount", "committed_amount", "practical_amount"])]
    fn _compute_availability(batch: &mut Batch<'_, AccountBudgetLine>) -> Result<()> {
        for i in batch.indices() {
            let released = batch.get(i, AccountBudgetLine::RELEASED_AMOUNT)?;
            let planned = batch.get(i, AccountBudgetLine::PLANNED_AMOUNT)?;
            let effective_released = if released.is_zero() && !planned.is_zero() {
                planned
            } else {
                released
            };
            let committed = batch.get(i, AccountBudgetLine::COMMITTED_AMOUNT)?;
            let practical = batch.get(i, AccountBudgetLine::PRACTICAL_AMOUNT)?;
            let assigned = committed + practical;
            batch.set(i, AccountBudgetLine::ASSIGNED_AMOUNT, assigned)?;
            batch.set(
                i,
                AccountBudgetLine::AVAILABLE_AMOUNT,
                effective_released - assigned,
            )?;
        }
        Ok(())
    }
}

fn decimal_cell(cell: Option<&Cell>) -> Option<Decimal> {
    match cell {
        Some(Cell::Decimal(d)) => Some(*d),
        Some(Cell::Int(i)) => Some(Decimal::from(*i)),
        _ => None,
    }
}

fn normalize_budget_line_vals(vals: &mut RawVals) {
    let orig = decimal_cell(vals.get("original_amount"));
    let planned = decimal_cell(vals.get("planned_amount"));
    let rev = decimal_cell(vals.get("revision_amount")).unwrap_or(Decimal::ZERO);
    let release_pct = decimal_cell(vals.get("release_pct")).unwrap_or(Decimal::from(100));

    let (final_orig, final_planned) = match (orig, planned) {
        (Some(o), None) => (o, o + rev),
        (None, Some(p)) => (p, p + rev),
        (Some(o), Some(p)) if o.is_zero() && !p.is_zero() => (p, p + rev),
        (Some(o), Some(_)) => (o, o + rev),
        (None, None) => (Decimal::ZERO, Decimal::ZERO),
    };
    vals.insert("original_amount".into(), Cell::Decimal(final_orig));
    vals.insert("planned_amount".into(), Cell::Decimal(final_planned));
    vals.insert("release_pct".into(), Cell::Decimal(release_pct));
    if decimal_cell(vals.get("released_amount")).is_none() {
        let released = (final_planned * release_pct / Decimal::from(100)).round_dp(2);
        vals.insert("released_amount".into(), Cell::Decimal(released));
    }
}

#[cfg(test)]
mod tests {
    use super::compute_budget_line_metrics;
    use erp_sdk::{Date, Decimal};
    use std::str::FromStr;

    #[test]
    fn budget_metrics_compute_exact_prorata_theoretical_and_percentage() {
        let d_from = Date::from_ymd(2026, 1, 1).unwrap();
        let d_to = Date::from_ymd(2026, 1, 10).unwrap(); // 10 days inclusive
        let today = Date::from_ymd(2026, 1, 5).unwrap(); // 5 days elapsed => 50%
        let planned = Decimal::from_str("1000.00").unwrap();
        let practical = Decimal::from_str("400.00").unwrap();
        let (theoretical, pct) =
            compute_budget_line_metrics(planned, practical, d_from, d_to, today);
        assert_eq!(theoretical, Decimal::from_str("500.00").unwrap());
        assert_eq!(pct, Decimal::from_str("80.00").unwrap());
    }
}
