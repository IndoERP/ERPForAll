// SPDX-License-Identifier: MIT OR Apache-2.0
//! Interactive text turn transport, with the signed browser preferences token used by login/logout.
use super::*;
use erp_security::auth::SessionSecret;
use serde_json::{Value, json};

/// The sender of one prior message the signed-in browser tab keeps in its transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryRole {
    /// What the user typed.
    User,
    /// The assistant's earlier reply as shown in the tab.
    Assistant,
}

/// One prior message of the visible conversation, resent by the tab with the current question.
/// The addon re-validates every bound before the entry reaches a provider.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    role: HistoryRole,
    text: String,
}

impl HistoryEntry {
    /// The sender of the message.
    #[must_use]
    pub fn role(&self) -> HistoryRole {
        self.role
    }

    /// The message text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Application composition for one explicitly submitted message. The service authenticates the
/// session again with the selected company. A provider call must run outside a database transaction.
pub trait WebAssistant: Send + Sync {
    /// Check a pending approval and retry the exact original operation under current rights.
    fn resume_operation<'a>(
        &'a self,
        _auth: &'a Arc<Auth>,
        _secret: &'a SessionSecret,
        _company: erp_orm::CompanyId,
        _approval: i64,
        _rpc: &'a erp_rpc::RpcService,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Value>> {
        Box::pin(async {
            Err(erp_fields::Error::user(
                "erp.ai.operation_unavailable: approval continuation is unavailable.",
            ))
        })
    }

    /// Return the tab history and message admission limits for the signed-in browser.
    fn tab_history<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Value>>;

    /// Process one user-authored message with the configured company provider, with the prior
    /// visible conversation the browser tab supplied as bounded, re-validated history.
    fn turn<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a SessionSecret,
        company: erp_orm::CompanyId,
        message: String,
        history: Vec<HistoryEntry>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Value>>;

    /// Process a turn identified by the client, continuing an owned server conversation.
    fn session_turn<'a>(
        &'a self,
        _auth: &'a Arc<Auth>,
        _secret: &'a SessionSecret,
        _company: erp_orm::CompanyId,
        _message: String,
        _conversation: ConversationTurn,
        _rpc: Option<Arc<erp_rpc::RpcService>>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Value>> {
        Box::pin(async {
            Err(erp_fields::Error::user(
                "erp.ai.session_unavailable: this assistant does not support saved conversations.",
            ))
        })
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    company_id: i64,
    message: String,
    history: Option<Vec<HistoryEntry>>,
    session_id: Option<i64>,
    request_id: Option<String>,
}

/// One explicit submission. The request identity is also used to deduplicate business actions.
#[derive(Clone, Copy, Debug)]
pub struct ConversationTurn {
    /// Server conversation to continue, or a new conversation.
    pub session_id: Option<i64>,
    /// Stable identity of this submission.
    pub request_id: erp_fields::Uuid,
}

impl Input {
    fn conversation(&self) -> erp_fields::Result<Option<ConversationTurn>> {
        let invalid = || erp_fields::Error::validation("Invalid saved conversation request.");
        let Some(id) = self.request_id.as_deref() else {
            return if self.session_id.is_none() {
                Ok(None)
            } else {
                Err(invalid())
            };
        };
        let id = erp_fields::Uuid::parse(id)?;
        if id.is_nil()
            || self.session_id.is_some_and(|id| id < 1)
            || self.history.as_ref().is_some_and(|h| !h.is_empty())
        {
            return Err(invalid());
        }
        Ok(Some(ConversationTurn {
            session_id: self.session_id,
            request_id: id,
        }))
    }
}

fn refusal(status: StatusCode, message: &str) -> Response {
    json_response(status, &json!({"message":message})).unwrap_or_else(|error| error)
}

impl WebApp {
    /// Mount the optional native assistant composition.
    pub fn with_assistant(mut self, assistant: Arc<dyn WebAssistant>) -> Self {
        self.assistant = Some(assistant);
        self
    }

    pub(super) async fn assistant(
        &self,
        parts: &axum::http::request::Parts,
        body: Body,
        auth: &Arc<Auth>,
        path: &str,
        rpc: Option<&Arc<erp_rpc::RpcService>>,
    ) -> HttpResult<Response> {
        let status = path == routes::ASSISTANT_STATUS;
        let headers = &parts.headers;
        fn error(s: StatusCode, message: &str) -> HttpResult<Response> {
            Err(refusal(s, message))
        }
        if (status && parts.method != Method::GET) || (!status && parts.method != Method::POST) {
            return error(
                StatusCode::METHOD_NOT_ALLOWED,
                "Unsupported assistant request method.",
            );
        }
        if headers.contains_key(header::AUTHORIZATION) {
            return error(StatusCode::FORBIDDEN, "Use an interactive browser session.");
        }
        let header = |key| {
            one(headers, key).map_err(|_| {
                refusal(
                    StatusCode::BAD_REQUEST,
                    "Invalid assistant request headers.",
                )
            })
        };
        boundary::same_origin_fetch(
            header("sec-fetch-site")?,
            header("origin")?,
            self.config.origin(),
        )
        .map_err(|_| {
            refusal(
                StatusCode::FORBIDDEN,
                "Open the assistant in the signed-in application.",
            )
        })?;
        let secret = self
            .session(headers)
            .map_err(|_| refusal(StatusCode::UNAUTHORIZED, "Sign in to use the assistant."))?
            .ok_or_else(|| refusal(StatusCode::UNAUTHORIZED, "Sign in to use the assistant."))?;
        if !status {
            if header("content-type")? != Some("application/json") {
                return error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Expected application/json.",
                );
            }
            let token = header("x-csrf-token")?.ok_or_else(|| {
                refusal(
                    StatusCode::FORBIDDEN,
                    "Refresh the application and submit again.",
                )
            })?;
            self.verify_csrf(headers, token).await.map_err(|err| {
                refusal(err.status(), "Refresh the application and submit again.")
            })?;
        }
        let mut begun = auth
            .begin_session(
                &secret,
                CredentialOptions::new().read_only("assistant availability"),
            )
            .await
            .map_err(|err| {
                refusal(
                    auth_error(err).status(),
                    "The assistant could not validate this session.",
                )
            })?;
        let env = begun.tx.env();
        let principal = env.principal();
        if principal.kind() != erp_orm::PrincipalKind::User
            || principal.actor().is_some()
            || env.is_superuser()
            || principal.elevation() != env.sudo(false).principal().elevation()
            || !begun
                .tx
                .has_group(&env, "base.group_user")
                .await
                .map_err(|_| {
                    refusal(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Could not check assistant access.",
                    )
                })?
        {
            return error(
                StatusCode::FORBIDDEN,
                "An internal user session is required.",
            );
        }
        if env.registry().model("ai.route").is_none() {
            return error(StatusCode::NOT_FOUND, "The AI addon is unavailable.");
        }
        let engine = self.assistant.as_ref().ok_or_else(|| {
            refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "The assistant service is unavailable.",
            )
        })?;
        let history = if status {
            Some(engine.tab_history(&mut begun.tx).await.map_err(|_| {
                refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Could not load the assistant history settings.",
                )
            })?)
        } else {
            None
        };
        begun.tx.rollback().await.map_err(|_| {
            refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not complete the access check.",
            )
        })?;
        drop(begun);
        if let Some(history) = history {
            return json_response(
                StatusCode::OK,
                &json!({"available": true, "history": history}),
            );
        }
        let bytes = to_bytes(
            body,
            usize::try_from(self.config.startup_limits().rpc_body_bytes())
                .map_err(|_| internal())?,
        )
        .await
        .map_err(|_| {
            refusal(
                StatusCode::PAYLOAD_TOO_LARGE,
                "The assistant request is too large.",
            )
        })?;
        if path == routes::ASSISTANT_RESUME {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Resume {
                company_id: i64,
                approval_request_id: i64,
            }
            let request: Resume = serde_json::from_slice(&bytes).map_err(|_| {
                refusal(StatusCode::BAD_REQUEST, "Select a valid approval request.")
            })?;
            let company = erp_orm::CompanyId::new(request.company_id)
                .map_err(|_| refusal(StatusCode::BAD_REQUEST, "Select an active company."))?;
            if request.approval_request_id < 1 {
                return error(StatusCode::BAD_REQUEST, "Select a valid approval request.");
            }
            let rpc = rpc.ok_or_else(|| {
                refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The ERP action service is unavailable.",
                )
            })?;
            let result = engine.resume_operation(auth,&secret,company,request.approval_request_id,rpc).await.map_err(|err|{
                let status = StatusCode::from_u16(err.kind().http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                refusal(status,"The operation could not be continued. Review the approval and your current access.")
            })?;
            return json_response(StatusCode::OK, &result);
        }
        let request: Input = serde_json::from_slice(&bytes).map_err(|_| {
            refusal(
                StatusCode::BAD_REQUEST,
                "Enter one message, an optional conversation history, and select a company.",
            )
        })?;
        let company = erp_orm::CompanyId::new(request.company_id)
            .map_err(|_| refusal(StatusCode::BAD_REQUEST, "Select an active company."))?;
        let conversation = request.conversation().map_err(|_| {
            refusal(
                StatusCode::BAD_REQUEST,
                "Invalid saved conversation request.",
            )
        })?;
        let result = if let Some(turn) = conversation {
            engine
                .session_turn(auth, &secret, company, request.message, turn, rpc.cloned())
                .await
        } else {
            engine
                .turn(
                    auth,
                    &secret,
                    company,
                    request.message,
                    request.history.unwrap_or_default(),
                )
                .await
        };
        match result {
            Ok(reply) => json_response(StatusCode::OK, &reply),
            Err(err) => {
                let explicit = err.message().starts_with("erp.ai.");
                let msg = if explicit {
                    err.message()
                } else {
                    "The assistant could not complete this request."
                };
                let status = if err.message().starts_with("erp.ai.busy") {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::from_u16(err.kind().http_status())
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                };
                error(status, msg)
            }
        }
    }
}

#[cfg(test)]
mod conversation_input_tests {
    use super::*;
    #[test]
    fn persisted_turn_refuses_mixed_or_invalid_identity() {
        for raw in [
            json!({"company_id":1,"message":"go","session_id":3}),
            json!({"company_id":1,"message":"go","request_id":"not-a-uuid"}),
            json!({"company_id":1,"message":"go","request_id":"00000000-0000-0000-0000-000000000000"}),
            json!({"company_id":1,"message":"go","request_id":"10000000-0000-4000-8000-000000000001","history":[{"role":"assistant","text":"old"}]}),
        ] {
            let input = serde_json::from_value::<Input>(raw);
            assert!(input.is_err() || input.is_ok_and(|request| request.conversation().is_err()));
        }
    }
}
