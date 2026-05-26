//! Model registry that tracks deployments across multiple providers.

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::client::AiCoreClient;
use crate::config::{FallbackModels, Model, Provider};
use crate::token::TokenManager;

/// Resolved deployment information including which provider hosts it
#[derive(Debug, Clone)]
struct ResolvedDeployment {
    deployment_id: String,
    provider_name: String,
}

/// Runtime model registry that manages resolved deployment IDs across multiple providers
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    /// Resolved model name to deployment info mappings (model -> list of providers that have it)
    resolved_models: Arc<RwLock<HashMap<String, Vec<ResolvedDeployment>>>>,
    /// Original model configurations from config file
    config_models: Vec<Model>,
    /// Fallback models configuration for each family
    fallback_models: FallbackModels,
    /// Providers to query for deployments
    providers: Vec<Provider>,
    /// Token manager for authentication
    token_manager: TokenManager,
    /// Refresh interval for background updates
    refresh_interval: Duration,
}

impl ModelRegistry {
    /// Create a new model registry
    pub fn new(
        config_models: Vec<Model>,
        fallback_models: FallbackModels,
        providers: Vec<Provider>,
        token_manager: TokenManager,
        refresh_interval_secs: u64,
    ) -> Self {
        Self {
            resolved_models: Arc::new(RwLock::new(HashMap::new())),
            config_models,
            fallback_models,
            providers,
            token_manager,
            refresh_interval: Duration::from_secs(refresh_interval_secs),
        }
    }

    /// Start the registry: validate config, do an initial deployment fetch,
    /// then spawn the background refresh task.
    ///
    /// Returns the `JoinHandle` of the background task so the caller can
    /// observe panics or coordinate shutdown. If the handle is dropped, the
    /// task continues running (Tokio JoinHandle drop does not abort);
    /// callers wanting graceful shutdown should call `.abort()` on it.
    pub async fn start(&self) -> Result<JoinHandle<()>> {
        // Validate fallback models configuration — fail fast on misconfig
        // rather than letting bad fallbacks surface as confusing per-request
        // errors at runtime.
        self.validate_fallback_models()?;

        // Initial resolution
        self.refresh_deployments().await?;

        // Start background refresh task
        let registry = self.clone();
        let handle = tokio::spawn(async move {
            registry.background_refresh().await;
        });

        Ok(handle)
    }

    /// Validate that configured fallback models exist in the models list.
    /// Returns an error listing every misconfigured family so users see all
    /// problems in one shot rather than fixing them one at a time.
    fn validate_fallback_models(&self) -> Result<()> {
        let model_names: Vec<&str> = self.config_models.iter().map(|m| m.name.as_str()).collect();

        let missing: Vec<String> = self
            .fallback_models
            .iter()
            .filter(|(_, fallback)| !model_names.contains(fallback))
            .map(|(family, fallback)| format!("{family} -> '{fallback}'"))
            .collect();

        if !missing.is_empty() {
            return Err(anyhow!(
                "fallback model(s) not configured in models list: {}",
                missing.join(", ")
            ));
        }
        Ok(())
    }

    /// Get deployment info for a model on a specific provider
    pub async fn get_deployment_for_provider(
        &self,
        model_name: &str,
        provider_name: &str,
    ) -> Option<String> {
        let resolved = self.resolved_models.read().await;
        resolved.get(model_name).and_then(|deployments| {
            deployments
                .iter()
                .find(|d| d.provider_name == provider_name)
                .map(|d| d.deployment_id.clone())
        })
    }

    /// Get all available (resolved) model names
    pub async fn get_available_models(&self) -> Vec<String> {
        let mut models: Vec<String> = {
            let resolved = self.resolved_models.read().await;
            resolved.keys().cloned().collect()
        };
        models.sort();
        models
    }

    /// Non-blocking count of resolved models for synchronous contexts (e.g. TUI rendering).
    /// Returns `None` if the lock is contended (e.g. during a refresh).
    pub fn resolved_model_count_sync(&self) -> Option<usize> {
        self.resolved_models.try_read().ok().map(|m| m.len())
    }

    /// Find model configuration by name
    pub fn find_model_config(&self, model_name: &str) -> Option<&Model> {
        self.config_models.iter().find(|m| m.name == model_name)
    }

    /// Get fallback model for a given model prefix/family
    pub fn get_fallback_model(&self, prefix: &str) -> Option<&str> {
        use crate::constants::models::*;
        match prefix {
            CLAUDE_PREFIX => self.fallback_models.claude.as_deref(),
            GPT_PREFIX | TEXT_PREFIX => self.fallback_models.openai.as_deref(),
            GEMINI_PREFIX => self.fallback_models.gemini.as_deref(),
            _ => None,
        }
    }

    /// Find a model config by checking aliases with glob pattern matching.
    /// Returns the model with the most specific matching pattern.
    /// Specificity is determined by the length of the literal prefix before the `*`.
    pub fn find_model_by_alias(&self, requested_model: &str) -> Option<&Model> {
        let mut best_match: Option<(&Model, usize)> = None;

        for model in &self.config_models {
            for alias in &model.aliases {
                if let Some(specificity) = glob_matches(alias, requested_model) {
                    match &best_match {
                        None => {
                            best_match = Some((model, specificity));
                        }
                        Some((_, current_specificity)) if specificity > *current_specificity => {
                            best_match = Some((model, specificity));
                        }
                        _ => {} // Keep current best match
                    }
                }
            }
        }

        best_match.map(|(model, _)| model)
    }

    async fn background_refresh(&self) {
        let mut interval = tokio::time::interval(self.refresh_interval);

        // Skip the first tick since we already did initial refresh
        interval.tick().await;

        loop {
            interval.tick().await;
            if let Err(e) = self.refresh_deployments().await {
                error!("Failed to refresh deployments: {}", e);
            }
        }
    }

    async fn refresh_deployments(&self) -> Result<()> {
        info!(
            "Refreshing deployment mappings for {} providers...",
            self.providers.len()
        );

        let mut all_resolved: HashMap<String, Vec<ResolvedDeployment>> = HashMap::new();

        // Collect rows for the summary table: (provider, deployment_id, status, deployed_model, config_model)
        let mut table_rows: Vec<(String, String, String, String, String)> = Vec::new();

        // Query each provider for deployments
        for provider in &self.providers {
            if !provider.enabled {
                continue;
            }

            // Create a client for this provider
            let client = AiCoreClient::from_provider(provider.clone(), self.token_manager.clone());

            match client
                .list_deployments(Some(&provider.resource_group))
                .await
            {
                Ok(deployments) => {
                    // Build mapping from aicore model name -> (deployment_id, status)
                    let mut aicore_map: HashMap<String, (String, String)> = HashMap::new();
                    for deployment in &deployments.resources {
                        if let Some(model_name) = deployment.get_aicore_model_name() {
                            aicore_map.insert(
                                model_name,
                                (deployment.id.clone(), deployment.status.clone()),
                            );
                        }
                    }

                    // Log all deployments from this provider
                    for deployment in &deployments.resources {
                        let deployed_model = deployment
                            .get_aicore_model_name()
                            .unwrap_or_else(|| "N/A".to_string());
                        // Find matching config model
                        let config_model = self
                            .config_models
                            .iter()
                            .find(|m| {
                                let aicore_name = m.aicore_model_name.as_ref().unwrap_or(&m.name);
                                aicore_name == &deployed_model
                            })
                            .map(|m| m.name.clone())
                            .unwrap_or_else(|| "-".to_string());

                        table_rows.push((
                            provider.name.clone(),
                            deployment.id.clone(),
                            deployment.status.clone(),
                            deployed_model,
                            config_model,
                        ));
                    }

                    // Resolve config models to deployments
                    for model_config in &self.config_models {
                        let aicore_model_name = model_config
                            .aicore_model_name
                            .as_ref()
                            .unwrap_or(&model_config.name);

                        if let Some((deployment_id, status)) = aicore_map.get(aicore_model_name)
                            && status == crate::constants::deployment::RUNNING_STATUS
                        {
                            all_resolved
                                .entry(model_config.name.clone())
                                .or_default()
                                .push(ResolvedDeployment {
                                    deployment_id: deployment_id.clone(),
                                    provider_name: provider.name.clone(),
                                });
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "Failed to query provider '{}': {}. Skipping this provider.",
                        provider.name, e
                    );
                }
            }
        }

        // Log the summary table
        use crate::table::{Align, CliTable, Col};

        table_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.4.cmp(&b.4)));

        let rows: Vec<Vec<String>> = table_rows
            .iter()
            .map(|(provider, id, status, deployed, config)| {
                vec![
                    provider.clone(),
                    id.clone(),
                    status.clone(),
                    deployed.clone(),
                    config.clone(),
                ]
            })
            .collect();

        let table = CliTable::new(vec![
            Col {
                header: "PROVIDER",
                align: Align::Left,
            },
            Col {
                header: "DEPLOYMENT ID",
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
        ])
        .title("Deployment resolution summary")
        .rows(rows);

        for line in table.render() {
            info!("{}", line);
        }

        let resolved_count = all_resolved.len();
        let total_deployments: usize = all_resolved.values().map(|v| v.len()).sum();

        // Compute unresolved before moving all_resolved
        let unresolved: Vec<&str> = self
            .config_models
            .iter()
            .filter(|m| !all_resolved.contains_key(&m.name))
            .map(|m| m.name.as_str())
            .collect();

        // Update the resolved models
        {
            let mut resolved_models = self.resolved_models.write().await;
            *resolved_models = all_resolved;
        }

        info!(
            "Deployment refresh complete: {} models resolved across {} provider deployments",
            resolved_count, total_deployments
        );

        if resolved_count == 0 {
            error!(
                "No models resolved \u{2014} proxy cannot route requests. Check config and deployments."
            );
        }

        if !unresolved.is_empty() {
            warn!(
                "Unresolved config models (no matching deployment found): {}",
                unresolved.join(", ")
            );

            // Warn specifically about unresolved fallback models
            for (family, fb) in self.fallback_models.iter() {
                if unresolved.contains(&fb) {
                    warn!(
                        "Fallback model '{}' for {} family is unresolved — fallback will not work",
                        fb, family
                    );
                }
            }
        }

        Ok(())
    }
}

/// Check if a glob pattern matches a string. `*` is a wildcard matching any
/// character sequence (including empty) and may appear anywhere in the pattern;
/// all other characters match literally.
///
/// Returns the specificity (length of the literal portion — sum of segment
/// lengths between `*`s) when the pattern matches, `None` otherwise. Higher
/// specificity = more specific match, used to break ties between competing
/// alias patterns.
///
/// Examples:
/// - `claude-*-sonnet` matches `claude-4.6-sonnet` with specificity 14
/// - `*-haiku-*` matches `claude-haiku-4-5` with specificity 7
/// - `claude-*` matches `claude-anything` with specificity 7 (trailing-only is the common case)
/// - `claude-opus-4-7` exact-matches only `claude-opus-4-7` with specificity 15
fn glob_matches(pattern: &str, input: &str) -> Option<usize> {
    // Fast paths first.
    if !pattern.contains('*') {
        return (pattern == input).then_some(pattern.len());
    }
    // Convert glob to regex anchored at both ends; escape regex metacharacters
    // in literal segments so e.g. `claude-4.6-*` doesn't treat `.` as wildcard.
    let mut re = String::with_capacity(pattern.len() + 4);
    re.push('^');
    for (i, segment) in pattern.split('*').enumerate() {
        if i > 0 {
            re.push_str(".*");
        }
        re.push_str(&regex::escape(segment));
    }
    re.push('$');
    let regex = regex::Regex::new(&re).ok()?;
    if regex.is_match(input) {
        // Specificity: total literal length (everything except the `*`s).
        let literal_len: usize = pattern.split('*').map(|s| s.len()).sum();
        Some(literal_len)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_matches_trailing_wildcard() {
        // Prefix match with trailing *
        assert_eq!(glob_matches("claude-*", "claude-sonnet"), Some(7));
        assert_eq!(
            glob_matches("claude-sonnet-4-5-*", "claude-sonnet-4-5-20250929"),
            Some(18)
        );
        assert_eq!(glob_matches("gpt-4o-*", "gpt-4o-mini"), Some(7));

        // Non-matching prefix
        assert_eq!(glob_matches("claude-*", "gpt-4"), None);
        assert_eq!(
            glob_matches("claude-sonnet-4-5-*", "claude-sonnet-4-0"),
            None
        );
    }

    #[test]
    fn test_glob_matches_exact() {
        // Exact match (no wildcard)
        assert_eq!(glob_matches("claude-sonnet", "claude-sonnet"), Some(13));
        assert_eq!(glob_matches("gpt-4o", "gpt-4o"), Some(6));

        // Non-matching exact
        assert_eq!(glob_matches("claude-sonnet", "claude-sonnet-4"), None);
        assert_eq!(glob_matches("gpt-4o", "gpt-4o-mini"), None);
    }

    #[test]
    fn test_glob_matches_empty_pattern() {
        // Edge case: wildcard only matches anything
        assert_eq!(glob_matches("*", "anything"), Some(0));
        assert_eq!(glob_matches("*", ""), Some(0));

        // Empty pattern only matches empty string
        assert_eq!(glob_matches("", ""), Some(0));
        assert_eq!(glob_matches("", "something"), None);
    }

    #[test]
    fn test_glob_specificity() {
        // More specific patterns have higher specificity
        let specific = glob_matches("claude-sonnet-4-5-*", "claude-sonnet-4-5-20250929");
        let general = glob_matches("claude-*", "claude-sonnet-4-5-20250929");

        assert!(specific.is_some());
        assert!(general.is_some());
        assert!(specific.unwrap() > general.unwrap());
        assert_eq!(specific.unwrap(), 18);
        assert_eq!(general.unwrap(), 7);
    }

    #[test]
    fn test_glob_matches_mid_string_wildcard() {
        // `*` may now appear anywhere in the pattern
        assert_eq!(
            glob_matches("claude-*-sonnet", "claude-4.6-sonnet"),
            Some(14) // "claude-" (7) + "-sonnet" (7) = 14
        );
        assert!(glob_matches("claude-*-sonnet", "claude-4-sonnet").is_some());
        assert!(glob_matches("claude-*-sonnet", "claude-haiku").is_none());
    }

    #[test]
    fn test_glob_matches_leading_and_trailing_wildcards() {
        // `*-haiku-*` matches anything containing `-haiku-`
        assert_eq!(glob_matches("*-haiku-*", "claude-haiku-4-5"), Some(7));
        assert!(glob_matches("*-haiku-*", "claude-haiku").is_none()); // no trailing dash
        assert!(glob_matches("*haiku*", "claude-haiku-4-5").is_some());
    }

    #[test]
    fn test_glob_matches_escapes_regex_metachars_in_literal_segments() {
        // The `.` in `4.6` must match literally, not as regex `.`
        assert!(glob_matches("claude-4.6-*", "claude-4.6-sonnet").is_some());
        assert!(glob_matches("claude-4.6-*", "claude-4x6-sonnet").is_none());
    }

    fn create_test_registry(models: Vec<Model>) -> ModelRegistry {
        ModelRegistry::new(
            models,
            FallbackModels::default(),
            vec![],
            TokenManager::new(vec!["test".to_string()]),
            600,
        )
    }

    #[test]
    fn test_find_model_by_alias_exact() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec!["claude-4-sonnet".to_string()],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let found = registry.find_model_by_alias("claude-4-sonnet");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "claude-sonnet-4-5");
    }

    #[test]
    fn test_find_model_by_alias_wildcard() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec!["claude-sonnet-4-5-*".to_string()],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let found = registry.find_model_by_alias("claude-sonnet-4-5-20250929");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "claude-sonnet-4-5");
    }

    #[test]
    fn test_find_model_by_alias_most_specific_wins() {
        let models = vec![
            Model {
                name: "claude-general".to_string(),
                aicore_model_name: None,
                aliases: vec!["claude-*".to_string()],
                pricing: None,
            },
            Model {
                name: "claude-sonnet-4-5".to_string(),
                aicore_model_name: None,
                aliases: vec!["claude-sonnet-4-5-*".to_string()],
                pricing: None,
            },
        ];
        let registry = create_test_registry(models);

        // Should match the more specific pattern
        let found = registry.find_model_by_alias("claude-sonnet-4-5-20250929");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "claude-sonnet-4-5");

        // Should match the general pattern for non-specific requests
        let found2 = registry.find_model_by_alias("claude-opus");
        assert!(found2.is_some());
        assert_eq!(found2.unwrap().name, "claude-general");
    }

    #[test]
    fn test_find_model_by_alias_no_match() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec!["claude-sonnet-4-5-*".to_string()],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let found = registry.find_model_by_alias("gpt-4o-mini");
        assert!(found.is_none());
    }

    #[test]
    fn test_find_model_by_alias_multiple_aliases() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec![
                "claude-sonnet-4-5-*".to_string(),
                "claude-4-sonnet".to_string(),
                "sonnet-4.5".to_string(),
            ],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        // Test all aliases match the same model
        assert_eq!(
            registry
                .find_model_by_alias("claude-sonnet-4-5-20250929")
                .map(|m| &m.name),
            Some(&"claude-sonnet-4-5".to_string())
        );
        assert_eq!(
            registry
                .find_model_by_alias("claude-4-sonnet")
                .map(|m| &m.name),
            Some(&"claude-sonnet-4-5".to_string())
        );
        assert_eq!(
            registry.find_model_by_alias("sonnet-4.5").map(|m| &m.name),
            Some(&"claude-sonnet-4-5".to_string())
        );
    }
}
