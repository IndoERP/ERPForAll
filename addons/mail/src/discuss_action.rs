// SPDX-License-Identifier: MIT OR Apache-2.0
//! The full Discuss workspace uses the same authenticated conversation RPCs as quick chat.
use erp_sdk::client_actions::ClientActionDecl;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Map, Value as Json};
use erp_sdk::{BoxFuture, PrincipalKind};

fn tags() -> Result<Vec<ClientActionDecl>> {
    Ok(vec![ClientActionDecl::new(
        "mail.discuss",
        "Discuss",
        workspace,
    )?])
}
erp_sdk::client_action_declarations!(tags);

fn workspace<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Json>,
) -> BoxFuture<'a, Result<Map<String, Json>>> {
    Box::pin(async move {
        let principal = env.principal();
        if principal.kind() != PrincipalKind::User
            || principal.scope().is_some()
            || principal.actor().is_some()
            || (!env.su() && !tx.has_group(env, "base.group_user").await?)
        {
            return Err(Error::access("Discuss requires an internal user."));
        }
        if env.company_id()?.is_none() {
            return Err(Error::validation("Select a company to open Discuss."));
        }
        if !params.is_empty() {
            return Err(Error::validation("Discuss takes no action parameters."));
        }
        Ok(Map::new())
    })
}
