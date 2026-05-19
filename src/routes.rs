use axum::{
    Router,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use thiserror::Error;

use crate::{
    balancer::LoadBalancer,
    config::Config,
    metrics::MetricsService,
    proxy::{ProxyExecuteResult, ProxyRequestBuilder, ProxyRequestParams, extract_api_key},
    quota::{QuotaCheckResult, QuotaManager},
    rate_limit::AuthRateLimiter,
    registry::ModelRegistry,
    token::TokenManager,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub model_registry: ModelRegistry,
    pub token_manager: TokenManager,
    pub load_balancer: LoadBalancer,
    pub client: reqwest::Client,
    pub metrics: MetricsService,
    #[cfg(feature = "db")]
    pub database: Option<crate::database::Database>,
    pub rate_limiter: AuthRateLimiter,
    pub quota_manager: Option<QuotaManager>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/v1/models", get(get_models))
        .route("/v1/chat/completions", post(handle_openai_chat))
        .route("/litellm/v1/chat/completions", post(handle_openai_chat))
        .route(
            "/openai/deployments/{model}/chat/completions",
            post(handle_azure_openai),
        )
        .route(
            "/openai/deployments/{model}/embedding",
            post(handle_azure_openai),
        )
        .route("/v1/messages", post(handle_claude_messages))
        .route("/anthropic/v1/messages", post(handle_claude_messages))
        .route(
            "/gemini/models/{model_operation}",
            post(handle_gemini_models),
        )
        .route(
            "/gemini/v1beta/models/{model_operation}",
            post(handle_gemini_models),
        )
        .route(
            "/v1beta/models/{model_operation}",
            post(handle_gemini_models),
        )
        .with_state(state)
}

pub async fn health_check() -> impl IntoResponse {
    "OK"
}

fn extract_model_from_body(body: &Value) -> Result<String, AppError> {
    body.get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::BadRequest("model is required".to_string()))
}

fn ensure_model_in_body(body: &mut Value, model: &str) {
    if let Some(obj) = body.as_object_mut()
        && !obj.contains_key("model")
    {
        obj.insert("model".to_string(), json!(model));
    }
}

fn parse_model_operation(model_operation: &str) -> Result<(String, String), AppError> {
    match model_operation.split_once(':') {
        Some((model, action))
            if !model.is_empty() && !action.is_empty() && !action.contains(':') =>
        {
            Ok((model.to_string(), action.to_string()))
        }
        _ => Err(AppError::BadRequest(
            "Invalid model operation format. Expected 'model:action'".to_string(),
        )),
    }
}

/// Record a failed request (decrement active count and log failure metrics).
async fn record_failure_metrics(metrics: &MetricsService) {
    metrics.decrement_active();
    metrics
        .record_completion(false, None, &crate::metrics::TokenCounts::default())
        .await;
}

#[cfg_attr(not(feature = "db"), allow(unused_variables))]
async fn execute_proxy_request(
    state: &AppState,
    headers: &HeaderMap,
    body: Value,
    model: &str,
    action: Option<String>,
    client_ip: &str,
    request_path: &str,
) -> Result<Response, AppError> {
    // Check rate limiting before processing
    if let Some(remaining) = state.rate_limiter.is_rate_limited(client_ip).await {
        return Err(AppError::RateLimitedAuth {
            retry_after_secs: remaining.as_secs(),
        });
    }

    // Reject the "internal" key from non-loopback IPs
    let request_api_key = extract_api_key(headers);
    if let Some(ref key) = request_api_key
        && key == "internal"
    {
        let ip: std::net::IpAddr = client_ip.parse().unwrap_or([0, 0, 0, 0].into());
        if !ip.is_loopback() {
            return Err(AppError::InvalidApiKey);
        }
    }

    // Pre-compute API key hash once for quota checks, DB logging, and usage recording
    let api_key_hash = request_api_key.as_ref().map(|k| crate::quota::hash_api_key(k));

    // Check token quota before processing
    if let Some(ref qm) = state.quota_manager
        && let Some(ref kh) = api_key_hash
    {
        match qm.check_quota_hashed(kh).await {
            QuotaCheckResult::Exceeded {
                retry_after_secs,
                limit_type,
            } => {
                return Err(AppError::QuotaExceeded {
                    retry_after_secs,
                    limit_type,
                });
            }
            QuotaCheckResult::Allowed { .. } => {}
        }
    }

    state.metrics.increment_active();

    let params = ProxyRequestParams {
        headers,
        method: Method::POST,
        body,
        model: model.to_string(),
        action,
        config: &state.config,
        token_manager: &state.token_manager,
        model_registry: &state.model_registry,
        load_balancer: &state.load_balancer,
    };

    let builder = ProxyRequestBuilder::new(params);

    // Get providers in round-robin order with fallback
    let providers = state.load_balancer.get_ordered_providers();
    if providers.len() == 0 {
        record_failure_metrics(&state.metrics).await;
        return Err(AppError::Internal(anyhow::anyhow!(
            "No providers available"
        )));
    }

    let mut last_error: Option<AppError> = None;

    // Try each provider in order until one succeeds or all are exhausted
    for (i, provider) in providers.enumerate() {
        // Try to build the request for this provider
        let proxy = match builder.build_for_provider(provider).await {
            Ok(proxy) => proxy,
            Err(AppError::ModelNotAvailableOnProvider { model, provider }) => {
                tracing::debug!(
                    "Model '{}' not available on provider '{}', trying next",
                    model,
                    provider
                );
                last_error = Some(AppError::ModelNotAvailableOnProvider { model, provider });
                continue;
            }
            Err(AppError::InvalidApiKey) => {
                // Record auth failure for rate limiting
                state.rate_limiter.record_failure(client_ip).await;
                record_failure_metrics(&state.metrics).await;
                return Err(AppError::InvalidApiKey);
            }
            Err(e) => {
                // Non-recoverable error (auth failure, etc.)
                record_failure_metrics(&state.metrics).await;
                return Err(e);
            }
        };

        #[cfg(feature = "db")]
        let db_context = {
            state.database.as_ref().map(|db| crate::proxy::DbContext {
                database: db.clone(),
                request_path: request_path.to_string(),
                api_key_hash: api_key_hash.clone(),
            })
        };

        // Execute the request
        #[cfg(feature = "db")]
        let start_time = std::time::Instant::now();
        match proxy
            .execute(
                &state.client,
                &state.metrics,
                #[cfg(feature = "db")]
                db_context,
                state.quota_manager.clone(),
                api_key_hash.clone(),
            )
            .await
        {
            Ok(ProxyExecuteResult::Response {
                response,
                token_stats,
            }) => {
                let is_success = response.status().is_success();

                // Record successful auth only after a successful response
                if is_success {
                    state.rate_limiter.record_success(client_ip).await;
                }
                if i > 0 && is_success {
                    tracing::info!(
                        "Request succeeded on provider '{}' after {} fallback(s)",
                        provider.name,
                        i
                    );
                }

                // For non-streaming responses, record metrics now.
                // Streaming responses record metrics when the stream completes,
                // UNLESS the response is an error (no streaming task was spawned).
                if !proxy.stream || !is_success {
                    let counts = token_stats.to_counts();
                    state.metrics.decrement_active();
                    state
                        .metrics
                        .record_completion(is_success, Some(&proxy.model), &counts)
                        .await;

                    // Log request to database
                    #[cfg(feature = "db")]
                    if let Some(ref db) = state.database {
                        let elapsed = start_time.elapsed();
                        let response_status = response.status().as_u16();
                        let record = crate::database::RequestRecord::new(
                            request_path.to_string(),
                            proxy.model.clone(),
                            proxy.provider_name.clone(),
                            elapsed,
                            response_status,
                            false,
                            &token_stats,
                            api_key_hash.clone(),
                        );
                        let db = db.clone();
                        tokio::spawn(async move {
                            if let Err(e) = db.insert_request(record).await {
                                tracing::warn!("Failed to log request to database: {}", e);
                            }
                        });
                    }

                    // Record quota usage for non-streaming responses
                    if let Some(ref qm) = state.quota_manager
                        && let Some(ref kh) = api_key_hash
                    {
                        qm.record_usage_hashed(kh, &counts).await;
                    }
                }

                return Ok(response);
            }
            Ok(ProxyExecuteResult::RateLimited) => {
                tracing::warn!(
                    "Provider '{}' returned 429, trying next provider",
                    provider.name
                );
                last_error = Some(AppError::RateLimited(provider.name.clone()));
                continue;
            }
            Err(e) => {
                // Request failed, try next provider
                tracing::error!(
                    "Request failed on provider '{}': {}, trying next",
                    provider.name,
                    e
                );
                last_error = Some(AppError::Internal(e));
                continue;
            }
        }
    }

    // All providers exhausted
    record_failure_metrics(&state.metrics).await;
    match last_error {
        Some(AppError::RateLimited(_)) => Err(AppError::AllProvidersRateLimited),
        Some(e) => Err(e),
        None => Err(AppError::Internal(anyhow::anyhow!(
            "No providers could handle the request"
        ))),
    }
}

pub async fn get_models(State(state): State<AppState>) -> impl IntoResponse {
    use crate::constants::get_context_length;

    let model_names = state.model_registry.get_available_models().await;

    let model_data: Vec<serde_json::Value> = model_names
        .into_iter()
        .map(|model_name| {
            let mut obj = serde_json::Map::new();
            obj.insert("id".into(), json!(model_name));
            obj.insert("object".into(), json!("model"));
            if let Some(ctx_len) = get_context_length(&model_name) {
                obj.insert("context_length".into(), json!(ctx_len));
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    let models = json!({
        "object": "list",
        "data": model_data
    });
    Json(models)
}

pub async fn handle_openai_chat(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let model = extract_model_from_body(&body)?;
    let client_ip = addr.ip().to_string();
    execute_proxy_request(
        &state,
        &headers,
        body,
        &model,
        None,
        &client_ip,
        "/v1/chat/completions",
    )
    .await
}

pub async fn handle_azure_openai(
    State(state): State<AppState>,
    Path(model): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Result<Response, AppError> {
    ensure_model_in_body(&mut body, &model);
    let model = extract_model_from_body(&body)?;
    let client_ip = addr.ip().to_string();
    execute_proxy_request(
        &state,
        &headers,
        body,
        &model,
        None,
        &client_ip,
        "/openai/deployments",
    )
    .await
}

pub async fn handle_claude_messages(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let model = extract_model_from_body(&body)?;
    let client_ip = addr.ip().to_string();
    execute_proxy_request(
        &state,
        &headers,
        body,
        &model,
        None,
        &client_ip,
        "/v1/messages",
    )
    .await
}

pub async fn handle_gemini_models(
    State(state): State<AppState>,
    Path(model_operation): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let (model, action) = parse_model_operation(&model_operation)?;
    let client_ip = addr.ip().to_string();
    execute_proxy_request(
        &state,
        &headers,
        body,
        &model,
        Some(action),
        &client_ip,
        "/gemini/models",
    )
    .await
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("API key not found in headers")]
    MissingApiKey,
    #[error("Invalid API key")]
    InvalidApiKey,
    #[error("Model '{model}' not available on provider '{provider}'")]
    ModelNotAvailableOnProvider { model: String, provider: String },
    #[error("Rate limited by provider: {0}")]
    RateLimited(String),
    #[error("All providers are rate limited")]
    AllProvidersRateLimited,
    #[error("Too many failed authentication attempts")]
    RateLimitedAuth { retry_after_secs: u64 },
    #[error("Token quota exceeded ({limit_type} limit)")]
    QuotaExceeded {
        retry_after_secs: u64,
        limit_type: crate::quota::LimitType,
    },
    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::MissingApiKey => (
                StatusCode::UNAUTHORIZED,
                "API key not found in headers".to_string(),
            ),
            AppError::InvalidApiKey => (StatusCode::UNAUTHORIZED, "Invalid API key".to_string()),
            AppError::ModelNotAvailableOnProvider { model, provider } => (
                StatusCode::BAD_REQUEST,
                format!("Model '{}' not available on provider '{}'", model, provider),
            ),
            AppError::RateLimited(provider) => (
                StatusCode::TOO_MANY_REQUESTS,
                format!("Rate limited by provider: {}", provider),
            ),
            AppError::AllProvidersRateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "All providers are rate limited. Please try again later.".to_string(),
            ),
            AppError::RateLimitedAuth { retry_after_secs } => (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Too many failed authentication attempts. Retry after {} seconds.",
                    retry_after_secs
                ),
            ),
            AppError::QuotaExceeded {
                retry_after_secs,
                limit_type,
            } => (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Token quota exceeded ({} limit). Retry after {} seconds.",
                    limit_type, retry_after_secs
                ),
            ),
            AppError::Internal(err) => {
                tracing::error!("Internal error: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
        };

        let mut response = (status, Json(json!({ "error": message }))).into_response();

        if let AppError::RateLimitedAuth { retry_after_secs } = &self
            && let Ok(val) = axum::http::HeaderValue::from_str(&retry_after_secs.to_string())
        {
            response.headers_mut().insert("retry-after", val);
        }

        if let AppError::QuotaExceeded {
            retry_after_secs, ..
        } = &self
            && let Ok(val) = axum::http::HeaderValue::from_str(&retry_after_secs.to_string())
        {
            response.headers_mut().insert("retry-after", val);
        }

        response
    }
}
