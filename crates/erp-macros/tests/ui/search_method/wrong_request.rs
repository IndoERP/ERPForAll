#[erp_macros::methods(Foo, addon = "test_search")]
impl Foo {
    #[search_method]
    fn search(req: &str, ctx: &mut FieldSearchContext<'_>) -> Result<Domain> {
        todo!()
    }
}

fn main() {}
