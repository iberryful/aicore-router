//! Token management for OAuth authentication with multiple providers.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, RwLock};

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
    api_keys: Vec<String>,
    /// Cached tokens keyed by provider credentials hash
    tokens: Arc<RwLock<HashMap<String, TokenInfo>>>,
    /// Per-key mutexes to serialize concurrent refresh attempts for the same provider
    refresh_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// HTTP client for token requests
    client: Client,
}

impl TokenManager {
    /// Create a new token manager with the given API keys.
    pub fn new(api_keys: Vec<String>) -> Self {
        Self {
            api_keys,
            tokens: Arc::new(RwLock::new(HashMap::new())),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
            client: Client::new(),
        }
    }

    /// Check if an API key is valid using constant-time comparison.
    /// The special "internal" key and all stored keys are checked in a
    /// single uniform loop to avoid timing side-channels.
    pub fn is_valid_api_key(&self, api_key: &str) -> bool {
        let input_bytes = api_key.as_bytes();
        let mut found = 0u8;

        // Build a single iterator over "internal" + all stored keys
        let internal_key: &str = "internal";
        let all_keys =
            std::iter::once(internal_key).chain(self.api_keys.iter().map(|s| s.as_str()));

        for stored_key in all_keys {
            let stored_bytes = stored_key.as_bytes();
            if input_bytes.len() == stored_bytes.len() {
                found |= input_bytes.ct_eq(stored_bytes).unwrap_u8();
            }
        }
        found != 0
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

        let token_key = {
            let mut hasher = Sha256::new();
            hasher.update(provider.uaa_token_url.as_bytes());
            hasher.update(b"\0");
            hasher.update(provider.uaa_client_id.as_bytes());
            hasher.update(b"\0");
            hasher.update(provider.uaa_client_secret.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        // Fast path: check cache under read lock
        {
            let tokens = self.tokens.read().await;
            if let Some(token_info) = tokens.get(&token_key)
                && token_info.is_valid()
            {
                return Ok(Some(token_info.token.clone()));
            }
        }

        // Get or create per-key refresh lock to serialize concurrent refreshes
        let refresh_lock = {
            let mut locks = self.refresh_locks.lock().await;
            locks.entry(token_key.clone()).or_default().clone()
        };

        // Only one task refreshes at a time per provider
        let _guard = refresh_lock.lock().await;

        // Re-check cache: another task may have refreshed while we waited
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

        let token_value = new_token.token.clone();

        // Store in cache
        {
            let mut tokens = self.tokens.write().await;
            tokens.insert(token_key, new_token);
        }

        Ok(Some(token_value))
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

        if !status.is_success() {
            // Consume the response body but don't log it (may contain credentials)
            let _ = response.text().await;
            tracing::error!(
                "OAuth token request failed - Status: {}, URL: {}",
                status,
                url,
            );
            return Err(anyhow::anyhow!(
                "Token request failed with status {}",
                status,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_api_key_matches() {
        let tm = TokenManager::new(vec!["key-abc-123".into(), "key-xyz-789".into()]);
        assert!(tm.is_valid_api_key("key-abc-123"));
        assert!(tm.is_valid_api_key("key-xyz-789"));
    }

    #[test]
    fn test_invalid_api_key_rejected() {
        let tm = TokenManager::new(vec!["key-abc-123".into()]);
        assert!(!tm.is_valid_api_key("wrong-key"));
        assert!(!tm.is_valid_api_key("key-abc-12")); // prefix
        assert!(!tm.is_valid_api_key("key-abc-1234")); // longer
        assert!(!tm.is_valid_api_key(""));
    }

    #[test]
    fn test_internal_key_always_valid() {
        let tm = TokenManager::new(vec!["some-key".into()]);
        assert!(tm.is_valid_api_key("internal"));
    }

    #[test]
    fn test_empty_api_keys() {
        let tm = TokenManager::new(vec![]);
        assert!(!tm.is_valid_api_key("any-key"));
        assert!(tm.is_valid_api_key("internal"));
    }
}
