// an argument given twice: the second key
#[erp_macros::migration(addon = "shop", version = "1.1", stage = post, version = "1.2")]
async fn fill(_tx: u8, _from: u8) {}

fn main() {}
