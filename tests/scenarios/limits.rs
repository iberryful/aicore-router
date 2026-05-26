//! Per-key rate-limit and quota enforcement.
//!
//! These tests use dedicated synthesized API keys with deliberately tight
//! per-key limits (`KEY_RPM_LIMITED`, `KEY_TIGHT_TOKENS`) so we can drive
//! deterministic 429 responses without affecting the default key path.
//!
//! Per-IP auth lockout is **not** tested here. The `AuthRateLimiter`'s
//! state machine has thorough unit coverage in `src/rate_limit.rs`, and
//! triggering it from a shared-process e2e suite would lock out 127.0.0.1
//! for ~30s and cascade-fail every subsequent test in the run.

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

/// `KEY_TIGHT_TOKENS` has a 10-token daily limit, deliberately set below
/// the per-request token cost (~28 for a minimal chat). The first call's
/// recorded usage alone exceeds the budget, so the second call must 429.
#[tokio::test]
async fn daily_token_limit_returns_429() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };

    // First call must succeed — the synthesized DB is a fresh tempfile per
    // test run, so no prior usage exists for KEY_TIGHT_TOKENS.
    let r1 = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_TIGHT_TOKENS,
    )
    .json(&small_chat(model))
    .send()
    .await
    .expect("request");
    let r1_status = r1.status().as_u16();
    let r1_body = r1.text().await.unwrap_or_default();
    assert_eq!(
        r1_status, 200,
        "expected first KEY_TIGHT_TOKENS request to succeed; got {r1_status}: {r1_body}"
    );

    // Quota recording is async (tokio::spawn off the response path), so wait
    // for the usage write before issuing the follow-up request.
    tokio::time::sleep(Duration::from_millis(750)).await;

    // Second call should be 429 with Retry-After since the first call's
    // recorded tokens already exceed the 10-token daily budget.
    let r2 = auth_bearer(
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_TIGHT_TOKENS,
    )
    .json(&small_chat(model))
    .send()
    .await
    .expect("request");
    assert_eq!(
        r2.status().as_u16(),
        429,
        "expected 429 from token quota after first call recorded usage > 10"
    );
    assert!(
        r2.headers().get("retry-after").is_some(),
        "expected Retry-After on quota 429"
    );
}
