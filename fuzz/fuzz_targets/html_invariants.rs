//! The invariants the `erp-html` fuzz targets check, shared with the
//! stable test `crates/erp-html/tests/fuzz_invariants.rs`, which includes this file by path so the
//! nightly fuzz run and the ordinary CI test cannot drift apart. A violation panics, which
//! libFuzzer reports as a crash and the test reports as a failure.
//!
//! For each of `Limits::ceiling()` and `Limits::recommended()`:
//! - a refusal is never `HtmlSanitizeError::Internal` (an inconsistency inside the sanitiser) and,
//!   for bytes, `InvalidUtf8` only when the bytes are not UTF-8;
//! - a success `y` is no longer than `min(output_bytes, input_bytes)`, is trusted, and sanitising
//!   `y` again returns `y` (the working-memory limit is not guaranteed idempotent at its boundary,
//!   so a `MemoryLimit` refusal of that second call is allowed, as the crate documents).

#![allow(dead_code, clippy::panic, clippy::expect_used)]

use erp_html::{
    DefaultPolicy, HtmlSanitizeError, Limits, SanitizeMode, SanitizePolicy, sanitize,
    sanitize_bytes,
};

fn policy() -> SanitizePolicy {
    SanitizePolicy::Default(DefaultPolicy::new())
}

fn limit_sets() -> [Limits; 2] {
    [Limits::ceiling(), Limits::recommended()]
}

/// Check one outcome of sanitising `input`; `y` is the markup on success.
fn check_outcome(input_len: usize, limits: &Limits, outcome: Result<String, HtmlSanitizeError>) {
    match outcome {
        Err(HtmlSanitizeError::Internal { reason }) => {
            panic!("internal sanitiser fault on a {input_len} byte input: {reason}")
        }
        Err(_) => {}
        Ok(y) => {
            let cap = limits.output_bytes().min(limits.input_bytes());
            assert!(
                y.len() as u64 <= cap,
                "output of {} bytes is longer than min(output_bytes, input_bytes) = {cap}",
                y.len()
            );
            match sanitize(&y, &policy(), limits, SanitizeMode::Clean) {
                Ok(again) => assert_eq!(
                    again.into_output().as_str(),
                    y,
                    "sanitising the output changed it"
                ),
                Err(HtmlSanitizeError::MemoryLimit { .. }) => {}
                Err(other) => panic!("sanitising an accepted output failed: {other}"),
            }
        }
    }
}

/// The invariants for text input.
pub fn check_str(input: &str) {
    for limits in limit_sets() {
        let outcome = sanitize(input, &policy(), &limits, SanitizeMode::Clean).map(|r| {
            let output = r.into_output();
            assert!(
                output.is_trusted_markup(),
                "a default-policy success must be trusted"
            );
            output.as_str().to_owned()
        });
        check_outcome(input.len(), &limits, outcome);
    }
}

/// The invariants for byte input; invalid UTF-8 must be refused as such.
pub fn check_bytes(input: &[u8]) {
    for limits in limit_sets() {
        let outcome = sanitize_bytes(input, &policy(), &limits, SanitizeMode::Clean);
        // admission order is length, then UTF-8: within the length limit, invalid UTF-8 must be refused
        if input.len() as u64 <= limits.input_bytes() && std::str::from_utf8(input).is_err() {
            assert!(
                matches!(outcome, Err(HtmlSanitizeError::InvalidUtf8 { .. })),
                "invalid UTF-8 not refused as InvalidUtf8: {outcome:?}"
            );
        }
        if let Err(HtmlSanitizeError::InvalidUtf8 { .. }) = &outcome {
            assert!(
                std::str::from_utf8(input).is_err(),
                "valid UTF-8 refused as invalid"
            );
            continue;
        }
        check_outcome(
            input.len(),
            &limits,
            outcome.map(|r| r.into_output().as_str().to_owned()),
        );
    }
}
