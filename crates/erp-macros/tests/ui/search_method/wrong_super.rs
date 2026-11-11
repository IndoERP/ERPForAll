#[erp_macros::methods(Foo, addon = "test_search")]
impl Foo {
    #[search_method(override)]
    fn search(
        req: &FieldSearchRequest<'_>,
        ctx: &mut FieldSearchContext<'_>,
        sup: usize,
    ) -> Result<Domain> {
        todo!()
    }
}

fn main() {}
