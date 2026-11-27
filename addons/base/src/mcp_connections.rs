// SPDX-License-Identifier: MIT OR Apache-2.0
//! An account owner may inspect and revoke their MCP connections without possessing a bearer.
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};

#[extend(ResUsers)]
struct McpConnections {}

#[methods(ResUsers, addon = "base")]
impl McpConnections {
    #[method(sealed)]
    #[rpc(effect = read, model)]
    async fn erp_mcp_connections(
        _this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        before: Option<i64>,
    ) -> Result<Value> {
        erp_sdk::auth::apikey_manage::require_connection_owner(tx)?;
        let uid = tx.env().user_id().get();
        if before.is_some_and(|id| id <= 0) {
            return Err(Error::validation("Invalid MCP connection cursor."));
        }
        let identity = tx
            .query(
                "SELECT current_database()::text,login::text FROM res_users WHERE id=$1 AND active",
                &[&uid],
            )
            .await?;
        let missing = || Error::access("A signed-in account is required.");
        let identity = identity.first().ok_or_else(missing)?;
        let shape = |e| Error::db("Invalid MCP connection metadata").with_source(e);
        let db: String = identity.try_get(0).map_err(shape)?;
        let login: String = identity.try_get(1).map_err(shape)?;
        let rows = tx.query("SELECT jsonb_build_object('id',id,'name',name,'read_only',read_only,'models',models,
              'methods',methods,'expires_at',to_char(expiration_date,'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
              'last_used_at',to_char(last_used_at,'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
              'expired',coalesce(expiration_date < now() AT TIME ZONE 'utc',false))
              FROM res_users_apikeys WHERE user_id=$1 AND scope='mcp' AND kind='agent' AND capabilities='none'
              AND digest IS NOT NULL AND legacy_hash IS NULL AND ($2::bigint IS NULL OR id<$2)
              ORDER BY id DESC LIMIT 51", &[&uid,&before]).await?;
        let more = rows.len() > 50;
        let items = rows
            .iter()
            .take(50)
            .map(|r| r.try_get::<_, Json>(0).map_err(shape))
            .collect::<Result<Vec<_>>>()?;
        let next = if more {
            items.last().and_then(|v| v["id"].as_i64())
        } else {
            None
        };
        Ok(Value::Json(Arc::new(
            json!({"db":db,"login":login,"items":items,"next":next}),
        )))
    }

    #[method(sealed)]
    #[rpc(effect = write, model)]
    async fn erp_revoke_mcp_connection(
        _this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        connection_id: i64,
    ) -> Result<bool> {
        erp_sdk::auth::apikey_manage::revoke_mcp_connection(tx, connection_id).await
    }
}
