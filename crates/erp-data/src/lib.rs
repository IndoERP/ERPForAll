//! ERPForAll data layer (layer L7).
//!
//! - Data files: the XML and CSV readers turn an addon's data file into typed
//!   operations without touching a database.
//!   Loading the operations into the
//!   database is.
//! - [`model_data`]: the `ir.model.data` model and the external-id API (lookup,
//!   resolution, upsert) shared by the import and data loading; external ids are the readers'
//!   validated [`XmlId`].
//!   Problems with imported data are [`ImportMessage`]s; misuse and
//!   infrastructure failures are `erp_fields::Error`s, the error type of the ORM it runs on.

pub mod ast;
pub mod csv;
pub mod error;
pub mod i18n;
pub mod impex;
mod lines;
pub mod load;
pub mod migration;
pub mod model_data;
pub mod reader;
pub mod xml;

pub use ast::{
    AssetOp, CsvOp, DataOperation, DeleteOp, DeleteTarget, Expr, FieldOp, FieldValue, FunctionArg,
    FunctionOp, GroupCommand, InvalidXmlId, MenuItemOp, OpEnv, QualifiedXmlId, RecordOp,
    RootActive, ShorthandTarget, TemplateOp, ValueOp, XmlId, XmlIdProblem,
};
pub use csv::{CsvImport, CsvImportResult, CsvRow, CsvTranslation, model_from_filename, parse_csv};
pub use error::{NODE_TEXT_LIMIT, ParseError, ParseErrorKind};
pub use i18n::{
    OverwriteMode, PoEntry, PoLoadSummary, PoModelRef, PoOccurrence, PoReader, SaveSummary,
    TranslationEntry, TranslationImporter,
};
pub use impex::{
    DEFAULT_BATCH_SIZE, ExportResult, ExportRights, ImpexExt, ImportMessage, LoadResult, MAX_ROWS,
    MessageType, RowSpan, SuperuserOnly, export_data, load,
};

#[doc(hidden)]
pub mod internal_impex {
    //! Test-only access for the allocation/semantics tests of PERF.LOAD-STREAM; not API.
    pub use crate::impex::extract::{Extracted, Key, Path, Raw, RawRecord, extract_batch};
    pub use crate::impex::row::RowSource;
}
pub use model_data::{
    IR_MODEL_DATA, LoadRecord, XidBinding, XidCache, XidRecord, ir_model_data_decl,
};
pub use reader::{ConvertMode, DataFileContent, load_data_file};
pub use xml::{MAX_DEPTH, MAX_OPERATION_BYTES, ReaderLimits, XmlDataReader, parse_xml_str};
