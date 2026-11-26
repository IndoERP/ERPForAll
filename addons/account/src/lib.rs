// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `account` for ERPForAll: the accounting application.
//! This file holds only the declaration and, later, sorted `mod` lines; one directory per slice family.

erp_sdk::addon!(
    name = "account",
    depends = ["base", "web", "product", "analytic"],
    exact_only = true
);

/// Feature group enabling Cost Center & Profit Center Controlling menus and fields.
pub const COST_CENTER_GROUP: &str = "account.group_cost_center_controlling";
/// Feature group enabling Fixed Assets & Construction in Progress (CIP) menus and fields.
pub const ASSET_CIP_GROUP: &str = "account.group_asset_cip";
/// Feature group enabling Advanced Budgeting & Availability Control (AVC) menus and fields.
pub const BUDGET_AVC_GROUP: &str = "account.group_budget_avc";
/// Feature group enabling Group Financial Statement Consolidation menus and fields.
pub const CONSOLIDATION_GROUP: &str = "account.group_group_consolidation";

pub mod asset;
pub mod bank;
pub mod budget;
pub mod company;
pub mod consolidation;
pub mod controlling;
mod costing;
pub mod fiscal;
pub mod followup;
pub mod fpos;
pub mod invoice;
pub mod journal;
pub mod ledger;
pub mod locking;
pub mod mapping;
pub mod moves;
pub mod numbering;
pub mod payment;
pub mod payterm;
pub mod reconcile;
pub mod recurring;
mod res_groups;

mod settings_migration;
pub mod smart_buttons;
pub mod tax;
