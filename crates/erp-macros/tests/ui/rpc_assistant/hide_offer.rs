#[erp_macros::methods(Foo, addon = "test_assistant_ui")]
impl Foo {
    #[method(override)]
    #[rpc(hide, assistant = offer)]
    async fn send(this: &RecordSet<Foo>, tx: &mut Transaction, sup: Super) -> Result<bool> {
        Ok(true)
    }
}

fn main() {}
