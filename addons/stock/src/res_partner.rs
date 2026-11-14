// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company-local stock endpoints of a contact. Provisioning transit overrides is WH-STEPS.
use erp_addon_base::ResPartner;
use erp_sdk::prelude::*;

#[extend(ResPartner, addon = "stock", check_company_auto = true)]
pub(crate) struct StockPartner {
    /// Destination used when shipping goods to this contact in the active company.
    #[field(
        many2one,
        comodel = "stock.location",
        company_dependent = true,
        check_company = true,
        string = "Customer location"
    )]
    pub property_stock_customer: Option<RecId>,
    /// Source used when receiving goods from this contact in the active company.
    #[field(
        many2one,
        comodel = "stock.location",
        company_dependent = true,
        check_company = true,
        string = "Vendor location"
    )]
    pub property_stock_supplier: Option<RecId>,
}
