use std::{env, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use async_stream::stream;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, pin_mut};
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_MESSAGES: usize = 100;
const MAX_CONTENT_BYTES: usize = 256 * 1024;
const MAX_TOTAL_CONTENT_BYTES: usize = 768 * 1024;
const MAX_MODEL_BYTES: usize = 200;
const MAX_ERROR_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Openai,
    Anthropic,
    Gemini,
    OpenaiCompatible,
}

impl Provider {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            "openai-compatible" => Some(Self::OpenaiCompatible),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::OpenaiCompatible => "openai-compatible",
        }
    }

    fn default_base_url(self) -> &'static str {
        match self {
            Self::Openai => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Self::OpenaiCompatible => "http://127.0.0.1:11434/v1",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub provider: Provider,
    pub ai_base_url: String,
    pub ai_model: String,
    pub ai_api_key: Option<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("APP_BIND must be a valid socket address: {0}")]
    InvalidBind(String),
    #[error("AI_PROVIDER must be openai, anthropic, gemini, or openai-compatible")]
    InvalidProvider,
    #[error("AI_BASE_URL must be an absolute http(s) URL")]
    InvalidBaseUrl,
    #[error("AI_MODEL must not be empty or longer than {MAX_MODEL_BYTES} bytes")]
    InvalidModel,
    #[error("a provider API key is required for {0}")]
    MissingApiKey(&'static str),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_text = env::var("APP_BIND").unwrap_or_else(|_| "127.0.0.1:41788".into());
        let bind = SocketAddr::from_str(&bind_text)
            .map_err(|_| ConfigError::InvalidBind(bind_text.clone()))?;
        let provider = env::var("AI_PROVIDER")
            .ok()
            .map(|value| Provider::parse(&value).ok_or(ConfigError::InvalidProvider))
            .transpose()?
            .unwrap_or(Provider::OpenaiCompatible);
        let ai_base_url =
            env::var("AI_BASE_URL").unwrap_or_else(|_| provider.default_base_url().to_owned());
        let ai_model = env::var("AI_MODEL").unwrap_or_else(|_| "llama3.2".into());
        let provider_api_key = match provider {
            Provider::Openai => env_key("OPENAI_API_KEY"),
            Provider::Anthropic => env_key("ANTHROPIC_API_KEY"),
            Provider::Gemini => env_key("GOOGLE_API_KEY").or_else(|| env_key("GEMINI_API_KEY")),
            Provider::OpenaiCompatible => None,
        };
        let ai_api_key = env_key("AI_API_KEY").or(provider_api_key);

        let config = Self {
            bind,
            provider,
            ai_base_url,
            ai_model,
            ai_api_key,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let url =
            reqwest::Url::parse(&self.ai_base_url).map_err(|_| ConfigError::InvalidBaseUrl)?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(ConfigError::InvalidBaseUrl);
        }
        if self.ai_model.trim().is_empty()
            || self.ai_model.len() > MAX_MODEL_BYTES
            || self.ai_model.chars().any(char::is_control)
        {
            return Err(ConfigError::InvalidModel);
        }
        if !matches!(self.provider, Provider::OpenaiCompatible) && self.ai_api_key.is_none() {
            return Err(ConfigError::MissingApiKey(self.provider.as_str()));
        }
        Ok(())
    }

    fn endpoint(&self, suffix: &str) -> String {
        format!(
            "{}/{}",
            self.ai_base_url.trim_end_matches('/'),
            suffix.trim_start_matches('/')
        )
    }

    fn models_endpoint(&self) -> String {
        match self.provider {
            Provider::Anthropic => self.endpoint("v1/models"),
            _ => self.endpoint("models"),
        }
    }

    fn chat_endpoint(&self, model: &str) -> String {
        match self.provider {
            Provider::Openai | Provider::OpenaiCompatible => self.endpoint("chat/completions"),
            Provider::Anthropic => self.endpoint("v1/messages"),
            Provider::Gemini => self.endpoint(&format!(
                "models/{}:streamGenerateContent?alt=sse",
                encode_path_segment(model.strip_prefix("models/").unwrap_or(model))
            )),
        }
    }
}

fn env_key(name: &str) -> Option<String> {
    env::var(name).ok().filter(|key| !key.is_empty())
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    client: Client,
}

pub fn build_router(config: Config) -> Result<Router, BuildError> {
    config.validate()?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(90))
        .pool_idle_timeout(Duration::from_secs(30))
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let state = AppState {
        config: Arc::new(config),
        client,
    };

    Ok(Router::new()
        .route("/api/health", get(health))
        .route("/api/models", get(models))
        .route("/api/chat", post(chat))
        .fallback(not_found)
        .with_state(state)
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BYTES))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]),
        )
        .layer(TraceLayer::new_for_http()))
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("failed to build HTTP client: {0}")]
    Http(#[from] reqwest::Error),
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "provider": state.config.provider.as_str(),
        "model": state.config.ai_model
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}

async fn models(State(state): State<AppState>) -> Json<Value> {
    let discovered = fetch_models(&state).await;
    let models = match discovered {
        Ok(models) if !models.is_empty() => models,
        Ok(_) | Err(_) => {
            tracing::warn!(
                provider = state.config.provider.as_str(),
                "model discovery unavailable; using configured fallback"
            );
            vec![ModelInfo {
                id: state.config.ai_model.clone(),
                name: state.config.ai_model.clone(),
            }]
        }
    };
    Json(json!({ "models": models }))
}

async fn fetch_models(state: &AppState) -> Result<Vec<ModelInfo>, reqwest::Error> {
    let request = authenticated(
        state.client.get(state.config.models_endpoint()),
        &state.config,
    );
    let response = request
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    let payload: Value = response.json().await?;
    Ok(normalize_models(state.config.provider, &payload))
}

fn normalize_models(provider: Provider, payload: &Value) -> Vec<ModelInfo> {
    let items = match provider {
        Provider::Gemini => payload.get("models"),
        _ => payload.get("data"),
    }
    .and_then(Value::as_array);
    let mut result = Vec::new();
    for item in items.into_iter().flatten() {
        let raw_id = item
            .get("id")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str);
        let Some(raw_id) = raw_id.filter(|id| !id.is_empty()) else {
            continue;
        };
        let id = raw_id.strip_prefix("models/").unwrap_or(raw_id).to_owned();
        let name = item
            .get("display_name")
            .or_else(|| item.get("displayName"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or(&id)
            .to_owned();
        result.push(ModelInfo { id, name });
    }
    result
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRequest {
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

async fn chat(
    State(state): State<AppState>,
    payload: Result<Json<ChatRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.body_text(),
            );
        }
    };
    if let Err(message) = validate_chat(&request) {
        return json_error(StatusCode::BAD_REQUEST, "invalid_request", message);
    }
    let model = request
        .model
        .as_deref()
        .unwrap_or(&state.config.ai_model)
        .to_owned();
    let upstream_payload = provider_payload(state.config.provider, &model, &request.messages);
    let upstream_request = authenticated(
        state
            .client
            .post(state.config.chat_endpoint(&model))
            .header(header::ACCEPT, "text/event-stream")
            .json(&upstream_payload),
        &state.config,
    );

    let upstream = match tokio::time::timeout(Duration::from_secs(30), upstream_request.send())
        .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::warn!(provider = state.config.provider.as_str(), error = %error, "chat upstream request failed");
            return sse_error("The AI provider is unavailable");
        }
        Err(_) => {
            tracing::warn!(
                provider = state.config.provider.as_str(),
                "chat upstream response timed out"
            );
            return sse_error("The AI provider timed out");
        }
    };
    if !upstream.status().is_success() {
        let status = upstream.status();
        let detail = limited_error_body(upstream).await;
        tracing::warn!(provider = state.config.provider.as_str(), %status, upstream_detail = %detail, "chat upstream rejected request");
        return sse_error(&format!("The AI provider returned {status}"));
    }
    let is_sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
    if !is_sse {
        tracing::warn!(
            provider = state.config.provider.as_str(),
            "chat upstream returned a non-SSE response"
        );
        return sse_error("The AI provider returned an invalid stream");
    }

    let provider = state.config.provider;
    let source = upstream.bytes_stream().eventsource();
    let output = stream! {
        pin_mut!(source);
        let mut sent_done = false;
        while let Some(event) = source.next().await {
            match event {
                Ok(event) => {
                    if let Some(data) = translate_event(provider, &event.event, &event.data) {
                        if data == "[DONE]" { sent_done = true; }
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {data}\n\n")));
                        if sent_done { break; }
                    }
                }
                Err(error) => {
                    tracing::warn!(provider = provider.as_str(), error = %error, "chat upstream stream interrupted");
                    let payload = error_payload("The AI provider stream was interrupted");
                    yield Ok(Bytes::from(format!("data: {payload}\n\n")));
                    break;
                }
            }
        }
        if !sent_done {
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
    };
    let mut response = Response::new(Body::from_stream(output));
    add_sse_headers(response.headers_mut());
    response
}

fn authenticated(request: RequestBuilder, config: &Config) -> RequestBuilder {
    let Some(key) = &config.ai_api_key else {
        return request;
    };
    match config.provider {
        Provider::Openai | Provider::OpenaiCompatible => request.bearer_auth(key),
        Provider::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        Provider::Gemini => request.header("x-goog-api-key", key),
    }
}

fn provider_payload(provider: Provider, model: &str, messages: &[ChatMessage]) -> Value {
    match provider {
        Provider::Openai | Provider::OpenaiCompatible => json!({
            "model": model, "messages": messages, "stream": true
        }),
        Provider::Anthropic => {
            let system = messages
                .iter()
                .filter(|m| matches!(m.role, Role::System))
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let messages = messages.iter().filter(|m| !matches!(m.role, Role::System))
                .map(|m| json!({"role": match m.role { Role::Assistant => "assistant", _ => "user" }, "content": m.content}))
                .collect::<Vec<_>>();
            let mut payload =
                json!({"model": model, "messages": messages, "max_tokens": 4096, "stream": true});
            if !system.is_empty() {
                payload["system"] = Value::String(system);
            }
            payload
        }
        Provider::Gemini => {
            let system = messages
                .iter()
                .filter(|m| matches!(m.role, Role::System))
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let contents = messages.iter().filter(|m| !matches!(m.role, Role::System))
                .map(|m| json!({"role": if matches!(m.role, Role::Assistant) { "model" } else { "user" }, "parts": [{"text": m.content}]}))
                .collect::<Vec<_>>();
            let mut payload = json!({"contents": contents});
            if !system.is_empty() {
                payload["systemInstruction"] = json!({"parts": [{"text": system}]});
            }
            payload
        }
    }
}

fn translate_event(provider: Provider, event_type: &str, data: &str) -> Option<String> {
    if data.trim().is_empty() {
        return None;
    }
    match provider {
        Provider::Openai | Provider::OpenaiCompatible => Some(data.to_owned()),
        Provider::Anthropic => {
            if matches!(event_type, "message_stop") || data == "[DONE]" {
                return Some("[DONE]".into());
            }
            let payload: Value = serde_json::from_str(data).ok()?;
            match payload.get("type").and_then(Value::as_str) {
                Some("content_block_delta") => payload
                    .pointer("/delta/text")
                    .and_then(Value::as_str)
                    .map(openai_delta),
                Some("message_stop") => Some("[DONE]".into()),
                Some("error") => {
                    Some(error_payload("The Anthropic stream returned an error").to_string())
                }
                _ => None,
            }
        }
        Provider::Gemini => {
            let payload: Value = serde_json::from_str(data).ok()?;
            let text = payload
                .pointer("/candidates/0/content/parts/0/text")
                .and_then(Value::as_str);
            let finish = payload
                .pointer("/candidates/0/finishReason")
                .and_then(Value::as_str);
            text.map(openai_delta)
                .or_else(|| finish.map(|_| "[DONE]".into()))
        }
    }
}

fn openai_delta(text: &str) -> String {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]})
        .to_string()
}

fn validate_chat(request: &ChatRequest) -> Result<(), &'static str> {
    if request.messages.is_empty() {
        return Err("messages must contain at least one message");
    }
    if request.messages.len() > MAX_MESSAGES {
        return Err("too many messages");
    }
    if let Some(model) = &request.model
        && (model.trim().is_empty()
            || model.len() > MAX_MODEL_BYTES
            || model.chars().any(char::is_control))
    {
        return Err("model is invalid");
    }
    let mut total = 0usize;
    for message in &request.messages {
        if message.content.trim().is_empty() {
            return Err("message content must not be empty");
        }
        if message.content.len() > MAX_CONTENT_BYTES {
            return Err("a message is too large");
        }
        total = total.saturating_add(message.content.len());
        if total > MAX_TOTAL_CONTENT_BYTES {
            return Err("total message content is too large");
        }
    }
    Ok(())
}

async fn limited_error_body(response: reqwest::Response) -> String {
    let mut source = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = source.next().await {
        let Ok(chunk) = chunk else { break };
        let remaining = MAX_ERROR_BYTES.saturating_sub(collected.len());
        if remaining == 0 {
            break;
        }
        collected.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    String::from_utf8_lossy(&collected).into_owned()
}

fn error_payload(message: &str) -> Value {
    json!({"error": {"type": "upstream_error", "message": message}})
}

fn json_error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"type": kind, "message": message.into()}})),
    )
        .into_response()
}

fn sse_error(message: &str) -> Response {
    let body = format!("data: {}\n\ndata: [DONE]\n\n", error_payload(message));
    let mut response = Response::new(Body::from(body));
    add_sse_headers(response.headers_mut());
    response
}

fn add_sse_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
}

async fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "not_found", "route not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: Provider) -> Config {
        Config {
            bind: "127.0.0.1:41788".parse().unwrap(),
            provider,
            ai_base_url: provider.default_base_url().to_owned(),
            ai_model: "test-model".to_owned(),
            ai_api_key: Some("test-key".to_owned()),
        }
    }

    #[test]
    fn translates_native_events() {
        let anthropic = translate_event(
            Provider::Anthropic,
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#,
        )
        .unwrap();
        assert!(anthropic.contains(r#""content":"Hi""#));
        let gemini = translate_event(
            Provider::Gemini,
            "message",
            r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}"#,
        )
        .unwrap();
        assert!(gemini.contains(r#""content":"Hello""#));
    }

    #[test]
    fn encodes_model_path_segment() {
        assert_eq!(encode_path_segment("gemini/foo bar"), "gemini%2Ffoo%20bar");
    }

    #[test]
    fn applies_provider_specific_authentication_without_query_keys() {
        let client = Client::new();
        let openai = authenticated(
            client.get("https://example.test/models"),
            &config(Provider::Openai),
        )
        .build()
        .unwrap();
        assert_eq!(
            openai.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer test-key"
        );

        let anthropic = authenticated(
            client.get("https://example.test/models"),
            &config(Provider::Anthropic),
        )
        .build()
        .unwrap();
        assert_eq!(anthropic.headers().get("x-api-key").unwrap(), "test-key");
        assert_eq!(
            anthropic.headers().get("anthropic-version").unwrap(),
            "2023-06-01"
        );

        let gemini = authenticated(
            client.get("https://example.test/models"),
            &config(Provider::Gemini),
        )
        .build()
        .unwrap();
        assert_eq!(gemini.headers().get("x-goog-api-key").unwrap(), "test-key");
        assert!(gemini.url().query().is_none());
    }

    #[test]
    fn builds_native_provider_payloads() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "Be concise".into(),
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".into(),
            },
            ChatMessage {
                role: Role::Assistant,
                content: "Hi".into(),
            },
        ];
        let anthropic = provider_payload(Provider::Anthropic, "claude-test", &messages);
        assert_eq!(anthropic["system"], "Be concise");
        assert_eq!(anthropic["messages"][0]["role"], "user");
        assert_eq!(anthropic["stream"], true);

        let gemini = provider_payload(Provider::Gemini, "gemini-test", &messages);
        assert_eq!(
            gemini["systemInstruction"]["parts"][0]["text"],
            "Be concise"
        );
        assert_eq!(gemini["contents"][1]["role"], "model");

        let openai = provider_payload(Provider::OpenaiCompatible, "local-test", &messages);
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(openai["model"], "local-test");
    }

    #[test]
    fn normalizes_native_model_catalogs() {
        let anthropic = normalize_models(
            Provider::Anthropic,
            &json!({"data": [{"id": "claude-test", "display_name": "Claude Test"}]}),
        );
        assert_eq!(
            anthropic,
            vec![ModelInfo {
                id: "claude-test".into(),
                name: "Claude Test".into()
            }]
        );
        let gemini = normalize_models(
            Provider::Gemini,
            &json!({"models": [{"name": "models/gemini-test", "displayName": "Gemini Test"}]}),
        );
        assert_eq!(
            gemini,
            vec![ModelInfo {
                id: "gemini-test".into(),
                name: "Gemini Test".into()
            }]
        );
    }
}
