// an unknown form: the callee
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = owner(edge = "doc_id")
)]
struct Doc;

fn main() {}
