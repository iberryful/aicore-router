//! OpenAI Responses API (`/v1/responses`) — used by Codex CLI v0.130+.

#![cfg(feature = "e2e")]

use aicore_router::proxy::LlmFamily;
use serde_json::json;

use crate::harness::{
    assertions::{assert_responses_response, read_json_status, read_status_and_body, skip},
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
    sse::{final_usage, parse_sse},
};

#[tokio::test]
async fn responses_non_stream_returns_usage() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "model": model,
        "input": "Reply with one short word.",
        "max_output_tokens": 16,
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/responses", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let json = read_json_status(resp, 200, "/v1/responses").await;
    assert_responses_response(&json);
}

#[tokio::test]
async fn responses_stream_terminates_with_completed_event_and_usage() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "model": model,
        "input": "Reply with one short word.",
        "max_output_tokens": 16,
        "stream": true,
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/responses", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    assert!(resp.status().is_success());

    let events = parse_sse(resp).await;
    assert!(!events.is_empty(), "no events received from /v1/responses");

    // The terminal event should be one of the documented usage-bearing types.
    let terminal_seen = events.iter().any(|ev| {
        ev.json
            .as_ref()
            .and_then(|v| v["type"].as_str())
            .is_some_and(|t| {
                matches!(
                    t,
                    "response.completed" | "response.incomplete" | "response.failed"
                )
            })
    });
    assert!(
        terminal_seen,
        "expected one of response.completed/incomplete/failed in stream"
    );

    let usage = final_usage(&events, LlmFamily::OpenAiResponses)
        .expect("usage on terminal /v1/responses event");
    assert!(usage.input_tokens.unwrap_or(0) > 0);
}

/// AI Core 400s on Codex's `custom` / `web_search` / `tool_search` tool
/// types; `transforms::openai_responses::prepare` filters them. The request
/// should still succeed end-to-end.
#[tokio::test]
async fn responses_with_filtered_tools_succeeds() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "model": model,
        "input": "Reply with one short word.",
        "max_output_tokens": 16,
        "tools": [
            {"type": "custom", "name": "codex_internal"},
            {"type": "web_search"},
            {"type": "tool_search"}
        ],
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/responses", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "filtered tools: {body}");
}

/// `/v1/responses/compact` is the Codex auto-compact-remote endpoint —
/// passthrough handler with the `compact` URL action. Returns the same
/// shape as the create endpoint plus an `instructions` field.
#[tokio::test]
async fn responses_compact_endpoint_returns_usage() {
    let acr = shared().await;
    let Some(model) = acr.config.model_for_family("gpt") else {
        skip("no OpenAI model configured");
        return;
    };
    let body = json!({
        "model": model,
        "input": "Summarize: This is a short sample conversation.",
        "instructions": "Compact this conversation.",
        "max_output_tokens": 32,
    });
    let resp = auth_bearer(
        client().post(format!("{}/v1/responses/compact", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let json = read_json_status(resp, 200, "/v1/responses/compact").await;
    assert!(
        json.get("usage").is_some(),
        "compact response missing usage: {json}"
    );
}
