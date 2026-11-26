// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native assistant composition with multi-turn tool calling, live data aggregation, and canvas staging.
use crate::assistant_read;
use erp_addon_ai_base::{
    canvas, catalog, compact, config::Credentials, context_cache, ledger, routing,
};
use erp_fields::{Error, Result, Value};
use erp_orm::{BoxFuture, CompanyId, Context, CredentialOptions};
use erp_rpc::RpcService;
use erp_sdk::ai::{
    ContentPart, DataSource, Instruction, LabelledData, ToolName, ToolResult, ToolTurnRequest,
    TurnMessage, TurnRole, UserText,
};
use erp_security::auth::{Auth, SessionSecret};
use erp_server::web::WebAssistant;
use serde_json::{Value as Json, json};
use std::sync::Arc;

const ASSISTANT_SYSTEM_PROMPT: &str = "\
You are ERPForAll Assistant, an executive AI analyst and ERP agent for modern business intelligence and operations.\n\
CRITICAL EXECUTION RULES:\n\
1. NEVER output conversational promises, preliminary filler, or thoughts as plain text (do NOT say 'I will look up...', 'I will check...', 'Let me pull the data').\n\
2. Immediately execute the appropriate tool calls in your first turn.\n\
3. For ANY business data question, ranking, statistics, or reports: ALWAYS use `aggregate_records` or `search_records`.\n\
   Common ERP models:\n\
   - Sales: 'sale.order' (fields: name, partner_id, amount_total, date_order, state).\n\
     Example aggregate_records: {\"model\": \"sale.order\", \"domain\": [[\"state\", \"=\", \"sale\"]], \"groupby\": [\"partner_id\"], \"aggregates\": [\"amount_total:sum\"], \"limit\": 5, \"order\": \"amount_total desc\"}\n\
   - Invoices / Piutang: 'account.move' (fields: name, partner_id, amount_total, amount_residual, invoice_date, invoice_date_due, payment_state, state).\n\
     Customer invoices filter: [[\"move_type\", \"=\", \"out_invoice\"]]\n\
   - Purchases: 'purchase.order' (fields: name, partner_id, amount_total, date_order, state).\n\
   - Stock / Deliveries: 'stock.picking' (fields: name, partner_id, state, scheduled_date).\n\
   - Partners / Customers: 'res.partner' (fields: name, email, phone, city, customer_rank).\n\
4. Visual Canvas Rendering:\n\
   Whenever the user asks for a chart, graph, table, pivot table, dashboard, or workflow, you MUST call `render_canvas` after retrieving data.\n\
   Allowed canvas_type values:\n\
   - 'pivot': Interactive multidimensional BI Pivot Table. Provide data_json containing {\"dimensions\": [\"...\"], \"measures\": [{\"key\": \"...\", \"label\": \"...\"}], \"records\": [...]}.\n\
   - 'dashboard': Multi-card executive dashboard with KPI stats, chart, and table.\n\
   - 'chart': Bar, line, or donut chart. Provide series: [{\"label\": \"...\", \"value\": 100}].\n\
   - 'table': Tabular view with columns and rows.\n\
   - 'flow': Process workflow diagram.\n\
5. Language: Always respond in the language used by the user (Indonesian if asked in Indonesian). In your final response after calling tools, provide a concise, professional summary highlighting key insights.\n\
6. If the server sends you a notice that your previous reply could not be rendered, immediately produce the complete, valid answer. Never mention the notice, never apologize, and never repeat the unusable reply.";

struct AssistantFallbackHost;
impl erp_rpc::RpcHost for AssistantFallbackHost {
    fn language_active(&self, code: &str) -> Result<bool> {
        Ok(code == "en_US" || code == "id_ID")
    }
    fn has_capability(&self, _: &erp_orm::Principal, _: &erp_orm::Capability) -> Result<bool> {
        Ok(false)
    }
}

/// Site-bound credential registry plus a node admission semaphore. Per-session and per-user
/// admission and spending limits are coordinated by PostgreSQL across nodes.
pub struct TextAssistant {
    credentials: Credentials,
    slots: tokio::sync::Semaphore,
    rpc: std::sync::RwLock<Option<Arc<RpcService>>>,
}

impl TextAssistant {
    /// Construct once at server startup from protected node configuration.
    pub fn new(credentials: Credentials) -> Self {
        let concurrent = credentials.max_concurrent_turns();
        Self {
            credentials,
            slots: tokio::sync::Semaphore::new(concurrent),
            rpc: std::sync::RwLock::new(None),
        }
    }

    /// Supply the runtime RPC service for tool dispatch.
    pub fn with_rpc(self, rpc: Arc<RpcService>) -> Self {
        if let Ok(mut guard) = self.rpc.write() {
            *guard = Some(rpc);
        }
        self
    }

    /// Set or update the runtime RPC service.
    pub fn set_rpc(&self, rpc: Arc<RpcService>) {
        if let Ok(mut guard) = self.rpc.write() {
            *guard = Some(rpc);
        }
    }
}

impl WebAssistant for TextAssistant {
    fn tab_history<'a>(&'a self, tx: &'a mut erp_orm::Transaction) -> BoxFuture<'a, Result<Json>> {
        Box::pin(ledger::tab_history(tx))
    }

    fn turn<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a SessionSecret,
        company: CompanyId,
        message: String,
        history: Vec<erp_server::web::HistoryEntry>,
    ) -> BoxFuture<'a, Result<Json>> {
        Box::pin(async move {
            let _slot = self.slots.try_acquire().map_err(|_| {
                Error::user("erp.ai.busy: the assistant is busy. Try again shortly.")
            })?;
            let options = || {
                CredentialOptions::new().context(Context::from_pairs([(
                    "allowed_company_ids",
                    Value::list([Value::Int(company.get())]),
                )]))
            };
            let mut begun = auth.begin_session(secret, options()).await?;
            let env = begun.tx.env();
            if !begun.tx.has_group(&env, "base.group_user").await?
                || env.principal().actor().is_some()
                || env.is_superuser()
            {
                return Err(Error::access(
                    "An ordinary internal user session is required.",
                ));
            }
            let (max_bytes, max_tokens) = ledger::limits(&mut begun.tx).await?;
            if message.trim().is_empty() || message.len() > max_bytes {
                return Err(Error::validation(
                    "erp.ai.message_invalid: enter a message within the configured length limit.",
                ));
            }
            let correction_rounds = usize::try_from(
                erp_sdk::params::get_int(
                    &mut begun.tx,
                    &erp_addon_ai_base::ADDON,
                    "ai_base.answer_correction_rounds",
                )
                .await
                .map_err(|_| Error::validation("Invalid assistant correction limit."))?,
            )
            .map_err(|_| Error::validation("Invalid assistant correction limit."))?;
            let (history_turns, history_bytes, history_message_bytes) =
                ledger::history_bounds(&mut begun.tx).await?;
            if let Err(reason) = validate_history(
                &history,
                history_turns,
                history_bytes,
                history_message_bytes,
            ) {
                return Err(Error::validation(format!(
                    "erp.ai.message_invalid: {reason}"
                )));
            }
            let route = routing::resolve(&env, &mut begun.tx, &self.credentials)
                .await
                .map_err(|error| {
                    if error.message().starts_with("erp.ai.route_missing") {
                        Error::user("erp.ai.route_missing: configure an assistant route for this company.")
                    } else if error.message().contains("erp.ai.provider_unconfigured")
                        || error.message().contains("credential")
                    {
                        Error::user("erp.ai.provider_unconfigured: ask an administrator to configure the selected provider.")
                    } else {
                        error
                    }
                })?;
            let transport = route.transport(&self.credentials).map_err(|_| {
                Error::user(
                    "erp.ai.provider_unconfigured: the provider is unavailable for this session.",
                )
            })?;
            let invalid =
                |_| Error::validation("erp.ai.message_invalid: could not prepare this message.");

            let mut tools = Vec::new();
            if let Ok(spec) = catalog::find_models_spec() {
                tools.push(spec);
            }
            if let Ok(spec) = erp_addon_ai_base::search::search_records_spec() {
                tools.push(spec);
            }
            if let Ok(spec) = erp_addon_ai_base::aggregate::aggregate_records_spec() {
                tools.push(spec);
            }
            if let Ok(spec) = canvas::render_canvas_spec() {
                tools.push(spec);
            }

            let sys_msg = TurnMessage::new(
                TurnRole::System,
                vec![ContentPart::Instruction(Instruction::from_static(
                    ASSISTANT_SYSTEM_PROMPT,
                ))],
            )
            .map_err(invalid)?;
            let user_msg = TurnMessage::new(
                TurnRole::User,
                vec![ContentPart::UserText(UserText::new(message))],
            )
            .map_err(invalid)?;
            // The visible conversation the tab resent, oldest first, before the current question.
            // Every entry already passed the transcript bounds; roles map onto the typed parts.
            let mut messages = Vec::with_capacity(history.len() + 2);
            messages.push(sys_msg);
            for entry in &history {
                let prior = match entry.role() {
                    erp_server::web::HistoryRole::User => TurnMessage::new(
                        TurnRole::User,
                        vec![ContentPart::UserText(UserText::new(
                            entry.text().to_string(),
                        ))],
                    ),
                    erp_server::web::HistoryRole::Assistant => TurnMessage::new(
                        TurnRole::Assistant,
                        vec![ContentPart::AssistantText(entry.text().to_string())],
                    ),
                }
                .map_err(invalid)?;
                messages.push(prior);
            }
            messages.push(user_msg);

            let initial_req = ToolTurnRequest::new(route.model(), messages.clone(), tools.clone())
                .map_err(invalid)?
                .with_max_tokens(max_tokens);

            // Bound the input reservation by the entire serialized provider payload, including its framing.
            let encoded = route
                .request_encoder()
                .and_then(|encoder| encoder.encode(&initial_req))
                .map_err(|_| {
                    Error::validation(
                        "erp.ai.message_invalid: message exceeds the provider request limit.",
                    )
                })?;
            let estimated = i64::try_from(
                serde_json::to_vec(&encoded)
                    .map_err(|_| Error::internal("Could not encode the provider request."))?
                    .len(),
            )
            .map_err(|_| Error::validation("erp.ai.message_invalid: message is too large."))?;
            let timeout = self.credentials.exchange_timeout();
            let expiry = i32::try_from(timeout.as_secs().saturating_mul(2).saturating_add(2))
                .map_err(|_| Error::internal("Invalid assistant timeout."))?;
            let reserved = ledger::reserve(
                &mut begun.tx,
                &begun.session.id().to_text(),
                &route,
                estimated,
                max_tokens,
                expiry,
            )
            .await?;
            let user = env.user_id();
            begun.tx.commit().await?;
            drop(begun);

            let service = self
                .rpc
                .read()
                .ok()
                .and_then(|guard| guard.clone())
                .unwrap_or_else(|| {
                    Arc::new(
                        RpcService::new(
                            auth.orm().clone(),
                            AssistantFallbackHost,
                            erp_rpc::RpcConfig::default(),
                        )
                        .attempt_gate(crate::approval_rpc::ApprovalGate),
                    )
                });

            let secret_arc = Arc::new(
                SessionSecret::from_cookie(&secret.to_cookie())
                    .ok_or_else(|| Error::internal("Invalid session secret."))?,
            );
            let mut total_prompt_tokens: u64 = 0;
            let mut total_completion_tokens: u64 = 0;
            let mut last_model = route.model().to_string();
            let mut text = String::new();
            let mut final_text = String::new();
            let mut staged_canvas: Option<Json> = None;

            // AI1.CONTEXT-CACHE (1): every provider exchange below is recorded for
            // the running session. The turn loop does not yet receive a session id; the
            // turn-to-session wiring (and the event log's turn id) arrives with AI1.ASSIST-TURN,
            // so the branch in the loop is inert until then.
            let ai_session: Option<i64> = None;
            let turn_id: Option<erp_sdk::Uuid> = match ai_session {
                Some(_) => match context_cache::new_turn_id() {
                    Ok(id) => Some(id),
                    Err(error) => {
                        tracing::warn!(%error, "assistant context cache could not generate a turn id");
                        None
                    }
                },
                None => None,
            };

            const MAX_TURNS: usize = 6;
            // An unusable final reply (no text, no canvas) is sent back once per correction round
            // so the model can fix its own answer before anything reaches the user; the rounds are
            // a declared setting. Only when they are exhausted does the turn refuse loudly.
            const CORRECTION_TEMPLATE: &str = "\
Your previous reply could not be rendered: it contained no readable text and no valid canvas. \
Answer the user's question again now with a complete plain-text answer, or call \
render_canvas with a valid payload. Do not apologize, do not mention this notice, and do not repeat the mistake.";
            let max_attempts = correction_rounds.saturating_add(1);
            let mut rounds_used: usize = 0;
            'attempts: for _attempt in 0..max_attempts {
                'rounds: for _turn_idx in 0..MAX_TURNS {
                    let req = ToolTurnRequest::new(route.model(), messages.clone(), tools.clone())
                        .map_err(invalid)?
                        .with_max_tokens(max_tokens);

                    let result = transport.complete_turn(&req).await;
                    let reply = match result {
                        Ok(r) => r,
                        Err(error) => {
                            // An unrepresentable provider reply (empty or malformed) is a render
                            // failure the model can fix: it takes the correction path below instead
                            // of failing the turn. Transport and server failures still fail the
                            // turn at once.
                            if matches!(error, erp_sdk::ai::ProviderError::InvalidResponse { .. }) {
                                break 'rounds;
                            }
                            let _ = auth
                                .orm()
                                .run(user, Context::new(), async move |tx| {
                                    reserved.settle(tx, None).await
                                })
                                .await;
                            return Err(Error::user(format!("erp.ai.provider_failed: {error}")));
                        }
                    };

                    total_prompt_tokens =
                        total_prompt_tokens.saturating_add(u64::from(reply.usage().prompt_tokens));
                    total_completion_tokens = total_completion_tokens
                        .saturating_add(u64::from(reply.usage().completion_tokens));
                    last_model = reply.usage().model.clone();
                    rounds_used = rounds_used.saturating_add(1);

                    // AI1.CONTEXT-CACHE (1): record this exchange's bytes and prefix
                    // hash for the session. The request is re-encoded with the same deterministic
                    // inputs the transport sent; the turn carries no labelled data yet, so the
                    // bytes are identical to what the provider received. The stable prefix is the
                    // instruction template today; the assembled prefix (template, tool list, skill
                    // index, memory,.2) replaces these bytes with AI1.ASSIST-TURN. The
                    // raw response body is not exposed by the transport yet and is recorded when
                    // the turn wiring lands.
                    if let (Some(session_id), Some(turn_id)) = (ai_session, turn_id) {
                        let request_bytes = route
                            .request_encoder()
                            .ok()
                            .and_then(|encoder| encoder.encode(&req).ok())
                            .and_then(|value| serde_json::to_vec(&value).ok());
                        if let Some(request_bytes) = request_bytes {
                            let round = i32::try_from(_turn_idx)
                                .ok()
                                .and_then(|index| index.checked_add(1))
                                .ok_or_else(|| {
                                    Error::programming(
                                        "assistant round index cannot be represented",
                                    )
                                })?;
                            let prefix = ASSISTANT_SYSTEM_PROMPT.as_bytes().to_vec();
                            let input_tokens = i64::from(reply.usage().prompt_tokens);
                            let output_tokens = i64::from(reply.usage().completion_tokens);
                            let recorded = auth
                                .orm()
                                .run(user, Context::new(), async move |tx| {
                                    let exchange = context_cache::Exchange {
                                        turn_id,
                                        round,
                                        prefix: &prefix,
                                        request: &request_bytes,
                                        response: None,
                                        input_tokens: Some(input_tokens),
                                        output_tokens: Some(output_tokens),
                                    };
                                    context_cache::record_exchange(tx, session_id, &exchange).await
                                })
                                .await;
                            if let Err(error) = recorded {
                                tracing::warn!(
                                    %error, session_id, round,
                                    "assistant context cache refused this exchange"
                                );
                            }
                        } else {
                            tracing::warn!(
                                "assistant context cache could not re-encode the sent request"
                            );
                        }
                    }

                    messages.push(
                        TurnMessage::new(TurnRole::Assistant, reply.parts().to_vec())
                            .map_err(invalid)?,
                    );

                    let mut current_turn_text = String::new();
                    let mut tool_calls = Vec::new();
                    for part in reply.parts() {
                        match part {
                            ContentPart::AssistantText(chunk) => {
                                if !current_turn_text.is_empty()
                                    && !current_turn_text.ends_with('\n')
                                    && !current_turn_text.ends_with(' ')
                                {
                                    current_turn_text.push('\n');
                                }
                                current_turn_text.push_str(chunk);
                                if current_turn_text.len() > 65536 {
                                    current_turn_text.truncate(65536);
                                }
                            }
                            ContentPart::ToolCall(call) => {
                                tool_calls.push(call.clone());
                            }
                            _ => {}
                        }
                    }

                    if tool_calls.is_empty() {
                        if !current_turn_text.trim().is_empty() {
                            final_text = current_turn_text;
                        }
                        break;
                    } else if !current_turn_text.trim().is_empty() {
                        text = current_turn_text;
                    }

                    const MAX_TOOL_CALLS_PER_TURN: usize = 8;
                    if tool_calls.len() > MAX_TOOL_CALLS_PER_TURN {
                        tool_calls.truncate(MAX_TOOL_CALLS_PER_TURN);
                    }

                    let mut tool_results = Vec::new();
                    for call in tool_calls {
                        let tool_result = match call.name() {
                            "find_models" => {
                                let mut tx = auth.begin_session(secret, options()).await?.tx;
                                let r = catalog::find_models(&mut tx, &call).await;
                                tx.rollback().await.ok();
                                r
                            }
                            "search_records" => {
                                assistant_read::search_records(
                                    &service,
                                    auth.clone(),
                                    secret_arc.clone(),
                                    company,
                                    &call,
                                )
                                .await
                            }
                            "aggregate_records" => {
                                assistant_read::aggregate_records(
                                    &service,
                                    auth.clone(),
                                    secret_arc.clone(),
                                    company,
                                    &call,
                                )
                                .await
                            }
                            "render_canvas" => {
                                let res = canvas::render_canvas(&call);
                                if res.is_ok()
                                    && let Ok(payload) = canvas::extract_canvas_payload(&call)
                                {
                                    staged_canvas = Some(payload);
                                }
                                res
                            }
                            other => Err(Error::validation(format!(
                                "Unknown assistant tool '{other}'."
                            ))),
                        };

                        match tool_result {
                            Ok(result) => {
                                tool_results.push(ContentPart::ToolResult(result));
                            }
                            Err(err) => {
                                let tool_name = ToolName::new(call.name())
                                    .or_else(|_| ToolName::new("unknown"))
                                    .map_err(|e| Error::programming(e.to_string()))?;
                                let source = DataSource::new(tool_name, None, Vec::new())
                                    .map_err(|e| Error::programming(e.to_string()))?;
                                let err_result = ToolResult::new(
                                    call.id().clone(),
                                    LabelledData::new(
                                        source,
                                        json!({
                                            "error": err.message()
                                        }),
                                    ),
                                    true,
                                );
                                tool_results.push(ContentPart::ToolResult(err_result));
                            }
                        }
                    }

                    messages.push(TurnMessage::new(TurnRole::User, tool_results).map_err(invalid)?);
                }
                if !final_text.trim().is_empty()
                    || !text.trim().is_empty()
                    || staged_canvas.is_some()
                {
                    break 'attempts;
                }
                // The final reply of this attempt is unusable: tell the model what is wrong and
                // give it one more round, within the declared correction budget.
                messages.push(
                    TurnMessage::new(
                        TurnRole::User,
                        vec![ContentPart::UserText(UserText::new(
                            CORRECTION_TEMPLATE.to_string(),
                        ))],
                    )
                    .map_err(invalid)?,
                );
            }

            // AI1.SESSION-COMPACT (5, D4.6): after the turn's events are appended,
            // compaction runs when the session is past its threshold — one summarize call inside
            // the turn's remaining caps, folded into the single reservation settle below so it
            // stays inside the turn's budget — and the session title is made on the same in-turn
            // path, each on its own route (routing::resolve_task; a missing route logs and keeps
            // the answer). The turn-to-session wiring (the session id, the turn id
            // and the event appends) is AI1.ASSIST-TURN; until it lands, ai_session stays None
            // and this call site is inert.
            if let (Some(session_id), Some(in_turn)) =
                (ai_session, turn_id.map(compact::InTurn::begin))
            {
                let caps = compact::TurnCaps {
                    round_trips_left: MAX_TURNS.saturating_sub(rounds_used),
                };
                // Compaction: one summarize exchange within the turn's remaining caps, then the
                // title on its own route (D4.6); the usage of each is folded into the settle
                // below so both stay inside the turn's budget.
                match auth.begin_session(secret, options()).await {
                    Ok(mut begun) => {
                        let summarize_route = routing::resolve_task(
                            &begun.tx.env(),
                            &mut begun.tx,
                            &self.credentials,
                            "summarize",
                        )
                        .await;
                        match summarize_route {
                            Ok(route) => match route.transport(&self.credentials) {
                                Ok(transport) => {
                                    let summarizer =
                                        compact::RouteTransport::new(&route, transport);
                                    match compact::compact_in_turn(
                                        &mut begun.tx,
                                        session_id,
                                        Some(&in_turn),
                                        &caps,
                                        &summarizer,
                                    )
                                    .await
                                    {
                                        Ok(compact::CompactOutcome::Compacted {
                                            from_seq,
                                            to_seq,
                                            input_tokens,
                                            output_tokens,
                                        }) => {
                                            tracing::info!(
                                                session_id,
                                                from_seq,
                                                to_seq,
                                                "assistant session compacted"
                                            );
                                            total_prompt_tokens = total_prompt_tokens
                                                .saturating_add(
                                                    u64::try_from(input_tokens.unwrap_or(0))
                                                        .unwrap_or(0),
                                                );
                                            total_completion_tokens = total_completion_tokens
                                                .saturating_add(
                                                    u64::try_from(output_tokens.unwrap_or(0))
                                                        .unwrap_or(0),
                                                );
                                            if let Err(error) = begun.tx.commit().await {
                                                tracing::warn!(%error, session_id, "assistant compaction commit failed");
                                            }
                                        }
                                        Ok(compact::CompactOutcome::Pending) => {
                                            tracing::debug!(
                                                session_id,
                                                "assistant compaction deferred to the next turn"
                                            );
                                            begun.tx.rollback().await.ok();
                                        }
                                        Ok(compact::CompactOutcome::NotNeeded) => {
                                            begun.tx.rollback().await.ok();
                                        }
                                        Err(error) => {
                                            tracing::warn!(%error, session_id, "assistant compaction refused");
                                            begun.tx.rollback().await.ok();
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "assistant summarize transport unavailable");
                                    begun.tx.rollback().await.ok();
                                }
                            },
                            Err(error) => {
                                tracing::warn!(%error, "assistant summarize route missing");
                                begun.tx.rollback().await.ok();
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "assistant summarize route could not be resolved");
                    }
                }
                // The title uses the same in-turn path on the title route (D4.6) and shares the
                // turn's remaining round trips with compaction: it sees one fewer so both calls
                // together never exceed the caps.
                let title_caps = compact::TurnCaps {
                    round_trips_left: caps.round_trips_left.saturating_sub(1),
                };
                match auth.begin_session(secret, options()).await {
                    Ok(mut begun) => {
                        let title_route = routing::resolve_task(
                            &begun.tx.env(),
                            &mut begun.tx,
                            &self.credentials,
                            "title",
                        )
                        .await;
                        match title_route {
                            Ok(route) => match route.transport(&self.credentials) {
                                Ok(transport) => {
                                    let summarizer =
                                        compact::RouteTransport::new(&route, transport);
                                    match compact::title_in_turn(
                                        &mut begun.tx,
                                        session_id,
                                        Some(&in_turn),
                                        &title_caps,
                                        &summarizer,
                                    )
                                    .await
                                    {
                                        Ok(compact::TitleOutcome::Titled {
                                            title: _,
                                            input_tokens,
                                            output_tokens,
                                        }) => {
                                            tracing::info!(session_id, "assistant session titled");
                                            total_prompt_tokens = total_prompt_tokens
                                                .saturating_add(
                                                    u64::try_from(input_tokens.unwrap_or(0))
                                                        .unwrap_or(0),
                                                );
                                            total_completion_tokens = total_completion_tokens
                                                .saturating_add(
                                                    u64::try_from(output_tokens.unwrap_or(0))
                                                        .unwrap_or(0),
                                                );
                                            if let Err(error) = begun.tx.commit().await {
                                                tracing::warn!(%error, session_id, "assistant compaction commit failed");
                                            }
                                        }
                                        Ok(compact::TitleOutcome::Pending) => {
                                            tracing::debug!(
                                                session_id,
                                                "assistant title deferred to the next turn"
                                            );
                                            begun.tx.rollback().await.ok();
                                        }
                                        Ok(compact::TitleOutcome::NotNeeded) => {
                                            begun.tx.rollback().await.ok();
                                        }
                                        Err(error) => {
                                            tracing::warn!(%error, session_id, "assistant title refused");
                                            begun.tx.rollback().await.ok();
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "assistant title transport unavailable");
                                    begun.tx.rollback().await.ok();
                                }
                            },
                            Err(error) => {
                                tracing::warn!(%error, "assistant title route missing");
                                begun.tx.rollback().await.ok();
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "assistant title route could not be resolved");
                    }
                }
            }

            let usage = Some((
                i64::try_from(total_prompt_tokens).unwrap_or(i64::MAX),
                i64::try_from(total_completion_tokens).unwrap_or(i64::MAX),
            ));

            auth.orm()
                .run(user, Context::new(), async move |tx| {
                    let reservation = reserved;
                    reservation.settle(tx, usage).await
                })
                .await?;

            let mut check = auth.begin_session(secret, options()).await?;
            let current = check.tx.env();
            if !check.tx.has_group(&current, "base.group_user").await? {
                return Err(Error::access("An internal session is required."));
            }
            check.tx.rollback().await?;

            let output_text = if !final_text.trim().is_empty() {
                final_text
            } else if !text.trim().is_empty() {
                text
            } else if staged_canvas.is_some() {
                "Visualisasi telah disiapkan di canvas.".to_string()
            } else {
                String::new()
            };

            if output_text.trim().is_empty() && staged_canvas.is_none() {
                return Err(Error::user(
                    "erp.ai.provider_failed: the provider returned no text or canvas.",
                ));
            }

            let mut resp = json!({
                "text": output_text,
                "model": last_model,
                "usage": {
                    "input_tokens": total_prompt_tokens,
                    "output_tokens": total_completion_tokens
                }
            });
            if let Some(canvas) = staged_canvas {
                resp["canvas"] = canvas;
            }

            Ok(resp)
        })
    }
}

/// Re-validate the tab-supplied transcript against the declared bounds before any entry reaches
/// a provider. Refuse, never truncate: the client trims to the same policy, so an oversized
/// submission is stale or hostile and gets a named error instead of a silent cut.
fn validate_history(
    history: &[erp_server::web::HistoryEntry],
    turns: usize,
    bytes: usize,
    message_bytes: usize,
) -> std::result::Result<(), String> {
    use erp_server::web::HistoryRole;
    if history.len() > turns.saturating_mul(2) {
        return Err(format!("history exceeds {turns} exchanges."));
    }
    if history
        .first()
        .is_some_and(|entry| entry.role() != HistoryRole::User)
    {
        return Err("history must begin with a question.".to_string());
    }
    // The provider protocols cannot represent adjacent messages of one role (the Anthropic
    // family refuses them outright), so the transcript must alternate strictly.
    for pair in history.windows(2) {
        if pair[0].role() == pair[1].role() {
            return Err("history must alternate between questions and replies.".to_string());
        }
    }
    let serialized =
        serde_json::to_vec(history).map_err(|_| "history could not be encoded.".to_string())?;
    if serialized.len() > bytes {
        return Err("history exceeds the conversation byte limit.".to_string());
    }
    if history
        .iter()
        .any(|entry| entry.text().len() > message_bytes || entry.text().trim().is_empty())
    {
        return Err(
            "one or more earlier messages exceed the message length limit or are empty."
                .to_string(),
        );
    }
    Ok(())
}
