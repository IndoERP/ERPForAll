// unrestricted without a reason: the call, shown at its name
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = unrestricted()
)]
struct Doc;

fn main() {}
