#[erp_macros::methods(Foo, addon = "test_search")]
impl Foo {
    #[search_method]
    fn search(req: &FieldSearchRequest<'_>, ctx: &mut FieldSearchContext<'_>) -> Result<bool> {
        todo!()
    }
}

fn main() {}
