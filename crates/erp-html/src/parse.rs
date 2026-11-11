//! Admission and the bounded fragment parse.
//!
//! Admission runs before a single byte reaches the parser: the input length against the input
//! limit, then (for byte input) UTF-8, then a scan for NUL. The parse is a fragment parse with
//! context `body`, HTML namespace and scripting enabled, built into the arena sink under one
//! [`HtmlBudget`]. The first refusal ends the parse; the partial tree is dropped, never returned.

use std::sync::Arc;

use html5ever::budget::{Budget, BudgetExceeded};
use html5ever::driver::{ParseOpts, parse_fragment};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tokenizer::TokenizerOpts;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{QualName, local_name, ns};

use crate::budget::{HtmlBudget, map_budget_error};
use crate::dom::{Dom, DomSink};
use crate::{HtmlSanitizeError, Limits};

/// Refuse input longer than the input limit. Runs first, before any scan or parse.
pub(crate) fn admit_len(len: usize, limits: &Limits) -> Result<(), HtmlSanitizeError> {
    let len = u64::try_from(len).unwrap_or(u64::MAX);
    let limit = limits.input_bytes();
    if len > limit {
        return Err(HtmlSanitizeError::InputTooLarge { len, limit });
    }
    Ok(())
}

/// Refuse input that holds a NUL byte, reporting the offset of the first one.
pub(crate) fn admit_nul(input: &str) -> Result<(), HtmlSanitizeError> {
    match input.bytes().position(|b| b == 0) {
        Some(at) => Err(HtmlSanitizeError::NulByte {
            offset: u64::try_from(at).unwrap_or(u64::MAX),
        }),
        None => Ok(()),
    }
}

/// The error for a refused request: an internal fault recorded in the budget wins, else the
/// refusal itself.
fn refusal(budget: &HtmlBudget, refused: BudgetExceeded) -> HtmlSanitizeError {
    budget
        .failure()
        .unwrap_or_else(|| map_budget_error(refused))
}

fn checked(budget: &HtmlBudget) -> Result<(), HtmlSanitizeError> {
    match budget.failure() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Parse `input` as a fragment in a `body` context under `budget`.
///
/// The input must already have passed admission. Charged up front: the input length as work and
/// as retained bytes (the parser holds a copy), and one node for the context element. The
/// generated `html` root and everything under it is admitted by the parser and the sink.
pub(crate) fn parse_fragment_bounded(
    input: &str,
    budget: &Arc<HtmlBudget>,
) -> Result<Dom, HtmlSanitizeError> {
    let len = input.len();
    budget
        .admit_work(u64::try_from(len).unwrap_or(u64::MAX))
        .map_err(|e| refusal(budget, e))?;
    budget.admit_bytes(len).map_err(|e| refusal(budget, e))?;
    budget.admit_node().map_err(|e| refusal(budget, e))?;

    let shared: Arc<dyn Budget> = budget.clone();
    let opts = ParseOpts {
        tokenizer: TokenizerOpts {
            budget: Some(Arc::clone(&shared)),
            ..TokenizerOpts::default()
        },
        tree_builder: TreeBuilderOpts {
            budget: Some(shared),
            ..TreeBuilderOpts::default()
        },
    };
    let context = QualName::new(None, ns!(html), local_name!("body"));
    let mut parser = parse_fragment(
        DomSink::new(Arc::clone(budget)),
        opts,
        context,
        Vec::new(),
        true,
    );
    checked(budget)?;
    parser.process(StrTendril::from_slice(input));
    checked(budget)?;
    let dom = parser.finish();
    checked(budget)?;
    Ok(dom)
}
