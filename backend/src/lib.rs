use std::{
    collections::HashSet,
    env,
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

mod coders;

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
use futures_util::{StreamExt, future::join_all, pin_mut};
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Semaphore;
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_MESSAGES: usize = 100;
const MAX_CONTENT_BYTES: usize = 256 * 1024;
const MAX_TOTAL_CONTENT_BYTES: usize = 768 * 1024;
const MAX_MODEL_BYTES: usize = 200;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_CUSTOM_PROVIDERS: usize = 20;
const DEFAULT_CODER_TIMEOUT_SECONDS: u64 = 300;
const DEFAULT_CODER_CONCURRENCY: usize = 2;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Openai,
    Anthropic,
    Gemini,
    OpenaiCompatible,
}

impl ProviderKind {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            "openai-compatible" | "custom" | "ollama" => Some(Self::OpenaiCompatible),
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
}

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub ai_coder: String,
    pub ai_coder_name: String,
    pub provider_id: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub default_model: String,
    pub api_key: Option<String>,
}

impl ProviderConfig {
    fn endpoint(&self, suffix: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            suffix.trim_start_matches('/')
        )
    }

    fn models_endpoint(&self) -> String {
        match self.kind {
            ProviderKind::Anthropic => self.endpoint("v1/models"),
            _ => self.endpoint("models"),
        }
    }

    fn chat_endpoint(&self, model: &str) -> String {
        match self.kind {
            ProviderKind::Openai | ProviderKind::OpenaiCompatible => {
                self.endpoint("chat/completions")
            }
            ProviderKind::Anthropic => self.endpoint("v1/messages"),
            ProviderKind::Gemini => self.endpoint(&format!(
                "models/{}:streamGenerateContent?alt=sse",
                encode_path_segment(model.strip_prefix("models/").unwrap_or(model))
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub providers: Vec<ProviderConfig>,
    pub coders: Vec<coders::CoderRoute>,
    pub default_coder: String,
    pub default_provider: String,
    pub coder_workspace: PathBuf,
    pub coder_timeout: Duration,
    pub coder_allow_writes: bool,
    pub coder_max_concurrency: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomProviderInput {
    id: String,
    #[serde(default)]
    name: Option<String>,
    base_url: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    ai_coder: Option<String>,
    #[serde(default)]
    ai_coder_name: Option<String>,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("APP_BIND must be a valid socket address: {0}")]
    InvalidBind(String),
    #[error("provider `{0}` has an invalid identifier")]
    InvalidProviderId(String),
    #[error("provider `{0}` must use an absolute http(s) URL")]
    InvalidBaseUrl(String),
    #[error("provider `{0}` has an invalid default model")]
    InvalidModel(String),
    #[error("duplicate provider id: {0}")]
    DuplicateProvider(String),
    #[error("duplicate AI coder/provider route: {0}")]
    DuplicateRoute(String),
    #[error("AI_ENDPOINTS_JSON is invalid: {0}")]
    InvalidCustomProviders(String),
    #[error("AI_DEFAULT_PROVIDER does not name a configured provider: {0}")]
    InvalidDefaultProvider(String),
    #[error("AI_DEFAULT_CODER does not name a configured AI coder: {0}")]
    InvalidDefaultCoder(String),
    #[error(
        "no AI backends are configured; install an AI coder CLI or configure a hosted provider"
    )]
    NoProviders,
    #[error("AI coder configuration is invalid: {0}")]
    InvalidCoderConfig(String),
    #[error("AI_CODER_WORKSPACE must name an existing directory: {0}")]
    InvalidCoderWorkspace(String),
    #[error("APP_BIND must use a loopback address")]
    RemoteBind,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_text = env::var("APP_BIND").unwrap_or_else(|_| "127.0.0.1:41788".into());
        let bind = SocketAddr::from_str(&bind_text)
            .map_err(|_| ConfigError::InvalidBind(bind_text.clone()))?;
        let legacy_kind = env::var("AI_PROVIDER")
            .ok()
            .and_then(|value| ProviderKind::parse(&value));
        let legacy_key = env_key("AI_API_KEY");
        let legacy_model = env::var("AI_MODEL").ok();
        let legacy_base = env::var("AI_BASE_URL").ok();
        let mut providers = Vec::new();

        if let Some(key) = env_key("OPENAI_API_KEY").or_else(|| {
            (legacy_kind == Some(ProviderKind::Openai))
                .then(|| legacy_key.clone())
                .flatten()
        }) {
            providers.push(ProviderConfig {
                id: "openai".into(),
                name: "OpenAI".into(),
                ai_coder: "api".into(),
                ai_coder_name: "Direct API".into(),
                provider_id: "openai".into(),
                kind: ProviderKind::Openai,
                base_url: env::var("OPENAI_BASE_URL")
                    .ok()
                    .or_else(|| {
                        (legacy_kind == Some(ProviderKind::Openai))
                            .then(|| legacy_base.clone())
                            .flatten()
                    })
                    .unwrap_or_else(|| "https://api.openai.com/v1".into()),
                default_model: env::var("OPENAI_MODEL")
                    .ok()
                    .or_else(|| {
                        (legacy_kind == Some(ProviderKind::Openai))
                            .then(|| legacy_model.clone())
                            .flatten()
                    })
                    .unwrap_or_else(|| "gpt-5-mini".into()),
                api_key: Some(key),
            });
        }
        if let Some(key) = env_key("ANTHROPIC_API_KEY").or_else(|| {
            (legacy_kind == Some(ProviderKind::Anthropic))
                .then(|| legacy_key.clone())
                .flatten()
        }) {
            providers.push(ProviderConfig {
                id: "anthropic".into(),
                name: "Anthropic".into(),
                ai_coder: "api".into(),
                ai_coder_name: "Direct API".into(),
                provider_id: "anthropic".into(),
                kind: ProviderKind::Anthropic,
                base_url: env::var("ANTHROPIC_BASE_URL")
                    .ok()
                    .or_else(|| {
                        (legacy_kind == Some(ProviderKind::Anthropic))
                            .then(|| legacy_base.clone())
                            .flatten()
                    })
                    .unwrap_or_else(|| "https://api.anthropic.com".into()),
                default_model: env::var("ANTHROPIC_MODEL")
                    .ok()
                    .or_else(|| {
                        (legacy_kind == Some(ProviderKind::Anthropic))
                            .then(|| legacy_model.clone())
                            .flatten()
                    })
                    .unwrap_or_else(|| "claude-sonnet-4-5".into()),
                api_key: Some(key),
            });
        }
        if let Some(key) = env_key("GOOGLE_API_KEY")
            .or_else(|| env_key("GEMINI_API_KEY"))
            .or_else(|| {
                (legacy_kind == Some(ProviderKind::Gemini))
                    .then(|| legacy_key.clone())
                    .flatten()
            })
        {
            providers.push(ProviderConfig {
                id: "gemini".into(),
                name: "Google Gemini".into(),
                ai_coder: "api".into(),
                ai_coder_name: "Direct API".into(),
                provider_id: "gemini".into(),
                kind: ProviderKind::Gemini,
                base_url: env::var("GEMINI_BASE_URL")
                    .ok()
                    .or_else(|| {
                        (legacy_kind == Some(ProviderKind::Gemini))
                            .then(|| legacy_base.clone())
                            .flatten()
                    })
                    .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".into()),
                default_model: env::var("GEMINI_MODEL")
                    .ok()
                    .or_else(|| {
                        (legacy_kind == Some(ProviderKind::Gemini))
                            .then(|| legacy_model.clone())
                            .flatten()
                    })
                    .unwrap_or_else(|| "gemini-2.5-flash".into()),
                api_key: Some(key),
            });
        }

        let ollama_is_legacy = legacy_kind == Some(ProviderKind::OpenaiCompatible);
        let ollama_enabled = ollama_is_legacy || env_flag("OLLAMA_ENABLED");
        if ollama_enabled {
            providers.push(ProviderConfig {
                id: "ollama".into(),
                name: "Ollama / Local".into(),
                ai_coder: "api".into(),
                ai_coder_name: "Direct API".into(),
                provider_id: "ollama".into(),
                kind: ProviderKind::OpenaiCompatible,
                base_url: env::var("OLLAMA_BASE_URL")
                    .ok()
                    .or_else(|| ollama_is_legacy.then(|| legacy_base.clone()).flatten())
                    .unwrap_or_else(|| "http://127.0.0.1:11434/v1".into()),
                default_model: env::var("OLLAMA_MODEL")
                    .ok()
                    .or_else(|| ollama_is_legacy.then(|| legacy_model.clone()).flatten())
                    .unwrap_or_else(|| "llama3.2".into()),
                api_key: env_key("OLLAMA_API_KEY")
                    .or_else(|| ollama_is_legacy.then(|| legacy_key.clone()).flatten()),
            });
        }

        if let Ok(raw) = env::var("AI_ENDPOINTS_JSON") {
            let custom: Vec<CustomProviderInput> = serde_json::from_str(&raw)
                .map_err(|error| ConfigError::InvalidCustomProviders(error.to_string()))?;
            if custom.len() > MAX_CUSTOM_PROVIDERS {
                return Err(ConfigError::InvalidCustomProviders(format!(
                    "at most {MAX_CUSTOM_PROVIDERS} endpoints are allowed"
                )));
            }
            for item in custom {
                let api_key = item.api_key_env.as_deref().and_then(env_key);
                let ai_coder = item.ai_coder.unwrap_or_else(|| "api".into());
                let ai_coder_name = item.ai_coder_name.unwrap_or_else(|| {
                    if ai_coder == "api" {
                        "Direct API".into()
                    } else {
                        display_identifier(&ai_coder)
                    }
                });
                let provider_id = item.provider.unwrap_or_else(|| item.id.clone());
                providers.push(ProviderConfig {
                    name: item.name.unwrap_or_else(|| provider_id.clone()),
                    id: item.id,
                    ai_coder,
                    ai_coder_name,
                    provider_id,
                    kind: ProviderKind::OpenaiCompatible,
                    base_url: item.base_url,
                    default_model: item.model.unwrap_or_else(|| "default".into()),
                    api_key,
                });
            }
        }

        let coders = coders::discover_routes().map_err(ConfigError::InvalidCoderConfig)?;
        if providers.is_empty() && coders.is_empty() {
            return Err(ConfigError::NoProviders);
        }
        let default_coder = env::var("AI_DEFAULT_CODER").unwrap_or_else(|_| {
            if legacy_kind == Some(ProviderKind::OpenaiCompatible) {
                "api".into()
            } else {
                "hermes".into()
            }
        });
        let provider_hint = env::var("AI_DEFAULT_PROVIDER").ok();
        let default_provider = coders
            .iter()
            .find(|route| {
                route.kind.id() == default_coder
                    && provider_hint
                        .as_ref()
                        .is_none_or(|hint| route.provider == *hint)
            })
            .map(|route| route.provider.clone())
            .or_else(|| {
                providers
                    .iter()
                    .find(|provider| {
                        provider.ai_coder == default_coder
                            && provider_hint.as_ref().is_none_or(|hint| {
                                provider.provider_id == *hint || provider.id == *hint
                            })
                    })
                    .map(|provider| provider.provider_id.clone())
            })
            .ok_or_else(|| {
                provider_hint.clone().map_or_else(
                    || ConfigError::InvalidDefaultCoder(default_coder.clone()),
                    ConfigError::InvalidDefaultProvider,
                )
            })?;
        let workspace_text = env::var("AI_CODER_WORKSPACE").ok();
        let coder_workspace = workspace_text
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            .canonicalize()
            .map_err(|_| {
                ConfigError::InvalidCoderWorkspace(workspace_text.unwrap_or_else(|| ".".into()))
            })?;
        if !coder_workspace.is_dir() {
            return Err(ConfigError::InvalidCoderWorkspace(
                coder_workspace.display().to_string(),
            ));
        }
        let coder_timeout = Duration::from_secs(
            env::var("AI_CODER_TIMEOUT_SECONDS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| (1..=900).contains(value))
                .unwrap_or(DEFAULT_CODER_TIMEOUT_SECONDS),
        );
        let coder_max_concurrency = env::var("AI_CODER_MAX_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (1..=16).contains(value))
            .unwrap_or(DEFAULT_CODER_CONCURRENCY);
        let config = Self {
            bind,
            providers,
            coders,
            default_coder,
            default_provider,
            coder_workspace,
            coder_timeout,
            coder_allow_writes: env_flag("AI_CODER_ALLOW_WRITES"),
            coder_max_concurrency,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.providers.is_empty() && self.coders.is_empty() {
            return Err(ConfigError::NoProviders);
        }
        if !self.bind.ip().is_loopback() {
            return Err(ConfigError::RemoteBind);
        }
        let mut ids = HashSet::new();
        let mut provider_routes = HashSet::new();
        for provider in &self.providers {
            if !valid_provider_id(&provider.id)
                || !valid_provider_id(&provider.ai_coder)
                || !valid_provider_id(&provider.provider_id)
            {
                return Err(ConfigError::InvalidProviderId(provider.id.clone()));
            }
            if !ids.insert(provider.id.clone()) {
                return Err(ConfigError::DuplicateProvider(provider.id.clone()));
            }
            let route = format!("{}/{}", provider.ai_coder, provider.provider_id);
            if !provider_routes.insert(route.clone()) {
                return Err(ConfigError::DuplicateRoute(route));
            }
            let url = reqwest::Url::parse(&provider.base_url)
                .map_err(|_| ConfigError::InvalidBaseUrl(provider.id.clone()))?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err(ConfigError::InvalidBaseUrl(provider.id.clone()));
            }
            if provider.default_model.trim().is_empty()
                || provider.default_model.len() > MAX_MODEL_BYTES
                || provider.default_model.chars().any(char::is_control)
            {
                return Err(ConfigError::InvalidModel(provider.id.clone()));
            }
        }
        let mut coder_routes = HashSet::new();
        for route in &self.coders {
            if !valid_provider_id(route.kind.id())
                || !valid_provider_id(&route.provider)
                || route.model.trim().is_empty()
                || route.model.len() > MAX_MODEL_BYTES
                || route.model.chars().any(char::is_control)
            {
                return Err(ConfigError::InvalidCoderConfig(route.key()));
            }
            if provider_routes.contains(&format!("{}/{}", route.kind.id(), route.provider))
                || !coder_routes.insert(route.key())
            {
                return Err(ConfigError::DuplicateRoute(route.key()));
            }
        }
        if self
            .coder_route(&self.default_coder, &self.default_provider, None)
            .is_none()
            && self
                .provider_route(&self.default_coder, &self.default_provider)
                .is_none()
        {
            return Err(ConfigError::InvalidDefaultCoder(self.default_coder.clone()));
        }
        Ok(())
    }

    fn provider_route(&self, coder: &str, provider: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|candidate| {
            candidate.ai_coder == coder
                && (candidate.provider_id == provider || candidate.id == provider)
        })
    }

    fn coder_route(
        &self,
        coder: &str,
        provider: &str,
        model: Option<&str>,
    ) -> Option<&coders::CoderRoute> {
        self.coders.iter().find(|route| {
            route.kind.id() == coder
                && route.provider == provider
                && model.is_none_or(|value| route.model == value)
        })
    }
}

fn valid_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
fn display_identifier(id: &str) -> String {
    let mut chars = id.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
fn parse_model_path(value: &str) -> Option<(&str, &str, &str)> {
    let mut parts = value.splitn(3, '/');
    let ai_coder = parts.next()?;
    let provider = parts.next()?;
    let model = parts.next()?;
    (valid_provider_id(ai_coder) && valid_provider_id(provider) && !model.trim().is_empty())
        .then_some((ai_coder, provider, model))
}
fn env_key(name: &str) -> Option<String> {
    env::var(name).ok().filter(|key| !key.is_empty())
}
fn env_flag(name: &str) -> bool {
    env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
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
    coder_slots: Arc<Semaphore>,
    metrics: Arc<AppMetrics>,
}

#[derive(Default)]
struct AppMetrics {
    chat_requests: AtomicU64,
    coder_busy_rejections: AtomicU64,
}

/// The API routes, mounted bare.
///
/// A composable router should not decide where it lives. `build_router` mounts
/// these at `/api` for standalone use; an embedding host nests them wherever it
/// keeps its own API, and applies its own authentication on the way in --
/// nothing here authenticates anything.
pub fn build_api_router(config: Config) -> Result<Router, BuildError> {
    config.validate()?;
    let coder_slots = Arc::new(Semaphore::new(config.coder_max_concurrency));
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(120))
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
        coder_slots,
        metrics: Arc::new(AppMetrics::default()),
    };
    Ok(Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/providers", get(providers))
        .route("/coders", get(coder_statuses))
        .route("/models", get(models))
        .route("/chat", post(chat))
        .with_state(state)
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BYTES))
        .layer(TraceLayer::new_for_http()))
}

/// The standalone service: the API at `/api`, and a JSON 404 for anything else.
pub fn build_router(config: Config) -> Result<Router, BuildError> {
    Ok(Router::new()
        .nest("/api", build_api_router(config)?)
        .fallback(not_found))
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("failed to build HTTP client: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ProviderInfo {
    id: String,
    name: String,
    ai_coder: String,
    ai_coder_name: String,
    provider: String,
    kind: String,
    default_model: String,
    is_default: bool,
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "default_ai_coder": state.config.default_coder,
        "default_provider": state.config.default_provider,
        "provider_count": state.config.providers.len(),
        "coder_count": state.config.coders.len()
    }))
}
async fn ready(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ready",
        "default_ai_coder": state.config.default_coder,
        "default_provider": state.config.default_provider,
    }))
}
async fn metrics(State(state): State<AppState>) -> Response {
    let body = format!(
        concat!(
            "# TYPE lumen_chat_requests_total counter\n",
            "lumen_chat_requests_total {}\n",
            "# TYPE lumen_coder_busy_rejections_total counter\n",
            "lumen_coder_busy_rejections_total {}\n",
            "# TYPE lumen_coder_slots_available gauge\n",
            "lumen_coder_slots_available {}\n",
            "# TYPE lumen_configured_coders gauge\n",
            "lumen_configured_coders {}\n",
            "# TYPE lumen_configured_providers gauge\n",
            "lumen_configured_providers {}\n",
        ),
        state.metrics.chat_requests.load(Ordering::Relaxed),
        state.metrics.coder_busy_rejections.load(Ordering::Relaxed),
        state.coder_slots.available_permits(),
        state.config.coders.len(),
        state.config.providers.len(),
    );
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    response
}
async fn providers(State(state): State<AppState>) -> Json<Value> {
    let providers = state
        .config
        .providers
        .iter()
        .map(|provider| ProviderInfo {
            id: provider.id.clone(),
            name: provider.name.clone(),
            ai_coder: provider.ai_coder.clone(),
            ai_coder_name: provider.ai_coder_name.clone(),
            provider: provider.provider_id.clone(),
            kind: provider.kind.as_str().into(),
            default_model: provider.default_model.clone(),
            is_default: provider.ai_coder == state.config.default_coder
                && provider.provider_id == state.config.default_provider,
        })
        .collect::<Vec<_>>();
    Json(
        json!({ "providers": providers, "default_ai_coder": state.config.default_coder, "default_provider": state.config.default_provider }),
    )
}

async fn coder_statuses(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "coders": coders::statuses(&state.config.coders),
        "allow_writes": state.config.coder_allow_writes,
        "workspace": state.config.coder_workspace,
        "timeout_seconds": state.config.coder_timeout.as_secs(),
        "max_concurrency": state.config.coder_max_concurrency,
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub key: String,
    pub name: String,
    pub provider: String,
    pub provider_name: String,
    pub ai_coder: String,
    pub ai_coder_name: String,
}

async fn models(State(state): State<AppState>) -> Json<Value> {
    let tasks = state.config.providers.iter().cloned().map(|provider| {
        let client = state.client.clone();
        async move {
            let result = fetch_models(&client, &provider).await;
            (provider, result)
        }
    });
    let mut all_models = state
        .config
        .coders
        .iter()
        .map(|route| ModelInfo {
            id: route.model.clone(),
            key: route.key(),
            name: route.model_name.clone(),
            provider: route.provider.clone(),
            provider_name: route.provider_name.clone(),
            ai_coder: route.kind.id().into(),
            ai_coder_name: route.kind.display_name().into(),
        })
        .collect::<Vec<_>>();
    let mut provider_errors = Vec::new();
    for (provider, result) in join_all(tasks).await {
        let discovered = match result {
            Ok(models) if !models.is_empty() => models,
            Ok(_) => {
                provider_errors.push(json!({"provider": provider.id, "message": "No models were returned; using the configured default."}));
                vec![(
                    provider.default_model.clone(),
                    provider.default_model.clone(),
                )]
            }
            Err(error) => {
                tracing::warn!(provider = provider.id, error = %error, "model discovery unavailable");
                provider_errors.push(json!({"provider": provider.id, "message": "Provider unavailable; showing the configured default."}));
                vec![(
                    provider.default_model.clone(),
                    provider.default_model.clone(),
                )]
            }
        };
        for (id, name) in discovered {
            all_models.push(ModelInfo {
                key: format!("{}/{}/{id}", provider.ai_coder, provider.provider_id),
                id,
                name,
                provider: provider.provider_id.clone(),
                provider_name: provider.name.clone(),
                ai_coder: provider.ai_coder.clone(),
                ai_coder_name: provider.ai_coder_name.clone(),
            });
        }
    }
    all_models.sort_by(|a, b| {
        a.ai_coder_name
            .cmp(&b.ai_coder_name)
            .then(a.provider_name.cmp(&b.provider_name))
            .then(a.name.cmp(&b.name))
    });
    Json(
        json!({ "models": all_models, "provider_errors": provider_errors, "default_ai_coder": state.config.default_coder, "default_provider": state.config.default_provider }),
    )
}

async fn fetch_models(
    client: &Client,
    provider: &ProviderConfig,
) -> Result<Vec<(String, String)>, reqwest::Error> {
    let request = authenticated(client.get(provider.models_endpoint()), provider);
    let response = request
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    let payload: Value = response.json().await?;
    Ok(normalize_models(provider.kind, &payload))
}
fn normalize_models(kind: ProviderKind, payload: &Value) -> Vec<(String, String)> {
    let items = match kind {
        ProviderKind::Gemini => payload.get("models"),
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
        result.push((id, name));
    }
    result
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRequest {
    #[serde(default)]
    pub ai_coder: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Tools the model may call.
    ///
    /// Lumen never executes these. It translates them into whichever wire
    /// format the selected provider speaks, and hands any resulting calls back
    /// to the caller to run. That keeps this service stateless and free of any
    /// knowledge of the caller's domain, and means a tool runs with the
    /// caller's own permissions rather than Lumen's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    pub messages: Vec<ChatMessage>,
}

/// A tool offered to the model, described in provider-neutral terms.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments. Passed through to the provider as-is;
    /// all three accept JSON Schema, they merely disagree about where it goes.
    pub input_schema: Value,
}

/// A call the model wants made.
///
/// `arguments` is a parsed object rather than the JSON string OpenAI puts on
/// the wire — callers should not have to parse a string out of a struct that
/// already knows it is JSON.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    /// Set on an assistant turn that asked for tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Set on a `tool` turn, naming the call this is the result of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// The result of a tool the caller ran on the model's behalf.
    Tool,
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
    state.metrics.chat_requests.fetch_add(1, Ordering::Relaxed);
    let embedded = request.model.as_deref().and_then(parse_model_path);
    if embedded.is_some_and(|(coder, provider, _)| {
        request
            .ai_coder
            .as_deref()
            .is_some_and(|value| value != coder)
            || request
                .provider
                .as_deref()
                .is_some_and(|value| value != provider)
    }) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "conflicting_route",
            "ai_coder/provider fields conflict with the canonical model path",
        );
    }
    let requested_coder = request
        .ai_coder
        .as_deref()
        .or_else(|| embedded.map(|(coder, _, _)| coder))
        .unwrap_or(&state.config.default_coder);
    let requested_provider = request
        .provider
        .as_deref()
        .or_else(|| embedded.map(|(_, provider, _)| provider))
        .or_else(|| {
            (requested_coder == state.config.default_coder)
                .then_some(state.config.default_provider.as_str())
        });
    let requested_model = embedded
        .map(|(_, _, model)| model)
        .or(request.model.as_deref());
    if let Some(provider_id) = requested_provider
        && let Some(route) = state
            .config
            .coder_route(requested_coder, provider_id, requested_model)
    {
        // A CLI coder has no tool protocol. Flattening the tools into the
        // prompt would let a model "call" something that never runs and whose
        // result is never returned -- an answer built on a tool it believes
        // executed. Refusing is the only honest option.
        if request
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        {
            return sse_error(
                "Tool calling is not supported on AI coder routes; select a hosted provider",
            );
        }
        let permit = match state.coder_slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                state
                    .metrics
                    .coder_busy_rejections
                    .fetch_add(1, Ordering::Relaxed);
                return sse_error("All AI coder execution slots are busy; try again shortly");
            }
        };
        let prompt = coder_prompt(&request.messages, state.config.coder_allow_writes);
        let result = coders::execute(
            route,
            &prompt,
            &state.config.coder_workspace,
            state.config.coder_allow_writes,
            state.config.coder_timeout,
        )
        .await;
        drop(permit);
        return match result {
            Ok(execution) => sse_text(&execution.output),
            Err(coders::CoderError::Timeout { .. }) => sse_error("The selected AI coder timed out"),
            Err(_) => {
                sse_error("The selected AI coder failed; verify its native CLI authentication")
            }
        };
    }
    let provider = requested_provider
        .and_then(|provider_id| state.config.provider_route(requested_coder, provider_id));
    let Some(provider) = provider else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "unknown_route",
            format!(
                "AI coder/provider route `{}/{}` is not configured",
                requested_coder,
                requested_provider.unwrap_or("*")
            ),
        );
    };
    let model = requested_model.unwrap_or(&provider.default_model);
    if model.trim().is_empty()
        || model.len() > MAX_MODEL_BYTES
        || model.chars().any(char::is_control)
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "model is invalid",
        );
    }
    let upstream_payload = provider_payload(
        provider.kind,
        model,
        &request.messages,
        request.temperature,
        request.max_tokens,
        request.tools.as_deref(),
    );
    let upstream_request = authenticated(
        state
            .client
            .post(provider.chat_endpoint(model))
            .header(header::ACCEPT, "text/event-stream")
            .json(&upstream_payload),
        provider,
    );
    let upstream = match tokio::time::timeout(Duration::from_secs(30), upstream_request.send())
        .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::warn!(provider = provider.id, error = %error, "chat upstream request failed");
            return sse_error("The selected AI provider is unavailable");
        }
        Err(_) => return sse_error("The selected AI provider timed out"),
    };
    if !upstream.status().is_success() {
        let status = upstream.status();
        let detail = limited_error_body(upstream).await;
        tracing::warn!(provider = provider.id, %status, upstream_detail = %detail, "chat upstream rejected request");
        return sse_error(&format!("The selected AI provider returned {status}"));
    }
    let is_sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
    if !is_sse {
        return sse_error("The selected AI provider returned an invalid stream");
    }

    let kind = provider.kind;
    let source = upstream.bytes_stream().eventsource();
    let output = stream! {
        pin_mut!(source);
        let mut sent_done = false;
        while let Some(event) = source.next().await {
            match event {
                Ok(event) => {
                    if let Some(data) = translate_event(kind, &event.event, &event.data) {
                        if data == "[DONE]" { sent_done = true; }
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {data}\n\n")));
                        if sent_done { break; }
                    }
                }
                Err(error) => {
                    tracing::warn!(provider = kind.as_str(), error = %error, "chat upstream stream interrupted");
                    yield Ok(Bytes::from(format!("data: {}\n\n", error_payload("The AI provider stream was interrupted"))));
                    break;
                }
            }
        }
        if !sent_done { yield Ok(Bytes::from_static(b"data: [DONE]\n\n")); }
    };
    let mut response = Response::new(Body::from_stream(output));
    add_sse_headers(response.headers_mut());
    response
}

fn authenticated(request: RequestBuilder, provider: &ProviderConfig) -> RequestBuilder {
    let Some(key) = &provider.api_key else {
        return request;
    };
    match provider.kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => request.bearer_auth(key),
        ProviderKind::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        ProviderKind::Gemini => request.header("x-goog-api-key", key),
    }
}
/// Map a tool-call id back to the tool's name.
///
/// Gemini keys a function response by *name* while OpenAI and Anthropic key it
/// by the id they issued, so a transcript that only carries ids has to be read
/// backwards to serve Gemini. Built once per request rather than scanned per
/// message.
fn tool_names_by_id(messages: &[ChatMessage]) -> std::collections::HashMap<&str, &str> {
    messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|c| (c.id.as_str(), c.name.as_str()))
        .collect()
}

/// Arguments as the JSON *string* OpenAI puts on the wire.
fn arguments_as_string(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

fn openai_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| match m.role {
            Role::Tool => json!({
                "role": "tool",
                "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": m.content,
            }),
            _ => {
                let role = match m.role {
                    Role::System => "system",
                    Role::Assistant => "assistant",
                    _ => "user",
                };
                let mut value = json!({"role": role, "content": m.content});
                if let Some(calls) = &m.tool_calls {
                    value["tool_calls"] = Value::Array(
                        calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        "arguments": arguments_as_string(&c.arguments),
                                    },
                                })
                            })
                            .collect(),
                    );
                }
                value
            }
        })
        .collect()
}

fn anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .map(|m| match m.role {
            // Anthropic carries a tool result as a user turn holding a
            // tool_result block, not as a role of its own.
            Role::Tool => json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                }],
            }),
            Role::Assistant if m.tool_calls.is_some() => {
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": m.content}));
                }
                for call in m.tool_calls.iter().flatten() {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                json!({"role": "assistant", "content": blocks})
            }
            Role::Assistant => json!({"role": "assistant", "content": m.content}),
            _ => json!({"role": "user", "content": m.content}),
        })
        .collect()
}

fn gemini_contents(messages: &[ChatMessage]) -> Vec<Value> {
    let names = tool_names_by_id(messages);
    messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .map(|m| match m.role {
            Role::Tool => {
                let name = m
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| names.get(id).copied())
                    .unwrap_or_default();
                json!({
                    "role": "user",
                    "parts": [{"functionResponse": {
                        "name": name,
                        "response": {"content": m.content},
                    }}],
                })
            }
            Role::Assistant if m.tool_calls.is_some() => {
                let mut parts: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(json!({"text": m.content}));
                }
                for call in m.tool_calls.iter().flatten() {
                    parts.push(json!({"functionCall": {
                        "name": call.name,
                        "args": call.arguments,
                    }}));
                }
                json!({"role": "model", "parts": parts})
            }
            Role::Assistant => json!({"role": "model", "parts": [{"text": m.content}]}),
            _ => json!({"role": "user", "parts": [{"text": m.content}]}),
        })
        .collect()
}

fn provider_payload(
    kind: ProviderKind,
    model: &str,
    messages: &[ChatMessage],
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    tools: Option<&[ToolDef]>,
) -> Value {
    match kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => {
            let mut payload = json!({
                "model": model,
                "messages": openai_messages(messages),
                "stream": true,
            });
            if let Some(value) = temperature {
                payload["temperature"] = json!(value);
            }
            if let Some(value) = max_tokens {
                payload["max_tokens"] = json!(value);
            }
            if let Some(tools) = tools.filter(|t| !t.is_empty()) {
                payload["tools"] = Value::Array(
                    tools
                        .iter()
                        .map(|tool| {
                            json!({"type": "function", "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema,
                            }})
                        })
                        .collect(),
                );
            }
            payload
        }
        ProviderKind::Anthropic => {
            let system = messages
                .iter()
                .filter(|m| matches!(m.role, Role::System))
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let mut payload = json!({
                "model": model,
                "messages": anthropic_messages(messages),
                "max_tokens": max_tokens.unwrap_or(4096),
                "stream": true,
            });
            if !system.is_empty() {
                payload["system"] = Value::String(system);
            }
            if let Some(value) = temperature {
                payload["temperature"] = json!(value);
            }
            if let Some(tools) = tools.filter(|t| !t.is_empty()) {
                payload["tools"] = Value::Array(
                    tools
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool.name,
                                "description": tool.description,
                                "input_schema": tool.input_schema,
                            })
                        })
                        .collect(),
                );
            }
            payload
        }
        ProviderKind::Gemini => {
            let system = messages
                .iter()
                .filter(|m| matches!(m.role, Role::System))
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let contents = gemini_contents(messages);
            let mut generation = serde_json::Map::new();
            if let Some(value) = temperature {
                generation.insert("temperature".into(), json!(value));
            }
            if let Some(value) = max_tokens {
                generation.insert("maxOutputTokens".into(), json!(value));
            }
            let mut payload = json!({"contents": contents});
            if !system.is_empty() {
                payload["systemInstruction"] = json!({"parts": [{"text": system}]});
            }
            if !generation.is_empty() {
                payload["generationConfig"] = Value::Object(generation);
            }
            if let Some(tools) = tools.filter(|t| !t.is_empty()) {
                payload["tools"] = json!([{
                    "functionDeclarations": tools
                        .iter()
                        .map(|tool| json!({
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        }))
                        .collect::<Vec<_>>(),
                }]);
            }
            payload
        }
    }
}
fn translate_event(kind: ProviderKind, event_type: &str, data: &str) -> Option<String> {
    if data.trim().is_empty() {
        return None;
    }
    match kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => Some(data.to_owned()),
        ProviderKind::Anthropic => {
            if event_type == "message_stop" || data == "[DONE]" {
                return Some("[DONE]".into());
            }
            let payload: Value = serde_json::from_str(data).ok()?;
            // The block index is reused as the OpenAI tool_calls index. It
            // counts text blocks too, so it may not start at zero -- harmless,
            // because every accumulator treats it as an opaque grouping key
            // rather than an offset.
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0);
            match payload.get("type").and_then(Value::as_str) {
                // Anthropic announces a tool call before streaming its
                // arguments, so the name arrives here and the arguments arrive
                // as input_json_delta below.
                Some("content_block_start")
                    if payload
                        .pointer("/content_block/type")
                        .and_then(Value::as_str)
                        == Some("tool_use") =>
                {
                    Some(openai_tool_start(
                        index,
                        payload
                            .pointer("/content_block/id")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        payload
                            .pointer("/content_block/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ))
                }
                Some("content_block_delta") => {
                    if let Some(text) = payload.pointer("/delta/text").and_then(Value::as_str) {
                        return Some(openai_delta(text));
                    }
                    payload
                        .pointer("/delta/partial_json")
                        .and_then(Value::as_str)
                        .map(|partial| openai_tool_arguments(index, partial))
                }
                Some("message_stop") => Some("[DONE]".into()),
                Some("error") => {
                    Some(error_payload("The Anthropic stream returned an error").to_string())
                }
                _ => None,
            }
        }
        ProviderKind::Gemini => {
            let payload: Value = serde_json::from_str(data).ok()?;
            let text = payload
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
                .filter(|text| !text.is_empty());
            let finish = payload
                .pointer("/candidates/0/finishReason")
                .and_then(Value::as_str);
            // Gemini sends a complete functionCall rather than streaming its
            // arguments, so one chunk carries both the name and the whole
            // argument object.
            if let Some(call) = payload
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .and_then(|parts| parts.iter().find_map(|part| part.get("functionCall")))
            {
                let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                return Some(openai_tool_whole(0, name, &args.to_string()));
            }
            text.as_deref()
                .map(openai_delta)
                .or_else(|| finish.map(|_| "[DONE]".into()))
        }
    }
}
fn openai_delta(text: &str) -> String {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]})
        .to_string()
}

/// Announce a tool call: id and name, arguments still to come.
fn openai_tool_start(index: u64, id: &str, name: &str) -> String {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": index,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": ""},
    }]}, "finish_reason": null}]})
    .to_string()
}

/// A further slice of a tool call's arguments.
fn openai_tool_arguments(index: u64, partial: &str) -> String {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": index,
        "function": {"arguments": partial},
    }]}, "finish_reason": null}]})
    .to_string()
}

/// A tool call that arrived complete, for providers that do not stream them.
fn openai_tool_whole(index: u64, name: &str, arguments: &str) -> String {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": index,
        "id": format!("call_{index}"),
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }]}, "finish_reason": null}]})
    .to_string()
}
fn validate_chat(request: &ChatRequest) -> Result<(), &'static str> {
    if let Some(ai_coder) = &request.ai_coder
        && (!valid_provider_id(ai_coder) || ai_coder.chars().any(char::is_control))
    {
        return Err("ai_coder is invalid");
    }
    if request.messages.is_empty() {
        return Err("messages must contain at least one message");
    }
    if request.messages.len() > MAX_MESSAGES {
        return Err("too many messages");
    }
    if let Some(provider) = &request.provider
        && (!valid_provider_id(provider) || provider.chars().any(char::is_control))
    {
        return Err("provider is invalid");
    }
    if let Some(temperature) = request.temperature
        && !(0.0..=2.0).contains(&temperature)
    {
        return Err("temperature must be between 0 and 2");
    }
    if let Some(max_tokens) = request.max_tokens
        && !(1..=32768).contains(&max_tokens)
    {
        return Err("max_tokens must be between 1 and 32768");
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
fn coder_prompt(messages: &[ChatMessage], allow_writes: bool) -> String {
    let mut prompt = String::from(
        "You are responding through the Lumen AI chat interface. Never read or expose .env files, credentials, tokens, or authentication data. Do not commit, push, or rewrite git history. ",
    );
    if allow_writes {
        prompt.push_str("Workspace edits are explicitly enabled for this execution.\n\n");
    } else {
        prompt.push_str(
            "This is a read-only execution: do not modify files or run destructive commands.\n\n",
        );
    }
    for message in messages {
        let role = match message.role {
            Role::System => "SYSTEM",
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
            // A CLI coder has no tool protocol, so a transcript that already
            // contains tool traffic is flattened into labelled text. That keeps
            // a conversation readable if it moves between a provider and a
            // coder mid-thread; it does not make tools work here, which is why
            // offering them to a coder route is refused outright below.
            Role::Tool => "TOOL RESULT",
        };
        prompt.push_str(role);
        prompt.push_str(":\n");
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                prompt.push_str(&format!(
                    "(requested tool {} with {})\n",
                    call.name, call.arguments
                ));
            }
        }
        prompt.push_str(&message.content);
        prompt.push_str("\n\n");
    }
    prompt.push_str("ASSISTANT:\n");
    prompt
}
fn json_error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"type": kind, "message": message.into()}})),
    )
        .into_response()
}
fn sse_text(text: &str) -> Response {
    let body = format!("data: {}\n\ndata: [DONE]\n\n", openai_delta(text));
    let mut response = Response::new(Body::from(body));
    add_sse_headers(response.headers_mut());
    response
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

    fn provider(id: &str, kind: ProviderKind) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            name: id.into(),
            ai_coder: "hermes".into(),
            ai_coder_name: "Hermes".into(),
            provider_id: id.into(),
            kind,
            base_url: "https://example.test/v1".into(),
            default_model: "test-model".into(),
            api_key: Some("test-key".into()),
        }
    }
    fn request() -> ChatRequest {
        ChatRequest {
            ai_coder: None,
            provider: None,
            model: None,
            temperature: None,
            max_tokens: None,
            tools: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "Hello".into(),
                tool_calls: None,
                tool_call_id: None,
            }],
        }
    }

    #[test]
    fn validates_multi_provider_configuration() {
        let config = Config {
            bind: "127.0.0.1:41788".parse().unwrap(),
            providers: vec![
                provider("openai", ProviderKind::Openai),
                provider("local", ProviderKind::OpenaiCompatible),
            ],
            coders: Vec::new(),
            default_coder: "hermes".into(),
            default_provider: "local".into(),
            coder_workspace: PathBuf::from("."),
            coder_timeout: Duration::from_secs(30),
            coder_allow_writes: false,
            coder_max_concurrency: 2,
        };
        assert!(config.validate().is_ok());
        let mut remote = config.clone();
        remote.coders.push(coders::CoderRoute {
            kind: coders::CoderKind::Codex,
            provider: "codex".into(),
            provider_name: "Codex".into(),
            model: "default".into(),
            model_name: "Default".into(),
            executable: PathBuf::from("/usr/bin/false"),
        });
        remote.bind = "0.0.0.0:41788".parse().unwrap();
        assert!(matches!(remote.validate(), Err(ConfigError::RemoteBind)));
        let duplicate = Config {
            providers: vec![
                provider("same", ProviderKind::Openai),
                provider("same", ProviderKind::Gemini),
            ],
            ..config.clone()
        };
        assert!(matches!(
            duplicate.validate(),
            Err(ConfigError::DuplicateProvider(_))
        ));
        let mut duplicate_route_provider = provider("second", ProviderKind::OpenaiCompatible);
        duplicate_route_provider.provider_id = "openai".into();
        let duplicate_route = Config {
            providers: vec![
                provider("openai", ProviderKind::Openai),
                duplicate_route_provider,
            ],
            default_provider: "openai".into(),
            ..config
        };
        assert!(matches!(
            duplicate_route.validate(),
            Err(ConfigError::DuplicateRoute(route)) if route == "hermes/openai"
        ));
    }
    #[test]
    fn translates_all_native_streams() {
        let anthropic = translate_event(
            ProviderKind::Anthropic,
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#,
        )
        .unwrap();
        assert!(anthropic.contains(r#""content":"Hi""#));
        let gemini = translate_event(
            ProviderKind::Gemini,
            "message",
            r#"{"candidates":[{"content":{"parts":[{"text":"Hello "},{"text":"world"}]}}]}"#,
        )
        .unwrap();
        assert!(gemini.contains(r#""content":"Hello world""#));
        assert_eq!(
            translate_event(ProviderKind::Openai, "message", "[DONE]").unwrap(),
            "[DONE]"
        );
    }
    #[test]
    fn applies_provider_specific_authentication() {
        let client = Client::new();
        let openai = authenticated(
            client.get("https://example.test"),
            &provider("openai", ProviderKind::Openai),
        )
        .build()
        .unwrap();
        assert_eq!(
            openai.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer test-key"
        );
        let anthropic = authenticated(
            client.get("https://example.test"),
            &provider("anthropic", ProviderKind::Anthropic),
        )
        .build()
        .unwrap();
        assert_eq!(anthropic.headers().get("x-api-key").unwrap(), "test-key");
        assert_eq!(
            anthropic.headers().get("anthropic-version").unwrap(),
            "2023-06-01"
        );
        let gemini = authenticated(
            client.get("https://example.test"),
            &provider("gemini", ProviderKind::Gemini),
        )
        .build()
        .unwrap();
        assert_eq!(gemini.headers().get("x-goog-api-key").unwrap(), "test-key");
        assert!(gemini.url().query().is_none());
    }
    #[test]
    fn builds_generation_parameters_for_every_protocol() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "Be concise".into(),
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".into(),
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let openai = provider_payload(
            ProviderKind::Openai,
            "gpt",
            &messages,
            Some(0.4),
            Some(123),
            None,
        );
        assert!((openai["temperature"].as_f64().unwrap() - 0.4).abs() < 0.0001);
        assert_eq!(openai["max_tokens"], 123);
        let anthropic = provider_payload(
            ProviderKind::Anthropic,
            "claude",
            &messages,
            Some(0.4),
            Some(123),
            None,
        );
        assert_eq!(anthropic["system"], "Be concise");
        assert_eq!(anthropic["max_tokens"], 123);
        let gemini = provider_payload(
            ProviderKind::Gemini,
            "gemini",
            &messages,
            Some(0.4),
            Some(123),
            None,
        );
        assert_eq!(gemini["generationConfig"]["maxOutputTokens"], 123);
    }
    #[test]
    fn validates_generation_bounds() {
        let mut value = request();
        value.temperature = Some(2.1);
        assert_eq!(
            validate_chat(&value),
            Err("temperature must be between 0 and 2")
        );
        value.temperature = None;
        value.max_tokens = Some(0);
        assert_eq!(
            validate_chat(&value),
            Err("max_tokens must be between 1 and 32768")
        );
    }
    #[test]
    fn normalizes_provider_model_catalogs() {
        assert_eq!(
            normalize_models(
                ProviderKind::Anthropic,
                &json!({"data":[{"id":"claude","display_name":"Claude"}]})
            ),
            vec![("claude".into(), "Claude".into())]
        );
        assert_eq!(
            normalize_models(
                ProviderKind::Gemini,
                &json!({"models":[{"name":"models/gemini","displayName":"Gemini"}]})
            ),
            vec![("gemini".into(), "Gemini".into())]
        );
    }
    #[test]
    fn encodes_model_paths() {
        assert_eq!(encode_path_segment("gemini/foo bar"), "gemini%2Ffoo%20bar");
        assert_eq!(
            parse_model_path("hermes/z.ai/glm-5.2"),
            Some(("hermes", "z.ai", "glm-5.2"))
        );
        assert_eq!(
            parse_model_path("codex/codex/gpt-5.6"),
            Some(("codex", "codex", "gpt-5.6"))
        );
    }

    fn msg(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn tool() -> ToolDef {
        ToolDef {
            name: "search_feed".into(),
            description: "Search opportunities".into(),
            input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        }
    }

    /// A transcript that has already been through one round of tool use.
    fn transcript() -> Vec<ChatMessage> {
        vec![
            msg(Role::User, "any IT work in Virginia?"),
            ChatMessage {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "search_feed".into(),
                    arguments: json!({"q": "IT Virginia"}),
                }]),
                tool_call_id: None,
            },
            ChatMessage {
                role: Role::Tool,
                content: "3 results".into(),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
            },
        ]
    }

    #[test]
    fn openai_sends_tools_and_a_tool_role() {
        let tools = [tool()];
        let payload = provider_payload(
            ProviderKind::Openai,
            "gpt",
            &transcript(),
            None,
            None,
            Some(&tools),
        );
        assert_eq!(payload.pointer("/tools/0/type").unwrap(), "function");
        assert_eq!(
            payload.pointer("/tools/0/function/name").unwrap(),
            "search_feed"
        );
        // OpenAI wants arguments as a JSON string, not an object.
        let args = payload
            .pointer("/messages/1/tool_calls/0/function/arguments")
            .unwrap();
        assert!(args.is_string(), "arguments must be a string, got {args}");
        assert_eq!(payload.pointer("/messages/2/role").unwrap(), "tool");
        assert_eq!(
            payload.pointer("/messages/2/tool_call_id").unwrap(),
            "call_1"
        );
    }

    /// Anthropic has no tool role: a result is a user turn holding a
    /// tool_result block, and the arguments stay an object.
    #[test]
    fn anthropic_carries_a_tool_result_as_a_user_block() {
        let tools = [tool()];
        let payload = provider_payload(
            ProviderKind::Anthropic,
            "claude",
            &transcript(),
            None,
            None,
            Some(&tools),
        );
        assert_eq!(payload.pointer("/tools/0/name").unwrap(), "search_feed");
        assert!(payload.pointer("/tools/0/input_schema").is_some());
        assert_eq!(
            payload.pointer("/messages/1/content/0/type").unwrap(),
            "tool_use"
        );
        assert!(
            payload
                .pointer("/messages/1/content/0/input")
                .unwrap()
                .is_object()
        );
        assert_eq!(payload.pointer("/messages/2/role").unwrap(), "user");
        assert_eq!(
            payload.pointer("/messages/2/content/0/type").unwrap(),
            "tool_result"
        );
        assert_eq!(
            payload
                .pointer("/messages/2/content/0/tool_use_id")
                .unwrap(),
            "call_1"
        );
    }

    /// Gemini keys a response by function *name* while the transcript only
    /// carries the id, so the name has to be recovered from the earlier turn.
    #[test]
    fn gemini_recovers_the_function_name_from_the_call_id() {
        let tools = [tool()];
        let payload = provider_payload(
            ProviderKind::Gemini,
            "gemini",
            &transcript(),
            None,
            None,
            Some(&tools),
        );
        assert_eq!(
            payload
                .pointer("/tools/0/functionDeclarations/0/name")
                .unwrap(),
            "search_feed"
        );
        assert_eq!(
            payload
                .pointer("/contents/1/parts/0/functionCall/name")
                .unwrap(),
            "search_feed"
        );
        assert_eq!(
            payload
                .pointer("/contents/2/parts/0/functionResponse/name")
                .unwrap(),
            "search_feed",
            "the id must resolve back to the name"
        );
    }

    #[test]
    fn tools_are_omitted_when_none_are_offered() {
        let payload = provider_payload(
            ProviderKind::Openai,
            "gpt",
            &[msg(Role::User, "hi")],
            None,
            None,
            None,
        );
        assert!(payload.get("tools").is_none());
        let empty: [ToolDef; 0] = [];
        let payload = provider_payload(
            ProviderKind::Anthropic,
            "claude",
            &[msg(Role::User, "hi")],
            None,
            None,
            Some(&empty),
        );
        assert!(
            payload.get("tools").is_none(),
            "an empty list is not a tool offer"
        );
    }

    #[test]
    fn anthropic_streams_a_tool_call_in_two_parts() {
        let start = translate_event(
            ProviderKind::Anthropic,
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"search_feed","input":{}}}"#,
        )
        .expect("a tool_use block must not be dropped");
        let start: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(
            start.pointer("/choices/0/delta/tool_calls/0/id").unwrap(),
            "toolu_9"
        );
        assert_eq!(
            start
                .pointer("/choices/0/delta/tool_calls/0/function/name")
                .unwrap(),
            "search_feed"
        );

        let args = translate_event(
            ProviderKind::Anthropic,
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
        )
        .expect("argument deltas must not be dropped");
        let args: Value = serde_json::from_str(&args).unwrap();
        assert_eq!(
            args.pointer("/choices/0/delta/tool_calls/0/function/arguments")
                .unwrap(),
            "{\"q\":"
        );
        // Both halves must land on the same index so an accumulator joins them.
        assert_eq!(
            start.pointer("/choices/0/delta/tool_calls/0/index"),
            args.pointer("/choices/0/delta/tool_calls/0/index")
        );
    }

    #[test]
    fn gemini_streams_a_whole_tool_call_at_once() {
        let chunk = translate_event(
            ProviderKind::Gemini,
            "message",
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"search_feed","args":{"q":"IT"}}}]}}]}"#,
        )
        .expect("a functionCall must not be dropped");
        let chunk: Value = serde_json::from_str(&chunk).unwrap();
        assert_eq!(
            chunk
                .pointer("/choices/0/delta/tool_calls/0/function/name")
                .unwrap(),
            "search_feed"
        );
        let args = chunk
            .pointer("/choices/0/delta/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
            .unwrap();
        assert!(args.contains("\"q\""), "arguments arrive whole, got {args}");
    }

    /// The reason the normaliser was text-only is that text still has to work.
    #[test]
    fn plain_text_streaming_is_unchanged() {
        for (kind, event, data) in [
            (
                ProviderKind::Anthropic,
                "content_block_delta",
                r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#,
            ),
            (
                ProviderKind::Gemini,
                "message",
                r#"{"candidates":[{"content":{"parts":[{"text":"Hi"}]}}]}"#,
            ),
        ] {
            let chunk = translate_event(kind, event, data).expect("text must survive");
            let chunk: Value = serde_json::from_str(&chunk).unwrap();
            assert_eq!(chunk.pointer("/choices/0/delta/content").unwrap(), "Hi");
        }
    }
}
