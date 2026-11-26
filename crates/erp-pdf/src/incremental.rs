// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::error::Result;
use crate::{
    ConformanceLevel, Duplicate, EInvoiceOptions, EmbeddedFileOptions, Emitter, MetadataUpdate,
    PdfLimits, SignedEditPolicy,
};

/// Explicit policy for appending unsigned changes to a signature-marked input.
/// No method validates cryptographic signatures, trust or the signer's authorization.
/// All emitted bytes must be discarded on error; foreign PDFs require caller-owned worker isolation.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct IncrementalEdits {
    signed: SignedEditPolicy,
}
/// Properties of a completed append operation, not a signature-validation verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct IncrementalEditSummary {
    signed: bool,
}
impl IncrementalEditSummary {
    pub(crate) fn new(signed: bool) -> Self {
        Self { signed }
    }
    /// Whether signature-related structures were observed in the source.
    /// A true value does not establish that a cryptographic signature exists or is valid.
    pub fn signed_input(&self) -> bool {
        self.signed
    }
    /// Successful operations retain every original byte as an unchanged output prefix.
    pub fn original_bytes_preserved(&self) -> bool {
        true
    }
}
impl IncrementalEdits {
    /// Default policy: refuse signature-marked inputs for mutation.
    pub fn new() -> Self {
        Self::default()
    }
    /// Select admission for signed inputs. Appended changes are not signed.
    pub fn signed_documents(mut self, policy: SignedEditPolicy) -> Self {
        self.signed = policy;
        self
    }
    /// Append an embedded file under the same limits and conformance rules as [`crate::attach`].
    pub fn attach(
        &self,
        input: &[u8],
        data: &[u8],
        file: &EmbeddedFileOptions,
        duplicate: Duplicate,
        limits: &PdfLimits,
        emit: &mut Emitter<'_>,
    ) -> Result<IncrementalEditSummary> {
        crate::attach::attach_with_policy(input, data, file, duplicate, limits, emit, self.signed)
    }
    /// Append synchronized metadata. The new values are not covered by existing signatures.
    ///
    /// ```
    /// use erp_pdf::{IncrementalEdits, MetadataUpdate, PdfError, PdfLimits, SignedEditPolicy};
    /// fn append_metadata(input: &[u8], update: &MetadataUpdate) -> Result<Vec<u8>, PdfError> {
    ///     let mut output = Vec::new();
    ///     let summary = IncrementalEdits::new().signed_documents(SignedEditPolicy::AppendOnly)
    ///         .update_metadata(input, update, &PdfLimits::default(), &mut |b| {
    ///             output.extend_from_slice(b); Ok(())
    ///         })?;
    ///     assert!(summary.original_bytes_preserved());
    ///     Ok(output) // Publish only after success; disclose that appended changes are unsigned.
    /// }
    /// ```
    pub fn update_metadata(
        &self,
        input: &[u8],
        update: &MetadataUpdate,
        limits: &PdfLimits,
        emit: &mut Emitter<'_>,
    ) -> Result<IncrementalEditSummary> {
        crate::meta::update_with_policy(input, update, limits, emit, self.signed)
    }
    /// Append an e-invoice container; existing PDF/A/XML checks still apply.
    /// Returns the XML profile and append summary, never a conformance/signature verdict.
    pub fn einvoice(
        &self,
        input: &[u8],
        xml: &[u8],
        options: &EInvoiceOptions,
        limits: &PdfLimits,
        emit: &mut Emitter<'_>,
    ) -> Result<(ConformanceLevel, IncrementalEditSummary)> {
        crate::einvoice::einvoice_with_policy(input, xml, options, limits, emit, self.signed)
    }
}
