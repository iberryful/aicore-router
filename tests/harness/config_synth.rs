//! Synthesized e2e test config.
//!
//! Loads the user's real `~/.aicore/config.yaml` (creds for SAP AI Core
//! providers + model definitions), copies the live bits we cannot synthesize
//! (`providers`, `models`, `fallback_models`, `openai_api_version`), and
//! injects our own `api_keys`, `quotas`, `log_requests`, and `bind` so the
//! e2e suite can drive deterministic limit / DB-row scenarios without
//! disturbing the user's setup.
//!
//! Fails loudly if the user has no providers configured — the e2e suite has
//! no real backend to test against.

#![cfg(feature = "e2e")]

use std::io::Write;
use std::path::{Path, PathBuf};

use aicore_router::config::Config;
use tempfile::NamedTempFile;

/// Test API key with permissive global quota — used by every happy-path test.
pub const KEY_DEFAULT: &str = "acr-test-default-key-do-not-use-in-prod";

/// Test API key with `requests_per_minute: 3` for RPM-rejection scenarios.
pub const KEY_RPM_LIMITED: &str = "acr-test-rpm-limited-key-do-not-use-in-prod";

/// Test API key with `daily_token_limit: 50` for token-quota scenarios.
pub const KEY_TIGHT_TOKENS: &str = "acr-test-tight-tokens-key-do-not-use-in-prod";

pub struct SynthesizedConfig {
    /// Tempfile holding the synthesized YAML. Kept alive so `acr` can read it
    /// for the lifetime of the suite.
    _file: NamedTempFile,
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    pub bind_port: u16,
    /// Snapshot of the providers from the user's config — exposed for future
    /// scenarios that need to assert provider names; kept around even when
    /// no live test references it yet.
    #[allow(dead_code)]
    pub provider_names: Vec<String>,
    /// Model names from the user's config — tests use this to pick a model
    /// available in the actual deployment surface.
    pub model_names: Vec<String>,
    /// Fallback models keyed by family, copied from the user's config.
    #[allow(dead_code)]
    pub fallback_claude: Option<String>,
    #[allow(dead_code)]
    pub fallback_openai: Option<String>,
    #[allow(dead_code)]
    pub fallback_gemini: Option<String>,
}

impl SynthesizedConfig {
    /// Build a synthesized config for the e2e suite, binding to `port`.
    /// Panics with an actionable message if the user has not configured any
    /// providers — there is no real backend for the e2e suite to test against.
    pub fn build(port: u16) -> Self {
        let user_config = Config::load(None).unwrap_or_else(|e| {
            panic!(
                "e2e suite requires a working user config at ~/.aicore/config.yaml \
                 (real SAP AI Core providers + models). Failed to load: {e}\n\
                 See examples/config.yaml for the expected shape."
            );
        });

        if user_config.providers.iter().all(|p| !p.enabled) {
            panic!(
                "e2e suite requires at least one enabled provider in \
                 ~/.aicore/config.yaml — found {} configured but none enabled. \
                 The e2e suite tests against a live backend; there is nothing to \
                 short-circuit to.",
                user_config.providers.len()
            );
        }

        let provider_names: Vec<String> = user_config
            .providers
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let model_names: Vec<String> = user_config.models.iter().map(|m| m.name.clone()).collect();

        let db_file = NamedTempFile::new().expect("create temp DB file");
        let db_path = db_file.path().to_path_buf();
        // Drop the file handle so the SQLite client can open it fresh.
        drop(db_file);

        let yaml = render_yaml(&user_config, port, &db_path);

        let mut config_file = NamedTempFile::with_suffix(".yaml").expect("create temp config");
        config_file
            .write_all(yaml.as_bytes())
            .expect("write synthesized config");
        config_file.flush().expect("flush synthesized config");

        let config_path = config_file.path().to_path_buf();

        Self {
            _file: config_file,
            config_path,
            db_path,
            bind_port: port,
            provider_names,
            model_names,
            fallback_claude: user_config.fallback_models.claude.clone(),
            fallback_openai: user_config.fallback_models.openai.clone(),
            fallback_gemini: user_config.fallback_models.gemini.clone(),
        }
    }

    /// Returns the first model name whose normalized prefix matches `family`,
    /// or `None` if the user has not configured one. Used by family-specific
    /// scenarios to skip when the relevant backend isn't deployed.
    pub fn model_for_family(&self, family_prefix: &str) -> Option<&str> {
        self.model_names
            .iter()
            .find(|n| n.starts_with(family_prefix))
            .map(|s| s.as_str())
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.bind_port)
    }
}

fn render_yaml(user: &Config, port: u16, db_path: &Path) -> String {
    let mut yaml = String::new();
    yaml.push_str("log_level: info\n");
    yaml.push_str(&format!("bind: \"127.0.0.1:{}\"\n", port));
    yaml.push_str(&format!(
        "openai_api_version: \"{}\"\n",
        user.openai_api_version
    ));
    yaml.push_str("refresh_interval_secs: 300\n\n");

    // Test API keys with deterministic per-key limits.
    yaml.push_str("api_keys:\n");
    yaml.push_str(&format!("  - {}\n", KEY_DEFAULT));
    yaml.push_str(&format!("  - key: {}\n", KEY_RPM_LIMITED));
    yaml.push_str("    requests_per_minute: 3\n");
    yaml.push_str(&format!("  - key: {}\n", KEY_TIGHT_TOKENS));
    yaml.push_str("    daily_token_limit: 50\n\n");

    // Permissive global quotas — only the dedicated keys above hit limits.
    yaml.push_str("quotas:\n");
    yaml.push_str("  enabled: true\n");
    yaml.push_str("  requests_per_minute: 600\n\n");

    // Request log to a known temp file so tests can read rows back.
    yaml.push_str("log_requests:\n");
    yaml.push_str("  enabled: true\n");
    yaml.push_str(&format!(
        "  db_path: \"{}\"\n",
        db_path.to_str().expect("db path utf8")
    ));
    yaml.push_str("  retention_days: 1\n\n");

    // Real providers (creds + URLs) lifted verbatim from the user's config.
    yaml.push_str("load_balancing: ");
    yaml.push_str(match user.load_balancing {
        aicore_router::config::LoadBalancingStrategy::RoundRobin => "round_robin\n",
        aicore_router::config::LoadBalancingStrategy::Fallback => "fallback\n",
    });
    yaml.push_str("providers:\n");
    for p in &user.providers {
        yaml.push_str(&format!("  - name: {}\n", yaml_escape(&p.name)));
        yaml.push_str(&format!(
            "    uaa_token_url: {}\n",
            yaml_escape(&p.uaa_token_url)
        ));
        yaml.push_str(&format!(
            "    uaa_client_id: {}\n",
            yaml_escape(&p.uaa_client_id)
        ));
        yaml.push_str(&format!(
            "    uaa_client_secret: {}\n",
            yaml_escape(&p.uaa_client_secret)
        ));
        yaml.push_str(&format!(
            "    genai_api_url: {}\n",
            yaml_escape(&p.genai_api_url)
        ));
        yaml.push_str(&format!(
            "    resource_group: {}\n",
            yaml_escape(&p.resource_group)
        ));
        yaml.push_str(&format!("    weight: {}\n", p.weight));
        yaml.push_str(&format!("    enabled: {}\n", p.enabled));
    }
    yaml.push('\n');

    // Models — names + aliases + aicore mapping. Pricing is irrelevant for
    // e2e behavior, so we drop it to keep the synthesized YAML small.
    yaml.push_str("models:\n");
    for m in &user.models {
        yaml.push_str(&format!("  - name: {}\n", yaml_escape(&m.name)));
        if let Some(ref aicore) = m.aicore_model_name {
            yaml.push_str(&format!("    aicore_model_name: {}\n", yaml_escape(aicore)));
        }
        if !m.aliases.is_empty() {
            yaml.push_str("    aliases:\n");
            for alias in &m.aliases {
                yaml.push_str(&format!("      - {}\n", yaml_escape(alias)));
            }
        }
    }
    yaml.push('\n');

    yaml.push_str("fallback_models:\n");
    if let Some(ref m) = user.fallback_models.claude {
        yaml.push_str(&format!("  claude: {}\n", yaml_escape(m)));
    }
    if let Some(ref m) = user.fallback_models.openai {
        yaml.push_str(&format!("  openai: {}\n", yaml_escape(m)));
    }
    if let Some(ref m) = user.fallback_models.gemini {
        yaml.push_str(&format!("  gemini: {}\n", yaml_escape(m)));
    }

    yaml
}

/// Quote any value that could trip YAML's bare-string parser. We use
/// double-quoted form and escape backslash + double quote — sufficient for
/// the small set of fields we write.
fn yaml_escape(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}
