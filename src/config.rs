use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use crate::constants::config::*;
use crate::metrics::TokenCounts;

/// Runtime configuration for the router
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// List of AI Core providers for load balancing
    pub providers: Vec<Provider>,
    /// API keys for authenticating requests (with optional per-key quota overrides)
    pub api_keys: Vec<ApiKeyConfig>,
    /// Bind address (IP or IP:PORT, default "127.0.0.1:8900")
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
    #[serde(default)]
    pub fallback_models: FallbackModels,
    /// Request logging configuration
    #[serde(default)]
    pub log_requests: LogRequestsConfig,
    /// Azure OpenAI API version (default: 2025-04-01-preview)
    #[serde(default = "default_openai_api_version")]
    pub openai_api_version: String,
    /// Token quota configuration
    #[serde(default)]
    pub quotas: QuotaConfig,
}

/// A single provider configuration (either a SAP AI Core tenant or a generic
/// OpenAI-compatible passthrough upstream).
#[derive(Clone, Deserialize, Serialize)]
pub struct Provider {
    /// Unique identifier for this provider
    pub name: String,
    /// Backend kind: SAP AI Core (default) or a generic passthrough upstream.
    pub kind: ProviderKind,
    /// Authentication flow used for this provider (AI Core kinds only).
    pub auth: ProviderAuth,
    /// UAA OAuth token URL
    pub uaa_token_url: String,
    /// UAA client ID
    pub uaa_client_id: String,
    /// UAA client secret
    pub uaa_client_secret: String,
    /// AI Core API base URL
    pub genai_api_url: String,
    /// Passthrough: upstream base URL (empty for AI Core kinds).
    pub base_url: String,
    /// Passthrough: static API key forwarded upstream (empty for AI Core kinds).
    pub api_key: String,
    /// Passthrough: header used to forward the key. `authorization` (default) is
    /// sent as `Bearer <key>`; any other header name sends the raw key value.
    pub auth_header: String,
    /// Passthrough: per-dialect path prefixes prepended to canonical routes.
    /// Different upstreams namespace dialects differently (a plain acr serves
    /// OpenAI at the root and Anthropic/Gemini under `/anthropic` and `/gemini`;
    /// a multi-dialect gateway may namespace every dialect, e.g. `/openai`).
    pub prefixes: PassthroughPrefixes,
    /// Resource group for this provider
    #[serde(default = "default_resource_group")]
    pub resource_group: String,
    /// Load-balancing weight of this credential within its pool (higher = more
    /// traffic). `0` keeps the credential available for explicit targeting but
    /// excludes it from the weighted shuffle.
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Name of the pool this provider belongs to. For a single-credential
    /// provider this equals `name`; for a credential pool, every expanded member
    /// shares the pool name (which is what a model's `providers:` allowlist and
    /// the default-pool grouping match against).
    #[serde(default)]
    pub pool: String,
    /// Seconds to cool a credential down after it returns a 429 before the
    /// balancer will offer it again.
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    /// Whether this provider is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("auth", &self.auth)
            .field("uaa_token_url", &self.uaa_token_url)
            .field("uaa_client_id", &self.uaa_client_id)
            .field("uaa_client_secret", &"[REDACTED]")
            .field("genai_api_url", &self.genai_api_url)
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("auth_header", &self.auth_header)
            .field("prefixes", &self.prefixes)
            .field("resource_group", &self.resource_group)
            .field("weight", &self.weight)
            .field("pool", &self.pool)
            .field("cooldown_secs", &self.cooldown_secs)
            .field("enabled", &self.enabled)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// SAP AI Core tenant (OAuth + deployment discovery + native transforms).
    #[default]
    Aicore,
    /// Generic OpenAI-compatible upstream (another acr instance, a multi-dialect
    /// gateway, etc.). Requests are relayed in their original dialect with a static key;
    /// no OAuth, no deployment discovery, no body transform.
    Passthrough,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuth {
    #[default]
    ClientCredentials,
    Joule,
}

/// Per-dialect path prefixes for a passthrough upstream. Defaults match a plain
/// acr instance: OpenAI at the root, Anthropic under `/anthropic`, Gemini under
/// `/gemini` (all three are served by acr, and the latter two are also how a
/// multi-dialect gateway namespaces them — so only `openai` typically needs an
/// override).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PassthroughPrefixes {
    pub openai: String,
    pub anthropic: String,
    pub gemini: String,
}

impl Default for PassthroughPrefixes {
    fn default() -> Self {
        Self {
            openai: String::new(),
            anthropic: "/anthropic".to_string(),
            gemini: "/gemini".to_string(),
        }
    }
}

/// File form of [`PassthroughPrefixes`]: each dialect is optional and falls back
/// to the default when omitted.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PassthroughPrefixesConfig {
    #[serde(default)]
    pub openai: Option<String>,
    #[serde(default)]
    pub anthropic: Option<String>,
    #[serde(default)]
    pub gemini: Option<String>,
}

impl PassthroughPrefixesConfig {
    /// Merge over the defaults, normalizing each provided prefix.
    fn resolve(self) -> PassthroughPrefixes {
        let d = PassthroughPrefixes::default();
        PassthroughPrefixes {
            openai: self.openai.map(normalize_prefix).unwrap_or(d.openai),
            anthropic: self.anthropic.map(normalize_prefix).unwrap_or(d.anthropic),
            gemini: self.gemini.map(normalize_prefix).unwrap_or(d.gemini),
        }
    }
}

impl Provider {
    pub fn is_joule(&self) -> bool {
        self.auth == ProviderAuth::Joule
    }

    pub fn is_passthrough(&self) -> bool {
        self.kind == ProviderKind::Passthrough
    }

    /// The (header-name, header-value) pair used to authenticate to a
    /// passthrough upstream. `authorization` is sent as `Bearer <key>`; any
    /// other header name carries the raw key value (e.g. `x-api-key`).
    pub fn passthrough_auth_header(&self) -> (String, String) {
        if self.auth_header.eq_ignore_ascii_case("authorization") {
            ("authorization".to_string(), format!("Bearer {}", self.api_key))
        } else {
            (self.auth_header.clone(), self.api_key.clone())
        }
    }

    pub fn oauth_issuer_url(&self) -> Option<&str> {
        self.is_joule()
            .then(|| self.uaa_token_url.strip_suffix("/oauth2/token"))
            .flatten()
    }
}

fn default_weight() -> u32 {
    1
}

fn default_cooldown_secs() -> u64 {
    DEFAULT_COOLDOWN_SECS
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub log_level: Option<String>,
    /// Multiple providers for load balancing
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub refresh_interval_secs: Option<u64>,
    #[serde(default)]
    pub fallback_models: FallbackModels,
    /// API keys for authenticating requests (supports both string and object formats)
    #[serde(default)]
    api_keys: Vec<ApiKeyEntry>,
    /// Request logging configuration
    #[serde(default)]
    pub log_requests: Option<LogRequestsConfig>,
    /// Azure OpenAI API version (overrides DEFAULT_API_VERSION)
    #[serde(default)]
    pub openai_api_version: Option<String>,
    /// Token quota configuration
    #[serde(default)]
    pub quotas: QuotaConfig,
    /// Catch-all for unknown fields
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml_ng::Value>,
}

/// Request logging configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogRequestsConfig {
    /// Whether request logging is enabled
    #[serde(default = "default_log_requests_enabled")]
    pub enabled: bool,
    /// Path to SQLite database
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// Number of days to retain logs (0 = keep forever)
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Catch-all for unknown fields
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml_ng::Value>,
}

impl Default for LogRequestsConfig {
    fn default() -> Self {
        Self {
            enabled: default_log_requests_enabled(),
            db_path: default_db_path(),
            retention_days: default_retention_days(),
            unknown: HashMap::new(),
        }
    }
}

fn default_log_requests_enabled() -> bool {
    false
}

fn default_db_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/.aicore/requests.db")
}

fn default_retention_days() -> u32 {
    30
}

/// Provider configuration as read from config file
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    /// Unique identifier for this provider
    pub name: String,
    /// Backend kind. Omit for a SAP AI Core provider (the default).
    #[serde(default)]
    pub kind: ProviderKind,
    /// Authentication mode. Omit for the existing UAA client-credentials flow.
    #[serde(default)]
    pub auth: ProviderAuth,
    /// UAA OAuth token URL
    #[serde(default)]
    pub uaa_token_url: Option<String>,
    /// UAA client ID
    #[serde(default)]
    pub uaa_client_id: Option<String>,
    /// UAA client secret
    #[serde(default)]
    pub uaa_client_secret: Option<String>,
    /// AI Core API base URL
    #[serde(default)]
    pub genai_api_url: Option<String>,
    /// Passthrough: upstream base URL (required when kind = passthrough).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Passthrough: static API key forwarded upstream (required when kind = passthrough).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Passthrough: header used to forward the key (default `authorization`).
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Passthrough: per-dialect path prefixes (each defaults when omitted).
    #[serde(default)]
    pub prefixes: Option<PassthroughPrefixesConfig>,
    /// Resource group for this provider
    #[serde(default)]
    pub resource_group: Option<String>,
    /// Provider-level weight is no longer supported — weight is a per-credential
    /// property. Parsed only so we can emit a clear migration error if present.
    #[serde(default)]
    pub weight: Option<u32>,
    /// Inline credential pool (AI Core kinds only). When set, the provider
    /// load-balances across these credentials (same backend, shared cache).
    #[serde(default)]
    pub credentials: Option<Vec<Credential>>,
    /// External credential-pool file (AI Core kinds only). Relative paths resolve
    /// against the main config file's directory; `~` is expanded. Mutually
    /// exclusive with inline `credentials`.
    #[serde(default)]
    pub credentials_file: Option<String>,
    /// Seconds to cool a credential down after a 429 (pool-level default).
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
    /// Whether this provider is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Catch-all for unknown fields
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml_ng::Value>,
}

/// A single OAuth credential within an AI Core provider pool. All credentials in
/// a pool front the same backend (tenant, `genai_api_url`, resource group), so
/// the balancer may spread across them without fragmenting the prompt cache.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Credential {
    /// Optional label, used to build the expanded member name `"{pool}/{name}"`.
    #[serde(default)]
    pub name: Option<String>,
    /// UAA client ID for this credential.
    pub uaa_client_id: String,
    /// UAA client secret for this credential.
    #[serde(default)]
    pub uaa_client_secret: String,
    /// Optional per-credential token URL; inherits the pool's `uaa_token_url`.
    #[serde(default)]
    pub uaa_token_url: Option<String>,
    /// Weight within the pool's weighted shuffle (`0` = excluded from shuffle).
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// File form of a credential pool: `{ credentials: [...] }`.
#[derive(Debug, Clone, Deserialize)]
struct CredentialFile {
    #[serde(default)]
    credentials: Vec<Credential>,
}

/// Pricing per 1M tokens for cost estimation.
/// All fields are optional — if a field is None, that token type contributes $0
/// to the cost estimate but is flagged as partial.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelPricing {
    /// Cost per 1M input tokens
    #[serde(default)]
    pub input: Option<f64>,
    /// Cost per 1M output tokens
    #[serde(default)]
    pub output: Option<f64>,
    /// Cost per 1M cache read tokens
    #[serde(default)]
    pub cache_read: Option<f64>,
    /// Cost per 1M cache write tokens
    #[serde(default)]
    pub cache_write: Option<f64>,
}

impl ModelPricing {
    /// Calculate the estimated cost given token counts.
    /// Missing rates contribute $0 to the total.
    pub fn calculate_cost(&self, tokens: &TokenCounts) -> f64 {
        let i = tokens.input as f64 * self.input.unwrap_or(0.0) / 1_000_000.0;
        let o = tokens.output as f64 * self.output.unwrap_or(0.0) / 1_000_000.0;
        let cr = tokens.cache_read as f64 * self.cache_read.unwrap_or(0.0) / 1_000_000.0;
        let cw = tokens.cache_write as f64 * self.cache_write.unwrap_or(0.0) / 1_000_000.0;
        i + o + cr + cw
    }

    /// Returns true if any token type that has actual usage is missing a rate.
    pub fn is_partial(&self, tokens: &TokenCounts) -> bool {
        self.input.is_none()
            || self.output.is_none()
            || (tokens.cache_read > 0 && self.cache_read.is_none())
            || (tokens.cache_write > 0 && self.cache_write.is_none())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    pub name: String,
    /// The model name as it appears in AI Core deployments.
    /// If not specified, the `name` field is used to look up deployments.
    pub aicore_model_name: Option<String>,
    /// Alias patterns that should resolve to this model.
    /// Supports trailing wildcard (*) for prefix matching.
    /// Example: ["claude-sonnet-4-5-*", "claude-4-sonnet"]
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Optional ordered provider allowlist. Empty means all enabled providers.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Pricing per 1M tokens for cost estimation.
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
}

impl Model {
    pub fn routes_to_provider(&self, provider: &Provider) -> bool {
        provider.enabled
            && if self.providers.is_empty() {
                // No allowlist: every enabled provider is in the default pool.
                true
            } else {
                // Allowlist entries name pools; a pool's expanded members all
                // carry the pool name in `pool`.
                self.providers
                    .iter()
                    .any(|configured| configured == &provider.pool)
            }
    }
}

/// Configuration for fallback models per model family.
/// When a requested model is not found, the router will fall back to the
/// configured model for that family (if available and configured).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FallbackModels {
    /// Fallback model for Claude family (models starting with "claude")
    #[serde(default)]
    pub claude: Option<String>,
    /// Fallback model for OpenAI family (models starting with "gpt" or "text")
    #[serde(default)]
    pub openai: Option<String>,
    /// Fallback model for Gemini family (models starting with "gemini")
    #[serde(default)]
    pub gemini: Option<String>,
}

impl FallbackModels {
    /// Iterate over configured fallback models as (family_name, model_name) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        [
            ("claude", self.claude.as_deref()),
            ("openai", self.openai.as_deref()),
            ("gemini", self.gemini.as_deref()),
        ]
        .into_iter()
        .filter_map(|(family, model)| model.map(|m| (family, m)))
    }
}

/// Global quota configuration with daily and monthly token limits.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct QuotaConfig {
    /// Master switch to enable/disable quota enforcement
    #[serde(default)]
    pub enabled: bool,
    /// Default daily token limit for all API keys (None = unlimited)
    #[serde(default)]
    pub daily_token_limit: Option<u64>,
    /// Default monthly token limit for all API keys (None = unlimited)
    #[serde(default)]
    pub monthly_token_limit: Option<u64>,
    /// Default per-key requests-per-minute rate limit (None / 0 = unlimited).
    /// Enforced as a token-bucket via the `governor` crate; rejected requests
    /// receive HTTP 429 with `Retry-After`.
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
    /// Catch-all for unknown fields
    #[serde(flatten, default)]
    pub unknown: HashMap<String, serde_yaml_ng::Value>,
}

/// Per-key configuration with optional quota overrides.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiKeyConfig {
    pub key: String,
    /// Per-key daily token limit override (None = use global default)
    #[serde(default)]
    pub daily_token_limit: Option<u64>,
    /// Per-key monthly token limit override (None = use global default)
    #[serde(default)]
    pub monthly_token_limit: Option<u64>,
    /// Per-key requests-per-minute override (None = use global default)
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
}

/// Intermediate deserialization type that accepts both string and object forms.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum ApiKeyEntry {
    /// Simple string format: "my-api-key"
    Simple(String),
    /// Object format with quota overrides: { key: "my-api-key", daily_token_limit: 1000000 }
    WithConfig {
        key: String,
        #[serde(default)]
        daily_token_limit: Option<u64>,
        #[serde(default)]
        monthly_token_limit: Option<u64>,
        #[serde(default)]
        requests_per_minute: Option<u32>,
    },
}

impl From<ApiKeyEntry> for ApiKeyConfig {
    fn from(entry: ApiKeyEntry) -> Self {
        match entry {
            ApiKeyEntry::Simple(key) => ApiKeyConfig {
                key,
                daily_token_limit: None,
                monthly_token_limit: None,
                requests_per_minute: None,
            },
            ApiKeyEntry::WithConfig {
                key,
                daily_token_limit,
                monthly_token_limit,
                requests_per_minute,
            } => ApiKeyConfig {
                key,
                daily_token_limit,
                monthly_token_limit,
                requests_per_minute,
            },
        }
    }
}

fn default_bind() -> String {
    DEFAULT_BIND.to_string()
}

/// Parse a bind address string into a SocketAddr.
/// Supports:
///   - "IP:PORT" (e.g. "127.0.0.1:8900", "[::1]:9000")
///   - "IP" only (e.g. "127.0.0.1", "0.0.0.0") — uses default port 8900
pub fn parse_bind_address(s: &str) -> Result<SocketAddr> {
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 8900));
    }
    bail!("Invalid bind address '{s}'. Expected IP (e.g. 127.0.0.1) or IP:PORT (e.g. 0.0.0.0:9000)")
}

/// If the `PORT` env var is set and parses as a u16, override the port part of `bind`.
/// The IP from the file (or default) is preserved.
fn apply_port_env_override(bind: String) -> Result<String> {
    let Ok(raw) = env::var("PORT") else {
        return Ok(bind);
    };
    let port: u16 = raw
        .parse()
        .with_context(|| format!("PORT env var '{raw}' is not a valid u16"))?;
    let addr = parse_bind_address(&bind)?;
    Ok(SocketAddr::new(addr.ip(), port).to_string())
}

fn default_log_level() -> String {
    DEFAULT_LOG_LEVEL.to_string()
}

fn default_refresh_interval_secs() -> u64 {
    DEFAULT_REFRESH_INTERVAL_SECS
}

fn default_resource_group() -> String {
    DEFAULT_RESOURCE_GROUP.to_string()
}

fn default_openai_api_version() -> String {
    crate::constants::api::DEFAULT_API_VERSION.to_string()
}

fn normalize_oauth_token_url(url: String) -> String {
    if !url.contains("/oauth/token") && !url.ends_with('/') {
        format!("{url}/oauth/token")
    } else if url.ends_with('/') && !url.contains("/oauth/token") {
        format!("{url}oauth/token")
    } else {
        url
    }
}

impl Config {
    /// Get the raw API key strings (for auth validation, TokenManager, etc.)
    pub fn api_key_strings(&self) -> Vec<String> {
        self.api_keys.iter().map(|k| k.key.clone()).collect()
    }

    /// Providers grouped by pool, preserving first-appearance order. Each entry
    /// is `(pool_name, members)`; a single-credential provider is a pool of one.
    /// The single place that reconstructs pools from the expanded provider list
    /// (used by startup/diagnostic logging and the e2e config synthesizer).
    pub fn provider_pools(&self) -> Vec<(&str, Vec<&Provider>)> {
        let mut groups: Vec<(&str, Vec<&Provider>)> = Vec::new();
        for provider in &self.providers {
            if let Some(entry) = groups.iter_mut().find(|(name, _)| *name == provider.pool) {
                entry.1.push(provider);
            } else {
                groups.push((provider.pool.as_str(), vec![provider]));
            }
        }
        groups
    }

    pub fn load(config_path: Option<&str>) -> Result<Self> {
        let config_file_path = match config_path {
            Some(path) => path.to_string(),
            None => {
                let home = env::var("HOME").context("HOME environment variable not set")?;
                format!("{home}/.aicore/config.yaml")
            }
        };

        if !Path::new(&config_file_path).exists() {
            return Err(anyhow::anyhow!(
                "Config file not found: {}. Please create a config file.",
                config_file_path
            ));
        }

        let config_content = std::fs::read_to_string(&config_file_path)
            .with_context(|| format!("Failed to read config file: {config_file_path}"))?;
        let file_config = serde_yaml_ng::from_str::<ConfigFile>(&config_content)
            .with_context(|| format!("Failed to parse config file: {config_file_path}"))?;

        // Warn about unknown fields (typos, deprecated keys, etc.)
        Self::warn_unknown_fields(&file_config);

        let config_dir = Path::new(&config_file_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self::from_file_and_env(file_config, &config_dir)
    }

    /// Print warnings for any unrecognized fields in the config file.
    fn warn_unknown_fields(file_config: &ConfigFile) {
        for key in file_config.unknown.keys() {
            eprintln!("Warning: Unknown config field '{key}' (ignored)");
        }
        for (i, provider) in file_config.providers.iter().enumerate() {
            for key in provider.unknown.keys() {
                eprintln!(
                    "Warning: Unknown field '{key}' in providers[{i}] '{}' (ignored)",
                    provider.name
                );
            }
        }
        if let Some(ref log_requests) = file_config.log_requests {
            for key in log_requests.unknown.keys() {
                eprintln!("Warning: Unknown field '{key}' in log_requests (ignored)");
            }
        }
        for key in file_config.quotas.unknown.keys() {
            eprintln!("Warning: Unknown field '{key}' in quotas (ignored)");
        }
    }

    /// Look up pricing configuration for a model by name.
    pub fn get_model_pricing(&self, model_name: &str) -> Option<&ModelPricing> {
        self.models
            .iter()
            .find(|m| m.name == model_name)?
            .pricing
            .as_ref()
    }

    fn from_file_and_env(file_config: ConfigFile, config_dir: &Path) -> Result<Self> {
        // Build providers list from config file
        let mut providers: Vec<Provider> = Vec::new();
        let mut joule_managed_config = None;

        for p in file_config.providers {
            let provider_name = p.name.clone();

            // Hard cut-over: weight is a per-credential property now. A stray
            // provider-level `weight` is almost always a stale config.
            if p.weight.is_some() {
                bail!(
                    "Provider '{provider_name}': provider-level 'weight' is no longer supported; set 'weight' on each credential inside the pool instead"
                );
            }
            let cooldown_secs = p.cooldown_secs.unwrap_or_else(default_cooldown_secs);

            // Passthrough providers skip the entire AI Core apparatus (OAuth,
            // deployment discovery, native transforms) and relay requests to a
            // generic OpenAI-compatible upstream with a static key.
            if p.kind == ProviderKind::Passthrough {
                let base_url =
                    required_field(&provider_name, "base_url", p.base_url, "passthrough kind")?;
                let api_key =
                    required_field(&provider_name, "api_key", p.api_key, "passthrough kind")?;
                providers.push(Provider {
                    name: p.name.clone(),
                    kind: ProviderKind::Passthrough,
                    auth: ProviderAuth::ClientCredentials,
                    uaa_token_url: String::new(),
                    uaa_client_id: String::new(),
                    uaa_client_secret: String::new(),
                    genai_api_url: String::new(),
                    base_url: base_url.trim_end_matches('/').to_string(),
                    api_key,
                    auth_header: p.auth_header.unwrap_or_else(default_auth_header),
                    prefixes: p.prefixes.unwrap_or_default().resolve(),
                    resource_group: p.resource_group.unwrap_or_else(default_resource_group),
                    weight: default_weight(),
                    pool: p.name,
                    cooldown_secs,
                    enabled: p.enabled,
                });
                continue;
            }

            let resource_group = p
                .resource_group
                .clone()
                .unwrap_or_else(default_resource_group);

            // Joule providers resolve a single credential from managed config;
            // credential pools don't apply (no client secret to rotate).
            if p.auth == ProviderAuth::Joule {
                if p.credentials.is_some() || p.credentials_file.is_some() {
                    bail!(
                        "Provider '{provider_name}': credential pools are not supported for Joule auth"
                    );
                }
                let needs_managed_config = p.uaa_token_url.is_none()
                    || p.uaa_client_id.is_none()
                    || p.genai_api_url.is_none();
                if needs_managed_config && joule_managed_config.is_none() {
                    joule_managed_config =
                        Some(crate::joule::JouleManagedConfig::load().with_context(|| {
                            format!(
                                "Failed to load Joule managed configuration for provider '{provider_name}'"
                            )
                        })?);
                }
                let managed = joule_managed_config.as_ref();
                let uaa_token_url = p.uaa_token_url.unwrap_or_else(|| {
                    format!(
                        "{}/oauth2/token",
                        managed.expect("managed Joule config loaded").cis_url
                    )
                });
                let uaa_client_id = p.uaa_client_id.unwrap_or_else(|| {
                    managed
                        .expect("managed Joule config loaded")
                        .client_id
                        .clone()
                });
                let genai_api_url = p.genai_api_url.unwrap_or_else(|| {
                    format!(
                        "{}/ai-core",
                        managed.expect("managed Joule config loaded").gateway_url
                    )
                });
                providers.push(Provider {
                    name: p.name.clone(),
                    kind: ProviderKind::Aicore,
                    auth: ProviderAuth::Joule,
                    uaa_token_url,
                    uaa_client_id,
                    uaa_client_secret: String::new(),
                    genai_api_url: genai_api_url.trim_end_matches('/').to_string(),
                    base_url: String::new(),
                    api_key: String::new(),
                    auth_header: default_auth_header(),
                    prefixes: PassthroughPrefixes::default(),
                    resource_group,
                    weight: default_weight(),
                    pool: p.name,
                    cooldown_secs,
                    enabled: p.enabled,
                });
                continue;
            }

            // AI Core client-credentials: expand the credential pool into one
            // runtime Provider per credential, all sharing the backend fields.
            let genai_api_url = required_field(
                &provider_name,
                "genai_api_url",
                p.genai_api_url.clone(),
                "client_credentials authentication",
            )?
            .trim_end_matches('/')
            .to_string();
            let pool_token_url = p.uaa_token_url.clone();
            let (credentials, is_pool) = resolve_credential_pool(&p, config_dir)?;

            for (index, cred) in credentials.into_iter().enumerate() {
                let cred_label = cred.name.clone().unwrap_or_else(|| index.to_string());
                if is_pool && cred.uaa_client_secret.trim().is_empty() {
                    eprintln!(
                        "Warning: credential '{cred_label}' in pool '{provider_name}' has an empty secret; skipping"
                    );
                    continue;
                }
                let token_url = required_field(
                    &provider_name,
                    "uaa_token_url",
                    cred.uaa_token_url.clone().or_else(|| pool_token_url.clone()),
                    "client_credentials authentication",
                )?;
                let member_name = if is_pool {
                    format!("{provider_name}/{cred_label}")
                } else {
                    provider_name.clone()
                };
                providers.push(Provider {
                    name: member_name,
                    kind: ProviderKind::Aicore,
                    auth: ProviderAuth::ClientCredentials,
                    uaa_token_url: normalize_oauth_token_url(token_url),
                    uaa_client_id: cred.uaa_client_id,
                    uaa_client_secret: cred.uaa_client_secret,
                    genai_api_url: genai_api_url.clone(),
                    base_url: String::new(),
                    api_key: String::new(),
                    auth_header: default_auth_header(),
                    prefixes: PassthroughPrefixes::default(),
                    resource_group: resource_group.clone(),
                    weight: cred.weight,
                    pool: provider_name.clone(),
                    cooldown_secs,
                    enabled: p.enabled,
                });
            }
        }

        if providers.is_empty() {
            return Err(anyhow::anyhow!(
                "At least one provider is required in the 'providers' array in config file"
            ));
        }

        // Build api_keys list from config file
        let mut api_keys: Vec<ApiKeyConfig> = Vec::new();
        api_keys.extend(file_config.api_keys.into_iter().map(ApiKeyConfig::from));

        // Deduplicate while preserving order (by key string)
        let mut seen = std::collections::HashSet::new();
        api_keys.retain(|k| seen.insert(k.key.clone()));

        if api_keys.is_empty() {
            return Err(anyhow::anyhow!(
                "At least one API key is required in the 'api_keys' config"
            ));
        }

        let bind = apply_port_env_override(file_config.bind)?;

        let log_level = file_config.log_level.unwrap_or_else(default_log_level);

        let refresh_interval_secs = file_config
            .refresh_interval_secs
            .unwrap_or_else(default_refresh_interval_secs);

        let models = file_config.models;
        let fallback_models = file_config.fallback_models;

        // Resolve log_requests config
        let mut log_requests = file_config.log_requests.unwrap_or_default();
        log_requests.db_path = shellexpand::tilde(&log_requests.db_path).into_owned();

        let openai_api_version = file_config
            .openai_api_version
            .unwrap_or_else(default_openai_api_version);
        let quotas = file_config.quotas;

        let config = Config {
            providers,
            api_keys,
            bind,
            models,
            log_level,
            refresh_interval_secs,
            fallback_models,
            log_requests,
            openai_api_version,
            quotas,
        };

        config.validate()?;
        Ok(config)
    }

    /// Validate semantic constraints that can't be expressed in types alone.
    fn validate(&self) -> Result<()> {
        // log_requests requires the db feature to be compiled.
        // Only reachable via YAML config (the --log-requests CLI flag doesn't exist without `db`).
        #[cfg(not(feature = "db"))]
        if self.log_requests.enabled {
            anyhow::bail!(
                "log_requests requires the 'db' feature. Rebuild with: cargo build --features db"
            );
        }

        let mut member_names = std::collections::HashSet::new();
        let mut configured_pools = std::collections::HashSet::new();
        let mut enabled_pools = std::collections::HashSet::new();
        for provider in &self.providers {
            if !member_names.insert(provider.name.as_str()) {
                anyhow::bail!(
                    "Provider names must be unique; '{}' is duplicated",
                    provider.name
                );
            }
            configured_pools.insert(provider.pool.as_str());
            if provider.enabled {
                enabled_pools.insert(provider.pool.as_str());
            }
        }

        for model in &self.models {
            let mut seen = std::collections::HashSet::new();
            for pool_name in &model.providers {
                if !configured_pools.contains(pool_name.as_str()) {
                    anyhow::bail!(
                        "Model '{}' references unknown provider '{}'",
                        model.name,
                        pool_name
                    );
                }
                if !seen.insert(pool_name.as_str()) {
                    anyhow::bail!(
                        "Model '{}' lists provider '{}' more than once",
                        model.name,
                        pool_name
                    );
                }
            }
            if !model.providers.is_empty()
                && !model
                    .providers
                    .iter()
                    .any(|pool| enabled_pools.contains(pool.as_str()))
            {
                anyhow::bail!(
                    "Model '{}' has no enabled provider in its provider allowlist",
                    model.name
                );
            }
            if model.providers.is_empty() && enabled_pools.is_empty() {
                anyhow::bail!(
                    "Model '{}' has no default provider because no provider is enabled",
                    model.name
                );
            }
        }

        // Fallback models must reference models in the models list
        let model_names: Vec<&str> = self.models.iter().map(|m| m.name.as_str()).collect();
        for (family, fb) in self.fallback_models.iter() {
            if !model_names.contains(&fb) {
                anyhow::bail!(
                    "fallback_models.{} references '{}' which is not in the models list",
                    family,
                    fb
                );
            }
        }

        Ok(())
    }
}

fn required_field(
    provider_name: &str,
    field_name: &str,
    value: Option<String>,
    context: &str,
) -> Result<String> {
    value.filter(|v| !v.trim().is_empty()).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider '{}' requires '{}' for {}",
            provider_name,
            field_name,
            context
        )
    })
}

/// Resolve an AI Core provider's credential pool. Returns the credentials and
/// whether this is a true pool (inline `credentials` / `credentials_file`) vs. a
/// single back-compat credential drawn from the provider's top-level fields.
/// Pool credentials with empty secrets are kept here and skipped by the caller;
/// the back-compat single credential requires both id and secret.
fn resolve_credential_pool(
    p: &ProviderConfig,
    config_dir: &Path,
) -> Result<(Vec<Credential>, bool)> {
    match (&p.credentials_file, &p.credentials) {
        (Some(_), Some(_)) => bail!(
            "Provider '{}': set either 'credentials' or 'credentials_file', not both",
            p.name
        ),
        (Some(path), None) => {
            let expanded = shellexpand::tilde(path);
            let candidate = Path::new(expanded.as_ref());
            let resolved = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                config_dir.join(candidate)
            };
            let content = std::fs::read_to_string(&resolved).with_context(|| {
                format!(
                    "Provider '{}': failed to read credentials_file '{}'",
                    p.name,
                    resolved.display()
                )
            })?;
            let file: CredentialFile = serde_yaml_ng::from_str(&content).with_context(|| {
                format!(
                    "Provider '{}': failed to parse credentials_file '{}'",
                    p.name,
                    resolved.display()
                )
            })?;
            Ok((file.credentials, true))
        }
        (None, Some(creds)) => Ok((creds.clone(), true)),
        (None, None) => {
            let uaa_client_id = required_field(
                &p.name,
                "uaa_client_id",
                p.uaa_client_id.clone(),
                "client_credentials authentication",
            )?;
            let uaa_client_secret = required_field(
                &p.name,
                "uaa_client_secret",
                p.uaa_client_secret.clone(),
                "client_credentials authentication",
            )?;
            Ok((
                vec![Credential {
                    name: None,
                    uaa_client_id,
                    uaa_client_secret,
                    uaa_token_url: None,
                    weight: default_weight(),
                }],
                false,
            ))
        }
    }
}

fn default_auth_header() -> String {
    "authorization".to_string()
}

/// Normalize a passthrough path prefix: trim whitespace and any trailing slash,
/// and ensure a non-empty prefix starts with a leading slash. An empty prefix
/// (e.g. the OpenAI root) stays empty.
fn normalize_prefix(prefix: String) -> String {
    let trimmed = prefix.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
pub fn test_aicore_provider(name: &str) -> Provider {
    Provider {
        name: name.to_string(),
        kind: ProviderKind::Aicore,
        auth: ProviderAuth::ClientCredentials,
        uaa_token_url: format!("https://{name}.example.com/oauth/token"),
        uaa_client_id: format!("{name}-client"),
        uaa_client_secret: format!("{name}-secret"),
        genai_api_url: format!("https://api.{name}.example.com"),
        base_url: String::new(),
        api_key: String::new(),
        auth_header: "authorization".to_string(),
        prefixes: PassthroughPrefixes::default(),
        resource_group: "default".to_string(),
        weight: 1,
        pool: name.to_string(),
        cooldown_secs: DEFAULT_COOLDOWN_SECS,
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_config_parsing_with_all_fields() {
        let yaml_content = r#"
log_level: DEBUG
bind: "0.0.0.0:9000"
providers:
  - name: test-provider
    uaa_token_url: https://test.example.com/oauth/token
    uaa_client_id: test-client-id
    uaa_client_secret: test-client-secret
    genai_api_url: https://api.test.example.com
    resource_group: default
api_keys:
  - test-api-key
models:
  - name: gpt-4
    aicore_model_name: gpt-4-turbo
  - name: claude-3
    aicore_model_name: anthropic--claude-3
"#;

        let config_file: ConfigFile =
            serde_yaml_ng::from_str(yaml_content).expect("Failed to parse YAML");

        assert_eq!(config_file.bind, "0.0.0.0:9000");
        assert_eq!(config_file.log_level, Some("DEBUG".to_string()));
        assert_eq!(config_file.models.len(), 2);
        assert_eq!(config_file.models[0].name, "gpt-4");
        assert_eq!(
            config_file.models[0].aicore_model_name,
            Some("gpt-4-turbo".to_string())
        );
        assert_eq!(config_file.providers.len(), 1);
        assert_eq!(config_file.providers[0].name, "test-provider");
    }

    #[test]
    fn test_config_load_from_file() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("test_config.yaml");

        let yaml_content = r#"
bind: "127.0.0.1:8080"
providers:
  - name: default
    uaa_token_url: https://test.example.com/oauth/token
    uaa_client_id: test-client-id
    uaa_client_secret: test-client-secret
    genai_api_url: https://api.test.example.com
    resource_group: default
api_keys:
  - test-api-key
models:
  - name: test-model
    aicore_model_name: test-aicore-model
"#;

        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.bind, "127.0.0.1:8080");
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].name, "default");
        assert_eq!(
            config.providers[0].uaa_token_url,
            "https://test.example.com/oauth/token"
        );
        assert_eq!(config.providers[0].uaa_client_id, "test-client-id");
        assert_eq!(config.providers[0].auth, ProviderAuth::ClientCredentials);
        assert_eq!(
            config.providers[0].genai_api_url,
            "https://api.test.example.com"
        );
        assert_eq!(config.api_key_strings(), vec!["test-api-key".to_string()]);
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "test-model");
        assert_eq!(
            config.models[0].aicore_model_name,
            Some("test-aicore-model".to_string())
        );
    }

    #[test]
    fn test_joule_provider_config_with_explicit_values() {
        let yaml_content = r#"
providers:
  - name: joule
    auth: joule
    uaa_token_url: https://identity.example.com/oauth2/token
    uaa_client_id: public-client
    genai_api_url: https://gateway.example.com/ai-core
api_keys:
  - test-api-key
models:
  - name: claude-opus-4-7
    aicore_model_name: anthropic--claude-4.7-opus
"#;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("joule.yaml");
        fs::write(&config_path, yaml_content).unwrap();

        let config = Config::load(Some(config_path.to_str().unwrap())).unwrap();
        let provider = &config.providers[0];
        assert_eq!(provider.auth, ProviderAuth::Joule);
        assert_eq!(provider.uaa_client_secret, "");
        assert_eq!(
            provider.oauth_issuer_url(),
            Some("https://identity.example.com")
        );
        assert_eq!(
            provider.genai_api_url,
            "https://gateway.example.com/ai-core"
        );
    }

    #[test]
    fn test_model_provider_allowlist_parses_in_order() {
        let yaml_content = r#"
providers:
  - name: lenny
    uaa_token_url: https://lenny.example.com/oauth/token
    uaa_client_id: lenny-client
    uaa_client_secret: lenny-secret
    genai_api_url: https://lenny.example.com
  - name: dev
    uaa_token_url: https://dev.example.com/oauth/token
    uaa_client_id: dev-client
    uaa_client_secret: dev-secret
    genai_api_url: https://dev.example.com
api_keys:
  - test-api-key
models:
  - name: claude-opus-4-7
    aicore_model_name: anthropic--claude-4.7-opus
    providers:
      - lenny
      - dev
    aliases:
      - claude-4.7-opus-*
"#;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("model-providers.yaml");
        fs::write(&config_path, yaml_content).unwrap();

        let config = Config::load(Some(config_path.to_str().unwrap())).unwrap();
        assert_eq!(
            config.models[0].providers,
            vec!["lenny".to_string(), "dev".to_string()]
        );
    }

    #[test]
    fn test_model_provider_allowlist_rejects_unknown_and_duplicate_names() {
        let base = r#"
providers:
  - name: lenny
    uaa_token_url: https://lenny.example.com/oauth/token
    uaa_client_id: lenny-client
    uaa_client_secret: lenny-secret
    genai_api_url: https://lenny.example.com
api_keys:
  - test-api-key
models:
  - name: claude-opus-4-7
    providers:
"#;

        for (suffix, expected) in [
            ("      - missing\n", "references unknown provider 'missing'"),
            (
                "      - lenny\n      - lenny\n",
                "lists provider 'lenny' more than once",
            ),
        ] {
            let temp_dir = TempDir::new().unwrap();
            let config_path = temp_dir.path().join("invalid-model-providers.yaml");
            fs::write(&config_path, format!("{base}{suffix}")).unwrap();
            let error = Config::load(Some(config_path.to_str().unwrap())).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn test_provider_level_weight_is_rejected() {
        let yaml_content = r#"
providers:
  - name: lenny
    uaa_token_url: https://lenny.example.com/oauth/token
    uaa_client_id: lenny-client
    uaa_client_secret: lenny-secret
    genai_api_url: https://lenny.example.com
    weight: 1
api_keys:
  - test-api-key
models:
  - name: claude-opus-4-7
    providers:
      - lenny
"#;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("provider-level-weight.yaml");
        fs::write(&config_path, yaml_content).unwrap();
        let error = Config::load(Some(config_path.to_str().unwrap())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("provider-level 'weight' is no longer supported"),
            "{error:#}"
        );
    }

    #[test]
    fn test_provider_pools_groups_members_in_order() {
        let yaml_content = r#"
providers:
  - name: poolA
    genai_api_url: https://a.example.com
    uaa_token_url: https://a.example.com/oauth/token
    resource_group: default
    credentials:
      - name: c1
        uaa_client_id: id1
        uaa_client_secret: s1
      - name: c2
        uaa_client_id: id2
        uaa_client_secret: s2
  - name: solo
    uaa_token_url: https://b.example.com/oauth/token
    uaa_client_id: idb
    uaa_client_secret: sb
    genai_api_url: https://b.example.com
api_keys:
  - test-api-key
models:
  - name: m1
"#;
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("pools.yaml");
        fs::write(&config_path, yaml_content).unwrap();
        let config = Config::load(Some(config_path.to_str().unwrap())).unwrap();

        let pools = config.provider_pools();
        // First-appearance order preserved; a pool groups its credentials, a
        // single-credential provider is a pool of one.
        let names: Vec<(&str, usize)> = pools.iter().map(|(n, m)| (*n, m.len())).collect();
        assert_eq!(names, vec![("poolA", 2), ("solo", 1)]);
        let member_names: Vec<&str> = pools[0].1.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(member_names, vec!["poolA/c1", "poolA/c2"]);
        assert_eq!(pools[1].1[0].name, "solo");
    }

    #[test]
    fn test_unrestricted_model_rejects_when_no_enabled_provider() {
        let yaml_content = r#"
providers:
  - name: lenny
    uaa_token_url: https://lenny.example.com/oauth/token
    uaa_client_id: lenny-client
    uaa_client_secret: lenny-secret
    genai_api_url: https://lenny.example.com
    enabled: false
api_keys:
  - test-api-key
models:
  - name: claude-opus-4-7
"#;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("no-enabled-default-provider.yaml");
        fs::write(&config_path, yaml_content).unwrap();
        let error = Config::load(Some(config_path.to_str().unwrap())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no default provider because no provider is enabled"),
            "{error:#}"
        );
    }

    #[test]
    fn test_model_provider_allowlist_rejects_disabled_provider() {
        let yaml_content = r#"
providers:
  - name: lenny
    uaa_token_url: https://lenny.example.com/oauth/token
    uaa_client_id: lenny-client
    uaa_client_secret: lenny-secret
    genai_api_url: https://lenny.example.com
    enabled: false
api_keys:
  - test-api-key
models:
  - name: claude-opus-4-7
    providers:
      - lenny
"#;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("disabled-model-provider.yaml");
        fs::write(&config_path, yaml_content).unwrap();
        let error = Config::load(Some(config_path.to_str().unwrap())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("has no enabled provider in its provider allowlist"),
            "{error:#}"
        );
    }

    #[test]
    fn test_model_routes_to_provider_pool_and_enabled_semantics() {
        let provider = |name: &str, pool: &str, enabled: bool| {
            let mut p = test_aicore_provider(name);
            p.pool = pool.to_string();
            p.enabled = enabled;
            p
        };
        let unrestricted = Model {
            name: "unrestricted".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            providers: vec![],
            pricing: None,
        };
        let restricted = Model {
            name: "restricted".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            providers: vec!["special".to_string()],
            pricing: None,
        };

        // No allowlist: every enabled provider is in the default pool.
        assert!(unrestricted.routes_to_provider(&provider("special/a", "special", true)));
        assert!(!unrestricted.routes_to_provider(&provider("special/a", "special", false)));
        // Allowlist matches on the pool name, so every member of that pool routes.
        assert!(restricted.routes_to_provider(&provider("special/a", "special", true)));
        assert!(!restricted.routes_to_provider(&provider("other/a", "other", true)));
        assert!(!restricted.routes_to_provider(&provider("special/a", "special", false)));
    }

    #[test]
    fn test_config_missing_providers() {
        let yaml_content = r#"
port: 8080
api_keys:
  - test-api-key
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("invalid_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let result = Config::load(Some(config_path.to_str().unwrap()));
        assert!(result.is_err());

        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("At least one provider is required"));
    }

    #[test]
    fn test_config_file_not_found() {
        let result = Config::load(Some("/nonexistent/path/config.yaml"));
        assert!(result.is_err());

        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Config file not found"));
    }

    #[test]
    fn test_default_bind() {
        assert_eq!(default_bind(), "127.0.0.1:8900");
    }

    #[test]
    fn test_parse_bind_address() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

        // Full IP:PORT
        assert_eq!(
            parse_bind_address("127.0.0.1:8900").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8900)
        );
        assert_eq!(
            parse_bind_address("0.0.0.0:9000").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 9000)
        );

        // IP only (default port 8900)
        assert_eq!(
            parse_bind_address("127.0.0.1").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8900)
        );
        assert_eq!(
            parse_bind_address("0.0.0.0").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8900)
        );

        // IPv6 with port
        assert_eq!(
            parse_bind_address("[::1]:9000").unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9000)
        );

        // IPv6 without port
        assert_eq!(
            parse_bind_address("::1").unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8900)
        );

        // Invalid
        assert!(parse_bind_address("not-an-address").is_err());
        assert!(parse_bind_address("").is_err());
    }

    #[test]
    fn test_partial_config_merge() {
        let config_file = ConfigFile {
            log_level: Some("INFO".to_string()),
            bind: "0.0.0.0:3000".to_string(),
            providers: vec![ProviderConfig {
                name: "test".to_string(),
                kind: ProviderKind::Aicore,
                auth: ProviderAuth::ClientCredentials,
                uaa_token_url: Some("https://example.com".to_string()),
                uaa_client_id: Some("client123".to_string()),
                uaa_client_secret: Some("secret456".to_string()),
                genai_api_url: Some("https://api.example.com".to_string()),
                base_url: None,
                api_key: None,
                auth_header: None,
                prefixes: None,
                resource_group: Some("test-group".to_string()),
                weight: None,
                credentials: None,
                credentials_file: None,
                cooldown_secs: None,
                enabled: true,
                unknown: HashMap::new(),
            }],
            models: vec![Model {
                name: "model1".to_string(),
                aicore_model_name: Some("aicore-model-1".to_string()),
                aliases: vec![],
                providers: vec![],
                pricing: None,
            }],
            refresh_interval_secs: None,
            fallback_models: FallbackModels::default(),
            api_keys: vec![ApiKeyEntry::Simple("key789".to_string())],
            log_requests: None,
            openai_api_version: None,
            quotas: QuotaConfig::default(),
            unknown: HashMap::new(),
        };

        let config = Config::from_file_and_env(config_file, Path::new("."))
            .expect("Failed to create config");

        assert_eq!(config.bind, "0.0.0.0:3000");
        assert_eq!(config.providers.len(), 1);
        assert_eq!(
            config.providers[0].uaa_token_url,
            "https://example.com/oauth/token"
        );
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "model1");
        assert_eq!(
            config.models[0].aicore_model_name,
            Some("aicore-model-1".to_string())
        );
        assert_eq!(config.providers[0].resource_group, "test-group");
        assert_eq!(config.api_key_strings(), vec!["key789".to_string()]);
    }

    #[test]
    fn test_token_url_automatic_oauth_token_suffix() {
        // Test case 1: URL without any path should get /oauth/token appended
        assert_eq!(
            normalize_oauth_token_url("https://auth.example.com".to_string()),
            "https://auth.example.com/oauth/token"
        );

        // Test case 2: URL ending with slash should get oauth/token appended
        assert_eq!(
            normalize_oauth_token_url("https://auth.example.com/".to_string()),
            "https://auth.example.com/oauth/token"
        );

        // Test case 3: URL already containing /oauth/token should remain unchanged
        assert_eq!(
            normalize_oauth_token_url("https://auth.example.com/oauth/token".to_string()),
            "https://auth.example.com/oauth/token"
        );

        // Test case 4: URL with custom path containing /oauth/token should remain unchanged
        assert_eq!(
            normalize_oauth_token_url("https://auth.example.com/uaa/oauth/token".to_string()),
            "https://auth.example.com/uaa/oauth/token"
        );
    }

    #[test]
    fn test_fallback_models_parsing() {
        let yaml_content = r#"
bind: "127.0.0.1:8080"
providers:
  - name: default
    uaa_token_url: https://test.example.com/oauth/token
    uaa_client_id: test-client-id
    uaa_client_secret: test-client-secret
    genai_api_url: https://api.test.example.com
api_keys:
  - test-api-key
models:
  - name: claude-sonnet-4-5
    aicore_model_name: dep-claude
  - name: gpt-4o
    aicore_model_name: dep-gpt
  - name: gemini-1.5-pro
    aicore_model_name: dep-gemini
fallback_models:
  claude: claude-sonnet-4-5
  openai: gpt-4o
  gemini: gemini-1.5-pro
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("fallback_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(
            config.fallback_models.claude,
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(config.fallback_models.openai, Some("gpt-4o".to_string()));
        assert_eq!(
            config.fallback_models.gemini,
            Some("gemini-1.5-pro".to_string())
        );
    }

    #[test]
    fn test_fallback_models_partial_config() {
        let yaml_content = r#"
bind: "127.0.0.1:8080"
providers:
  - name: default
    uaa_token_url: https://test.example.com/oauth/token
    uaa_client_id: test-client-id
    uaa_client_secret: test-client-secret
    genai_api_url: https://api.test.example.com
api_keys:
  - test-api-key
models:
  - name: claude-sonnet-4-5
    aicore_model_name: dep-claude
fallback_models:
  claude: claude-sonnet-4-5
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("partial_fallback_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(
            config.fallback_models.claude,
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(config.fallback_models.openai, None);
        assert_eq!(config.fallback_models.gemini, None);
    }

    #[test]
    fn test_fallback_models_default_empty() {
        let yaml_content = r#"
bind: "127.0.0.1:8080"
providers:
  - name: default
    uaa_token_url: https://test.example.com/oauth/token
    uaa_client_id: test-client-id
    uaa_client_secret: test-client-secret
    genai_api_url: https://api.test.example.com
api_keys:
  - test-api-key
models:
  - name: gpt-4
    aicore_model_name: dep-123
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("no_fallback_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.fallback_models.claude, None);
        assert_eq!(config.fallback_models.openai, None);
        assert_eq!(config.fallback_models.gemini, None);
    }

    #[test]
    fn test_multiple_api_keys() {
        let yaml_content = r#"
bind: "127.0.0.1:8080"
providers:
  - name: default
    uaa_token_url: https://test.example.com/oauth/token
    uaa_client_id: test-client-id
    uaa_client_secret: test-client-secret
    genai_api_url: https://api.test.example.com
models:
  - name: gpt-4
    aicore_model_name: dep-123
api_keys:
  - key-one
  - key-two
  - key-three
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("multi_api_keys_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.api_keys.len(), 3);
        assert_eq!(config.api_keys[0].key, "key-one");
        assert_eq!(config.api_keys[1].key, "key-two");
        assert_eq!(config.api_keys[2].key, "key-three");
    }

    #[test]
    fn test_api_keys_deduplication() {
        let yaml_content = r#"
bind: "127.0.0.1:8080"
providers:
  - name: default
    uaa_token_url: https://test.example.com/oauth/token
    uaa_client_id: test-client-id
    uaa_client_secret: test-client-secret
    genai_api_url: https://api.test.example.com
models:
  - name: gpt-4
    aicore_model_name: dep-123
api_keys:
  - duplicate-key
  - another-key
  - duplicate-key
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("dedup_api_keys_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        // Should deduplicate, keeping the first occurrence
        assert_eq!(config.api_keys.len(), 2);
        assert!(config.api_keys.iter().any(|k| k.key == "duplicate-key"));
        assert!(config.api_keys.iter().any(|k| k.key == "another-key"));
    }

    #[test]
    fn test_multi_provider_config() {
        let yaml_content = r#"
port: 8080
api_keys:
  - shared-api-key
providers:
  - name: provider1
    uaa_token_url: https://provider1.example.com/oauth/token
    uaa_client_id: client1
    uaa_client_secret: secret1
    genai_api_url: https://api1.example.com
    resource_group: rg1
    enabled: true
  - name: provider2
    uaa_token_url: https://provider2.example.com/oauth/token
    uaa_client_id: client2
    uaa_client_secret: secret2
    genai_api_url: https://api2.example.com
    resource_group: rg2
    enabled: true
  - name: provider3-disabled
    uaa_token_url: https://provider3.example.com/oauth/token
    uaa_client_id: client3
    uaa_client_secret: secret3
    genai_api_url: https://api3.example.com
    resource_group: rg3
    enabled: false
models:
  - name: gpt-4
    aicore_model_name: dep-123
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("multi_provider_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.providers.len(), 3);

        // Check provider1
        assert_eq!(config.providers[0].name, "provider1");
        assert_eq!(
            config.providers[0].uaa_token_url,
            "https://provider1.example.com/oauth/token"
        );
        assert_eq!(config.providers[0].resource_group, "rg1");
        assert!(config.providers[0].enabled);

        // Check provider2
        assert_eq!(config.providers[1].name, "provider2");
        assert_eq!(config.providers[1].resource_group, "rg2");

        // Check disabled provider
        assert_eq!(config.providers[2].name, "provider3-disabled");
        assert!(!config.providers[2].enabled);

        assert_eq!(config.api_key_strings(), vec!["shared-api-key".to_string()]);
    }

    #[test]
    fn test_example_config_is_valid() {
        let example_path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/config.yaml");
        let config = Config::load(Some(example_path))
            .expect("examples/config.yaml should be a valid config");

        // Verify structure is parsed correctly
        assert_eq!(config.bind, "127.0.0.1:8900");
        assert_eq!(config.log_level, "info");
        assert_eq!(config.providers.len(), 4);
        assert_eq!(config.providers[0].name, "primary");
        assert_eq!(config.providers[1].name, "secondary");
        assert!(config.providers[0].enabled);
        assert!(config.providers[1].enabled);
        assert!(!config.providers[0].is_passthrough());
        // Passthrough providers parse and normalize correctly.
        assert_eq!(config.providers[2].name, "downstream-acr");
        assert!(config.providers[2].is_passthrough());
        assert_eq!(config.providers[2].base_url, "http://downstream-acr:8900");
        assert_eq!(config.providers[3].name, "gateway");
        assert_eq!(config.providers[3].prefixes.openai, "/openai");
        // Anthropic/Gemini keep their defaults so the gateway's Claude/Gemini dialects route too.
        assert_eq!(config.providers[3].prefixes.anthropic, "/anthropic");
        assert_eq!(config.providers[3].prefixes.gemini, "/gemini");
        assert_eq!(config.api_keys.len(), 4);
        assert!(!config.models.is_empty());
        assert_eq!(config.refresh_interval_secs, 300);
        assert!(config.log_requests.enabled);
        assert_eq!(config.log_requests.retention_days, 30);
        assert_eq!(config.openai_api_version, "2025-04-01-preview");
        assert!(config.quotas.enabled);
        assert_eq!(config.quotas.daily_token_limit, Some(1000000));
        assert_eq!(config.quotas.monthly_token_limit, Some(20000000));
        assert!(config.fallback_models.claude.is_some());
        assert!(config.fallback_models.openai.is_some());
        assert!(config.fallback_models.gemini.is_some());

        // Verify pricing is parsed from example config
        let claude_pricing = config.get_model_pricing("claude-sonnet-4-6");
        assert!(
            claude_pricing.is_some(),
            "claude-sonnet-4-6 should have pricing"
        );
        let cp = claude_pricing.unwrap();
        assert_eq!(cp.input, Some(3.00));
        assert_eq!(cp.output, Some(15.00));
        assert_eq!(cp.cache_read, Some(0.30));
        assert_eq!(cp.cache_write, Some(3.75));

        // gpt-5-mini has partial pricing (no cache fields)
        let gpt_pricing = config.get_model_pricing("gpt-5-mini");
        assert!(gpt_pricing.is_some(), "gpt-5-mini should have pricing");
        let gp = gpt_pricing.unwrap();
        assert_eq!(gp.input, Some(0.25));
        assert_eq!(gp.output, Some(2.00));
        assert_eq!(gp.cache_read, None);
        assert_eq!(gp.cache_write, None);
    }

    #[test]
    fn test_calculate_cost_full_pricing() {
        let pricing = ModelPricing {
            input: Some(3.00),
            output: Some(15.00),
            cache_read: Some(0.30),
            cache_write: Some(3.75),
        };

        // 1M input tokens = $3.00, 500K output = $7.50, 200K cache_read = $0.06, 100K cache_write = $0.375
        let tokens = TokenCounts {
            input: 1_000_000,
            output: 500_000,
            cache_read: 200_000,
            cache_write: 100_000,
        };
        let cost = pricing.calculate_cost(&tokens);
        let expected = 3.00 + 7.50 + 0.06 + 0.375;
        assert!(
            (cost - expected).abs() < 1e-10,
            "cost={cost}, expected={expected}"
        );
    }

    #[test]
    fn test_calculate_cost_partial_pricing() {
        // Only input and output rates set, cache rates missing
        let pricing = ModelPricing {
            input: Some(0.25),
            output: Some(2.00),
            cache_read: None,
            cache_write: None,
        };

        let tokens = TokenCounts {
            input: 100_000,
            output: 50_000,
            cache_read: 30_000,
            cache_write: 10_000,
        };
        let cost = pricing.calculate_cost(&tokens);
        // Only input (0.025) + output (0.10) = 0.125; cache types contribute 0
        let expected = 0.025 + 0.10;
        assert!(
            (cost - expected).abs() < 1e-10,
            "cost={cost}, expected={expected}"
        );
    }

    #[test]
    fn test_calculate_cost_no_pricing() {
        // All fields None
        let pricing = ModelPricing {
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
        };

        let tokens = TokenCounts {
            input: 1_000_000,
            output: 500_000,
            cache_read: 200_000,
            cache_write: 100_000,
        };
        let cost = pricing.calculate_cost(&tokens);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn test_is_partial_detection() {
        // Full pricing — not partial regardless of cache usage
        let full = ModelPricing {
            input: Some(3.00),
            output: Some(15.00),
            cache_read: Some(0.30),
            cache_write: Some(3.75),
        };
        assert!(!full.is_partial(&TokenCounts {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5
        }));
        assert!(!full.is_partial(&TokenCounts {
            input: 100,
            output: 50,
            cache_read: 0,
            cache_write: 0
        }));

        // Missing cache_read but no cache read usage — not partial
        let no_cache = ModelPricing {
            input: Some(0.25),
            output: Some(2.00),
            cache_read: None,
            cache_write: None,
        };
        assert!(!no_cache.is_partial(&TokenCounts {
            input: 100,
            output: 50,
            cache_read: 0,
            cache_write: 0
        }));
        // With cache read usage — partial
        assert!(no_cache.is_partial(&TokenCounts {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 0
        }));
        // With cache write usage — partial
        assert!(no_cache.is_partial(&TokenCounts {
            input: 100,
            output: 50,
            cache_read: 0,
            cache_write: 5
        }));
        // Both cache types used — partial
        assert!(no_cache.is_partial(&TokenCounts {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5
        }));

        // Missing input — always partial
        let no_input = ModelPricing {
            input: None,
            output: Some(2.00),
            cache_read: Some(0.30),
            cache_write: Some(3.75),
        };
        assert!(no_input.is_partial(&TokenCounts {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0
        }));
    }

    #[test]
    fn test_get_model_pricing() {
        let yaml_content = r#"
bind: "127.0.0.1:8080"
providers:
  - name: default
    uaa_token_url: https://test.example.com/oauth/token
    uaa_client_id: test-client-id
    uaa_client_secret: test-client-secret
    genai_api_url: https://api.test.example.com
api_keys:
  - test-api-key
models:
  - name: claude-sonnet-4-6
    aicore_model_name: anthropic--claude-4.6-sonnet
    pricing:
      input: 3.00
      output: 15.00
      cache_read: 0.30
      cache_write: 3.75
  - name: gpt-5-mini
    pricing:
      input: 0.25
      output: 2.00
  - name: gemini-2.5-pro
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("pricing_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        // Claude has full pricing
        let claude_pricing = config.get_model_pricing("claude-sonnet-4-6").unwrap();
        assert_eq!(claude_pricing.input, Some(3.00));
        assert_eq!(claude_pricing.output, Some(15.00));
        assert_eq!(claude_pricing.cache_read, Some(0.30));
        assert_eq!(claude_pricing.cache_write, Some(3.75));
        assert!(!claude_pricing.is_partial(&TokenCounts {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5
        }));

        // GPT has partial pricing (no cache)
        let gpt_pricing = config.get_model_pricing("gpt-5-mini").unwrap();
        assert_eq!(gpt_pricing.input, Some(0.25));
        assert_eq!(gpt_pricing.output, Some(2.00));
        assert_eq!(gpt_pricing.cache_read, None);
        assert_eq!(gpt_pricing.cache_write, None);
        assert!(gpt_pricing.is_partial(&TokenCounts {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 0
        }));

        // Gemini has no pricing
        assert!(config.get_model_pricing("gemini-2.5-pro").is_none());

        // Non-existent model
        assert!(config.get_model_pricing("unknown-model").is_none());
    }

    #[test]
    fn test_normalize_prefix() {
        assert_eq!(normalize_prefix(String::new()), "");
        assert_eq!(normalize_prefix("  ".to_string()), "");
        assert_eq!(normalize_prefix("/openai".to_string()), "/openai");
        assert_eq!(normalize_prefix("openai".to_string()), "/openai");
        assert_eq!(normalize_prefix("/openai/".to_string()), "/openai");
        assert_eq!(normalize_prefix("/".to_string()), "");
    }

    #[test]
    fn test_passthrough_prefixes_defaults_and_overrides() {
        // Empty config = acr-shaped defaults.
        let d = PassthroughPrefixesConfig::default().resolve();
        assert_eq!(d.openai, "");
        assert_eq!(d.anthropic, "/anthropic");
        assert_eq!(d.gemini, "/gemini");

        // Partial override normalizes and keeps other defaults.
        let gateway = PassthroughPrefixesConfig {
            openai: Some("openai/".to_string()),
            anthropic: None,
            gemini: None,
        }
        .resolve();
        assert_eq!(gateway.openai, "/openai");
        assert_eq!(gateway.anthropic, "/anthropic");
        assert_eq!(gateway.gemini, "/gemini");
    }

    #[test]
    fn test_passthrough_auth_header() {
        let mut p = Provider {
            name: "p".to_string(),
            kind: ProviderKind::Passthrough,
            auth: ProviderAuth::ClientCredentials,
            uaa_token_url: String::new(),
            uaa_client_id: String::new(),
            uaa_client_secret: String::new(),
            genai_api_url: String::new(),
            base_url: "http://x".to_string(),
            api_key: "secret".to_string(),
            auth_header: "authorization".to_string(),
            prefixes: PassthroughPrefixes::default(),
            resource_group: "default".to_string(),
            weight: 1,
            pool: "p".to_string(),
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
            enabled: true,
        };
        // authorization -> Bearer-prefixed
        assert_eq!(
            p.passthrough_auth_header(),
            ("authorization".to_string(), "Bearer secret".to_string())
        );
        // any other header -> raw value
        p.auth_header = "x-api-key".to_string();
        assert_eq!(
            p.passthrough_auth_header(),
            ("x-api-key".to_string(), "secret".to_string())
        );
    }

    #[test]
    fn test_passthrough_provider_parses_and_resolves() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("passthrough.yaml");
        let yaml_content = r#"
providers:
  - name: downstream
    kind: passthrough
    base_url: http://downstream:8900/
    api_key: test-key
    auth_header: x-api-key
  - name: gateway
    kind: passthrough
    base_url: http://127.0.0.1:6655
    api_key: test-key
    prefixes:
      openai: /openai/
api_keys:
  - local-key
"#;
        fs::write(&config_path, yaml_content).expect("Failed to write config file");
        let config = Config::load(Some(config_path.to_str().unwrap())).expect("load");

        let downstream = &config.providers[0];
        assert!(downstream.is_passthrough());
        assert_eq!(downstream.base_url, "http://downstream:8900"); // trailing slash trimmed
        assert_eq!(downstream.api_key, "test-key");
        assert_eq!(downstream.auth_header, "x-api-key");
        // acr-shaped defaults: OpenAI at the root.
        assert_eq!(downstream.prefixes.openai, "");
        assert_eq!(downstream.prefixes.anthropic, "/anthropic");

        let gateway = &config.providers[1];
        assert_eq!(gateway.prefixes.openai, "/openai"); // overridden + normalized
        assert_eq!(gateway.prefixes.anthropic, "/anthropic"); // default kept
        assert_eq!(gateway.auth_header, "authorization"); // default

        // Non-fatal fields for AI Core are empty on a passthrough provider.
        assert!(downstream.genai_api_url.is_empty());
        assert!(downstream.uaa_token_url.is_empty());
    }

    #[test]
    fn test_passthrough_provider_requires_base_url_and_api_key() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("bad.yaml");
        let yaml_content = r#"
providers:
  - name: downstream
    kind: passthrough
    api_key: test-key
api_keys:
  - local-key
"#;
        fs::write(&config_path, yaml_content).expect("Failed to write config file");
        let err = Config::load(Some(config_path.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("base_url"), "got: {err}");
    }
}
