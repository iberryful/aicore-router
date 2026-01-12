use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

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

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub api_keys: Vec<String>,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone)]
pub struct TokenManager {
    oauth_config: Option<OAuthConfig>,
    tokens: Arc<RwLock<HashMap<String, TokenInfo>>>,
    client: Client,
}

impl TokenManager {
    pub fn with_oauth_config(oauth_config: OAuthConfig) -> Self {
        Self {
            oauth_config: Some(oauth_config),
            tokens: Arc::new(RwLock::new(HashMap::new())),
            client: Client::new(),
        }
    }

    pub async fn get_token(&self, api_key: &str) -> Result<Option<String>> {
        let oauth_config = self
            .oauth_config
            .as_ref()
            .context("TokenManager not configured with OAuth credentials")?;

        if !oauth_config.api_keys.contains(&api_key.to_string()) {
            return Ok(None);
        }

        let token_key = format!(
            "{}:{}:{}",
            oauth_config.token_url, oauth_config.client_id, oauth_config.client_secret
        );

        {
            let tokens = self.tokens.read().await;
            if let Some(token_info) = tokens.get(&token_key)
                && token_info.is_valid()
            {
                return Ok(Some(token_info.token.clone()));
            }
        }

        let new_token = self
            .refresh_token(
                &oauth_config.token_url,
                &oauth_config.client_id,
                &oauth_config.client_secret,
            )
            .await?;

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

        tracing::info!("Token refreshed for client id: {}", client_id);

        Ok(TokenInfo {
            token: token_response.access_token,
            expires_at,
        })
    }
}
