// the transient policy takes a plain decimal integer literal
#[erp_macros::model(name = "x.wiz", kind = "transient", transient_max_count = -5)]
struct Wiz {}

fn main() {}
