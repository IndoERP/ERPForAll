// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native addons of `addons/` by name: the ones the `erp` binary links and the ones it does
//! not, each with its reason. Plain constants without dependencies: `build.rs` reads this file
//! for the per-addon identity receipts, so the receipts and the linked code are one list.

/// The native addons the `erp` binary links, in dependency order. A served node offers exactly
/// these (and the data-only addons of its addon directory) for installation.
pub const LINKED: &[&str] = &[
    "base",
    "barcodes",
    "uom",
    "mail",
    "product",
    "product_mail",
    "web",
    "acceptance_notes",
    "theme_office_blue",
    "analytic",
    "account",
    "account_mail",
    "purchase",
    "purchase_mail",
    "purchase_three_way_match",
    "barcodes_gs1_nomenclature",
    "reporting",
    "account_reporting",
    "stock",
    "stock_account",
    "valuation",
    "stock_mail",
    "purchase_stock",
    "sales_team",
    "sale",
    "sale_stock",
    "ai_base",
    "hr",
];

/// The reason for an addon that is only its manifest and its dependency declaration: offering it
/// would install an application that promises features and has none.
pub const DECLARED_ONLY: &str = "declared, no behaviour yet (manifest and dependency declaration \
     only, no model, no data); not linked into this binary until it ships behaviour";

/// The addons of `addons/` with a Rust crate that the `erp` binary does not link, with the reason
/// the addon catalogue logs for each. Every crate under `addons/` is in [`LINKED`] or here.
pub const NOT_LINKED: &[(&str, &str)] = &[
    ("delivery", DECLARED_ONLY),
    ("product_expiry", DECLARED_ONLY),
    ("stock_delivery", DECLARED_ONLY),
    ("stock_dropshipping", DECLARED_ONLY),
    ("stock_landed_costs", DECLARED_ONLY),
    ("stock_sms", DECLARED_ONLY),
];
