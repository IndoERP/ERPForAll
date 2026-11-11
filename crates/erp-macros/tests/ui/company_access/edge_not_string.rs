// an owner name that is not a string literal: the value
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = parent(edge = 3)
)]
struct Doc;

fn main() {}
