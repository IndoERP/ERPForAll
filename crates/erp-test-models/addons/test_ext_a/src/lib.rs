//! Fixture addon `test_ext_a`: defines `test_ext.book` and its `describe` method. Separate crates
//! `test_ext_b` and `test_ext_c` extend it without this crate knowing them.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_ext_a");

/// `test_ext.book`.
#[model(name = "test_ext.book", description = "Book")]
pub struct Book {
    /// Title.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
}

#[methods(Book)]
impl Book {
    /// `describe()`: the title; extensions append to it through `super`.
    #[method]
    async fn describe(this: &RecordSet<Book>, tx: &mut Transaction) -> Result<String> {
        let name = this.get(tx, Book::NAME).await?;
        Ok(format!("book {}", name.as_deref().unwrap_or("?")))
    }
}
