//! Mock AI providers for testing (AI0.9, core hook H6).
//!
//! Provides in-memory, deterministic implementations of [`ModelProvider`] and
//! [`EmbeddingProvider`] for test suites and harnesses, without any HTTP or
//! network dependencies.

use erp_orm::BoxFuture;
use erp_sdk::ai::{
    BoxModelStream, CompletionRequest, CompletionResponse, ContentPart, EmbeddingProvider,
    EmbeddingRequest, EmbeddingResponse, FinishReason, ModelProvider, ProviderError, StreamChunk,
    ToolCall, ToolCallId, ToolModelProvider, ToolTurnRequest, ToolTurnResponse, ToolTypeError,
    UsageRecord,
};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Configurable mock language model provider.
#[derive(Debug, Clone)]
pub struct MockModelProvider {
    name: String,
    canned_completion: Arc<Mutex<Option<CompletionResponse>>>,
    canned_chunks: Arc<Mutex<Vec<StreamChunk>>>,
    simulated_error: Arc<Mutex<Option<ProviderError>>>,
    recorded_completions: Arc<Mutex<Vec<CompletionRequest>>>,
    tokens_per_word: u32,
}

impl MockModelProvider {
    /// Creates a mock provider with the given name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            canned_completion: Arc::new(Mutex::new(None)),
            canned_chunks: Arc::new(Mutex::new(Vec::new())),
            simulated_error: Arc::new(Mutex::new(None)),
            recorded_completions: Arc::new(Mutex::new(Vec::new())),
            tokens_per_word: 1,
        }
    }

    /// Sets the multiplier for token counting per word.
    #[must_use]
    pub fn with_tokens_per_word(mut self, tokens_per_word: u32) -> Self {
        self.tokens_per_word = tokens_per_word.max(1);
        self
    }

    /// Configures a canned completion response.
    pub fn set_canned_completion(&self, response: CompletionResponse) {
        *lock(&self.canned_completion) = Some(response);
    }

    /// Configures canned stream chunks.
    pub fn set_canned_stream(&self, chunks: Vec<StreamChunk>) {
        *lock(&self.canned_chunks) = chunks;
    }

    /// Configures an error to return on future calls, or clears the error.
    pub fn set_error(&self, error: Option<ProviderError>) {
        *lock(&self.simulated_error) = error;
    }

    /// Returns a snapshot of all completion requests received by this mock.
    #[must_use]
    pub fn recorded_completions(&self) -> Vec<CompletionRequest> {
        lock(&self.recorded_completions).clone()
    }
}

impl ModelProvider for MockModelProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn complete<'a>(
        &'a self,
        request: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionResponse, ProviderError>> {
        lock(&self.recorded_completions).push(request.clone());

        if let Some(err) = lock(&self.simulated_error).clone() {
            return Box::pin(async move { Err(err) });
        }

        if let Some(resp) = lock(&self.canned_completion).clone() {
            return Box::pin(async move { Ok(resp) });
        }

        let prompt_tokens = (request.prompt.split_whitespace().count() as u32).max(1);
        let echo_text = format!("Echo: {}", request.prompt);
        let completion_tokens = echo_text.split_whitespace().count() as u32;
        let usage = UsageRecord::new(&self.name, &request.model, prompt_tokens, completion_tokens);

        let response = CompletionResponse {
            text: echo_text,
            model: request.model.clone(),
            usage,
            finish_reason: Some(FinishReason::Stop),
        };

        Box::pin(async move { Ok(response) })
    }

    fn stream<'a>(
        &'a self,
        request: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<BoxModelStream<'a>, ProviderError>> {
        lock(&self.recorded_completions).push(request.clone());

        if let Some(err) = lock(&self.simulated_error).clone() {
            return Box::pin(async move { Err(err) });
        }

        let canned = lock(&self.canned_chunks).clone();
        let chunks = if !canned.is_empty() {
            canned
        } else {
            let words: Vec<&str> = request.prompt.split_whitespace().collect();
            let prompt_tokens = (words.len() as u32).max(1);
            let completion_tokens = words.len() as u32;
            let usage =
                UsageRecord::new(&self.name, &request.model, prompt_tokens, completion_tokens);
            let mut out = Vec::new();
            for w in words {
                out.push(StreamChunk::delta(format!("{w} ")));
            }
            out.push(StreamChunk::final_chunk(usage, FinishReason::Stop));
            out
        };

        let stream: BoxModelStream<'a> = Box::pin(SimpleChunkStream {
            items: chunks.into_iter().map(Ok).collect::<Vec<_>>().into_iter(),
        });

        Box::pin(async move { Ok(stream) })
    }

    fn count_tokens<'a>(
        &'a self,
        _model: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, Result<u32, ProviderError>> {
        if let Some(err) = lock(&self.simulated_error).clone() {
            return Box::pin(async move { Err(err) });
        }
        let words = text.split_whitespace().count() as u32;
        let count = words.saturating_mul(self.tokens_per_word).max(1);
        Box::pin(async move { Ok(count) })
    }
}

struct SimpleChunkStream {
    items: std::vec::IntoIter<Result<StreamChunk, ProviderError>>,
}

impl futures_core::Stream for SimpleChunkStream {
    type Item = Result<StreamChunk, ProviderError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(self.items.next())
    }
}

/// Configurable mock embedding provider.
#[derive(Debug, Clone)]
pub struct MockEmbeddingProvider {
    name: String,
    dimension: usize,
    canned_response: Arc<Mutex<Option<EmbeddingResponse>>>,
    simulated_error: Arc<Mutex<Option<ProviderError>>>,
    recorded_requests: Arc<Mutex<Vec<EmbeddingRequest>>>,
}

impl MockEmbeddingProvider {
    /// Creates a new mock embedding provider.
    #[must_use]
    pub fn new(name: impl Into<String>, dimension: usize) -> Self {
        Self {
            name: name.into(),
            dimension: dimension.max(1),
            canned_response: Arc::new(Mutex::new(None)),
            simulated_error: Arc::new(Mutex::new(None)),
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Sets canned embedding response.
    pub fn set_canned_response(&self, response: EmbeddingResponse) {
        if let Ok(mut lock) = self.canned_response.lock() {
            *lock = Some(response);
        }
    }

    /// Sets an error to return on future calls, or clears the error.
    pub fn set_error(&self, error: Option<ProviderError>) {
        if let Ok(mut lock) = self.simulated_error.lock() {
            *lock = error;
        }
    }

    /// Returns recorded requests.
    #[must_use]
    pub fn recorded_requests(&self) -> Vec<EmbeddingRequest> {
        self.recorded_requests
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }
}

impl EmbeddingProvider for MockEmbeddingProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn embed<'a>(
        &'a self,
        request: &'a EmbeddingRequest,
    ) -> BoxFuture<'a, Result<EmbeddingResponse, ProviderError>> {
        if let Ok(mut recorded) = self.recorded_requests.lock() {
            recorded.push(request.clone());
        }

        if let Some(err) = self.simulated_error.lock().ok().and_then(|g| g.clone()) {
            return Box::pin(async move { Err(err) });
        }

        if let Some(resp) = self.canned_response.lock().ok().and_then(|g| g.clone()) {
            return Box::pin(async move { Ok(resp) });
        }

        let mut embeddings = Vec::with_capacity(request.inputs.len());
        let mut total_prompt_tokens = 0u32;

        for (idx, input) in request.inputs.iter().enumerate() {
            let tokens = (input.split_whitespace().count() as u32).max(1);
            total_prompt_tokens = total_prompt_tokens.saturating_add(tokens);

            let mut vec = Vec::with_capacity(self.dimension);
            let seed = (input.len() as f32) + (idx as f32);
            for d in 0..self.dimension {
                let val = ((d as f32 + 1.0) * seed * 0.1).sin();
                vec.push(val);
            }
            embeddings.push(vec);
        }

        let usage = UsageRecord::new(&self.name, &request.model, total_prompt_tokens, 0);
        let response = EmbeddingResponse {
            model: request.model.clone(),
            embeddings,
            usage,
        };

        Box::pin(async move { Ok(response) })
    }
}

/// One queued answer of a [`ScriptedModelProvider`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptedReply {
    /// Answer the turn with this response.
    Response(ToolTurnResponse),
    /// Fail the turn with this error.
    Error(ProviderError),
}

impl ScriptedReply {
    /// Provider name in the usage of the responses built by [`ScriptedReply::tool_call`] and
    /// [`ScriptedReply::text`].
    pub const USAGE_PROVIDER: &'static str = "scripted";
    /// Model name of the responses built by [`ScriptedReply::tool_call`] and
    /// [`ScriptedReply::text`].
    pub const MODEL: &'static str = "scripted-model";

    /// A response holding one tool call, finish reason [`FinishReason::ToolUse`], usage 1 + 1
    /// tokens. `name` and `arguments` are kept as given, so a test can script a call to a tool
    /// that does not exist or with arguments that do not fit its schema.
    ///
    /// # Errors
    /// The [`ToolTypeError`] of an invalid id or an oversized name.
    pub fn tool_call(
        id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolTurnResponse, ToolTypeError> {
        let call = ToolCall::new(ToolCallId::new(id)?, name.to_owned(), arguments)?;
        ToolTurnResponse::new(
            Self::MODEL,
            vec![ContentPart::ToolCall(call)],
            UsageRecord::new(Self::USAGE_PROVIDER, Self::MODEL, 1, 1),
            Some(FinishReason::ToolUse),
        )
    }

    /// A response holding assistant text, finish reason [`FinishReason::Stop`], usage 1 + 1
    /// tokens.
    ///
    /// # Errors
    /// None today; the `Result` follows [`ToolTurnResponse::new`].
    pub fn text(text: &str) -> Result<ToolTurnResponse, ToolTypeError> {
        ToolTurnResponse::new(
            Self::MODEL,
            vec![ContentPart::AssistantText(text.to_owned())],
            UsageRecord::new(Self::USAGE_PROVIDER, Self::MODEL, 1, 1),
            Some(FinishReason::Stop),
        )
    }
}

/// A tool-calling test double: answers each tool turn with the next queued [`ScriptedReply`]
/// and records every request. An exhausted script is a [`ProviderError::Backend`] naming the call,
/// never a default answer. Text-only requests ([`ModelProvider`]) behave as [`MockModelProvider`]
/// (echo).
#[derive(Debug, Clone)]
pub struct ScriptedModelProvider {
    name: String,
    script: Arc<Mutex<VecDeque<ScriptedReply>>>,
    recorded: Arc<Mutex<Vec<ToolTurnRequest>>>,
    text: MockModelProvider,
}

/// Locks test-double state of [`MockModelProvider`] and [`ScriptedModelProvider`]. A poisoned
/// lock only means another test thread panicked while holding it; each critical section here is
/// a single assignment, push, pop or clone, so the data is consistent and the guard is taken over.
/// Configured answers and recorded requests are therefore never dropped or replaced by an empty
/// default.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ScriptedModelProvider {
    /// Creates a scripted provider with an empty script.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            text: MockModelProvider::new(name.clone()),
            name,
            script: Arc::new(Mutex::new(VecDeque::new())),
            recorded: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Queues a response.
    pub fn push_reply(&self, response: ToolTurnResponse) {
        lock(&self.script).push_back(ScriptedReply::Response(response));
    }

    /// Queues an error.
    pub fn push_error(&self, error: ProviderError) {
        lock(&self.script).push_back(ScriptedReply::Error(error));
    }

    /// Number of queued replies not yet used.
    #[must_use]
    pub fn remaining(&self) -> usize {
        lock(&self.script).len()
    }

    /// Every tool-turn request received, in order.
    #[must_use]
    pub fn recorded_requests(&self) -> Vec<ToolTurnRequest> {
        lock(&self.recorded).clone()
    }

    /// Every text-only completion request received, in order.
    #[must_use]
    pub fn recorded_completions(&self) -> Vec<CompletionRequest> {
        self.text.recorded_completions()
    }
}

impl ModelProvider for ScriptedModelProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn complete<'a>(
        &'a self,
        request: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionResponse, ProviderError>> {
        self.text.complete(request)
    }

    fn stream<'a>(
        &'a self,
        request: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<BoxModelStream<'a>, ProviderError>> {
        self.text.stream(request)
    }

    fn count_tokens<'a>(
        &'a self,
        model: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, Result<u32, ProviderError>> {
        self.text.count_tokens(model, text)
    }
}

impl ToolModelProvider for ScriptedModelProvider {
    fn complete_turn<'a>(
        &'a self,
        request: &'a ToolTurnRequest,
    ) -> BoxFuture<'a, Result<ToolTurnResponse, ProviderError>> {
        let call = {
            let mut recorded = lock(&self.recorded);
            recorded.push(request.clone());
            recorded.len()
        };
        let reply = lock(&self.script).pop_front();
        let outcome = match reply {
            Some(ScriptedReply::Response(response)) => Ok(response),
            Some(ScriptedReply::Error(error)) => Err(error),
            None => Err(ProviderError::Backend {
                provider: self.name.clone(),
                message: format!("the script is exhausted at call {call} (no reply queued)"),
            }),
        };
        Box::pin(async move { outcome })
    }
}
