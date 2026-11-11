// an unknown empty-company spelling: the empty literal
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = direct(field = "company_id", branch = "exact", empty = "x")
)]
struct Doc;

fn main() {}
