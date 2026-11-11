//! Fixture addon `test_ext_c`: a second extension of `test_ext.book`, independent of
//! `test_ext_b` (both depend only on `test_ext_a`), so two crates extend the same model and the
//! same method.

use erp_addon_test_ext_a::Book;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_ext_c", depends = ["test_ext_a"]);

/// C's extension of `test_ext.book`: a rating with a default.
#[extend(Book)]
pub struct BookRating {
    /// Rating from 0 to 5.
    #[field(integer, default = 3)]
    pub rating: i32,
}

#[methods(Book)]
impl BookRating {
    /// Appends the rating to the result of the next implementation.
    #[method(override)]
    async fn describe(this: &RecordSet<Book>, tx: &mut Transaction, sup: Super) -> Result<String> {
        let rating = this.get(tx, BookRating::RATING).await?;
        Ok(format!(
            "{} [{rating}/5]",
            sup.call(tx, this.as_records(), ()).await?
        ))
    }
}
