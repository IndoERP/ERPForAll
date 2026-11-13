//! Fixture addon for defaults and company-dependent tests (`ir.default`, `decimal.precision`).

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_defaults");

/// `test_defaults.partner`
#[model(
    name = "test_defaults.partner",
    description = "Test Defaults Partner",
    addon = "test_defaults"
)]
pub struct DefaultsPartner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub ref_code: Option<Arc<str>>,
    #[field(char, change_default = true)]
    pub zip: Option<Arc<str>>,
    #[field(char)]
    pub city: Option<Arc<str>>,
    #[field(integer)]
    pub color: i32,
}

/// `test_defaults.item`
#[model(
    name = "test_defaults.item",
    description = "Test Defaults Item",
    addon = "test_defaults"
)]
pub struct DefaultsItem {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_defaults.partner",
        company_dependent = true,
        ondelete = "restrict"
    )]
    pub pref_partner: Option<RecId>,
    #[field(many2one, comodel = "test_defaults.partner", company_dependent = true)]
    pub alt_partner: Option<RecId>,
    #[field(char, company_dependent = true)]
    pub note: Option<Arc<str>>,
    #[field(float, digits = "Product Price")]
    pub price: Decimal,
    #[field(selection, selection = [("opt_a", "Option A"), ("opt_b", "Option B")])]
    pub state: Option<Arc<str>>,
}
