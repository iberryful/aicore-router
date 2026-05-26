//! Claude-family scenarios — tool use, vision, cache_control TTL.
//!
//! `Anthropic-Beta` header propagation is **not** tested here. The
//! `transforms::extract_anthropic_beta_*` unit tests cover the remap and
//! pass-through logic directly; AI Core's beta-flag allowlist is upstream-
//! controlled and unstable, so a positive e2e assertion (request must
//! succeed) ends up testing the upstream's allowlist rather than acr's
//! propagation.

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
