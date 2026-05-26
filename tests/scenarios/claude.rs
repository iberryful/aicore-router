//! Claude-family scenarios — tool use, vision, anthropic-beta header,
//! cache_control TTL, long-context auto-injection.

#![cfg(feature = "e2e")]

use serde_json::json;

use crate::harness::{
    assertions::{assert_messages_response, read_json_status, read_status_and_body, skip},
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
};

#[tokio::test]
async fn tool_use_round_trip() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("claude") else {
        skip("no Claude model configured");
        return;
    };
    let body = json!({
        "model": model,
        "max_tokens": 256,
        "tools": [{
            "name": "get_weather",
            "description": "Get current weather for a city",
            "input_schema": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }],
        "messages": [
            {"role": "user", "content": "Use the get_weather tool for Berlin."}
        ],
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/messages", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let json = read_json_status(resp, 200, "tool use").await;
    assert_messages_response(&json);
}

#[tokio::test]
async fn vision_inline_image_block() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("claude") else {
        skip("no Claude model configured");
        return;
    };
    // 1x1 transparent PNG (base64).
    let pixel = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=";
    let body = json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image", "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": pixel
                }},
                {"type": "text", "text": "What color is this pixel? Reply in one word."}
            ]
        }],
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/messages", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "vision: {body}");
}

#[tokio::test]
async fn anthropic_beta_header_propagates() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("claude") else {
        skip("no Claude model configured");
        return;
    };
    let body = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/messages", acr.base_url())),
        KEY_DEFAULT,
    )
    .header("Anthropic-Beta", "prompt-caching-2024-07-31")
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "Anthropic-Beta: {body}");
}

/// `cache_control: { type: "ephemeral" }` on a system block — the router
/// injects `ttl: "1h"` automatically.
#[tokio::test]
async fn cache_control_with_ttl() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("claude") else {
        skip("no Claude model configured");
        return;
    };
    let body = json!({
        "model": model,
        "max_tokens": 16,
        "system": [{
            "type": "text",
            "text": "You are a brief assistant.",
            "cache_control": {"type": "ephemeral"}
        }],
        "messages": [{"role": "user", "content": "Reply with one short word."}],
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/messages", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "cache_control TTL: {body}");
}
