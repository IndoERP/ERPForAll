// a parameter given twice: the second one, shown at its name
#[erp_macros::model(
    name = "shop.doc",
    addon = "shop",
    company_access = parent(edge = "a", edge = "b")
)]
struct Doc;

fn main() {}
