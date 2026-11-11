// a stage that does not exist: the stage
#[erp_macros::migration(addon = "shop", version = "1.1", stage = middle)]
async fn fill(_tx: u8, _from: u8) {}

fn main() {}
