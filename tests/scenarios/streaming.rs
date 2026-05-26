//! Streaming wire format — per-family event shape.

#![cfg(feature = "e2e")]

use serde_json::json;

use crate::harness::{
    assertions::skip,
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
    sse::parse_sse,
};

/// Claude streams emit explicit `event: <type>` lines so SSE clients can key
/// off named events without parsing JSON. (See `proxy::format_sse_event`.)
#[tokio::test]
async fn claude_stream_emits_explicit_event_lines() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("claude") else {
        skip("no Claude model configured");
        return;
    };
    let body = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "stream": true,
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/messages", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    assert!(resp.status().is_success());
    let events = parse_sse(resp).await;
    assert!(!events.is_empty(), "no events from Claude stream");
    let with_event = events.iter().filter(|e| e.event.is_some()).count();
    assert!(
        with_event > 0,
        "Claude stream should include explicit `event:` lines for at least \
         one event; got {events:?}"
    );
}

/// OpenAI Chat Completions streaming chunks all use the `data: ` prefix.
/// Verified by checking that every event parsed has a JSON payload (no
/// stray non-`data:` lines made it through as events).
#[tokio::test]
async fn openai_stream_chunks_use_data_prefix() {
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
    assert!(!events.is_empty());
    for ev in &events {
        assert!(
            ev.json.is_some(),
            "every OpenAI streaming event should be valid JSON, got: {}",
            ev.data
        );
    }
}

/// Gemini stream chunks should each be valid JSON.
#[tokio::test]
async fn gemini_stream_chunks_parse_as_json() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gemini") else {
        skip("no Gemini model configured");
        return;
    };
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "Reply with one short word."}]}],
    });
    let resp = auth_bearer(
        client().post(format!(
            "{}/gemini/models/{model}:streamGenerateContent",
            acr.base_url()
        )),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    assert!(resp.status().is_success());
    let events = parse_sse(resp).await;
    assert!(!events.is_empty(), "no events from Gemini stream");
    for ev in &events {
        assert!(
            ev.json.is_some(),
            "every Gemini streaming event should be valid JSON, got: {}",
            ev.data
        );
    }
}
