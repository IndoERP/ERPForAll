// a branch spelling that does not exist: the branch literal
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = direct(field = "company_id", branch = "parent_of", empty = "deny")
)]
struct Doc;

fn main() {}
