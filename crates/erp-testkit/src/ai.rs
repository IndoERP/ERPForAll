//! Mock AI providers for testing (AI0.9, core hook H6).
//!
//! Provides in-memory, deterministic implementations of [`ModelProvider`] and
//! [`EmbeddingProvider`] for test suites and harnesses, without any HTTP or
//! network dependencies.

use erp_orm::BoxFuture;
use erp_sdk::ai::{
    BoxModelStream, CompletionRequest, CompletionResponse, EmbeddingProvider, EmbeddingRequest,
    EmbeddingResponse, FinishReason, ModelProvider, ProviderError, StreamChunk, UsageRecord,
};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

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
        if let Ok(mut lock) = self.canned_completion.lock() {
            *lock = Some(response);
        }
    }

    /// Configures canned stream chunks.
    pub fn set_canned_stream(&self, chunks: Vec<StreamChunk>) {
        if let Ok(mut lock) = self.canned_chunks.lock() {
            *lock = chunks;
        }
    }

    /// Configures an error to return on future calls, or clears the error.
    pub fn set_error(&self, error: Option<ProviderError>) {
        if let Ok(mut lock) = self.simulated_error.lock() {
            *lock = error;
        }
    }

    /// Returns a snapshot of all completion requests received by this mock.
    #[must_use]
    pub fn recorded_completions(&self) -> Vec<CompletionRequest> {
        self.recorded_completions
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
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
        if let Ok(mut recorded) = self.recorded_completions.lock() {
            recorded.push(request.clone());
        }

        if let Some(err) = self.simulated_error.lock().ok().and_then(|g| g.clone()) {
            return Box::pin(async move { Err(err) });
        }

        if let Some(resp) = self.canned_completion.lock().ok().and_then(|g| g.clone()) {
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
        if let Ok(mut recorded) = self.recorded_completions.lock() {
            recorded.push(request.clone());
        }

        if let Some(err) = self.simulated_error.lock().ok().and_then(|g| g.clone()) {
            return Box::pin(async move { Err(err) });
        }

        let chunks = if let Ok(canned) = self.canned_chunks.lock() {
            if !canned.is_empty() {
                canned.clone()
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
            }
        } else {
            Vec::new()
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
        if let Some(err) = self.simulated_error.lock().ok().and_then(|g| g.clone()) {
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
