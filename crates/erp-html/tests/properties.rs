//! Property tests through the public API only (step 10). The invariant
//! checker, which needs the crate's own parser, lives in `src/property_tests.rs`. Fixed case
//! count, no failure files written.

#![allow(clippy::expect_used)]

use erp_html::{
    DefaultPolicy, HtmlSanitizeError, Limits, ParseFailure, SanitizeMode, SanitizePolicy, sanitize,
};
use proptest::prelude::*;

fn run(input: &str, policy: &DefaultPolicy) -> Result<erp_html::SanitizedHtml, HtmlSanitizeError> {
    sanitize(
        input,
        &SanitizePolicy::Default(*policy),
        &Limits::ceiling(),
        SanitizeMode::Clean,
    )
    .map(erp_html::SanitizeResult::into_output)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Arbitrary text and markup-like strings (no NUL): never a panic, never an unstable success,
    /// deterministic, idempotent, and a success is trusted.
    #[test]
    fn arbitrary_input_is_deterministic_idempotent_and_trusted(
        input in "[\\x01-\\x{10FFFF}]{0,200}|[<>/!?=\"'&;#a-z0-9 \\-\\[\\]]{0,200}",
        xml in any::<bool>(),
    ) {
        let policy = if xml { DefaultPolicy::new().with_xml_output() } else { DefaultPolicy::new() };
        let first = run(&input, &policy);
        // A refusal by the reparse gate is the fail-closed outcome; anything else must succeed.
        if matches!(first, Err(HtmlSanitizeError::ParseFailed(ParseFailure::ReparseUnstable))) {
            return Ok(());
        }
        let first = first.map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert!(first.is_trusted_markup());
        let second = run(&input, &policy).map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(first.as_str(), second.as_str());
        let again = run(first.as_str(), &policy).map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(again.as_str(), first.as_str());
    }
}
