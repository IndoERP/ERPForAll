// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tool calling in the AI provider interface (D4; task AI1.SDK-TOOLS).
//!
//! A tool turn is a conversation of [`TurnMessage`]s, each a list of typed [`ContentPart`]s, plus
//! the [`ToolSpec`]s the model may call. The part types keep apart what the system says
//! ([`Instruction`], built only from `&'static str` templates and typed values), what the user
//! typed ([`UserText`]), what a tool read from the database ([`LabelledData`], always labelled with
//! its source) and what the model produced ([`ToolCall`], assistant text). No conversion turns a
//! runtime string into an [`Instruction`] or any string into [`LabelledData`] (
//! partial).
//!
//! The types do not close every path; three are review rules for `ai_base`:
//! - a leaked `String` (`String::leak`) is a `&'static str` and passes
//!   [`Instruction::from_static`]: only string literals go there;
//! - any crate can call [`LabelledData::new`]: only the tool executor builds it;
//! - [`ContentPart::AssistantText`] holds any `String`: only provider responses supply assistant
//!   parts.
//!
//! A provider with tool calling implements [`ToolModelProvider`] on top of
//! [`ModelProvider`](super::ModelProvider); one without it simply does not.
//!
//! # Examples
//!
//! ```
//! use erp_sdk::ai::{
//!     ContentPart, Instruction, ToolName, ToolSpec, ToolTurnRequest, TurnMessage, TurnRole,
//!     UserText,
//! };
//!
//! let serde_json::Value::Object(schema) = serde_json::json!({"type": "object"}) else {
//!     unreachable!("the literal is an object")
//! };
//! let tool = ToolSpec::new(ToolName::new("find_models")?, "List the models you can read.", schema)?;
//! let messages = vec![
//!     TurnMessage::new(
//!         TurnRole::System,
//!         vec![ContentPart::Instruction(Instruction::from_static("Use the tools."))],
//!     )?,
//!     TurnMessage::new(
//!         TurnRole::User,
//!         vec![ContentPart::UserText(UserText::new("Which models hold invoices?".to_owned()))],
//!     )?,
//! ];
//! let request = ToolTurnRequest::new("local-model", messages, vec![tool])?.with_max_tokens(512);
//! assert_eq!(request.tools()[0].name().as_str(), "find_models");
//! # Ok::<(), erp_sdk::ai::ToolTypeError>(())
//! ```

use super::{FinishReason, ModelProvider, ProviderError, UsageRecord};
use erp_orm::BoxFuture;
use std::collections::HashSet;
use std::fmt;

/// Longest tool name, in bytes (the common limit of the Anthropic and OpenAI tool-name rules).
pub const TOOL_NAME_MAX_BYTES: usize = 64;
/// Longest tool description, in characters.
pub const TOOL_DESCRIPTION_MAX_CHARS: usize = 1_024;
/// Longest tool call id, in bytes.
pub const TOOL_CALL_ID_MAX_BYTES: usize = 256;
/// Longest tool name a [`ToolCall`] may carry as received from a model, in bytes.
pub const TOOL_CALL_NAME_MAX_BYTES: usize = 256;

/// A refused tool-calling value: each variant names one invariant of the types in this module and
/// the offending value or index. A refused call id or called name (model output) is reported by
/// length or position; a refused [`ToolName`] is echoed cut to its first 64 characters and escaped
/// (`str::escape_debug`), so control, line-separator and format characters never reach a log raw;
/// an accepted call id is bounded and free of control, line-separator and format characters, so
/// later errors quote it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolTypeError {
    /// A tool name is empty, longer than [`TOOL_NAME_MAX_BYTES`] or has a character outside
    /// `A-Z a-z 0-9 _ -`.
    #[error(
        "tool name '{name}' ({len} bytes) is not 1 to {max} characters from A-Z, a-z, 0-9, '_' and '-'",
        max = TOOL_NAME_MAX_BYTES
    )]
    InvalidToolName {
        /// The refused name, cut to its first [`TOOL_NAME_MAX_BYTES`] characters, then escaped
        /// with `str::escape_debug`.
        name: String,
        /// Length of the whole refused name in bytes.
        len: usize,
    },
    /// A tool description is empty or only whitespace.
    #[error("tool '{tool}' has an empty description")]
    EmptyToolDescription {
        /// The tool.
        tool: String,
    },
    /// A tool description is longer than [`TOOL_DESCRIPTION_MAX_CHARS`].
    #[error(
        "tool '{tool}' description has {chars} characters, more than the {max} allowed",
        max = TOOL_DESCRIPTION_MAX_CHARS
    )]
    ToolDescriptionTooLong {
        /// The tool.
        tool: String,
        /// Its length in characters.
        chars: usize,
    },
    /// A tool input schema does not have `"type": "object"` at its top level.
    #[error("tool '{tool}' input schema must have \"type\": \"object\", found {found}")]
    ToolSchemaNotObject {
        /// The tool.
        tool: String,
        /// What the schema's `type` was: the JSON value, or `no "type" key`.
        found: String,
    },
    /// A tool call id is empty.
    #[error("tool call id is empty")]
    EmptyToolCallId,
    /// A tool call id is longer than [`TOOL_CALL_ID_MAX_BYTES`].
    #[error(
        "tool call id has {len} bytes, more than the {max} allowed",
        max = TOOL_CALL_ID_MAX_BYTES
    )]
    ToolCallIdTooLong {
        /// Its length in bytes.
        len: usize,
    },
    /// A tool call id contains a control or Unicode format (Cf) character.
    #[error("tool call id has a control or format character at byte {index}")]
    ToolCallIdControlChar {
        /// Byte offset of the first control or format character.
        index: usize,
    },
    /// A tool call names a tool with more than [`TOOL_CALL_NAME_MAX_BYTES`] bytes.
    #[error(
        "tool call '{id}' names a tool of {len} bytes, more than the {max} allowed",
        max = TOOL_CALL_NAME_MAX_BYTES
    )]
    ToolCallNameTooLong {
        /// The call id.
        id: String,
        /// Length of the called name in bytes.
        len: usize,
    },
    /// A data source names an empty model.
    #[error("data source of tool '{tool}' has an empty model name")]
    EmptyDataSourceModel {
        /// The tool.
        tool: String,
    },
    /// A message has no parts.
    #[error("{role} message has no parts")]
    EmptyMessage {
        /// The message role.
        role: TurnRole,
    },
    /// A message holds a part its role may not carry.
    #[error("{role} message cannot carry a part of kind '{part}' (part {index})")]
    PartNotAllowed {
        /// The message role.
        role: TurnRole,
        /// Position of the part in the message.
        index: usize,
        /// The part kind.
        part: &'static str,
    },
    /// Two tools of one request share a name.
    #[error("tool '{name}' is declared twice in one request")]
    DuplicateToolName {
        /// The repeated name.
        name: String,
    },
    /// Two tool calls of one conversation share an id.
    #[error("message {message} repeats tool call id '{id}'")]
    DuplicateToolCallId {
        /// The repeated id.
        id: String,
        /// Index of the message with the repeat.
        message: usize,
    },
    /// A tool result names a call that no earlier assistant message made.
    #[error(
        "message {message} carries a tool result for '{id}', which no earlier assistant message called"
    )]
    UnknownToolResult {
        /// The result's call id.
        id: String,
        /// Index of the message holding the result.
        message: usize,
    },
    /// A second tool result answers a call that already has one.
    #[error("message {message} carries a second tool result for '{id}'")]
    DuplicateToolResult {
        /// The call id.
        id: String,
        /// Index of the message holding the second result.
        message: usize,
    },
    /// A response holds a part that only a request may carry.
    #[error("response cannot carry a part of kind '{part}' (part {index})")]
    ResponsePartNotAssistant {
        /// Position of the part in the response.
        index: usize,
        /// The part kind.
        part: &'static str,
    },
    /// A response has no parts.
    #[error("response has no parts")]
    EmptyResponse,
    /// Two tool calls of one response share an id.
    #[error("response repeats tool call id '{id}'")]
    DuplicateResponseToolCallId {
        /// The repeated id.
        id: String,
    },
}

/// The name of a tool: 1 to 64 characters from `A-Z a-z 0-9 _ -`.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct ToolName(String);

impl ToolName {
    /// Checks and wraps a tool name.
    ///
    /// # Errors
    /// [`ToolTypeError::InvalidToolName`] when the name is empty, too long or has another
    /// character.
    pub fn new(name: &str) -> Result<Self, ToolTypeError> {
        let valid = !name.is_empty()
            && name.len() <= TOOL_NAME_MAX_BYTES
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if valid {
            Ok(Self(name.to_owned()))
        } else {
            Err(ToolTypeError::InvalidToolName {
                name: name
                    .chars()
                    .take(TOOL_NAME_MAX_BYTES)
                    .collect::<String>()
                    .escape_debug()
                    .to_string(),
                len: name.len(),
            })
        }
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ToolName {
    type Error = ToolTypeError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(&name)
    }
}

impl From<ToolName> for String {
    fn from(name: ToolName) -> Self {
        name.0
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tool the model may call: its name, a description from code-declared help, and the JSON
/// Schema of its arguments (an object schema).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ToolSpec {
    name: ToolName,
    description: String,
    input_schema: serde_json::Map<String, serde_json::Value>,
}

impl ToolSpec {
    /// Checks and builds a tool spec.
    ///
    /// # Errors
    /// [`ToolTypeError::EmptyToolDescription`], [`ToolTypeError::ToolDescriptionTooLong`], or
    /// [`ToolTypeError::ToolSchemaNotObject`] when the schema's top-level `type` is not `"object"`.
    pub fn new(
        name: ToolName,
        description: impl Into<String>,
        input_schema: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, ToolTypeError> {
        let description = description.into();
        if description.trim().is_empty() {
            return Err(ToolTypeError::EmptyToolDescription {
                tool: name.0.clone(),
            });
        }
        let chars = description.chars().count();
        if chars > TOOL_DESCRIPTION_MAX_CHARS {
            return Err(ToolTypeError::ToolDescriptionTooLong {
                tool: name.0.clone(),
                chars,
            });
        }
        match input_schema.get("type") {
            Some(serde_json::Value::String(kind)) if kind == "object" => {}
            Some(other) => {
                return Err(ToolTypeError::ToolSchemaNotObject {
                    tool: name.0.clone(),
                    found: other.to_string(),
                });
            }
            None => {
                return Err(ToolTypeError::ToolSchemaNotObject {
                    tool: name.0.clone(),
                    found: "no \"type\" key".to_owned(),
                });
            }
        }
        Ok(Self {
            name,
            description,
            input_schema,
        })
    }

    /// The tool name.
    #[must_use]
    pub fn name(&self) -> &ToolName {
        &self.name
    }

    /// The description shown to the model.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The JSON Schema of the arguments.
    #[must_use]
    pub fn input_schema(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.input_schema
    }
}

/// The id a provider gives a tool call: 1 to 256 bytes without control or Unicode format (Cf)
/// characters. Deliberately
/// loose, since local OpenAI-compatible servers emit ids of their own shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Checks and wraps a tool call id.
    ///
    /// # Errors
    /// [`ToolTypeError::EmptyToolCallId`], [`ToolTypeError::ToolCallIdTooLong`] or
    /// [`ToolTypeError::ToolCallIdControlChar`].
    pub fn new(id: &str) -> Result<Self, ToolTypeError> {
        if id.is_empty() {
            return Err(ToolTypeError::EmptyToolCallId);
        }
        if id.len() > TOOL_CALL_ID_MAX_BYTES {
            return Err(ToolTypeError::ToolCallIdTooLong { len: id.len() });
        }
        if let Some((index, _)) = id.char_indices().find(|(_, c)| {
            c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') || is_format_char(*c)
        }) {
            return Err(ToolTypeError::ToolCallIdControlChar { index });
        }
        Ok(Self(id.to_owned()))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Unicode general category Cf (format characters: bidirectional overrides, zero-width
/// characters, tags), as listed by the Unicode 14.0 character database. Invisible in logs and
/// screens, so a call id never carries one. Format characters added by later Unicode versions
/// are not in this table.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{13438}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

impl TryFrom<String> for ToolCallId {
    type Error = ToolTypeError;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::new(&id)
    }
}

impl From<ToolCallId> for String {
    fn from(id: ToolCallId) -> Self {
        id.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tool call as the model produced it. The name need not be a valid [`ToolName`] and the
/// arguments need not be an object: the tool executor answers "unknown tool" or "invalid
/// arguments" to the model rather than losing the call. Only the name length is bounded.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "ToolCallWire")]
pub struct ToolCall {
    id: ToolCallId,
    name: String,
    arguments: serde_json::Value,
}

/// Unchecked wire shape of [`ToolCall`]; deserialization goes through [`ToolCall::new`].
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallWire {
    id: ToolCallId,
    name: String,
    arguments: serde_json::Value,
}

impl TryFrom<ToolCallWire> for ToolCall {
    type Error = ToolTypeError;

    fn try_from(wire: ToolCallWire) -> Result<Self, Self::Error> {
        Self::new(wire.id, wire.name, wire.arguments)
    }
}

impl ToolCall {
    /// Builds a tool call from a model's answer.
    ///
    /// # Errors
    /// [`ToolTypeError::ToolCallNameTooLong`] when the called name has more than
    /// [`TOOL_CALL_NAME_MAX_BYTES`] bytes (an adapter reports it as
    /// [`ProviderError::InvalidResponse`]).
    pub fn new(
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    ) -> Result<Self, ToolTypeError> {
        if name.len() > TOOL_CALL_NAME_MAX_BYTES {
            return Err(ToolTypeError::ToolCallNameTooLong {
                id: id.0,
                len: name.len(),
            });
        }
        Ok(Self {
            id,
            name,
            arguments,
        })
    }

    /// The call id.
    #[must_use]
    pub fn id(&self) -> &ToolCallId {
        &self.id
    }

    /// The called tool name, as received.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The arguments, as received.
    #[must_use]
    pub fn arguments(&self) -> &serde_json::Value {
        &self.arguments
    }
}

/// One piece of an [`Instruction`]: a static template text or a typed value. Closed on purpose:
/// there is no runtime-string piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstructionPiece {
    Text(&'static str),
    Integer(i64),
    Flag(bool),
}

/// System text for the model: `&'static str` template pieces filled with typed values. There is
/// no conversion from a runtime `String`, no parsing and no deserialization (
/// [`Instruction::from_static`]" is a review rule for `ai_base`, not a type guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pieces: Vec<InstructionPiece>,
}

impl Instruction {
    /// Starts an instruction with a static text.
    #[must_use]
    pub fn from_static(text: &'static str) -> Self {
        Self {
            pieces: vec![InstructionPiece::Text(text)],
        }
    }

    /// Appends a static text.
    #[must_use]
    pub fn then_static(mut self, text: &'static str) -> Self {
        self.pieces.push(InstructionPiece::Text(text));
        self
    }

    /// Appends an integer, rendered in decimal.
    #[must_use]
    pub fn then_integer(mut self, value: i64) -> Self {
        self.pieces.push(InstructionPiece::Integer(value));
        self
    }

    /// Appends a flag, rendered as `true` or `false`.
    #[must_use]
    pub fn then_flag(mut self, value: bool) -> Self {
        self.pieces.push(InstructionPiece::Flag(value));
        self
    }

    /// The text sent to the model: the pieces concatenated.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                InstructionPiece::Text(text) => out.push_str(text),
                InstructionPiece::Integer(value) => out.push_str(&value.to_string()),
                InstructionPiece::Flag(value) => {
                    out.push_str(if *value { "true" } else { "false" })
                }
            }
        }
        out
    }
}

/// Where a [`LabelledData`] value came from: the tool, the model it read (`None` for
/// registry-wide results) and the record ids (empty for grouped results, errors and results
/// awaiting confirmation).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DataSource {
    tool: ToolName,
    model: Option<String>,
    ids: Vec<i64>,
}

impl DataSource {
    /// Builds a data source.
    ///
    /// # Errors
    /// [`ToolTypeError::EmptyDataSourceModel`] when `model` is `Some("")`.
    pub fn new(
        tool: ToolName,
        model: Option<String>,
        ids: Vec<i64>,
    ) -> Result<Self, ToolTypeError> {
        if model.as_deref() == Some("") {
            return Err(ToolTypeError::EmptyDataSourceModel { tool: tool.0 });
        }
        Ok(Self { tool, model, ids })
    }

    /// The tool that produced the value.
    #[must_use]
    pub fn tool(&self) -> &ToolName {
        &self.tool
    }

    /// The model that was read, if the result belongs to one.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The record ids the value came from.
    #[must_use]
    pub fn ids(&self) -> &[i64] {
        &self.ids
    }
}

/// A value a tool read, labelled with its [`DataSource`]. It has no conversion from any string
/// and no deserialization; adapters serialize it as data, never as instructions.
/// Any crate can call [`LabelledData::new`]; that only the tool executor does is a review rule
/// for `ai_base`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LabelledData {
    source: DataSource,
    value: serde_json::Value,
}

impl LabelledData {
    /// Labels a tool's value with its source.
    #[must_use]
    pub fn new(source: DataSource, value: serde_json::Value) -> Self {
        Self { source, value }
    }

    /// Where the value came from.
    #[must_use]
    pub fn source(&self) -> &DataSource {
        &self.source
    }

    /// The value.
    #[must_use]
    pub fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

/// What the user typed. A distinct type, so a user string cannot be passed where an
/// [`Instruction`] or [`LabelledData`] is expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserText(String);

impl UserText {
    /// Wraps the user's text.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(text)
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The answer to one [`ToolCall`], tied to it by id.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    id: ToolCallId,
    data: LabelledData,
    is_error: bool,
}

impl ToolResult {
    /// Builds the result of the call `id`; `is_error` marks a refused or failed call.
    #[must_use]
    pub fn new(id: ToolCallId, data: LabelledData, is_error: bool) -> Self {
        Self { id, data, is_error }
    }

    /// The id of the call this answers.
    #[must_use]
    pub fn id(&self) -> &ToolCallId {
        &self.id
    }

    /// The labelled result.
    #[must_use]
    pub fn data(&self) -> &LabelledData {
        &self.data
    }

    /// Whether the call was refused or failed.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.is_error
    }
}

/// One typed part of a tool-turn message.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    /// System text from static templates (system messages only).
    Instruction(Instruction),
    /// What the user typed (user messages only).
    UserText(UserText),
    /// A labelled value from a tool (user messages only).
    Data(LabelledData),
    /// A tool call made by the model (assistant messages only).
    ToolCall(ToolCall),
    /// The answer to a tool call (user messages only).
    ToolResult(ToolResult),
    /// The model's own prose, resent with the history (assistant messages only). It holds any
    /// `String`; that only provider responses supply it is a review rule for `ai_base`.
    AssistantText(String),
}

impl ContentPart {
    fn kind(&self) -> &'static str {
        match self {
            Self::Instruction(_) => "instruction",
            Self::UserText(_) => "user text",
            Self::Data(_) => "data",
            Self::ToolCall(_) => "tool call",
            Self::ToolResult(_) => "tool result",
            Self::AssistantText(_) => "assistant text",
        }
    }

    fn allowed_for(&self, role: TurnRole) -> bool {
        match self {
            Self::Instruction(_) => role == TurnRole::System,
            Self::UserText(_) | Self::Data(_) | Self::ToolResult(_) => role == TurnRole::User,
            Self::ToolCall(_) | Self::AssistantText(_) => role == TurnRole::Assistant,
        }
    }
}

/// The sender of a [`TurnMessage`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// System instructions.
    System,
    /// The user, and tool results returned on the user's side.
    User,
    /// The model.
    Assistant,
}

impl fmt::Display for TurnRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        })
    }
}

/// One message of a tool turn: a role and the parts that role may carry.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnMessage {
    role: TurnRole,
    parts: Vec<ContentPart>,
}

impl TurnMessage {
    /// Checks and builds a message. A system message holds only instructions; a user message
    /// only user text, data and tool results; an assistant message only assistant text and tool
    /// calls.
    ///
    /// # Errors
    /// [`ToolTypeError::EmptyMessage`] or [`ToolTypeError::PartNotAllowed`].
    pub fn new(role: TurnRole, parts: Vec<ContentPart>) -> Result<Self, ToolTypeError> {
        if parts.is_empty() {
            return Err(ToolTypeError::EmptyMessage { role });
        }
        if let Some((index, part)) = parts
            .iter()
            .enumerate()
            .find(|(_, part)| !part.allowed_for(role))
        {
            return Err(ToolTypeError::PartNotAllowed {
                role,
                index,
                part: part.kind(),
            });
        }
        Ok(Self { role, parts })
    }

    /// The sender.
    #[must_use]
    pub fn role(&self) -> TurnRole {
        self.role
    }

    /// The parts, in order.
    #[must_use]
    pub fn parts(&self) -> &[ContentPart] {
        &self.parts
    }
}

/// A tool turn sent to a [`ToolModelProvider`]: the conversation so far and the tools on offer.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolTurnRequest {
    model: String,
    messages: Vec<TurnMessage>,
    tools: Vec<ToolSpec>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl ToolTurnRequest {
    /// Checks and builds a request: tool names are unique, tool call ids are unique across the
    /// conversation, and every tool result answers a call of an earlier assistant message, at most
    /// once.
    ///
    /// # Errors
    /// [`ToolTypeError::DuplicateToolName`], [`ToolTypeError::DuplicateToolCallId`],
    /// [`ToolTypeError::UnknownToolResult`] or [`ToolTypeError::DuplicateToolResult`].
    pub fn new(
        model: impl Into<String>,
        messages: Vec<TurnMessage>,
        tools: Vec<ToolSpec>,
    ) -> Result<Self, ToolTypeError> {
        let mut names = HashSet::new();
        for tool in &tools {
            if !names.insert(tool.name.as_str()) {
                return Err(ToolTypeError::DuplicateToolName {
                    name: tool.name.0.clone(),
                });
            }
        }
        let mut called: HashSet<&str> = HashSet::new();
        let mut answered: HashSet<&str> = HashSet::new();
        for (message, turn) in messages.iter().enumerate() {
            // Calls of this message become answerable from the next message on.
            let mut new_calls = Vec::new();
            for part in &turn.parts {
                match part {
                    ContentPart::ToolCall(call) => {
                        let id = call.id.as_str();
                        if called.contains(id) || new_calls.contains(&id) {
                            return Err(ToolTypeError::DuplicateToolCallId {
                                id: id.to_owned(),
                                message,
                            });
                        }
                        new_calls.push(id);
                    }
                    ContentPart::ToolResult(result) => {
                        let id = result.id.as_str();
                        if !called.contains(id) {
                            return Err(ToolTypeError::UnknownToolResult {
                                id: id.to_owned(),
                                message,
                            });
                        }
                        if !answered.insert(id) {
                            return Err(ToolTypeError::DuplicateToolResult {
                                id: id.to_owned(),
                                message,
                            });
                        }
                    }
                    ContentPart::Instruction(_)
                    | ContentPart::UserText(_)
                    | ContentPart::Data(_)
                    | ContentPart::AssistantText(_) => {}
                }
            }
            called.extend(new_calls);
        }
        Ok(Self {
            model: model.into(),
            messages,
            tools,
            max_tokens: None,
            temperature: None,
        })
    }

    /// Sets the maximum tokens to generate.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// The model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The conversation, in order.
    #[must_use]
    pub fn messages(&self) -> &[TurnMessage] {
        &self.messages
    }

    /// The tools on offer.
    #[must_use]
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// The maximum tokens to generate, if set.
    #[must_use]
    pub fn max_tokens(&self) -> Option<u32> {
        self.max_tokens
    }

    /// The sampling temperature, if set.
    #[must_use]
    pub fn temperature(&self) -> Option<f32> {
        self.temperature
    }
}

/// A provider's answer to a tool turn: assistant parts only (text and tool calls).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolTurnResponse {
    model: String,
    parts: Vec<ContentPart>,
    usage: UsageRecord,
    finish_reason: Option<FinishReason>,
}

impl ToolTurnResponse {
    /// Checks and builds a response.
    ///
    /// # Errors
    /// [`ToolTypeError::EmptyResponse`] for no parts,
    /// [`ToolTypeError::ResponsePartNotAssistant`] for an instruction, user text, data or tool
    /// result part, [`ToolTypeError::DuplicateResponseToolCallId`] for a repeated call id. An
    /// adapter reports each as [`ProviderError::InvalidResponse`].
    pub fn new(
        model: impl Into<String>,
        parts: Vec<ContentPart>,
        usage: UsageRecord,
        finish_reason: Option<FinishReason>,
    ) -> Result<Self, ToolTypeError> {
        if parts.is_empty() {
            return Err(ToolTypeError::EmptyResponse);
        }
        let mut ids = HashSet::new();
        for (index, part) in parts.iter().enumerate() {
            if !part.allowed_for(TurnRole::Assistant) {
                return Err(ToolTypeError::ResponsePartNotAssistant {
                    index,
                    part: part.kind(),
                });
            }
            if let ContentPart::ToolCall(call) = part
                && !ids.insert(call.id.as_str())
            {
                return Err(ToolTypeError::DuplicateResponseToolCallId {
                    id: call.id.0.clone(),
                });
            }
        }
        Ok(Self {
            model: model.into(),
            parts,
            usage,
            finish_reason,
        })
    }

    /// The model that answered.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The assistant parts, in order.
    #[must_use]
    pub fn parts(&self) -> &[ContentPart] {
        &self.parts
    }

    /// Metered usage of the call.
    #[must_use]
    pub fn usage(&self) -> &UsageRecord {
        &self.usage
    }

    /// Why the model stopped ([`FinishReason::ToolUse`] when it waits for tool results).
    #[must_use]
    pub fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason
    }
}

/// A [`ModelProvider`] that can run tool turns. A provider without tool calling does not
/// implement it, so the lack is a type error rather than a runtime one.
pub trait ToolModelProvider: ModelProvider {
    /// Runs one tool turn: sends the conversation and tools, returns the model's parts.
    fn complete_turn<'a>(
        &'a self,
        request: &'a ToolTurnRequest,
    ) -> BoxFuture<'a, Result<ToolTurnResponse, ProviderError>>;
}
