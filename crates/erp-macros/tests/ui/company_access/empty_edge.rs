// an empty owner name: the edge literal
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny", parent = "")
)]
struct Doc;

fn main() {}
