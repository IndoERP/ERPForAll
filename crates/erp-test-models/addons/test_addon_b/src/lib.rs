//! Fixture addon `test_addon_b`: extends `test_addon.book` with fields, method override and inherited view.

pub mod models;
pub use models::*;

erp_sdk::addon!(name = "test_addon_b", depends = ["test_addon_a"]);
