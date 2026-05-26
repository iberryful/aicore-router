//! OpenAI Chat Completions specific behaviors — `max_tokens` rename,
//! streaming usage, o-series, embeddings, Codex-CLI preamble normalization.

#![cfg(feature = "e2e")]

use aicore_router::proxy::LlmFamily;
use serde_json::json;

use crate::harness::{
    assertions::{assert_chat_completion, assert_embeddings_response, read_json_status, skip},
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
    sse::{final_usage, parse_sse},
};

/// `max_tokens` (legacy) should be transparently renamed to
/// `max_completion_tokens` so the request reaches AI Core successfully.
#[tokio::test]
async fn max_tokens_only_request_succeeds() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_tokens": 16,
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let json = read_json_status(resp, 200, "max_tokens rename").await;
    assert_chat_completion(&json);
}

/// The streaming final chunk should carry usage — `prepare` injects
/// `stream_options.include_usage: true` automatically.
#[tokio::test]
async fn streaming_final_chunk_carries_usage() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_completion_tokens": 16,
        "stream": true,
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    assert!(resp.status().is_success());

    let events = parse_sse(resp).await;
    assert!(!events.is_empty(), "stream produced no events");
    let usage = final_usage(&events, LlmFamily::OpenAi)
        .expect("expected `usage` on final OpenAI chat-completions chunk");
    assert!(
        usage.input_tokens.unwrap_or(0) > 0,
        "input_tokens should be > 0, got {usage:?}"
    );
}

/// OpenAI embeddings: `text-embedding-*` family routes to the embeddings
/// endpoint and returns a `data[].embedding` array.
#[tokio::test]
async fn embeddings_text_embedding_returns_data() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("text") else {
        skip("no text-embedding-* model configured");
        return;
    };
    let body = json!({
        "model": model,
        "input": "embed me",
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/embeddings", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let json = read_json_status(resp, 200, "embeddings").await;
    assert_embeddings_response(&json);
}

/// Codex CLI sometimes inserts an assistant content "preamble" between an
/// `assistant(tool_calls)` and the matching `tool(response)`. The router
/// merges that in `transforms::openai::normalize_messages`. Verify the
/// upstream accepts the resulting (legal) shape.
#[tokio::test]
async fn codex_preamble_normalization_succeeds() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "model": model,
        "messages": [
            {"role": "user", "content": "what's 2+2?"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_test_1",
                    "type": "function",
                    "function": {"name": "calc", "arguments": "{\"expr\":\"2+2\"}"}
                }]
            },
            {"role": "assistant", "content": "Let me calculate."},
            {"role": "tool", "tool_call_id": "call_test_1", "content": "4"}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "calc",
                "description": "evaluate arithmetic",
                "parameters": {
                    "type": "object",
                    "properties": {"expr": {"type": "string"}},
                    "required": ["expr"]
                }
            }
        }],
        "max_completion_tokens": 32,
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let json = read_json_status(resp, 200, "codex preamble").await;
    assert_chat_completion(&json);
}
