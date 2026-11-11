// no stage: the attribute
#[erp_macros::migration(addon = "shop", version = "1.1")]
async fn fill(_tx: u8, _from: u8) {}

fn main() {}
