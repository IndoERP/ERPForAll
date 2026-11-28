// SPDX-License-Identifier: MIT OR Apache-2.0
//! The assistant workspace uses normal model access to offer installed retail sources.
use erp_sdk::access::Operation;
use erp_sdk::client_actions::ClientActionDecl;
use erp_sdk::prelude::*;
use serde_json::{Map, Value, json};

fn tags() -> Result<Vec<ClientActionDecl>> {
    Ok(vec![ClientActionDecl::new(
        "ai_base.workspace",
        "AI Assistant",
        check,
    )?])
}
erp_sdk::client_action_declarations!(tags);

fn check<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Value>,
) -> erp_sdk::BoxFuture<'a, Result<Map<String, Value>>> {
    Box::pin(async move {
        if !params.is_empty() {
            return Err(Error::validation(
                "Assistant workspace takes no input parameters.",
            ));
        }
        if !crate::ordinary_user(env) || !tx.has_group(env, "base.group_user").await? {
            return Err(Error::access(
                "Assistant workspace requires an internal user.",
            ));
        }
        let company = env
            .company()?
            .ok_or_else(|| Error::access("Select a company first."))?;
        let mut sources = Vec::new();
        for model in [
            "sale.order",
            "purchase.order",
            "stock.picking",
            "account.move",
        ] {
            if env.registry().model(model).is_some()
                && env.records(model)?.has_access(tx, Operation::Read).await?
            {
                sources.push(model);
            }
        }
        let limit = erp_sdk::params::get_int(tx, &crate::ADDON, "ai_base.search_rows").await?;
        let implementation_access = tx.has_group(env, "ai_base.group_implementation").await?;
        Ok(Map::from_iter([
            ("implementation_access".into(), json!(implementation_access)),
            ("history".into(), crate::ledger::tab_history(tx).await?),
            ("sources".into(), json!(sources)),
            ("limit".into(), json!(limit)),
            ("company_id".into(), json!(company.get())),
        ]))
    })
}
