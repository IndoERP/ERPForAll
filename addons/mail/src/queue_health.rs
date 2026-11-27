// SPDX-License-Identifier: MIT OR Apache-2.0
//! Technical mail queue screen. The metrics RPC independently checks jobs.admin.
use erp_sdk::BoxFuture;
use erp_sdk::client_actions::ClientActionDecl;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Map, Value as Json};

fn declarations() -> Result<Vec<ClientActionDecl>> {
    Ok(vec![ClientActionDecl::new(
        "mail.queue_health",
        "Outgoing Mail Queue",
        authorize,
    )?])
}
erp_sdk::client_action_declarations!(declarations);

fn authorize<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Json>,
) -> BoxFuture<'a, Result<Map<String, Json>>> {
    Box::pin(async move {
        if !tx.has_group(env, "base.group_system").await? {
            return Err(Error::access(
                "Mail queue monitoring requires a Settings administrator.",
            ));
        }
        if !params.is_empty() {
            return Err(Error::validation(
                "The mail queue screen takes no parameters.",
            ));
        }
        Ok(Map::new())
    })
}

use crate::outgoing::MailOutgoing;

#[extend(MailOutgoing)]
struct MailQueueHealth {}

#[methods(MailOutgoing, addon = "mail")]
impl MailQueueHealth {
    #[method]
    #[rpc(effect = read, model)]
    async fn queue_health(this: &RecordSet<MailOutgoing>, tx: &mut Transaction) -> Result<Value> {
        let env = this.env();
        let mut result = tx.job_queue_health(env, crate::outgoing::KIND).await?;
        let company = env
            .company_id()?
            .ok_or_else(|| Error::validation("Select a company."))?;
        let latest = tx.query(
            "SELECT a.detail::text FROM mail_outgoing_attempt a JOIN mail_outgoing m ON m.id=a.outgoing_id
             WHERE m.company_id=$1 AND a.detail IS NOT NULL ORDER BY a.started DESC, a.id DESC LIMIT 1",
            &[&company],
        ).await?;
        let detail = latest
            .first()
            .map(|row| row.try_get::<_, String>(0))
            .transpose()
            .map_err(|_| Error::invariant("Invalid mail attempt detail."))?;
        result["latest_error"] = erp_sdk::serde_json::json!(detail);
        Ok(Value::Json(Arc::new(result)))
    }
}
