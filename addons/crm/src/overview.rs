// SPDX-License-Identifier: MIT OR Apache-2.0
//! Authorize the CRM home. Its counts and records are ordinary reads as the salesperson.
use erp_sdk::client_actions::ClientActionDecl;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Map, Value as Json};
use erp_sdk::{BoxFuture, Operation, PrincipalKind};

fn actions() -> Result<Vec<ClientActionDecl>> {
    Ok(vec![ClientActionDecl::new(
        "crm.overview",
        "CRM Overview",
        admit,
    )?])
}
erp_sdk::client_action_declarations!(actions);

fn admit<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Json>,
) -> BoxFuture<'a, Result<Map<String, Json>>> {
    Box::pin(async move {
        if !params.is_empty() {
            return Err(Error::validation(
                "CRM Overview takes no action parameters.",
            ));
        }
        let principal = env.principal();
        if principal.kind() != PrincipalKind::User
            || principal.actor().is_some()
            || principal.scope().is_some()
            || (!env.su() && !tx.has_group(env, "sales_team.group_sale_salesman").await?)
            || !env
                .records("crm.lead")?
                .has_access(tx, Operation::Read)
                .await?
        {
            return Err(Error::access("CRM Overview requires sales access."));
        }
        Ok(Map::new())
    })
}
