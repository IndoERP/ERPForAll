#[erp_macros::methods(Foo, addon = "test_candidate")]
impl Foo {
    #[compute(candidate, depends = [])]
    async fn currency(
        this: &RecordSet<Foo>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Foo>,
    ) -> Result<()> {
        todo!()
    }
}

fn main() {}
