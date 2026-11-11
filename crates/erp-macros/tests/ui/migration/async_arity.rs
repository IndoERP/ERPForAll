// an async migration with one argument: the arguments
#[erp_macros::migration(addon = "shop", version = "1.1", stage = post)]
async fn fill(_tx: u8) {}

fn main() {}
