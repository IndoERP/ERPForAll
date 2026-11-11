//! HTML sanitising for ERPForAll.
//!
//! A pure, guest-compatible L1 crate with no internal dependencies. This slice holds the validated
//! types the sanitiser is called with and returns: [`FieldPolicy`] / [`SanitizePolicy`] /
//! [`DefaultPolicy`], [`Limits`], [`HtmlSanitizeError`] and the opaque [`SanitizedHtml`], plus the
//! static allow-list tables. [`sanitize`] runs admission, comment repair, a bounded parse, the
//! cleaner, normalisation and the serialiser under one budget, then checks that doing it again on
//! the output changes nothing; see its documentation for what is not done.
//!
//! [`SanitizedHtml`] cannot be built outside the crate:
//!
//! ```compile_fail,E0624
//! let _ = erp_html::SanitizedHtml::new(String::new(), true);
//! ```

#![deny(missing_docs)]

mod budget;
mod clean;
mod css;
mod dom;
#[cfg(test)]
mod engine_tests;
mod error;
#[cfg(test)]
mod fault;
#[cfg(test)]
mod lead_r1_tests;
mod limits;
mod normalize;
mod parse;
mod policy;
mod report;
mod result;
mod sanitizer;
mod serialize;
mod tables;
mod url;

pub use error::{HtmlSanitizeError, ParseFailure};
pub use limits::{LimitError, LimitKind, Limits};
pub use policy::{DefaultPolicy, FieldPolicy, OutputMethod, SanitizeMode, SanitizePolicy};
pub use report::{RemovalEntry, RemovalKind, RemovalReason, RemovalReport};
pub use result::{SanitizeResult, SanitizedHtml};
pub use sanitizer::{BuiltinSanitizer, HtmlSanitizer};

/// Sanitise `input` under `policy` and the call's `limits`: admission, comment-marker repair,
/// bounded parse, clean, normalise, serialise, then the same again on the output (the reparse
/// gate), all charged to one budget.
///
/// The cleaner removes kill-list elements (in every namespace) with their subtrees, form
/// controls, and conditional comments; unwraps `html`, `body`, `form`, every element outside the
/// tag allow-list and every HTML element with an `encoding` attribute; keeps only allow-listed
/// attributes, never `on*`; blanks a link attribute whose URL scheme is not allowed; scrubs
/// `style` attributes (dangerous declarations go in every mode, the property whitelist applies
/// under `sanitize_style`, the attribute goes under `strip_style`); wraps a fragment's leading
/// text in `<p>`; and returns the empty string for a result with no element and no text. The
/// serialiser writes HTML or XML (see [`OutputMethod`]).
///
/// **Reparse gate.** After serialising, the output is parsed, cleaned and serialised once more
/// under the same budget: the work counter covers both passes, while the node and live-byte
/// counters restart for the second pass (each pass is held to the node and memory limits on its
/// own). The depth limit applies to the output as well as the input: text-led input, which gets a
/// `<p>` wrapper, accepts one level less (254 nested elements at the ceiling) than input that does
/// not start with text (255), so the output is always acceptable input. The same holds for the
/// other limits that apply to input: an output longer than the input byte limit is refused with
/// [`HtmlSanitizeError::OutputTooLarge`] (escaping and the `<p>` wrapper can lengthen text), and
/// a call is refused with the work limit unless twice the second pass's work fits it, so
/// `sanitize(y)` succeeds under the same limits whenever `sanitize(x)` returned `y`, except that
/// the working-memory limit is not guaranteed idempotent at the boundary (not measured). If the second
/// result differs from the first by a single byte the call fails with
/// [`ParseFailure::ReparseUnstable`]; the output is never returned unstable.
///
/// **Report and strict mode.** The first pass counts every removal in a [`RemovalReport`] (grouped
/// by kind, name and reason, with bounded sample locations, see [`Limits::report_bytes`] and
/// [`Limits::report_samples`]); the result carries it. Under [`SanitizeMode::Strict`] a non-empty
/// report is returned as [`HtmlSanitizeError::DisallowedContent`] instead. A report over its byte
/// limit fails with [`HtmlSanitizeError::ReportTooLarge`]; it is never truncated.
///
/// **Trust.** A result that passed the gate under a [`SanitizePolicy::Default`] policy is marked
/// trusted ([`SanitizedHtml::is_trusted_markup`] is true). `FieldPolicy::UnsanitisedText` never
/// reaches this function and so never yields a [`SanitizedHtml`]. Whether a field may store and
/// render the result as markup is decided by the FIELDS slice's rollout, not here.
///
/// Not done (other slices): the `email_outgoing` preset (a sibling task), quote annotation, and the other wrapper elements the
/// reference's HTML library adds around top-level content (decision D-2: only the leading-text
/// paragraph is reproduced). Checks against a real browser's parser are not part of this crate;
/// the gate and the tests use the same parser as the sanitiser. The sealed [`HtmlSanitizer`] seam uses [`BuiltinSanitizer`] for host integration.
///
/// Link attributes are neither given a `rel` nor stripped of `target`: preventing reverse
/// tabnabbing is not a goal of the sanitiser (the `target` attribute follows the allow-list).
///
/// # Errors
/// [`HtmlSanitizeError::InputTooLarge`], [`HtmlSanitizeError::NulByte`], a limit error from any
/// stage, [`HtmlSanitizeError::OutputTooLarge`], [`HtmlSanitizeError::ParseFailed`], or
/// [`HtmlSanitizeError::Internal`].
pub fn sanitize(
    input: &str,
    policy: &SanitizePolicy,
    limits: &Limits,
    mode: SanitizeMode,
) -> Result<SanitizeResult, HtmlSanitizeError> {
    parse::admit_len(input.len(), limits)?;
    parse::admit_nul(input)?;
    let budget = budget::HtmlBudget::new(limits);
    let (markup, report) = run_pipeline(input, policy, limits, &budget)?;
    // Budget and report-limit failures above keep their own kind; strict rejection comes last.
    match mode {
        SanitizeMode::Strict if !report.is_empty() => {
            return Err(HtmlSanitizeError::DisallowedContent { report });
        }
        SanitizeMode::Strict | SanitizeMode::Clean => {}
    }
    // Only a Default-policy result that passed the reparse gate reaches this point.
    let SanitizePolicy::Default(_) = policy;
    Ok(SanitizeResult::new(
        SanitizedHtml::new(markup, true),
        report,
    ))
}

/// Run the pipeline on `input`, then again on its output, under the same `budget`; return the
/// output only if the second run reproduced it exactly. Admission has already run.
pub(crate) fn run_pipeline(
    input: &str,
    policy: &SanitizePolicy,
    limits: &Limits,
    budget: &std::sync::Arc<budget::HtmlBudget>,
) -> Result<(String, RemovalReport), HtmlSanitizeError> {
    let mut builder = report::ReportBuilder::new(limits.report_bytes(), limits.report_samples());
    let first = run_once(input, policy, limits, budget, Some(&mut builder))?;
    let report = builder.finish();
    // The output is read back as input (by the gate and by every later call), so it must itself
    // pass input admission under these limits; escaping can make it longer than the input.
    let input_limit = limits.input_bytes();
    if u64::try_from(first.len()).unwrap_or(u64::MAX) > input_limit {
        return Err(HtmlSanitizeError::OutputTooLarge { limit: input_limit });
    }
    // The first pass's tree is gone: the second starts its node and byte counts afresh (only the
    // first output is still held). The depth limit is unchanged.
    // The report stays held, so it counts toward the retained bytes.
    let held = first.capacity().saturating_add(report.retained_bytes());
    budget.begin_next_pass(held);
    // The second pass reports nothing: a removal there would change the output and fail the gate.
    let second = run_once(&first, policy, limits, budget, None)?;
    // Calling again on the output costs the second pass twice; see `admit_repeat_work`.
    budget.admit_repeat_work()?;
    #[cfg(test)]
    let second = fault::apply(second);
    // A fault the parser recorded without returning an error must still stop the call.
    if let Some(failure) = budget.failure() {
        return Err(failure);
    }
    if second != first {
        return Err(HtmlSanitizeError::ParseFailed(
            ParseFailure::ReparseUnstable,
        ));
    }
    Ok((first, report))
}

/// One pass: comment repair, bounded parse, clean, normalise, serialise.
fn run_once(
    input: &str,
    policy: &SanitizePolicy,
    limits: &Limits,
    budget: &std::sync::Arc<budget::HtmlBudget>,
    report: Option<&mut report::ReportBuilder>,
) -> Result<String, HtmlSanitizeError> {
    let SanitizePolicy::Default(policy) = policy;
    let text = normalize::repair_comments(input, budget)?;
    let dom = parse::parse_fragment_bounded(&text, budget)?;
    let mut items = clean::clean(&dom, policy, budget, report)?;
    normalize::wrap_leading_text(&mut items, budget)?;
    serialize::serialize(&items, policy.output_method(), limits, budget)
}

/// [`sanitize`] for byte input: the length is checked first, then UTF-8, then NUL.
///
/// # Errors
/// As [`sanitize`], plus [`HtmlSanitizeError::InvalidUtf8`].
pub fn sanitize_bytes(
    input: &[u8],
    policy: &SanitizePolicy,
    limits: &Limits,
    mode: SanitizeMode,
) -> Result<SanitizeResult, HtmlSanitizeError> {
    parse::admit_len(input.len(), limits)?;
    let text =
        std::str::from_utf8(input).map_err(|source| HtmlSanitizeError::InvalidUtf8 { source })?;
    sanitize(text, policy, limits, mode)
}

// Compile the published guide's Rust examples with the crate's doctests.
#[cfg(doctest)]
mod report_guide {}
