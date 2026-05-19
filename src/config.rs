use anyhow::{bail, Context, Result};
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
    /// Load balancing strategy for distributing requests across providers
    #[serde(default)]
    pub load_balancing: LoadBalancingStrategy,
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

/// A single AI Core provider configuration
#[derive(Clone, Deserialize, Serialize)]
pub struct Provider {
    /// Unique identifier for this provider
    pub name: String,
    /// UAA OAuth token URL
    pub uaa_token_url: String,
    /// UAA client ID
    pub uaa_client_id: String,
    /// UAA client secret
    pub uaa_client_secret: String,
    /// AI Core API base URL
    pub genai_api_url: String,
    /// Resource group for this provider
    #[serde(default = "default_resource_group")]
    pub resource_group: String,
    /// Weight for load balancing (higher = more traffic)
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Whether this provider is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("name", &self.name)
            .field("uaa_token_url", &self.uaa_token_url)
            .field("uaa_client_id", &self.uaa_client_id)
            .field("uaa_client_secret", &"[REDACTED]")
            .field("genai_api_url", &self.genai_api_url)
            .field("resource_group", &self.resource_group)
            .field("weight", &self.weight)
            .field("enabled", &self.enabled)
            .finish()
    }
}

fn default_weight() -> u32 {
    1
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
    /// Load balancing strategy
    #[serde(default)]
    pub load_balancing: LoadBalancingStrategy,
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
    /// UAA OAuth token URL
    pub uaa_token_url: String,
    /// UAA client ID
    pub uaa_client_id: String,
    /// UAA client secret
    pub uaa_client_secret: String,
    /// AI Core API base URL
    pub genai_api_url: String,
    /// Resource group for this provider
    #[serde(default)]
    pub resource_group: Option<String>,
    /// Weight for load balancing (higher = more traffic)
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Whether this provider is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Catch-all for unknown fields
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml_ng::Value>,
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
    /// Pricing per 1M tokens for cost estimation.
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
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

/// Load balancing strategy for distributing requests across providers.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancingStrategy {
    /// Round-robin: Distribute requests evenly across providers.
    /// Each request goes to the next provider in rotation.
    /// If a provider returns 429, automatically falls back to the next provider.
    #[default]
    RoundRobin,
    /// Fallback: Always try the first provider first.
    /// Only switch to the next provider if the current one returns 429 (rate limited).
    /// This prioritizes a primary provider while using others as backup.
    Fallback,
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
    },
}

impl From<ApiKeyEntry> for ApiKeyConfig {
    fn from(entry: ApiKeyEntry) -> Self {
        match entry {
            ApiKeyEntry::Simple(key) => ApiKeyConfig {
                key,
                daily_token_limit: None,
                monthly_token_limit: None,
            },
            ApiKeyEntry::WithConfig {
                key,
                daily_token_limit,
                monthly_token_limit,
            } => ApiKeyConfig {
                key,
                daily_token_limit,
                monthly_token_limit,
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
    bail!(
        "Invalid bind address '{s}'. Expected IP (e.g. 127.0.0.1) or IP:PORT (e.g. 0.0.0.0:9000)"
    )
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

        Self::from_file_and_env(file_config)
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
        self.models.iter().find(|m| m.name == model_name)?.pricing.as_ref()
    }

    fn from_file_and_env(file_config: ConfigFile) -> Result<Self> {
        // Build providers list from config file
        let mut providers: Vec<Provider> = Vec::new();

        for p in file_config.providers {
            providers.push(Provider {
                name: p.name,
                uaa_token_url: normalize_oauth_token_url(p.uaa_token_url),
                uaa_client_id: p.uaa_client_id,
                uaa_client_secret: p.uaa_client_secret,
                genai_api_url: p.genai_api_url,
                resource_group: p.resource_group.unwrap_or_else(default_resource_group),
                weight: p.weight,
                enabled: p.enabled,
            });
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

        let bind = file_config.bind;

        let log_level = file_config.log_level.unwrap_or_else(default_log_level);

        let refresh_interval_secs = file_config
            .refresh_interval_secs
            .unwrap_or_else(default_refresh_interval_secs);

        let models = file_config.models;
        let fallback_models = file_config.fallback_models;
        let load_balancing = file_config.load_balancing;

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
            load_balancing,
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
                uaa_token_url: "https://example.com".to_string(),
                uaa_client_id: "client123".to_string(),
                uaa_client_secret: "secret456".to_string(),
                genai_api_url: "https://api.example.com".to_string(),
                resource_group: Some("test-group".to_string()),
                weight: 1,
                enabled: true,
                unknown: HashMap::new(),
            }],
            models: vec![Model {
                name: "model1".to_string(),
                aicore_model_name: Some("aicore-model-1".to_string()),
                aliases: vec![],
                pricing: None,
            }],
            refresh_interval_secs: None,
            fallback_models: FallbackModels::default(),
            api_keys: vec![ApiKeyEntry::Simple("key789".to_string())],
            load_balancing: LoadBalancingStrategy::default(),
            log_requests: None,
            openai_api_version: None,
            quotas: QuotaConfig::default(),
            unknown: HashMap::new(),
        };

        let config = Config::from_file_and_env(config_file).expect("Failed to create config");

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
    weight: 2
    enabled: true
  - name: provider2
    uaa_token_url: https://provider2.example.com/oauth/token
    uaa_client_id: client2
    uaa_client_secret: secret2
    genai_api_url: https://api2.example.com
    resource_group: rg2
    weight: 1
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
        assert_eq!(config.providers[0].weight, 2);
        assert!(config.providers[0].enabled);

        // Check provider2
        assert_eq!(config.providers[1].name, "provider2");
        assert_eq!(config.providers[1].resource_group, "rg2");
        assert_eq!(config.providers[1].weight, 1);

        // Check disabled provider
        assert_eq!(config.providers[2].name, "provider3-disabled");
        assert!(!config.providers[2].enabled);

        assert_eq!(config.api_key_strings(), vec!["shared-api-key".to_string()]);
    }

    #[test]
    fn test_example_config_is_valid() {
        let example_path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/config.yaml");
        let config = Config::load(Some(example_path)).expect("examples/config.yaml should be a valid config");

        // Verify structure is parsed correctly
        assert_eq!(config.bind, "127.0.0.1:8900");
        assert_eq!(config.log_level, "info");
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.providers[0].name, "primary");
        assert_eq!(config.providers[1].name, "secondary");
        assert!(config.providers[0].enabled);
        assert!(config.providers[1].enabled);
        assert_eq!(config.api_keys.len(), 4);
        assert!(!config.models.is_empty());
        assert_eq!(config.refresh_interval_secs, 300);
        assert_eq!(config.load_balancing, LoadBalancingStrategy::RoundRobin);
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
        assert!(claude_pricing.is_some(), "claude-sonnet-4-6 should have pricing");
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
        let tokens = TokenCounts { input: 1_000_000, output: 500_000, cache_read: 200_000, cache_write: 100_000 };
        let cost = pricing.calculate_cost(&tokens);
        let expected = 3.00 + 7.50 + 0.06 + 0.375;
        assert!((cost - expected).abs() < 1e-10, "cost={cost}, expected={expected}");
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

        let tokens = TokenCounts { input: 100_000, output: 50_000, cache_read: 30_000, cache_write: 10_000 };
        let cost = pricing.calculate_cost(&tokens);
        // Only input (0.025) + output (0.10) = 0.125; cache types contribute 0
        let expected = 0.025 + 0.10;
        assert!((cost - expected).abs() < 1e-10, "cost={cost}, expected={expected}");
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

        let tokens = TokenCounts { input: 1_000_000, output: 500_000, cache_read: 200_000, cache_write: 100_000 };
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
        assert!(!full.is_partial(&TokenCounts { input: 100, output: 50, cache_read: 10, cache_write: 5 }));
        assert!(!full.is_partial(&TokenCounts { input: 100, output: 50, cache_read: 0, cache_write: 0 }));

        // Missing cache_read but no cache read usage — not partial
        let no_cache = ModelPricing {
            input: Some(0.25),
            output: Some(2.00),
            cache_read: None,
            cache_write: None,
        };
        assert!(!no_cache.is_partial(&TokenCounts { input: 100, output: 50, cache_read: 0, cache_write: 0 }));
        // With cache read usage — partial
        assert!(no_cache.is_partial(&TokenCounts { input: 100, output: 50, cache_read: 10, cache_write: 0 }));
        // With cache write usage — partial
        assert!(no_cache.is_partial(&TokenCounts { input: 100, output: 50, cache_read: 0, cache_write: 5 }));
        // Both cache types used — partial
        assert!(no_cache.is_partial(&TokenCounts { input: 100, output: 50, cache_read: 10, cache_write: 5 }));

        // Missing input — always partial
        let no_input = ModelPricing {
            input: None,
            output: Some(2.00),
            cache_read: Some(0.30),
            cache_write: Some(3.75),
        };
        assert!(no_input.is_partial(&TokenCounts { input: 0, output: 0, cache_read: 0, cache_write: 0 }));
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

        let config = Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        // Claude has full pricing
        let claude_pricing = config.get_model_pricing("claude-sonnet-4-6").unwrap();
        assert_eq!(claude_pricing.input, Some(3.00));
        assert_eq!(claude_pricing.output, Some(15.00));
        assert_eq!(claude_pricing.cache_read, Some(0.30));
        assert_eq!(claude_pricing.cache_write, Some(3.75));
        assert!(!claude_pricing.is_partial(&TokenCounts { input: 100, output: 50, cache_read: 10, cache_write: 5 }));

        // GPT has partial pricing (no cache)
        let gpt_pricing = config.get_model_pricing("gpt-5-mini").unwrap();
        assert_eq!(gpt_pricing.input, Some(0.25));
        assert_eq!(gpt_pricing.output, Some(2.00));
        assert_eq!(gpt_pricing.cache_read, None);
        assert_eq!(gpt_pricing.cache_write, None);
        assert!(gpt_pricing.is_partial(&TokenCounts { input: 100, output: 50, cache_read: 10, cache_write: 0 }));

        // Gemini has no pricing
        assert!(config.get_model_pricing("gemini-2.5-pro").is_none());

        // Non-existent model
        assert!(config.get_model_pricing("unknown-model").is_none());
    }
}
