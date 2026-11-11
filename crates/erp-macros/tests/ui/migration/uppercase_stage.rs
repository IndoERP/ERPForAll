// stages are written in lower case only: the stage
#[erp_macros::migration(addon = "shop", version = "1.1", stage = "Post")]
async fn fill(_tx: u8, _from: u8) {}

fn main() {}
