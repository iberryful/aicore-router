//! Verifies the request-logging side effect: a successful request appears
//! in the SQLite request log with token stats populated.

#![cfg(feature = "e2e")]

use std::time::Duration;

use aicore_router::{database::Database, quota::hash_api_key};
use serde_json::json;

use crate::harness::{
    assertions::{read_status_and_body, skip},
    client::{auth_bearer, client},
    config_synth::KEY_DEFAULT,
    process::shared,
};

/// Issue a non-streaming OpenAI chat call, then open the synthesized DB
/// read-only and confirm a row exists for the default test key with
/// non-zero token counts.
#[tokio::test]
async fn successful_request_persists_row_with_token_stats() {
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
        client().post(format!("{}/v1/chat/completions", acr.base_url())),
        KEY_DEFAULT,
    )
    .json(&body)
    .send()
    .await
    .expect("request");
    let (status, body) = read_status_and_body(resp).await;
    assert_eq!(status, 200, "chat: {body}");

    // Insertion happens in a tokio::spawn'd task — wait briefly.
    tokio::time::sleep(Duration::from_millis(750)).await;

    let db_path = acr.config.db_path.to_str().expect("db path utf8");
    let db = Database::open_readonly(db_path).expect("open db readonly");
    let key_hash = hash_api_key(KEY_DEFAULT);
    let usage = db
        .query_usage(
            Some(&key_hash),
            "1970-01-01 00:00:00",
            aicore_router::database::GroupBy::Day,
        )
        .await
        .expect("query usage");

    assert!(
        !usage.is_empty(),
        "no usage rows for default test key — DB at {db_path} did not record"
    );
    let total_input: u64 = usage.iter().map(|u| u.input_tokens).sum();
    assert!(
        total_input > 0,
        "expected input_tokens > 0 across {} rows, got 0",
        usage.len()
    );
    let request_count: u64 = usage.iter().map(|u| u.request_count).sum();
    assert!(
        request_count >= 1,
        "expected request_count >= 1, got {request_count}"
    );
}
