// an empty name: the literal
#[erp_macros::migration(addon = "shop", version = "1.1", stage = post, name = " ")]
async fn fill(_tx: u8, _from: u8) {}

fn main() {}
