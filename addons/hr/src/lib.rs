// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `hr`: job positions (`hr.job`) and employees (`hr.employee`).

erp_sdk::addon!(name = "hr", depends = ["base"]);

pub mod employee;
pub mod job;

pub use employee::HrEmployee;
pub use job::HrJob;
