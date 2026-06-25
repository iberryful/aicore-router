//! CLI command handlers for administrative operations.

#[cfg(feature = "db")]
use crate::table::format_number;
use crate::table::{Align, CliTable, Col};
use crate::{client::AiCoreClient, config::Config, token::TokenManager};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct CommandHandler {
    client: AiCoreClient,
    config: Config,
}

/// Picked Claude models for the per-family `ANTHROPIC_*_MODEL` env vars that
/// `acr configure claude` writes into `~/.claude/settings.json`.
///
/// Each field is `Some(name)` when a model in that sub-family is configured in
/// the user's aicore config, or `None` when none is. The chosen name within a
/// sub-family is the **newest version**, picked by numeric comparison on the
/// trailing `-<major>-<minor>` segments of the model name (so `claude-opus-4-10`
/// correctly beats `claude-opus-4-2`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeModelChoices {
    pub opus: Option<String>,
    pub sonnet: Option<String>,
    pub haiku: Option<String>,
}

impl ClaudeModelChoices {
    /// Pick the newest configured model in each Claude sub-family.
    /// Picks are independent: any subset (or none) may be present.
    pub fn from_models(models: &[crate::config::Model]) -> Self {
        Self {
            opus: pick_newest_in_family(models, "opus"),
            sonnet: pick_newest_in_family(models, "sonnet"),
            haiku: pick_newest_in_family(models, "haiku"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.opus.is_none() && self.sonnet.is_none() && self.haiku.is_none()
    }

    /// Build the `ANTHROPIC_*` env-var pairs for Claude Code's `settings.json`.
    ///
    /// Each `ANTHROPIC_DEFAULT_*_MODEL` is emitted only when a model exists for
    /// that sub-family. `ANTHROPIC_MODEL` defaults to opus → sonnet → haiku in
    /// availability order; `ANTHROPIC_SMALL_FAST_MODEL` prefers haiku → sonnet
    /// → opus. Model names are written verbatim — appending the `[1m]` client
    /// hint is left to the user, since acr itself strips the suffix server-side.
    pub fn env_vars(&self) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = Vec::new();

        if let Some(name) = self.primary_model() {
            out.push(("ANTHROPIC_MODEL", name.to_string()));
        }
        if let Some(name) = &self.opus {
            out.push(("ANTHROPIC_DEFAULT_OPUS_MODEL", name.clone()));
        }
        if let Some(name) = &self.sonnet {
            out.push(("ANTHROPIC_DEFAULT_SONNET_MODEL", name.clone()));
        }
        if let Some(name) = &self.haiku {
            out.push(("ANTHROPIC_DEFAULT_HAIKU_MODEL", name.clone()));
        }
        if let Some(name) = self.small_fast_model() {
            out.push(("ANTHROPIC_SMALL_FAST_MODEL", name.to_string()));
        }

        out
    }

    fn primary_model(&self) -> Option<&str> {
        self.opus
            .as_deref()
            .or(self.sonnet.as_deref())
            .or(self.haiku.as_deref())
    }

    fn small_fast_model(&self) -> Option<&str> {
        self.haiku
            .as_deref()
            .or(self.sonnet.as_deref())
            .or(self.opus.as_deref())
    }
}

fn pick_newest_in_family(models: &[crate::config::Model], family: &str) -> Option<String> {
    let prefix = format!("claude-{family}-");
    let mut best: Option<((u32, u32), String)> = None;
    for m in models {
        let Some(rest) = m.name.strip_prefix(&prefix) else {
            continue;
        };
        let mut parts = rest.split('-');
        let Some(major) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let minor = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        // Skip names with trailing non-numeric segments (e.g. preview tags).
        if parts.next().is_some() {
            continue;
        }
        let key = (major, minor);
        if best.as_ref().is_none_or(|(b, _)| key > *b) {
            best = Some((key, m.name.clone()));
        }
    }
    best.map(|(_, name)| name)
}

impl CommandHandler {
    pub fn new(config: Config) -> Result<Self> {
        // Create a token manager for CLI operations
        let token_manager = TokenManager::new(config.api_key_strings());

        // Use the first provider for CLI commands
        let provider = config
            .providers
            .first()
            .context("At least one provider must be configured")?;

        let client = AiCoreClient::from_provider(provider.clone(), token_manager);
        Ok(Self { client, config })
    }

    /// Get a client for the provider that owns the given resource group.
    /// Falls back to the default client if no match is found.
    fn client_for_resource_group(&self, resource_group: &str) -> AiCoreClient {
        let provider = self
            .config
            .providers
            .iter()
            .find(|p| p.resource_group == resource_group)
            .unwrap_or_else(|| self.config.providers.first().unwrap());

        let token_manager = TokenManager::new(self.config.api_key_strings());
        AiCoreClient::from_provider(provider.clone(), token_manager)
    }

    pub async fn list_resource_groups(&self) -> Result<()> {
        println!("Fetching resource groups...");
        let resource_groups = self.client.list_resource_groups().await?;

        if resource_groups.resources.is_empty() {
            println!("No resource groups found.");
            return Ok(());
        }

        let rows: Vec<Vec<String>> = resource_groups
            .resources
            .iter()
            .map(|rg| {
                vec![
                    rg.resource_group_id.clone(),
                    rg.status.clone(),
                    rg.zone_id.as_deref().unwrap_or("N/A").to_string(),
                    rg.created_at
                        .split('T')
                        .next()
                        .unwrap_or(&rg.created_at)
                        .to_string(),
                ]
            })
            .collect();

        CliTable::new(vec![
            Col {
                header: "RESOURCE GROUP",
                align: Align::Left,
            },
            Col {
                header: "STATUS",
                align: Align::Left,
            },
            Col {
                header: "ZONE ID",
                align: Align::Left,
            },
            Col {
                header: "CREATED AT",
                align: Align::Left,
            },
        ])
        .title(format!("Resource Groups ({} total)", resource_groups.count))
        .rows(rows)
        .print();

        Ok(())
    }

    pub async fn list_deployments(&self, resource_group: Option<&str>) -> Result<()> {
        if let Some(rg_name) = resource_group {
            // Validate that the resource group is configured
            if !self
                .config
                .providers
                .iter()
                .any(|p| p.resource_group == rg_name)
            {
                let available: Vec<&str> = self
                    .config
                    .providers
                    .iter()
                    .map(|p| p.resource_group.as_str())
                    .collect();
                anyhow::bail!(
                    "Resource group '{}' is not configured. Available: {}",
                    rg_name,
                    available.join(", ")
                );
            }
            self.list_deployments_for_resource_group(rg_name).await
        } else {
            // List deployments for all configured resource groups
            for (i, provider) in self.config.providers.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                self.list_deployments_for_resource_group(&provider.resource_group)
                    .await?;
            }
            Ok(())
        }
    }

    async fn list_deployments_for_resource_group(&self, rg_name: &str) -> Result<()> {
        println!("Fetching deployments for resource group '{rg_name}'...");

        let client = self.client_for_resource_group(rg_name);
        let deployments = client.list_deployments(Some(rg_name)).await?;

        if deployments.resources.is_empty() {
            println!("No deployments found in resource group '{rg_name}'.");
            return Ok(());
        }

        let mut rows: Vec<Vec<String>> = deployments
            .resources
            .iter()
            .map(|deployment| {
                let (model_name, model_version) = deployment.get_model_info();
                let model_display = match (model_name, model_version) {
                    (Some(name), Some(version)) => format!("{name}:{version}"),
                    (Some(name), None) => name,
                    _ => "N/A".to_string(),
                };
                vec![
                    deployment.id[..std::cmp::min(deployment.id.len(), 16)].to_string(),
                    deployment.status.clone(),
                    model_display,
                    deployment
                        .configuration_name
                        .as_deref()
                        .unwrap_or("N/A")
                        .to_string(),
                    deployment
                        .start_time
                        .as_deref()
                        .and_then(|t| t.split('T').next())
                        .unwrap_or("N/A")
                        .to_string(),
                ]
            })
            .collect();

        rows.sort_by(|a, b| a[2].cmp(&b[2]));

        CliTable::new(vec![
            Col {
                header: "ID",
                align: Align::Left,
            },
            Col {
                header: "STATUS",
                align: Align::Left,
            },
            Col {
                header: "DEPLOYED MODEL",
                align: Align::Left,
            },
            Col {
                header: "CONFIG MODEL",
                align: Align::Left,
            },
            Col {
                header: "START TIME",
                align: Align::Left,
            },
        ])
        .title(format!("Deployments ({} total)", deployments.count))
        .rows(rows)
        .print();

        Ok(())
    }

    /// Auto-configure Claude Code to use this router as its backend.
    ///
    /// Configures settings.json with:
    /// - Required env vars (auth, base URL, telemetry, model defaults) — always written
    /// - Recommended settings (tool search, git attribution) — only written if absent
    ///
    /// Model defaults are derived from the Claude models actually configured in
    /// `~/.aicore/config.yaml`: for each sub-family (opus / sonnet / haiku) the
    /// newest configured version becomes the corresponding `ANTHROPIC_*_MODEL`
    /// env var. Aborts if no Claude models are configured.
    ///
    /// Also configures .claude.json to skip onboarding.
    pub fn configure_claude_code(&self) -> Result<()> {
        let home = std::env::var("HOME").context("HOME environment variable not set")?;
        let home_path = PathBuf::from(&home);

        // Respect CLAUDE_CONFIG_DIR if set, otherwise default to ~/.claude
        let claude_dir = std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_path.join(".claude"));
        let settings_path = claude_dir.join("settings.json");
        let onboarding_path = home_path.join(".claude.json");

        let addr =
            crate::config::parse_bind_address(&self.config.bind).context("Invalid bind address")?;
        let api_key = &self
            .config
            .api_keys
            .first()
            .context("No API keys configured")?
            .key;

        let base_url = format!("http://localhost:{}/v1", addr.port());

        // Derive per-family Claude model env vars from the configured models.
        let claude_models = ClaudeModelChoices::from_models(&self.config.models);
        if claude_models.is_empty() {
            anyhow::bail!(
                "No Claude models configured under `models:` in your aicore config \
                 (typically ~/.aicore/config.yaml). Add at least one entry whose \
                 `name` starts with `claude-opus-`, `claude-sonnet-`, or \
                 `claude-haiku-` (e.g., `claude-opus-4-8`, `claude-sonnet-4-6`, \
                 `claude-haiku-4-5`), then re-run `acr configure claude`."
            );
        }

        // --- Configure settings.json ---
        let settings_modified =
            Self::configure_settings_file(&settings_path, &base_url, api_key, &claude_models)?;

        // --- Configure .claude.json (onboarding) ---
        let onboarding_modified = Self::configure_onboarding_file(&onboarding_path)?;

        if !settings_modified && !onboarding_modified {
            println!("Claude Code settings are already up to date.");
        } else {
            println!(
                "\nClaude Code configured to use AI Core Router at {}",
                base_url
            );
            if settings_modified {
                println!("  Settings: {}", settings_path.display());
            }
            if onboarding_modified {
                println!("  Onboarding: {}", onboarding_path.display());
            }
        }

        Ok(())
    }

    /// Configure ~/.claude/settings.json with required and recommended fields.
    fn configure_settings_file(
        settings_path: &PathBuf,
        base_url: &str,
        api_key: &str,
        claude_models: &ClaudeModelChoices,
    ) -> Result<bool> {
        // Read existing settings or start fresh
        let mut settings: serde_json::Value = if settings_path.exists() {
            let content = std::fs::read_to_string(settings_path)
                .with_context(|| format!("Failed to read {}", settings_path.display()))?;
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        // Create timestamped backup if file exists
        if settings_path.exists() {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let backup_path = settings_path.with_extension(format!("json.backup.{timestamp}"));
            std::fs::copy(settings_path, &backup_path)
                .with_context(|| format!("Failed to create backup at {}", backup_path.display()))?;
            println!("Created backup: {}", backup_path.display());
        }

        let obj = settings
            .as_object_mut()
            .context("Settings is not a JSON object")?;

        // Ensure env object exists
        if !obj.contains_key("env") {
            obj.insert("env".to_string(), serde_json::json!({}));
        }

        let env_obj = obj
            .get_mut("env")
            .and_then(|v| v.as_object_mut())
            .context("env is not a JSON object")?;

        // Required env vars — always written
        let mut required_env: Vec<(&str, String)> = vec![
            ("ANTHROPIC_BASE_URL", base_url.to_string()),
            ("ANTHROPIC_AUTH_TOKEN", api_key.to_string()),
            ("DISABLE_TELEMETRY", "1".to_string()),
            ("DISABLE_ERROR_REPORTING", "1".to_string()),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_string()),
            ("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1".to_string()),
        ];
        required_env.extend(claude_models.env_vars());

        // Recommended env vars — only set if absent
        let recommended_env: Vec<(&str, String)> = vec![("ENABLE_TOOL_SEARCH", "true".to_string())];

        let mut modified = false;

        for (key, value) in &required_env {
            let old_value = env_obj.get(*key).and_then(|v| v.as_str()).map(String::from);
            if old_value.as_deref() != Some(value) {
                if let Some(old) = &old_value {
                    println!("  Updated {key}: {old} -> {value}");
                } else {
                    println!("  Set {key}: {value}");
                }
                env_obj.insert(key.to_string(), serde_json::json!(value));
                modified = true;
            }
        }

        for (key, value) in &recommended_env {
            if !env_obj.contains_key(*key) {
                println!("  Set {key}: {value} (recommended)");
                env_obj.insert(key.to_string(), serde_json::json!(value));
                modified = true;
            }
        }

        // Recommended top-level settings — only set if absent
        let obj = settings
            .as_object_mut()
            .context("Settings is not a JSON object")?;

        let recommended_toplevel: Vec<(&str, serde_json::Value)> = vec![
            ("alwaysThinkingEnabled", serde_json::json!(false)),
            ("gitAttribution", serde_json::json!(false)),
            ("includeCoAuthoredBy", serde_json::json!(false)),
        ];

        for (key, value) in &recommended_toplevel {
            if !obj.contains_key(*key) {
                println!("  Set {key}: {value} (recommended)");
                obj.insert(key.to_string(), value.clone());
                modified = true;
            }
        }

        if !modified {
            return Ok(false);
        }

        // Ensure parent directory exists
        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write updated settings with restrictive permissions (contains API keys)
        let content = serde_json::to_string_pretty(&settings)?;
        std::fs::write(settings_path, &content)
            .with_context(|| format!("Failed to write {}", settings_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(settings_path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| {
                    format!("Failed to set permissions on {}", settings_path.display())
                })?;
        }

        Ok(true)
    }

    /// Configure ~/.claude.json to skip onboarding.
    fn configure_onboarding_file(onboarding_path: &PathBuf) -> Result<bool> {
        let mut onboarding: serde_json::Value = if onboarding_path.exists() {
            let content = std::fs::read_to_string(onboarding_path)
                .with_context(|| format!("Failed to read {}", onboarding_path.display()))?;
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        let obj = onboarding
            .as_object_mut()
            .context("Onboarding file is not a JSON object")?;

        // Only set if not already true
        if obj.get("hasCompletedOnboarding").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(false);
        }

        obj.insert(
            "hasCompletedOnboarding".to_string(),
            serde_json::json!(true),
        );

        let content = serde_json::to_string_pretty(&onboarding)?;
        std::fs::write(onboarding_path, &content)
            .with_context(|| format!("Failed to write {}", onboarding_path.display()))?;

        println!("  Set hasCompletedOnboarding: true");
        Ok(true)
    }

    /// Auto-configure OpenCode to use this router as its backend.
    ///
    /// Writes opencode.jsonc with providers for Anthropic, OpenAI, and Gemini
    /// all pointing at this router's endpoints.
    pub fn configure_opencode(&self) -> Result<()> {
        let addr =
            crate::config::parse_bind_address(&self.config.bind).context("Invalid bind address")?;
        let api_key = &self
            .config
            .api_keys
            .first()
            .context("No API keys configured")?
            .key;

        // Resolve config path: OPENCODE_CONFIG env var → default
        let config_path = std::env::var("OPENCODE_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(home)
                    .join(".config")
                    .join("opencode")
                    .join("opencode.jsonc")
            });

        let base_url = format!("http://localhost:{}", addr.port());

        // Read existing config or start fresh
        let mut config: serde_json::Value = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)
                .with_context(|| format!("Failed to read {}", config_path.display()))?;
            // Strip comments from JSONC
            let stripped = Self::strip_jsonc_comments(&content);
            serde_json::from_str(&stripped).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        // Create timestamped backup if file exists
        if config_path.exists() {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let backup_path = config_path.with_extension(format!("jsonc.backup.{timestamp}"));
            std::fs::copy(&config_path, &backup_path)
                .with_context(|| format!("Failed to create backup at {}", backup_path.display()))?;
            println!("Created backup: {}", backup_path.display());
        }

        let obj = config
            .as_object_mut()
            .context("Config is not a JSON object")?;

        // Set required fields
        obj.insert(
            "$schema".to_string(),
            serde_json::json!("https://opencode.ai/config.json"),
        );
        obj.insert(
            "model".to_string(),
            serde_json::json!("anthropic/claude-sonnet-latest"),
        );
        obj.insert("share".to_string(), serde_json::json!("disabled"));
        obj.insert(
            "enabled_providers".to_string(),
            serde_json::json!(["anthropic", "openai", "google"]),
        );
        obj.insert(
            "provider".to_string(),
            serde_json::json!({
                "anthropic": {
                    "name": "ACR Anthropic",
                    "options": {
                        "baseURL": format!("{base_url}/anthropic/v1"),
                        "apiKey": api_key
                    }
                },
                "openai": {
                    "name": "ACR OpenAI",
                    "options": {
                        "baseURL": format!("{base_url}/v1"),
                        "apiKey": api_key
                    }
                },
                "google": {
                    "name": "ACR Gemini",
                    "options": {
                        "baseURL": format!("{base_url}/gemini/v1beta"),
                        "apiKey": api_key
                    }
                }
            }),
        );

        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(&config)?;
        std::fs::write(&config_path, &content)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| {
                    format!("Failed to set permissions on {}", config_path.display())
                })?;
        }

        println!("\nOpenCode configured to use AI Core Router at {base_url}");
        println!("  Config: {}", config_path.display());
        println!("  Providers:");
        println!("    anthropic -> {base_url}/anthropic/v1");
        println!("    openai    -> {base_url}/v1");
        println!("    gemini    -> {base_url}/gemini/v1beta");

        Ok(())
    }

    /// Strip single-line (//) and block (/* */) comments from JSONC content.
    fn strip_jsonc_comments(input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        let mut in_string = false;

        while let Some(&ch) = chars.peek() {
            if in_string {
                result.push(ch);
                chars.next();
                if ch == '\\' {
                    if let Some(&next) = chars.peek() {
                        result.push(next);
                        chars.next();
                    }
                } else if ch == '"' {
                    in_string = false;
                }
            } else if ch == '"' {
                in_string = true;
                result.push(ch);
                chars.next();
            } else if ch == '/' {
                chars.next();
                match chars.peek() {
                    Some(&'/') => {
                        // Single-line comment: skip until newline
                        while let Some(&c) = chars.peek() {
                            if c == '\n' {
                                break;
                            }
                            chars.next();
                        }
                    }
                    Some(&'*') => {
                        // Block comment: skip until */
                        chars.next();
                        loop {
                            match chars.next() {
                                Some('*') if chars.peek() == Some(&'/') => {
                                    chars.next();
                                    break;
                                }
                                None => break,
                                _ => {}
                            }
                        }
                    }
                    _ => {
                        result.push('/');
                    }
                }
            } else {
                result.push(ch);
                chars.next();
            }
        }

        result
    }

    /// Show token usage statistics from the request database.
    #[cfg(feature = "db")]
    pub async fn usage(
        &self,
        api_key: Option<&str>,
        daily: Option<u32>,
        weekly: Option<u32>,
        monthly: Option<u32>,
        show_cost: bool,
    ) -> Result<()> {
        use crate::database::Database;
        use crate::quota::hash_api_key;
        use chrono::Local;

        // Resolve database path
        let db_path = &self.config.log_requests.db_path;

        if !std::path::Path::new(db_path).exists() {
            return Err(anyhow::anyhow!(
                "Database not found: {}. Enable log_requests in config.",
                db_path
            ));
        }

        let db = Database::open_readonly(db_path)?;

        // Resolve api_key_hash filter
        let key_hash_filter = api_key.map(hash_api_key);

        let now = Local::now();
        let today = now.date_naive();

        // Format a local date as a naive datetime string for DB queries.
        // SQLite's datetime(?1, 'utc') handles local→UTC conversion natively.
        let to_local_since = |date: chrono::NaiveDate| -> String { format!("{} 00:00:00", date) };

        let key_label = api_key.map(|k| format!(" (key: {k})")).unwrap_or_default();

        if let Some(n) = daily {
            let since = to_local_since(today - chrono::Duration::days(n as i64));
            let rows = db
                .query_usage(
                    key_hash_filter.as_deref(),
                    &since,
                    crate::database::GroupBy::Day,
                )
                .await?;
            let rows = Self::aggregate_by_period_and_model(&rows);
            let title = format!("Token Usage \u{2014} Past {} Days{}", n, key_label);
            Self::print_usage_table(&rows, true, show_cost, &self.config, &title);
        } else if let Some(n) = weekly {
            let since = to_local_since(today - chrono::Duration::weeks(n as i64));
            let rows = db
                .query_usage(
                    key_hash_filter.as_deref(),
                    &since,
                    crate::database::GroupBy::Week,
                )
                .await?;
            let rows = Self::aggregate_by_period_and_model(&rows);
            let title = format!("Token Usage \u{2014} Past {} Weeks{}", n, key_label);
            Self::print_usage_table(&rows, true, show_cost, &self.config, &title);
        } else if let Some(n) = monthly {
            let since_date = today
                .checked_sub_months(chrono::Months::new(n))
                .map(crate::quota::start_of_month)
                .unwrap_or(today);
            let since = to_local_since(since_date);
            let rows = db
                .query_usage(
                    key_hash_filter.as_deref(),
                    &since,
                    crate::database::GroupBy::Month,
                )
                .await?;
            let rows = Self::aggregate_by_period_and_model(&rows);
            let title = format!("Token Usage \u{2014} Past {} Months{}", n, key_label);
            Self::print_usage_table(&rows, true, show_cost, &self.config, &title);
        } else {
            let today_str = to_local_since(today);
            let rows = db
                .query_usage(
                    key_hash_filter.as_deref(),
                    &today_str,
                    crate::database::GroupBy::Day,
                )
                .await?;

            // Aggregate by model (collapse per-key breakdown)
            let aggregated = Self::aggregate_by_period_and_model(&rows);
            let title = format!("Token Usage \u{2014} Today{}", key_label);
            Self::print_usage_table(&aggregated, false, show_cost, &self.config, &title);

            if show_cost {
                Self::print_partial_warnings(&Self::collect_partial_models(
                    &aggregated,
                    &self.config,
                ));
            }
        }

        Ok(())
    }

    /// Print a usage table. If `show_period` is true, includes a "Period" column.
    #[cfg(feature = "db")]
    fn print_usage_table(
        rows: &[crate::database::UsageRow],
        show_period: bool,
        show_cost: bool,
        config: &Config,
        title: &str,
    ) {
        if rows.is_empty() {
            println!("\n{title}");
            println!("No usage data found.");
            return;
        }

        // Build column definitions conditionally
        let mut columns: Vec<Col> = Vec::new();
        if show_period {
            columns.push(Col {
                header: "Period",
                align: Align::Left,
            });
        }
        columns.push(Col {
            header: "Model",
            align: Align::Left,
        });
        columns.push(Col {
            header: "Input",
            align: Align::Right,
        });
        columns.push(Col {
            header: "Output",
            align: Align::Right,
        });
        columns.push(Col {
            header: "Cache R",
            align: Align::Right,
        });
        columns.push(Col {
            header: "Cache W",
            align: Align::Right,
        });
        columns.push(Col {
            header: "Total",
            align: Align::Right,
        });
        if show_cost {
            columns.push(Col {
                header: "Est. Cost",
                align: Align::Right,
            });
        }
        columns.push(Col {
            header: "Reqs",
            align: Align::Right,
        });

        // Build data rows and accumulate totals
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut total_cache_read = 0u64;
        let mut total_cache_write = 0u64;
        let mut total_reqs = 0u64;
        let mut total_cost = 0.0f64;
        let mut partial_models: Vec<String> = Vec::new();
        let mut data_rows: Vec<Vec<String>> = Vec::new();

        for row in rows {
            let total = row.input_tokens
                + row.output_tokens
                + row.cache_read_tokens
                + row.cache_write_tokens;
            total_input += row.input_tokens;
            total_output += row.output_tokens;
            total_cache_read += row.cache_read_tokens;
            total_cache_write += row.cache_write_tokens;
            total_reqs += row.request_count;

            let cost_str = if show_cost {
                let tokens = crate::metrics::TokenCounts {
                    input: row.input_tokens,
                    output: row.output_tokens,
                    cache_read: row.cache_read_tokens,
                    cache_write: row.cache_write_tokens,
                };
                Self::format_cost_cell(
                    &row.model,
                    &tokens,
                    config,
                    &mut total_cost,
                    &mut partial_models,
                )
            } else {
                String::new()
            };

            let mut cells = Vec::new();
            if show_period {
                cells.push(row.period.clone());
            }
            cells.push(row.model.clone());
            cells.push(format_number(row.input_tokens));
            cells.push(format_number(row.output_tokens));
            cells.push(format_number(row.cache_read_tokens));
            cells.push(format_number(row.cache_write_tokens));
            cells.push(format_number(total));
            if show_cost {
                cells.push(cost_str);
            }
            cells.push(row.request_count.to_string());

            data_rows.push(cells);
        }

        // Build total row
        let grand_total = total_input + total_output + total_cache_read + total_cache_write;
        let mut total_cells = Vec::new();
        if show_period {
            total_cells.push(String::new());
        }
        total_cells.push("Total".to_string());
        total_cells.push(format_number(total_input));
        total_cells.push(format_number(total_output));
        total_cells.push(format_number(total_cache_read));
        total_cells.push(format_number(total_cache_write));
        total_cells.push(format_number(grand_total));
        if show_cost {
            total_cells.push(crate::format_cost_value(total_cost));
        }
        total_cells.push(total_reqs.to_string());

        CliTable::new(columns)
            .title(title)
            .rows(data_rows)
            .total_row(total_cells)
            .print();

        if show_cost {
            Self::print_partial_warnings(&partial_models);
        }
    }

    /// Collect model names with incomplete pricing data.
    #[cfg(feature = "db")]
    fn collect_partial_models(rows: &[crate::database::UsageRow], config: &Config) -> Vec<String> {
        let mut partial = Vec::new();
        for row in rows {
            let tokens = crate::metrics::TokenCounts {
                input: row.input_tokens,
                output: row.output_tokens,
                cache_read: row.cache_read_tokens,
                cache_write: row.cache_write_tokens,
            };
            if let Some(pricing) = config.get_model_pricing(&row.model)
                && pricing.is_partial(&tokens)
                && !partial.contains(&row.model.to_string())
            {
                partial.push(row.model.to_string());
            }
        }
        partial
    }

    /// Format cost for a single row. Returns "N/A" if no pricing, value with "*" if partial.
    /// Accumulates into total_cost and tracks partial models.
    #[cfg(feature = "db")]
    fn format_cost_cell(
        model: &str,
        tokens: &crate::metrics::TokenCounts,
        config: &Config,
        total_cost: &mut f64,
        partial_models: &mut Vec<String>,
    ) -> String {
        match config.get_model_pricing(model) {
            None => "N/A".to_string(),
            Some(pricing) => {
                let cost = pricing.calculate_cost(tokens);
                *total_cost += cost;
                let is_partial = pricing.is_partial(tokens);
                if is_partial && !partial_models.contains(&model.to_string()) {
                    partial_models.push(model.to_string());
                }
                let formatted = crate::format_cost_value(cost);
                if is_partial {
                    format!("{}*", formatted)
                } else {
                    formatted
                }
            }
        }
    }

    /// Print warnings for models with incomplete pricing.
    #[cfg(feature = "db")]
    fn print_partial_warnings(partial_models: &[String]) {
        for model in partial_models {
            println!(
                "\nWarning: '{}' pricing is incomplete (missing cache fields) \u{2014} cost may be underestimated",
                model
            );
        }
    }

    /// Aggregate usage rows by (period, model), collapsing per-key breakdown.
    #[cfg(feature = "db")]
    fn aggregate_by_period_and_model(
        rows: &[crate::database::UsageRow],
    ) -> Vec<crate::database::UsageRow> {
        let mut map: std::collections::HashMap<(String, String), crate::database::UsageRow> =
            std::collections::HashMap::new();
        for row in rows {
            let key = (row.period.clone(), row.model.clone());
            let entry = map.entry(key).or_insert_with(|| crate::database::UsageRow {
                api_key_hash: String::new(),
                model: row.model.clone(),
                period: row.period.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                request_count: 0,
            });
            entry.input_tokens += row.input_tokens;
            entry.output_tokens += row.output_tokens;
            entry.cache_read_tokens += row.cache_read_tokens;
            entry.cache_write_tokens += row.cache_write_tokens;
            entry.request_count += row.request_count;
        }
        let mut result: Vec<_> = map.into_values().collect();
        result.sort_by(|a, b| b.period.cmp(&a.period).then_with(|| a.model.cmp(&b.model)));
        result
    }

    /// Delete request logs older than N days.
    #[cfg(feature = "db")]
    pub async fn logs_clean(&self, days: Option<u32>) -> Result<()> {
        use crate::database::Database;

        let retention_days = days.unwrap_or(self.config.log_requests.retention_days);
        let db_path = &self.config.log_requests.db_path;

        if !std::path::Path::new(db_path).exists() {
            println!("No database found at: {db_path}");
            return Ok(());
        }

        let db = Database::open(db_path.into()).await?;
        let deleted = db.cleanup_old_requests(retention_days).await?;

        if deleted > 0 {
            println!(
                "Deleted {} log entries older than {} days.",
                deleted, retention_days
            );
        } else {
            println!("No log entries older than {} days.", retention_days);
        }

        Ok(())
    }

    /// Print diagnostic information about the router configuration.
    pub fn diagnose(&self, config_path: Option<&str>) -> Result<()> {
        println!("AI Core Router Diagnostics");
        println!("{}", "=".repeat(50));

        // Version and platform
        println!("\nSystem:");
        println!("  Version:    {}", env!("CARGO_PKG_VERSION"));
        println!(
            "  Platform:   {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let config_display = config_path.unwrap_or("~/.aicore/config.yaml");
        println!("  Config:     {}", config_display);

        // Server config
        println!("\nServer:");
        println!("  Bind:       {}", self.config.bind);
        println!("  Log Level:  {}", self.config.log_level);
        println!("  Refresh:    {}s", self.config.refresh_interval_secs);
        println!("  LB Strategy:{:?}", self.config.load_balancing);

        // Port availability check
        let addr_available = std::net::TcpListener::bind(&*self.config.bind).is_ok();
        println!(
            "  Bind Status:{}",
            if addr_available {
                " available"
            } else {
                " in use"
            }
        );

        // Providers
        println!("\nProviders ({}):", self.config.providers.len());
        for p in &self.config.providers {
            println!(
                "  {} - {} (rg: {}, weight: {}, enabled: {})",
                p.name, p.genai_api_url, p.resource_group, p.weight, p.enabled
            );
        }

        // Models
        println!("\nModels ({}):", self.config.models.len());
        for m in &self.config.models {
            let aicore_name = m.aicore_model_name.as_deref().unwrap_or("(same as name)");
            let aliases = if m.aliases.is_empty() {
                String::new()
            } else {
                format!(" aliases: [{}]", m.aliases.join(", "))
            };
            println!("  {} -> {}{}", m.name, aicore_name, aliases);
        }

        // API keys
        println!("\nAPI Keys:   {} configured", self.config.api_keys.len());

        // Fallback models
        println!("\nFallback Models:");
        println!(
            "  Claude:  {}",
            self.config
                .fallback_models
                .claude
                .as_deref()
                .unwrap_or("(none)")
        );
        println!(
            "  OpenAI:  {}",
            self.config
                .fallback_models
                .openai
                .as_deref()
                .unwrap_or("(none)")
        );
        println!(
            "  Gemini:  {}",
            self.config
                .fallback_models
                .gemini
                .as_deref()
                .unwrap_or("(none)")
        );

        // Database
        println!("\nRequest Logging:");
        if self.config.log_requests.enabled {
            let db_path = &self.config.log_requests.db_path;
            let exists = std::path::Path::new(db_path).exists();
            println!("  Enabled:    true");
            println!("  Path:       {}", db_path);
            println!(
                "  Status:     {}",
                if exists { "exists" } else { "not created yet" }
            );
            println!(
                "  Retention:  {} days",
                self.config.log_requests.retention_days
            );
        } else {
            println!("  Enabled:    false");
        }

        println!("\n{}", "=".repeat(50));
        println!("Diagnostics complete.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeModelChoices, CommandHandler, pick_newest_in_family};
    use crate::config::Model;
    use tempfile::TempDir;

    fn make_model(name: &str) -> Model {
        Model {
            name: name.to_string(),
            aicore_model_name: None,
            aliases: Vec::new(),
            pricing: None,
        }
    }

    fn opus_sonnet_haiku_choices() -> ClaudeModelChoices {
        ClaudeModelChoices {
            opus: Some("claude-opus-4-8".to_string()),
            sonnet: Some("claude-sonnet-4-6".to_string()),
            haiku: Some("claude-haiku-4-5".to_string()),
        }
    }

    #[test]
    fn pick_newest_in_family_picks_highest_minor() {
        let models = vec![
            make_model("claude-opus-4-6"),
            make_model("claude-opus-4-8"),
            make_model("claude-opus-4-7"),
            make_model("claude-sonnet-4-6"),
        ];
        assert_eq!(
            pick_newest_in_family(&models, "opus"),
            Some("claude-opus-4-8".to_string())
        );
        assert_eq!(
            pick_newest_in_family(&models, "sonnet"),
            Some("claude-sonnet-4-6".to_string())
        );
        assert_eq!(pick_newest_in_family(&models, "haiku"), None);
    }

    #[test]
    fn pick_newest_in_family_uses_numeric_not_lexicographic_ordering() {
        // Lexicographically "4-10" < "4-2"; numerically "4-10" > "4-2".
        let models = vec![
            make_model("claude-opus-4-2"),
            make_model("claude-opus-4-10"),
        ];
        assert_eq!(
            pick_newest_in_family(&models, "opus"),
            Some("claude-opus-4-10".to_string())
        );
    }

    #[test]
    fn pick_newest_in_family_skips_unparseable_and_preview_suffixes() {
        let models = vec![
            make_model("claude-opus"),
            make_model("claude-opus-4"),
            make_model("claude-opus-4-8-preview"),
            make_model("claude-opus-4-7"),
        ];
        assert_eq!(
            pick_newest_in_family(&models, "opus"),
            Some("claude-opus-4-7".to_string())
        );
    }

    #[test]
    fn claude_model_choices_is_empty_when_no_models_match() {
        let choices = ClaudeModelChoices::from_models(&[make_model("gpt-5.5")]);
        assert!(choices.is_empty());
    }

    #[test]
    fn claude_model_choices_env_vars_skips_missing_families() {
        let choices = ClaudeModelChoices {
            opus: Some("claude-opus-4-8".to_string()),
            sonnet: None,
            haiku: None,
        };
        let vars: Vec<(&str, String)> = choices.env_vars();
        let keys: Vec<&str> = vars.iter().map(|(k, _)| *k).collect();
        // Only the opus-derived env vars should be present.
        assert!(keys.contains(&"ANTHROPIC_MODEL"));
        assert!(keys.contains(&"ANTHROPIC_DEFAULT_OPUS_MODEL"));
        assert!(keys.contains(&"ANTHROPIC_SMALL_FAST_MODEL"));
        assert!(!keys.contains(&"ANTHROPIC_DEFAULT_SONNET_MODEL"));
        assert!(!keys.contains(&"ANTHROPIC_DEFAULT_HAIKU_MODEL"));
        // The primary and small-fast vars fall back to opus when no haiku/sonnet.
        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(map["ANTHROPIC_MODEL"], "claude-opus-4-8");
        assert_eq!(map["ANTHROPIC_SMALL_FAST_MODEL"], "claude-opus-4-8");
    }

    #[test]
    fn claude_model_choices_env_vars_prefers_haiku_for_small_fast() {
        let choices = opus_sonnet_haiku_choices();
        let map: std::collections::HashMap<_, _> = choices.env_vars().into_iter().collect();
        assert_eq!(map["ANTHROPIC_MODEL"], "claude-opus-4-8");
        assert_eq!(map["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus-4-8");
        assert_eq!(map["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet-4-6");
        assert_eq!(map["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku-4-5");
        // SMALL_FAST prefers haiku.
        assert_eq!(map["ANTHROPIC_SMALL_FAST_MODEL"], "claude-haiku-4-5");
    }

    #[test]
    fn test_configure_settings_file_creates_all_required_fields() {
        let dir = TempDir::new().unwrap();
        let settings_path = dir.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

        let choices = opus_sonnet_haiku_choices();
        let modified = CommandHandler::configure_settings_file(
            &settings_path,
            "http://localhost:8900/v1",
            "test-key",
            &choices,
        )
        .unwrap();

        assert!(modified);

        let content = std::fs::read_to_string(&settings_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        // Required env vars
        assert_eq!(
            parsed["env"]["ANTHROPIC_BASE_URL"],
            "http://localhost:8900/v1"
        );
        assert_eq!(parsed["env"]["ANTHROPIC_AUTH_TOKEN"], "test-key");
        assert_eq!(parsed["env"]["DISABLE_TELEMETRY"], "1");
        assert_eq!(parsed["env"]["DISABLE_ERROR_REPORTING"], "1");
        assert_eq!(
            parsed["env"]["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"],
            "1"
        );
        assert_eq!(parsed["env"]["CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"], "1");
        assert_eq!(parsed["env"]["ANTHROPIC_MODEL"], "claude-opus-4-8");
        assert_eq!(
            parsed["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "claude-sonnet-4-6"
        );
        assert_eq!(
            parsed["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
            "claude-haiku-4-5"
        );
        assert_eq!(
            parsed["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            "claude-opus-4-8"
        );
        assert_eq!(
            parsed["env"]["ANTHROPIC_SMALL_FAST_MODEL"],
            "claude-haiku-4-5"
        );

        // Recommended env vars
        assert_eq!(parsed["env"]["ENABLE_TOOL_SEARCH"], "true");

        // Recommended top-level settings
        assert_eq!(parsed["gitAttribution"], false);
        assert_eq!(parsed["includeCoAuthoredBy"], false);
    }

    #[test]
    fn test_configure_settings_preserves_existing_recommended_fields() {
        let dir = TempDir::new().unwrap();
        let settings_path = dir.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

        // Pre-populate with user-customized recommended fields
        let existing = serde_json::json!({
            "env": {
                "ENABLE_TOOL_SEARCH": "disabled"
            },
            "gitAttribution": true,
            "includeCoAuthoredBy": true,
            "customUserSetting": "preserved"
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        CommandHandler::configure_settings_file(
            &settings_path,
            "http://localhost:8900/v1",
            "test-key",
            &opus_sonnet_haiku_choices(),
        )
        .unwrap();

        let content = std::fs::read_to_string(&settings_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        // Recommended fields should NOT be overwritten
        assert_eq!(parsed["env"]["ENABLE_TOOL_SEARCH"], "disabled");
        assert_eq!(parsed["gitAttribution"], true);
        assert_eq!(parsed["includeCoAuthoredBy"], true);

        // Custom user fields should be preserved
        assert_eq!(parsed["customUserSetting"], "preserved");

        // Required fields should still be set
        assert_eq!(
            parsed["env"]["ANTHROPIC_BASE_URL"],
            "http://localhost:8900/v1"
        );
    }

    #[test]
    fn test_configure_onboarding_file() {
        let dir = TempDir::new().unwrap();
        let onboarding_path = dir.path().join(".claude.json");

        let modified = CommandHandler::configure_onboarding_file(&onboarding_path).unwrap();
        assert!(modified);

        let content = std::fs::read_to_string(&onboarding_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["hasCompletedOnboarding"], true);

        // Second call should be no-op
        let modified = CommandHandler::configure_onboarding_file(&onboarding_path).unwrap();
        assert!(!modified);
    }

    #[test]
    fn test_configure_opencode() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("opencode").join("opencode.jsonc");

        // Set env to use our temp path
        unsafe { std::env::set_var("OPENCODE_CONFIG", config_path.to_str().unwrap()) };

        let config = crate::config::Config {
            providers: vec![crate::config::Provider {
                name: "test".to_string(),
                uaa_token_url: "https://test.com/oauth/token".to_string(),
                uaa_client_id: "client".to_string(),
                uaa_client_secret: "secret".to_string(),
                genai_api_url: "https://api.test.com".to_string(),
                resource_group: "default".to_string(),
                weight: 1,
                enabled: true,
            }],
            api_keys: vec![crate::config::ApiKeyConfig {
                key: "test-key".to_string(),
                daily_token_limit: None,
                monthly_token_limit: None,
                requests_per_minute: None,
            }],
            bind: "127.0.0.1:8900".to_string(),
            models: vec![],
            log_level: "info".to_string(),
            refresh_interval_secs: 300,
            fallback_models: crate::config::FallbackModels::default(),
            load_balancing: crate::config::LoadBalancingStrategy::default(),
            log_requests: crate::config::LogRequestsConfig::default(),
            openai_api_version: crate::constants::api::DEFAULT_API_VERSION.to_string(),
            quotas: crate::config::QuotaConfig::default(),
        };

        let handler = CommandHandler::new(config).unwrap();
        handler.configure_opencode().unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(parsed["$schema"], "https://opencode.ai/config.json");
        assert_eq!(parsed["model"], "anthropic/claude-sonnet-latest");
        assert_eq!(parsed["share"], "disabled");
        assert_eq!(
            parsed["enabled_providers"],
            serde_json::json!(["anthropic", "openai", "google"])
        );
        assert_eq!(
            parsed["provider"]["anthropic"]["options"]["baseURL"],
            "http://localhost:8900/anthropic/v1"
        );
        assert_eq!(
            parsed["provider"]["openai"]["options"]["baseURL"],
            "http://localhost:8900/v1"
        );
        assert_eq!(
            parsed["provider"]["google"]["options"]["baseURL"],
            "http://localhost:8900/gemini/v1beta"
        );
        assert_eq!(
            parsed["provider"]["anthropic"]["options"]["apiKey"],
            "test-key"
        );

        // Clean up env
        unsafe { std::env::remove_var("OPENCODE_CONFIG") };
    }

    #[test]
    fn test_strip_jsonc_comments() {
        let input = r#"{
  // This is a comment
  "key": "value", // inline comment
  /* block
     comment */
  "url": "http://example.com/path"
}"#;
        let stripped = CommandHandler::strip_jsonc_comments(input);
        let parsed: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(parsed["key"], "value");
        assert_eq!(parsed["url"], "http://example.com/path");
    }
}
