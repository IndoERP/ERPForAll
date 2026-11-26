// SPDX-License-Identifier: MIT OR Apache-2.0
use std::error::Error;
/// Stable category of a PDF operation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PdfErrorKind {
    /// Invalid caller options.
    InvalidOptions,
    /// Invalid PDF syntax or graph.
    Malformed,
    /// A feature outside the supported profile.
    Unsupported,
    /// A configured admission bound was reached.
    Limit,
    /// The caller's byte consumer failed.
    Output,
    /// An unwind-capable dependency panicked.
    Panic,
}
/// Typed failure with a stable message and retained diagnostic source.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
#[non_exhaustive]
pub struct PdfError {
    kind: PdfErrorKind,
    message: String,
    #[source]
    source: Option<Box<dyn Error + Send + Sync>>,
}
impl PdfError {
    /// Stable failure category.
    pub fn kind(&self) -> PdfErrorKind {
        self.kind
    }
    pub(crate) fn new(kind: PdfErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }
    pub(crate) fn source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }
}
pub(crate) type Result<T> = std::result::Result<T, PdfError>;
pub(crate) fn invalid(s: impl Into<String>) -> PdfError {
    PdfError::new(PdfErrorKind::InvalidOptions, s)
}
pub(crate) fn malformed(s: impl Into<String>) -> PdfError {
    PdfError::new(PdfErrorKind::Malformed, s)
}
/// Tasks that own a refused feature (specification §4.4): an error names the one that adds it.
pub(crate) mod owner {
    pub const ENCRYPT: &str = "a later release";
    pub const OVERLAY: &str = "a later release";
    pub const ARCHIVAL: &str = "a later release";
}
/// A feature outside the profile that `owner` (one of [`owner`]) will add.
pub(crate) fn unsupported(s: impl std::fmt::Display, owner: &str) -> PdfError {
    PdfError::new(
        PdfErrorKind::Unsupported,
        format!("PDF feature {s} is unsupported ({owner})"),
    )
}
/// A feature this library does not process and no task plans to add.
pub(crate) fn refused(s: impl std::fmt::Display) -> PdfError {
    PdfError::new(
        PdfErrorKind::Unsupported,
        format!("PDF feature {s} is refused by design"),
    )
}
pub(crate) fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| PdfError::new(PdfErrorKind::Panic, "PDF operation panicked"))?
}
