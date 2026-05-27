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
    /// OpenAI Responses API (`/v1/responses`) — different request shape (`input`
    /// instead of `messages`), different URL action, different SSE event types
    /// (`response.created` … `response.completed`), different usage-field names
    /// (`input_tokens` / `output_tokens`). Selected by route, not model name.
    OpenAiResponses,
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
    /// Route-determined family override. When set, takes priority over
    /// `determine_family(model)`. Used when the route uniquely determines the
    /// family regardless of model name — e.g. `/v1/responses` is OpenAI-only
    /// (Responses API), so `handle_openai_responses` sets this to
    /// `Some(LlmFamily::OpenAiResponses)`. Other routes leave it `None`.
    pub force_family: Option<LlmFamily>,
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

        // Step 4: Determine LLM family and stream flag.
        // Route-driven override takes priority — used by routes that are tied
        // to a specific API shape regardless of model name (e.g. /v1/responses).
        let family = match self.params.force_family {
            Some(f) => f,
            None => determine_family(&normalized_model)?,
        };
        let stream = extract_stream_flag(&self.params.body, &family, &self.params.action);

        // Step 5: Prepare request body
        let mut body = self.params.body.clone();
        prepare_body(&mut body, &family, stream, &normalized_model)?;

        // Step 6: Extract Anthropic-Beta header and convert to Bedrock beta features
        let mut anthropic_beta = if matches!(family, LlmFamily::Claude) {
            crate::transforms::extract_anthropic_beta(self.params.headers)
        } else {
            vec![]
        };

        // Step 6b: Auto-enable each Claude model's maximum context window. Inject
        // the unlocking beta only when the model needs one (Sonnet 4 / 4.5 →
        // context-1m-2025-08-07; native-1M and 200k-only models get nothing).
        // The client-side `[1m]` suffix is silently accepted as a backward-compat
        // no-op — `normalize_model` strips it; capability-driven injection above
        // is what actually enables max context.
        if matches!(family, LlmFamily::Claude)
            && let Some(beta) = crate::constants::get_extended_context_beta(&normalized_model)
        {
            let feature = beta.to_string();
            if !anthropic_beta.contains(&feature) {
                tracing::debug!(
                    "Auto-enabling extended-context beta '{}' for model '{}'",
                    beta,
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
    /// Returns (normalized_model, deployment_id).
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
        active_guard: &mut Option<crate::metrics::ActiveRequestGuard>,
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
            // Peek the upstream's first chunks: if a rate-limit / throttling
            // signal arrives before any forwardable data, surface as
            // `RateLimited` so the existing 429-retry loop in
            // `routes::execute_proxy_request` fails over to the next
            // provider — the client never sees the failure. See
            // `transforms::stream_classify` for per-family detection rules.
            let mut byte_stream: futures::stream::BoxStream<
                'static,
                reqwest::Result<axum::body::Bytes>,
            > = Box::pin(response.bytes_stream());
            let peek_timeout = Duration::from_secs(crate::constants::api::STREAM_PEEK_TIMEOUT_SECS);
            let (outcome, prebuffered) =
                peek_classify_stream(&mut byte_stream, &self.family, peek_timeout).await;
            match outcome {
                PeekOutcome::RateLimited => {
                    tracing::warn!(
                        "Rate-limited mid-stream on original_model: {}, resolved_model: {}, provider: {}, time: {:.2}ms (failing over)",
                        self.original_model,
                        self.model,
                        self.provider_name,
                        start_time.elapsed().as_secs_f64() * 1000.0
                    );
                    return Ok(ProxyExecuteResult::RateLimited);
                }
                PeekOutcome::Transport(e) => {
                    return Err(anyhow::anyhow!("upstream stream error during peek: {}", e));
                }
                PeekOutcome::Committed | PeekOutcome::PeekTimeout | PeekOutcome::StreamEnded => {}
            }

            let response = self.handle_streaming_response(
                PreparedStream {
                    stream: byte_stream,
                    prebuffered,
                },
                start_time,
                metrics,
                active_guard
                    .take()
                    .expect("active_guard must be Some on streaming success path"),
                #[cfg(feature = "db")]
                db_context,
                quota_manager,
                api_key_hash,
            )?;
            // The body now owns the guard; `active_requests` decrements when
            // axum drops the body (client done, disconnect, or error).
            // Token-stat / quota recording still happens inside the spawned
            // drain task — see `handle_streaming_response`.
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

    // Eight parameters — each is a distinct request-scoped concern (upstream
    // stream prep, timing, metrics, RAII guard, optional db context, optional
    // quota manager, api key hash). Bundling them adds boilerplate without
    // cutting call-site complexity.
    #[allow(clippy::too_many_arguments)]
    fn handle_streaming_response(
        &self,
        prepared: PreparedStream,
        start_time: Instant,
        metrics: &MetricsService,
        active_guard: crate::metrics::ActiveRequestGuard,
        #[cfg(feature = "db")] db_context: Option<DbContext>,
        quota_manager: Option<crate::quota::QuotaManager>,
        api_key_hash: Option<String>,
    ) -> Result<Response> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, reqwest::Error>>(64);
        let is_claude = matches!(self.family, LlmFamily::Claude);
        let model = self.model.clone();
        let original_model = self.original_model.clone();
        let provider_name = self.provider_name.clone();
        let family = self.family;
        let metrics = metrics.clone();
        let PreparedStream {
            mut stream,
            prebuffered,
        } = prepared;

        tokio::spawn(async move {
            // Seed `byte_buf` with whatever the peek phase pulled from the
            // upstream stream — those bytes were not consumed destructively
            // and the same line-extraction logic below picks them up first.
            let mut byte_buf: Vec<u8> = prebuffered;
            let mut token_stats = TokenStats::default();
            let mut client_gone = false;
            let mut stream_error = false;
            let chunk_timeout = Duration::from_secs(STREAMING_TIMEOUT_SECS);

            // Drain whatever the peek phase already buffered before pulling
            // any new chunks — otherwise a tiny initial response (rate-limit
            // signals replaced post-peek by a normal event, etc.) could be
            // mistaken for an idle stall.
            loop {
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
                        let bytes = format_sse_event(data, &family, is_claude, &mut token_stats);
                        if tx.send(Ok(bytes)).await.is_err() {
                            tracing::debug!("Client disconnected during streaming");
                            client_gone = true;
                            break;
                        }
                    }
                }
                if client_gone {
                    break;
                }

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
                    }
                    Err(e) => {
                        tracing::error!("Stream error: {}", e);
                        let _ = tx.send(Err(e)).await;
                        stream_error = true;
                        break;
                    }
                }
            }

            // Flush any remaining buffered data — a tail without a trailing
            // newline. Mirrors the main-loop formatting so a final partial
            // Claude event still gets its `event: <type>` prefix.
            if !client_gone
                && !byte_buf.is_empty()
                && let Ok(remaining) = String::from_utf8(byte_buf)
            {
                let line = remaining.trim();
                if let Some(data) = line.strip_prefix(STREAM_DATA_PREFIX)
                    && !data.is_empty()
                {
                    let bytes = format_sse_event(data, &family, is_claude, &mut token_stats);
                    let _ = tx.send(Ok(bytes)).await;
                }
            }

            // Record metrics when streaming is done. `active_requests` is
            // *not* decremented here — that lives with the response body
            // (`active_guard` rides inside `GuardedStream` below) so the
            // counter reflects the body's lifetime, not this drain task's.
            let success = !stream_error;
            let counts = token_stats.to_counts();
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

        let stream = GuardedStream {
            inner: ReceiverStream::new(rx),
            _guard: active_guard,
        };
        let body = Body::from_stream(stream);

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .body(body)?)
    }
}

/// Resolve a client-supplied model name to a configured model name.
///
/// Strips the cosmetic `[1m]` suffix if present (silently accepted as a no-op
/// for backward compat — capability-driven beta injection is what actually
/// enables max context now), then resolves via:
/// 1. Exact match against configured `models[].name`
/// 2. Alias pattern match against configured `models[].aliases`
/// 3. Family-fallback (claude/gemini/gpt/text) to a configured default
/// 4. Pass-through unchanged
fn normalize_model(model: &str, registry: &ModelRegistry) -> Result<String> {
    let base_model = model.strip_suffix(EXTENDED_CONTEXT_SUFFIX).unwrap_or(model);

    // 1. Exact match - if the model exists in config, use it directly
    if registry.find_model_config(base_model).is_some() {
        return Ok(base_model.to_string());
    }

    // 2. Alias pattern match - find by configured alias patterns
    if let Some(matched_model) = registry.find_model_by_alias(base_model) {
        tracing::debug!(
            "Model '{}' matched alias pattern for configured model '{}'",
            base_model,
            matched_model.name
        );
        return Ok(matched_model.name.clone());
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
        // Unknown family, return as-is — `determine_family` will reject it later
        // if it's not in any supported family.
        return Ok(base_model.to_string());
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
        return Ok(fallback_model.to_string());
    }

    Ok(base_model.to_string())
}

/// OpenAI o-series reasoning models: `o1`, `o3`, `o3-mini`, `o4-mini`, future `o5`/etc.
/// Pattern: `o` followed by one or more digits, optionally a `-<variant>` suffix.
static O_SERIES_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^o\d+(-[a-z]+)?$").unwrap());

/// Map a normalized model name to one of the three LLM families acr supports.
///
/// Strict allowlist: Claude (Anthropic via Bedrock), Gemini (Google via Vertex),
/// OpenAI (GPT / o-series / text-embedding via Azure). acr is purpose-built for
/// SAP AI Core's three-family deployment surface; routing requests for other
/// AI Core backends (Mistral, Cohere, Nova, RPT, Perplexity, etc.) is explicitly
/// out of scope — those clients should use the AI Core SDK directly.
fn determine_family(model: &str) -> Result<LlmFamily, AppError> {
    if model.starts_with(CLAUDE_PREFIX) {
        Ok(LlmFamily::Claude)
    } else if model.starts_with(GEMINI_PREFIX) {
        Ok(LlmFamily::Gemini)
    } else if model.starts_with(GPT_PREFIX)
        || model.starts_with(TEXT_PREFIX)
        || O_SERIES_RE.is_match(model)
    {
        Ok(LlmFamily::OpenAi)
    } else {
        Err(AppError::BadRequest(format!(
            "Model '{model}' is not in a family acr supports. acr only routes \
             Claude (Anthropic), Gemini (Google), and OpenAI GPT / o-series / \
             text-embedding-* models. Use the SAP AI Core SDK directly for other \
             backends (Mistral, Cohere, Nova, RPT, Perplexity, etc.)."
        )))
    }
}

fn extract_stream_flag(body: &Value, family: &LlmFamily, action: &Option<String>) -> bool {
    match family {
        LlmFamily::Claude => body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        LlmFamily::Gemini => action.as_deref() == Some(STREAM_GENERATE_CONTENT_ACTION),
        LlmFamily::OpenAi | LlmFamily::OpenAiResponses => body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn prepare_body(body: &mut Value, family: &LlmFamily, stream: bool, model: &str) -> Result<()> {
    match family {
        LlmFamily::Claude => crate::transforms::anthropic::prepare(body, model),
        LlmFamily::Gemini => crate::transforms::gemini::prepare(body),
        LlmFamily::OpenAi => crate::transforms::openai::prepare(body, stream),
        // Responses API: filter `tools[]` to types AI Core / Azure currently
        // accepts (`function`-only allowlist, mirrors what the upstream itself
        // enforces — last verified 2026-05-26 against gpt-5.5) and reset
        // `tool_choice` if it pointed at a dropped tool. Codex CLI v0.130+
        // injects `custom` / `web_search` / `tool_search` / etc. that AI Core
        // 400s on with no client-side flag to suppress.
        // Re-probe AI Core periodically; if newer types become accepted,
        // broaden `ALLOWED_TOOL_TYPES` in `transforms::openai_responses`.
        LlmFamily::OpenAiResponses => crate::transforms::openai_responses::prepare(body),
    }
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

/// Extract OpenAI Responses-API token stats from a `usage` JSON object.
/// Field names differ from Chat Completions: `input_tokens` / `output_tokens` /
/// `input_tokens_details.cached_tokens`. The Responses API has no cache-write
/// concept; `output_tokens_details.reasoning_tokens` is not currently tracked.
fn extract_responses_tokens(usage: &Value) -> TokenStats {
    TokenStats {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        cache_read: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        cache_write: None,
    }
}

/// Extract Gemini token stats from a `usageMetadata` JSON object.
/// In streaming mode, input_tokens is required (returns None if absent).
/// In non-streaming mode, input_tokens is optional; output returns None if zero.
fn extract_gemini_tokens(usage_metadata: &Value, streaming: bool) -> Option<TokenStats> {
    let input_tokens = usage_metadata
        .get("promptTokenCount")
        .and_then(|v| v.as_u64());
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

/// Outcome of [`peek_classify_stream`] — describes what we learned from
/// reading the upstream's first SSE chunks before committing to forwarding
/// the response.
enum PeekOutcome {
    /// First parseable `data:` line was a normal event — proceed with the
    /// existing forwarder.
    Committed,
    /// First parseable `data:` line signalled a rate-limit / throttling
    /// failure. Caller should surface this as
    /// [`ProxyExecuteResult::RateLimited`] so the existing retry loop in
    /// `routes::execute_proxy_request` fails over to the next provider.
    RateLimited,
    /// Upstream stream ended before any `data:` line arrived. Proceed with
    /// the forwarder anyway — it'll exit cleanly.
    StreamEnded,
    /// Peek window elapsed before any `data:` line arrived. Proceed with
    /// the forwarder; the per-chunk timeout inside the forwarder
    /// (`STREAMING_TIMEOUT_SECS`) is the real watchdog.
    PeekTimeout,
    /// Transport-level error reading from the upstream stream.
    Transport(reqwest::Error),
}

/// Bundle of (remaining stream, bytes consumed during the peek) that the
/// streaming forwarder needs to resume processing where the peek phase
/// left off. They always travel together, so packaging them avoids the
/// `too_many_arguments` lint on `handle_streaming_response`.
struct PreparedStream {
    stream: futures::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
    prebuffered: Vec<u8>,
}

/// Wraps the per-request response stream so that the `ActiveRequestGuard`
/// rides along with the body. When axum drops the body — normal completion,
/// client disconnect, or hyper-side error — the guard's `Drop` decrements
/// `active_requests`. This decouples "is the request still in flight" from
/// "is the spawned upstream-drain task still running."
struct GuardedStream<S> {
    inner: S,
    _guard: crate::metrics::ActiveRequestGuard,
}

impl<S, T, E> futures::Stream for GuardedStream<S>
where
    S: futures::Stream<Item = std::result::Result<T, E>> + Unpin,
{
    type Item = std::result::Result<T, E>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Read upstream chunks until we either see a `data:` line we can classify
/// (committing the stream forward to the client) or recognise a rate-limit
/// signal that warrants a provider fallback. The buffered bytes (everything
/// pulled from the stream during the peek) are returned so the caller can
/// hand them to the forwarder, which re-parses them as-if it had read them
/// itself.
async fn peek_classify_stream(
    stream: &mut futures::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
    family: &LlmFamily,
    timeout: Duration,
) -> (PeekOutcome, Vec<u8>) {
    use crate::transforms::stream_classify::{EventDisposition, classify_first_event};
    let mut buf: Vec<u8> = Vec::new();
    // Position in `buf` where the next non-destructive line scan resumes —
    // we re-walk only newly-arrived bytes, never the prefix the forwarder
    // is going to re-process.
    let mut scan_cursor = 0usize;
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (PeekOutcome::PeekTimeout, buf);
        }
        let next = match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(e))) => return (PeekOutcome::Transport(e), buf),
            Ok(None) => return (PeekOutcome::StreamEnded, buf),
            Err(_) => return (PeekOutcome::PeekTimeout, buf),
        };
        buf.extend_from_slice(&next);

        // Walk all complete lines newly visible in `buf` (non-destructive
        // — bytes stay in `buf` for the forwarder).
        while let Some(rel) = buf[scan_cursor..].iter().position(|&b| b == b'\n') {
            let end = scan_cursor + rel;
            let line_bytes = &buf[scan_cursor..end];
            scan_cursor = end + 1;
            let Ok(line) = std::str::from_utf8(line_bytes) else {
                continue;
            };
            let line = line.trim();
            let Some(data) = line.strip_prefix(STREAM_DATA_PREFIX) else {
                continue;
            };
            if data.is_empty() {
                continue;
            }
            match classify_first_event(data, family) {
                EventDisposition::RateLimited => return (PeekOutcome::RateLimited, buf),
                EventDisposition::Content => return (PeekOutcome::Committed, buf),
                EventDisposition::Metadata => continue,
            }
        }
    }
}

/// Format a single SSE `data:` payload for the downstream client. Updates
/// `token_stats` in place when the payload carries usage. For Claude, prefixes
/// the formatted output with an explicit `event: <type>` line so SSE clients
/// that key off named events (rather than parsing JSON) see the right event
/// type — the upstream Bedrock invoke-with-response-stream encoding only
/// embeds the type as a JSON field.
fn format_sse_event(
    data: &str,
    family: &LlmFamily,
    is_claude: bool,
    token_stats: &mut TokenStats,
) -> axum::body::Bytes {
    if let Some(stats) = extract_token_stats(data, family) {
        *token_stats = stats;
    }

    let mut output = String::new();
    if is_claude
        && let Ok(parsed) = serde_json::from_str::<Value>(data)
        && let Some(event_type) = parsed.get("type").and_then(|v| v.as_str())
    {
        output.push_str(&format!("event: {event_type}\n"));
    }
    output.push_str(&format!("{STREAM_DATA_PREFIX}{data}\n\n"));
    axum::body::Bytes::from(output)
}

/// Extract token usage from a single SSE `data:` payload. Public so e2e tests
/// can reuse the same field-name logic when asserting that streamed
/// responses carry usage on their terminal event.
pub fn extract_token_stats(data: &str, family: &LlmFamily) -> Option<TokenStats> {
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
        LlmFamily::OpenAiResponses => {
            // Responses API streams a sequence of `data: {"type": "...", ...}` events.
            // Usage appears on the terminal event regardless of completion status:
            // - `response.completed` — happy path
            // - `response.incomplete` — stream ended before natural completion
            //   (e.g., max_output_tokens hit, content_filter, or other truncation
            //   reasons; usage is still populated)
            // - `response.failed` — upstream error after stream started; usage is
            //   still emitted with whatever was consumed before the failure
            //
            // Earlier events (response.created, response.in_progress,
            // response.output_item.added, response.output_text.delta, etc.)
            // carry no usage. Cross-validated against LiteLLM's
            // `responses/streaming_iterator.py` terminal-event handling.
            let event_type = parsed.get("type")?.as_str()?;
            if !matches!(
                event_type,
                "response.completed" | "response.incomplete" | "response.failed"
            ) {
                return None;
            }
            let usage = parsed.get("response")?.get("usage")?;
            Some(extract_responses_tokens(usage))
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
        LlmFamily::OpenAiResponses => {
            let usage = parsed.get("usage")?;
            Some(extract_responses_tokens(usage))
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
        LlmFamily::OpenAiResponses => {
            // `action == Some("compact")` selects the compaction subpath, used by
            // Codex CLI's auto-compact-remote feature. The compact endpoint is
            // unary (never streamed) but uses the same body+response shape as the
            // create endpoint, so token extraction is identical.
            let path = if action.as_deref() == Some("compact") {
                RESPONSES_COMPACT_PATH
            } else {
                RESPONSES_PATH
            };
            Ok(format!(
                "{base_url}{INFERENCE_DEPLOYMENTS_PATH}/{deployment_id}{path}?api-version={openai_api_version}"
            ))
        }
    }
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
        // The `[1m]` suffix is silently stripped (no error, no flag returned).
        // Capability-driven beta injection is what actually enables max context.
        let models = vec![Model {
            name: "claude-opus-4-7".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let name = normalize_model("claude-opus-4-7[1m]", &registry).unwrap();
        assert_eq!(name, "claude-opus-4-7");
    }

    #[test]
    fn test_normalize_model_without_1m_suffix() {
        let models = vec![Model {
            name: "claude-opus-4-7".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let name = normalize_model("claude-opus-4-7", &registry).unwrap();
        assert_eq!(name, "claude-opus-4-7");
    }

    #[test]
    fn test_normalize_model_1m_suffix_with_alias_resolution() {
        let models = vec![Model {
            name: "claude-opus-4-7".to_string(),
            aicore_model_name: None,
            aliases: vec!["claude-opus-4-7-*".to_string()],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        // Alias match with [1m] suffix — suffix stripped, then alias resolves.
        let name = normalize_model("claude-opus-4-7-20250101[1m]", &registry).unwrap();
        assert_eq!(name, "claude-opus-4-7");
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

        // Unknown claude model with [1m] should strip the suffix and fall back.
        let name = normalize_model("claude-unknown-model[1m]", &registry).unwrap();
        assert_eq!(name, "claude-sonnet-4-5");
    }

    #[test]
    fn test_normalize_model_1m_suffix_non_claude() {
        // The suffix-strip is universal (not Claude-only); a non-Claude model name
        // with `[1m]` is still cleaned. The suffix is a no-op for these families.
        let models = vec![Model {
            name: "gpt-4o".to_string(),
            aicore_model_name: None,
            aliases: vec![],
            pricing: None,
        }];
        let registry = create_test_registry(models);

        let name = normalize_model("gpt-4o[1m]", &registry).unwrap();
        assert_eq!(name, "gpt-4o");
    }

    // -------------------------------------------------------------------------
    // determine_family — strict allowlist
    // -------------------------------------------------------------------------

    #[test]
    fn determine_family_routes_known_prefixes() {
        assert!(matches!(
            determine_family("claude-sonnet-4-6").unwrap(),
            LlmFamily::Claude
        ));
        assert!(matches!(
            determine_family("gemini-2.5-pro").unwrap(),
            LlmFamily::Gemini
        ));
        assert!(matches!(
            determine_family("gpt-5.4").unwrap(),
            LlmFamily::OpenAi
        ));
        assert!(matches!(
            determine_family("text-embedding-3-small").unwrap(),
            LlmFamily::OpenAi
        ));
    }

    #[test]
    fn determine_family_o_series_via_regex() {
        for model in ["o1", "o3", "o3-mini", "o4-mini", "o5", "o6-preview"] {
            assert!(
                matches!(determine_family(model).unwrap(), LlmFamily::OpenAi),
                "{model} should route to OpenAi via o-series regex"
            );
        }
    }

    #[test]
    fn determine_family_rejects_unsupported_backends() {
        for model in [
            "nova-lite",
            "amazon--nova-pro",
            "mistralai--mistral-large-instruct",
            "cohere--command-a-reasoning",
            "sap-rpt-1-large",
            "sonar-pro",
        ] {
            let err = determine_family(model).unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{model} should be rejected with BadRequest, got {err:?}"
            );
        }
    }

    // -------------------------------------------------------------------------
    // OpenAI Responses API (`/v1/responses`)
    // -------------------------------------------------------------------------

    #[test]
    fn extract_responses_tokens_reads_responses_field_names() {
        // Empirical shape from a live AI Core probe against gpt-5.4.
        let usage = json!({
            "input_tokens": 8,
            "input_tokens_details": { "cached_tokens": 3 },
            "output_tokens": 5,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": 13
        });
        let stats = extract_responses_tokens(&usage);
        assert_eq!(stats.input_tokens, Some(8));
        assert_eq!(stats.output_tokens, Some(5));
        assert_eq!(stats.cache_read, Some(3));
        assert_eq!(stats.cache_write, None);
    }

    #[test]
    fn extract_token_stats_responses_completed_event_yields_usage() {
        let event = r#"{
            "type": "response.completed",
            "response": {
                "usage": {
                    "input_tokens": 8,
                    "output_tokens": 5,
                    "input_tokens_details": { "cached_tokens": 0 }
                }
            }
        }"#;
        let stats = extract_token_stats(event, &LlmFamily::OpenAiResponses).unwrap();
        assert_eq!(stats.input_tokens, Some(8));
        assert_eq!(stats.output_tokens, Some(5));
    }

    #[test]
    fn extract_token_stats_responses_incomplete_and_failed_also_yield_usage() {
        // `response.incomplete` (e.g., max_output_tokens hit) and `response.failed`
        // (upstream error) both carry usage. Verified live against AI Core: a stream
        // with max_output_tokens=16 terminated as `response.incomplete` with
        // usage = {input_tokens: 12, output_tokens: 16}. Pre-fix acr logged N/A.
        for terminal_type in ["response.incomplete", "response.failed"] {
            let event = format!(
                r#"{{
                    "type": "{terminal_type}",
                    "response": {{
                        "usage": {{
                            "input_tokens": 12,
                            "output_tokens": 16,
                            "input_tokens_details": {{ "cached_tokens": 0 }}
                        }}
                    }}
                }}"#
            );
            let stats = extract_token_stats(&event, &LlmFamily::OpenAiResponses)
                .unwrap_or_else(|| panic!("usage should extract from {terminal_type}"));
            assert_eq!(stats.input_tokens, Some(12));
            assert_eq!(stats.output_tokens, Some(16));
        }
    }

    #[test]
    fn extract_token_stats_responses_non_completed_events_yield_none() {
        // Earlier events in the stream don't carry usage.
        for event_type in [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.output_text.delta",
        ] {
            let event = format!(r#"{{"type":"{event_type}","response":{{}}}}"#);
            assert!(
                extract_token_stats(&event, &LlmFamily::OpenAiResponses).is_none(),
                "{event_type} should not produce token stats"
            );
        }
    }

    #[test]
    fn extract_token_stats_from_body_responses() {
        // Non-streaming response — usage is at the top level.
        let body = r#"{
            "id": "resp_…",
            "status": "completed",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7,
                "input_tokens_details": { "cached_tokens": 4 }
            }
        }"#;
        let stats = extract_token_stats_from_body(body, &LlmFamily::OpenAiResponses).unwrap();
        assert_eq!(stats.input_tokens, Some(12));
        assert_eq!(stats.output_tokens, Some(7));
        assert_eq!(stats.cache_read, Some(4));
    }

    #[test]
    fn build_url_routes_responses_to_responses_endpoint() {
        let url = build_url(
            "gpt-5.4",
            "dccbb05e08654c63",
            &None,
            "https://api.example.com",
            &LlmFamily::OpenAiResponses,
            false, // stream flag is irrelevant for OpenAI URL building
            "2025-04-01-preview",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://api.example.com/v2/inference/deployments/dccbb05e08654c63/responses?api-version=2025-04-01-preview"
        );
    }

    #[test]
    fn build_url_responses_url_independent_of_streaming() {
        // OpenAI Responses uses the same URL for streaming vs non-streaming
        // (the request body's `"stream": true` is what triggers SSE).
        let url_stream = build_url(
            "gpt-5.4",
            "d1",
            &None,
            "https://x",
            &LlmFamily::OpenAiResponses,
            true,
            "2025-04-01-preview",
        )
        .unwrap();
        let url_nostream = build_url(
            "gpt-5.4",
            "d1",
            &None,
            "https://x",
            &LlmFamily::OpenAiResponses,
            false,
            "2025-04-01-preview",
        )
        .unwrap();
        assert_eq!(url_stream, url_nostream);
    }

    #[test]
    fn build_url_responses_with_compact_action_targets_compact_subpath() {
        let url = build_url(
            "gpt-5.4",
            "dccbb05e08654c63",
            &Some("compact".to_string()),
            "https://api.example.com",
            &LlmFamily::OpenAiResponses,
            false,
            "2025-04-01-preview",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://api.example.com/v2/inference/deployments/dccbb05e08654c63/responses/compact?api-version=2025-04-01-preview"
        );
    }

    #[test]
    fn build_url_responses_with_unknown_action_falls_back_to_create() {
        // Defensive: only the literal "compact" routes to the compact subpath;
        // any other action string defaults to the create endpoint.
        let url = build_url(
            "gpt-5.4",
            "d1",
            &Some("not-a-real-action".to_string()),
            "https://x",
            &LlmFamily::OpenAiResponses,
            false,
            "2025-04-01-preview",
        )
        .unwrap();
        assert!(url.ends_with("/responses?api-version=2025-04-01-preview"));
        assert!(!url.contains("/compact"));
    }

    /// Build a synthetic `BoxStream` from a list of pre-baked chunks for
    /// driving `peek_classify_stream` in tests. Each chunk is delivered as
    /// `Ok(Bytes)`; no transport errors are simulated.
    fn synthetic_stream(
        chunks: Vec<&'static str>,
    ) -> futures::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>> {
        let items: Vec<reqwest::Result<axum::body::Bytes>> = chunks
            .into_iter()
            .map(|c| Ok(axum::body::Bytes::from(c)))
            .collect();
        Box::pin(futures::stream::iter(items))
    }

    #[tokio::test]
    async fn peek_flags_responses_rate_limit_event() {
        let mut s = synthetic_stream(vec![
            "data: {\"type\":\"error\",\"error\":{\"type\":\"too_many_requests\",\"code\":\"too_many_requests\"}}\n\n",
        ]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::OpenAiResponses, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::RateLimited));
    }

    #[tokio::test]
    async fn peek_commits_on_normal_responses_event() {
        // A content-bearing event (`.delta`) should commit immediately —
        // no need to keep peeking past it.
        let mut s = synthetic_stream(vec![
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        ]);
        let (outcome, buf) =
            peek_classify_stream(&mut s, &LlmFamily::OpenAiResponses, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::Committed));
        // Bytes are preserved so the forwarder can reprocess them.
        assert!(
            std::str::from_utf8(&buf)
                .unwrap()
                .contains("output_text.delta")
        );
    }

    #[tokio::test]
    async fn peek_skips_metadata_and_rate_limits_on_trailing_error() {
        // Captured wire order from gpt-5.5 throttling:
        // response.created -> response.failed -> error{too_many_requests}.
        // Peek must skip the first two and surface the rate-limit signal
        // from the trailing `error` event so the proxy fails over before
        // any bytes reach the client.
        let mut s = synthetic_stream(vec![
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r_1\"}}\n\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":null}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"too_many_requests\",\"code\":\"too_many_requests\"}}\n\n",
        ]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::OpenAiResponses, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::RateLimited));
    }

    #[tokio::test]
    async fn peek_skips_metadata_and_commits_on_content() {
        // Healthy stream: metadata events followed by a content delta.
        let mut s = synthetic_stream(vec![
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r_1\"}}\n\n",
            "data: {\"type\":\"response.in_progress\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        ]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::OpenAiResponses, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::Committed));
    }

    #[tokio::test]
    async fn peek_handles_split_chunks() {
        // Rate-limit JSON split mid-token across two chunks.
        let mut s = synthetic_stream(vec![
            "data: {\"type\":\"error\",\"error\":{\"type\":\"too_many_",
            "requests\",\"code\":\"too_many_requests\"}}\n",
        ]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::OpenAiResponses, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::RateLimited));
    }

    #[tokio::test]
    async fn peek_skips_blank_and_comment_lines_before_data() {
        // SSE keepalives / comments are common; peek should walk past them
        // and eventually classify a real `data:` payload.
        let mut s = synthetic_stream(vec![
            ": keepalive\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        ]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::OpenAiResponses, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::Committed));
    }

    #[tokio::test]
    async fn peek_returns_stream_ended_on_empty_upstream() {
        let mut s = synthetic_stream(vec![]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::OpenAiResponses, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::StreamEnded));
    }

    #[tokio::test]
    async fn peek_flags_claude_throttling() {
        let mut s = synthetic_stream(vec![
            "data: {\"type\":\"error\",\"message\":\"ThrottlingException: Rate exceeded\"}\n\n",
        ]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::Claude, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::RateLimited));
    }

    #[tokio::test]
    async fn peek_flags_gemini_resource_exhausted() {
        let mut s = synthetic_stream(vec![
            "data: {\"error\":{\"code\":429,\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n",
        ]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::Gemini, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::RateLimited));
    }

    #[tokio::test]
    async fn peek_flags_openai_chat_rate_limit() {
        let mut s = synthetic_stream(vec![
            "data: {\"error\":{\"code\":\"rate_limit_exceeded\"}}\n\n",
        ]);
        let (outcome, _buf) =
            peek_classify_stream(&mut s, &LlmFamily::OpenAi, Duration::from_secs(2)).await;
        assert!(matches!(outcome, PeekOutcome::RateLimited));
    }

    #[tokio::test]
    async fn guarded_stream_drops_guard_when_consumed_to_end() {
        use crate::metrics::{ActiveRequestGuard, MetricsService};
        use futures::StreamExt;

        let metrics = MetricsService::new();
        let guard = ActiveRequestGuard::new(&metrics);
        assert_eq!(metrics.snapshot_sync().active_requests, 1);

        let inner = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"a")),
            Ok(axum::body::Bytes::from_static(b"b")),
        ]);
        let mut wrapped = GuardedStream {
            inner,
            _guard: guard,
        };
        while wrapped.next().await.is_some() {}
        // Stream consumed but wrapper still alive.
        assert_eq!(metrics.snapshot_sync().active_requests, 1);
        drop(wrapped);
        assert_eq!(metrics.snapshot_sync().active_requests, 0);
    }

    #[tokio::test]
    async fn guarded_stream_drops_guard_on_early_drop() {
        use crate::metrics::{ActiveRequestGuard, MetricsService};

        let metrics = MetricsService::new();
        let guard = ActiveRequestGuard::new(&metrics);
        assert_eq!(metrics.snapshot_sync().active_requests, 1);

        let inner = futures::stream::iter(vec![Ok::<_, std::io::Error>(
            axum::body::Bytes::from_static(b"a"),
        )]);
        let wrapped = GuardedStream {
            inner,
            _guard: guard,
        };
        // Simulate axum dropping the body before draining (e.g. client gone).
        drop(wrapped);
        assert_eq!(metrics.snapshot_sync().active_requests, 0);
    }
}
