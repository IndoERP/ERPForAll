//! Reading files of an addon from the loader (file-valued fields, `type="file"` checks, the
//! `<function>` of D). The module system implements [`AddonFiles`] with its bounded, no-link
//! reader; the loader never touches the file system itself.

use std::fmt;

use erp_orm::BoxFuture;

/// Why a file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileReadError {
    /// The addon has no such file (`File not found: m/static/x.png`).
    NotFound {
        /// The module.
        module: String,
        /// The path inside the module.
        path: String,
    },
    /// The file is larger than the limit.
    TooLarge {
        /// The module.
        module: String,
        /// The path inside the module.
        path: String,
        /// The limit in bytes.
        limit: u64,
    },
    /// Any other failure (a link, a source that cannot be read), with the reader's message.
    Failed(String),
}

impl fmt::Display for FileReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { module, path } => write!(f, "File not found: {module}/{path}"),
            Self::TooLarge {
                module,
                path,
                limit,
            } => write!(f, "{module}/{path} is larger than {limit} bytes"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FileReadError {}

/// Read-only access to the files of the addons being loaded.
pub trait AddonFiles: Send + Sync {
    /// The content of `path` inside `module`, at most the reader's byte limit.
    fn read<'a>(
        &'a self,
        module: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileReadError>>;

    /// Whether `path` inside `module` is a file this reader would read, without reading its
    /// content (`type="file"` only names the file). The default reads it; a reader with a cheaper
    /// check overrides it.
    fn check<'a>(
        &'a self,
        module: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<(), FileReadError>> {
        Box::pin(async move { self.read(module, path).await.map(drop) })
    }
}

/// No files at all: every read fails. For callers that load data without addon files (tests of
/// records and expressions).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFiles;

impl AddonFiles for NoFiles {
    fn read<'a>(
        &'a self,
        module: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileReadError>> {
        Box::pin(async move {
            Err(FileReadError::Failed(format!(
                "no addon files are available to this loader session (asked for {module}/{path})"
            )))
        })
    }
}
