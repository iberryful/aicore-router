//! Health endpoint — the cheapest readiness signal that doesn't depend on
//! upstream availability.
//!
//! `/v1/models` is intentionally not e2e-tested here: the registry's
//! lookup logic is covered by unit tests in `src/registry.rs`, and the
//! HTTP envelope offers little wire-level signal beyond serialization.

#![cfg(feature = "e2e")]

use crate::harness::process::shared;

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
