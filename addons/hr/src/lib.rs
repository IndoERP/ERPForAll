// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `hr`: job positions (`hr.job`). Spike addon proving how a new native addon is
//! registered in the `erp` binary.

erp_sdk::addon!(name = "hr", depends = ["base"]);

pub mod job;

pub use job::HrJob;
