//! Health & discovery endpoints — neither requires upstream calls, so they're
//! the cheapest way to confirm the router is up and the model registry has
//! discovered configured deployments.

#![cfg(feature = "e2e")]

use crate::harness::{assertions::skip, process::shared};

#[tokio::test]
async fn health_returns_ok() {
    let acr = shared().await;
    let resp = reqwest::get(format!("{}/health", acr.base_url()))
        .await
        .expect("GET /health");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("/health body");
    assert_eq!(body, "OK");
}

#[tokio::test]
async fn models_endpoint_lists_configured_models() {
    let acr = shared().await;
    let resp = reqwest::get(format!("{}/v1/models", acr.base_url()))
        .await
        .expect("GET /v1/models");
    assert!(resp.status().is_success());
    let json: serde_json::Value = resp.json().await.expect("parse /v1/models");

    assert_eq!(json["object"], "list");
    let data = json["data"].as_array().expect("data array on /v1/models");

    if data.is_empty() {
        skip("model registry has not yet discovered any deployments — try again later");
        return;
    }

    // At least one configured model should be listed; not all configured
    // models are guaranteed (the registry only surfaces models with a live
    // deployment). Look for any overlap.
    let listed: std::collections::HashSet<String> = data
        .iter()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect();
    let configured: std::collections::HashSet<String> =
        acr.config.model_names.iter().cloned().collect();
    let overlap = configured.intersection(&listed).count();
    assert!(
        overlap > 0,
        "expected /v1/models to list at least one configured model. \
         configured={configured:?}, listed={listed:?}"
    );

    // Every model with a known context length should expose it in /v1/models.
    for m in data {
        if let Some(name) = m["id"].as_str()
            && aicore_router::constants::get_context_length(name).is_some()
        {
            assert!(
                m.get("context_length").is_some(),
                "model {name} should have context_length"
            );
        }
    }
}
