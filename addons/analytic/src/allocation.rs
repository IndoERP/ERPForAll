// SPDX-License-Identifier: MIT OR Apache-2.0
//! Period-end Cost Center Allocation Cycles & Runs (`account.cost.allocation.cycle`,
//! `account.cost.allocation.rule`, `account.cost.allocation.receiver`,
//! `account.cost.allocation.run`) supporting Distribution (primary costs) and Assessment
//! (primary + secondary overhead) by Fixed %, Statistical Key Figures (SKF), or Variable Actuals.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::{db, default_today, is_center_valid_on, split_proportional_exact};

fn default_month_start(_ctx: &erp_sdk::DefaultContext) -> Value {
    let (y, m, _) = Date::today().ymd();
    Value::Date(Date::from_ymd(y, m, 1).unwrap_or_else(Date::today))
}

/// `account.cost.allocation.cycle`: reusable period-end cost center allocation definition
/// supporting Distribution (primary costs) and Assessment (primary + secondary overhead).
#[model(
    name = "account.cost.allocation.cycle",
    description = "Cost Allocation Cycle",
    order = "code, name, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_unique_company_code",
        fields = ["company_id", "code"],
        message = "Allocation cycle code must be unique per company."
    )]
)]
pub struct AccountCostAllocationCycle {
    /// Cycle name (e.g. "Month-End IT & Facility Assessment").
    #[field(char, required = true, string = "Cycle Name")]
    pub name: Option<Arc<str>>,
    /// Short cycle code (e.g. "ALLOC-IT").
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
    /// Distribution transfers primary cost balances; Assessment allocates total primary + secondary costs.
    #[field(
        selection,
        selection = [
            ("distribution", "Distribution (Primary Costs)"),
            ("assessment", "Assessment (Secondary Overhead)"),
        ],
        required = true,
        default = "assessment",
        string = "Allocation Method"
    )]
    pub allocation_method: Option<Arc<str>>,
    /// Driver used to compute receiver allocation shares.
    #[field(
        selection,
        selection = [
            ("fixed_pct", "Fixed Percentages"),
            ("skf", "Statistical Key Figure (SKF)"),
            ("variable_actual", "Variable Actual Costs"),
        ],
        required = true,
        default = "fixed_pct",
        string = "Tracing Basis"
    )]
    pub tracing_basis: Option<Arc<str>>,
    /// Default Statistical Key Figure when `tracing_basis == 'skf'`.
    #[field(
        many2one,
        comodel = "account.analytic.skf",
        check_company = true,
        ondelete = "restrict",
        string = "Statistical Key Figure"
    )]
    pub skf_id: Option<RecId>,
    /// Active flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Ordered sender-to-receiver allocation segments.
    #[field(
        one2many,
        comodel = "account.cost.allocation.rule",
        inverse_name = "cycle_id",
        string = "Allocation Rules"
    )]
    pub rule_ids: Vec<RecId>,
    /// Execution runs of this cycle.
    #[field(
        one2many,
        comodel = "account.cost.allocation.run",
        inverse_name = "cycle_id",
        string = "Execution Runs"
    )]
    pub run_ids: Vec<RecId>,
}

#[methods(AccountCostAllocationCycle, addon = "analytic")]
impl AccountCostAllocationCycle {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountCostAllocationCycle>,
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
}

/// `account.cost.allocation.rule`: one sender segment inside a cost allocation cycle.
#[model(
    name = "account.cost.allocation.rule",
    description = "Cost Allocation Rule",
    order = "sequence, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountCostAllocationRule {
    /// Parent cycle.
    #[field(
        many2one,
        comodel = "account.cost.allocation.cycle",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Allocation Cycle"
    )]
    pub cycle_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "cycle_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Execution order (allows step-down allocation across service centers).
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// Segment label (e.g. "IT Support to Operating Units").
    #[field(char, required = true, string = "Segment Name")]
    pub name: Option<Arc<str>>,
    /// Sender cost center whose period costs are allocated out.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Sender Cost Center"
    )]
    pub sender_center_id: Option<RecId>,
    /// Percentage of the sender's period cost pool to allocate (1..100%).
    #[field(
        float,
        digits = (16, 2),
        required = true,
        default = 100,
        string = "Sender Share %"
    )]
    pub sender_share_pct: Decimal,
    /// Receiver cost centers and optional fixed percentages.
    #[field(
        one2many,
        comodel = "account.cost.allocation.receiver",
        inverse_name = "rule_id",
        string = "Receivers"
    )]
    pub receiver_ids: Vec<RecId>,
}

#[methods(AccountCostAllocationRule, addon = "analytic")]
impl AccountCostAllocationRule {
    #[constrains("sender_share_pct")]
    async fn _check_sender_share(
        this: &RecordSet<AccountCostAllocationRule>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for pct in this
            .mapped(tx, AccountCostAllocationRule::SENDER_SHARE_PCT)
            .await?
        {
            if pct <= Decimal::ZERO || pct > Decimal::from(100) {
                return Err(Error::validation(
                    "Sender Share % must be greater than 0 and at most 100.",
                ));
            }
        }
        Ok(())
    }
}

/// `account.cost.allocation.receiver`: receiver cost center within an allocation rule.
#[model(
    name = "account.cost.allocation.receiver",
    description = "Cost Allocation Receiver",
    order = "id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountCostAllocationReceiver {
    /// Parent rule.
    #[field(
        many2one,
        comodel = "account.cost.allocation.rule",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Allocation Rule"
    )]
    pub rule_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "rule_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Receiver cost center.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Receiver Cost Center"
    )]
    pub receiver_center_id: Option<RecId>,
    /// Fixed percentage share (used when cycle tracing_basis == 'fixed_pct').
    #[field(float, digits = (16, 4), default = 0, string = "Fixed %")]
    pub fixed_pct: Decimal,
}

#[methods(AccountCostAllocationReceiver, addon = "analytic")]
impl AccountCostAllocationReceiver {}

/// `account.cost.allocation.run`: auditable period-end execution of a cost allocation cycle.
#[model(
    name = "account.cost.allocation.run",
    description = "Cost Allocation Run",
    order = "posting_date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountCostAllocationRun {
    /// Run reference label.
    #[field(char, required = true, string = "Run Name")]
    pub name: Option<Arc<str>>,
    /// Allocation cycle being executed.
    #[field(
        many2one,
        comodel = "account.cost.allocation.cycle",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Allocation Cycle"
    )]
    pub cycle_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "cycle_id.company_id",
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
    /// Start date of sender cost accumulation period (inclusive).
    #[field(date, required = true, default_fn = default_month_start, string = "Period Start")]
    pub date_from: Option<Date>,
    /// End date of sender cost accumulation period (inclusive).
    #[field(date, required = true, default_fn = default_today, string = "Period End")]
    pub date_to: Option<Date>,
    /// Posting date stamped onto generated secondary analytic lines.
    #[field(date, required = true, default_fn = default_today, string = "Posting Date")]
    pub posting_date: Option<Date>,
    /// Execution status.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("posted", "Posted"),
            ("reversed", "Reversed"),
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Total cost allocated across all rules in this run.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Total Allocated"
    )]
    pub total_allocated: Decimal,
    /// Secondary analytic postings created by this run.
    #[field(
        one2many,
        comodel = "account.analytic.line",
        inverse_name = "allocation_run_id",
        readonly = true,
        string = "Allocation Postings"
    )]
    pub line_ids: Vec<RecId>,
}

#[methods(AccountCostAllocationRun, addon = "analytic")]
impl AccountCostAllocationRun {
    /// Executes the cycle's allocation rules in sequence order, posting balanced secondary
    /// sender-credit and receiver-debit analytic lines with exact cent conservation.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_execute_allocation(
        this: &RecordSet<AccountCostAllocationRun>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let run = this.ensure_one()?;
        run.as_records().check_access(tx, Operation::Write).await?;
        let run_id = run.as_records().ids()[0];
        let state = run.get(tx, AccountCostAllocationRun::STATE).await?;
        if state.as_deref() != Some("draft") {
            return Err(Error::user("Only a draft allocation run can be executed."));
        }
        let cycle_id = run
            .get(tx, AccountCostAllocationRun::CYCLE_ID)
            .await?
            .ok_or_else(|| Error::validation("Select an allocation cycle."))?
            .get();
        let date_from = run
            .get(tx, AccountCostAllocationRun::DATE_FROM)
            .await?
            .ok_or_else(|| Error::validation("Period Start is required."))?;
        let date_to = run
            .get(tx, AccountCostAllocationRun::DATE_TO)
            .await?
            .ok_or_else(|| Error::validation("Period End is required."))?;
        if date_to.days() < date_from.days() {
            return Err(Error::validation(
                "Period End cannot be earlier than Period Start.",
            ));
        }
        let posting_date = run
            .get(tx, AccountCostAllocationRun::POSTING_DATE)
            .await?
            .unwrap_or(date_to);
        let run_name = run
            .get(tx, AccountCostAllocationRun::NAME)
            .await?
            .unwrap_or_else(|| Arc::from("Allocation Run"));

        // Idempotency guard: prevent double allocation for the same cycle & overlapping dates.
        let dup_rows = tx
            .query(
                "SELECT id::int8, name::text FROM account_cost_allocation_run \
                 WHERE cycle_id = $1 AND id <> $2 AND state = 'posted' \
                   AND date_from <= $4 AND date_to >= $3 \
                 LIMIT 1",
                &[&cycle_id, &run_id, &date_from, &date_to],
            )
            .await?;
        if let Some(dup) = dup_rows.first() {
            let dup_name: String = dup.try_get(1).map_err(db)?;
            return Err(Error::user(format!(
                "An active posted allocation run ('{dup_name}') already covers this cycle and period. Reverse it before re-running."
            )));
        }

        let cyc_rows = tx
            .query(
                "SELECT allocation_method::text, tracing_basis::text, skf_id::int8 \
                 FROM account_cost_allocation_cycle WHERE id = $1",
                &[&cycle_id],
            )
            .await?;
        let Some(cyc) = cyc_rows.first() else {
            return Err(Error::missing("Allocation cycle no longer exists."));
        };
        let method: String = cyc.try_get(0).map_err(db)?;
        let basis: String = cyc.try_get(1).map_err(db)?;
        let skf_id: Option<i64> = cyc.try_get(2).map_err(db)?;
        let cost_kind = if method == "distribution" {
            "secondary_distribution"
        } else {
            "secondary_assessment"
        };

        let rules = tx
            .query(
                "SELECT id::int8, name::text, sender_center_id::int8, COALESCE(sender_share_pct, 100) \
                 FROM account_cost_allocation_rule \
                 WHERE cycle_id = $1 ORDER BY sequence, id",
                &[&cycle_id],
            )
            .await?;
        if rules.is_empty() {
            return Err(Error::user(
                "The allocation cycle has no sender rules configured.",
            ));
        }

        let clean_env = run.env().with_context_replaced(erp_sdk::Context::default());
        let mut grand_total = Decimal::ZERO;

        for rule_row in rules {
            let rule_id: i64 = rule_row.try_get(0).map_err(db)?;
            let rule_name: String = rule_row.try_get(1).map_err(db)?;
            let sender_id: i64 = rule_row.try_get(2).map_err(db)?;
            let sender_share_pct: Decimal = rule_row.try_get(3).map_err(db)?;

            check_secondary_allowed(tx, sender_id, posting_date).await?;

            let sender_cost_sql = if method == "distribution" {
                "SELECT COALESCE(SUM(-amount), 0) FROM account_analytic_line \
                 WHERE account_id = $1 AND date >= $2 AND date <= $3 \
                   AND COALESCE(cost_kind, 'primary') = 'primary'"
            } else {
                "SELECT COALESCE(SUM(-amount), 0) FROM account_analytic_line \
                 WHERE account_id = $1 AND date >= $2 AND date <= $3"
            };
            let sender_pool: Decimal = tx
                .query(sender_cost_sql, &[&sender_id, &date_from, &date_to])
                .await?
                .first()
                .and_then(|r| r.try_get(0).ok())
                .unwrap_or(Decimal::ZERO);

            let allocable = (sender_pool * sender_share_pct / Decimal::from(100)).round_dp(2);
            if allocable <= Decimal::ZERO {
                continue;
            }

            let receivers = tx
                .query(
                    "SELECT receiver_center_id::int8, COALESCE(fixed_pct, 0) \
                     FROM account_cost_allocation_receiver \
                     WHERE rule_id = $1 ORDER BY id",
                    &[&rule_id],
                )
                .await?;
            if receivers.is_empty() {
                return Err(Error::user(format!(
                    "Allocation rule '{rule_name}' has no receiver cost centers."
                )));
            }

            let mut weights = Vec::with_capacity(receivers.len());
            for rec in &receivers {
                let recv_id: i64 = rec.try_get(0).map_err(db)?;
                let fixed_pct: Decimal = rec.try_get(1).map_err(db)?;
                if recv_id == sender_id {
                    return Err(Error::validation(format!(
                        "Allocation rule '{rule_name}' cannot allocate a cost center to itself."
                    )));
                }
                check_secondary_allowed(tx, recv_id, posting_date).await?;
                let w = match basis.as_str() {
                    "fixed_pct" => fixed_pct,
                    "skf" => {
                        let sid = skf_id.ok_or_else(|| {
                            Error::validation(
                                "Select a Statistical Key Figure on the allocation cycle.",
                            )
                        })?;
                        read_skf_weight(tx, sid, recv_id, date_from, date_to).await?
                    }
                    "variable_actual" => {
                        let arows = tx
                            .query(
                                "SELECT COALESCE(SUM(-amount), 0) FROM account_analytic_line \
                                 WHERE account_id = $1 AND date >= $2 AND date <= $3 \
                                   AND COALESCE(cost_kind, 'primary') = 'primary'",
                                &[&recv_id, &date_from, &date_to],
                            )
                            .await?;
                        arows
                            .first()
                            .and_then(|r| r.try_get::<_, Decimal>(0).ok())
                            .unwrap_or(Decimal::ZERO)
                            .max(Decimal::ZERO)
                    }
                    other => {
                        return Err(Error::validation(format!(
                            "Unknown tracing basis '{other}'."
                        )));
                    }
                };
                weights.push((recv_id, w));
            }

            let shares = split_proportional_exact(allocable, &weights, 2)?;
            let mut vals_list = Vec::with_capacity(shares.len() * 2);
            for (recv_id, share) in shares {
                if share.is_zero() {
                    continue;
                }
                let mut credit = RawVals::new();
                credit.insert("account_id".into(), Cell::Id(sender_id));
                credit.insert("partner_center_id".into(), Cell::Id(recv_id));
                credit.insert("date".into(), Cell::Date(posting_date));
                credit.insert(
                    "name".into(),
                    Cell::Str(format!("Allocation Out: {rule_name}").into()),
                );
                credit.insert("ref".into(), Cell::Str(run_name.clone()));
                credit.insert("amount".into(), Cell::Decimal(share));
                credit.insert("cost_kind".into(), Cell::Str(cost_kind.into()));
                credit.insert("allocation_run_id".into(), Cell::Id(run_id));
                vals_list.push(credit);

                let mut debit = RawVals::new();
                debit.insert("account_id".into(), Cell::Id(recv_id));
                debit.insert("partner_center_id".into(), Cell::Id(sender_id));
                debit.insert("date".into(), Cell::Date(posting_date));
                debit.insert(
                    "name".into(),
                    Cell::Str(format!("Allocation In: {rule_name}").into()),
                );
                debit.insert("ref".into(), Cell::Str(run_name.clone()));
                debit.insert("amount".into(), Cell::Decimal(-share));
                debit.insert("cost_kind".into(), Cell::Str(cost_kind.into()));
                debit.insert("allocation_run_id".into(), Cell::Id(run_id));
                vals_list.push(debit);

                grand_total += share;
            }
            if !vals_list.is_empty() {
                clean_env
                    .records("account.analytic.line")?
                    .create(tx, vals_list)
                    .await?;
            }
        }

        if grand_total.is_zero() {
            return Err(Error::user(
                "No positive sender costs were found in the selected period to allocate.",
            ));
        }

        tx.execute(
            "UPDATE account_cost_allocation_run \
             SET state = 'posted', total_allocated = $2, write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[&run_id, &grand_total],
        )
        .await?;
        tx.invalidate_records(
            run.as_records(),
            Some(&["state", "total_allocated", "line_ids"]),
        )
        .await?;
        tx.invalidate_model(
            "account.analytic.account",
            Some(&[
                "debit",
                "credit",
                "balance",
                "subtree_debit",
                "subtree_credit",
                "subtree_balance",
            ]),
        )
        .await
    }

    /// Reverses a posted allocation run by removing its secondary analytic items atomically.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_reverse_allocation(
        this: &RecordSet<AccountCostAllocationRun>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let run = this.ensure_one()?;
        run.as_records().check_access(tx, Operation::Write).await?;
        let run_id = run.as_records().ids()[0];
        let state = run.get(tx, AccountCostAllocationRun::STATE).await?;
        if state.as_deref() != Some("posted") {
            return Err(Error::user("Only a posted allocation run can be reversed."));
        }
        tx.execute(
            "DELETE FROM account_analytic_line WHERE allocation_run_id = $1",
            &[&run_id],
        )
        .await?;
        tx.execute(
            "UPDATE account_cost_allocation_run \
             SET state = 'reversed', write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1",
            &[&run_id],
        )
        .await?;
        tx.invalidate_records(run.as_records(), Some(&["state", "line_ids"]))
            .await?;
        tx.invalidate_model(
            "account.analytic.account",
            Some(&[
                "debit",
                "credit",
                "balance",
                "subtree_debit",
                "subtree_credit",
                "subtree_balance",
            ]),
        )
        .await
    }
}

async fn check_secondary_allowed(
    tx: &mut Transaction,
    center_id: i64,
    posting_date: Date,
) -> Result<()> {
    let rows = tx
        .query(
            "SELECT name::text, code::text, valid_from, valid_to, \
                    COALESCE(lock_secondary_costs, false), COALESCE(active, true) \
             FROM account_analytic_account WHERE id = $1",
            &[&center_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(Error::missing(format!(
            "Cost center {center_id} does not exist."
        )));
    };
    let name: String = row.try_get(0).map_err(db)?;
    let code: Option<String> = row.try_get(1).map_err(db)?;
    let valid_from: Option<Date> = row.try_get(2).map_err(db)?;
    let valid_to: Option<Date> = row.try_get(3).map_err(db)?;
    let lock_sec: bool = row.try_get(4).map_err(db)?;
    let active: bool = row.try_get(5).map_err(db)?;
    let label = match code.as_deref() {
        Some(c) if !c.is_empty() => format!("[{c}] {name}"),
        _ => name,
    };
    if !active {
        return Err(Error::user(format!(
            "Cost Center {label} is archived and cannot participate in allocations."
        )));
    }
    if lock_sec {
        return Err(Error::user(format!(
            "Cost Center {label} is locked for secondary allocations."
        )));
    }
    if !is_center_valid_on(valid_from, valid_to, posting_date) {
        return Err(Error::user(format!(
            "Cost Center {label} is outside its validity window on {posting_date}."
        )));
    }
    Ok(())
}

async fn read_skf_weight(
    tx: &mut Transaction,
    skf_id: i64,
    center_id: i64,
    date_from: Date,
    date_to: Date,
) -> Result<Decimal> {
    let type_rows = tx
        .query(
            "SELECT COALESCE(skf_type, 'fixed')::text FROM account_analytic_skf WHERE id = $1",
            &[&skf_id],
        )
        .await?;
    let skf_type: String = type_rows
        .first()
        .and_then(|r| r.try_get(0).ok())
        .unwrap_or_else(|| "fixed".to_owned());
    if skf_type == "total" {
        let rows = tx
            .query(
                "SELECT COALESCE(SUM(quantity), 0) FROM account_analytic_skf_line \
                 WHERE skf_id = $1 AND analytic_account_id = $2 \
                   AND date >= $3 AND date <= $4",
                &[&skf_id, &center_id, &date_from, &date_to],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| r.try_get(0).ok())
            .unwrap_or(Decimal::ZERO))
    } else {
        let rows = tx
            .query(
                "SELECT COALESCE(quantity, 0) FROM account_analytic_skf_line \
                 WHERE skf_id = $1 AND analytic_account_id = $2 AND date <= $3 \
                 ORDER BY date DESC, id DESC LIMIT 1",
                &[&skf_id, &center_id, &date_to],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| r.try_get(0).ok())
            .unwrap_or(Decimal::ZERO))
    }
}
