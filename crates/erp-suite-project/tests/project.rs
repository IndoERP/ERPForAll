// SPDX-License-Identifier: MIT OR Apache-2.0
//! The `project` addon installed into real databases: the install without and with demo data,
//! access as project users and managers of one or two companies, and the task chatter.
//! One test binary keeps the link cost of the full addon stack to a single build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]

#[path = "project/support.rs"]
mod support;

#[path = "project/access.rs"]
mod access;
#[path = "project/business_demo.rs"]
mod business_demo;
#[path = "project/demo.rs"]
mod demo;
#[path = "project/install.rs"]
mod install;
#[path = "project/tracking.rs"]
mod tracking;
