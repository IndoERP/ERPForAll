// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native addons the `erp` binary links: one list for the serving node, the lifecycle
//! commands and the identity receipts (`names.rs`, read by `build.rs`).
use erp_registry::AddonDescriptor;

mod names;
pub use names::{DECLARED_ONLY, LINKED, NOT_LINKED};

/// The descriptors of [`LINKED`], in the same order; every composition site passes these to
/// `ModuleSystem::builder(..).code(..)`.
pub static DESCRIPTORS: &[&AddonDescriptor] = &[
    &erp_addon_base::ADDON,
    &erp_addon_barcodes::ADDON,
    &erp_addon_uom::ADDON,
    &erp_addon_mail::ADDON,
    &erp_addon_product::ADDON,
    &erp_addon_product_mail::ADDON,
    &erp_addon_web::ADDON,
    &erp_addon_acceptance_notes::ADDON,
    &erp_addon_theme_office_blue::ADDON,
    &erp_addon_analytic::ADDON,
    &erp_addon_account::ADDON,
    &erp_addon_barcodes_gs1_nomenclature::ADDON,
    &erp_addon_stock::ADDON,
];

/// Why an addon of `addons/` with a Rust crate is not linked, if it is in [`NOT_LINKED`].
#[must_use]
pub fn not_linked_reason(name: &str) -> Option<&'static str> {
    NOT_LINKED
        .iter()
        .find(|(listed, _)| *listed == name)
        .map(|(_, reason)| *reason)
}
