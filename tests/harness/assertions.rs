//! Higher-level assertion helpers.
//!
//! Tests should fail with messages that point straight at the cause; these
//! helpers wrap the low-level checks so each scenario stays terse.

#![cfg(feature = "e2e")]

use reqwest::Response;
use serde_json::Value;

/// Read a response body once, asserting that the status matches and
/// returning the parsed JSON. Body text is included in panic messages so
/// upstream API errors are visible in test output.
pub async fn read_json_status(response: Response, expected: u16, ctx: &str) -> Value {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(
        status, expected,
        "{ctx}: expected HTTP {expected}, got {status} — body: {body}"
    );
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("{ctx}: response body was not valid JSON ({e}) — body: {body}"))
}

/// Read a response body, returning (status, parsed_or_text).
pub async fn read_status_and_body(response: Response) -> (u16, String) {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    (status, body)
}

/// Assert an OpenAI Chat Completions response has the expected shape.
pub fn assert_chat_completion(json: &Value) {
    let choices = json
        .get("choices")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("chat completion missing `choices` array: {json}"));
    assert!(!choices.is_empty(), "chat completion `choices` was empty");
    assert!(
        json.get("usage").is_some(),
        "chat completion missing `usage`: {json}"
    );
}

/// Assert a Claude Messages response has the expected shape.
pub fn assert_messages_response(json: &Value) {
    let role = json.get("role").and_then(|v| v.as_str());
    assert_eq!(
        role,
        Some("assistant"),
        "messages response role mismatch: {json}"
    );
    let content = json
        .get("content")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("messages response missing `content` array: {json}"));
    assert!(!content.is_empty(), "messages `content` was empty");
    assert!(
        json.get("usage").is_some(),
        "messages response missing `usage`: {json}"
    );
}

/// Assert an OpenAI Responses-API non-streaming response has the expected shape.
pub fn assert_responses_response(json: &Value) {
    let object = json.get("object").and_then(|v| v.as_str());
    assert_eq!(
        object,
        Some("response"),
        "responses-API object mismatch: {json}"
    );
    assert!(
        json.get("usage").is_some(),
        "responses-API missing `usage`: {json}"
    );
}

/// Assert an OpenAI embeddings response has the expected shape.
pub fn assert_embeddings_response(json: &Value) {
    let data = json
        .get("data")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("embeddings response missing `data` array: {json}"));
    assert!(!data.is_empty(), "embeddings `data` was empty");
    let first = &data[0];
    let embedding = first
        .get("embedding")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("first embedding missing `embedding` array: {first}"));
    assert!(
        !embedding.is_empty(),
        "first embedding vector was empty: {first}"
    );
}

/// Assert a Gemini generateContent response has at least one candidate.
pub fn assert_gemini_response(json: &Value) {
    let candidates = json
        .get("candidates")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("gemini response missing `candidates`: {json}"));
    assert!(!candidates.is_empty(), "gemini `candidates` was empty");
}

/// Helper: skip a test with a clear stderr message when the user's config
/// lacks the prerequisites for that scenario.
pub fn skip(reason: &str) {
    eprintln!("\n  ⚠ skipping — {reason}\n");
}
