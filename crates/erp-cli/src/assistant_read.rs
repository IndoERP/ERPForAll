// SPDX-License-Identifier: MIT OR Apache-2.0
//! Connect the native assistant's typed search and aggregation to the same authenticated, audited RPC as the web client.
use erp_addon_ai_base::{
    aggregate::{self, AggregatePlan},
    search::{self, SearchPlan},
};
use erp_fields::{Error, Result, Value};
use erp_orm::{BoxFuture, CompanyId, Context, CredentialOptions, Orm, Transaction};
use erp_rpc::{Caller, RpcCall, RpcCredential, RpcService};
use erp_sdk::ai::{ToolCall, ToolResult};
use erp_security::auth::{Auth, SessionSecret};
use std::sync::Arc;

struct SearchCredential {
    auth: Arc<Auth>,
    secret: Arc<SessionSecret>,
    call: ToolCall,
    plan: SearchPlan,
}
impl std::fmt::Debug for SearchCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchCredential").finish_non_exhaustive()
    }
}
impl RpcCredential for SearchCredential {
    fn begin<'a>(
        &'a self,
        orm: &'a Orm,
        options: CredentialOptions,
    ) -> BoxFuture<'a, Result<Transaction>> {
        Box::pin(async move {
            if !Arc::ptr_eq(orm.registry(), self.auth.orm().registry()) {
                return Err(Error::access(
                    "Assistant session registry changed; repeat the search.",
                ));
            }
            let mut tx = self.auth.begin_session(&self.secret, options).await?.tx;
            if search::prepare(&mut tx, &self.call).await? != self.plan {
                return Err(Error::access(
                    "Assistant search permissions or limits changed; repeat the search.",
                ));
            }
            Ok(tx)
        })
    }
}
/// Read retail records with an existing browser session and a trusted, selected company. The
/// metadata transaction ends before business dispatch. Every RPC attempt rechecks the session,
/// field scope and configuration in its own primary transaction. Successful reads inherit the
/// configured RPC audit for the actual target model. A future HTTP turn must authenticate CSRF.
///
/// # Errors
/// Reports an invalid/expired session, refused model/field, invalid query, or failed audited RPC.
/// Raw RPC errors and business record contents are never returned in diagnostics.
pub async fn search_records(
    service: &RpcService,
    auth: Arc<Auth>,
    secret: Arc<SessionSecret>,
    company: CompanyId,
    call: &ToolCall,
) -> Result<ToolResult> {
    let ctx = Context::from_pairs([(
        "allowed_company_ids",
        Value::list([Value::Int(company.get())]),
    )]);
    let mut tx = auth
        .begin_session(
            &secret,
            CredentialOptions::new()
                .context(ctx)
                .read_only("ai_base.search_records.prepare"),
        )
        .await?
        .tx;
    let plan = search::prepare(&mut tx, call).await?;
    let principal = tx.env().principal().clone();
    tx.rollback().await?;
    drop(tx);
    let path = format!("/json/2/{}/web_search_read", plan.model());
    let bytes = serde_json::to_vec(&plan.rpc_arguments())
        .map_err(|_| Error::internal("Could not encode assistant search."))?;
    let auth = Arc::new(SearchCredential {
        auth,
        secret,
        call: call.clone(),
        plan: plan.clone(),
    });
    let reply = service
        .call(RpcCall::json2(&path, bytes, Caller::Principal(principal)).credential(auth))
        .await;
    match reply.status() {
        200 => {}
        401 | 403 | 404 => {
            return Err(Error::access(
                "Assistant search cannot access this model or field.",
            ));
        }
        422 => {
            return Err(Error::validation(
                "The requested search could not be applied to these fields.",
            ));
        }
        status => {
            return Err(Error::internal(format!(
                "Audited web_search_read failed with status {status}."
            )));
        }
    }
    let value = serde_json::from_slice(reply.body())
        .map_err(|_| Error::internal("Invalid audited search response."))?;
    plan.project(call, &value)
}

struct AggregateCredential {
    auth: Arc<Auth>,
    secret: Arc<SessionSecret>,
    call: ToolCall,
    plan: AggregatePlan,
}
impl std::fmt::Debug for AggregateCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregateCredential")
            .finish_non_exhaustive()
    }
}
impl RpcCredential for AggregateCredential {
    fn begin<'a>(
        &'a self,
        orm: &'a Orm,
        options: CredentialOptions,
    ) -> BoxFuture<'a, Result<Transaction>> {
        Box::pin(async move {
            if !Arc::ptr_eq(orm.registry(), self.auth.orm().registry()) {
                return Err(Error::access(
                    "Assistant session registry changed; repeat the aggregation.",
                ));
            }
            let mut tx = self.auth.begin_session(&self.secret, options).await?.tx;
            if aggregate::prepare(&mut tx, &self.call).await? != self.plan {
                return Err(Error::access(
                    "Assistant aggregation permissions or limits changed; repeat the aggregation.",
                ));
            }
            Ok(tx)
        })
    }
}

/// Compute group-by aggregates across retail records with an existing browser session and selected company.
/// Dispatches the audited grouped-read RPC and returns bounded, labelled result data.
///
/// # Errors
/// Reports an invalid/expired session, refused model/field, invalid query, or failed audited RPC.
pub async fn aggregate_records(
    service: &RpcService,
    auth: Arc<Auth>,
    secret: Arc<SessionSecret>,
    company: CompanyId,
    call: &ToolCall,
) -> Result<ToolResult> {
    let ctx = Context::from_pairs([(
        "allowed_company_ids",
        Value::list([Value::Int(company.get())]),
    )]);
    let mut tx = auth
        .begin_session(
            &secret,
            CredentialOptions::new()
                .context(ctx)
                .read_only("ai_base.aggregate_records.prepare"),
        )
        .await?
        .tx;
    let plan = aggregate::prepare(&mut tx, call).await?;
    let principal = tx.env().principal().clone();
    tx.rollback().await?;
    drop(tx);
    let path = format!("/json/2/{}/{}", plan.model(), plan.rpc_method());
    let bytes = serde_json::to_vec(&plan.rpc_arguments())
        .map_err(|_| Error::internal("Could not encode assistant aggregation."))?;
    let auth = Arc::new(AggregateCredential {
        auth,
        secret,
        call: call.clone(),
        plan: plan.clone(),
    });
    let reply = service
        .call(RpcCall::json2(&path, bytes, Caller::Principal(principal)).credential(auth))
        .await;
    match reply.status() {
        200 => {}
        401 | 403 | 404 => {
            return Err(Error::access(
                "Assistant aggregation cannot access this model or field.",
            ));
        }
        422 => {
            return Err(Error::validation(
                "The requested aggregation could not be applied to these fields.",
            ));
        }
        status => {
            return Err(Error::internal(format!(
                "Audited grouped read failed with status {status}."
            )));
        }
    }
    let value = serde_json::from_slice(reply.body())
        .map_err(|_| Error::internal("Invalid audited aggregation response."))?;
    plan.project(call, &value)
}
