// SPDX-License-Identifier: MIT OR Apache-2.0
//! Refusal sentences of the stock addon are ERPForAll's own wording (amendment 2026-11-13).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_addon_stock::StockError;
use erp_addon_stock::quant_engine::QuantError;

/// A refusal that used to repeat the reference's sentence now says the same thing in our words, and the two enums
/// that refuse a company change share one sentence.
#[test]
fn company_change_refusals_use_our_own_sentence() {
    let stock = StockError::CompanyChangeForbidden.to_string();
    let quant = QuantError::CompanyChange.to_string();
    assert_eq!(stock, quant);
    assert!(
        stock.contains("Archive it and create a new record for the other company"),
        "{stock}"
    );
    assert!(!stock.contains("forbidden at this point"), "{stock}");
}

/// Quant refusals name the thing and what to do, with the removed reference phrasings absent.
#[test]
fn quant_refusals_do_not_repeat_removed_phrasings() {
    let texts = [
        QuantError::EmptyChange.to_string(),
        QuantError::NotStorable.to_string(),
        QuantError::Copy.to_string(),
        QuantError::UsageToView.to_string(),
        QuantError::UsageWithStock.to_string(),
    ];
    for text in &texts {
        for old in [
            "should be set",
            "duplicate stock quants",
            "can't be converted",
        ] {
            assert!(!text.contains(old), "{text}");
        }
    }
    assert_eq!(texts[0], "Set a quantity, a reserved quantity or both.");
}
