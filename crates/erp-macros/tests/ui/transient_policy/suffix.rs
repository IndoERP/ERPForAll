// the transient policy takes a plain decimal integer literal
#[erp_macros::model(
    name = "x.wiz",
    kind = "transient",
    transient_max_age_seconds = 3600u64
)]
struct Wiz {}

fn main() {}
