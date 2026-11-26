// SPDX-License-Identifier: MIT OR Apache-2.0
//! Plain company accounting configuration. Property defaults use the separate mapping
//! runtime; fiscal calendars, lock policies and taxes remain in their owning modules.
pub mod chart_activation;
pub mod settings;
pub(crate) mod validation;
pub(crate) mod valuation;

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
    /// Identifier supplied by chart activation.
    #[field(
        char,
        size = 128,
        readonly = true,
        copy = false,
        string = "Active Chart Template"
    )]
    pub chart_template: Option<Arc<str>>,
    /// Template chosen for chart activation.
    #[field(
        selection,
        selection = [
            ("generic_coa", "Generic International Chart of Accounts"),
            ("id_sak", "Indonesian SAK Chart of Accounts")
        ],
        default = "generic_coa",
        string = "Chart of Accounts Package"
    )]
    pub chart_template_selection: Option<Arc<str>>,
    /// Enable storno accounting (negative debits/credits on reversals).
    #[field(boolean, default = false, string = "Storno Accounting")]
    pub account_storno: bool,
    /// Allow resetting un-reconciled, unlocked posted journal entries back to draft.
    #[field(boolean, default = false, string = "Allow Resetting Posted Entries")]
    pub allow_posted_reset: bool,
    /// Enable cash basis tax accounting.
    #[field(boolean, default = false, string = "Cash Basis Taxes")]
    pub tax_exigibility: bool,
    /// Journal used for cash basis tax entries.
    #[field(
        many2one,
        comodel = "account.journal",
        ondelete = "restrict",
        check_company = true,
        copy = false,
        string = "Cash Basis Journal"
    )]
    pub tax_cash_basis_journal_id: Option<RecId>,
    /// Enable cash rounding on customer invoices and vendor bills.
    #[field(boolean, default = false, string = "Cash Rounding")]
    pub account_cash_rounding_enabled: bool,
    /// Warn when a customer's receivable exceeds their credit limit.
    #[field(boolean, default = false, string = "Sales Credit Limit")]
    pub account_use_credit_limit: bool,
    /// Default credit limit for new customers.
    #[field(
        float,
        digits = "Product Price",
        default = 0,
        string = "Default Credit Limit"
    )]
    pub account_default_credit_limit: Decimal,
    /// Print total invoice amount spelled out in words.
    #[field(boolean, default = false, string = "Total Amount in Words")]
    pub display_invoice_amount_total_words: bool,
    /// Display payment QR code on customer invoices.
    #[field(boolean, default = false, string = "Payment QR Code")]
    pub qr_code: bool,
    /// Quick encoding mode for invoices and vendor bills.
    #[field(
        selection,
        selection = [
            ("none", "Disabled"),
            ("out_invoices", "Customer Invoices"),
            ("in_invoices", "Vendor Bills"),
            ("out_and_in_invoices", "Customer Invoices and Vendor Bills")
        ],
        default = "none",
        required = true,
        string = "Quick Encoding"
    )]
    pub quick_edit_mode: Option<Arc<str>>,
    /// Automatically post vendor bills when confirmed from trusted sources.
    #[field(boolean, default = false, string = "Auto-post Bills")]
    pub autopost_bills: bool,
    /// Default commercial Incoterm for invoices and shipments.
    #[field(many2one, comodel = "account.incoterms", string = "Default Incoterm")]
    pub incoterm_id: Option<RecId>,
    /// Default terms and conditions printed on invoices.
    #[field(text, string = "Default Terms & Conditions")]
    pub invoice_terms: Option<Arc<str>>,
}
