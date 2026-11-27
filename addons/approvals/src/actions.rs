// SPDX-License-Identifier: MIT OR Apache-2.0
use erp_sdk::client_actions::ClientActionDecl;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Map, Value as Json};
use erp_sdk::{BoxFuture, PrincipalKind};

fn declarations() -> Result<Vec<ClientActionDecl>> {
    Ok(vec![
        ClientActionDecl::new("approvals.requests", "Approval requests", requests)?,
        ClientActionDecl::new("approvals.configuration", "Approval workflows", workflows)?,
    ])
}
erp_sdk::client_action_declarations!(declarations);

fn requests<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Json>,
) -> BoxFuture<'a, Result<Map<String, Json>>> {
    authorize(env, tx, params, "base.group_user")
}

fn workflows<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Json>,
) -> BoxFuture<'a, Result<Map<String, Json>>> {
    authorize(env, tx, params, "base.group_system")
}

fn authorize<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Json>,
    group: &'static str,
) -> BoxFuture<'a, Result<Map<String, Json>>> {
    Box::pin(async move {
        let principal = env.principal();
        if principal.kind() != PrincipalKind::User
            || principal.scope().is_some()
            || principal.actor().is_some()
            || (!env.su() && !tx.has_group(env, group).await?)
        {
            return Err(Error::access("You cannot open this approval workspace."));
        }
        if !params.is_empty() {
            return Err(Error::validation(
                "This approval workspace takes no action parameters.",
            ));
        }
        Ok(Map::new())
    })
}
