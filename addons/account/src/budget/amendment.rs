// SPDX-License-Identifier: MIT OR Apache-2.0
//! Budgeting Workbench (`account.budget.amendment`, `account.budget.amendment.line`):
//! auditable, approval-controlled Supplements, Returns, Transfers (Virement), Releases,
//! and Year-End Commitment & Residual Budget Carryforwards.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use super::{db, refresh_budget_lines};

/// Validates a set of transfer (virement) deltas: must have at least one negative sender and
/// one positive receiver, and exact sum equal to `0` at currency `scale`.
pub fn validate_virement_lines(deltas: &[Decimal]) -> Result<()> {
    validate_virement_lines_with_scale(deltas, 2)
}

/// Currency-scale-aware virement balance validation.
pub fn validate_virement_lines_with_scale(deltas: &[Decimal], scale: u32) -> Result<()> {
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
    if !sum.round_dp(scale).is_zero() {
        return Err(Error::user(format!(
            "Budget Transfer lines must balance to 0 (net difference: {sum})."
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
    #[field(date, required = true, string = "Date")]
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
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountBudgetAmendment>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "date") && !values.contains_key("date") {
            let today = if let Some(cid) = this.env().company_id()? {
                erp_addon_base::company_calendar::business_date(tx, cid).await?
            } else {
                this.env().context_today()?
            };
            values.insert("date".to_owned(), Value::Date(today));
        }
        Ok(values)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountBudgetAmendment>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() && !this.env().su() {
            if vals.contains("state") {
                return Err(Error::user(
                    "Budget amendment status cannot be edited directly; use Submit, Approve, or Cancel.",
                ));
            }
            for state in this.mapped(tx, AccountBudgetAmendment::STATE).await? {
                if state.as_deref() != Some("draft") {
                    return Err(Error::user("Only draft budget amendments can be edited."));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountBudgetAmendment>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() && !this.env().su() {
            for state in this.mapped(tx, AccountBudgetAmendment::STATE).await? {
                if state.as_deref() == Some("approved") {
                    return Err(Error::user("Approved budget amendments cannot be deleted."));
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    #[compute(depends = ["line_ids", "line_ids.amount"])]
    async fn _compute_total_delta(
        this: &RecordSet<AccountBudgetAmendment>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountBudgetAmendment>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|r| r.as_real())
            .map(|r| r.get())
            .collect();
        let mut by_doc = std::collections::HashMap::<i64, Decimal>::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT amendment_id::int8, COALESCE(SUM(amount), 0) \
                     FROM account_budget_amendment_line \
                     WHERE amendment_id = ANY($1::int8[]) \
                     GROUP BY amendment_id",
                    &[&real_ids],
                )
                .await?;
            for row in rows {
                let id: i64 = row.try_get(0).map_err(db)?;
                let sum: Decimal = row.try_get(1).map_err(db)?;
                by_doc.insert(id, sum);
            }
        }
        for (i, rid) in rec_ids.iter().enumerate() {
            let sum = rid
                .as_real()
                .and_then(|r| by_doc.get(&r.get()).copied())
                .unwrap_or(Decimal::ZERO);
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
        tx.flush().await?;
        let id = doc.as_records().ids()[0];
        let rows = tx
            .query(
                "SELECT state::text FROM account_budget_amendment WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Budget amendment no longer exists."));
        };
        let state: String = row.try_get(0).map_err(db)?;
        if state != "draft" {
            return Err(Error::user("Only a draft amendment can be submitted."));
        }
        let uid = doc.env().uid();
        tx.execute(
            "UPDATE account_budget_amendment \
             SET state = 'submitted', write_uid = $2, write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1 AND state = 'draft'",
            &[&id, &uid],
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
        tx.flush().await?;
        let doc_id = doc.as_records().ids()[0];
        let doc_rows = tx
            .query(
                "SELECT a.state::text, a.amendment_type::text, a.budget_id::int8, \
                        a.target_budget_id::int8, a.company_id::int8, b.state::text, \
                        COALESCE(cur.decimal_places, 2)::int4 \
                 FROM account_budget_amendment a \
                 JOIN account_budget b ON b.id = a.budget_id \
                 JOIN res_currency cur ON cur.id = a.currency_id \
                 WHERE a.id = $1 FOR UPDATE OF a",
                &[&doc_id],
            )
            .await?;
        let Some(drow) = doc_rows.first() else {
            return Err(Error::missing("Budget amendment no longer exists."));
        };
        let state: String = drow.try_get(0).map_err(db)?;
        if !matches!(state.as_str(), "draft" | "submitted") {
            return Err(Error::user(
                "Only a draft or submitted budget amendment can be approved.",
            ));
        }
        let kind: String = drow
            .try_get::<_, Option<String>>(1)
            .map_err(db)?
            .ok_or_else(|| Error::validation("Amendment process type is required."))?;
        let budget_id: i64 = drow.try_get(2).map_err(db)?;
        let target_budget_id: Option<i64> = drow.try_get(3).map_err(db)?;
        let company_id: i64 = drow.try_get(4).map_err(db)?;
        let budget_state: String = drow.try_get(5).map_err(db)?;
        let scale_i32: i32 = drow.try_get(6).map_err(db)?;
        let scale = u32::try_from(scale_i32)
            .map_err(|_| Error::validation(format!("Invalid currency scale {scale_i32}.")))?;
        if budget_state != "validate" {
            return Err(Error::user(format!(
                "Budget amendments can only be applied to an approved (AVC active) budget (current budget status: '{budget_state}')."
            )));
        }

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
        let mut bl_ids_sorted = Vec::with_capacity(lines.len());
        for row in lines {
            let bl_id: i64 = row.try_get(1).map_err(db)?;
            let amt: Decimal = row.try_get(2).map_err(db)?;
            deltas.push(amt);
            parsed.push((bl_id, amt));
            bl_ids_sorted.push(bl_id);
        }

        if kind == "transfer" {
            validate_virement_lines_with_scale(&deltas, scale)?;
        }

        // Lock all target budget lines in canonical sorted ID order to prevent deadlocks.
        bl_ids_sorted.sort_unstable();
        bl_ids_sorted.dedup();
        tx.query(
            "SELECT id::int8 FROM account_budget_line \
             WHERE id = ANY($1::int8[]) ORDER BY id FOR UPDATE",
            &[&bl_ids_sorted],
        )
        .await?;

        let uid = doc.env().uid();
        let mut touched = Vec::new();
        for (bl_id, raw_amt) in parsed {
            let bl_rows = tx
                .query(
                    "SELECT original_amount, revision_amount, planned_amount, \
                            released_amount, committed_amount, practical_amount, \
                            name::text, account_id::int8, analytic_account_id::int8, \
                            asset_id::int8, budget_id::int8 \
                     FROM account_budget_line WHERE id = $1",
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
            let bl_budget_id: i64 = bl.try_get(10).map_err(db)?;
            if bl_budget_id != budget_id {
                return Err(Error::validation(format!(
                    "Budget line '{bl_name}' does not belong to the amended budget."
                )));
            }
            let available = released - committed - practical;

            let calc_release_pct = |next_released: Decimal, next_planned: Decimal| -> Decimal {
                if next_planned.is_zero() {
                    if next_released.is_zero() {
                        Decimal::ZERO
                    } else {
                        Decimal::from(100)
                    }
                } else {
                    (next_released / next_planned * Decimal::from(100)).round_dp(2)
                }
            };

            match kind.as_str() {
                "supplement" => {
                    if raw_amt <= Decimal::ZERO {
                        return Err(Error::user("Budget Supplement amounts must be positive."));
                    }
                    let next_rev = rev + raw_amt;
                    let next_planned = orig + next_rev;
                    let next_released = released + raw_amt;
                    let next_pct = calc_release_pct(next_released, next_planned);
                    tx.execute(
                        "UPDATE account_budget_line \
                         SET revision_amount = $2, planned_amount = $3, released_amount = $4, \
                             release_pct = $5, write_uid = $6, write_date = now() AT TIME ZONE 'UTC' \
                         WHERE id = $1",
                        &[&bl_id, &next_rev, &next_planned, &next_released, &next_pct, &uid],
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
                    let next_pct = calc_release_pct(next_released, next_planned);
                    tx.execute(
                        "UPDATE account_budget_line \
                         SET revision_amount = $2, planned_amount = $3, released_amount = $4, \
                             release_pct = $5, write_uid = $6, write_date = now() AT TIME ZONE 'UTC' \
                         WHERE id = $1",
                        &[&bl_id, &next_rev, &next_planned, &next_released, &next_pct, &uid],
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
                    let next_pct = calc_release_pct(next_released, next_planned);
                    tx.execute(
                        "UPDATE account_budget_line \
                         SET revision_amount = $2, planned_amount = $3, released_amount = $4, \
                             release_pct = $5, write_uid = $6, write_date = now() AT TIME ZONE 'UTC' \
                         WHERE id = $1",
                        &[&bl_id, &next_rev, &next_planned, &next_released, &next_pct, &uid],
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
                    let next_pct = calc_release_pct(next_released, planned);
                    tx.execute(
                        "UPDATE account_budget_line \
                         SET released_amount = $2, release_pct = $3, write_uid = $4, \
                             write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                        &[&bl_id, &next_released, &next_pct, &uid],
                    )
                    .await?;
                }
                "carryforward" => {
                    let tgt_bid = target_budget_id.ok_or_else(|| {
                        Error::validation(
                            "Select a Target Next-Year Budget for Year-End Carryforward.",
                        )
                    })?;
                    if tgt_bid == budget_id {
                        return Err(Error::validation(
                            "Target Next-Year Budget must be different from the source budget.",
                        ));
                    }
                    if raw_amt < Decimal::ZERO {
                        return Err(Error::validation("Carryforward amount cannot be negative."));
                    }
                    if raw_amt > available {
                        return Err(Error::user(format!(
                            "Cannot carry forward {raw_amt} from '{bl_name}': only {available} is currently available."
                        )));
                    }
                    let carry = if raw_amt > Decimal::ZERO {
                        raw_amt
                    } else if available > Decimal::ZERO {
                        available
                    } else if committed > Decimal::ZERO {
                        Decimal::ZERO
                    } else {
                        return Err(Error::user(format!(
                            "Budget line '{bl_name}' has no available budget or open commitments to carry forward."
                        )));
                    };
                    let tgt_rows = tx
                        .query(
                            "SELECT date_from, date_to, company_id::int8 \
                             FROM account_budget WHERE id = $1 FOR UPDATE",
                            &[&tgt_bid],
                        )
                        .await?;
                    let Some(td) = tgt_rows.first() else {
                        return Err(Error::missing("Target Next-Year Budget does not exist."));
                    };
                    let t_from: Date = td.try_get(0).map_err(db)?;
                    let t_to: Date = td.try_get(1).map_err(db)?;
                    let t_company_id: i64 = td.try_get(2).map_err(db)?;
                    if t_company_id != company_id {
                        return Err(Error::validation(
                            "Target Next-Year Budget must belong to the same company.",
                        ));
                    }

                    // Deduct BOTH carried residual AND re-pointed open commitments from old-year
                    // line so total budget across FY(N) + FY(N+1) is strictly conserved (no doubling!).
                    let total_moved = carry + committed;
                    if total_moved > Decimal::ZERO {
                        let next_rev = rev - total_moved;
                        let next_planned = orig + next_rev;
                        let next_released = released - total_moved;
                        let next_pct = calc_release_pct(next_released, next_planned);
                        tx.execute(
                            "UPDATE account_budget_line \
                             SET revision_amount = $2, planned_amount = $3, released_amount = $4, \
                                 release_pct = $5, write_uid = $6, write_date = now() AT TIME ZONE 'UTC' \
                             WHERE id = $1",
                            &[&bl_id, &next_rev, &next_planned, &next_released, &next_pct, &uid],
                        )
                        .await?;
                    }

                    // Create carryforward budget line in target fiscal year budget (via sudo so it
                    // works whether the target FY budget is draft or already validated).
                    let env = crate::moves::post::strip_default_context(doc.env()).sudo(true);
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
                    new_bl.insert("revision_amount".into(), Cell::Decimal(total_moved));
                    new_bl.insert("planned_amount".into(), Cell::Decimal(total_moved));
                    new_bl.insert("released_amount".into(), Cell::Decimal(total_moved));
                    let created_bl = env
                        .records("account.budget.line")?
                        .create(tx, vec![new_bl])
                        .await?;
                    let new_bl_id = created_bl.ids()[0];

                    // Re-point open PO commitments from prior-year line to carryforward line.
                    tx.execute(
                        "UPDATE account_budget_commitment \
                         SET budget_line_id = $2, write_uid = $3, write_date = now() AT TIME ZONE 'UTC' \
                         WHERE budget_line_id = $1 AND state IN ('open', 'partially_billed')",
                        &[&bl_id, &new_bl_id, &uid],
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

        touched.sort_unstable();
        touched.dedup();
        refresh_budget_lines(tx, &touched).await?;
        let updated = tx
            .execute(
                "UPDATE account_budget_amendment \
                 SET state = 'approved', write_uid = $2, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state IN ('draft', 'submitted')",
                &[&doc_id, &uid],
            )
            .await?;
        if updated != 1 {
            return Err(Error::conflict(
                "Budget amendment state changed concurrently; retry.",
            ));
        }
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
        tx.flush().await?;
        let id = doc.as_records().ids()[0];
        let rows = tx
            .query(
                "SELECT state::text FROM account_budget_amendment WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Budget amendment no longer exists."));
        };
        let state: String = row.try_get(0).map_err(db)?;
        if state == "approved" {
            return Err(Error::user(
                "Approved amendments cannot be cancelled; create a compensating Return or Transfer.",
            ));
        }
        let uid = doc.env().uid();
        tx.execute(
            "UPDATE account_budget_amendment \
             SET state = 'cancelled', write_uid = $2, write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[&id, &uid],
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
impl AccountBudgetAmendmentLine {
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountBudgetAmendmentLine>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if !this.env().su() {
            let doc_ids: Vec<i64> = vals_list
                .iter()
                .filter_map(|v| match v.get("amendment_id") {
                    Some(Cell::Id(id)) => Some(*id),
                    _ => None,
                })
                .collect();
            if !doc_ids.is_empty() {
                let locked: i64 = tx
                    .query(
                        "SELECT count(*)::int8 FROM account_budget_amendment \
                         WHERE id = ANY($1::int8[]) AND state <> 'draft'",
                        &[&doc_ids],
                    )
                    .await?[0]
                    .try_get(0)
                    .map_err(db)?;
                if locked > 0 {
                    return Err(Error::user(
                        "Cannot add lines to a submitted, approved, or cancelled budget amendment.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountBudgetAmendmentLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() && !this.env().su() {
            let ids = this.as_records().ids();
            let locked: i64 = tx
                .query(
                    "SELECT count(*)::int8 FROM account_budget_amendment_line l \
                     JOIN account_budget_amendment a ON a.id = l.amendment_id \
                     WHERE l.id = ANY($1::int8[]) AND a.state <> 'draft'",
                    &[&ids],
                )
                .await?[0]
                .try_get(0)
                .map_err(db)?;
            if locked > 0 {
                return Err(Error::user(
                    "Lines of a submitted, approved, or cancelled budget amendment cannot be modified.",
                ));
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountBudgetAmendmentLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() && !this.env().su() {
            let ids = this.as_records().ids();
            let locked: i64 = tx
                .query(
                    "SELECT count(*)::int8 FROM account_budget_amendment_line l \
                     JOIN account_budget_amendment a ON a.id = l.amendment_id \
                     WHERE l.id = ANY($1::int8[]) AND a.state <> 'draft'",
                    &[&ids],
                )
                .await?[0]
                .try_get(0)
                .map_err(db)?;
            if locked > 0 {
                return Err(Error::user(
                    "Lines of a submitted, approved, or cancelled budget amendment cannot be deleted.",
                ));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

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
