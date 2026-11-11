//! Fixture addon `test_ext_b`: extends `test_ext.book` of the separately compiled `test_ext_a`
//! with fields and a `describe` override calling `super`.

use erp_addon_test_ext_a::Book;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_ext_b", depends = ["test_ext_a"]);

/// B's extension of `test_ext.book`.
#[extend(Book)]
pub struct BookPages {
    /// Number of pages.
    #[field(integer)]
    pub pages: i32,
    /// ISBN, with an explicit label.
    #[field(char, string = "ISBN")]
    pub isbn: Option<Arc<str>>,
}

#[methods(Book)]
impl BookPages {
    /// Appends the page count to the result of the next implementation.
    #[method(override)]
    async fn describe(this: &RecordSet<Book>, tx: &mut Transaction, sup: Super) -> Result<String> {
        let pages = this.get(tx, BookPages::PAGES).await?;
        Ok(format!(
            "{}, {pages} pages",
            sup.call(tx, this.as_records(), ()).await?
        ))
    }
}
