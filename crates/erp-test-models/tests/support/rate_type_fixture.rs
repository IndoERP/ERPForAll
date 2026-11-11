//! The six seeded rate types of `data/res_currency_rate_type_data.xml`, committed once into a test
//! database (harnesses do not load addon data files). Rates default to the `spot` type.

use erp_sdk::{Cell, RawVals};
use erp_testkit::Harness;

/// `(code, name, date_rule, later_fallback)` exactly as the data file seeds them.
pub const SEEDED: [(&str, &str, &str, bool); 6] = [
    ("spot", "Spot", "strictly_before", true),
    ("monthly_average", "Monthly Average", "on_or_before", false),
    ("closing", "Closing", "on_or_before", false),
    ("historical", "Historical", "strictly_before", false),
    ("budget", "Budget", "strictly_before", false),
    ("tax", "Tax", "on_or_before", false),
];

/// Creates the seeded types and commits them.
pub async fn seed_rate_types(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    let types = tx.env().records("res.currency.rate.type").unwrap();
    for (sequence, (code, name, rule, fallback)) in (10_i32..).step_by(10).zip(SEEDED) {
        let vals = RawVals::new()
            .set_cell("code", Cell::Str(code.into()))
            .set_cell("name", Cell::Str(name.into()))
            .set_cell("date_rule", Cell::Str(rule.into()))
            .set_cell("later_fallback", Cell::Bool(fallback))
            .set_cell("sequence", Cell::Int(sequence));
        types.create(&mut tx, vec![vals]).await.unwrap();
    }
    tx.commit().await.unwrap();
}
