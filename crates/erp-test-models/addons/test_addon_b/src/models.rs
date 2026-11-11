//! Extension of `test_addon.book`.

use erp_addon_test_addon_a::Book;
use erp_sdk::prelude::*;

/// `test_addon_b`'s extension of `test_addon.book`.
#[extend(Book)]
pub struct BookExtension {
    /// ISBN.
    #[field(char, string = "ISBN")]
    pub isbn: Option<Arc<str>>,
    /// Rating.
    #[field(float, string = "Rating")]
    pub rating: f64,
}

#[methods(Book)]
impl BookExtension {
    /// Appends the rating to the base description.
    #[method(override)]
    pub async fn describe(
        this: &RecordSet<Book>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<String> {
        let base_desc = sup.call(tx, this.as_records(), ()).await?;
        let rating = this.get(tx, BookExtension::RATING).await?;
        Ok(format!("{base_desc} (Rating: {rating:.1})"))
    }
}
