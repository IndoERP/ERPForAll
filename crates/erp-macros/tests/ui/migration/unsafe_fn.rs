// an unsafe function: `unsafe`
#[erp_macros::migration(addon = "shop", version = "1.1", stage = post)]
unsafe fn fill(_tx: u8, _from: u8) {}

fn main() {}
