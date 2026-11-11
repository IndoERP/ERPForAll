//! Internationalization and translation support for data loading (task).
//!
//! Provides gettext PO/POT catalog parsing and translation collecting from data files (XML/CSV).
//! The importer collects entries; [`TranslationImporter::save`] writes them into the jsonb language
//! maps of translated fields (task).

pub mod importer;
pub mod po;
pub mod save;

pub use importer::{PoLoadSummary, TranslationEntry, TranslationImporter};
pub use po::{PoEntry, PoModelRef, PoOccurrence, PoReader};
pub use save::{OverwriteMode, SaveSummary};
