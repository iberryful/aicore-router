use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    balancer::LoadBalancer,
    config::Config,
    proxy::{ProxyExecuteResult, ProxyRequestBuilder, ProxyRequestParams},
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
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/v1/models", get(get_models))
        .route("/v1/chat/completions", post(handle_openai_chat))
        .route(
            "/openai/deployments/{model}/chat/completions",
            post(handle_azure_openai),
        )
        .route(
            "/openai/deployments/{model}/embedding",
            post(handle_azure_openai),
        )
        .route("/v1/messages", post(handle_claude_messages))
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
    let parts: Vec<&str> = model_operation.split(':').collect();
    if parts.len() != 2 {
        return Err(AppError::BadRequest(
            "Invalid model operation format. Expected 'model:action'".to_string(),
        ));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

async fn execute_proxy_request(
    state: &AppState,
    headers: &HeaderMap,
    body: Value,
    model: &str,
    action: Option<String>,
) -> Result<Response, AppError> {
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
    if providers.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "No providers available"
        )));
    }

    let mut last_error: Option<AppError> = None;

    // Try each provider in order until one succeeds or all are exhausted
    for (i, provider) in providers.iter().enumerate() {
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
            Err(e) => {
                // Non-recoverable error (auth failure, etc.)
                return Err(e);
            }
        };

        // Execute the request
        match proxy.execute(&state.client, &state.config).await {
            Ok(ProxyExecuteResult::Response(response)) => {
                if i > 0 {
                    tracing::info!(
                        "Request succeeded on provider '{}' after {} fallback(s)",
                        provider.name,
                        i
                    );
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
    match last_error {
        Some(AppError::RateLimited(_)) => Err(AppError::AllProvidersRateLimited),
        Some(e) => Err(e),
        None => Err(AppError::Internal(anyhow::anyhow!(
            "No providers could handle the request"
        ))),
    }
}

pub async fn get_models(State(state): State<AppState>) -> impl IntoResponse {
    let model_names = state.model_registry.get_available_models().await;

    let model_data: Vec<serde_json::Value> = model_names
        .into_iter()
        .map(|model_name| {
            json!({
                "id": model_name,
                "object": "model"
            })
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
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let model = extract_model_from_body(&body)?;
    execute_proxy_request(&state, &headers, body, &model, None).await
}

pub async fn handle_azure_openai(
    State(state): State<AppState>,
    Path(model): Path<String>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Result<Response, AppError> {
    ensure_model_in_body(&mut body, &model);
    let model = extract_model_from_body(&body)?;
    execute_proxy_request(&state, &headers, body, &model, None).await
}

pub async fn handle_claude_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let model = extract_model_from_body(&body)?;
    execute_proxy_request(&state, &headers, body, &model, None).await
}

pub async fn handle_gemini_models(
    State(state): State<AppState>,
    Path(model_operation): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let (model, action) = parse_model_operation(&model_operation)?;
    execute_proxy_request(&state, &headers, body, &model, Some(action)).await
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
    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
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
            AppError::Internal(err) => {
                tracing::error!("Internal error: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
        };

        let body = json!({
            "error": message
        });

        (status, Json(body)).into_response()
    }
}
