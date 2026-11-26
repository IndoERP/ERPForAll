// SPDX-License-Identifier: MIT OR Apache-2.0
//! Accounting report providers. Financial source reads always use the current caller.
erp_sdk::addon!(
    name = "account_reporting",
    exact_only = true,
    depends = ["account", "analytic", "reporting"]
);
mod aged_partner;
mod asset_register;
mod balance_sheet;
mod budget_avc_report;
mod consolidation_report;
mod cost_center_report;
mod general_ledger;
mod migrations;
mod partner_ledger;
pub mod schema;
mod trial_balance;
erp_addon_reporting::provider_declarations!(general_ledger::GeneralLedger);
