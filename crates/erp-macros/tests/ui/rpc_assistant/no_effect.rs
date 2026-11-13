#[erp_macros::methods(Foo, addon = "test_assistant_ui")]
impl Foo {
    #[method]
    #[rpc(assistant = offer)]
    async fn send(this: &RecordSet<Foo>, tx: &mut Transaction) -> Result<bool> {
        Ok(true)
    }
}

fn main() {}
