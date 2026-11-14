// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company-owned tax configuration; cash-basis and fiscal-position fields belong to their slices.
use erp_sdk::prelude::*;

#[extend(ResCompany, addon = "account")]
struct TaxCompany {
    /// Fiscal jurisdiction. Tax defaults use the company's country when this is empty.
    #[field(many2one, comodel = "res.country", string = "Fiscal country")]
    pub account_fiscal_country_id: Option<RecId>,
    /// Whether tax is included when a tax definition has no explicit override.
    #[field(selection, selection = [("tax_excluded", "Tax excluded"), ("tax_included", "Tax included")], default = "tax_excluded", required = true, string = "Default tax price inclusion")]
    pub account_price_include: Option<Arc<str>>,
}
