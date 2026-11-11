// a blank reason: the reason literal
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = unrestricted(reason = "   ")
)]
struct Doc;

fn main() {}
