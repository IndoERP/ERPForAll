// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stateless MCP streamable HTTP. Credentials are authenticated before the request is parsed.
use super::*;
use serde_json::{Value, json};

const VERSION: &str = "2025-11-25";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn rpc_error(id: &Value, code: i32, message: &str) -> HttpResult<Response> {
    json_response(
        StatusCode::OK,
        &json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
}

impl WebApp {
    pub(super) async fn mcp(
        &self,
        parts: &axum::http::request::Parts,
        body: Body,
        client: IpAddr,
        auth: &Arc<Auth>,
        rpc: &erp_rpc::RpcService,
    ) -> HttpResult<Response> {
        let headers = &parts.headers;
        if one(headers, "origin")?.is_some_and(|origin| origin != self.config.origin()) {
            return Err(response(StatusCode::FORBIDDEN, "Foreign origin"));
        }
        if parts.method != Method::POST {
            let mut result = response(StatusCode::METHOD_NOT_ALLOWED, "POST required");
            result
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST"));
            return Err(result);
        }
        if one(headers, "content-type")?
            .and_then(|s| s.split(';').next())
            .map(str::trim)
            != Some("application/json")
        {
            return Err(response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected application/json",
            ));
        }
        if one(headers, "mcp-protocol-version")?.is_some_and(|version| version != VERSION) {
            return Err(bad("Unsupported MCP protocol version."));
        }
        if !headers.contains_key(header::AUTHORIZATION) {
            return Err(self.unauthorized("A bearer API key is required.").await);
        }
        let mut credential = self.bearer_credential(headers, client, auth).await?;
        credential.audience = KeyScope::Mcp;
        let credential = Arc::new(credential);
        let mut tx = self.open_document_bearer(&credential).await?;
        let principal = tx.env().principal().clone();
        // Release the connection before waiting for input or beginning business dispatch.
        tx.rollback().await.map_err(auth_error)?;
        drop(tx);
        let request = McpRequest {
            parts,
            client,
            auth,
            rpc,
            credential: &credential,
            principal,
        };
        let mut result = request.dispatch(body).await;
        if credential.drop_cookie.load(Ordering::Relaxed) {
            let response = match &mut result {
                Ok(res) | Err(res) => res,
            };
            self.set_cookie(response, "session_id", "", 0)?;
        }
        result
    }
}

struct McpRequest<'a> {
    parts: &'a axum::http::request::Parts,
    client: IpAddr,
    auth: &'a Auth,
    rpc: &'a erp_rpc::RpcService,
    credential: &'a Arc<BearerCredential>,
    principal: erp_orm::Principal,
}

impl McpRequest<'_> {
    async fn dispatch(self, body: Body) -> HttpResult<Response> {
        let bytes = to_bytes(body, 1024 * 1024)
            .await
            .map_err(|_| response(StatusCode::PAYLOAD_TOO_LARGE, "Request too large"))?;
        let message: Message = match serde_json::from_slice(&bytes) {
            Ok(message) => message,
            Err(error) if error.is_syntax() || error.is_eof() => {
                return rpc_error(&Value::Null, -32700, "Invalid JSON.");
            }
            Err(_) => return rpc_error(&Value::Null, -32600, "Invalid JSON-RPC message."),
        };
        if message.jsonrpc != "2.0" || !(message.params.is_null() || message.params.is_object()) {
            return rpc_error(&Value::Null, -32600, "Invalid JSON-RPC message.");
        }
        let Some(id) = message.id else {
            if message.method.starts_with("notifications/") {
                return Ok(StatusCode::ACCEPTED.into_response());
            }
            return Err(bad("Unsupported notification."));
        };
        if !(id.is_string() || id.is_i64() || id.is_u64()) {
            return rpc_error(&Value::Null, -32600, "Invalid request ID.");
        }
        let result = match message.method.as_str() {
            "initialize" => {
                if message.params["protocolVersion"].as_str().is_none() {
                    return rpc_error(&id, -32602, "Missing protocolVersion.");
                }
                json!({
                    "protocolVersion": VERSION, "capabilities": {"tools": {}},
                    "serverInfo": {"name": "ERPForAll", "version": env!("CARGO_PKG_VERSION")}
                })
            }
            "ping" => json!({}),
            "tools/list" => {
                let cursor = message.params.get("cursor");
                if cursor.is_some_and(|value| value.as_str().is_none_or(|text| text.len() > 128)) {
                    return rpc_error(&id, -32602, "Invalid tool cursor.");
                }
                self.list(cursor.and_then(Value::as_str)).await?
            }
            "tools/call" => return self.call(&id, &message.params).await,
            _ => return rpc_error(&id, -32601, "Unknown method."),
        };
        json_response(
            StatusCode::OK,
            &json!({"jsonrpc":"2.0", "id":id, "result":result}),
        )
    }

    async fn list(&self, cursor: Option<&str>) -> HttpResult<Value> {
        let mut tx = self
            .credential
            .open(CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        let tools = self
            .rpc
            .assistant_tools(self.auth.orm(), &mut tx, cursor)
            .await;
        tx.rollback().await.map_err(auth_error)?;
        let (page, next) = tools.map_err(auth_error)?;
        let encoded = erp_fields::json::canonical(&erp_fields::json::Json::Array(page));
        let tools: Value = serde_json::from_slice(&encoded).map_err(|_| internal())?;
        let mut result = json!({"tools":tools});
        if let Some(cursor) = next {
            result["nextCursor"] = cursor.into();
        }
        Ok(result)
    }

    async fn call(self, id: &Value, params: &Value) -> HttpResult<Response> {
        let Some((model, method)) = params["name"]
            .as_str()
            .filter(|name| name.len() <= 128)
            .and_then(|name| name.rsplit_once('.'))
        else {
            return rpc_error(id, -32602, "Invalid tool name.");
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return rpc_error(id, -32602, "Invalid tool arguments.");
        }
        let mut call = erp_rpc::RpcCall::json2(
            &format!("/json/2/{model}/{method}"),
            serde_json::to_vec(&arguments).map_err(|_| internal())?,
            erp_rpc::Caller::Principal(self.principal),
        )
        .assistant()
        .credential(self.credential.clone())
        .client_address(self.client);
        if let Some(correlation) = self.parts.extensions.get::<erp_rpc::CorrelationId>() {
            call = call.correlation(correlation.clone());
        }
        if let Some(key) = one(&self.parts.headers, "idempotency-key")?
            && let Some(key) =
                erp_rpc::IdempotencyKey::from_headers(&[key]).map_err(|e| bad(e.message()))?
        {
            call = call.idempotency_key(key);
        }
        let reply = self.rpc.call(call).await;
        let result = match (reply.status(), reply.approval_request_id()) {
            (401 | 403 | 404, _) => return rpc_error(id, -32602, "This tool is unavailable."),
            (422, _) => return rpc_error(id, -32602, "Invalid tool arguments."),
            (200, _) => {
                let text = std::str::from_utf8(reply.body()).map_err(|_| internal())?;
                json!({"content":[{"type":"text", "text":text}], "isError":false})
            }
            (409, Some(request_id)) => json!({
                "content":[{"type":"text", "text":format!("Approval request {request_id} is pending.")}],
                "structuredContent":{"approval_request_id":request_id}, "isError":true
            }),
            (status, _) => json!({
                "content":[{"type":"text", "text":format!("ERP call failed (HTTP {status}).")}], "isError":true
            }),
        };
        json_response(
            StatusCode::OK,
            &json!({"jsonrpc":"2.0", "id":id, "result":result}),
        )
    }
}
