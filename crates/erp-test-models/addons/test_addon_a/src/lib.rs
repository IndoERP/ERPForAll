//! Fixture addon `test_addon_a`: defines `test_addon.book`, views, data and `describe` method.

pub mod models;
pub use models::*;

erp_sdk::addon!(name = "test_addon_a");
