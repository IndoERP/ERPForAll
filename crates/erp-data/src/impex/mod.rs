//! - [`load`]: a row-major matrix of strings, columns named by field paths (`value`,
//!   `value/id`, `value/.id`, `line_ids/name`), into created or updated records. Problems with
//!   the data are returned as [`ImportMessage`]s; when there is any error, nothing is kept
//!   ([`LoadResult::ids`] is `None`). Misuse of the API (an unknown field, an unsupported context
//!   key) and infrastructure failures (lost connection, serialization failures) are returned as
//!   errors instead.
//! - [`export_data`]: records to a matrix of export cells (`id` gives external ids, created on
//!   demand under `__export__`), after checking the caller's export right ([`ExportRights`]).

mod convert;
mod export;
pub(crate) mod extract;
mod load;
mod message;
mod path;
pub(crate) mod row;
mod rpc;

pub use export::{ExportResult, ExportRights, SuperuserOnly, export_data};
pub(crate) use load::load_rows;
pub use load::{DEFAULT_BATCH_SIZE, MAX_ROWS, load};
pub use message::{ImportMessage, LoadResult, MessageType, RowSpan};
pub(crate) use row::ValueRows;

use erp_fields::Result;
use erp_orm::{Records, Transaction};

/// `load` and `export_data` as recordset methods.
pub trait ImpexExt {
    /// [`load`] on this model.
    fn load(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
        data: &[Vec<String>],
    ) -> impl Future<Output = Result<LoadResult>> + Send;

    /// [`export_data`] on these records.
    fn export_data(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
        rights: &dyn ExportRights,
    ) -> impl Future<Output = Result<ExportResult>> + Send;
}

impl ImpexExt for Records {
    async fn load(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
        data: &[Vec<String>],
    ) -> Result<LoadResult> {
        load::load(self, tx, fields, data).await
    }

    async fn export_data(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
        rights: &dyn ExportRights,
    ) -> Result<ExportResult> {
        export::export_data(self, tx, fields, rights).await
    }
}
