use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{Provider, ProviderAuth};

pub const CLIENT_VERSION_HEADER: &str = "X-Joule-Desktop-Client-Version";
pub const CLIENT_VERSION: &str = concat!("acr/", env!("CARGO_PKG_VERSION"));
const KEYCHAIN_SERVICE: &str = "com.sap.aicore-router.joule.oauth";
const CALLBACK_PORT: u16 = 51337;
const CALLBACK_PATH: &str = "/callback";
const OAUTH_SCOPE: &str = "openid offline_access";

#[derive(Debug, Clone)]
pub struct JouleManagedConfig {
    pub gateway_url: String,
    pub cis_url: String,
    pub client_id: String,
}

#[derive(Debug, Deserialize)]
struct ManagedPreferences {
    #[serde(rename = "GATEWAY_URL")]
    gateway_url: String,
    #[serde(rename = "CLOUD_IDENTITY_SERVICE_URL")]
    cis_url: String,
    #[serde(rename = "CLIENT_ID")]
    client_id: String,
}

impl JouleManagedConfig {
    #[cfg(target_os = "macos")]
    pub fn load() -> Result<Self> {
        let user = std::env::var("USER").unwrap_or_default();
        let user_path = format!("/Library/Managed Preferences/{user}/com.sap.joule-desktop.plist");
        Self::load_from_paths(&[
            std::path::Path::new(&user_path),
            std::path::Path::new("/Library/Managed Preferences/com.sap.joule-desktop.plist"),
        ])
    }

    #[cfg(not(target_os = "macos"))]
    pub fn load() -> Result<Self> {
        bail!("Joule managed configuration is currently supported only on macOS")
    }

    #[cfg(target_os = "macos")]
    fn load_from_paths(paths: &[&std::path::Path]) -> Result<Self> {
        for path in paths {
            if !path.is_file() {
                continue;
            }
            let file = std::fs::File::open(path)
                .with_context(|| format!("Failed to open {}", path.display()))?;
            let prefs: ManagedPreferences = plist::from_reader(file).with_context(|| {
                format!(
                    "Failed to parse Joule managed preferences at {}",
                    path.display()
                )
            })?;
            return Ok(Self {
                gateway_url: prefs.gateway_url.trim_end_matches('/').to_string(),
                cis_url: prefs.cis_url.trim_end_matches('/').to_string(),
                client_id: prefs.client_id,
            });
        }
        bail!(
            "Joule managed preferences not found. Install/configure Joule Desktop or provide uaa_token_url, uaa_client_id, and genai_api_url explicitly"
        )
    }
}

#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JouleTokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

pub(crate) async fn refresh_access_token(
    client: &reqwest::Client,
    provider: &Provider,
    refresh_token: &str,
) -> Result<JouleTokenResponse> {
    let response = client
        .post(&provider.uaa_token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", provider.uaa_client_id.as_str()),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .context("Failed to send Joule token refresh request")?;
    parse_token_response(response, "Joule token refresh").await
}

pub async fn login(provider: &Provider) -> Result<()> {
    ensure_joule_provider(provider)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = provider;
        bail!("Joule browser login and Keychain storage are currently supported only on macOS")
    }
    #[cfg(target_os = "macos")]
    login_macos(provider).await
}

pub fn logout(provider: &Provider) -> Result<()> {
    ensure_joule_provider(provider)?;
    delete_refresh_token(provider)
}

fn ensure_joule_provider(provider: &Provider) -> Result<()> {
    if provider.auth != ProviderAuth::Joule {
        bail!(
            "Provider '{}' does not use Joule authentication",
            provider.name
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn login_macos(provider: &Provider) -> Result<()> {
    use axum::{
        Router,
        extract::{Query, State},
        response::Html,
        routing::get,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{Mutex, Notify, oneshot};

    type CallbackSender = Arc<Mutex<Option<oneshot::Sender<Result<String, String>>>>>;

    #[derive(Clone)]
    struct CallbackState {
        expected_state: String,
        result_tx: CallbackSender,
        shutdown: Arc<Notify>,
    }

    async fn callback(
        State(state): State<CallbackState>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Html<String> {
        let result = validate_callback(&params, &state.expected_state);
        let page = match &result {
            Ok(_) => "Joule authentication completed. You may close this tab.".to_string(),
            Err(error) => format!("Joule authentication failed: {error}"),
        };
        if let Some(tx) = state.result_tx.lock().await.take() {
            let _ = tx.send(result);
        }
        state.shutdown.notify_one();
        Html(page)
    }

    let issuer = provider.oauth_issuer_url().ok_or_else(|| {
        anyhow::anyhow!(
            "Joule token URL must end with /oauth2/token: {}",
            provider.uaa_token_url
        )
    })?;
    let client = reqwest::Client::new();
    let discovery: OidcDiscovery = client
        .get(format!("{issuer}/.well-known/openid-configuration"))
        .send()
        .await
        .context("Failed to discover Joule OAuth configuration")?
        .error_for_status()
        .context("Joule OAuth discovery failed")?
        .json()
        .await
        .context("Failed to parse Joule OAuth discovery response")?;

    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let oauth_state = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let redirect_uri = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| {
            format!(
                "Failed to bind OAuth callback port {CALLBACK_PORT}; close another login flow and retry"
            )
        })?;
    let (result_tx, result_rx) = oneshot::channel();
    let shutdown = Arc::new(Notify::new());
    let state = CallbackState {
        expected_state: oauth_state.clone(),
        result_tx: Arc::new(Mutex::new(Some(result_tx))),
        shutdown: shutdown.clone(),
    };
    let app = Router::new()
        .route(CALLBACK_PATH, get(callback))
        .with_state(state);
    let shutdown_signal = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_signal.notified().await })
            .await
    });

    let mut authorization_url = reqwest::Url::parse(&discovery.authorization_endpoint)
        .context("Invalid Joule authorization endpoint")?;
    authorization_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &provider.uaa_client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", OAUTH_SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &oauth_state)
        .append_pair("prompt", "login");

    let status = std::process::Command::new("/usr/bin/open")
        .arg(authorization_url.as_str())
        .status()
        .context("Failed to open browser for Joule authentication")?;
    if !status.success() {
        shutdown.notify_one();
        let _ = server.await;
        bail!("Failed to open browser for Joule authentication");
    }
    println!("Complete the Joule login in your browser...");

    let callback_wait = tokio::time::timeout(std::time::Duration::from_secs(120), result_rx).await;
    shutdown.notify_one();
    let _ = server.await;
    let callback_result = callback_wait
        .context("Timed out waiting for Joule OAuth callback")?
        .context("Joule OAuth callback server stopped unexpectedly")?;
    let code = callback_result.map_err(anyhow::Error::msg)?;

    let response = client
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", provider.uaa_client_id.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .context("Failed to exchange Joule authorization code")?;
    let tokens = parse_token_response(response, "Joule authorization code exchange").await?;
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .context("Joule OAuth response did not include a refresh token")?;

    check_gateway_authorization(&client, provider, &tokens.access_token).await?;
    save_refresh_token(provider, refresh_token)?;
    println!(
        "Joule credentials saved to macOS Keychain for provider '{}'.",
        provider.name
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn validate_callback(
    params: &std::collections::HashMap<String, String>,
    expected_state: &str,
) -> std::result::Result<String, String> {
    if let Some(error) = params.get("error") {
        return Err(params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone()));
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Err("OAuth state mismatch".to_string());
    }
    params
        .get("code")
        .cloned()
        .ok_or_else(|| "OAuth callback did not include an authorization code".to_string())
}

async fn parse_token_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<JouleTokenResponse> {
    let status = response.status();
    if !status.is_success() {
        let _ = response.text().await;
        bail!("{operation} failed with status {status}");
    }
    response
        .json()
        .await
        .with_context(|| format!("Failed to parse {operation} response"))
}

async fn check_gateway_authorization(
    client: &reqwest::Client,
    provider: &Provider,
    access_token: &str,
) -> Result<()> {
    let gateway = provider
        .genai_api_url
        .strip_suffix("/ai-core")
        .unwrap_or(&provider.genai_api_url);
    let response = client
        .get(format!("{gateway}/authorization"))
        .bearer_auth(access_token)
        .send()
        .await
        .context("Failed to check Joule gateway authorization")?;
    if response.status() == reqwest::StatusCode::FORBIDDEN {
        bail!("Your account is not authorized to use the Joule gateway");
    }
    if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
        bail!(
            "Joule gateway authorization check failed with status {}",
            response.status()
        );
    }
    Ok(())
}

fn keychain_account(provider: &Provider) -> String {
    let mut hasher = Sha256::new();
    hasher.update(provider.uaa_token_url.as_bytes());
    hasher.update(b"\0");
    hasher.update(provider.uaa_client_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(target_os = "macos")]
pub(crate) fn load_refresh_token(provider: &Provider) -> Result<Option<String>> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(provider))
        .context("Failed to open macOS Keychain entry")?;
    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error).context("Failed to read Joule refresh token from macOS Keychain"),
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn load_refresh_token(_provider: &Provider) -> Result<Option<String>> {
    bail!("Joule Keychain storage is currently supported only on macOS")
}

#[cfg(target_os = "macos")]
pub(crate) fn save_refresh_token(provider: &Provider, refresh_token: &str) -> Result<()> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(provider))
        .context("Failed to open macOS Keychain entry")?
        .set_password(refresh_token)
        .context("Failed to save Joule refresh token to macOS Keychain")
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn save_refresh_token(_provider: &Provider, _refresh_token: &str) -> Result<()> {
    bail!("Joule Keychain storage is currently supported only on macOS")
}

#[cfg(target_os = "macos")]
pub(crate) fn delete_refresh_token(provider: &Provider) -> Result<()> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(provider))
        .context("Failed to open macOS Keychain entry")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => {
            Err(error).context("Failed to delete Joule refresh token from macOS Keychain")
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn delete_refresh_token(_provider: &Provider) -> Result<()> {
    bail!("Joule Keychain storage is currently supported only on macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_uses_refresh_grant_and_accepts_rotation() {
        use axum::{
            Router,
            extract::{Form, State},
            routing::post,
        };
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        type ReceivedForm = Arc<Mutex<Option<HashMap<String, String>>>>;

        async fn token_endpoint(
            State(received): State<ReceivedForm>,
            Form(form): Form<HashMap<String, String>>,
        ) -> axum::Json<serde_json::Value> {
            *received.lock().await = Some(form);
            axum::Json(serde_json::json!({
                "access_token": "access-new",
                "refresh_token": "refresh-new",
                "expires_in": 3600
            }))
        }

        let received = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/oauth2/token", post(token_endpoint))
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let provider = {
            let mut p = crate::config::test_aicore_provider("joule");
            p.auth = ProviderAuth::Joule;
            p.uaa_token_url = format!("http://{address}/oauth2/token");
            p.uaa_client_id = "public-client".to_string();
            p.uaa_client_secret = String::new();
            p.genai_api_url = "https://gateway.example.com/ai-core".to_string();
            p
        };

        let response = refresh_access_token(&reqwest::Client::new(), &provider, "refresh-old")
            .await
            .unwrap();
        assert_eq!(response.access_token, "access-new");
        assert_eq!(response.refresh_token.as_deref(), Some("refresh-new"));
        let form = received.lock().await.take().unwrap();
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            form.get("client_id").map(String::as_str),
            Some("public-client")
        );
        assert_eq!(
            form.get("refresh_token").map(String::as_str),
            Some("refresh-old")
        );
        server.abort();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn callback_validation_rejects_wrong_state() {
        let params = std::collections::HashMap::from([
            ("state".to_string(), "wrong".to_string()),
            ("code".to_string(), "code".to_string()),
        ]);
        assert_eq!(
            validate_callback(&params, "expected").unwrap_err(),
            "OAuth state mismatch"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn callback_validation_returns_code() {
        let params = std::collections::HashMap::from([
            ("state".to_string(), "expected".to_string()),
            ("code".to_string(), "authorization-code".to_string()),
        ]);
        assert_eq!(
            validate_callback(&params, "expected").unwrap(),
            "authorization-code"
        );
    }
}
