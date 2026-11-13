//! AI provider and metering interfaces (AI0.9, core hook H6).
//!
//! Provides abstract traits and data structures for LLM text generation,
//! streaming completions, vector embeddings, and metered usage accounting.
//! Core crates never depend on an HTTP client for AI. Concrete providers
//! (OpenAI, Anthropic, Gemini, Ollama, etc.) are implemented by addons (such
//! as `ai_base` in M4).
//!
//! # Examples
//!
//! ```
//! use erp_sdk::ai::{CompletionRequest, UsageRecord};
//!
//! let req = CompletionRequest::new("gpt-4o", "Hello world")
//!     .with_max_tokens(100);
//! assert_eq!(req.prompt, "Hello world");
//! assert_eq!(req.max_tokens, Some(100));
//!
//! let usage = UsageRecord::new("mock", "gpt-4o", 10, 20)
//!     .with_cost_micros(50)
//!     .with_latency_ms(150);
//! assert_eq!(usage.total_tokens, 30);
//! assert_eq!(usage.cost_micros, Some(50));
//! ```

use erp_orm::BoxFuture;
use std::pin::Pin;

/// Tool calling: typed content parts, tool specs and the tool-turn provider trait.
#[path = "ai_tools.rs"]
pub mod tools;

pub use tools::{
    ContentPart, DataSource, Instruction, LabelledData, ToolCall, ToolCallId, ToolModelProvider,
    ToolName, ToolResult, ToolSpec, ToolTurnRequest, ToolTurnResponse, ToolTypeError, TurnMessage,
    TurnRole, UserText,
};

/// Metered usage accounting for a model or embedding call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageRecord {
    /// Provider identifier (e.g. "mock", "openai", "anthropic", "gemini").
    pub provider: String,
    /// Model name or identifier (e.g. "gpt-4o", "claude-3-5-sonnet", "gemini-1.5-pro").
    pub model: String,
    /// Number of prompt / input tokens consumed.
    pub prompt_tokens: u32,
    /// Number of completion / output tokens generated (0 for embeddings).
    pub completion_tokens: u32,
    /// Total tokens (`prompt_tokens + completion_tokens`).
    pub total_tokens: u32,
    /// Estimated cost in micro-units of base currency (e.g. 1_000_000 = 1.00 USD), if known.
    pub cost_micros: Option<u64>,
    /// Request round-trip or execution latency in milliseconds, if recorded.
    pub latency_ms: Option<u64>,
}

impl UsageRecord {
    /// Creates a new usage record, computing `total_tokens` via saturating addition.
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        prompt_tokens: u32,
        completion_tokens: u32,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
            cost_micros: None,
            latency_ms: None,
        }
    }

    /// Sets the estimated cost in micro-units.
    #[must_use]
    pub fn with_cost_micros(mut self, cost_micros: u64) -> Self {
        self.cost_micros = Some(cost_micros);
        self
    }

    /// Sets the latency in milliseconds.
    #[must_use]
    pub fn with_latency_ms(mut self, latency_ms: u64) -> Self {
        self.latency_ms = Some(latency_ms);
        self
    }

    /// Accumulates token counts and costs from another record if provider and model match.
    ///
    /// Returns `None` if the provider or model identifiers do not match.
    #[must_use]
    pub fn accumulate(&self, other: &Self) -> Option<Self> {
        if self.provider != other.provider || self.model != other.model {
            return None;
        }
        let prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        let completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        let total_tokens = prompt_tokens.saturating_add(completion_tokens);
        let cost_micros = match (self.cost_micros, other.cost_micros) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let latency_ms = match (self.latency_ms, other.latency_ms) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        Some(Self {
            provider: self.provider.clone(),
            model: self.model.clone(),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost_micros,
            latency_ms,
        })
    }
}

/// Role of a participant in a chat completion prompt.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MessageRole {
    /// System prompt / instructions.
    System,
    /// User message.
    User,
    /// Assistant response.
    Assistant,
}

/// A single message in a chat conversation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChatMessage {
    /// Role of the sender.
    pub role: MessageRole,
    /// Content of the message.
    pub content: String,
}

impl ChatMessage {
    /// Creates a system message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
        }
    }

    /// Creates a user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
        }
    }

    /// Creates an assistant message.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
        }
    }
}

/// The reason a model stopped generating tokens.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FinishReason {
    /// Generation stopped naturally or on a stop sequence.
    Stop,
    /// Generation reached the maximum allowed tokens limit.
    Length,
    /// Content was filtered by safety or content policy.
    ContentFilter,
    /// Any other provider-specific finish reason.
    Other,
    /// The model stopped to wait for tool results (tool turns, [`tools`]).
    ToolUse,
}

/// Request parameters for text or chat completion.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompletionRequest {
    /// Model identifier.
    pub model: String,
    /// Text prompt (used for single prompt completion).
    pub prompt: String,
    /// System instructions, if specified.
    pub system: Option<String>,
    /// Chat messages (for multi-turn conversations).
    pub messages: Vec<ChatMessage>,
    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
    /// Sampling temperature (typically 0.0 to 2.0).
    pub temperature: Option<f32>,
    /// Optional stop sequences.
    pub stop_sequences: Vec<String>,
}

impl CompletionRequest {
    /// Creates a simple single-turn completion request.
    #[must_use]
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            prompt: prompt.into(),
            system: None,
            messages: Vec::new(),
            max_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
        }
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets multi-turn chat messages.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<ChatMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Sets maximum tokens to generate.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Adds a stop sequence.
    #[must_use]
    pub fn with_stop_sequence(mut self, stop: impl Into<String>) -> Self {
        self.stop_sequences.push(stop.into());
        self
    }
}

/// Response returned by a completion request.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompletionResponse {
    /// The generated text content.
    pub text: String,
    /// The model that produced the completion.
    pub model: String,
    /// Metered usage metrics.
    pub usage: UsageRecord,
    /// Why the model stopped generating tokens.
    pub finish_reason: Option<FinishReason>,
}

/// A single chunk in an asynchronous completion stream.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StreamChunk {
    /// Incremental delta text generated in this chunk.
    pub delta: String,
    /// Final usage record, typically present only on the terminating chunk.
    pub usage: Option<UsageRecord>,
    /// Reason generation stopped, present on the final chunk.
    pub finish_reason: Option<FinishReason>,
}

impl StreamChunk {
    /// Creates a content delta chunk.
    #[must_use]
    pub fn delta(text: impl Into<String>) -> Self {
        Self {
            delta: text.into(),
            usage: None,
            finish_reason: None,
        }
    }

    /// Creates a final terminating chunk with usage and finish reason.
    #[must_use]
    pub fn final_chunk(usage: UsageRecord, finish_reason: FinishReason) -> Self {
        Self {
            delta: String::new(),
            usage: Some(usage),
            finish_reason: Some(finish_reason),
        }
    }
}

/// Request for generating vector embeddings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmbeddingRequest {
    /// Embedding model identifier.
    pub model: String,
    /// Batch of text inputs to embed.
    pub inputs: Vec<String>,
}

impl EmbeddingRequest {
    /// Creates a request for a single input text.
    #[must_use]
    pub fn single(model: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            inputs: vec![input.into()],
        }
    }

    /// Creates a request for multiple input texts.
    #[must_use]
    pub fn batch(model: impl Into<String>, inputs: Vec<String>) -> Self {
        Self {
            model: model.into(),
            inputs,
        }
    }
}

/// Response containing generated vector embeddings.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EmbeddingResponse {
    /// The embedding model used.
    pub model: String,
    /// Vector embeddings, one float vector per input text in the request.
    pub embeddings: Vec<Vec<f32>>,
    /// Metered usage metrics.
    pub usage: UsageRecord,
}

/// Errors returned by AI providers.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// Requested model is not available or not recognized by the provider.
    #[error("provider '{provider}' model '{model}' not found")]
    ModelNotFound {
        /// Provider name.
        provider: String,
        /// Model name.
        model: String,
    },

    /// Authentication or API key verification failed.
    #[error("provider '{provider}' authentication failed: {message}")]
    AuthenticationFailed {
        /// Provider name.
        provider: String,
        /// Error details.
        message: String,
    },

    /// Provider rate limit or concurrency limit exceeded.
    #[error("provider '{provider}' rate limit exceeded: {message}")]
    RateLimitExceeded {
        /// Provider name.
        provider: String,
        /// Error details.
        message: String,
    },

    /// Input prompt or messages exceeded the model's context window.
    #[error("provider '{provider}' context length exceeded: {message}")]
    ContextLengthExceeded {
        /// Provider name.
        provider: String,
        /// Error details.
        message: String,
    },

    /// Provider service is temporarily unavailable or unreachable.
    #[error("provider '{provider}' service unavailable: {message}")]
    Unavailable {
        /// Provider name.
        provider: String,
        /// Error details.
        message: String,
    },

    /// The request payload or options were invalid for this provider.
    #[error("provider '{provider}' invalid request: {message}")]
    InvalidRequest {
        /// Provider name.
        provider: String,
        /// Error details.
        message: String,
    },

    /// Generic or unexpected provider backend error.
    #[error("provider '{provider}' backend error: {message}")]
    Backend {
        /// Provider name.
        provider: String,
        /// Error details.
        message: String,
    },
    /// The provider answered with something the interface cannot represent (an empty tool call
    /// id, an oversized tool name, a part a response may not carry).
    #[error("provider '{provider}' invalid response: {message}")]
    InvalidResponse {
        /// Provider name.
        provider: String,
        /// Error details.
        message: String,
    },
}

/// A boxed asynchronous stream of chunks or errors.
pub type BoxModelStream<'a> =
    Pin<Box<dyn futures_core::Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'a>>;

/// Provider trait for language models (completion, streaming, token counting).
pub trait ModelProvider: Send + Sync {
    /// Identifier for this provider (e.g. "mock", "openai", "anthropic", "gemini").
    fn name(&self) -> &str;

    /// Complete a prompt or chat request asynchronously.
    fn complete<'a>(
        &'a self,
        request: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionResponse, ProviderError>>;

    /// Stream completion chunks asynchronously.
    fn stream<'a>(
        &'a self,
        request: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<BoxModelStream<'a>, ProviderError>>;

    /// Count tokens for a given input text on the specified model.
    fn count_tokens<'a>(
        &'a self,
        model: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, Result<u32, ProviderError>>;
}

/// Provider trait for generating vector embeddings.
pub trait EmbeddingProvider: Send + Sync {
    /// Identifier for this provider.
    fn name(&self) -> &str;

    /// Generate vector embeddings for a batch of input texts.
    fn embed<'a>(
        &'a self,
        request: &'a EmbeddingRequest,
    ) -> BoxFuture<'a, Result<EmbeddingResponse, ProviderError>>;
}
