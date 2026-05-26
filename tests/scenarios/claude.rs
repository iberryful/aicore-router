//! Claude-family scenarios — tool use and vision.
//!
//! `cache_control` TTL injection is not e2e-tested: the unit tests
//! `inject_cache_ttl_writes_1h_into_ephemeral_blocks` and
//! `inject_cache_ttl_is_idempotent_when_ttl_already_set` directly verify
//! that the TTL field is added to the body. An e2e assertion can only
//! observe "upstream accepts the request," which doesn't prove injection
//! happened — the request would also succeed if the field were silently
//! dropped.
//!
//! `Anthropic-Beta` header propagation is also not tested here — see the
//! commit history for the rationale (deferred to a future change that
//! filters Bedrock-irrelevant Anthropic-direct flags).

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
