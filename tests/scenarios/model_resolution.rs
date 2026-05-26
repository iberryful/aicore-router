//! Model resolution — exact, alias, fallback, suffix-stripping, and unsupported.

#![cfg(feature = "e2e")]

use serde_json::json;

use crate::harness::{
    assertions::{read_status_and_body, skip},
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
};

fn chat_payload(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_completion_tokens": 16,
    })
}

fn messages_payload(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_tokens": 16,
    })
}

#[tokio::test]
async fn exact_name_resolves() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let resp = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&chat_payload(model))
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "exact name: {body}");
}

/// Family fallback — request a model whose family is configured but whose
/// exact name isn't, expecting the configured `fallback_models.<family>` to
/// take over.
#[tokio::test]
async fn family_fallback_for_unknown_claude_name() {
    let acr = shared().await;
    if acr.config.fallback_claude.is_none() {
        skip("no Claude fallback configured");
        return;
    }
    let resp = auth_bearer(
        client().post(format!("{}/v1/messages", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&messages_payload("claude-this-name-does-not-exist-12345"))
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "claude family fallback: {body}");
}

/// `[1m]` extended-context suffix is silently stripped.
#[tokio::test]
async fn extended_context_suffix_stripped() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("claude") else {
        skip("no Claude model configured");
        return;
    };
    let suffixed = format!("{model}[1m]");
    let resp = auth_bearer(
        client().post(format!("{}/v1/messages", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&messages_payload(&suffixed))
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "[1m] suffix stripped: {body}");
}

/// Unsupported families bounce with a 400 and an actionable error message
/// pointing the user at the SAP AI Core SDK directly.
#[tokio::test]
async fn unsupported_family_returns_400() {
    let acr = shared().await;
    let resp = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&chat_payload("mistral-large-2402"))
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 400, "unsupported family: {body}");
    assert!(
        body.contains("SAP AI Core SDK"),
        "expected error to mention 'SAP AI Core SDK', got: {body}"
    );
}

/// Missing `model` field in body → 400 "model is required".
#[tokio::test]
async fn missing_model_field_returns_400() {
    let acr = shared().await;
    let body = json!({
        "messages": [{"role": "user", "content": "hi"}],
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    assert_eq!(resp.status().as_u16(), 400);
}

/// Gemini's path encodes `model:action`. A bad shape (no colon) is rejected
/// with 400 before any upstream call.
#[tokio::test]
async fn gemini_malformed_model_operation_returns_400() {
    let acr = shared().await;
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
    });
    let resp = auth_bearer(
        client().post(format!("{}/gemini/models/gemini-2.5-flash", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    assert_eq!(resp.status().as_u16(), 400);
}
