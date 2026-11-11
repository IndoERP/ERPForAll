//! The opaque result of a sanitising call.

use crate::report::RemovalReport;

/// Sanitised markup. It has no public constructor and no `serde` support, so the only way to hold
/// one is to get it from the sanitiser; a plain `String` cannot be passed off as sanitised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedHtml {
    markup: String,
    trusted: bool,
}

impl SanitizedHtml {
    /// Wrap sanitiser output. `trusted` is true only for the full default allow-lists (spec 4.0).
    pub(crate) fn new(markup: String, trusted: bool) -> SanitizedHtml {
        SanitizedHtml { markup, trusted }
    }

    /// The sanitised markup.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.markup
    }

    /// Whether the markup may be shown as trusted: true only for the full default allow-lists or a
    /// stricter subset with no bypass.
    #[must_use]
    pub fn is_trusted_markup(&self) -> bool {
        self.trusted
    }
}

/// What a sanitising call returns: the markup and the aggregated removal report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SanitizeResult {
    output: SanitizedHtml,
    report: RemovalReport,
}

impl SanitizeResult {
    pub(crate) fn new(output: SanitizedHtml, report: RemovalReport) -> SanitizeResult {
        SanitizeResult { output, report }
    }

    /// What the policy removed (empty when nothing was).
    #[must_use]
    pub fn report(&self) -> &RemovalReport {
        &self.report
    }

    /// Take the markup and the report.
    #[must_use]
    pub fn into_parts(self) -> (SanitizedHtml, RemovalReport) {
        (self.output, self.report)
    }

    /// The sanitised markup.
    #[must_use]
    pub fn output(&self) -> &SanitizedHtml {
        &self.output
    }

    /// Take the sanitised markup.
    #[must_use]
    pub fn into_output(self) -> SanitizedHtml {
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_exposes_the_markup_and_the_trust_flag() {
        let result = SanitizeResult::new(
            SanitizedHtml::new("<p>x</p>".to_owned(), true),
            RemovalReport::default(),
        );
        assert_eq!(result.output().as_str(), "<p>x</p>");
        assert!(result.output().is_trusted_markup());
        let output = result.into_output();
        assert_eq!(output.as_str(), "<p>x</p>");

        let untrusted = SanitizedHtml::new(String::new(), false);
        assert!(!untrusted.is_trusted_markup());
        assert_eq!(untrusted.as_str(), "");
    }
}
