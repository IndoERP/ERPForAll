// SPDX-License-Identifier: MIT OR Apache-2.0
use erp_addon_base::ResCountryGroup;
use erp_sdk::prelude::*;
#[extend(ResCountryGroup, addon = "account")]
struct FiscalCountryGroup {
    /// Member states excluded from this group's fiscal-position matches.
    #[field(many2many, comodel = "res.country.state")]
    pub exclude_state_ids: Vec<RecId>,
}
#[methods(ResCountryGroup, addon = "account")]
impl FiscalCountryGroup {}
