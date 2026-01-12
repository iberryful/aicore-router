//! Token management for OAuth authentication with multiple providers.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Provider;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Debug, Clone)]
struct TokenInfo {
    token: String,
    expires_at: DateTime<Utc>,
}

impl TokenInfo {
    fn is_valid(&self) -> bool {
        Utc::now() + chrono::Duration::seconds(60) < self.expires_at
    }
}

/// Token manager that handles OAuth tokens for multiple providers.
#[derive(Debug, Clone)]
pub struct TokenManager {
    /// Set of valid API keys for request authentication
    api_keys: HashSet<String>,
    /// Cached tokens keyed by provider credentials hash
    tokens: Arc<RwLock<HashMap<String, TokenInfo>>>,
    /// HTTP client for token requests
    client: Client,
}

impl TokenManager {
    /// Create a new token manager with the given API keys.
    pub fn new(api_keys: Vec<String>) -> Self {
        Self {
            api_keys: api_keys.into_iter().collect(),
            tokens: Arc::new(RwLock::new(HashMap::new())),
            client: Client::new(),
        }
    }

    /// Check if an API key is valid.
    /// The special "internal" key is always valid for internal operations.
    pub fn is_valid_api_key(&self, api_key: &str) -> bool {
        api_key == "internal" || self.api_keys.contains(api_key)
    }

    /// Get an OAuth token for a specific provider.
    /// Returns None if the API key is invalid.
    pub async fn get_token_for_provider(
        &self,
        api_key: &str,
        provider: &Provider,
    ) -> Result<Option<String>> {
        if !self.is_valid_api_key(api_key) {
            return Ok(None);
        }

        let token_key = format!(
            "{}:{}:{}",
            provider.uaa_token_url, provider.uaa_client_id, provider.uaa_client_secret
        );

        // Check cache first
        {
            let tokens = self.tokens.read().await;
            if let Some(token_info) = tokens.get(&token_key)
                && token_info.is_valid()
            {
                return Ok(Some(token_info.token.clone()));
            }
        }

        // Refresh token
        let new_token = self
            .refresh_token(
                &provider.uaa_token_url,
                &provider.uaa_client_id,
                &provider.uaa_client_secret,
            )
            .await?;

        // Store in cache
        {
            let mut tokens = self.tokens.write().await;
            tokens.insert(token_key, new_token.clone());
        }

        Ok(Some(new_token.token))
    }

    async fn refresh_token(
        &self,
        url: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<TokenInfo> {
        let params = [("grant_type", "client_credentials")];

        let response = self
            .client
            .post(url)
            .form(&params)
            .basic_auth(client_id, Some(client_secret))
            .send()
            .await
            .context("Failed to send token request")?;

        let status = response.status();
        let headers = response.headers().clone();

        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            tracing::error!(
                "OAuth token request failed - Status: {}, Headers: {:?}, Body: {}",
                status,
                headers,
                text
            );
            return Err(anyhow::anyhow!(
                "Token request failed with status {}: {}",
                status,
                text
            ));
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .context("Failed to parse token response")?;

        let expires_at = Utc::now() + chrono::Duration::seconds(token_response.expires_in as i64);

        tracing::debug!(
            "Token refreshed for client id: {} (expires in {}s)",
            client_id,
            token_response.expires_in
        );

        Ok(TokenInfo {
            token: token_response.access_token,
            expires_at,
        })
    }
}

// Keep the old OAuthConfig for backward compatibility during migration
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub api_keys: Vec<String>,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
}
