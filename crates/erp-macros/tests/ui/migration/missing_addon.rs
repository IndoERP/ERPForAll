// no addon: the attribute
#[erp_macros::migration(version = "1.1", stage = post)]
async fn fill(_tx: u8, _from: u8) {}

fn main() {}
