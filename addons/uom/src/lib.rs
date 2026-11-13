//! Units of measure: internal model and exact conversion foundation.

erp_sdk::addon!(name = "uom", exact_only = true, depends = ["base"]);

pub mod conversion;
pub mod error;

pub mod models;
pub use models::{Uom, UomMethods};

mod res_groups;

mod protection;

mod guard_state;
pub mod references;

pub mod stock;
