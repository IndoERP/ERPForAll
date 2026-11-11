// an argument the macro does not know: its key
#[erp_macros::migration(addon = "shop", version = "1.1", stage = post, order = "1")]
async fn fill(_tx: u8, _from: u8) {}

fn main() {}
