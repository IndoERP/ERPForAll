//! Model `test_addon.book`.

use erp_sdk::prelude::*;

/// `test_addon.book`.
#[model(name = "test_addon.book", description = "Book")]
pub struct Book {
    /// Book title.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Author name.
    #[field(char)]
    pub author: Option<Arc<str>>,
    /// Publication year.
    #[field(integer)]
    pub published_year: i32,
}

#[methods(Book)]
impl Book {
    /// `describe()`: the title and author.
    #[method]
    pub async fn describe(this: &RecordSet<Book>, tx: &mut Transaction) -> Result<String> {
        let name = this.get(tx, Book::NAME).await?;
        let author = this.get(tx, Book::AUTHOR).await?;
        Ok(format!(
            "{} by {}",
            name.as_deref().unwrap_or("?"),
            author.as_deref().unwrap_or("Anonymous")
        ))
    }
}
