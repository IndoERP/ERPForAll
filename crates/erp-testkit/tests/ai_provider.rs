//! Integration tests for AI provider traits and metering (AI0.9, core hook H6).

use erp_sdk::ai::{
    ChatMessage, CompletionRequest, CompletionResponse, EmbeddingProvider, EmbeddingRequest,
    FinishReason, MessageRole, ModelProvider, ProviderError, UsageRecord,
};
use erp_testkit::ai::{MockEmbeddingProvider, MockModelProvider};
use futures_core::Stream;
use std::sync::Arc;

#[tokio::test]
async fn model_provider_complete_echo_and_usage() {
    let provider = MockModelProvider::new("mock-llm");
    assert_eq!(provider.name(), "mock-llm");

    let req = CompletionRequest::new("gpt-4o", "erpforall core test")
        .with_system("You are a helpful assistant")
        .with_max_tokens(50)
        .with_temperature(0.7);

    let resp = provider
        .complete(&req)
        .await
        .expect("complete should succeed");

    assert_eq!(resp.model, "gpt-4o");
    assert_eq!(resp.text, "Echo: erpforall core test");
    assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
    assert_eq!(resp.usage.provider, "mock-llm");
    assert_eq!(resp.usage.model, "gpt-4o");
    assert_eq!(resp.usage.prompt_tokens, 3);
    assert_eq!(resp.usage.completion_tokens, 4);
    assert_eq!(resp.usage.total_tokens, 7);

    let recorded = provider.recorded_completions();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].prompt, "erpforall core test");
    assert_eq!(
        recorded[0].system.as_deref(),
        Some("You are a helpful assistant")
    );
    assert_eq!(recorded[0].max_tokens, Some(50));
}

#[tokio::test]
async fn model_provider_canned_completion() {
    let provider = MockModelProvider::new("mock-canned");

    let canned_usage = UsageRecord::new("mock-canned", "custom-model", 42, 58)
        .with_cost_micros(1_200)
        .with_latency_ms(250);

    let canned_resp = CompletionResponse {
        text: "Custom response text".to_string(),
        model: "custom-model".to_string(),
        usage: canned_usage.clone(),
        finish_reason: Some(FinishReason::Stop),
    };

    provider.set_canned_completion(canned_resp.clone());

    let req = CompletionRequest::new("custom-model", "any input");
    let resp = provider
        .complete(&req)
        .await
        .expect("complete should succeed");

    assert_eq!(resp.text, "Custom response text");
    assert_eq!(resp.usage, canned_usage);
    assert_eq!(resp.usage.cost_micros, Some(1_200));
    assert_eq!(resp.usage.latency_ms, Some(250));
}

#[tokio::test]
async fn model_provider_stream_chunks_and_usage() {
    let provider = MockModelProvider::new("mock-stream");
    let req = CompletionRequest::new("test-stream-model", "one two three");

    let stream = provider.stream(&req).await.expect("stream should succeed");

    let mut deltas = Vec::new();
    let mut final_usage = None;
    let mut final_reason = None;

    // Pin and poll the stream manually via futures_core::Stream
    let mut pinned = std::pin::pin!(stream);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());

    while let std::task::Poll::Ready(Some(chunk_res)) = pinned.as_mut().poll_next(&mut cx) {
        let chunk = chunk_res.expect("chunk should succeed");
        if !chunk.delta.is_empty() {
            deltas.push(chunk.delta);
        }
        if let Some(usage) = chunk.usage {
            final_usage = Some(usage);
        }
        if let Some(reason) = chunk.finish_reason {
            final_reason = Some(reason);
        }
    }

    assert_eq!(deltas, vec!["one ", "two ", "three "]);
    assert_eq!(final_reason, Some(FinishReason::Stop));
    assert!(final_usage.is_some());
    let usage = final_usage.expect("usage should be present on final chunk");
    assert_eq!(usage.prompt_tokens, 3);
    assert_eq!(usage.completion_tokens, 3);
    assert_eq!(usage.total_tokens, 6);
}

#[tokio::test]
async fn model_provider_count_tokens() {
    let provider = MockModelProvider::new("mock-tokens").with_tokens_per_word(2);

    let count = provider
        .count_tokens("any-model", "hello world from erpforall")
        .await
        .expect("count_tokens should succeed");

    assert_eq!(count, 8); // 4 words * 2 tokens_per_word = 8
}

#[tokio::test]
async fn model_provider_error_handling() {
    let provider = MockModelProvider::new("mock-err");

    // ModelNotFound
    provider.set_error(Some(ProviderError::ModelNotFound {
        provider: "mock-err".to_string(),
        model: "nonexistent".to_string(),
    }));

    let req = CompletionRequest::new("nonexistent", "test");
    let err = provider
        .complete(&req)
        .await
        .expect_err("should return error");
    match err {
        ProviderError::ModelNotFound { provider, model } => {
            assert_eq!(provider, "mock-err");
            assert_eq!(model, "nonexistent");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }

    // RateLimitExceeded
    provider.set_error(Some(ProviderError::RateLimitExceeded {
        provider: "mock-err".to_string(),
        message: "quota exceeded".to_string(),
    }));

    let err = provider
        .complete(&req)
        .await
        .expect_err("should return rate limit error");
    assert!(matches!(err, ProviderError::RateLimitExceeded { .. }));
    assert_eq!(
        err.to_string(),
        "provider 'mock-err' rate limit exceeded: quota exceeded"
    );

    // Clear error
    provider.set_error(None);
    let resp = provider
        .complete(&req)
        .await
        .expect("should succeed after error clear");
    assert_eq!(resp.text, "Echo: test");
}

#[tokio::test]
async fn embedding_provider_embed_and_usage() {
    let provider = MockEmbeddingProvider::new("mock-embed", 4);
    assert_eq!(provider.name(), "mock-embed");

    let req = EmbeddingRequest::batch(
        "text-embedding-3-small",
        vec![
            "product catalog".to_string(),
            "customer invoice".to_string(),
        ],
    );

    let resp = provider.embed(&req).await.expect("embed should succeed");

    assert_eq!(resp.model, "text-embedding-3-small");
    assert_eq!(resp.embeddings.len(), 2);
    assert_eq!(resp.embeddings[0].len(), 4);
    assert_eq!(resp.embeddings[1].len(), 4);
    assert_eq!(resp.usage.prompt_tokens, 4); // 2 words + 2 words = 4 tokens
    assert_eq!(resp.usage.completion_tokens, 0); // embeddings have 0 completion tokens
    assert_eq!(resp.usage.total_tokens, 4);

    let recorded = provider.recorded_requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].inputs.len(), 2);
}

#[tokio::test]
async fn embedding_provider_error_handling() {
    let provider = MockEmbeddingProvider::new("mock-embed-err", 4);

    provider.set_error(Some(ProviderError::AuthenticationFailed {
        provider: "mock-embed-err".to_string(),
        message: "invalid api key".to_string(),
    }));

    let req = EmbeddingRequest::single("embed-model", "test input");
    let err = provider
        .embed(&req)
        .await
        .expect_err("should return auth error");
    assert_eq!(
        err.to_string(),
        "provider 'mock-embed-err' authentication failed: invalid api key"
    );
}

#[test]
fn usage_record_accumulation_and_checks() {
    let u1 = UsageRecord::new("openai", "gpt-4o", 100, 50)
        .with_cost_micros(300)
        .with_latency_ms(200);

    let u2 = UsageRecord::new("openai", "gpt-4o", 80, 40)
        .with_cost_micros(240)
        .with_latency_ms(180);

    let acc = u1
        .accumulate(&u2)
        .expect("matching records should accumulate");
    assert_eq!(acc.provider, "openai");
    assert_eq!(acc.model, "gpt-4o");
    assert_eq!(acc.prompt_tokens, 180);
    assert_eq!(acc.completion_tokens, 90);
    assert_eq!(acc.total_tokens, 270);
    assert_eq!(acc.cost_micros, Some(540));
    assert_eq!(acc.latency_ms, Some(380));

    // Mismatched provider or model returns None
    let diff_model = UsageRecord::new("openai", "gpt-4o-mini", 10, 10);
    assert!(u1.accumulate(&diff_model).is_none());

    let diff_provider = UsageRecord::new("anthropic", "gpt-4o", 10, 10);
    assert!(u1.accumulate(&diff_provider).is_none());
}

#[tokio::test]
async fn dyn_trait_object_dispatch() {
    let model_box: Box<dyn ModelProvider> = Box::new(MockModelProvider::new("boxed-model"));
    let embed_arc: Arc<dyn EmbeddingProvider> =
        Arc::new(MockEmbeddingProvider::new("arc-embed", 8));

    assert_eq!(model_box.name(), "boxed-model");
    assert_eq!(embed_arc.name(), "arc-embed");

    let comp_req = CompletionRequest::new("m", "boxed prompt");
    let comp_res = model_box
        .complete(&comp_req)
        .await
        .expect("boxed complete should work");
    assert_eq!(comp_res.text, "Echo: boxed prompt");

    let embed_req = EmbeddingRequest::single("e", "arc input");
    let embed_res = embed_arc
        .embed(&embed_req)
        .await
        .expect("arc embed should work");
    assert_eq!(embed_res.embeddings.len(), 1);
    assert_eq!(embed_res.embeddings[0].len(), 8);
}

#[test]
fn chat_message_constructors_and_serde() {
    let sys = ChatMessage::system("instructions");
    assert_eq!(sys.role, MessageRole::System);
    assert_eq!(sys.content, "instructions");

    let user = ChatMessage::user("question");
    assert_eq!(user.role, MessageRole::User);
    assert_eq!(user.content, "question");

    let assistant = ChatMessage::assistant("answer");
    assert_eq!(assistant.role, MessageRole::Assistant);
    assert_eq!(assistant.content, "answer");

    // Serde round trip
    let json = serde_json::to_string(&user).expect("serialization should succeed");
    let round_trip: ChatMessage =
        serde_json::from_str(&json).expect("deserialization should succeed");
    assert_eq!(round_trip, user);
}
