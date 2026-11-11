// a type parameter: the parameter
#[erp_macros::migration(addon = "shop", version = "1.1", stage = post)]
async fn fill<T>(_tx: T, _from: u8) {}

fn main() {}
