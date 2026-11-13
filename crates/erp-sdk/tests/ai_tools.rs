// SPDX-License-Identifier: MIT OR Apache-2.0
//! Every `ToolTypeError` variant (kind and full message) and the accepted boundaries of the
//! tool-calling types in `erp_sdk::ai::tools` (AI1.SDK-TOOLS).
#![allow(clippy::expect_used, clippy::panic, reason = "acceptance tests")]

use erp_sdk::ai::tools::{
    TOOL_CALL_ID_MAX_BYTES, TOOL_CALL_NAME_MAX_BYTES, TOOL_DESCRIPTION_MAX_CHARS,
    TOOL_NAME_MAX_BYTES,
};
use erp_sdk::ai::{
    ContentPart, DataSource, FinishReason, Instruction, LabelledData, ProviderError, ToolCall,
    ToolCallId, ToolName, ToolResult, ToolSpec, ToolTurnRequest, ToolTurnResponse, ToolTypeError,
    TurnMessage, TurnRole, UsageRecord, UserText,
};
use serde_json::json;

fn schema(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map,
        other => panic!("schema literal is not an object: {other}"),
    }
}

fn name(text: &str) -> ToolName {
    ToolName::new(text).expect("valid tool name")
}

fn id(text: &str) -> ToolCallId {
    ToolCallId::new(text).expect("valid id")
}

fn data() -> LabelledData {
    LabelledData::new(
        DataSource::new(name("search_read"), None, Vec::new()).expect("source"),
        json!(null),
    )
}

fn call(call_id: &str) -> ContentPart {
    ContentPart::ToolCall(
        ToolCall::new(id(call_id), "search_read".to_owned(), json!({})).expect("call"),
    )
}

fn result(call_id: &str) -> ContentPart {
    ContentPart::ToolResult(ToolResult::new(id(call_id), data(), false))
}

fn user(parts: Vec<ContentPart>) -> TurnMessage {
    TurnMessage::new(TurnRole::User, parts).expect("user message")
}

fn assistant(parts: Vec<ContentPart>) -> TurnMessage {
    TurnMessage::new(TurnRole::Assistant, parts).expect("assistant message")
}

fn question() -> TurnMessage {
    user(vec![ContentPart::UserText(UserText::new("q".to_owned()))])
}

fn usage() -> UsageRecord {
    UsageRecord::new("unit", "m", 1, 1)
}

#[test]
fn tool_name_accepts_the_portable_alphabet_up_to_64_bytes() {
    let longest = "a".repeat(TOOL_NAME_MAX_BYTES);
    assert_eq!(name(&longest).as_str(), longest);
    assert_eq!(name("search_read-2").as_str(), "search_read-2");
    let json = serde_json::to_string(&name("find_models")).expect("serialize");
    assert_eq!(json, "\"find_models\"");
    let back: ToolName = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, name("find_models"));
}

#[test]
fn invalid_tool_name() {
    let too_long = "a".repeat(TOOL_NAME_MAX_BYTES + 1);
    for (bad, len) in [("", 0), ("search.read", 11), ("café", 5), ("a b", 3)] {
        let err = ToolName::new(bad).expect_err("refused");
        assert_eq!(
            err,
            ToolTypeError::InvalidToolName {
                name: bad.to_owned(),
                len
            }
        );
        assert_eq!(
            err.to_string(),
            format!(
                "tool name '{bad}' ({len} bytes) is not 1 to 64 characters from A-Z, a-z, 0-9, '_' and '-'"
            )
        );
    }
    // A long refused name is reported cut to 64 characters, with its full byte length.
    let err = ToolName::new(&too_long).expect_err("refused");
    let cut = "a".repeat(TOOL_NAME_MAX_BYTES);
    assert_eq!(
        err,
        ToolTypeError::InvalidToolName {
            name: cut.clone(),
            len: 65
        }
    );
    assert_eq!(
        err.to_string(),
        format!(
            "tool name '{cut}' (65 bytes) is not 1 to 64 characters from A-Z, a-z, 0-9, '_' and '-'"
        )
    );
    let huge = "é".repeat(10_000);
    let err = ToolName::new(&huge).expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::InvalidToolName {
            name: "é".repeat(TOOL_NAME_MAX_BYTES),
            len: 20_000
        }
    );
    // A refused name is escaped before it is quoted: a right-to-left override and a newline do
    // not reach the message raw.
    let err = ToolName::new("\u{202E}x\ny").expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::InvalidToolName {
            name: "\\u{202e}x\\ny".to_owned(),
            len: 6
        }
    );
    assert_eq!(
        err.to_string(),
        "tool name '\\u{202e}x\\ny' (6 bytes) is not 1 to 64 characters from A-Z, a-z, 0-9, '_' and '-'"
    );
    let err = serde_json::from_str::<ToolName>("\"no.dots\"").expect_err("refused");
    assert_eq!(
        err.to_string(),
        "tool name 'no.dots' (7 bytes) is not 1 to 64 characters from A-Z, a-z, 0-9, '_' and '-'"
    );
}

#[test]
fn empty_tool_description() {
    for blank in ["", " \n\t"] {
        let err = ToolSpec::new(name("t"), blank, schema(json!({"type": "object"})))
            .expect_err("refused");
        assert_eq!(
            err,
            ToolTypeError::EmptyToolDescription {
                tool: "t".to_owned()
            }
        );
        assert_eq!(err.to_string(), "tool 't' has an empty description");
    }
}

#[test]
fn tool_description_too_long_counts_characters() {
    // 1024 two-byte characters pass (2048 bytes): the limit is in characters.
    let longest = "é".repeat(TOOL_DESCRIPTION_MAX_CHARS);
    let spec = ToolSpec::new(
        name("t"),
        longest.clone(),
        schema(json!({"type": "object"})),
    )
    .expect("1024 characters are allowed");
    assert_eq!(spec.description(), longest);
    let err = ToolSpec::new(
        name("t"),
        "é".repeat(TOOL_DESCRIPTION_MAX_CHARS + 1),
        schema(json!({"type": "object"})),
    )
    .expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::ToolDescriptionTooLong {
            tool: "t".to_owned(),
            chars: TOOL_DESCRIPTION_MAX_CHARS + 1
        }
    );
    assert_eq!(
        err.to_string(),
        "tool 't' description has 1025 characters, more than the 1024 allowed"
    );
}

#[test]
fn tool_schema_not_object() {
    let err = ToolSpec::new(name("t"), "d", schema(json!({"type": "array"}))).expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::ToolSchemaNotObject {
            tool: "t".to_owned(),
            found: "\"array\"".to_owned()
        }
    );
    assert_eq!(
        err.to_string(),
        "tool 't' input schema must have \"type\": \"object\", found \"array\""
    );
    let err =
        ToolSpec::new(name("t"), "d", schema(json!({"properties": {}}))).expect_err("refused");
    assert_eq!(
        err.to_string(),
        "tool 't' input schema must have \"type\": \"object\", found no \"type\" key"
    );
    assert!(matches!(err, ToolTypeError::ToolSchemaNotObject { .. }));
}

#[test]
fn empty_tool_call_id() {
    let err = ToolCallId::new("").expect_err("refused");
    assert_eq!(err, ToolTypeError::EmptyToolCallId);
    assert_eq!(err.to_string(), "tool call id is empty");
}

#[test]
fn tool_call_id_too_long_counts_bytes() {
    // 128 two-byte characters are 256 bytes: allowed; one more character is not.
    let longest = "é".repeat(TOOL_CALL_ID_MAX_BYTES / 2);
    assert_eq!(id(&longest).as_str(), longest);
    let err = ToolCallId::new(&format!("{longest}é")).expect_err("refused");
    assert_eq!(err, ToolTypeError::ToolCallIdTooLong { len: 258 });
    assert_eq!(
        err.to_string(),
        "tool call id has 258 bytes, more than the 256 allowed"
    );
}

#[test]
fn tool_call_id_control_char() {
    let err = ToolCallId::new("call\u{e9}\r\n").expect_err("refused");
    assert_eq!(err, ToolTypeError::ToolCallIdControlChar { index: 6 });
    assert_eq!(
        err.to_string(),
        "tool call id has a control or format character at byte 6"
    );
    let err = serde_json::from_str::<ToolCallId>("\"a\\u0000\"").expect_err("refused");
    assert_eq!(
        err.to_string(),
        "tool call id has a control or format character at byte 1"
    );
    // Format (Cf) characters: right-to-left override, zero-width space, byte-order mark, a tag.
    for (raw, index) in [
        ("ab\u{202E}cd", 2),
        ("\u{200B}x", 0),
        ("x\u{FEFF}", 1),
        ("id\u{E0041}", 2),
        // Line and paragraph separators (Zl, Zp) break log lines too.
        ("a\u{2028}", 1),
        ("\u{2029}b", 0),
    ] {
        let err = ToolCallId::new(raw).expect_err("refused");
        assert_eq!(err, ToolTypeError::ToolCallIdControlChar { index });
        assert_eq!(
            err.to_string(),
            format!("tool call id has a control or format character at byte {index}")
        );
    }
    // Visible non-ASCII is fine.
    assert_eq!(id("call-é-ü").as_str(), "call-é-ü");
}

#[test]
fn tool_call_keeps_what_the_model_sent() {
    let raw = json!({"id": "c1", "name": "not a tool!", "arguments": [1, "two"]});
    let call: ToolCall = serde_json::from_value(raw).expect("received as is");
    assert_eq!(call.id().as_str(), "c1");
    assert_eq!(call.name(), "not a tool!");
    assert_eq!(call.arguments(), &json!([1, "two"]));
}

#[test]
fn tool_call_name_too_long() {
    let longest = "n".repeat(TOOL_CALL_NAME_MAX_BYTES);
    let call = ToolCall::new(id("c1"), longest.clone(), json!({})).expect("256 bytes allowed");
    assert_eq!(call.name(), longest);
    let err = ToolCall::new(id("c1"), format!("{longest}n"), json!({})).expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::ToolCallNameTooLong {
            id: "c1".to_owned(),
            len: 257
        }
    );
    assert_eq!(
        err.to_string(),
        "tool call 'c1' names a tool of 257 bytes, more than the 256 allowed"
    );
    let raw = json!({"id": "c1", "name": "n".repeat(257), "arguments": {}});
    let err = serde_json::from_value::<ToolCall>(raw).expect_err("refused");
    assert_eq!(
        err.to_string(),
        "tool call 'c1' names a tool of 257 bytes, more than the 256 allowed"
    );
}

#[test]
fn empty_data_source_model() {
    let err =
        DataSource::new(name("search_read"), Some(String::new()), vec![1]).expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::EmptyDataSourceModel {
            tool: "search_read".to_owned()
        }
    );
    assert_eq!(
        err.to_string(),
        "data source of tool 'search_read' has an empty model name"
    );
}

#[test]
fn instruction_renders_its_typed_pieces() {
    let text = Instruction::from_static("limit ")
        .then_integer(i64::MIN)
        .then_static(", draft ")
        .then_flag(false);
    assert_eq!(text.render(), format!("limit {}, draft false", i64::MIN));
}

#[test]
fn empty_message() {
    let err = TurnMessage::new(TurnRole::Assistant, Vec::new()).expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::EmptyMessage {
            role: TurnRole::Assistant
        }
    );
    assert_eq!(err.to_string(), "assistant message has no parts");
}

#[test]
fn part_not_allowed() {
    let instruction = ContentPart::Instruction(Instruction::from_static("x"));
    let err = TurnMessage::new(
        TurnRole::User,
        vec![
            ContentPart::UserText(UserText::new("u".to_owned())),
            instruction,
        ],
    )
    .expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::PartNotAllowed {
            role: TurnRole::User,
            index: 1,
            part: "instruction"
        }
    );
    assert_eq!(
        err.to_string(),
        "user message cannot carry a part of kind 'instruction' (part 1)"
    );
    let err =
        TurnMessage::new(TurnRole::System, vec![ContentPart::Data(data())]).expect_err("refused");
    assert_eq!(
        err.to_string(),
        "system message cannot carry a part of kind 'data' (part 0)"
    );
    let err = TurnMessage::new(TurnRole::Assistant, vec![result("c1")]).expect_err("refused");
    assert_eq!(
        err.to_string(),
        "assistant message cannot carry a part of kind 'tool result' (part 0)"
    );
}

#[test]
fn duplicate_tool_name() {
    let spec = ToolSpec::new(name("t"), "d", schema(json!({"type": "object"}))).expect("spec");
    let err =
        ToolTurnRequest::new("m", vec![question()], vec![spec.clone(), spec]).expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::DuplicateToolName {
            name: "t".to_owned()
        }
    );
    assert_eq!(err.to_string(), "tool 't' is declared twice in one request");
}

#[test]
fn duplicate_tool_call_id() {
    // Within one message and across messages.
    let err = ToolTurnRequest::new(
        "m",
        vec![question(), assistant(vec![call("c1"), call("c1")])],
        Vec::new(),
    )
    .expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::DuplicateToolCallId {
            id: "c1".to_owned(),
            message: 1
        }
    );
    assert_eq!(err.to_string(), "message 1 repeats tool call id 'c1'");
    let err = ToolTurnRequest::new(
        "m",
        vec![
            question(),
            assistant(vec![call("c1")]),
            user(vec![result("c1")]),
            assistant(vec![call("c1")]),
        ],
        Vec::new(),
    )
    .expect_err("refused");
    assert_eq!(err.to_string(), "message 3 repeats tool call id 'c1'");
}

#[test]
fn unknown_tool_result() {
    // A result before its call is as unknown as a result for no call at all.
    let err = ToolTurnRequest::new(
        "m",
        vec![user(vec![result("c1")]), assistant(vec![call("c1")])],
        Vec::new(),
    )
    .expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::UnknownToolResult {
            id: "c1".to_owned(),
            message: 0
        }
    );
    assert_eq!(
        err.to_string(),
        "message 0 carries a tool result for 'c1', which no earlier assistant message called"
    );
}

#[test]
fn duplicate_tool_result() {
    let err = ToolTurnRequest::new(
        "m",
        vec![
            question(),
            assistant(vec![call("c1")]),
            user(vec![result("c1")]),
            user(vec![result("c1")]),
        ],
        Vec::new(),
    )
    .expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::DuplicateToolResult {
            id: "c1".to_owned(),
            message: 3
        }
    );
    assert_eq!(
        err.to_string(),
        "message 3 carries a second tool result for 'c1'"
    );
}

#[test]
fn a_valid_conversation_with_two_calls_is_accepted() {
    let request = ToolTurnRequest::new(
        "m",
        vec![
            question(),
            assistant(vec![
                ContentPart::AssistantText("checking".to_owned()),
                call("c1"),
                call("c2"),
            ]),
            user(vec![result("c2"), result("c1")]),
        ],
        Vec::new(),
    )
    .expect("valid")
    .with_max_tokens(64)
    .with_temperature(0.5);
    assert_eq!(request.messages().len(), 3);
    assert_eq!(request.max_tokens(), Some(64));
    assert_eq!(request.temperature(), Some(0.5));
}

#[test]
fn response_part_not_assistant() {
    let err = ToolTurnResponse::new(
        "m",
        vec![call("c1"), ContentPart::Data(data())],
        usage(),
        Some(FinishReason::ToolUse),
    )
    .expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::ResponsePartNotAssistant {
            index: 1,
            part: "data"
        }
    );
    assert_eq!(
        err.to_string(),
        "response cannot carry a part of kind 'data' (part 1)"
    );
}

#[test]
fn empty_response() {
    let err = ToolTurnResponse::new("m", Vec::new(), usage(), Some(FinishReason::Stop))
        .expect_err("refused");
    assert_eq!(err, ToolTypeError::EmptyResponse);
    assert_eq!(err.to_string(), "response has no parts");
}

#[test]
fn duplicate_response_tool_call_id() {
    let err = ToolTurnResponse::new(
        "m",
        vec![call("c1"), call("c1")],
        usage(),
        Some(FinishReason::ToolUse),
    )
    .expect_err("refused");
    assert_eq!(
        err,
        ToolTypeError::DuplicateResponseToolCallId {
            id: "c1".to_owned()
        }
    );
    assert_eq!(err.to_string(), "response repeats tool call id 'c1'");
}

#[test]
fn invalid_response_names_the_provider() {
    let err = ProviderError::InvalidResponse {
        provider: "local".to_owned(),
        message: "tool call id is empty".to_owned(),
    };
    assert_eq!(
        err.to_string(),
        "provider 'local' invalid response: tool call id is empty"
    );
}

/// `true` when `$ty` implements `$tr`, decided at compile time without a compile-fail case: the
/// inherent constant of `Probe<T>` exists only when the bound holds and wins over the blanket
/// trait constant. Used for traits whose compile-fail message lists every other implementor,
/// which depends on the crate graph of the host that renders it.
macro_rules! implements {
    ($ty:ty: $tr:path) => {{
        #[allow(dead_code)]
        trait Fallback {
            const IMPLEMENTS: bool = false;
        }
        impl<T: ?Sized> Fallback for T {}
        struct Probe<T: ?Sized>(std::marker::PhantomData<T>);
        #[allow(dead_code)]
        impl<T: ?Sized + $tr> Probe<T> {
            const IMPLEMENTS: bool = true;
        }
        <Probe<$ty>>::IMPLEMENTS
    }};
}

/// Neither `Instruction` nor `LabelledData` can be parsed or deserialized from text; the probe
/// itself is shown to detect both traits on types that do implement them.
#[test]
fn instruction_and_labelled_data_cannot_be_parsed_or_deserialized() {
    assert!(implements!(ToolCall: serde::de::DeserializeOwned));
    assert!(implements!(i64: std::str::FromStr));
    assert!(!implements!(Instruction: std::str::FromStr));
    assert!(!implements!(Instruction: serde::de::DeserializeOwned));
    assert!(!implements!(LabelledData: std::str::FromStr));
    assert!(!implements!(LabelledData: serde::de::DeserializeOwned));
    assert!(!implements!(LabelledData: From<serde_json::Value>));
    assert!(!implements!(LabelledData: TryFrom<String>));
    assert!(!implements!(Instruction: From<&'static str>));
    assert!(!implements!(Instruction: From<Box<str>>));
    assert!(!implements!(Instruction: From<UserText>));
    assert!(!implements!(Instruction: TryFrom<String>));
    // The probe sees a conversion where one exists.
    assert!(implements!(ToolName: TryFrom<String>));
}
