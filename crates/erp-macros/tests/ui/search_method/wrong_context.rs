#[erp_macros::methods(Foo, addon = "test_search")]
impl Foo {
    #[search_method]
    fn search(req: &FieldSearchRequest<'_>, ctx: &FieldSearchContext<'_>) -> Result<Domain> {
        todo!()
    }
}

fn main() {}
