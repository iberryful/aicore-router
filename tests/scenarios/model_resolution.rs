//! Model resolution — only the case that genuinely needs a live backend.
//!
//! Pure-function paths (`normalize_model`, `determine_family`, glob/alias
//! matching, `parse_model_operation`) are exhaustively covered by unit
//! tests in `src/proxy.rs`, `src/registry.rs`, and `src/routes.rs`.
//! E2E only earns its keep when the assertion depends on live state —
//! here, the registry's discovery loop having actually populated a
//! deployment for the configured fallback.

#![cfg(feature = "e2e")]

use serde_json::json;

use crate::harness::{
    assertions::{read_status_and_body, skip},
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
};

/// Family fallback — request a Claude name that's neither configured nor
/// matches any alias. The router should select `fallback_models.claude`
/// from the user's config; that fallback must itself have a live
/// deployment for the upstream call to succeed. This is the part unit
/// tests can't reach: the lookup goes through the running registry.
#[tokio::test]
async fn family_fallback_for_unknown_claude_name() {
    let acr = shared().await;
    if acr.config.fallback_claude.is_none() {
        skip("no Claude fallback configured");
        return;
    }
    let body = json!({
        "model": "claude-this-name-does-not-exist-12345",
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_tokens": 16,
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
    assert_eq!(status, 200, "claude family fallback: {body}");
}
