//! Sealed host integration seam.
use crate::{HtmlSanitizeError, Limits, SanitizeMode, SanitizePolicy, SanitizeResult};

mod sealed {
    pub trait Sealed {}
}

/// The built-in sanitizer interface. Addons cannot implement a bypass.
///
/// Results remain untrusted across a guest boundary: the host calls its own
/// implementation for each field write even if a guest already validated it.
pub trait HtmlSanitizer: sealed::Sealed {
    /// Admit and sanitize UTF-8 text, preserving every diagnostic.
    ///
    /// # Errors
    /// Returns the engine's typed input, resource, policy or internal failure.
    fn sanitize(
        &self,
        input: &str,
        policy: &SanitizePolicy,
        limits: &Limits,
        mode: SanitizeMode,
    ) -> Result<SanitizeResult, HtmlSanitizeError>;
}

/// The framework's stateless, bounded HTML sanitizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinSanitizer;
impl sealed::Sealed for BuiltinSanitizer {}
impl HtmlSanitizer for BuiltinSanitizer {
    fn sanitize(
        &self,
        input: &str,
        policy: &SanitizePolicy,
        limits: &Limits,
        mode: SanitizeMode,
    ) -> Result<SanitizeResult, HtmlSanitizeError> {
        crate::sanitize(input, policy, limits, mode)
    }
}
