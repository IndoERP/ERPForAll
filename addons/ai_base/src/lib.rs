// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI provider configuration, explicit company routing and offline request encoding.
erp_sdk::addon!(name = "ai_base", depends = ["base"], exact_only = true);

pub mod aggregate;
pub mod canvas;
pub mod catalog;
pub mod compact;
pub mod config;
pub mod context_cache;
pub mod conversation;
pub mod encoding;
pub mod implementation;
mod insights;
pub mod ledger;
pub mod memory;
pub mod models;
pub mod operation;
pub mod routing;
pub mod search;
pub mod session;
pub mod session_rpc;
mod settings;
pub mod transport;
mod workspace;

// Transport and catalog lookup run only with an ordinary, unimpersonated user.
// The public HTTP layer separately establishes a live interactive session.
fn ordinary_user(env: &erp_sdk::prelude::Env) -> bool {
    let principal = env.principal();
    principal.kind() == erp_sdk::PrincipalKind::User
        && principal.actor().is_none()
        && !env.is_superuser()
        && principal.elevation() == env.sudo(false).principal().elevation()
}

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static MODEL_LIST_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("ai_base.model_list", &["text"]);
