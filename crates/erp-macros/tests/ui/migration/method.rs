// a method: the receiver
struct Shop;

impl Shop {
    #[erp_macros::migration(addon = "shop", version = "1.1", stage = post)]
    async fn fill(&self, _from: u8) {}
}

fn main() {}
