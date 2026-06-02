//! Auth header variants — `extract_api_key` reads four header names plus
//! `Authorization: Bearer`. Each should authenticate equivalently.

#![cfg(feature = "e2e")]

use serde_json::json;

use crate::harness::{
    assertions::{read_status_and_body, skip},
    client::{auth_api_key, auth_bearer, auth_x_api_key, auth_x_goog_api_key, client},
    config_synth::KEY_DEFAULT,
    process::shared,
};

fn small_chat_body(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_completion_tokens": 16,
    })
}

#[tokio::test]
async fn bearer_auth_succeeds() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let resp = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&small_chat_body(model))
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "bearer auth: {body}");
}

#[tokio::test]
async fn api_key_header_succeeds() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let resp = auth_api_key(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&small_chat_body(model))
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "api-key header: {body}");
}

#[tokio::test]
async fn x_api_key_header_succeeds() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let resp = auth_x_api_key(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&small_chat_body(model))
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "x-api-key header: {body}");
}

#[tokio::test]
async fn x_goog_api_key_header_succeeds() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gemini") else {
        skip("no Gemini model configured");
        return;
    };
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "Reply with one short word."}]}],
    });
    let resp = auth_x_goog_api_key(
        client().post(format!(
            "{}/gemini/models/{model}:generateContent",
            acr.base_url()
        )),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "x-goog-api-key header: {body}");
}

#[tokio::test]
async fn missing_auth_returns_401() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let resp = client()
        .post(format!("{}/v1/chat/completions", acr.base_url()))
        .json(&small_chat_body(model))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn invalid_key_returns_401() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let resp = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        "definitely-not-a-real-key",
    )
    .json(&small_chat_body(model))
    .send()
    .await
    .expect("request");
    assert_eq!(resp.status().as_u16(), 401);
}
