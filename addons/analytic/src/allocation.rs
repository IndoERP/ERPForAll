// SPDX-License-Identifier: MIT OR Apache-2.0
//! Period-end Cost Center Allocation Cycles & Runs (`account.cost.allocation.cycle`,
//! `account.cost.allocation.rule`, `account.cost.allocation.receiver`,
//! `account.cost.allocation.run`) supporting Distribution (primary costs) and Assessment
//! (primary + secondary overhead) by Fixed %, Statistical Key Figures (SKF), or Variable Actuals.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::{db, is_center_valid_on, split_proportional_exact, strip_default_context};

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
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_unique_rule_receiver",
        fields = ["rule_id", "receiver_center_id"],
        message = "A receiver cost center can appear only once per allocation rule."
    )]
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
impl AccountCostAllocationReceiver {
    #[constrains("fixed_pct")]
    async fn _check_fixed_pct(
        this: &RecordSet<AccountCostAllocationReceiver>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for pct in this
            .mapped(tx, AccountCostAllocationReceiver::FIXED_PCT)
            .await?
        {
            if pct < Decimal::ZERO {
                return Err(Error::validation(
                    "Allocation receiver Fixed % cannot be negative.",
                ));
            }
        }
        Ok(())
    }
}

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
    #[field(date, required = true, string = "Period Start")]
    pub date_from: Option<Date>,
    /// End date of sender cost accumulation period (inclusive).
    #[field(date, required = true, string = "Period End")]
    pub date_to: Option<Date>,
    /// Posting date stamped onto generated secondary analytic lines.
    #[field(date, required = true, string = "Posting Date")]
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
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountCostAllocationRun>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let today = this.env().context_today()?;
        if names.iter().any(|n| n == "date_from") && !values.contains_key("date_from") {
            let (y, m, _) = today.ymd();
            let first = Date::from_ymd(y, m, 1).ok_or_else(|| {
                Error::invariant(format!("Could not build first day of {y}-{m:02}"))
            })?;
            values.insert("date_from".to_owned(), Value::Date(first));
        }
        if names.iter().any(|n| n == "date_to") && !values.contains_key("date_to") {
            values.insert("date_to".to_owned(), Value::Date(today));
        }
        if names.iter().any(|n| n == "posting_date") && !values.contains_key("posting_date") {
            values.insert("posting_date".to_owned(), Value::Date(today));
        }
        Ok(values)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountCostAllocationRun>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() {
            if vals.contains("state") || vals.contains("total_allocated") {
                return Err(Error::user(
                    "Allocation run status and total cannot be modified directly; use Execute or Reverse.",
                ));
            }
            for state in this.mapped(tx, AccountCostAllocationRun::STATE).await? {
                if state.as_deref() != Some("draft") {
                    return Err(Error::user(
                        "Posted or reversed allocation runs cannot be modified.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountCostAllocationRun>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<bool> {
        if !this.as_records().is_empty() {
            for state in this.mapped(tx, AccountCostAllocationRun::STATE).await? {
                if state.as_deref() == Some("posted") {
                    return Err(Error::user(
                        "Posted allocation runs cannot be deleted; reverse the run first.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Executes the cycle's allocation rules in sequence order, posting balanced secondary
    /// sender-credit and receiver-debit analytic lines with exact currency-unit conservation.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_execute_allocation(
        this: &RecordSet<AccountCostAllocationRun>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let run = this.ensure_one()?;
        run.as_records().check_access(tx, Operation::Write).await?;
        let run_id = run.as_records().ids()[0];
        tx.flush().await?;

        let cycle_id = run
            .get(tx, AccountCostAllocationRun::CYCLE_ID)
            .await?
            .ok_or_else(|| Error::validation("Select an allocation cycle."))?
            .get();

        // Lock cycle first, then run, to serialize concurrent executions on the same cycle.
        let cyc_rows = tx
            .query(
                "SELECT allocation_method::text, tracing_basis::text, skf_id::int8, company_id::int8 \
                 FROM account_cost_allocation_cycle WHERE id = $1 FOR UPDATE",
                &[&cycle_id],
            )
            .await?;
        let Some(cyc) = cyc_rows.first() else {
            return Err(Error::missing("Allocation cycle no longer exists."));
        };
        let method: String = cyc.try_get(0).map_err(db)?;
        let basis: String = cyc.try_get(1).map_err(db)?;
        let skf_id: Option<i64> = cyc.try_get(2).map_err(db)?;
        let run_company_id: i64 = cyc.try_get(3).map_err(db)?;

        let run_rows = tx
            .query(
                "SELECT state::text, cycle_id::int8, date_from, date_to, posting_date, name::text \
                 FROM account_cost_allocation_run WHERE id = $1 FOR UPDATE",
                &[&run_id],
            )
            .await?;
        let Some(rrow) = run_rows.first() else {
            return Err(Error::missing("Allocation run no longer exists."));
        };
        let state: String = rrow.try_get(0).map_err(db)?;
        if state != "draft" {
            return Err(Error::user("Only a draft allocation run can be executed."));
        }
        let locked_cycle_id: i64 = rrow.try_get(1).map_err(db)?;
        if locked_cycle_id != cycle_id {
            return Err(Error::conflict(
                "The allocation run's cycle changed while executing; retry.",
            ));
        }
        let date_from: Date = rrow
            .try_get::<_, Option<Date>>(2)
            .map_err(db)?
            .ok_or_else(|| Error::validation("Period Start is required."))?;
        let date_to: Date = rrow
            .try_get::<_, Option<Date>>(3)
            .map_err(db)?
            .ok_or_else(|| Error::validation("Period End is required."))?;
        if date_to.days() < date_from.days() {
            return Err(Error::validation(
                "Period End cannot be earlier than Period Start.",
            ));
        }
        let posting_date: Date = rrow
            .try_get::<_, Option<Date>>(4)
            .map_err(db)?
            .ok_or_else(|| Error::validation("Posting Date is required."))?;
        let run_name: Arc<str> = rrow
            .try_get::<_, Option<String>>(5)
            .map_err(db)?
            .filter(|s| !s.trim().is_empty())
            .map(Arc::from)
            .ok_or_else(|| Error::validation("Allocation Run Name is required."))?;

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

        let cost_kind = match method.as_str() {
            "distribution" => "secondary_distribution",
            "assessment" => "secondary_assessment",
            other => {
                return Err(Error::validation(format!(
                    "Unknown allocation method '{other}'."
                )));
            }
        };

        let scale_rows = tx
            .query(
                "SELECT cur.decimal_places::int4 \
                 FROM res_company c \
                 JOIN res_currency cur ON cur.id = c.currency_id \
                 WHERE c.id = $1",
                &[&run_company_id],
            )
            .await?;
        let scale_i32: i32 = scale_rows
            .first()
            .ok_or_else(|| Error::missing("Company currency not found for allocation run."))?
            .try_get(0)
            .map_err(db)?;
        let scale = u32::try_from(scale_i32).map_err(|_| {
            Error::validation(format!(
                "Invalid currency decimal_places ({scale_i32}) for allocation run."
            ))
        })?;

        let hoisted_skf = if basis == "skf" {
            let sid = skf_id.ok_or_else(|| {
                Error::validation("Select a Statistical Key Figure on the allocation cycle.")
            })?;
            let skf_rows = tx
                .query(
                    "SELECT skf_type::text, company_id::int8 FROM account_analytic_skf WHERE id = $1",
                    &[&sid],
                )
                .await?;
            let Some(skf_row) = skf_rows.first() else {
                return Err(Error::missing(format!(
                    "Statistical Key Figure {sid} does not exist."
                )));
            };
            let skf_type: String = skf_row
                .try_get::<_, Option<String>>(0)
                .map_err(db)?
                .ok_or_else(|| {
                    Error::validation(format!("Statistical Key Figure {sid} has no category."))
                })?;
            let skf_company_id: i64 = skf_row.try_get(1).map_err(db)?;
            if skf_company_id != run_company_id {
                return Err(Error::validation(
                    "Statistical Key Figure must belong to the same company as the allocation cycle.",
                ));
            }
            if !matches!(skf_type.as_str(), "fixed" | "total") {
                return Err(Error::validation(format!(
                    "Unknown Statistical Key Figure category '{skf_type}'."
                )));
            }
            Some((sid, skf_type))
        } else {
            None
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

        let clean_env = strip_default_context(run.env());
        let mut grand_total = Decimal::ZERO;

        for rule_row in rules {
            let rule_id: i64 = rule_row.try_get(0).map_err(db)?;
            let rule_name: String = rule_row.try_get(1).map_err(db)?;
            let sender_id: i64 = rule_row.try_get(2).map_err(db)?;
            let sender_share_pct: Decimal = rule_row.try_get(3).map_err(db)?;

            check_secondary_allowed(tx, sender_id, posting_date).await?;

            // In assessment mode, include secondary lines posted earlier in this same step-down run
            // even when `posting_date` lies outside `[date_from, date_to]`.
            let sender_rows = if method == "distribution" {
                tx.query(
                    "SELECT COALESCE(SUM(-amount), 0) FROM account_analytic_line \
                     WHERE account_id = $1 AND company_id = $4 \
                       AND date >= $2 AND date <= $3 \
                       AND COALESCE(cost_kind, 'primary') = 'primary'",
                    &[&sender_id, &date_from, &date_to, &run_company_id],
                )
                .await?
            } else {
                tx.query(
                    "SELECT COALESCE(SUM(-amount), 0) FROM account_analytic_line \
                     WHERE account_id = $1 AND company_id = $4 \
                       AND ((date >= $2 AND date <= $3) OR allocation_run_id = $5)",
                    &[&sender_id, &date_from, &date_to, &run_company_id, &run_id],
                )
                .await?
            };
            let sender_pool: Decimal = sender_rows
                .first()
                .ok_or_else(|| Error::invariant("Sender cost pool aggregate returned no row."))?
                .try_get(0)
                .map_err(db)?;

            let allocable = (sender_pool * sender_share_pct / Decimal::from(100)).round_dp(scale);
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
                        let (sid, skf_type) = hoisted_skf
                            .as_ref()
                            .ok_or_else(|| Error::invariant("SKF metadata was not loaded."))?;
                        read_skf_weight(
                            tx,
                            *sid,
                            skf_type,
                            recv_id,
                            run_company_id,
                            date_from,
                            date_to,
                        )
                        .await?
                    }
                    "variable_actual" => {
                        let arows = tx
                            .query(
                                "SELECT COALESCE(SUM(-amount), 0) FROM account_analytic_line \
                                 WHERE account_id = $1 AND company_id = $4 \
                                   AND date >= $2 AND date <= $3 \
                                   AND COALESCE(cost_kind, 'primary') = 'primary'",
                                &[&recv_id, &date_from, &date_to, &run_company_id],
                            )
                            .await?;
                        let val: Decimal = arows
                            .first()
                            .ok_or_else(|| {
                                Error::invariant("Variable actual aggregate returned no row.")
                            })?
                            .try_get(0)
                            .map_err(db)?;
                        val.max(Decimal::ZERO)
                    }
                    other => {
                        return Err(Error::validation(format!(
                            "Unknown tracing basis '{other}'."
                        )));
                    }
                };
                weights.push((recv_id, w));
            }

            let shares = split_proportional_exact(allocable, &weights, scale)?;
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

        let uid = run.env().uid();
        let updated = tx
            .execute(
                "UPDATE account_cost_allocation_run \
                 SET state = 'posted', total_allocated = $2, write_uid = $3, \
                     write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state = 'draft'",
                &[&run_id, &grand_total, &uid],
            )
            .await?;
        if updated != 1 {
            return Err(Error::conflict(
                "Allocation run state changed concurrently; retry.",
            ));
        }
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
        tx.flush().await?;

        let rows = tx
            .query(
                "SELECT state::text FROM account_cost_allocation_run WHERE id = $1 FOR UPDATE",
                &[&run_id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Allocation run no longer exists."));
        };
        let state: String = row.try_get(0).map_err(db)?;
        if state != "posted" {
            return Err(Error::user("Only a posted allocation run can be reversed."));
        }
        tx.execute(
            "DELETE FROM account_analytic_line WHERE allocation_run_id = $1",
            &[&run_id],
        )
        .await?;
        let uid = run.env().uid();
        let updated = tx
            .execute(
                "UPDATE account_cost_allocation_run \
                 SET state = 'reversed', write_uid = $2, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state = 'posted'",
                &[&run_id, &uid],
            )
            .await?;
        if updated != 1 {
            return Err(Error::conflict(
                "Allocation run state changed concurrently; retry.",
            ));
        }
        tx.invalidate_records(run.as_records(), Some(&["state", "line_ids"]))
            .await?;
        tx.invalidate_model("account.analytic.line", None).await?;
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
    skf_type: &str,
    center_id: i64,
    company_id: i64,
    date_from: Date,
    date_to: Date,
) -> Result<Decimal> {
    if skf_type == "total" {
        let rows = tx
            .query(
                "SELECT COALESCE(SUM(quantity), 0) FROM account_analytic_skf_line \
                 WHERE skf_id = $1 AND analytic_account_id = $2 AND company_id = $5 \
                   AND date >= $3 AND date <= $4",
                &[&skf_id, &center_id, &date_from, &date_to, &company_id],
            )
            .await?;
        let val: Decimal = rows
            .first()
            .ok_or_else(|| Error::invariant("SKF total aggregate returned no row."))?
            .try_get(0)
            .map_err(db)?;
        Ok(val)
    } else {
        let rows = tx
            .query(
                "SELECT quantity FROM account_analytic_skf_line \
                 WHERE skf_id = $1 AND analytic_account_id = $2 AND company_id = $4 \
                   AND date <= $3 \
                 ORDER BY date DESC, id DESC LIMIT 1",
                &[&skf_id, &center_id, &date_to, &company_id],
            )
            .await?;
        match rows.first() {
            Some(row) => {
                let qty: Option<Decimal> = row.try_get(0).map_err(db)?;
                Ok(qty.unwrap_or(Decimal::ZERO))
            }
            None => Ok(Decimal::ZERO),
        }
    }
}
