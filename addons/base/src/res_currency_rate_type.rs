//! `res.currency.rate.type`: the kind of an exchange-rate series (foundation L-3, L-4).
//!
//! A rate type names an independent dated series per currency and company (spot, tax, closing, ...)
//! and owns the date rule its lookup follows. The six seeded types are in
//! `data/res_currency_rate_type_data.xml`. The lookup that reads the rows is
//! [`crate::money::RateBook::load_typed`]; a type never falls back to another type.

use erp_sdk::prelude::*;

/// The code of the type an untyped caller or an untyped new rate means.
pub const SPOT_RATE_TYPE: &str = "spot";

/// `res.currency.rate.type`.
#[model(
    name = "res.currency.rate.type",
    description = "Currency Rate Type",
    order = "sequence, id",
    keys = [key(
        name = "_unique_code",
        fields = ["code"],
        message = "The rate type code must be unique!"
    )],
)]
pub struct ResCurrencyRateType {
    /// Stable identity of the type (`spot`, `tax`, ...); what addons and APIs name it by.
    #[field(char, required = true)]
    pub code: Option<Arc<str>>,
    /// Label shown to users.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    #[field(
        selection,
        selection = [
            ("strictly_before", "Strictly Before the Date"),
            ("on_or_before", "On or Before the Date")
        ],
        default = "strictly_before",
        required = true
    )]
    pub date_rule: Option<Arc<str>>,
    /// Only `spot` allows it; any other type reports a missing rate.
    #[field(boolean, default = false)]
    pub later_fallback: bool,
    /// Display order.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// Archived types are hidden from pickers; their rates stay and lookups still read them.
    #[field(boolean, default = true)]
    pub active: bool,
}
