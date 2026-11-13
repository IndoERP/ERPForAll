// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `barcodes` for ERPForAll.

#[cfg(feature = "addon")]
erp_sdk::addon!(name = "barcodes", depends = ["base", "web"]);

/// Shared native/browser barcode parsing.
pub mod engine;

#[cfg(all(feature = "browser", target_arch = "wasm32"))]
mod wasm;

#[cfg(feature = "addon")]
pub mod models;
#[cfg(feature = "addon")]
mod res_company;
