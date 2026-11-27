// SPDX-License-Identifier: MIT OR Apache-2.0
//! Administrative browser-session inventory. Only operator metadata crosses the RPC boundary.

use erp_sdk::auth::{EndReason, SessionId, end_sessions};
use erp_sdk::client_actions::ClientActionDecl;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Map, Value as Json, json};
use erp_sdk::{BoxFuture, PrincipalKind, Value};

const TAG: &str = "base.technical_sessions";
const PAGE: usize = 50;

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static TECHNICAL_ATTACHMENT_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("technical_attachment", &["char"]);

fn tags() -> Result<Vec<ClientActionDecl>> {
    Ok(vec![
        ClientActionDecl::new(TAG, "Sessions and Devices", check_action)?,
        ClientActionDecl::new("base.identity_provider", "Single Sign-On", check_action)?,
    ])
}
erp_sdk::client_action_declarations!(tags);

async fn authorize(env: &Env, tx: &mut Transaction) -> Result<()> {
    let caller = env.principal();
    if caller.kind() != PrincipalKind::User
        || caller.scope().is_some()
        || caller.actor().is_some()
        || env.su()
        || !caller.resolution().is_resolved()
        || !tx.has_group(env, "base.group_system").await?
    {
        return Err(Error::access(
            "Browser session administration requires a system administrator session",
        ));
    }
    Ok(())
}

fn check_action<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Json>,
) -> BoxFuture<'a, Result<Map<String, Json>>> {
    Box::pin(async move {
        authorize(env, tx).await?;
        if !params.is_empty() {
            return Err(Error::validation(
                "This security screen takes no action parameters",
            ));
        }
        Ok(Map::new())
    })
}

fn parse_id(raw: &str) -> Result<SessionId> {
    SessionId::parse(raw).map_err(|_| Error::validation("Invalid browser session identifier"))
}

#[extend(ResUsers)]
struct ResUsersTechnicalSessions {}

#[methods(ResUsers, addon = "base")]
impl ResUsersTechnicalSessions {
    /// Read at most fifty active sessions, or the detail of a specific listed session.
    #[method(sealed)]
    #[rpc(effect = read, model)]
    async fn erp_technical_sessions(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        cursor: Option<String>,
        session: Option<String>,
    ) -> Result<Value> {
        authorize(this.env(), tx).await?;
        if cursor.is_some() && session.is_some() {
            return Err(Error::validation(
                "Choose either a browser session or a page cursor",
            ));
        }
        let cursor = cursor
            .as_deref()
            .map(parse_id)
            .transpose()?
            .map(|id| id.to_text());
        let session = session
            .as_deref()
            .map(parse_id)
            .transpose()?
            .map(|id| id.to_text());
        let rows = tx
            .query(
                r#"
SELECT s.id::text,jsonb_build_object(
    'id',s.id::text,'user',left(u.login,256),'trusted',s.device_trusted,
    'created',s.created_at,'last_seen',s.last_seen_at,'expires',s.expires_at,
    'agent',left(s.last_agent,1024))
FROM erp_session s JOIN res_users u ON u.id=s.user_id
WHERE s.state='active' AND s.expires_at>statement_timestamp()
  AND ($1::text IS NULL OR s.id<$1::text::uuid)
  AND ($2::text IS NULL OR s.id=$2::text::uuid)
ORDER BY s.id DESC LIMIT $3
"#,
                &[&cursor, &session, &((PAGE + 1) as i64)],
            )
            .await?;
        if session.is_some() && rows.is_empty() {
            return Err(Error::validation(
                "This browser session is no longer active",
            ));
        }
        let shape = |source| Error::db("Invalid browser session data").with_source(source);
        let next = if rows.len() > PAGE {
            Some(rows[PAGE - 1].try_get::<_, String>(0).map_err(shape)?)
        } else {
            None
        };
        let sessions = rows
            .iter()
            .take(PAGE)
            .map(|row| row.try_get::<_, Json>(1).map_err(shape))
            .collect::<Result<Vec<_>>>()?;
        Ok(Value::Json(Arc::new(
            json!({"sessions":sessions,"next":next}),
        )))
    }

    /// Revoke this browser session through the normal session-end hooks.
    #[method(sealed)]
    #[rpc(effect = write, model)]
    async fn erp_revoke_technical_session(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        session: String,
    ) -> Result<bool> {
        authorize(this.env(), tx).await?;
        let id = parse_id(&session)?;
        Ok(!end_sessions(tx, &[id], EndReason::Revoked)
            .await?
            .is_empty())
    }
}
