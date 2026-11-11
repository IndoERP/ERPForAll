#[erp_macros::methods(Foo, addon = "test_candidate")]
impl Foo {
    #[compute(override, candidate, depends = [])]
    async fn currency(
        this: &RecordSet<Foo>,
        tx: &mut ReadTx<'_>,
        out: &mut Assign<'_, Foo>,
        sup: AsyncComputeSuper,
    ) -> Result<()> {
        todo!()
    }
}

fn main() {}
