// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `analytic` for ERPForAll: analytic plans, hierarchical cost & profit centers,
//! validity & posting control indicators, statistical key figures, analytic items, and
//! analytic distribution models.

erp_sdk::addon!(name = "analytic", depends = ["base"], exact_only = true);

pub mod allocation;

use erp_sdk::migrations::SemverVersion;
use erp_sdk::prelude::*;

pub(crate) fn strip_default_context(env: &Env) -> Env {
    let mut ctx = env.context().clone();
    let keys: Vec<String> = ctx
        .default_field_names()
        .map(|k| format!("default_{k}"))
        .collect();
    for k in keys {
        ctx.remove(&k);
    }
    env.with_context_replaced(ctx)
}

pub(crate) fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Analytic query failed: {err}"))
}

/// Checks whether `date` falls inside an optional `[valid_from, valid_to]` inclusive window.
pub fn is_center_valid_on(valid_from: Option<Date>, valid_to: Option<Date>, date: Date) -> bool {
    if let Some(from) = valid_from
        && date.days() < from.days()
    {
        return false;
    }
    if let Some(to) = valid_to
        && date.days() > to.days()
    {
        return false;
    }
    true
}

/// Splits `total` (rounded to `scale` decimal places) proportionally across positive `(id, weight)`
/// receivers while conserving the exact total down to the last cent on the largest-weight receiver.
pub fn split_proportional_exact(
    total: Decimal,
    weights: &[(i64, Decimal)],
    scale: u32,
) -> Result<Vec<(i64, Decimal)>> {
    if weights.is_empty() {
        return Err(Error::validation(
            "At least one receiver with a positive allocation weight is required.",
        ));
    }
    let mut sum_weight = Decimal::ZERO;
    for (_, w) in weights {
        if *w < Decimal::ZERO {
            return Err(Error::validation("Allocation weights cannot be negative."));
        }
        sum_weight += *w;
    }
    if sum_weight <= Decimal::ZERO {
        return Err(Error::validation(
            "Total allocation weight across receivers must be greater than zero.",
        ));
    }
    let target = total.round_dp(scale);
    let mut out = Vec::with_capacity(weights.len());
    let mut allocated = Decimal::ZERO;
    let mut largest_idx = 0usize;
    let mut largest_weight = Decimal::MIN;

    for (idx, &(id, w)) in weights.iter().enumerate() {
        if w > largest_weight {
            largest_weight = w;
            largest_idx = idx;
        }
        let share = (target * w / sum_weight).round_dp(scale);
        allocated += share;
        out.push((id, share));
    }
    let remainder = target - allocated;
    if !remainder.is_zero() {
        out[largest_idx].1 += remainder;
    }
    Ok(out)
}

/// `account.analytic.plan`: grouping dimension for analytic accounts (e.g. Cost Centers, Profit Centers, Projects).
#[model(
    name = "account.analytic.plan",
    description = "Analytic Plan",
    order = "name, id"
)]
pub struct AccountAnalyticPlan {
    /// Plan name.
    #[field(char, required = true, translate = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Optional parent plan for hierarchical plans.
    #[field(
        many2one,
        comodel = "account.analytic.plan",
        ondelete = "restrict",
        string = "Parent"
    )]
    pub parent_id: Option<RecId>,
    /// Description of this analytic dimension.
    #[field(text, string = "Description")]
    pub description: Option<Arc<str>>,
    /// Default applicability across accounting documents.
    #[field(
        selection,
        selection = [
            ("optional", "Optional"),
            ("mandatory", "Mandatory"),
            ("unavailable", "Unavailable"),
        ],
        required = true,
        default = "optional",
        string = "Default Applicability"
    )]
    pub default_applicability: Option<Arc<str>>,
    /// Display color index.
    #[field(integer, default = 0, string = "Color")]
    pub color: i32,
}

#[methods(AccountAnalyticPlan, addon = "analytic")]
impl AccountAnalyticPlan {}

/// `account.analytic.account`: hierarchical cost center, profit center, internal order, or project
/// tracking controlling costs, revenues, validity windows, and posting locks.
#[model(
    name = "account.analytic.account",
    description = "Analytic Account",
    order = "code, name, id",
    parent_store = true,
    parent_name = "parent_id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny")
)]
pub struct AccountAnalyticAccount {
    /// Center name.
    #[field(char, required = true, index = true, string = "Analytic Account")]
    pub name: Option<Arc<str>>,
    /// Short reference code (e.g. CC-IT-01, PC-NORTH).
    #[field(char, index = true, string = "Reference")]
    pub code: Option<Arc<str>>,
    /// Analytic plan dimension.
    #[field(
        many2one,
        comodel = "account.analytic.plan",
        required = true,
        ondelete = "restrict",
        string = "Plan"
    )]
    pub plan_id: Option<RecId>,
    /// Controlling object classification.
    #[field(
        selection,
        selection = [
            ("cost_center", "Cost Center"),
            ("profit_center", "Profit Center"),
            ("internal_order", "Internal Order"),
            ("project", "Project"),
        ],
        required = true,
        default = "cost_center",
        index = true,
        string = "Center Type"
    )]
    pub center_type: Option<Arc<str>>,
    /// Functional cost center category.
    #[field(
        selection,
        selection = [
            ("production", "Production"),
            ("service", "Service / Shared"),
            ("sales", "Sales & Distribution"),
            ("administration", "Administration"),
            ("rd", "Research & Development"),
            ("capital", "Capital Project"),
        ],
        string = "Category"
    )]
    pub category: Option<Arc<str>>,
    /// Parent cost/profit center group in the standard hierarchy.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "restrict",
        index = true,
        string = "Parent Group"
    )]
    pub parent_id: Option<RecId>,
    /// Materialized ancestry path managed by the ORM parent store.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// Direct child cost/profit centers in the standard hierarchy.
    #[field(
        one2many,
        comodel = "account.analytic.account",
        inverse_name = "parent_id",
        string = "Sub-Centers"
    )]
    pub child_ids: Vec<RecId>,
    /// Profit center this cost center or internal order rolls up into.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "set null",
        string = "Profit Center"
    )]
    pub profit_center_id: Option<RecId>,
    /// Responsible manager / owner of this center.
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "set null",
        string = "Responsible Manager"
    )]
    pub manager_user_id: Option<RecId>,
    /// Optional customer/partner associated with this analytic account.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        ondelete = "set null",
        string = "Customer"
    )]
    pub partner_id: Option<RecId>,
    /// Owning company.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Company currency for analytic totals.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Start date of the controlling validity window (inclusive).
    #[field(date, string = "Valid From")]
    pub valid_from: Option<Date>,
    /// End date of the controlling validity window (inclusive).
    #[field(date, string = "Valid To")]
    pub valid_to: Option<Date>,
    /// Block direct primary cost postings to this center.
    #[field(boolean, default = false, string = "Lock Primary Costs")]
    pub lock_primary_costs: bool,
    /// Block direct revenue postings to this center.
    #[field(boolean, default = false, string = "Lock Direct Revenues")]
    pub lock_revenues: bool,
    /// Block period-end secondary allocation postings to this center.
    #[field(boolean, default = false, string = "Lock Secondary Allocations")]
    pub lock_secondary_costs: bool,
    /// Block new purchase order commitments against this center.
    #[field(boolean, default = false, string = "Lock Commitments")]
    pub lock_commitment: bool,
    /// Whether this analytic account is active.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Analytic items posted directly to this account.
    #[field(
        one2many,
        comodel = "account.analytic.line",
        inverse_name = "account_id",
        string = "Analytic Lines"
    )]
    pub line_ids: Vec<RecId>,
    /// Total negative analytic amounts (costs), shown positive.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_debit_credit_balance",
        string = "Debit"
    )]
    pub debit: Decimal,
    /// Total positive analytic amounts (revenues).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_debit_credit_balance",
        string = "Credit"
    )]
    pub credit: Decimal,
    /// Net analytic balance (`credit - debit`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_debit_credit_balance",
        string = "Balance"
    )]
    pub balance: Decimal,
    /// Rolled-up costs across this center and all descendants in the standard hierarchy.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_subtree_totals",
        string = "Hierarchy Debit"
    )]
    pub subtree_debit: Decimal,
    /// Rolled-up revenues across this center and all descendants in the standard hierarchy.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_subtree_totals",
        string = "Hierarchy Credit"
    )]
    pub subtree_credit: Decimal,
    /// Rolled-up net balance across this center and all descendants in the standard hierarchy.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_subtree_totals",
        string = "Hierarchy Balance"
    )]
    pub subtree_balance: Decimal,
}

#[methods(AccountAnalyticAccount, addon = "analytic")]
impl AccountAnalyticAccount {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountAnalyticAccount>,
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
        if names.iter().any(|n| n == "plan_id") && !values.contains_key("plan_id") {
            let rows = tx
                .query(
                    "SELECT id::int8 FROM account_analytic_plan ORDER BY id LIMIT 1",
                    &[],
                )
                .await?;
            if let Some(pid) = rows.first().and_then(|r| r.try_get::<_, i64>(0).ok()) {
                values.insert("plan_id".to_owned(), Value::Int(pid));
            }
        }
        Ok(values)
    }

    #[constrains("valid_from", "valid_to")]
    async fn _check_validity_dates(
        this: &RecordSet<AccountAnalyticAccount>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let froms = this.mapped(tx, AccountAnalyticAccount::VALID_FROM).await?;
        let tos = this.mapped(tx, AccountAnalyticAccount::VALID_TO).await?;
        for (f, t) in froms.into_iter().zip(tos) {
            if let (Some(from), Some(to)) = (f, t)
                && to.days() < from.days()
            {
                return Err(Error::validation(
                    "Cost center Valid To date cannot be earlier than Valid From date.",
                ));
            }
        }
        Ok(())
    }

    #[compute(
        depends = ["line_ids", "line_ids.amount"],
        depends_context = ["uid", "allowed_company_ids"]
    )]
    async fn _compute_debit_credit_balance(
        this: &RecordSet<AccountAnalyticAccount>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountAnalyticAccount>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|r| r.as_real())
            .map(|r| r.get())
            .collect();
        let mut totals = std::collections::HashMap::<i64, (Decimal, Decimal, Decimal)>::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            let rows = if this.env().su() {
                tx.query(
                    "SELECT account_id::int8, \
                            COALESCE(SUM(-amount) FILTER (WHERE amount < 0), 0), \
                            COALESCE(SUM(amount) FILTER (WHERE amount > 0), 0), \
                            COALESCE(SUM(amount), 0) \
                     FROM account_analytic_line \
                     WHERE account_id = ANY($1::int8[]) \
                     GROUP BY account_id",
                    &[&real_ids],
                )
                .await?
            } else {
                let allowed: Vec<i64> = tx
                    .companies(this.env())
                    .await?
                    .into_iter()
                    .map(|c| c.get())
                    .collect();
                if allowed.is_empty() {
                    Vec::new()
                } else {
                    tx.query(
                        "SELECT account_id::int8, \
                                COALESCE(SUM(-amount) FILTER (WHERE amount < 0), 0), \
                                COALESCE(SUM(amount) FILTER (WHERE amount > 0), 0), \
                                COALESCE(SUM(amount), 0) \
                         FROM account_analytic_line \
                         WHERE account_id = ANY($1::int8[]) \
                           AND company_id = ANY($2::int8[]) \
                         GROUP BY account_id",
                        &[&real_ids, &allowed],
                    )
                    .await?
                }
            };
            for row in rows {
                let aid: i64 = row.try_get(0).map_err(db)?;
                let debit: Decimal = row.try_get(1).map_err(db)?;
                let credit: Decimal = row.try_get(2).map_err(db)?;
                let balance: Decimal = row.try_get(3).map_err(db)?;
                totals.insert(aid, (debit, credit, balance));
            }
        }
        for (i, rid) in rec_ids.iter().enumerate() {
            let (d, c, b) = rid
                .as_real()
                .and_then(|r| totals.get(&r.get()).copied())
                .unwrap_or((Decimal::ZERO, Decimal::ZERO, Decimal::ZERO));
            out.set(i, AccountAnalyticAccount::DEBIT, d)?;
            out.set(i, AccountAnalyticAccount::CREDIT, c)?;
            out.set(i, AccountAnalyticAccount::BALANCE, b)?;
        }
        Ok(())
    }

    #[compute(
        depends = ["parent_path", "line_ids", "line_ids.amount", "child_ids"],
        depends_context = ["uid", "allowed_company_ids"]
    )]
    async fn _compute_subtree_totals(
        this: &RecordSet<AccountAnalyticAccount>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountAnalyticAccount>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|r| r.as_real())
            .map(|r| r.get())
            .collect();
        let mut totals = std::collections::HashMap::<i64, (Decimal, Decimal, Decimal)>::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            let rows = if this.env().su() {
                tx.query(
                    "SELECT p.id::int8, \
                            COALESCE(SUM(-l.amount) FILTER (WHERE l.amount < 0), 0), \
                            COALESCE(SUM(l.amount) FILTER (WHERE l.amount > 0), 0), \
                            COALESCE(SUM(l.amount), 0) \
                     FROM account_analytic_account p \
                     JOIN account_analytic_account d \
                       ON (d.id = p.id OR (p.parent_path IS NOT NULL AND d.parent_path IS NOT NULL \
                           AND starts_with(d.parent_path, p.parent_path))) \
                     LEFT JOIN account_analytic_line l ON l.account_id = d.id \
                     WHERE p.id = ANY($1::int8[]) \
                     GROUP BY p.id",
                    &[&real_ids],
                )
                .await?
            } else {
                let allowed: Vec<i64> = tx
                    .companies(this.env())
                    .await?
                    .into_iter()
                    .map(|c| c.get())
                    .collect();
                if allowed.is_empty() {
                    Vec::new()
                } else {
                    tx.query(
                        "SELECT p.id::int8, \
                                COALESCE(SUM(-l.amount) FILTER (WHERE l.amount < 0), 0), \
                                COALESCE(SUM(l.amount) FILTER (WHERE l.amount > 0), 0), \
                                COALESCE(SUM(l.amount), 0) \
                         FROM account_analytic_account p \
                         JOIN account_analytic_account d \
                           ON (d.id = p.id OR (p.parent_path IS NOT NULL AND d.parent_path IS NOT NULL \
                               AND starts_with(d.parent_path, p.parent_path))) \
                         LEFT JOIN account_analytic_line l \
                           ON l.account_id = d.id AND l.company_id = ANY($2::int8[]) \
                         WHERE p.id = ANY($1::int8[]) \
                         GROUP BY p.id",
                        &[&real_ids, &allowed],
                    )
                    .await?
                }
            };
            for row in rows {
                let aid: i64 = row.try_get(0).map_err(db)?;
                let debit: Decimal = row.try_get(1).map_err(db)?;
                let credit: Decimal = row.try_get(2).map_err(db)?;
                let balance: Decimal = row.try_get(3).map_err(db)?;
                totals.insert(aid, (debit, credit, balance));
            }
        }
        for (i, rid) in rec_ids.iter().enumerate() {
            let (d, c, b) = rid
                .as_real()
                .and_then(|r| totals.get(&r.get()).copied())
                .unwrap_or((Decimal::ZERO, Decimal::ZERO, Decimal::ZERO));
            out.set(i, AccountAnalyticAccount::SUBTREE_DEBIT, d)?;
            out.set(i, AccountAnalyticAccount::SUBTREE_CREDIT, c)?;
            out.set(i, AccountAnalyticAccount::SUBTREE_BALANCE, b)?;
        }
        Ok(())
    }
}

/// `account.analytic.line`: a single primary or secondary controlling cost/revenue item.
#[model(
    name = "account.analytic.line",
    description = "Analytic Line",
    order = "date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountAnalyticLine {
    /// Line description.
    #[field(char, required = true, string = "Description")]
    pub name: Option<Arc<str>>,
    /// Posting date.
    #[field(date, required = true, index = true, string = "Date")]
    pub date: Option<Date>,
    /// Target analytic account / cost center.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        required = true,
        check_company = true,
        ondelete = "restrict",
        index = true,
        string = "Analytic Account"
    )]
    pub account_id: Option<RecId>,
    /// Analytic plan derived from the account.
    #[field(
        many2one,
        comodel = "account.analytic.plan",
        related = "account_id.plan_id",
        store = true,
        string = "Plan"
    )]
    pub plan_id: Option<RecId>,
    /// Whether this item originated from a direct GL entry or a period-end allocation cycle.
    #[field(
        selection,
        selection = [
            ("primary", "Primary Posting"),
            ("secondary_distribution", "Secondary Distribution"),
            ("secondary_assessment", "Secondary Assessment"),
        ],
        required = true,
        default = "primary",
        index = true,
        string = "Cost Kind"
    )]
    pub cost_kind: Option<Arc<str>>,
    /// Counterpart sender or receiver cost center for period-end allocations.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "set null",
        string = "Partner Center"
    )]
    pub partner_center_id: Option<RecId>,
    /// Period-end cost allocation run that generated this secondary posting.
    #[field(
        many2one,
        comodel = "account.cost.allocation.run",
        ondelete = "cascade",
        index = "btree_not_null",
        string = "Allocation Run"
    )]
    pub allocation_run_id: Option<RecId>,
    /// Optional partner.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        ondelete = "set null",
        string = "Partner"
    )]
    pub partner_id: Option<RecId>,
    /// Company derived from the analytic account.
    #[field(
        many2one,
        comodel = "res.company",
        related = "account_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency derived from the company.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Signed analytic amount: positive = revenue/credit, negative = expense/debit.
    #[field(monetary, currency_field = "currency_id", string = "Amount")]
    pub amount: Decimal,
    /// Optional quantity (hours, units).
    #[field(float, digits = (16, 2), default = 0, string = "Quantity")]
    pub unit_amount: Decimal,
    /// Source document reference.
    #[field(char, string = "Reference")]
    pub r#ref: Option<Arc<str>>,
}

#[methods(AccountAnalyticLine, addon = "analytic")]
impl AccountAnalyticLine {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountAnalyticLine>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "date") && !values.contains_key("date") {
            let today = this.env().context_today()?;
            values.insert("date".to_owned(), Value::Date(today));
        }
        Ok(values)
    }
}

/// `account.analytic.distribution.model`: rule automatically suggesting an analytic account
/// based on partner or account code prefix.
#[model(
    name = "account.analytic.distribution.model",
    description = "Analytic Distribution Model",
    order = "sequence, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny")
)]
pub struct AccountAnalyticDistributionModel {
    /// Rule name.
    #[field(char, required = true, string = "Rule Name")]
    pub name: Option<Arc<str>>,
    /// Priority order.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// Active flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Owning company.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Optional partner filter.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        string = "Partner"
    )]
    pub partner_id: Option<RecId>,
    /// Optional account code prefix filter (e.g. "6" for expenses).
    #[field(char, string = "Accounts Prefix")]
    pub account_prefix: Option<Arc<str>>,
    /// Analytic account applied when matched.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        required = true,
        check_company = true,
        ondelete = "cascade",
        string = "Analytic Account"
    )]
    pub analytic_account_id: Option<RecId>,
    /// Allocation percentage (0..100).
    #[field(float, digits = (16, 2), default = 100, string = "Allocation %")]
    pub percentage: Decimal,
}

#[methods(AccountAnalyticDistributionModel, addon = "analytic")]
impl AccountAnalyticDistributionModel {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountAnalyticDistributionModel>,
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

    #[constrains("percentage")]
    async fn _check_percentage(
        this: &RecordSet<AccountAnalyticDistributionModel>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for pct in this
            .mapped(tx, AccountAnalyticDistributionModel::PERCENTAGE)
            .await?
        {
            if pct <= Decimal::ZERO || pct > Decimal::from(100) {
                return Err(Error::validation(
                    "Analytic distribution percentage must be greater than 0 and at most 100.",
                ));
            }
        }
        Ok(())
    }
}

/// `account.analytic.skf`: Statistical Key Figure definition (e.g. Headcount, Floor Area m2,
/// Machine Hours) used as a quantitative tracing driver for period-end cost center allocations.
#[model(
    name = "account.analytic.skf",
    description = "Statistical Key Figure",
    order = "code, name, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"),
    keys = [key(
        name = "_unique_company_code",
        fields = ["company_id", "code"],
        message = "Statistical Key Figure code must be unique per company."
    )]
)]
pub struct AccountAnalyticSkf {
    /// Display name (e.g. "Employee Headcount").
    #[field(char, required = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Short code (e.g. "HEADCOUNT", "SQM").
    #[field(char, required = true, index = true, string = "Code")]
    pub code: Option<Arc<str>>,
    /// Unit description (e.g. "Employees", "m2", "Servers").
    #[field(char, required = true, default = "Units", string = "Unit")]
    pub unit_label: Option<Arc<str>>,
    /// Fixed values carry forward until replaced; Total values sum within the allocation period.
    #[field(
        selection,
        selection = [
            ("fixed", "Fixed Value (Latest as of Period End)"),
            ("total", "Total Value (Sum in Period)"),
        ],
        required = true,
        default = "fixed",
        string = "SKF Category"
    )]
    pub skf_type: Option<Arc<str>>,
    /// Owning company.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Active flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Posted statistical measurements per cost center.
    #[field(
        one2many,
        comodel = "account.analytic.skf.line",
        inverse_name = "skf_id",
        string = "Measurements"
    )]
    pub line_ids: Vec<RecId>,
}

#[methods(AccountAnalyticSkf, addon = "analytic")]
impl AccountAnalyticSkf {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountAnalyticSkf>,
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

/// `account.analytic.skf.line`: quantitative statistical key figure measurement for a cost center.
#[model(
    name = "account.analytic.skf.line",
    description = "Statistical Key Figure Entry",
    order = "date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountAnalyticSkfLine {
    /// Parent SKF definition.
    #[field(
        many2one,
        comodel = "account.analytic.skf",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Statistical Key Figure"
    )]
    pub skf_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "skf_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Measured cost center.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        required = true,
        check_company = true,
        ondelete = "cascade",
        index = true,
        string = "Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// Effective date of the measurement.
    #[field(date, required = true, index = true, string = "Date")]
    pub date: Option<Date>,
    /// Positive quantitative measure (e.g. 25 employees, 450.5 m2).
    #[field(float, digits = (16, 4), required = true, default = 0, string = "Quantity")]
    pub quantity: Decimal,
    /// Optional note.
    #[field(char, string = "Note")]
    pub note: Option<Arc<str>>,
}

#[methods(AccountAnalyticSkfLine, addon = "analytic")]
impl AccountAnalyticSkfLine {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountAnalyticSkfLine>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "date") && !values.contains_key("date") {
            let today = this.env().context_today()?;
            values.insert("date".to_owned(), Value::Date(today));
        }
        Ok(values)
    }

    #[constrains("quantity")]
    async fn _check_quantity(
        this: &RecordSet<AccountAnalyticSkfLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for qty in this.mapped(tx, AccountAnalyticSkfLine::QUANTITY).await? {
            if qty < Decimal::ZERO {
                return Err(Error::validation(
                    "Statistical Key Figure quantity cannot be negative.",
                ));
            }
        }
        Ok(())
    }
}

/// Post-schema migration for `analytic` 1.2: initialize center_type, cost_kind, active flags,
/// and root parent_path values for pre-existing analytic accounts.
#[erp_sdk::migration(addon = "analytic", version = "1.2", stage = post)]
async fn backfill_analytic_controlling_fields(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    tx.execute(
        "UPDATE account_analytic_account \
         SET center_type = COALESCE(center_type, 'cost_center'), \
             lock_primary_costs = COALESCE(lock_primary_costs, false), \
             lock_revenues = COALESCE(lock_revenues, false), \
             lock_secondary_costs = COALESCE(lock_secondary_costs, false), \
             lock_commitment = COALESCE(lock_commitment, false), \
             parent_path = COALESCE(parent_path, id::text || '/') \
         WHERE center_type IS NULL \
            OR lock_primary_costs IS NULL \
            OR lock_revenues IS NULL \
            OR lock_secondary_costs IS NULL \
            OR lock_commitment IS NULL \
            OR parent_path IS NULL",
        &[],
    )
    .await?;
    tx.execute(
        "UPDATE account_analytic_line \
         SET cost_kind = 'primary' WHERE cost_kind IS NULL",
        &[],
    )
    .await?;
    tx.execute(
        "UPDATE account_analytic_distribution_model \
         SET active = true WHERE active IS NULL",
        &[],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_center_valid_on, split_proportional_exact};
    use erp_sdk::{Date, Decimal};
    use std::str::FromStr;

    #[test]
    fn proportional_split_conserves_exact_total_with_cent_remainder() {
        let total = Decimal::from_str("1000.00").unwrap();
        let weights = vec![
            (1, Decimal::from_str("1").unwrap()),
            (2, Decimal::from_str("1").unwrap()),
            (3, Decimal::from_str("1").unwrap()),
        ];
        let shares = split_proportional_exact(total, &weights, 2).unwrap();
        assert_eq!(shares.len(), 3);
        let sum: Decimal = shares.iter().map(|(_, a)| *a).sum();
        assert_eq!(sum, total);
        assert_eq!(shares[0].1, Decimal::from_str("333.34").unwrap());
        assert_eq!(shares[1].1, Decimal::from_str("333.33").unwrap());
        assert_eq!(shares[2].1, Decimal::from_str("333.33").unwrap());
    }

    #[test]
    fn center_validity_window_checks_inclusive_bounds() {
        let d_from = Date::from_ymd(2026, 1, 1).unwrap();
        let d_to = Date::from_ymd(2026, 12, 31).unwrap();
        assert!(is_center_valid_on(
            Some(d_from),
            Some(d_to),
            Date::from_ymd(2026, 1, 1).unwrap()
        ));
        assert!(is_center_valid_on(
            Some(d_from),
            Some(d_to),
            Date::from_ymd(2026, 12, 31).unwrap()
        ));
        assert!(!is_center_valid_on(
            Some(d_from),
            Some(d_to),
            Date::from_ymd(2025, 12, 31).unwrap()
        ));
        assert!(!is_center_valid_on(
            Some(d_from),
            Some(d_to),
            Date::from_ymd(2027, 1, 1).unwrap()
        ));
    }
}
