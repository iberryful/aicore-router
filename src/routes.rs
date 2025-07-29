use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{config::Config, proxy::ProxyRequest, token::TokenManager};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub token_manager: TokenManager,
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
    if let Some(obj) = body.as_object_mut() {
        if !obj.contains_key("model") {
            obj.insert("model".to_string(), json!(model));
        }
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
    let proxy = ProxyRequest::new(
        headers,
        Method::POST,
        body,
        model.to_string(),
        action,
        &state.config,
        &state.token_manager,
    )
    .await?;

    Ok(proxy.execute(&state.client, &state.config).await?)
}

pub async fn get_models(State(state): State<AppState>) -> impl IntoResponse {
    let model_data: Vec<serde_json::Value> = state
        .config
        .models
        .keys()
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
