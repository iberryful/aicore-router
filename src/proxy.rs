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

use crate::config::Config;
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
    pub model: String,
}

impl ProxyRequest {
    pub async fn new(
        headers: &HeaderMap,
        method: Method,
        body: Value,
        model: String,
        action: Option<String>,
        config: &Config,
        token_manager: &TokenManager,
    ) -> Result<Self, AppError> {
        let api_key = extract_api_key(headers).ok_or(AppError::MissingApiKey)?;

        let token = token_manager
            .get_token(&api_key)
            .await
            .map_err(AppError::Internal)?
            .ok_or(AppError::InvalidApiKey)?;

        let normalized_model =
            normalize_model(&model, config).map_err(|e| AppError::BadRequest(e.to_string()))?;

        let deployment_id = resolve_deployment_id(&normalized_model, config)
            .await
            .map_err(|e| AppError::BadRequest(e.to_string()))?;

        let family = determine_family(&normalized_model);
        let mut body = body;
        let stream = extract_stream_flag(&body, &family, &action);

        let url = build_url(
            &normalized_model,
            &deployment_id,
            &action,
            &config.genai_api_url,
            &family,
            stream,
        )?;

        prepare_body(&mut body, &family, stream)?;

        Ok(Self {
            family,
            method,
            body,
            stream,
            url,
            token,
            model: normalized_model,
        })
    }

    pub async fn execute(&self, client: &Client, config: &Config) -> Result<Response> {
        let start_time = Instant::now();

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", self.token))?,
        );
        headers.insert(
            "ai-resource-group",
            HeaderValue::from_str(&config.resource_group)?,
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
            tracing::error!("Proxy request failed: {} - {}", status, text);
            tracing::info!(
                "Proxy done - model: {}, time: {:.2}ms, status: {}, stream: {}",
                self.model,
                elapsed.as_secs_f64() * 1000.0,
                status,
                self.stream
            );
            return Ok(Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(text))?);
        }

        if self.stream {
            self.handle_streaming_response(response, start_time).await
        } else {
            let result = self.handle_regular_response(response).await;
            let elapsed = start_time.elapsed();
            tracing::info!(
                "Proxy done - model: {}, time: {:.2}ms, status: 200, stream: {}",
                self.model,
                elapsed.as_secs_f64() * 1000.0,
                self.stream
            );
            result
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

                                if let Some(data) = line.strip_prefix("data: ") {
                                    if !data.is_empty() {
                                        // Log streaming data at debug level
                                        tracing::debug!("Stream data: {}", data);

                                        // Extract token stats if available
                                        if let Some(stats) = extract_token_stats(data, &family) {
                                            token_stats = stats
                                        }

                                        let mut output = String::new();

                                        if is_claude {
                                            if let Ok(parsed) = serde_json::from_str::<Value>(data)
                                            {
                                                if let Some(event_type) =
                                                    parsed.get("type").and_then(|v| v.as_str())
                                                {
                                                    output.push_str(&format!(
                                                        "event: {event_type}\n"
                                                    ));
                                                }
                                            }
                                        }

                                        output.push_str(&format!("data: {data}\n\n"));
                                        let _ = tx.send(Ok(axum::body::Bytes::from(output))).await;
                                    }
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
                "Proxy done - model: {}, time: {:.2}ms, status: 200, stream: true, {}",
                model,
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

fn normalize_model(model: &str, config: &Config) -> Result<String> {
    // Simple normalization - if the model exists in config, use it
    if config.models.iter().any(|m| m.name == model) {
        return Ok(model.to_string());
    }

    // Basic fallback for claude models
    if model.starts_with("claude") && config.models.iter().any(|m| m.name == "claude-sonnet-4") {
        return Ok("claude-sonnet-4".to_string());
    }

    Ok(model.to_string())
}

async fn resolve_deployment_id(model: &str, config: &Config) -> Result<String> {
    if let Some(deployment_id) = config.get_resolved_deployment_id(model).await {
        Ok(deployment_id)
    } else {
        let available = config.get_available_models().await.join(", ");
        Err(anyhow::anyhow!(
            "Model '{}' not found or not resolved. Available models: {}",
            model,
            available
        ))
    }
}

fn determine_family(model: &str) -> LlmFamily {
    if model.starts_with("claude") {
        LlmFamily::Claude
    } else if model.starts_with("gemini") {
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
        LlmFamily::Gemini => action.as_deref() == Some("streamGenerateContent"),
        LlmFamily::OpenAi => body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn prepare_body(body: &mut Value, family: &LlmFamily, stream: bool) -> Result<()> {
    match family {
        LlmFamily::Claude => {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("anthropic_version".to_string(), json!("bedrock-2023-05-31"));
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
    const DEFAULT_API_VERSION: &str = "2025-04-01-preview";

    match family {
        LlmFamily::Claude => {
            let action = if stream {
                "invoke-with-response-stream"
            } else {
                "invoke"
            };
            Ok(format!(
                "{base_url}/v2/inference/deployments/{deployment_id}/{action}"
            ))
        }
        LlmFamily::Gemini => {
            let action = action.as_deref().unwrap_or("generateContent");
            Ok(format!(
                "{base_url}/v2/inference/deployments/{deployment_id}/models/{model}:{action}"
            ))
        }
        LlmFamily::OpenAi => {
            if model.starts_with("text") {
                Ok(format!(
                    "{base_url}/v2/inference/deployments/{deployment_id}/embeddings?api-version={DEFAULT_API_VERSION}"
                ))
            } else {
                Ok(format!(
                    "{base_url}/v2/inference/deployments/{deployment_id}/chat/completions?api-version={DEFAULT_API_VERSION}"
                ))
            }
        }
    }
}
