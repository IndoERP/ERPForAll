// `rpc` marks a method an addon defines; an override is never exposed
struct Thing;

#[erp_macros::methods(Thing)]
impl Thing {
    #[method(override, wire, rpc)]
    async fn lend(this: &u8, tx: &mut u8, sup: u8) {}
}

fn main() {}
