//! Rate-limit and quota enforcement.
//!
//! These tests use dedicated synthesized API keys with deliberately tight
//! per-key limits (`KEY_RPM_LIMITED`, `KEY_TIGHT_TOKENS`) so we can drive
//! deterministic 429 responses without affecting the default key path.

#![cfg(feature = "e2e")]

use std::time::Duration;

use serde_json::json;

use crate::harness::{
    assertions::skip,
    client::{auth_bearer, client},
    config_synth::{KEY_RPM_LIMITED, KEY_TIGHT_TOKENS},
    process::shared,
};

fn small_chat(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_completion_tokens": 16,
    })
}

/// `KEY_RPM_LIMITED` allows 3 req/min. Burst more than that — the next
/// request should be 429 with a `Retry-After` header.
#[tokio::test]
async fn per_key_rpm_returns_429_with_retry_after() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };

    // The governor token bucket is per minute, so a quick burst of 5 must
    // exhaust the budget regardless of upstream latency.
    let mut last_status = 0u16;
    let mut last_retry_after: Option<String> = None;
    for _ in 0..5 {
        let resp = auth_bearer(
            client().post(format!("{}/v1/chat/completions", acr.base_url())),
            KEY_RPM_LIMITED,
        )
        .json(&small_chat(model))
        .send()
        .await
        .expect("request");
        last_status = resp.status().as_u16();
        last_retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        if last_status == 429 {
            break;
        }
    }
    assert_eq!(
        last_status, 429,
        "expected 429 after RPM burst; last_status={last_status}"
    );
    assert!(
        last_retry_after.is_some(),
        "expected Retry-After header on 429 response"
    );
}

/// Repeated invalid auths from loopback should trip the per-IP auth lockout
/// and produce 429.
#[tokio::test]
async fn auth_lockout_after_repeated_invalid_keys() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };

    // Hammer with bad keys — eventually the AuthRateLimiter should fire.
    let mut got_429 = false;
    for _ in 0..15 {
        let resp = auth_bearer(
            client().post(format!("{}/v1/chat/completions", acr.base_url())),
            "definitely-not-a-real-key",
        )
        .json(&small_chat(model))
        .send()
        .await
        .expect("request");
        if resp.status().as_u16() == 429 {
            got_429 = true;
            break;
        }
        // Tiny pause so we don't overwhelm the upstream HTTP client backlog.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        got_429,
        "expected auth-lockout 429 after repeated invalid keys"
    );
}

/// `KEY_TIGHT_TOKENS` has a 50-token daily limit. A single small request
/// will exceed that on its second invocation since AI Core counts even a
/// minimal chat at >50 prompt tokens.
#[tokio::test]
async fn daily_token_limit_returns_429() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    // First call: succeeds, but the resulting usage row exceeds the 50-token
    // budget. Quota tracking is async so we re-check after a brief pause.
    let r1 = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_TIGHT_TOKENS,
    )
    .json(&small_chat(model))
    .send()
    .await
    .expect("request");
    if !r1.status().is_success() && r1.status().as_u16() != 429 {
        skip(&format!(
            "tight-tokens key blocked at first call with {} — possibly stale quota state from previous run",
            r1.status()
        ));
        return;
    }
    let _ = r1.bytes().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Second call should be 429 since the first consumed >50 tokens.
    let r2 = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_TIGHT_TOKENS,
    )
    .json(&small_chat(model))
    .send()
    .await
    .expect("request");
    assert_eq!(r2.status().as_u16(), 429, "expected 429 from token quota");
    assert!(
        r2.headers().get("retry-after").is_some(),
        "expected Retry-After on quota 429"
    );
}
