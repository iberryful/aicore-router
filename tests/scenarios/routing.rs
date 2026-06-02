//! Path equivalence — alternate routes that should behave identically to
//! their canonical counterparts.

#![cfg(feature = "e2e")]

use serde_json::json;

use crate::harness::{
    assertions::{read_status_and_body, skip},
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
};

/// `/anthropic/v1/messages` is wired to the same handler as `/v1/messages`.
#[tokio::test]
async fn anthropic_path_routes_to_messages() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("claude") else {
        skip("no Claude model configured");
        return;
    };
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_tokens": 16,
    });
    let resp = auth_bearer(
        client().post(format!("{}/anthropic/v1/messages", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "/anthropic/v1/messages: {body}");
}

/// `/litellm/v1/chat/completions` is wired to the same handler as
/// `/v1/chat/completions` for LiteLLM-prefixed clients.
#[tokio::test]
async fn litellm_path_routes_to_chat_completions() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_completion_tokens": 16,
    });
    let resp = auth_bearer(
        client().post(format!("{}/litellm/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "/litellm/v1/chat/completions: {body}");
}

/// `/gemini/v1beta/models/...` is an alias for `/gemini/models/...`.
#[tokio::test]
async fn gemini_v1beta_path_equivalent() {
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
            "{}/gemini/v1beta/models/{model}:generateContent",
            acr.base_url()
        )),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "/gemini/v1beta/models: {body}");
}

/// `/v1beta/models/...` (Gemini-native, without the `/gemini` prefix) is
/// also accepted — Google's own SDKs hit this path.
#[tokio::test]
async fn gemini_native_v1beta_path_equivalent() {
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
            "{}/v1beta/models/{model}:generateContent",
            acr.base_url()
        )),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "/v1beta/models: {body}");
}

/// Azure-shaped `/openai/deployments/{model}/chat/completions` — model name
/// is taken from the path; the body need not include it.
#[tokio::test]
async fn azure_openai_deployment_path() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "messages": [{"role": "user", "content": "Reply with one short word."}],
        "max_completion_tokens": 16,
    });
    let resp = auth_bearer(
        client().post(format!(
            "{}/openai/deployments/{model}/chat/completions",
            acr.base_url()
        )),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "/openai/deployments/{{model}}: {body}");
}
