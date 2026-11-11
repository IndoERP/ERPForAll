#[erp_macros::methods(Foo, addon = "test_search")]
impl Foo {
    #[search_method]
    #[compute(depends = [])]
    fn search(req: &FieldSearchRequest<'_>, ctx: &mut FieldSearchContext<'_>) -> Result<Domain> {
        todo!()
    }
}

fn main() {}
