// missing parameters: the call, shown at its name (stable rustc cannot join spans)
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = direct(field = "company_id")
)]
struct Doc;

fn main() {}
