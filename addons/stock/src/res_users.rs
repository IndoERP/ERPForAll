// SPDX-License-Identifier: MIT OR Apache-2.0
//! An inventory user's company-local warehouse preference.
use erp_sdk::prelude::*;

#[extend(ResUsers, addon = "stock", check_company_auto = true)]
struct StockUser {
    /// Preferred warehouse in the active company. Base's self-service field mask limits
    /// ordinary writes to the user's own record; this preference grants no stock access.
    #[field(
        many2one,
        comodel = "stock.warehouse",
        company_dependent = true,
        check_company = true,
        user_writeable,
        string = "Preferred warehouse"
    )]
    pub property_warehouse_id: Option<RecId>,
}
