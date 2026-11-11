#[erp_macros::methods(Foo, addon = "test_candidate")]
impl Foo {
    #[compute(candidate, depends = [])]
    fn currency(batch: &mut Batch<'_, Foo>) -> Result<()> {
        todo!()
    }
}

fn main() {}
