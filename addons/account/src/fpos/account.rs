// SPDX-License-Identifier: MIT OR Apache-2.0
use erp_sdk::prelude::*;
/// One explicit account substitution belonging to a fiscal position.
#[model(name = "account.fiscal.position.account", description = "Fiscal account mapping", company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"), company_match = "parent_of", constraints = [("_pair_uniq", "unique(position_id,account_src_id,account_dest_id)", "This fiscal position already contains this source and destination account pair.")])]
pub struct FiscalAccount {
    /// Mapping owner.
    #[field(
        many2one,
        comodel = "account.fiscal.position",
        required = true,
        ondelete = "cascade"
    )]
    pub position_id: Option<RecId>,
    /// Ownership follows the position, including direct mapping-row writes.
    #[field(
        many2one,
        comodel = "res.company",
        related = "position_id.company_id",
        store = true
    )]
    pub company_id: Option<RecId>,
    /// Source account checked against the owning company hierarchy.
    #[field(
        many2one,
        comodel = "account.account",
        required = true,
        check_company = true,
        ondelete = "restrict"
    )]
    pub account_src_id: Option<RecId>,
    /// Replacement account checked against the same hierarchy.
    #[field(
        many2one,
        comodel = "account.account",
        required = true,
        check_company = true,
        ondelete = "restrict"
    )]
    pub account_dest_id: Option<RecId>,
}
#[methods(FiscalAccount, addon = "account")]
impl FiscalAccount {}
