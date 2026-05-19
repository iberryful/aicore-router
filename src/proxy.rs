use anyhow::{Context, Result};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
};
use futures::stream::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use std::fmt;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::ReceiverStream;

use crate::balancer::LoadBalancer;
use crate::config::{Config, Provider};
use crate::constants::{api::*, models::*};
use crate::metrics::MetricsService;
use crate::registry::ModelRegistry;
use crate::routes::AppError;
use crate::token::TokenManager;

pub fn extract_api_key(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("api-key")
        .or_else(|| headers.get("x-api-key"))
        .or_else(|| headers.get("x-goog-api-key"))
        .and_then(|header_value| header_value.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|auth| auth.to_str().ok())
                .and_then(|auth_str| auth_str.strip_prefix("Bearer ").map(|s| s.to_string()))
        })
}

#[derive(Debug, Default, Clone)]
pub struct TokenStats {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

impl fmt::Display for TokenStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "input_tokens: ")?;
        match self.input_tokens {
            Some(t) => write!(f, "{}", t)?,
            None => write!(f, "N/A")?,
        }
        write!(f, ", output_tokens: ")?;
        match self.output_tokens {
            Some(t) => write!(f, "{}", t)?,
            None => write!(f, "N/A")?,
        }
        write!(f, ", cache_read: ")?;
        match self.cache_read {
            Some(t) => write!(f, "{}", t)?,
            None => write!(f, "N/A")?,
        }
        write!(f, ", cache_write: ")?;
        match self.cache_write {
            Some(t) => write!(f, "{}", t)?,
            None => write!(f, "N/A")?,
        }
        Ok(())
    }
}

impl TokenStats {
    /// Convert optional token stats to concrete counts (defaulting None to 0).
    pub fn to_counts(&self) -> crate::metrics::TokenCounts {
        crate::metrics::TokenCounts {
            input: self.input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
            cache_read: self.cache_read.unwrap_or(0),
            cache_write: self.cache_write.unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LlmFamily {
    OpenAi,
    Claude,
    Gemini,
}

#[derive(Debug)]
pub struct ProxyRequest {
    pub family: LlmFamily,
    pub method: Method,
    pub body: Value,
    pub stream: bool,
    pub url: String,
    pub token: String,
    pub model: String,          // Resolved/normalized model name
    pub original_model: String, // Original requested model name
    pub provider_name: String,  // Provider handling this request
    pub resource_group: String,
    pub anthropic_beta: Vec<String>, // Bedrock-mapped beta features from Anthropic-Beta header
}

/// Input parameters for building a ProxyRequest
#[derive(Debug)]
pub struct ProxyRequestParams<'a> {
    pub headers: &'a HeaderMap,
    pub method: Method,
    pub body: Value,
    pub model: String,
    pub action: Option<String>,
    pub config: &'a Config,
    pub token_manager: &'a TokenManager,
    pub model_registry: &'a ModelRegistry,
    pub load_balancer: &'a LoadBalancer,
}

/// Builder for ProxyRequest with step-by-step validation
pub struct ProxyRequestBuilder<'a> {
    params: ProxyRequestParams<'a>,
}

impl<'a> ProxyRequestBuilder<'a> {
    pub fn new(params: ProxyRequestParams<'a>) -> Self {
        Self { params }
    }

    /// Build a proxy request for a specific provider.
    /// This is used for 429 fallback - try providers in order until one succeeds.
    pub async fn build_for_provider(&self, provider: &Provider) -> Result<ProxyRequest, AppError> {
        // Step 1: Extract and validate API key
        let api_key = self.extract_api_key()?;

        // Step 2: Get authentication token for this provider
        let token = self.get_auth_token(&api_key, provider).await?;

        // Step 3: Resolve model and deployment for this provider
        let (normalized_model, deployment_id, has_extended_context) =
            self.resolve_model_for_provider(provider).await?;

        // Step 4: Determine LLM family and stream flag
        let family = determine_family(&normalized_model);
        let stream = extract_stream_flag(&self.params.body, &family, &self.params.action);

        // Step 5: Prepare request body
        let mut body = self.params.body.clone();
        prepare_body(&mut body, &family, stream, &normalized_model)?;

        // Step 6: Extract Anthropic-Beta header and convert to Bedrock beta features
        let mut anthropic_beta = if matches!(family, LlmFamily::Claude) {
            extract_anthropic_beta(self.params.headers)
        } else {
            vec![]
        };

        // Step 6b: Auto-inject 1M context beta if model was requested with [1m] suffix
        if has_extended_context && matches!(family, LlmFamily::Claude) {
            let feature = CONTEXT_1M_BETA.to_string();
            if !anthropic_beta.contains(&feature) {
                tracing::info!(
                    "Auto-enabling 1M extended context for model '{}'",
                    normalized_model
                );
                anthropic_beta.push(feature);
            }
        }

        // Step 7: For Claude, add anthropic_beta to body if any features were extracted
        if !anthropic_beta.is_empty()
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("anthropic_beta".to_string(), json!(anthropic_beta));
        }

        // Step 8: Build target URL using the provider's API URL
        let url = build_url(
            &normalized_model,
            &deployment_id,
            &self.params.action,
            &provider.genai_api_url,
            &family,
            stream,
            &self.params.config.openai_api_version,
        )?;

        Ok(ProxyRequest {
            family,
            method: self.params.method.clone(),
            body,
            stream,
            url,
            token,
            model: normalized_model,
            original_model: self.params.model.clone(),
            provider_name: provider.name.clone(),
            resource_group: provider.resource_group.clone(),
            anthropic_beta,
        })
    }

    fn extract_api_key(&self) -> Result<String, AppError> {
        extract_api_key(self.params.headers).ok_or(AppError::MissingApiKey)
    }

    async fn get_auth_token(&self, api_key: &str, provider: &Provider) -> Result<String, AppError> {
        self.params
            .token_manager
            .get_token_for_provider(api_key, provider)
            .await
            .map_err(AppError::Internal)?
            .ok_or(AppError::InvalidApiKey)
    }

    /// Resolve model to deployment ID for a specific provider.
    /// Returns (normalized_model, deployment_id, has_extended_context).
    async fn resolve_model_for_provider(
        &self,
        provider: &Provider,
    ) -> Result<(String, String, bool), AppError> {
        let (normalized_model, has_extended_context) =
            normalize_model(&self.params.model, self.params.model_registry)
                .map_err(|e| AppError::BadRequest(e.to_string()))?;

        // Try to get deployment for this specific provider
        if let Some(deployment_id) = self
            .params
            .model_registry
            .get_deployment_for_provider(&normalized_model, &provider.name)
            .await
        {
            return Ok((normalized_model, deployment_id, has_extended_context));
        }

        // Model not available on this provider
        Err(AppError::ModelNotAvailableOnProvider {
            model: normalized_model,
            provider: provider.name.clone(),
        })
    }
}

/// Result of executing a proxy request, indicating if fallback should be attempted
pub enum ProxyExecuteResult {
    /// Request succeeded or failed with non-retriable error
    Response {
        response: Response,
        token_stats: TokenStats,
    },
    /// Got 429 rate limit - should try next provider
    RateLimited,
}

/// Optional database context for request logging.
#[cfg(feature = "db")]
#[derive(Clone)]
pub struct DbContext {
    pub database: crate::database::Database,
    pub request_path: String,
    pub api_key_hash: Option<String>,
}

impl ProxyRequest {
    pub async fn execute(
        &self,
        client: &Client,
        metrics: &MetricsService,
        #[cfg(feature = "db")] db_context: Option<DbContext>,
        quota_manager: Option<crate::quota::QuotaManager>,
        api_key_hash: Option<String>,
    ) -> Result<ProxyExecuteResult> {
        let start_time = Instant::now();

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", self.token))?,
        );
        headers.insert(
            "ai-resource-group",
            HeaderValue::from_str(&self.resource_group)?,
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert(
            AI_CLIENT_TYPE_HEADER,
            HeaderValue::from_static(AI_CLIENT_TYPE_VALUE),
        );

        tracing::debug!(
            "Proxying request to: {} (model: {}, stream: {})",
            self.url,
            self.model,
            self.stream
        );

        let response = client
            .request(self.method.clone(), &self.url)
            .headers(headers)
            .json(&self.body)
            .send()
            .await
            .context("Failed to send proxy request")?;

        if !response.status().is_success() {
            let elapsed = start_time.elapsed();
            let status = response.status();

            // Check for rate limiting - signal to try next provider
            if status == StatusCode::TOO_MANY_REQUESTS {
                let body = response.text().await.unwrap_or_default();
                tracing::warn!(
                    "Rate limited (429) on original_model: {}, resolved_model: {}, provider: {}, time: {:.2}ms, body_len: {}",
                    self.original_model,
                    self.model,
                    self.provider_name,
                    elapsed.as_secs_f64() * 1000.0,
                    body.len()
                );
                return Ok(ProxyExecuteResult::RateLimited);
            }

            // Preserve the upstream content-type instead of hardcoding JSON
            let content_type = extract_content_type(&response);
            let text = response.text().await.unwrap_or_else(|e| {
                tracing::warn!("Failed to decode error response body: {}", e);
                String::new()
            });

            tracing::error!("Proxy request failed: {} - {}", status, text);
            tracing::info!(
                "Proxy done - original_model: {}, resolved_model: {}, provider: {}, time: {:.2}ms, status: {}, stream: {}",
                self.original_model,
                self.model,
                self.provider_name,
                elapsed.as_secs_f64() * 1000.0,
                status,
                self.stream
            );
            return Ok(ProxyExecuteResult::Response {
                response: Response::builder()
                    .status(status)
                    .header("content-type", content_type)
                    .body(Body::from(text))?,
                token_stats: TokenStats::default(),
            });
        }

        if self.stream {
            let response = self
                .handle_streaming_response(
                    response,
                    start_time,
                    metrics,
                    #[cfg(feature = "db")]
                    db_context,
                    quota_manager,
                    api_key_hash,
                )?;
            // Streaming records metrics when the stream completes (inside spawned task)
            Ok(ProxyExecuteResult::Response {
                response,
                token_stats: TokenStats::default(),
            })
        } else {
            let (result, token_stats) = self.handle_regular_response(response).await?;
            let elapsed = start_time.elapsed();
            tracing::info!(
                "Proxy done - original_model: {}, resolved_model: {}, provider: {}, time: {:.2}ms, status: 200, stream: {}, {}",
                self.original_model,
                self.model,
                self.provider_name,
                elapsed.as_secs_f64() * 1000.0,
                self.stream,
                token_stats
            );
            Ok(ProxyExecuteResult::Response {
                response: result,
                token_stats,
            })
        }
    }

    async fn handle_regular_response(
        &self,
        response: reqwest::Response,
    ) -> Result<(Response, TokenStats)> {
        let content_type = extract_content_type(&response);

        let body = response.bytes().await?;

        // Extract token stats from non-streaming response
        let token_stats = match std::str::from_utf8(&body) {
            Ok(body_str) => {
                tracing::debug!("Response received: {} bytes", body.len());
                match extract_token_stats_from_body(body_str, &self.family) {
                    Some(stats) => stats,
                    None => {
                        tracing::debug!(
                            "Could not extract token stats from {:?} response ({} bytes)",
                            self.family,
                            body.len()
                        );
                        TokenStats::default()
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Response body is not valid UTF-8 ({} bytes): {}",
                    body.len(),
                    e
                );
                TokenStats::default()
            }
        };

        Ok((
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", content_type)
                .body(Body::from(body))?,
            token_stats,
        ))
    }

    fn handle_streaming_response(
        &self,
        response: reqwest::Response,
        start_time: Instant,
        metrics: &MetricsService,
        #[cfg(feature = "db")] db_context: Option<DbContext>,
        quota_manager: Option<crate::quota::QuotaManager>,
        api_key_hash: Option<String>,
    ) -> Result<Response> {
        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<axum::body::Bytes, reqwest::Error>>(64);
        let is_claude = matches!(self.family, LlmFamily::Claude);
        let model = self.model.clone();
        let original_model = self.original_model.clone();
        let provider_name = self.provider_name.clone();
        let family = self.family;
        let metrics = metrics.clone();

        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut byte_buf: Vec<u8> = Vec::new();
            let mut token_stats = TokenStats::default();
            let mut client_gone = false;
            let mut stream_error = false;
            let chunk_timeout = Duration::from_secs(STREAMING_TIMEOUT_SECS);

            loop {
                let chunk_result = match tokio::time::timeout(chunk_timeout, stream.next()).await {
                    Ok(Some(result)) => result,
                    Ok(None) => break, // Stream ended normally
                    Err(_) => {
                        tracing::error!(
                            "Streaming response timed out after {}s with no data",
                            STREAMING_TIMEOUT_SECS
                        );
                        stream_error = true;
                        break;
                    }
                };
                match chunk_result {
                    Ok(chunk) => {
                        byte_buf.extend_from_slice(&chunk);

                        // Process complete lines from the byte buffer.
                        // Only convert to String after finding a newline boundary,
                        // so partial multi-byte UTF-8 sequences stay safely in byte_buf.
                        while let Some(pos) = byte_buf.iter().position(|&b| b == b'\n') {
                            let line_bytes = byte_buf[..pos].to_vec();
                            byte_buf.drain(..pos + 1);

                            let line = match String::from_utf8(line_bytes) {
                                Ok(s) => s,
                                Err(e) => {
                                    tracing::warn!("Non-UTF-8 line in stream, skipping: {}", e);
                                    continue;
                                }
                            };
                            let line = line.trim();

                            if let Some(data) = line.strip_prefix(STREAM_DATA_PREFIX)
                                && !data.is_empty()
                            {
                                // Extract token stats if available
                                if let Some(stats) = extract_token_stats(data, &family) {
                                    token_stats = stats
                                }

                                let mut output = String::new();

                                if is_claude
                                    && let Ok(parsed) = serde_json::from_str::<Value>(data)
                                    && let Some(event_type) =
                                        parsed.get("type").and_then(|v| v.as_str())
                                {
                                    output.push_str(&format!("event: {event_type}\n"));
                                }

                                output.push_str(&format!("{STREAM_DATA_PREFIX}{data}\n\n"));
                                if tx.send(Ok(axum::body::Bytes::from(output))).await.is_err() {
                                    tracing::debug!("Client disconnected during streaming");
                                    client_gone = true;
                                    break;
                                }
                            }
                        }
                        if client_gone {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("Stream error: {}", e);
                        let _ = tx.send(Err(e)).await;
                        stream_error = true;
                        break;
                    }
                }
            }

            // Flush any remaining buffered data
            if !client_gone && !byte_buf.is_empty()
                && let Ok(remaining) = String::from_utf8(byte_buf)
            {
                let line = remaining.trim();
                if let Some(data) = line.strip_prefix(STREAM_DATA_PREFIX)
                    && !data.is_empty()
                {
                    if let Some(stats) = extract_token_stats(data, &family) {
                        token_stats = stats;
                    }
                    let output = format!("{STREAM_DATA_PREFIX}{data}\n\n");
                    let _ = tx.send(Ok(axum::body::Bytes::from(output))).await;
                }
            }

            // Record metrics when streaming is done
            let success = !stream_error;
            let counts = token_stats.to_counts();
            metrics.decrement_active();
            metrics
                .record_completion(success, Some(&model), &counts)
                .await;

            // Log completion when streaming is done
            let elapsed = start_time.elapsed();
            tracing::info!(
                "Proxy done - original_model: {}, resolved_model: {}, provider: {}, time: {:.2}ms, status: 200, stream: true, {}",
                original_model,
                model,
                provider_name,
                elapsed.as_secs_f64() * 1000.0,
                token_stats
            );

            // Log streaming request to database and record quota usage
            if let (Some(qm), Some(kh)) = (&quota_manager, &api_key_hash) {
                qm.record_usage_hashed(kh, &counts).await;
            }

            #[cfg(feature = "db")]
            if let Some(ctx) = db_context {
                let record = crate::database::RequestRecord::new(
                    ctx.request_path,
                    model.clone(),
                    provider_name.clone(),
                    elapsed,
                    200,
                    true,
                    &token_stats,
                    ctx.api_key_hash,
                );
                if let Err(e) = ctx.database.insert_request(record).await {
                    tracing::warn!("Failed to log streaming request to database: {}", e);
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        let body = Body::from_stream(stream);

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .body(body)?)
    }
}

/// Returns (resolved_model_name, has_extended_context).
/// When the requested model ends with `[1m]`, the suffix is stripped before resolution
/// and `has_extended_context` is set to true so the caller can inject the 1M context beta.
fn normalize_model(model: &str, registry: &ModelRegistry) -> Result<(String, bool)> {
    // Strip [1m] suffix if present
    let (base_model, has_extended_context) = if let Some(base) = model.strip_suffix(EXTENDED_CONTEXT_SUFFIX) {
        (base, true)
    } else {
        (model, false)
    };

    // 1. Exact match - if the model exists in config, use it directly
    if registry.find_model_config(base_model).is_some() {
        return Ok((base_model.to_string(), has_extended_context));
    }

    // 2. Alias pattern match - find by configured alias patterns
    if let Some(matched_model) = registry.find_model_by_alias(base_model) {
        tracing::debug!(
            "Model '{}' matched alias pattern for configured model '{}'",
            base_model,
            matched_model.name
        );
        return Ok((matched_model.name.clone(), has_extended_context));
    }

    // 3. Family fallback - determine family prefix and check for configured fallback
    let prefix = if base_model.starts_with(CLAUDE_PREFIX) {
        CLAUDE_PREFIX
    } else if base_model.starts_with(GEMINI_PREFIX) {
        GEMINI_PREFIX
    } else if base_model.starts_with(GPT_PREFIX) {
        GPT_PREFIX
    } else if base_model.starts_with(TEXT_PREFIX) {
        TEXT_PREFIX
    } else {
        // Unknown family, return as-is
        return Ok((base_model.to_string(), has_extended_context));
    };

    // Try to get configured fallback for this family
    if let Some(fallback_model) = registry.get_fallback_model(prefix)
        && registry.find_model_config(fallback_model).is_some()
    {
        tracing::info!(
            "Model '{}' not found, falling back to configured '{}' for {} family",
            base_model,
            fallback_model,
            prefix
        );
        return Ok((fallback_model.to_string(), has_extended_context));
    }

    Ok((base_model.to_string(), has_extended_context))
}

fn determine_family(model: &str) -> LlmFamily {
    if model.starts_with(CLAUDE_PREFIX) {
        LlmFamily::Claude
    } else if model.starts_with(GEMINI_PREFIX) {
        LlmFamily::Gemini
    } else {
        LlmFamily::OpenAi
    }
}

fn extract_stream_flag(body: &Value, family: &LlmFamily, action: &Option<String>) -> bool {
    match family {
        LlmFamily::Claude => body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        LlmFamily::Gemini => action.as_deref() == Some(STREAM_GENERATE_CONTENT_ACTION),
        LlmFamily::OpenAi => body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn prepare_body(body: &mut Value, family: &LlmFamily, stream: bool, _model: &str) -> Result<()> {
    match family {
        LlmFamily::Claude => {
            // Validate messages array before body transformation
            validate_anthropic_messages(body)?;

            if let Some(obj) = body.as_object_mut() {
                obj.insert("anthropic_version".to_string(), json!(ANTHROPIC_VERSION));
                obj.remove("stream");
                obj.remove("model");
                obj.remove("context_management");

                // Default max_tokens if not provided (Bedrock requires this field)
                if !obj.contains_key("max_tokens") {
                    obj.insert("max_tokens".to_string(), json!(ANTHROPIC_DEFAULT_MAX_TOKENS));
                }

                // Strip unsupported "scope" field from cache_control objects
                // (Claude Code 2.1.88+ sends this but Bedrock doesn't support it)
                strip_cache_control_scope(obj);

                // Validate and clamp thinking budget for Bedrock compatibility
                clamp_thinking(obj);
            }
        }
        LlmFamily::Gemini => {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("model");
                obj.remove("stream");

                // Strip ID from function responses (AI Core rejects them)
                strip_gemini_function_response_ids(obj);

                // Convert ThinkingBudget 0 to -1 (dynamic) to avoid AI Core errors
                fix_gemini_thinking_budget(obj);
            }
        }
        LlmFamily::OpenAi => {
            if let Some(obj) = body.as_object_mut() {
                // Convert max_tokens to max_completion_tokens for all OpenAI models
                // (max_completion_tokens is the preferred field since GPT-4o / 2024-08-06+)
                if obj.contains_key("max_tokens") && !obj.contains_key("max_completion_tokens")
                    && let Some(max_tokens) = obj.remove("max_tokens")
                {
                    obj.insert("max_completion_tokens".to_string(), max_tokens);
                }

                // Add stream_options to include usage stats for streaming requests
                if stream {
                    match obj.get_mut("stream_options") {
                        Some(existing_options) => {
                            // Merge include_usage into existing stream_options
                            if let Some(options_obj) = existing_options.as_object_mut() {
                                options_obj.insert("include_usage".to_string(), json!(true));
                            }
                        }
                        None => {
                            // Create new stream_options with include_usage
                            obj.insert(
                                "stream_options".to_string(),
                                json!({"include_usage": true}),
                            );
                        }
                    }
                }

                // Fix Codex CLI bug: preamble assistant message inserted between
                // assistant(tool_calls) and tool(response) messages.
                normalize_openai_messages(obj);
            }
        }
    }
    Ok(())
}

fn extract_content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string()
}

/// Extract OpenAI token stats from a `usage` JSON object.
fn extract_openai_tokens(usage: &Value) -> TokenStats {
    TokenStats {
        input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()),
        cache_read: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        cache_write: None,
    }
}

/// Extract Gemini token stats from a `usageMetadata` JSON object.
/// In streaming mode, input_tokens is required (returns None if absent).
/// In non-streaming mode, input_tokens is optional; output returns None if zero.
fn extract_gemini_tokens(usage_metadata: &Value, streaming: bool) -> Option<TokenStats> {
    let input_tokens = usage_metadata.get("promptTokenCount").and_then(|v| v.as_u64());
    if streaming && input_tokens.is_none() {
        return None;
    }
    let candidates = usage_metadata
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let thoughts = usage_metadata
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = if streaming || candidates > 0 || thoughts > 0 {
        Some(candidates + thoughts)
    } else {
        None
    };
    Some(TokenStats {
        input_tokens,
        output_tokens,
        cache_read: usage_metadata
            .get("cachedContentTokenCount")
            .and_then(|v| v.as_u64()),
        cache_write: None,
    })
}

fn extract_token_stats(data: &str, family: &LlmFamily) -> Option<TokenStats> {
    let parsed: Value = serde_json::from_str(data).ok()?;

    match family {
        LlmFamily::Claude => {
            if parsed.get("type")?.as_str()? == "message_stop" {
                let metrics = parsed.get("amazon-bedrock-invocationMetrics")?;
                Some(TokenStats {
                    input_tokens: metrics.get("inputTokenCount")?.as_u64(),
                    output_tokens: metrics.get("outputTokenCount")?.as_u64(),
                    cache_read: metrics.get("cacheReadInputTokenCount")?.as_u64(),
                    cache_write: metrics.get("cacheWriteInputTokenCount")?.as_u64(),
                })
            } else {
                None
            }
        }
        LlmFamily::OpenAi => {
            let usage = parsed.get("usage")?;
            Some(extract_openai_tokens(usage))
        }
        LlmFamily::Gemini => {
            let usage_metadata = parsed.get("usageMetadata")?;
            extract_gemini_tokens(usage_metadata, true)
        }
    }
}

/// Extract token stats from a complete (non-streaming) response body.
fn extract_token_stats_from_body(body: &str, family: &LlmFamily) -> Option<TokenStats> {
    let parsed: Value = serde_json::from_str(body).ok()?;

    match family {
        LlmFamily::Claude => {
            let usage = parsed.get("usage")?;
            Some(TokenStats {
                input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                cache_read: usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64()),
                cache_write: usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64()),
            })
        }
        LlmFamily::OpenAi => {
            let usage = parsed.get("usage")?;
            Some(extract_openai_tokens(usage))
        }
        LlmFamily::Gemini => {
            let usage_metadata = parsed.get("usageMetadata")?;
            extract_gemini_tokens(usage_metadata, false)
        }
    }
}

fn build_url(
    model: &str,
    deployment_id: &str,
    action: &Option<String>,
    base_url: &str,
    family: &LlmFamily,
    stream: bool,
    openai_api_version: &str,
) -> Result<String> {
    match family {
        LlmFamily::Claude => {
            let action = if stream {
                INVOKE_STREAM_ACTION
            } else {
                INVOKE_ACTION
            };
            Ok(format!(
                "{base_url}{INFERENCE_DEPLOYMENTS_PATH}/{deployment_id}/{action}"
            ))
        }
        LlmFamily::Gemini => {
            let action = action.as_deref().unwrap_or(GENERATE_CONTENT_ACTION);
            Ok(format!(
                "{base_url}{INFERENCE_DEPLOYMENTS_PATH}/{deployment_id}{MODELS_PATH}/{model}:{action}"
            ))
        }
        LlmFamily::OpenAi => {
            if model.starts_with(TEXT_PREFIX) {
                Ok(format!(
                    "{base_url}{INFERENCE_DEPLOYMENTS_PATH}/{deployment_id}{EMBEDDINGS_PATH}?api-version={openai_api_version}"
                ))
            } else {
                Ok(format!(
                    "{base_url}{INFERENCE_DEPLOYMENTS_PATH}/{deployment_id}{CHAT_COMPLETIONS_PATH}?api-version={openai_api_version}"
                ))
            }
        }
    }
}

/// Extract Anthropic-Beta header and map features to Bedrock-supported equivalents.
/// Unknown features are silently dropped.
fn extract_anthropic_beta(headers: &HeaderMap) -> Vec<String> {
    let header_value = match headers.get(ANTHROPIC_BETA_HEADER) {
        Some(v) => match v.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return vec![],
        },
        None => return vec![],
    };

    let mut features = Vec::new();
    for feature in header_value.split(',') {
        let feature = feature.trim().to_lowercase();
        for &(anthropic_name, bedrock_name) in ALLOWED_BETA_FEATURES {
            if feature == anthropic_name {
                let bedrock_feature = bedrock_name.to_string();
                if !features.contains(&bedrock_feature) {
                    features.push(bedrock_feature);
                }
                break;
            }
        }
    }
    features
}

/// Validate Anthropic messages array is non-empty and messages have content.
/// The last message may be an empty assistant message (pre-fill pattern).
fn validate_anthropic_messages(body: &Value) -> Result<()> {
    let messages = match body.get("messages").and_then(|v| v.as_array()) {
        Some(msgs) => msgs,
        None => return Ok(()), // Let Bedrock handle missing field
    };

    if messages.is_empty() {
        anyhow::bail!("messages array cannot be empty");
    }

    for (i, msg) in messages.iter().enumerate() {
        let content = msg.get("content");
        let is_empty = match content {
            None => true,
            Some(Value::String(s)) => s.is_empty(),
            Some(Value::Array(a)) => a.is_empty(),
            Some(Value::Null) => true,
            _ => false,
        };

        if is_empty {
            let is_last = i == messages.len() - 1;
            let is_assistant = msg.get("role").and_then(|v| v.as_str()) == Some("assistant");
            if !is_last || !is_assistant {
                anyhow::bail!(
                    "message at index {} has empty content (only the last assistant message may be empty)",
                    i
                );
            }
        }
    }

    Ok(())
}

/// Strip unsupported "scope" field from cache_control objects in system and message content.
/// Claude Code 2.1.88+ adds a "scope" field that Bedrock doesn't support and will reject.
fn strip_cache_control_scope(obj: &mut serde_json::Map<String, Value>) {
    // Strip from system content (can be string or array of content blocks)
    if let Some(system) = obj.get_mut("system") {
        strip_scope_from_content(system);
    }

    // Strip from message content blocks
    if let Some(Value::Array(messages)) = obj.get_mut("messages") {
        for message in messages.iter_mut() {
            if let Some(content) = message.get_mut("content") {
                strip_scope_from_content(content);
            }
        }
    }
}

fn strip_scope_from_content(content: &mut Value) {
    match content {
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                if let Some(cache_control) = block.get_mut("cache_control")
                    && let Some(cc_obj) = cache_control.as_object_mut()
                {
                    cc_obj.remove("scope");
                }
            }
        }
        Value::Object(obj) => {
            if let Some(cache_control) = obj.get_mut("cache_control")
                && let Some(cc_obj) = cache_control.as_object_mut()
            {
                cc_obj.remove("scope");
            }
        }
        _ => {}
    }
}

/// Validate and clamp thinking budget for Bedrock compatibility.
/// - Disables thinking if max_tokens < 1025 (need 1024 for thinking + 1 for output)
/// - Ensures budget_tokens >= 1024 (Anthropic minimum)
/// - Clamps budget_tokens < max_tokens (Bedrock constraint)
fn clamp_thinking(obj: &mut serde_json::Map<String, Value>) {
    let thinking = match obj.get("thinking") {
        Some(t) if t.is_object() => t,
        _ => return,
    };

    // Only process if thinking is enabled
    let thinking_type = thinking
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if thinking_type != "enabled" {
        return;
    }

    let max_tokens = obj
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let min_required = MIN_BUDGET_TOKENS_FOR_THINKING + BUDGET_RESERVE_MARGIN;

    // Disable thinking if max_tokens is too small
    if max_tokens < min_required {
        tracing::debug!(
            "Disabling thinking: max_tokens ({}) < minimum required ({})",
            max_tokens,
            min_required
        );
        obj.remove("thinking");
        return;
    }

    let budget_tokens = thinking
        .get("budget_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Ensure budget_tokens >= 1024
    let mut new_budget = budget_tokens;
    if budget_tokens > 0 && budget_tokens < MIN_BUDGET_TOKENS_FOR_THINKING {
        new_budget = MIN_BUDGET_TOKENS_FOR_THINKING;
    }

    // Clamp budget_tokens if it exceeds or equals max_tokens (Bedrock constraint)
    if new_budget >= max_tokens {
        new_budget = max_tokens - BUDGET_RESERVE_MARGIN;
    }

    if new_budget != budget_tokens {
        tracing::debug!(
            "Clamping thinking budget_tokens: {} -> {} (max_tokens: {})",
            budget_tokens,
            new_budget,
            max_tokens
        );
        if let Some(thinking_obj) = obj.get_mut("thinking").and_then(|t| t.as_object_mut()) {
            thinking_obj.insert("budget_tokens".to_string(), json!(new_budget));
        }
    }
}

/// Strip ID from Gemini function responses (AI Core rejects them).
fn strip_gemini_function_response_ids(obj: &mut serde_json::Map<String, Value>) {
    if let Some(Value::Array(contents)) = obj.get_mut("contents") {
        for content in contents.iter_mut() {
            if let Some(Value::Array(parts)) = content.get_mut("parts") {
                for part in parts.iter_mut() {
                    if let Some(func_response) = part.get_mut("functionResponse")
                        && let Some(fr_obj) = func_response.as_object_mut()
                    {
                        fr_obj.remove("id");
                    }
                }
            }
        }
    }
}

/// Convert Gemini ThinkingBudget 0 to -1 (dynamic) to avoid AI Core errors.
fn fix_gemini_thinking_budget(obj: &mut serde_json::Map<String, Value>) {
    if let Some(config) = obj.get_mut("generationConfig")
        && let Some(thinking_config) = config.get_mut("thinkingConfig")
        && let Some(budget) = thinking_config.get("thinkingBudget")
        && budget.as_i64() == Some(0)
    {
        tracing::debug!("Gemini thinking budget 0 changed to -1 (dynamic)");
        if let Some(tc_obj) = thinking_config.as_object_mut() {
            tc_obj.insert("thinkingBudget".to_string(), json!(-1));
        }
    }
}

/// Check if a triple of messages matches the Codex CLI preamble pattern:
/// assistant(tool_calls) → assistant(content preamble) → tool(response with matching id).
fn is_preamble_pattern(msg: &Value, preamble: &Value, tool_msg: &Value) -> bool {
    let is_assistant_with_tool_calls = msg.get("role").and_then(|v| v.as_str()) == Some("assistant")
        && msg.get("tool_calls").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty());
    let is_preamble = preamble.get("role").and_then(|v| v.as_str()) == Some("assistant")
        && preamble.get("content").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty());
    let is_tool_response = tool_msg.get("role").and_then(|v| v.as_str()) == Some("tool");

    if !(is_assistant_with_tool_calls && is_preamble && is_tool_response) {
        return false;
    }
    let tool_call_id = tool_msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
    msg.get("tool_calls")
        .and_then(|v| v.as_array())
        .is_some_and(|calls| {
            calls.iter().any(|c| c.get("id").and_then(|v| v.as_str()) == Some(tool_call_id))
        })
}

/// Fix Codex CLI bug where a preamble assistant message is inserted between
/// assistant(tool_calls) and tool(response) messages.
/// Pattern: assistant(tool_calls) → assistant(content) → tool(response)
/// Fix: merge preamble content into the first assistant message, remove the duplicate.
fn normalize_openai_messages(obj: &mut serde_json::Map<String, Value>) {
    let messages = match obj.get("messages") {
        Some(Value::Array(msgs)) if msgs.len() >= 3 => msgs,
        _ => return,
    };

    // First pass: detect if normalization is needed (avoid cloning if not)
    let needs_normalization = {
        let mut found = false;
        let mut i = 0;
        while i + 2 < messages.len() {
            if is_preamble_pattern(&messages[i], &messages[i + 1], &messages[i + 2]) {
                found = true;
                break;
            }
            i += 1;
        }
        found
    };

    if !needs_normalization {
        return;
    }

    // Only clone when we know normalization is needed
    let messages = match obj.remove("messages") {
        Some(Value::Array(msgs)) => msgs,
        _ => return,
    };

    let mut normalized: Vec<Value> = Vec::with_capacity(messages.len());
    let mut i = 0;

    while i < messages.len() {
        if i + 2 < messages.len()
            && is_preamble_pattern(&messages[i], &messages[i + 1], &messages[i + 2])
        {
            let preamble_content = messages[i + 1].get("content").and_then(|v| v.as_str()).unwrap_or("");
            let mut merged = messages[i].clone();

            let existing_content = messages[i].get("content").and_then(|v| v.as_str()).unwrap_or("");
            let new_content = if existing_content.is_empty() {
                preamble_content.to_string()
            } else {
                format!("{}\n\n{}", existing_content, preamble_content)
            };
            if let Some(obj) = merged.as_object_mut() {
                obj.insert("content".to_string(), Value::String(new_content));
            }

            tracing::debug!("Normalized Codex CLI preamble message at index {}", i);
            normalized.push(merged);
            i += 2; // Skip the preamble; loop increment will advance past it
            continue;
        }

        normalized.push(messages[i].clone());
        i += 1;
    }

    obj.insert("messages".to_string(), Value::Array(normalized));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FallbackModels, Model};
    use crate::registry::ModelRegistry;
    use crate::token::TokenManager;

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
    fn test_normalize_model_with_1m_suffix() {
        let models = vec![Model {
            name: "claude-opus-4-6".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let (name, extended) = normalize_model("claude-opus-4-6[1m]", &registry).unwrap();
        assert_eq!(name, "claude-opus-4-6");
        assert!(extended);
    }

    #[test]
    fn test_normalize_model_without_1m_suffix() {
        let models = vec![Model {
            name: "claude-opus-4-6".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let (name, extended) = normalize_model("claude-opus-4-6", &registry).unwrap();
        assert_eq!(name, "claude-opus-4-6");
        assert!(!extended);
    }

    #[test]
    fn test_normalize_model_1m_suffix_with_alias_resolution() {
        let models = vec![Model {
            name: "claude-opus-4-6".to_string(),
            aicore_model_name: None,
            aliases: vec!["claude-opus-4-6-*".to_string()],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        // Alias match with [1m] suffix
        let (name, extended) =
            normalize_model("claude-opus-4-6-20250101[1m]", &registry).unwrap();
        assert_eq!(name, "claude-opus-4-6");
        assert!(extended);
    }

    #[test]
    fn test_normalize_model_1m_suffix_with_family_fallback() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            pricing: None,
        }];
        let registry = ModelRegistry::new(
            models,
            FallbackModels {
                claude: Some("claude-sonnet-4-5".to_string()),
                openai: None,
                gemini: None,
            },
            vec![],
            TokenManager::new(vec!["test".to_string()]),
            600,
        );

        // Unknown claude model with [1m] should fall back to configured claude fallback
        let (name, extended) =
            normalize_model("claude-unknown-model[1m]", &registry).unwrap();
        assert_eq!(name, "claude-sonnet-4-5");
        assert!(extended);
    }

    #[test]
    fn test_normalize_model_1m_suffix_non_claude() {
        let models = vec![Model {
            name: "gpt-4o".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let (name, extended) = normalize_model("gpt-4o[1m]", &registry).unwrap();
        assert_eq!(name, "gpt-4o");
        assert!(extended);
    }
}
