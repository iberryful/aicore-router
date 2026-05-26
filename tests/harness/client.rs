//! HTTP client helpers — auth header variants and a small wrapper that
//! attaches a uniform timeout to every request.

#![cfg(feature = "e2e")]

use std::time::Duration;

use reqwest::{Client, RequestBuilder};

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub fn client() -> Client {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("reqwest client")
}

pub fn auth_bearer(req: RequestBuilder, key: &str) -> RequestBuilder {
    req.header("Authorization", format!("Bearer {key}"))
}

pub fn auth_api_key(req: RequestBuilder, key: &str) -> RequestBuilder {
    req.header("api-key", key)
}

pub fn auth_x_api_key(req: RequestBuilder, key: &str) -> RequestBuilder {
    req.header("x-api-key", key)
}

pub fn auth_x_goog_api_key(req: RequestBuilder, key: &str) -> RequestBuilder {
    req.header("x-goog-api-key", key)
}
