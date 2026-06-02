//! Gemini-family scenarios.
//!
//! `thinkingBudget: 0 → -1` rewrite is not e2e-tested: the unit test
//! `fix_thinking_budget_zero_becomes_negative_one` verifies the rewrite
//! directly, and the upstream's acceptance of `-1` (Gemini's documented
//! "auto" sentinel) is stable.

#![cfg(feature = "e2e")]

use serde_json::json;

use crate::harness::{
    assertions::{assert_gemini_response, read_json_status, read_status_and_body, skip},
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
};

#[tokio::test]
async fn generate_content_action_routes() {
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
            "{}/gemini/models/{model}:generateContent",
            acr.base_url()
        )),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let json = read_json_status(resp, 200, "generateContent").await;
    assert_gemini_response(&json);
}

#[tokio::test]
async fn stream_generate_content_action_routes() {
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
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "streamGenerateContent: {body}");
}
