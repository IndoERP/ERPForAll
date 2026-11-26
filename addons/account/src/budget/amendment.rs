// SPDX-License-Identifier: MIT OR Apache-2.0
//! Budgeting Workbench (`account.budget.amendment`, `account.budget.amendment.line`):
//! auditable, approval-controlled Supplements, Returns, Transfers (Virement), Releases,
//! and Year-End Commitment & Residual Budget Carryforwards.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use super::{db, refresh_budget_lines};

fn default_today(_ctx: &erp_sdk::DefaultContext) -> Value {
    Value::Date(Date::today())
}

/// Validates a set of transfer (virement) deltas: must have at least one negative sender and
/// one positive receiver, and exact sum equal to `0.00`.
pub fn validate_virement_lines(deltas: &[Decimal]) -> Result<()> {
    if deltas.len() < 2 {
        return Err(Error::user(
            "A Budget Transfer (Virement) requires at least one sender line (negative) and one receiver line (positive).",
        ));
    }
    let has_neg = deltas.iter().any(|d| *d < Decimal::ZERO);
    let has_pos = deltas.iter().any(|d| *d > Decimal::ZERO);
    if !has_neg || !has_pos {
        return Err(Error::user(
            "A Budget Transfer must contain both a negative sender amount and a positive receiver amount.",
        ));
    }
    let sum: Decimal = deltas.iter().copied().sum();
    if !sum.round_dp(2).is_zero() {
        return Err(Error::user(format!(
            "Budget Transfer lines must balance to 0.00 (net difference: {sum})."
        )));
    }
    Ok(())
}

/// `account.budget.amendment`: approval-controlled budget revision document.
#[model(
    name = "account.budget.amendment",
    description = "Budget Amendment",
    order = "date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountBudgetAmendment {
    /// Document label (e.g. "Q3 IT Supplement", "Transfer Marketing -> Engineering").
    #[field(char, required = true, string = "Reference")]
    pub name: Option<Arc<str>>,
    /// Budget being amended.
    #[field(
        many2one,
        comodel = "account.budget",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Budget"
    )]
    pub budget_id: Option<RecId>,
    /// Target next-year budget (used when `amendment_type == 'carryforward'`).
    #[field(
        many2one,
        comodel = "account.budget",
        check_company = true,
        ondelete = "restrict",
        string = "Target Next-Year Budget"
    )]
    pub target_budget_id: Option<RecId>,
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
    /// Type of budget entry operation.
    #[field(
        selection,
        selection = [
            ("supplement", "Supplement (Increase Budget)"),
            ("return", "Return (Reduce Budget)"),
            ("transfer", "Transfer / Virement (Zero-Sum)"),
            ("release", "Budget Release"),
            ("carryforward", "Year-End Carryforward"),
        ],
        required = true,
        default = "supplement",
        string = "Process Type"
    )]
    pub amendment_type: Option<Arc<str>>,
    /// Effective date.
    #[field(date, required = true, default_fn = default_today, string = "Date")]
    pub date: Option<Date>,
    /// Business justification.
    #[field(text, string = "Justification")]
    pub reason: Option<Arc<str>>,
    /// Approval workflow state.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("submitted", "Submitted for Approval"),
            ("approved", "Approved & Applied"),
            ("cancelled", "Cancelled"),
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Net sum of amendment line amounts.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_total_delta",
        string = "Net Change"
    )]
    pub total_delta: Decimal,
    /// Amendment lines targeting budget positions.
    #[field(
        one2many,
        comodel = "account.budget.amendment.line",
        inverse_name = "amendment_id",
        string = "Amendment Lines"
    )]
    pub line_ids: Vec<RecId>,
}

#[methods(AccountBudgetAmendment, addon = "account")]
impl AccountBudgetAmendment {
    #[compute(depends = ["line_ids", "line_ids.amount"])]
    async fn _compute_total_delta(
        this: &RecordSet<AccountBudgetAmendment>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountBudgetAmendment>,
    ) -> Result<()> {
        for (i, rid) in this.as_records().rec_ids().iter().enumerate() {
            let sum = if let Some(real) = rid.as_real() {
                tx.query(
                    "SELECT COALESCE(SUM(amount), 0) FROM account_budget_amendment_line WHERE amendment_id = $1",
                    &[&real.get()],
                )
                .await?
                .first()
                .and_then(|r| r.try_get(0).ok())
                .unwrap_or(Decimal::ZERO)
            } else {
                Decimal::ZERO
            };
            out.set(i, AccountBudgetAmendment::TOTAL_DELTA, sum)?;
        }
        Ok(())
    }

    /// Submits a draft amendment for manager approval.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_submit(
        this: &RecordSet<AccountBudgetAmendment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let doc = this.ensure_one()?;
        doc.as_records().check_access(tx, Operation::Write).await?;
        let state = doc.get(tx, AccountBudgetAmendment::STATE).await?;
        if state.as_deref() != Some("draft") {
            return Err(Error::user("Only a draft amendment can be submitted."));
        }
        let id = doc.as_records().ids()[0];
        tx.execute(
            "UPDATE account_budget_amendment SET state = 'submitted', write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
            &[&id],
        )
        .await?;
        tx.invalidate_records(doc.as_records(), Some(&["state"]))
            .await
    }

    /// Approves and atomically applies the amendment (requires Accounting Manager).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_approve(
        this: &RecordSet<AccountBudgetAmendment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let doc = this.ensure_one()?;
        doc.as_records().check_access(tx, Operation::Write).await?;
        if !doc.env().su()
            && !tx
                .has_group(doc.env(), "account.group_account_manager")
                .await?
        {
            return Err(Error::access(
                "Only an Accounting Manager can approve budget amendments.",
            ));
        }
        let doc_id = doc.as_records().ids()[0];
        let state = doc.get(tx, AccountBudgetAmendment::STATE).await?;
        if !matches!(state.as_deref(), Some("draft" | "submitted")) {
            return Err(Error::user(
                "Only a draft or submitted budget amendment can be approved.",
            ));
        }
        let kind = doc
            .get(tx, AccountBudgetAmendment::AMENDMENT_TYPE)
            .await?
            .unwrap_or_else(|| Arc::from("supplement"));
        let target_budget_id = doc
            .get(tx, AccountBudgetAmendment::TARGET_BUDGET_ID)
            .await?
            .map(|r| r.get());

        let lines = tx
            .query(
                "SELECT id::int8, budget_line_id::int8, COALESCE(amount, 0) \
                 FROM account_budget_amendment_line WHERE amendment_id = $1 ORDER BY id",
                &[&doc_id],
            )
            .await?;
        if lines.is_empty() {
            return Err(Error::user(
                "Add at least one budget amendment line before approving.",
            ));
        }

        let mut parsed = Vec::with_capacity(lines.len());
        let mut deltas = Vec::with_capacity(lines.len());
        for row in lines {
            let bl_id: i64 = row.try_get(1).map_err(db)?;
            let amt: Decimal = row.try_get(2).map_err(db)?;
            deltas.push(amt);
            parsed.push((bl_id, amt));
        }

        if kind.as_ref() == "transfer" {
            validate_virement_lines(&deltas)?;
        }

        let mut touched = Vec::new();
        for (bl_id, raw_amt) in parsed {
            let bl_rows = tx
                .query(
                    "SELECT original_amount, revision_amount, planned_amount, \
                            released_amount, committed_amount, practical_amount, \
                            name::text, account_id::int8, analytic_account_id::int8, asset_id::int8 \
                     FROM account_budget_line WHERE id = $1 FOR UPDATE",
                    &[&bl_id],
                )
                .await?;
            let Some(bl) = bl_rows.first() else {
                return Err(Error::missing(format!(
                    "Budget line {bl_id} does not exist."
                )));
            };
            let orig: Decimal = bl.try_get(0).map_err(db)?;
            let rev: Decimal = bl.try_get(1).map_err(db)?;
            let planned: Decimal = bl.try_get(2).map_err(db)?;
            let released: Decimal = bl.try_get(3).map_err(db)?;
            let committed: Decimal = bl.try_get(4).map_err(db)?;
            let practical: Decimal = bl.try_get(5).map_err(db)?;
            let bl_name: String = bl.try_get(6).map_err(db)?;
            let bl_acc: Option<i64> = bl.try_get(7).map_err(db)?;
            let bl_cc: Option<i64> = bl.try_get(8).map_err(db)?;
            let bl_asset: Option<i64> = bl.try_get(9).map_err(db)?;
            let available = released - committed - practical;

            match kind.as_ref() {
                "supplement" => {
                    if raw_amt <= Decimal::ZERO {
                        return Err(Error::user("Budget Supplement amounts must be positive."));
                    }
                    let next_rev = rev + raw_amt;
                    let next_planned = orig + next_rev;
                    let next_released = released + raw_amt;
                    tx.execute(
                        "UPDATE account_budget_line \
                         SET revision_amount = $2, planned_amount = $3, released_amount = $4, \
                             write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                        &[&bl_id, &next_rev, &next_planned, &next_released],
                    )
                    .await?;
                }
                "return" => {
                    let ret = raw_amt.abs();
                    if ret <= Decimal::ZERO {
                        return Err(Error::user("Budget Return amount must be non-zero."));
                    }
                    if ret > available {
                        return Err(Error::user(format!(
                            "Cannot return {ret} from '{bl_name}': only {available} is currently available."
                        )));
                    }
                    let next_rev = rev - ret;
                    let next_planned = orig + next_rev;
                    let next_released = released - ret;
                    tx.execute(
                        "UPDATE account_budget_line \
                         SET revision_amount = $2, planned_amount = $3, released_amount = $4, \
                             write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                        &[&bl_id, &next_rev, &next_planned, &next_released],
                    )
                    .await?;
                }
                "transfer" => {
                    if raw_amt < Decimal::ZERO && raw_amt.abs() > available {
                        return Err(Error::user(format!(
                            "Sender line '{bl_name}' lacks available budget for transfer: requested {}, available {available}.",
                            raw_amt.abs()
                        )));
                    }
                    let next_rev = rev + raw_amt;
                    let next_planned = orig + next_rev;
                    let next_released = released + raw_amt;
                    tx.execute(
                        "UPDATE account_budget_line \
                         SET revision_amount = $2, planned_amount = $3, released_amount = $4, \
                             write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                        &[&bl_id, &next_rev, &next_planned, &next_released],
                    )
                    .await?;
                }
                "release" => {
                    if raw_amt <= Decimal::ZERO {
                        return Err(Error::user("Release amount must be positive."));
                    }
                    let next_released = released + raw_amt;
                    if next_released > planned {
                        return Err(Error::user(format!(
                            "Cannot release {next_released} on '{bl_name}': exceeds Current Budget {planned}."
                        )));
                    }
                    let next_pct = if !planned.is_zero() {
                        (next_released / planned * Decimal::from(100)).round_dp(2)
                    } else {
                        Decimal::from(100)
                    };
                    tx.execute(
                        "UPDATE account_budget_line \
                         SET released_amount = $2, release_pct = $3, \
                             write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                        &[&bl_id, &next_released, &next_pct],
                    )
                    .await?;
                }
                "carryforward" => {
                    let tgt_bid = target_budget_id.ok_or_else(|| {
                        Error::validation(
                            "Select a Target Next-Year Budget for Year-End Carryforward.",
                        )
                    })?;
                    let carry = if raw_amt > Decimal::ZERO {
                        raw_amt.min(available)
                    } else {
                        available.max(Decimal::ZERO)
                    };
                    let tgt_dates = tx
                        .query(
                            "SELECT date_from, date_to FROM account_budget WHERE id = $1",
                            &[&tgt_bid],
                        )
                        .await?;
                    let Some(td) = tgt_dates.first() else {
                        return Err(Error::missing("Target Next-Year Budget does not exist."));
                    };
                    let t_from: Date = td.try_get(0).map_err(db)?;
                    let t_to: Date = td.try_get(1).map_err(db)?;

                    // Deduct carried residual from old year line.
                    if carry > Decimal::ZERO {
                        let next_rev = rev - carry;
                        let next_planned = orig + next_rev;
                        let next_released = released - carry;
                        tx.execute(
                            "UPDATE account_budget_line \
                             SET revision_amount = $2, planned_amount = $3, released_amount = $4, \
                                 write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                            &[&bl_id, &next_rev, &next_planned, &next_released],
                        )
                        .await?;
                    }

                    // Create carryforward budget line in target fiscal year budget.
                    let env = doc.env().with_context_replaced(erp_sdk::Context::default());
                    let mut new_bl = RawVals::new();
                    new_bl.insert("budget_id".into(), Cell::Id(tgt_bid));
                    new_bl.insert(
                        "name".into(),
                        Cell::Str(format!("Carryforward: {bl_name}").into()),
                    );
                    if let Some(a) = bl_acc {
                        new_bl.insert("account_id".into(), Cell::Id(a));
                    }
                    if let Some(cc) = bl_cc {
                        new_bl.insert("analytic_account_id".into(), Cell::Id(cc));
                    }
                    if let Some(aid) = bl_asset {
                        new_bl.insert("asset_id".into(), Cell::Id(aid));
                    }
                    new_bl.insert("date_from".into(), Cell::Date(t_from));
                    new_bl.insert("date_to".into(), Cell::Date(t_to));
                    new_bl.insert("original_amount".into(), Cell::Decimal(Decimal::ZERO));
                    new_bl.insert("revision_amount".into(), Cell::Decimal(carry + committed));
                    new_bl.insert("planned_amount".into(), Cell::Decimal(carry + committed));
                    new_bl.insert("released_amount".into(), Cell::Decimal(carry + committed));
                    let created_bl = env
                        .records("account.budget.line")?
                        .create(tx, vec![new_bl])
                        .await?;
                    let new_bl_id = created_bl.ids()[0];

                    // Re-point open PO commitments from prior-year line to carryforward line.
                    tx.execute(
                        "UPDATE account_budget_commitment \
                         SET budget_line_id = $2, write_date = now() AT TIME ZONE 'UTC' \
                         WHERE budget_line_id = $1 AND state IN ('open', 'partially_billed')",
                        &[&bl_id, &new_bl_id],
                    )
                    .await?;
                    touched.push(new_bl_id);
                }
                other => {
                    return Err(Error::validation(format!(
                        "Unknown amendment type '{other}'."
                    )));
                }
            }
            if !touched.contains(&bl_id) {
                touched.push(bl_id);
            }
        }

        refresh_budget_lines(tx, &touched).await?;
        tx.execute(
            "UPDATE account_budget_amendment SET state = 'approved', write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
            &[&doc_id],
        )
        .await?;
        tx.invalidate_records(doc.as_records(), Some(&["state"]))
            .await?;
        tx.invalidate_model(
            "account.budget.line",
            Some(&[
                "revision_amount",
                "planned_amount",
                "released_amount",
                "release_pct",
                "committed_amount",
                "available_amount",
                "avc_status",
            ]),
        )
        .await?;
        tx.invalidate_model(
            "account.budget",
            Some(&[
                "total_revision",
                "total_planned",
                "total_released",
                "total_committed",
                "total_available",
            ]),
        )
        .await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_cancel(
        this: &RecordSet<AccountBudgetAmendment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let doc = this.ensure_one()?;
        doc.as_records().check_access(tx, Operation::Write).await?;
        let state = doc.get(tx, AccountBudgetAmendment::STATE).await?;
        if state.as_deref() == Some("approved") {
            return Err(Error::user(
                "Approved amendments cannot be cancelled; create a compensating Return or Transfer.",
            ));
        }
        let id = doc.as_records().ids()[0];
        tx.execute(
            "UPDATE account_budget_amendment SET state = 'cancelled', write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
            &[&id],
        )
        .await?;
        tx.invalidate_records(doc.as_records(), Some(&["state"]))
            .await
    }
}

/// `account.budget.amendment.line`: individual budget line adjustment inside an amendment document.
#[model(
    name = "account.budget.amendment.line",
    description = "Budget Amendment Line",
    order = "id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountBudgetAmendmentLine {
    /// Parent amendment document.
    #[field(
        many2one,
        comodel = "account.budget.amendment",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Amendment"
    )]
    pub amendment_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "amendment_id.company_id",
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
    /// Target budget line.
    #[field(
        many2one,
        comodel = "account.budget.line",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Budget Line"
    )]
    pub budget_line_id: Option<RecId>,
    /// Signed change amount (for transfers: negative on sender, positive on receiver).
    #[field(monetary, currency_field = "currency_id", string = "Amount")]
    pub amount: Decimal,
    /// Optional note.
    #[field(char, string = "Note")]
    pub note: Option<Arc<str>>,
}

#[methods(AccountBudgetAmendmentLine, addon = "account")]
impl AccountBudgetAmendmentLine {}

#[cfg(test)]
mod tests {
    use super::validate_virement_lines;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn virement_requires_exact_zero_sum_between_sender_and_receiver() {
        let valid = vec![
            Decimal::from_str("-5000.00").unwrap(),
            Decimal::from_str("3000.00").unwrap(),
            Decimal::from_str("2000.00").unwrap(),
        ];
        assert!(validate_virement_lines(&valid).is_ok());

        let unbalanced = vec![
            Decimal::from_str("-5000.00").unwrap(),
            Decimal::from_str("4999.99").unwrap(),
        ];
        assert!(validate_virement_lines(&unbalanced).is_err());
    }
}
