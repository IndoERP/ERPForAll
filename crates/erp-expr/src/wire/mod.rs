//! Versioned, bounded AST transport shared with the browser interpreter.
//!
//! Raw schema DTOs are not executable. [`ValidatedAst::from_json`] admits bytes only after
//! checking the JSON limits, closed schema, tree edges and exact logical height. [`encode`]
//! checks even manually constructed public [`Expr`] values before publication.
//!
//! ```
//! use erp_expr::{parse, wire::{encode, ValidatedAst}};
//! let expression = parse("amount * 2")?;
//! let bytes = encode(&expression)?;
//! let admitted = ValidatedAst::from_json(&bytes)?;
//! assert_eq!(admitted.as_expr(), &expression);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
mod admission;
mod convert;
mod numbers;
mod schema;
mod validate;

use crate::Expr;
pub use schema::*;
use std::io::Write;

/// Maximum encoded UTF-8 JSON bytes per expression (including whitespace).
pub const MAX_ENCODED_BYTES: usize = 16 * 1024 * 1024;
/// Maximum postorder nodes in one AST.
pub const MAX_NODES: usize = 65_536;
/// Maximum nested JSON containers, independently of logical AST depth.
pub const MAX_JSON_DEPTH: usize = 16;
/// Maximum logical tree height, including comprehension clause levels.
pub const MAX_AST_DEPTH: usize = 100;

/// Wire admission errors are distinct from Python evaluation exceptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WireErrorKind {
    /// Malformed schema, invalid graph, or transport quota violation.
    InvalidAst,
}

/// An invalid transport, with a stable path and complete diagnostic.
#[derive(Debug, thiserror::Error)]
#[error("InvalidAst at {path}: {message}")]
#[non_exhaustive]
pub struct WireError {
    path: String,
    message: String,
    #[source]
    source: Option<serde_json::Error>,
}
impl WireError {
    pub(super) fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
            source: None,
        }
    }
    fn json(source: serde_json::Error) -> Self {
        Self {
            path: "$".into(),
            message: format!("invalid JSON: {source}"),
            source: Some(source),
        }
    }
    /// Typed error classification; never a Python exception.
    pub fn kind(&self) -> WireErrorKind {
        WireErrorKind::InvalidAst
    }
    /// JSON path of the rejected component.
    pub fn path(&self) -> &str {
        &self.path
    }
    /// Complete diagnostic without the kind and path prefix.
    pub fn message(&self) -> &str {
        &self.message
    }
}
fn byte_limit() -> WireError {
    WireError::new("$", "encoded JSON exceeds 16777216 bytes")
}
fn depth_limit() -> WireError {
    WireError::new("$.nodes", "logical AST depth exceeds 100")
}
fn node_limit() -> WireError {
    WireError::new("$.nodes", "expected 1..=65536 nodes")
}

/// Owned admitted AST. Construction is private; no unchecked `Deserialize` exists.
#[derive(Debug)]
#[non_exhaustive]
pub struct ValidatedAst {
    expr: Expr,
    encoded_bytes: usize,
}
impl ValidatedAst {
    /// Decode bounded JSON bytes. Fails on malformed JSON, schema, graph, or any transport limit.
    pub fn from_json(bytes: &[u8]) -> Result<Self, WireError> {
        let normalized = admission::normalize(bytes)?;
        let document: WireDocument =
            serde_json::from_slice(&normalized).map_err(WireError::json)?;
        Self::try_from(document)
    }
    /// Admit and own a copy of a public AST, checking every shape and transport limit.
    pub fn from_expr(expr: &Expr) -> Result<Self, WireError> {
        Self::try_from(convert::from_expr(expr)?)
    }
    /// Borrow the admitted tree. Callers cannot mutate the stored expression.
    pub fn as_expr(&self) -> &Expr {
        &self.expr
    }
    /// The exact UTF-8 byte length of the canonical JSON produced by [`Self::to_json`].
    ///
    /// Admission computes this from the normalized wire document using the same serializer as
    /// encoding. This accessor borrows immutable metadata: it neither allocates nor re-encodes
    /// the tree. It is not the length of the original JSON input, which can contain whitespace,
    /// escaped Unicode or noncanonical numeric tokens.
    ///
    /// ```
    /// use erp_expr::{parse, wire::ValidatedAst};
    /// let ast = ValidatedAst::from_expr(&parse("name + 1")?)?;
    /// assert_eq!(ast.encoded_len(), ast.to_json()?.len());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.encoded_bytes
    }
    /// Encode the admitted tree, preserving integer values and exact floating-point bits.
    pub fn to_json(&self) -> Result<Vec<u8>, WireError> {
        encode(&self.expr)
    }
}
impl TryFrom<WireDocument> for ValidatedAst {
    type Error = WireError;
    fn try_from(document: WireDocument) -> Result<Self, Self::Error> {
        document.validate()?;
        let encoded_bytes = encoded_len(&document)?;
        Ok(Self {
            expr: convert::into_expr(document)?,
            encoded_bytes,
        })
    }
}
impl WireDocument {
    /// Produce a raw transport from a public tree, rejecting invalid shapes and exceeded limits.
    pub fn from_expr(expr: &Expr) -> Result<Self, WireError> {
        let document = convert::from_expr(expr)?;
        document.validate()?;
        encoded_len(&document)?;
        Ok(document)
    }
    /// Serialize a raw transport only after validation. Output never exceeds the byte limit.
    pub fn to_json(&self) -> Result<Vec<u8>, WireError> {
        self.validate()?;
        let length = encoded_len(self)?;
        let mut writer = BoundedWriter {
            length: 0,
            output: Some(Vec::with_capacity(length)),
            exceeded: false,
        };
        serde_json::to_writer(&mut writer, self).map_err(|source| {
            if writer.exceeded {
                byte_limit()
            } else {
                WireError::json(source)
            }
        })?;
        match writer.output {
            Some(bytes) => Ok(bytes),
            None => Err(WireError::new("$", "encoder output buffer missing")),
        }
    }
}

/// Encode a borrowed public AST, with no intermediate cloned executable tree.
///
/// Returns [`WireErrorKind::InvalidAst`] for invalid shapes or transport limits.
pub fn encode(expr: &Expr) -> Result<Vec<u8>, WireError> {
    let document = convert::from_expr(expr)?;
    document.to_json()
}

// Count before allocation. This writer is memory-only: no filesystem, sockets or platform I/O.
struct BoundedWriter {
    length: usize,
    output: Option<Vec<u8>>,
    exceeded: bool,
}
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(length) = self
            .length
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_ENCODED_BYTES)
        else {
            self.exceeded = true;
            return Err(std::io::Error::other("AST byte limit"));
        };
        if let Some(output) = &mut self.output {
            output.extend_from_slice(bytes);
        }
        self.length = length;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded_len(document: &WireDocument) -> Result<usize, WireError> {
    let mut writer = BoundedWriter {
        length: 0,
        output: None,
        exceeded: false,
    };
    serde_json::to_writer(&mut writer, document).map_err(|source| {
        if writer.exceeded {
            byte_limit()
        } else {
            WireError::json(source)
        }
    })?;
    Ok(writer.length)
}
