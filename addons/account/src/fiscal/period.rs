// SPDX-License-Identifier: MIT OR Apache-2.0
//! `account.fiscal.period` (R2) and the company fields it is derived from.
//!
//! The reference has no period table: lock dates are company fields and every check compares a
//! date with them. Here each company has one row
//! per period with the statuses writers decide on (amendment G6); the company dates
//! are what setters and the lock wizard show.

use erp_addon_base::ResCompany;
use erp_sdk::prelude::*;

/// The period model.
pub const PERIOD_MODEL: &str = "account.fiscal.period";

/// One fiscal period of a company. Rows are created by [`super::ensure_periods`] and changed only
/// by the status operations of [`super::status`]; the record API refuses every create, write and
/// delete (the reference ships no such rows, so there is no default data).
#[model(
    name = "account.fiscal.period",
    description = "Fiscal Period",
    order = "company_id, date_from, kind, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        (
            "_period_key_unique",
            "UNIQUE (company_id, fiscal_year, kind, number)",
            "A company has one fiscal period per fiscal year, kind and number."
        ),
        (
            "_period_number",
            "CHECK ((kind = 'regular' AND number BETWEEN 1 AND 12) OR \
             (kind = 'adjustment' AND number BETWEEN 13 AND 16 AND date_from = date_to))",
            "Regular periods are numbered 1 to 12; adjustment periods 13 to 16 and last one day."
        ),
        (
            "_period_advisory_key",
            "CHECK (id <= 2147483647)",
            "The fiscal period id must fit an advisory lock key."
        ),
        (
            "_period_closed_at",
            "CHECK ((status = 'open') = (closed_at IS NULL))",
            "A fiscal period records when it was closed, and only while it is not open."
        ),
        (
            "_period_hard_locked_at",
            "CHECK ((status = 'hard_locked') = (hard_locked_at IS NOT NULL))",
            "A fiscal period records when it was hard-locked, and only once it is."
        ),
        (
            "_period_hard_implies_stock",
            "CHECK (status <> 'hard_locked' OR stock_lock_status = 'stock_locked')",
            "A hard-locked fiscal period is stock-locked too."
        ),
        (
            "_period_stock_starter",
            "CHECK ((stock_lock_status = 'stock_locking') = (stock_lock_started_at IS NOT NULL) \
             AND (stock_lock_started_by IS NULL) = (stock_lock_started_at IS NULL))",
            "Only a stock lock in progress records who started it and when."
        ),
        (
            "_period_hard_starter",
            "CHECK ((status = 'hard_locking') = (hard_lock_started_at IS NOT NULL) \
             AND (hard_lock_started_by IS NULL) = (hard_lock_started_at IS NULL))",
            "Only a hard lock in progress records who started it and when."
        ),
        (
            "_period_status_values",
            "CHECK (status IN ('open', 'closed', 'hard_locking', 'hard_locked'))",
            "A fiscal period is open, closed, being hard-locked or hard-locked."
        ),
        (
            "_period_stock_values",
            "CHECK (stock_lock_status IN ('open', 'stock_locking', 'stock_locked'))",
            "A fiscal period's stock status is open, being stock-locked or stock-locked."
        ),
        (
            "_period_generations",
            "CHECK (stock_lock_generation >= 0 AND hard_lock_generation >= 0)",
            "Lock run generations only count up from zero."
        ),
    ],
    exclusions = [exclude(
        name = "_period_no_overlap",
        equal = ["company_id"],
        period = ("date_from", "date_to"),
        bounds = "[]",
        filter = "[('kind', '=', 'regular')]",
        message = "The regular fiscal periods of a company cannot overlap."
    )],
    indexes = [index(name = "_company_date_from", fields = ["company_id", "date_from"])],
)]
pub struct AccountFiscalPeriod {
    /// `2026/03`, or `2025-26/01` for a fiscal year over two calendar years.
    #[field(char, required = true, readonly = true)]
    pub name: Option<Arc<str>>,
    /// The company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        readonly = true
    )]
    pub company_id: Option<RecId>,
    /// The year of the fiscal year's last day.
    #[field(integer, required = true, readonly = true)]
    pub fiscal_year: i32,
    /// Regular month, or a one-day adjustment period on the year's last day.
    #[field(
        selection,
        selection = [("regular", "Regular"), ("adjustment", "Adjustment")],
        required = true,
        default = "regular",
        readonly = true
    )]
    pub kind: Option<Arc<str>>,
    /// 1 to 12 for regular periods, 13 to 16 for adjustment periods.
    #[field(integer, required = true, readonly = true)]
    pub number: i32,
    /// First day.
    #[field(date, required = true, readonly = true, string = "Start Date")]
    pub date_from: Option<Date>,
    /// Last day, included.
    #[field(date, required = true, readonly = true, string = "End Date")]
    pub date_to: Option<Date>,
    /// The accounting status: closed by the fiscal-year lock, then hard-locked (two steps).
    #[field(
        selection,
        selection = [
            ("open", "Open"),
            ("closed", "Closed"),
            ("hard_locking", "Hard locking"),
            ("hard_locked", "Hard locked")
        ],
        required = true,
        default = "open",
        readonly = true
    )]
    pub status: Option<Arc<str>>,
    /// The stock status: stock-locked in two steps, or by the hard lock.
    #[field(
        selection,
        selection = [
            ("open", "Open"),
            ("stock_locking", "Stock locking"),
            ("stock_locked", "Stock locked")
        ],
        required = true,
        default = "open",
        readonly = true,
        string = "Stock Status"
    )]
    pub stock_lock_status: Option<Arc<str>>,
    /// Who started the stock lock in progress.
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "restrict",
        readonly = true
    )]
    pub stock_lock_started_by: Option<RecId>,
    /// When the stock lock in progress started.
    #[field(datetime, readonly = true)]
    pub stock_lock_started_at: Option<DateTime>,
    /// Who started the hard lock in progress.
    #[field(
        many2one,
        comodel = "res.users",
        ondelete = "restrict",
        readonly = true
    )]
    pub hard_lock_started_by: Option<RecId>,
    /// When the hard lock in progress started.
    #[field(datetime, readonly = true)]
    pub hard_lock_started_at: Option<DateTime>,
    /// Run counter of the stock lock: a later run supersedes an earlier one.
    #[field(integer, required = true, default = 0, readonly = true)]
    pub stock_lock_generation: i32,
    /// Run counter of the hard lock.
    #[field(integer, required = true, default = 0, readonly = true)]
    pub hard_lock_generation: i32,
    /// When the period left `open`.
    #[field(datetime, readonly = true)]
    pub closed_at: Option<DateTime>,
    /// When the period reached `hard_locked`.
    #[field(datetime, readonly = true)]
    pub hard_locked_at: Option<DateTime>,
}

const REFUSED: &str = "Fiscal periods are created by ensure_periods and change only through the \
                       lock operations (account fiscal period API); they cannot be created, edited \
                       or deleted directly.";

#[methods(AccountFiscalPeriod, addon = "account")]
impl AccountFiscalPeriod {
    /// Periods are created by `ensure_periods`, never through the record API.
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountFiscalPeriod>,
        _tx: &mut Transaction,
        _vals_list: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        let _ = this;
        Err(Error::validation(REFUSED))
    }

    /// The range, company and statuses of a period never change through the record API.
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountFiscalPeriod>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        if this.is_empty() {
            return Ok(());
        }
        Err(Error::validation(REFUSED))
    }

    /// A period is never deleted: entries and lock history refer to it.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountFiscalPeriod>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        if this.is_empty() {
            return Ok(());
        }
        Err(Error::validation(REFUSED))
    }
}

/// The company lock dates the period statuses are derived from. Their setters (for the
/// fiscal-year and hard lock dates, for the stock lock date) write them and call
/// [`super::status::sync_lock_dates`]; the hard and stock lock operations write them in their T2.
#[extend(ResCompany, addon = "account")]
struct AccountCompanyLockDates {
    /// Any entry up to and including that date is postponed to a later date.
    #[field(date, readonly = true, string = "Global Lock Date")]
    pub fiscalyear_lock_date: Option<Date>,
    /// Irreversible lock date, without exceptions.
    #[field(date, readonly = true, string = "Hard Lock Date")]
    pub hard_lock_date: Option<Date>,
    /// The end of the last stock-locked period; no reference counterpart.
    #[field(date, readonly = true, string = "Stock Lock Date")]
    pub stock_lock_date: Option<Date>,
    /// Bumped by every period creation and lock-date synchronisation of the company tree, so that
    /// the two serialize on the root company row (see [`super::ensure_periods`]).
    #[field(integer, readonly = true)]
    pub fiscal_period_serial: i32,
}
