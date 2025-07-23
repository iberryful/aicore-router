use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub uaa_token_url: String,
    pub uaa_client_id: String,
    pub uaa_client_secret: String,
    pub genai_api_url: String,
    pub api_key: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub models: HashMap<String, String>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub credentials: Option<Credentials>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Credentials {
    pub uaa_token_url: Option<String>,
    pub uaa_client_id: Option<String>,
    pub uaa_client_secret: Option<String>,
    pub aicore_api_url: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    pub name: String,
    pub deployment_id: String,
}

fn default_port() -> u16 {
    8900
}

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug)]
struct PartialConfig {
    uaa_token_url: Option<String>,
    uaa_client_id: Option<String>,
    uaa_client_secret: Option<String>,
    genai_api_url: Option<String>,
    api_key: Option<String>,
    port: u16,
    models: HashMap<String, String>,
    log_level: String,
}

impl PartialConfig {
    fn merge_file(&mut self, file_config: ConfigFile) {
        self.port = file_config.port;

        if let Some(log_level) = file_config.log_level {
            self.log_level = log_level;
        }

        if let Some(creds) = file_config.credentials {
            if let Some(mut url) = creds.uaa_token_url {
                // Automatically append /oauth/token if the URL doesn't end with a URI path
                if !url.contains("/oauth/token") && !url.ends_with('/') {
                    url.push_str("/oauth/token");
                } else if url.ends_with('/') && !url.contains("/oauth/token") {
                    url.push_str("oauth/token");
                }
                self.uaa_token_url = Some(url);
            }
            if let Some(id) = creds.uaa_client_id {
                self.uaa_client_id = Some(id);
            }
            if let Some(secret) = creds.uaa_client_secret {
                self.uaa_client_secret = Some(secret);
            }
            if let Some(api_url) = creds.aicore_api_url {
                self.genai_api_url = Some(api_url);
            }
            if let Some(key) = creds.api_key {
                self.api_key = Some(key);
            }
        }

        // Convert models from Vec<Model> to HashMap
        if !file_config.models.is_empty() {
            self.models = file_config
                .models
                .into_iter()
                .map(|m| (m.name, m.deployment_id))
                .collect();
        }
    }

    fn merge_env(&mut self) {
        if let Ok(val) = env::var("UAA_TOKEN_URL") {
            self.uaa_token_url = Some(val);
        }
        if let Ok(val) = env::var("UAA_CLIENT_ID") {
            self.uaa_client_id = Some(val);
        }
        if let Ok(val) = env::var("UAA_CLIENT_SECRET") {
            self.uaa_client_secret = Some(val);
        }
        if let Ok(val) = env::var("GENAI_API_URL") {
            self.genai_api_url = Some(val);
        }
        if let Ok(val) = env::var("API_KEY") {
            self.api_key = Some(val);
        }
        if let Ok(val) = env::var("PORT") {
            if let Ok(port) = val.parse::<u16>() {
                self.port = port;
            }
        }
        if let Ok(val) = env::var("LOG_LEVEL") {
            self.log_level = val;
        }
    }

    fn into_config(self) -> Result<Config> {
        Ok(Config {
            uaa_token_url: self
                .uaa_token_url
                .context("uaa_token_url is required in config file or UAA_TOKEN_URL env var")?,
            uaa_client_id: self
                .uaa_client_id
                .context("uaa_client_id is required in config file or UAA_CLIENT_ID env var")?,
            uaa_client_secret: self.uaa_client_secret.context(
                "uaa_client_secret is required in config file or UAA_CLIENT_SECRET env var",
            )?,
            genai_api_url: self
                .genai_api_url
                .context("aicore_api_url is required in config file or GENAI_API_URL env var")?,
            api_key: self
                .api_key
                .context("api_key is required in config file or API_KEY env var")?,
            port: self.port,
            models: self.models,
            log_level: self.log_level,
        })
    }
}

impl Default for PartialConfig {
    fn default() -> Self {
        Self {
            uaa_token_url: None,
            uaa_client_id: None,
            uaa_client_secret: None,
            genai_api_url: None,
            api_key: None,
            port: default_port(),
            models: HashMap::new(),
            log_level: default_log_level(),
        }
    }
}

impl Config {
    pub fn load(config_path: Option<&str>) -> Result<Self> {
        // Determine config file path
        let config_file_path = match config_path {
            Some(path) => path.to_string(),
            None => {
                let home = env::var("HOME").context("HOME environment variable not set")?;
                format!("{home}/.aicore/config.yaml")
            }
        };

        // Load config file (mandatory)
        if !Path::new(&config_file_path).exists() {
            return Err(anyhow::anyhow!(
                "Config file not found: {}. Please create a config file.",
                config_file_path
            ));
        }

        let config_content = std::fs::read_to_string(&config_file_path)
            .with_context(|| format!("Failed to read config file: {config_file_path}"))?;
        let config_file = serde_yaml::from_str::<ConfigFile>(&config_content)
            .with_context(|| format!("Failed to parse config file: {config_file_path}"))?;

        // Convert config file to final config
        let mut final_config = PartialConfig::default();
        final_config.merge_file(config_file);
        // Environment variables override config file values
        final_config.merge_env();
        final_config.into_config()
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
port: 9000
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: test-api-key
models:
  - name: gpt-4
    deployment_id: dep-123
  - name: claude-3
    deployment_id: dep-456
"#;

        let config_file: ConfigFile =
            serde_yaml::from_str(yaml_content).expect("Failed to parse YAML");

        assert_eq!(config_file.port, 9000);
        assert_eq!(config_file.log_level, Some("DEBUG".to_string()));
        assert_eq!(config_file.models.len(), 2);
        assert_eq!(config_file.models[0].name, "gpt-4");
        assert_eq!(config_file.models[0].deployment_id, "dep-123");

        let creds = config_file.credentials.unwrap();
        assert_eq!(
            creds.uaa_token_url,
            Some("https://test.example.com/oauth/token".to_string())
        );
        assert_eq!(creds.uaa_client_id, Some("test-client-id".to_string()));
        assert_eq!(creds.api_key, Some("test-api-key".to_string()));
    }

    #[test]
    fn test_config_load_from_file() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("test_config.yaml");

        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: test-api-key
models:
  - name: test-model
    deployment_id: test-deployment
"#;

        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.port, 8080);
        assert_eq!(config.uaa_token_url, "https://test.example.com/oauth/token");
        assert_eq!(config.uaa_client_id, "test-client-id");
        assert_eq!(config.genai_api_url, "https://api.test.example.com");
        assert_eq!(config.api_key, "test-api-key");
        assert_eq!(
            config.models.get("test-model"),
            Some(&"test-deployment".to_string())
        );
    }

    #[test]
    fn test_config_missing_required_fields() {
        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  # Missing required fields
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("invalid_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let result = Config::load(Some(config_path.to_str().unwrap()));
        assert!(result.is_err());

        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("required"));
    }

    #[test]
    fn test_config_file_not_found() {
        let result = Config::load(Some("/nonexistent/path/config.yaml"));
        assert!(result.is_err());

        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Config file not found"));
    }

    #[test]
    fn test_default_port() {
        assert_eq!(default_port(), 8900);
    }

    #[test]
    fn test_partial_config_merge() {
        let mut partial_config = PartialConfig::default();

        let config_file = ConfigFile {
            log_level: Some("INFO".to_string()),
            port: 3000,
            credentials: Some(Credentials {
                uaa_token_url: Some("https://example.com".to_string()),
                uaa_client_id: Some("client123".to_string()),
                uaa_client_secret: Some("secret456".to_string()),
                aicore_api_url: Some("https://api.example.com".to_string()),
                api_key: Some("key789".to_string()),
            }),
            models: vec![Model {
                name: "model1".to_string(),
                deployment_id: "dep1".to_string(),
            }],
        };

        partial_config.merge_file(config_file);

        assert_eq!(partial_config.port, 3000);
        assert_eq!(
            partial_config.uaa_token_url,
            Some("https://example.com/oauth/token".to_string())
        );
        assert_eq!(
            partial_config.models.get("model1"),
            Some(&"dep1".to_string())
        );
    }

    #[test]
    fn test_token_url_automatic_oauth_token_suffix() {
        let mut partial_config = PartialConfig::default();

        // Test case 1: URL without any path should get /oauth/token appended
        let config_file1 = ConfigFile {
            log_level: None,
            port: 8900,
            credentials: Some(Credentials {
                uaa_token_url: Some("https://auth.example.com".to_string()),
                uaa_client_id: Some("client".to_string()),
                uaa_client_secret: Some("secret".to_string()),
                aicore_api_url: Some("https://api.example.com".to_string()),
                api_key: Some("key".to_string()),
            }),
            models: vec![],
        };

        partial_config.merge_file(config_file1);
        assert_eq!(
            partial_config.uaa_token_url,
            Some("https://auth.example.com/oauth/token".to_string())
        );

        // Test case 2: URL ending with slash should get oauth/token appended
        let config_file2 = ConfigFile {
            log_level: None,
            port: 8900,
            credentials: Some(Credentials {
                uaa_token_url: Some("https://auth.example.com/".to_string()),
                uaa_client_id: Some("client".to_string()),
                uaa_client_secret: Some("secret".to_string()),
                aicore_api_url: Some("https://api.example.com".to_string()),
                api_key: Some("key".to_string()),
            }),
            models: vec![],
        };

        partial_config.merge_file(config_file2);
        assert_eq!(
            partial_config.uaa_token_url,
            Some("https://auth.example.com/oauth/token".to_string())
        );

        // Test case 3: URL already containing /oauth/token should remain unchanged
        let config_file3 = ConfigFile {
            log_level: None,
            port: 8900,
            credentials: Some(Credentials {
                uaa_token_url: Some("https://auth.example.com/oauth/token".to_string()),
                uaa_client_id: Some("client".to_string()),
                uaa_client_secret: Some("secret".to_string()),
                aicore_api_url: Some("https://api.example.com".to_string()),
                api_key: Some("key".to_string()),
            }),
            models: vec![],
        };

        partial_config.merge_file(config_file3);
        assert_eq!(
            partial_config.uaa_token_url,
            Some("https://auth.example.com/oauth/token".to_string())
        );

        // Test case 4: URL with custom path containing /oauth/token should remain unchanged
        let config_file4 = ConfigFile {
            log_level: None,
            port: 8900,
            credentials: Some(Credentials {
                uaa_token_url: Some("https://auth.example.com/uaa/oauth/token".to_string()),
                uaa_client_id: Some("client".to_string()),
                uaa_client_secret: Some("secret".to_string()),
                aicore_api_url: Some("https://api.example.com".to_string()),
                api_key: Some("key".to_string()),
            }),
            models: vec![],
        };

        partial_config.merge_file(config_file4);
        assert_eq!(
            partial_config.uaa_token_url,
            Some("https://auth.example.com/uaa/oauth/token".to_string())
        );
    }
}
