// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installed approval inbox and workflow administration for the RPC approval engine.
erp_sdk::addon!(
    name = "approvals",
    exact_only = true,
    depends = ["base", "web"]
);

mod actions;
mod configuration;
mod workflow;

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static APPROVAL_METHOD: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("approvals.method", &["char"]);

mod request_views;
