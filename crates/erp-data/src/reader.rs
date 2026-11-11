//! One entry point for an addon data file, by extension.

use std::path::Path;

use crate::ast::DataOperation;
use crate::csv::{CsvImportResult, parse_csv};
use crate::error::{ParseError, ParseErrorKind};
use crate::xml::parse_xml_str;

/// Install (`init`) or upgrade (`update`) loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConvertMode {
    Init,
    Update,
}

impl ConvertMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Update => "update",
        }
    }
}

/// The operations of one data file.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum DataFileContent {
    Xml(Vec<DataOperation>),
    Csv(Vec<DataOperation>),
    /// Not loaded, with the reason the caller reports.
    Skipped {
        reason: String,
    },
    Empty,
}

/// `mode` matters for CSV only; `noupdate` is the file-level default.
pub fn load_data_file(
    content: &str,
    filename: &str,
    module: &str,
    mode: ConvertMode,
    noupdate: bool,
) -> Result<DataFileContent, ParseError> {
    let extension = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("xml") => Ok(DataFileContent::Xml(parse_xml_str(
            content, filename, module, noupdate,
        )?)),
        Some("csv") => match parse_csv(content, filename, module, mode)? {
            CsvImportResult::Loaded(import) => Ok(DataFileContent::Csv(
                import
                    .to_record_ops(noupdate)?
                    .into_iter()
                    .map(DataOperation::Record)
                    .collect(),
            )),
            CsvImportResult::Skipped { reason } => Ok(DataFileContent::Skipped { reason }),
            CsvImportResult::Empty => Ok(DataFileContent::Empty),
        },
        _ => Err(ParseError::new(
            filename,
            1,
            ParseErrorKind::UnknownFileType,
            format!("can't load unknown file type {filename} (expected .xml or .csv)"),
        )),
    }
}
