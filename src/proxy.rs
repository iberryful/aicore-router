use anyhow::{Context, Result};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
};
use futures::stream::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

use crate::balancer::LoadBalancer;
use crate::config::{Config, Provider};
use crate::constants::{api::*, models::*};
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
                .and_then(|auth_str| {
                    if auth_str.starts_with("Bearer ") {
                        Some(auth_str.strip_prefix("Bearer ").unwrap().to_string())
                    } else {
                        None
                    }
                })
        })
}

use std::fmt;

#[derive(Debug, Default)]
struct TokenStats {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

impl fmt::Display for TokenStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "input_tokens: {}, output_tokens: {}, cache_read: {}, cache_write: {}",
            self.input_tokens
                .map_or("N/A".to_string(), |t| t.to_string()),
            self.output_tokens
                .map_or("N/A".to_string(), |t| t.to_string()),
            self.cache_read.map_or("N/A".to_string(), |t| t.to_string()),
            self.cache_write
                .map_or("N/A".to_string(), |t| t.to_string())
        )
    }
}

#[derive(Debug, Clone)]
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
        let (normalized_model, deployment_id) = self.resolve_model_for_provider(provider).await?;

        // Step 4: Determine LLM family and stream flag
        let family = determine_family(&normalized_model);
        let stream = extract_stream_flag(&self.params.body, &family, &self.params.action);

        // Step 5: Prepare request body
        let mut body = self.params.body.clone();
        prepare_body(&mut body, &family, stream, &normalized_model)?;

        // Step 6: Build target URL using the provider's API URL
        let url = build_url(
            &normalized_model,
            &deployment_id,
            &self.params.action,
            &provider.genai_api_url,
            &family,
            stream,
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

    /// Resolve model to deployment ID for a specific provider
    async fn resolve_model_for_provider(
        &self,
        provider: &Provider,
    ) -> Result<(String, String), AppError> {
        let normalized_model = normalize_model(&self.params.model, self.params.model_registry)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;

        // Try to get deployment for this specific provider
        if let Some(deployment_id) = self
            .params
            .model_registry
            .get_deployment_for_provider(&normalized_model, &provider.name)
            .await
        {
            return Ok((normalized_model, deployment_id));
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
    Response(Response),
    /// Got 429 rate limit - should try next provider
    RateLimited,
}

impl ProxyRequest {
    pub async fn execute(&self, client: &Client, _config: &Config) -> Result<ProxyExecuteResult> {
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

        tracing::debug!("Proxying request to: {}", self.url);
        tracing::debug!(
            "Request body: {}",
            serde_json::to_string_pretty(&self.body)?
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
            let text = response.text().await.unwrap_or_default();

            // Check for rate limiting - signal to try next provider
            if status == StatusCode::TOO_MANY_REQUESTS {
                tracing::warn!(
                    "Rate limited (429) on original_model: {}, resolved_model: {}, provider: {}, time: {:.2}ms",
                    self.original_model,
                    self.model,
                    self.provider_name,
                    elapsed.as_secs_f64() * 1000.0
                );
                return Ok(ProxyExecuteResult::RateLimited);
            }

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
            return Ok(ProxyExecuteResult::Response(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(text))?,
            ));
        }

        if self.stream {
            Ok(ProxyExecuteResult::Response(
                self.handle_streaming_response(response, start_time).await?,
            ))
        } else {
            let result = self.handle_regular_response(response).await;
            let elapsed = start_time.elapsed();
            tracing::info!(
                "Proxy done - original_model: {}, resolved_model: {}, provider: {}, time: {:.2}ms, status: 200, stream: {}",
                self.original_model,
                self.model,
                self.provider_name,
                elapsed.as_secs_f64() * 1000.0,
                self.stream
            );
            Ok(ProxyExecuteResult::Response(result?))
        }
    }

    async fn handle_regular_response(&self, response: reqwest::Response) -> Result<Response> {
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();

        let body = response.bytes().await?;

        // Log response body at debug level
        if let Ok(body_str) = String::from_utf8(body.to_vec()) {
            tracing::debug!("Response body: {}", body_str);
        }

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", content_type)
            .body(Body::from(body))?)
    }

    async fn handle_streaming_response(
        &self,
        response: reqwest::Response,
        start_time: Instant,
    ) -> Result<Response> {
        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<axum::body::Bytes, reqwest::Error>>(1024);
        let is_claude = matches!(self.family, LlmFamily::Claude);
        let model = self.model.clone();
        let original_model = self.original_model.clone();
        let provider_name = self.provider_name.clone();
        let family = self.family.clone();

        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut token_stats = TokenStats::default();

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        if let Ok(chunk_str) = String::from_utf8(chunk.to_vec()) {
                            buffer.push_str(&chunk_str);

                            while let Some(line_end) = buffer.find('\n') {
                                let line = buffer[..line_end].trim().to_string();
                                buffer.drain(..line_end + 1);

                                if let Some(data) = line.strip_prefix("data: ")
                                    && !data.is_empty()
                                {
                                    // Log streaming data at debug level
                                    tracing::debug!("Stream data: {}", data);

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

                                    output.push_str(&format!("data: {data}\n\n"));
                                    let _ = tx.send(Ok(axum::body::Bytes::from(output))).await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Stream error: {}", e);
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }

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

fn normalize_model(model: &str, registry: &ModelRegistry) -> Result<String> {
    // 1. Exact match - if the model exists in config, use it directly
    if registry.find_model_config(model).is_some() {
        return Ok(model.to_string());
    }

    // 2. Alias pattern match - find by configured alias patterns
    if let Some(matched_model) = registry.find_model_by_alias(model) {
        tracing::debug!(
            "Model '{}' matched alias pattern for configured model '{}'",
            model,
            matched_model.name
        );
        return Ok(matched_model.name.clone());
    }

    // 3. Family fallback - determine family prefix and check for configured fallback
    let prefix = if model.starts_with(CLAUDE_PREFIX) {
        CLAUDE_PREFIX
    } else if model.starts_with(GEMINI_PREFIX) {
        GEMINI_PREFIX
    } else if model.starts_with(GPT_PREFIX) {
        GPT_PREFIX
    } else if model.starts_with(TEXT_PREFIX) {
        TEXT_PREFIX
    } else {
        // Unknown family, return as-is
        return Ok(model.to_string());
    };

    // Try to get configured fallback for this family
    if let Some(fallback_model) = registry.get_fallback_model(prefix)
        && registry.find_model_config(fallback_model).is_some()
    {
        tracing::info!(
            "Model '{}' not found, falling back to configured '{}' for {} family",
            model,
            fallback_model,
            prefix
        );
        return Ok(fallback_model.to_string());
    }

    Ok(model.to_string())
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

fn prepare_body(body: &mut Value, family: &LlmFamily, stream: bool, model: &str) -> Result<()> {
    match family {
        LlmFamily::Claude => {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("anthropic_version".to_string(), json!(ANTHROPIC_VERSION));
                obj.remove("stream");
                obj.remove("model");

                if obj.contains_key("thinking") && obj.contains_key("temperature") {
                    obj.remove("temperature");
                }
            }
        }
        LlmFamily::Gemini => {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("model");
                obj.remove("stream");
            }
        }
        LlmFamily::OpenAi => {
            if let Some(obj) = body.as_object_mut() {
                // For GPT-5 models, replace max_tokens with max_completion_tokens and drop temperature
                if model.starts_with("gpt-5") {
                    if let Some(max_tokens) = obj.remove("max_tokens") {
                        obj.insert("max_completion_tokens".to_string(), max_tokens);
                    }
                    obj.remove("temperature");
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
            }
        }
    }
    Ok(())
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
            Some(TokenStats {
                input_tokens: usage.get("prompt_tokens")?.as_u64(),
                output_tokens: usage.get("completion_tokens")?.as_u64(),
                cache_read: None,
                cache_write: None,
            })
        }
        LlmFamily::Gemini => {
            let usage_metadata = parsed.get("usageMetadata")?;
            let input_tokens = usage_metadata.get("promptTokenCount")?.as_u64()?;
            let total_tokens = usage_metadata.get("totalTokenCount")?.as_u64();
            let output_tokens = total_tokens.map(|t| t.saturating_sub(input_tokens));

            Some(TokenStats {
                input_tokens: Some(input_tokens),
                output_tokens,
                cache_read: usage_metadata
                    .get("cachedContentTokenCount")
                    .and_then(|v| v.as_u64()),
                cache_write: None,
            })
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
                    "{base_url}{INFERENCE_DEPLOYMENTS_PATH}/{deployment_id}{EMBEDDINGS_PATH}?api-version={DEFAULT_API_VERSION}"
                ))
            } else {
                Ok(format!(
                    "{base_url}{INFERENCE_DEPLOYMENTS_PATH}/{deployment_id}{CHAT_COMPLETIONS_PATH}?api-version={DEFAULT_API_VERSION}"
                ))
            }
        }
    }
}
