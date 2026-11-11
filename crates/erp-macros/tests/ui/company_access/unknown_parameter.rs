// an unknown parameter: its name
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = unrestricted(why = "x")
)]
struct Doc;

fn main() {}
