// SPDX-License-Identifier: MIT OR Apache-2.0
//! Accounting mail bridge: gives `account.move` (invoices, bills, credit notes and journal
//! entries) the message thread and activities, the chatter on its two forms, and tracks the
//! header fields the reference tracks (the
//! fields declared with `tracking=True`), as far as our model has them.
//!
//! A bridge rather than a `mail` dependency of `account`: the accounting addon stays installable
//! without messaging, the same choice as `product_mail` (
//! section 2). The fields are re-declared here with only `tracking`; every other attribute stays
//! the accounting addon's.

erp_sdk::addon!(name = "account_mail", depends = ["base", "account", "mail"]);

use erp_addon_account::moves::AccountMove;
use erp_addon_mail::{MailActivityMixin, MailThread};
use erp_sdk::Operation;
use erp_sdk::prelude::*;

/// `account.move` with the thread and activity mixins and its tracked header fields.
#[extend(AccountMove, addon = "account_mail", inherit = [MailThread, MailActivityMixin])]
pub struct AccountMoveThread {
    /// Number.
    #[field(char, tracking = true)]
    pub name: Option<Arc<str>>,
    /// Reference.
    #[field(char, tracking = true)]
    pub r#ref: Option<Arc<str>>,
    /// Accounting date.
    #[field(date, tracking = true)]
    pub date: Option<Date>,
    /// Status.
    #[field(selection, tracking = true)]
    pub state: Option<Arc<str>>,
    /// Type of the entry.
    #[field(selection, tracking = true)]
    pub move_type: Option<Arc<str>>,
    /// Partner.
    #[field(many2one, comodel = "res.partner", tracking = true)]
    pub partner_id: Option<RecId>,
    /// Currency.
    #[field(many2one, comodel = "res.currency", tracking = true)]
    pub currency_id: Option<RecId>,
    /// Untaxed amount.
    #[field(monetary, tracking = true)]
    pub amount_untaxed: Decimal,
    /// Payment status.
    #[field(selection, tracking = true)]
    pub payment_state: Option<Arc<str>>,
}

#[methods(AccountMove, addon = "account_mail")]
impl AccountMoveThread {
    /// Posting on an entry needs read access, as in the reference: a
    /// reader of a posted entry can still write a note on it.
    #[method(override, sealed)]
    async fn _mail_post_access(
        _this: &RecordSet<AccountMove>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }
}

mod send;
