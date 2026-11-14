// SPDX-License-Identifier: MIT OR Apache-2.0
//! Plain company accounting configuration. Property defaults use the separate mapping
//! runtime; fiscal calendars, lock policies and taxes remain in their owning modules.
mod settings;
pub(crate) mod validation;

use erp_sdk::prelude::*;

#[extend(ResCompany, addon = "account", check_company_auto = true)]
struct AccountingCompany {
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Transfer account"
    )]
    pub transfer_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Cash surplus account"
    )]
    pub default_cash_difference_income_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Cash shortage account"
    )]
    pub default_cash_difference_expense_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Bank suspense account"
    )]
    pub account_journal_suspense_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Cash discount gain account"
    )]
    pub account_journal_early_pay_discount_gain_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Cash discount loss account"
    )]
    pub account_journal_early_pay_discount_loss_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Exchange gain account"
    )]
    pub income_currency_exchange_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Exchange loss account"
    )]
    pub expense_currency_exchange_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "PoS receivable account"
    )]
    pub account_default_pos_receivable_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Expense accrual account"
    )]
    pub expense_accrual_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Revenue accrual account"
    )]
    pub revenue_accrual_account_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.journal",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Exchange difference journal"
    )]
    pub currency_exchange_journal_id: Option<RecId>,
    /// Company-owned configuration; unset until its chart or an administrator supplies it.
    #[field(
        many2one,
        comodel = "account.journal",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        index = "btree_not_null",
        string = "Automatic entry journal"
    )]
    pub automatic_entry_default_journal_id: Option<RecId>,
    /// Optional code prefix; bounded by the account-code storage width, validated before writes.
    #[field(char, string = "Bank account prefix")]
    pub bank_account_code_prefix: Option<Arc<str>>,
    /// Optional code prefix; bounded by the account-code storage width, validated before writes.
    #[field(char, string = "Cash account prefix")]
    pub cash_account_code_prefix: Option<Arc<str>>,
    /// Optional code prefix; bounded by the account-code storage width, validated before writes.
    #[field(char, string = "Transfer account prefix")]
    pub transfer_account_code_prefix: Option<Arc<str>>,
    /// Selection stored for the tax calculation consumer; it does not execute that consumer.
    #[field(selection, selection = [("round_globally", "Round on totals"), ("round_per_line", "Round each line")], default = "round_globally", required = true, string = "Tax rounding")]
    pub tax_calculation_rounding_method: Option<Arc<str>>,
    /// Accounting convention selected for this company, independent of other companies.
    #[field(
        boolean,
        default = false,
        required = true,
        string = "Anglo-Saxon accounting"
    )]
    pub anglo_saxon_accounting: bool,
    /// Whether the company expects a chart; chart activation remains a separate operation.
    #[field(boolean, default = true, required = true, string = "Chart expected")]
    pub expects_chart_of_accounts: bool,
    /// Identifier supplied by the future chart activation operation, never by a generic write.
    #[field(
        char,
        size = 128,
        readonly = true,
        copy = false,
        string = "Chart identifier"
    )]
    pub chart_template: Option<Arc<str>>,
}
