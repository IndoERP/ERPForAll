// parent without an edge: the call, shown at its name
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = parent()
)]
struct Doc;

fn main() {}
