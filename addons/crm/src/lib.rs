// SPDX-License-Identifier: MIT OR Apache-2.0
//! CRM business records, sales lifecycle and configuration.
erp_sdk::addon!(
    name = "crm",
    exact_only = true,
    depends = ["base", "web", "contacts", "mail", "reporting", "sales_team"]
);

pub mod lead;
pub use lead::CrmLead;
mod attainment;
mod history;
mod intake;
mod lifecycle;
mod overview;
mod pipeline;
mod qualification;
mod reporting;
mod routing;
