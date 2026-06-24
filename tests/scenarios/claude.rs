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

/// Opus 4.7/4.8 reject `thinking.type.enabled` and non-1 sampling params
/// upstream; `transforms::anthropic::apply_adaptive_thinking_overrides` strips
/// those params and converts `thinking` to `adaptive` form. This test verifies
/// the end-to-end effect: a request the model would otherwise 400 on succeeds
/// AND actually produces thinking output (non-zero `thinking_tokens` or a
/// thinking content block).
///
/// Skips if the user's config doesn't expose claude-opus-4-8. Note that
/// `model_for_family("claude-opus-4-8")` matches the full name exactly via the
/// `starts_with` check in `model_for_family`, so it returns the model only
/// when 4.8 is explicitly listed.
#[tokio::test]
async fn opus_4_8_adaptive_thinking_round_trip() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("claude-opus-4-8") else {
        skip("claude-opus-4-8 not configured");
        return;
    };
    let body = json!({
        "model": model,
        "max_tokens": 2048,
        // Both `temperature` (non-1) and `top_p`/`top_k` would 400 upstream
        // on this model. The override strips them.
        "temperature": 0.7,
        "top_p": 0.9,
        "top_k": 40,
        // `enabled` would 400 upstream; the override converts it to `adaptive`.
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [{
            "role": "user",
            "content": "Compute 137 divided by 3, rounded to 2 decimal places. Then estimate 137 * 47 / 3.",
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
    let json = read_json_status(resp, 200, "opus 4.8 adaptive thinking").await;
    assert_messages_response(&json);

    let thinking_tokens = json
        .pointer("/usage/output_tokens_details/thinking_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let has_thinking_block = json["content"]
        .as_array()
        .map(|arr| arr.iter().any(|b| b["type"] == "thinking"))
        .unwrap_or(false);
    assert!(
        thinking_tokens > 0 || has_thinking_block,
        "expected adaptive thinking to produce thinking output (thinking_tokens \
         > 0 or thinking content block); response: {json}"
    );
}
