// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stand-in for `account.move` while it does not exist: a document numbered by the
//! chain rows of `account`. It carries the two uniqueness rules.5 gives the
//! move table, so the tests can show that the database refuses a duplicate number.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_numbering", depends = ["account"]);

/// A numbered document of one journal.
#[model(
    name = "test_numbering.document",
    description = "Numbered Document",
    addon = "test_numbering",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        (
            "_name_unique",
            "UNIQUE (journal_id, name)",
            "Another document of this journal has the same number."
        ),
        (
            "_number_unique",
            "UNIQUE (chain_id, sequence_number)",
            "This number is already used in its chain."
        )
    ]
)]
pub struct Document {
    #[field(many2one, comodel = "res.company", required = true)]
    pub company_id: Option<RecId>,
    #[field(integer, required = true)]
    pub journal_id: i32,
    #[field(date, required = true)]
    pub date: Option<Date>,
    #[field(char)]
    pub reference: Option<Arc<str>>,
    /// Empty until numbered (5: a never-numbered draft has no name).
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "account.sequence.chain", ondelete = "restrict")]
    pub chain_id: Option<RecId>,
    #[field(integer)]
    pub sequence_number: i32,
}
