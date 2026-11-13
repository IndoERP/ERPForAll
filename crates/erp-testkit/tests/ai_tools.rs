// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tool calls through the provider interface with the scripted mock (AI1.SDK-TOOLS).
#![allow(clippy::expect_used, clippy::panic, reason = "acceptance tests")]

use erp_sdk::ai::{
    CompletionRequest, ContentPart, DataSource, FinishReason, Instruction, LabelledData,
    ModelProvider, ProviderError, ToolCall, ToolCallId, ToolModelProvider, ToolName, ToolResult,
    ToolSpec, ToolTurnRequest, ToolTypeError, TurnMessage, TurnRole, UserText,
};
use erp_testkit::ai::{ScriptedModelProvider, ScriptedReply};
use serde_json::json;

fn search_read_spec() -> ToolSpec {
    let schema = json!({
        "type": "object",
        "properties": {"model": {"type": "string"}, "limit": {"type": "integer"}},
        "required": ["model"],
        "additionalProperties": false
    });
    let serde_json::Value::Object(schema) = schema else {
        panic!("the schema literal is an object");
    };
    ToolSpec::new(
        ToolName::new("search_read").expect("valid tool name"),
        "Read records of one model.",
        schema,
    )
    .expect("valid tool spec")
}

fn first_turn() -> Vec<TurnMessage> {
    vec![
        TurnMessage::new(
            TurnRole::System,
            vec![ContentPart::Instruction(Instruction::from_static(
                "Answer with the tools you are given.",
            ))],
        )
        .expect("system message"),
        TurnMessage::new(
            TurnRole::User,
            vec![ContentPart::UserText(UserText::new(
                "How many customers in Jakarta?".to_owned(),
            ))],
        )
        .expect("user message"),
    ]
}

fn only_tool_call(parts: &[ContentPart]) -> &ToolCall {
    match parts {
        [ContentPart::ToolCall(call)] => call,
        other => panic!("expected exactly one tool call, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_call_round_trips_through_the_provider_interface() {
    let provider = ScriptedModelProvider::new("scripted-llm");
    let scripted_call = ScriptedReply::tool_call(
        "call_01",
        "search_read",
        json!({"model": "res.partner", "limit": 5}),
    )
    .expect("scripted tool call");
    provider.push_reply(scripted_call.clone());
    let scripted_text = ScriptedReply::text("There are 5 customers in Jakarta.").expect("text");
    provider.push_reply(scripted_text.clone());

    // Turn 1: tools sent, the call comes back with id, name and arguments intact.
    let first = ToolTurnRequest::new("test-model", first_turn(), vec![search_read_spec()])
        .expect("first request");
    let answer = provider.complete_turn(&first).await.expect("first turn");
    assert_eq!(answer, scripted_call);
    assert_eq!(answer.finish_reason(), Some(FinishReason::ToolUse));
    let sent = only_tool_call(scripted_call.parts());
    let received = only_tool_call(answer.parts());
    assert_eq!(received.id(), sent.id());
    assert_eq!(received.name(), sent.name());
    assert_eq!(received.arguments(), sent.arguments());
    assert_eq!(received.id().as_str(), "call_01");
    assert_eq!(received.name(), "search_read");
    assert_eq!(
        received.arguments(),
        &json!({"model": "res.partner", "limit": 5})
    );

    // Turn 2: the result goes back as a ToolResult part tied to the same id.
    let source = DataSource::new(
        ToolName::new(received.name()).expect("the called tool is a valid name"),
        Some("res.partner".to_owned()),
        vec![3, 4, 5, 8, 13],
    )
    .expect("data source");
    let data = LabelledData::new(source, json!({"count": 5}));
    let result = ToolResult::new(received.id().clone(), data.clone(), false);
    let mut messages = first_turn();
    messages.push(
        TurnMessage::new(TurnRole::Assistant, answer.parts().to_vec()).expect("assistant message"),
    );
    messages.push(
        TurnMessage::new(
            TurnRole::User,
            vec![ContentPart::ToolResult(result.clone())],
        )
        .expect("tool result message"),
    );
    let second = ToolTurnRequest::new("test-model", messages, vec![search_read_spec()])
        .expect("second request");
    let final_answer = provider.complete_turn(&second).await.expect("second turn");
    assert_eq!(final_answer, scripted_text);
    assert_eq!(provider.remaining(), 0, "both scripted replies were used");

    let recorded = provider.recorded_requests();
    assert_eq!(recorded, vec![first.clone(), second.clone()]);
    assert_eq!(recorded[0].tools(), first.tools());
    let returned = match recorded[1].messages()[3].parts() {
        [ContentPart::ToolResult(returned)] => returned,
        other => panic!("expected the tool result part, got {other:?}"),
    };
    assert_eq!(returned.id(), received.id());
    assert_eq!(returned.data(), &data);
    assert!(!returned.is_error());

    // A text-only request through the same double still works unchanged.
    let text_only = CompletionRequest::new("test-model", "plain question");
    let echo = provider
        .complete(&text_only)
        .await
        .expect("text completion");
    assert_eq!(echo.text, format!("Echo: {}", text_only.prompt));
    assert_eq!(echo.finish_reason, Some(FinishReason::Stop));
    assert_eq!(provider.recorded_completions(), vec![text_only]);
}

#[test]
fn a_tool_result_for_an_unknown_call_is_refused() {
    let data = LabelledData::new(
        DataSource::new(
            ToolName::new("search_read").expect("name"),
            None,
            Vec::new(),
        )
        .expect("source"),
        json!({"count": 0}),
    );
    let stray = ToolCallId::new("call_99").expect("id");
    let mut messages = first_turn();
    messages.push(
        TurnMessage::new(
            TurnRole::User,
            vec![ContentPart::ToolResult(ToolResult::new(stray, data, false))],
        )
        .expect("user message"),
    );
    let err = ToolTurnRequest::new("test-model", messages, vec![search_read_spec()])
        .expect_err("a result without its call is refused");
    assert!(
        matches!(&err, ToolTypeError::UnknownToolResult { id, message } if id == "call_99" && *message == 2),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "message 2 carries a tool result for 'call_99', which no earlier assistant message called"
    );
}

#[tokio::test]
async fn scripted_errors_are_returned_and_an_exhausted_script_fails_loudly() {
    let provider = ScriptedModelProvider::new("scripted-llm");
    let scripted_error = ProviderError::RateLimitExceeded {
        provider: "scripted-llm".to_owned(),
        message: "slow down".to_owned(),
    };
    provider.push_error(scripted_error.clone());
    let request = ToolTurnRequest::new("test-model", first_turn(), Vec::new()).expect("request");

    let err = provider
        .complete_turn(&request)
        .await
        .expect_err("scripted error");
    assert_eq!(err, scripted_error);

    let err = provider
        .complete_turn(&request)
        .await
        .expect_err("exhausted script");
    assert!(matches!(&err, ProviderError::Backend { provider, .. } if provider == "scripted-llm"));
    assert_eq!(
        err.to_string(),
        "provider 'scripted-llm' backend error: the script is exhausted at call 2 (no reply queued)"
    );
    // Both calls were recorded, the failing one included.
    assert_eq!(provider.recorded_requests().len(), 2);
}
