// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `stock_account` for ERPForAll.
//!
//! Following boundary §4.1 and Decision N8, `stock_account` is the bridge addon
//! hosting stock valuation (engine erp-stockval, value journal, cost state, FIFO layers,
//! and move/lot/quant/location valuation fields).

erp_sdk::addon!(
    name = "stock_account",
    exact_only = true,
    depends = ["stock", "account"]
);

// Numbered files preserve composite foreign-key target order (registry sorts source paths).
pub mod valuation_01_source;
pub mod valuation_02_journal;
pub mod valuation_03_state;
pub mod valuation_04_layer;

fn recorded_now(_ctx: &erp_sdk::DefaultContext) -> erp_sdk::Value {
    erp_sdk::Value::DateTime(erp_sdk::DateTime::now())
}
