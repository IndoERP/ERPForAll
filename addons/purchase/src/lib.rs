// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `purchase` for ERPForAll: requests for quotation and purchase orders.

erp_sdk::addon!(
    name = "purchase",
    depends = ["base", "web", "reporting", "account"],
    exact_only = true
);

mod bill_buttons;
pub use bill_buttons::{counts, flat, readable_links, window};
mod bill_history;
mod bill_source_review;
pub mod billing;
mod direct_bill;
mod direct_bill_integrity;
pub mod line;
mod migrations;
pub mod order;
mod partner_product_buttons;
pub mod report;
pub mod requisition;
mod res_groups;
pub mod rfq_send;
pub mod settings;
mod warnings;

/// The purchase user: creates, confirms and cancels orders.
pub const USER_GROUP: &str = "purchase.group_purchase_user";
/// The purchase administrator: approves orders above the amount and unlocks.
pub const MANAGER_GROUP: &str = "purchase.group_purchase_manager";
/// The feature group controlling purchase warning messages on vendors, products, and orders.
pub const WARNING_GROUP: &str = "purchase.group_warning_purchase";
