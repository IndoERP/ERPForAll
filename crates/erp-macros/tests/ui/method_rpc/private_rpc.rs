// `rpc` on a name that starts with `_`: such a name is private
struct Thing;

#[erp_macros::methods(Thing)]
impl Thing {
    #[method(wire, rpc)]
    async fn _lend(this: &u8, tx: &mut u8) {}
}

fn main() {}
