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

pub mod coders;
pub mod harness;
pub mod identity;
pub mod quota;
pub mod store;
pub mod workspaces;

use crate::harness::{
    Harness, HarnessConfig, HarnessEvent,
    memory::HarnessMemory,
    tools::{ToolContext, ToolRegistry},
    workspace::{Workspace, WorkspaceError},
};
use crate::identity::{CurrentUser, Identity, ProxyHeaders};
use crate::quota::{Limits, Quotas};
use crate::store::{ConversationStore, StoreError, StoredConversation};
use crate::workspaces::{Tenancy, WorkspaceMode, Workspaces};

use async_stream::stream;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
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
use tokio::sync::{Semaphore, mpsc};
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Concurrent agent runs. Low on purpose: each is a loop of model calls holding
/// a workspace and an index, and the failure mode of setting it high is a
/// machine that stops answering anything.
const DEFAULT_AGENT_CONCURRENCY: usize = 4;
const MAX_MESSAGES: usize = 100;
const MAX_CONTENT_BYTES: usize = 256 * 1024;
const MAX_TOTAL_CONTENT_BYTES: usize = 768 * 1024;
const MAX_MODEL_BYTES: usize = 200;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_CUSTOM_PROVIDERS: usize = 20;
const DEFAULT_CODER_TIMEOUT_SECONDS: u64 = 300;
const DEFAULT_CODER_CONCURRENCY: usize = 2;
const DEFAULT_HARNESS_MAX_STEPS: usize = 12;
const DEFAULT_HARNESS_MEMORY_HITS: usize = 6;

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

/// How the agent harness behaves for this deployment.
///
/// Separate from the coder settings above because they answer a different
/// question: those decide how a *native CLI* is invoked, these decide how the
/// gateway itself drives a hosted model through a tool loop.
#[derive(Clone, Debug)]
pub struct HarnessSettings {
    /// Where the codebase index lives. `None` disables retrieval, and the
    /// harness then does not advertise `search_memory` at all.
    pub memory_dir: Option<PathBuf>,
    /// Whether failing to open the index should stop the service.
    ///
    /// True when an operator named the directory: they asked for retrieval, and
    /// silently starting without it would hide the misconfiguration. False for
    /// the default location, which is a guess -- and a wrong guess is routine,
    /// since the standard deployment mounts the workspace read-only.
    pub memory_required: bool,
    /// Build the index at startup rather than leaving it to a later request.
    pub index_on_start: bool,
    /// Model turns before the loop gives up, however many tools each one calls.
    pub max_steps: usize,
    /// Records retrieved before the first turn. Zero waits to be asked.
    pub memory_hits: usize,
}

impl Default for HarnessSettings {
    fn default() -> Self {
        Self {
            memory_dir: None,
            memory_required: false,
            index_on_start: false,
            max_steps: DEFAULT_HARNESS_MAX_STEPS,
            memory_hits: DEFAULT_HARNESS_MEMORY_HITS,
        }
    }
}

/// Where server-side conversations live, and how hard their absence is.
#[derive(Clone, Debug)]
pub struct StoreSettings {
    /// `None` derives the path from the workspace, the way the index does.
    pub path: Option<PathBuf>,
    /// Set when an operator named a path: failing to open one that was asked
    /// for is fatal, rather than quietly serving a service without history.
    pub required: bool,
    /// `false` keeps conversations in the browser, which is what every
    /// single-user deployment did before there was anywhere else to put them.
    pub enabled: bool,
}

impl Default for StoreSettings {
    fn default() -> Self {
        Self {
            path: None,
            required: false,
            enabled: true,
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
    /// Agent runs allowed to be in flight at once.
    ///
    /// A quota is a count over a window; it says nothing about simultaneity. An
    /// agent run is a dozen model calls, each of which may execute tools, so a
    /// handful started together is real load on one process with nothing between
    /// it and the machine.
    pub agent_max_concurrency: usize,
    /// Whether everyone shares one workspace or each user gets a subtree.
    pub workspace_mode: WorkspaceMode,
    /// What one person may spend. Unlimited unless configured.
    pub quotas: Limits,
    /// Whether the operator has said something else terminates TLS and
    /// authenticates in front of this process. Binding off loopback without it
    /// is refused.
    pub trusted_proxy: bool,
    pub harness: HarnessSettings,
    pub store: StoreSettings,
    /// How a request becomes a user. Single-user by default, which is the
    /// behaviour every deployment had before this existed.
    pub identity: Identity,
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
    #[error(
        "APP_BIND may only leave loopback for a deployment that is behind a \
         trusted proxy: set AI_IDENTITY_MODE=proxy and AI_TRUSTED_PROXY=true, \
         and make sure nothing but that proxy can reach this port"
    )]
    RemoteBind,
    #[error("AI_IDENTITY_MODE must be `single` or `proxy`, not `{0}`")]
    UnknownIdentityMode(String),
    #[error("AI_IDENTITY_USER_HEADER is not a usable header name: {0}")]
    InvalidIdentityHeader(String),
    #[error("AI_WORKSPACE_MODE must be `shared` or `per_user`, not `{0}`")]
    UnknownWorkspaceMode(String),
    #[error("{0}")]
    UnsafeTenancy(&'static str),
    #[error(
        "a bind off loopback needs AI_IDENTITY_SHARED_SECRET, set to a value \
         only the proxy knows and sent by it on every request: without it any \
         host that can reach this port can claim to be any user"
    )]
    UnprovenProxy,
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
        // Hosted providers win unless the operator explicitly selects a coder.
        // Discovering a native CLI must not silently reroute an API-configured
        // deployment through that CLI (and, for callers offering tools, coder
        // routes cannot satisfy the request at all).
        let default_coder = env::var("AI_DEFAULT_CODER")
            .unwrap_or_else(|_| default_coder_for_backends(&providers, &coders));
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
        let workspace_mode = match env::var("AI_WORKSPACE_MODE")
            .unwrap_or_else(|_| "shared".into())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "shared" => WorkspaceMode::Shared,
            "per_user" | "per-user" => WorkspaceMode::PerUser,
            other => return Err(ConfigError::UnknownWorkspaceMode(other.into())),
        };
        let requested_workspace = workspace_text
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        // In shared mode this names a project, which has to be there already. In
        // per-user mode it names the parent the subtrees hang off, which the
        // service creates -- a fresh deployment has an empty volume and nobody
        // to have made it.
        let coder_workspace = match workspace_mode {
            WorkspaceMode::Shared => {
                let resolved = requested_workspace.canonicalize().map_err(|_| {
                    ConfigError::InvalidCoderWorkspace(
                        workspace_text.clone().unwrap_or_else(|| ".".into()),
                    )
                })?;
                if !resolved.is_dir() {
                    return Err(ConfigError::InvalidCoderWorkspace(
                        resolved.display().to_string(),
                    ));
                }
                resolved
            }
            WorkspaceMode::PerUser => requested_workspace,
        };
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
        // The index lives under `data/`, which the workspace exclusion list
        // already hides: without that the indexer would walk its own segments
        // and the read-only workspace copy would carry them into every CLI run.
        let (memory_dir, memory_required) = if env_disabled("AI_HARNESS_MEMORY") {
            (None, false)
        } else {
            match env::var("AI_HARNESS_MEMORY_DIR") {
                Ok(configured) => (Some(PathBuf::from(configured)), true),
                Err(_) => (Some(coder_workspace.join("data/memory")), false),
            }
        };
        let harness = HarnessSettings {
            memory_dir,
            memory_required,
            index_on_start: env_flag("AI_HARNESS_INDEX_ON_START"),
            max_steps: env::var("AI_HARNESS_MAX_STEPS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| (1..=50).contains(value))
                .unwrap_or(DEFAULT_HARNESS_MAX_STEPS),
            memory_hits: env::var("AI_HARNESS_MEMORY_HITS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value <= 25)
                .unwrap_or(DEFAULT_HARNESS_MEMORY_HITS),
        };
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
            agent_max_concurrency: env::var("AI_AGENT_MAX_CONCURRENCY")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| (1..=64).contains(value))
                .unwrap_or(DEFAULT_AGENT_CONCURRENCY),
            harness,
            workspace_mode,
            trusted_proxy: env_flag("AI_TRUSTED_PROXY"),
            quotas: Limits {
                requests: env::var("AI_QUOTA_REQUESTS_PER_WINDOW")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
                tokens: env::var("AI_QUOTA_TOKENS_PER_WINDOW")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
                window: Duration::from_secs(
                    env::var("AI_QUOTA_WINDOW_SECONDS")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .filter(|seconds| (60..=86_400).contains(seconds))
                        .unwrap_or(3600),
                ),
            },
            store: StoreSettings {
                path: env::var("AI_STORE_PATH").ok().map(PathBuf::from),
                required: env::var("AI_STORE_PATH").is_ok(),
                enabled: !env_disabled("AI_STORE"),
            },
            identity: identity_from_env()?,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.providers.is_empty() && self.coders.is_empty() {
            return Err(ConfigError::NoProviders);
        }
        // Loopback stays the default, and leaving it is an explicit act.
        //
        // Identity here is asserted by a header, which is only worth anything if
        // the only thing that can set it is the proxy in front. On loopback that
        // is a property of the machine. Off it, it is a property of the network,
        // which this process cannot check -- so the most it can do is refuse
        // unless an operator has said both that a proxy authenticates in front
        // and that this is deliberate.
        if !self.bind.ip().is_loopback() {
            if !(self.trusted_proxy && self.identity.is_multi_user()) {
                return Err(ConfigError::RemoteBind);
            }
            // On loopback, "only the proxy can set the header" is a property of
            // the machine. Off it, it is a property of the network, and the one
            // thing this process can actually check is that the sender knows
            // something only the proxy was told. So off loopback the secret is
            // required rather than encouraged.
            if !self.identity.has_shared_secret() {
                return Err(ConfigError::UnprovenProxy);
            }
        }
        // Refused before the service is listening, because no runtime check
        // makes a shared writable workspace safe to share between people.
        workspaces::validate(
            self.workspace_mode,
            self.identity.is_multi_user(),
            self.coder_allow_writes,
        )
        .map_err(ConfigError::UnsafeTenancy)?;
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

    /// The route a request names, applying every default in one place.
    fn route_for<'a>(&'a self, request: &'a ChatRequest) -> Route<'a> {
        let embedded = request.model.as_deref().and_then(parse_model_path);
        let coder = request
            .ai_coder
            .as_deref()
            .or_else(|| embedded.map(|(coder, _, _)| coder))
            .unwrap_or(&self.default_coder);
        Route {
            coder,
            provider: request
                .provider
                .as_deref()
                .or_else(|| embedded.map(|(_, provider, _)| provider))
                .or_else(|| {
                    (coder == self.default_coder).then_some(self.default_provider.as_str())
                }),
            model: embedded
                .map(|(_, _, model)| model)
                .or(request.model.as_deref()),
        }
    }

    /// The native CLI serving this route, if one does.
    fn native_route(&self, route: &Route<'_>) -> Option<&coders::CoderRoute> {
        route
            .provider
            .and_then(|provider| self.coder_route(route.coder, provider, route.model))
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

/// The route a request selects, before any decision about how to serve it.
///
/// Resolving this once is what keeps `complete` and `execution_for` from
/// disagreeing about where a request would have gone — a disagreement the
/// harness would experience as the gateway refusing tools it had just been
/// told the route accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Route<'a> {
    coder: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
}

/// How a request will actually be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Execution {
    /// A hosted provider API. The harness offers tools and runs them itself.
    Hosted,
    /// A native coder CLI, which runs its own agent loop with its own tools.
    /// The harness delegates the turn rather than driving it.
    Native,
}

fn default_coder_for_backends(
    providers: &[ProviderConfig],
    coders: &[coders::CoderRoute],
) -> String {
    providers
        .first()
        .map(|provider| provider.ai_coder.clone())
        .or_else(|| coders.first().map(|route| route.kind.id().to_string()))
        .unwrap_or_else(|| "hermes".into())
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
/// Whether an opt-out flag is set. Distinct from `env_flag`: this feature is on
/// by default, so an unset variable must not read as "off".
fn env_disabled(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no" | "disabled"
            )
        })
        .unwrap_or(false)
}
/// Identity is single-user unless a deployment says otherwise, because trusting
/// a header is only safe once something in front is setting it.
fn identity_from_env() -> Result<Identity, ConfigError> {
    let mode = env::var("AI_IDENTITY_MODE").unwrap_or_else(|_| "single".into());
    match mode.trim().to_ascii_lowercase().as_str() {
        "single" => Ok(Identity::single_user()),
        "proxy" => {
            let defaults = ProxyHeaders::default();
            let user_header = env::var("AI_IDENTITY_USER_HEADER")
                .map(|value| value.trim().to_ascii_lowercase())
                .unwrap_or(defaults.user_header);
            if user_header.is_empty()
                || axum::http::HeaderName::from_bytes(user_header.as_bytes()).is_err()
            {
                return Err(ConfigError::InvalidIdentityHeader(user_header));
            }
            let secret = match env::var("AI_IDENTITY_SHARED_SECRET") {
                Ok(value) if value.trim().is_empty() => None,
                Ok(value) => Some(identity::Secret::new(
                    optional_header("AI_IDENTITY_SECRET_HEADER", None)
                        .unwrap_or_else(|| "x-lumen-proxy-secret".into()),
                    value.trim(),
                )),
                Err(_) => None,
            };
            Ok(Identity::proxy(ProxyHeaders {
                user_header,
                email_header: optional_header("AI_IDENTITY_EMAIL_HEADER", defaults.email_header),
                name_header: optional_header("AI_IDENTITY_NAME_HEADER", defaults.name_header),
                secret,
            }))
        }
        other => Err(ConfigError::UnknownIdentityMode(other.into())),
    }
}

/// An explicitly empty value turns a display field off; an unset one keeps the
/// default, so a deployment that names only its user header still gets the rest.
fn optional_header(key: &str, fallback: Option<String>) -> Option<String> {
    match env::var(key) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(value.trim().to_ascii_lowercase()),
        Err(_) => fallback,
    }
}

/// A short, unique-enough id for one request.
///
/// Not a UUID: this identifies a line in a log for as long as somebody is
/// reading it, and pulling in a dependency to make it prettier would be the
/// wrong trade. Time plus a counter cannot collide within a process, and the
/// process id separates two of them.
pub fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as u64);
    format!("{:x}-{:x}-{n:x}", std::process::id(), micros)
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
    agent_slots: Arc<Semaphore>,
    metrics: Arc<AppMetrics>,
    /// Which files a given user is allowed to touch, and the index over them.
    workspaces: Arc<Workspaces>,
    identity: Identity,
    quotas: Arc<Quotas>,
    /// `None` when conversations are kept in the browser, or when the store
    /// could not be opened and was not required.
    store: Option<ConversationStore>,
}

impl axum::extract::FromRef<AppState> for Identity {
    fn from_ref(state: &AppState) -> Self {
        state.identity.clone()
    }
}

impl AppState {
    /// A harness over this state.
    ///
    /// Built per request rather than held in the state, because a `Harness`
    /// owns an `Arc<Gateway>` and a `Gateway` owns this state: keeping one here
    /// would be a reference cycle that never drops. Construction is cheap --
    /// the index and the HTTP client are shared, not rebuilt.
    /// The files this user may touch, or the reason they may not.
    fn tenancy(&self, user: &identity::User) -> Result<Tenancy, WorkspaceRejection> {
        self.workspaces.for_user(user).map_err(|error| {
            tracing::error!(%error, user = user.id, "could not resolve a workspace");
            WorkspaceRejection
        })
    }

    fn harness_for(&self, tenancy: &Tenancy) -> Harness {
        let context = ToolContext {
            workspace: tenancy.workspace.clone(),
            memory: tenancy.memory.clone(),
        };
        Harness::new(
            Arc::new(Gateway {
                state: self.clone(),
                // A native coder runs where this user's files are, not where
                // the process happened to be configured.
                coder_root: tenancy.workspace.root().to_path_buf(),
            }),
            context,
            HarnessConfig {
                max_steps: self.config.harness.max_steps,
                budget: harness::transcript::Budget::default(),
                memory_hits: self.config.harness.memory_hits,
                inject_memory: self.config.harness.memory_hits > 0,
            },
        )
    }
}

#[derive(Default)]
struct AppMetrics {
    chat_requests: AtomicU64,
    agent_requests: AtomicU64,
    tool_calls: AtomicU64,
    coder_busy_rejections: AtomicU64,
    agent_busy_rejections: AtomicU64,
    quota_rejections: AtomicU64,
    tokens_charged: AtomicU64,
}

/// The API routes, mounted bare.
///
/// A composable router should not decide where it lives. `build_router` mounts
/// these at `/api` for standalone use; an embedding host nests them wherever it
/// keeps its own API, and applies its own authentication on the way in --
/// nothing here authenticates anything.
pub fn build_api_router(config: Config) -> Result<Router, BuildError> {
    let state = gateway_state(config)?;
    Ok(Router::new()
        .route("/me", get(current_user))
        .route(
            "/conversations",
            get(list_conversations).delete(clear_conversations),
        )
        .route(
            "/conversations/{id}",
            get(read_conversation)
                .put(save_conversation)
                .delete(remove_conversation),
        )
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/providers", get(providers))
        .route("/coders", get(coder_statuses))
        .route("/models", get(models))
        .route("/chat", post(chat))
        .route("/agent", post(agent))
        .route("/harness", get(harness_status))
        .route("/harness/index", post(reindex))
        .with_state(state)
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BYTES))
        // Every line a request produces carries the same id, and the caller
        // gets it back. Without that, "my agent run failed" cannot be turned
        // into the lines that describe it -- a shared log has no thread to pull.
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &axum::http::Request<_>| {
                let id = request
                    .headers()
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .filter(|value| !value.is_empty() && value.len() <= 128)
                    .map(str::to_owned)
                    .unwrap_or_else(new_request_id);
                tracing::info_span!(
                    "request",
                    method = %request.method(),
                    path = %request.uri().path(),
                    request_id = %id,
                )
            }),
        ))
}

fn gateway_state(config: Config) -> Result<AppState, BuildError> {
    config.validate()?;
    let coder_slots = Arc::new(Semaphore::new(config.coder_max_concurrency));
    let agent_slots = Arc::new(Semaphore::new(config.agent_max_concurrency));
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
    let workspaces = match config.workspace_mode {
        WorkspaceMode::Shared => {
            let workspace = Workspace::new(&config.coder_workspace, config.coder_allow_writes)?;
            let memory = open_memory(&config, &workspace)?;
            Workspaces::shared(workspace, memory)
        }
        // Each subtree is created on demand, so nothing is built here except the
        // base it hangs off.
        WorkspaceMode::PerUser => Workspaces::per_user(
            &config.coder_workspace,
            config.coder_allow_writes,
            config.harness.memory_dir.clone(),
        )?,
    };
    let store = open_store(&config)?;
    let state = AppState {
        store,
        quotas: Arc::new(Quotas::new(config.quotas)),
        identity: config.identity.clone(),
        config: Arc::new(config),
        client,
        coder_slots,
        agent_slots,
        metrics: Arc::new(AppMetrics::default()),
        workspaces: Arc::new(workspaces),
    };
    Ok(state)
}

/// Open the codebase index, and start building it if asked to.
///
/// Opening is cheap enough to do inline -- it maps the segments and rebuilds the
/// graph from rows already committed. Indexing a whole tree is not, so it runs
/// on a blocking thread and the service starts serving immediately: an index
/// that is still filling answers fewer queries, which is a better failure than
/// a gateway that will not start.
/// Open the conversation store, or decide to live without one.
///
/// The same rule as the index: a path an operator named is a promise, so
/// failing to open it stops the service. A derived one is a convenience, and a
/// read-only workspace losing it should not take chat down with it.
fn open_store(config: &Config) -> Result<Option<ConversationStore>, BuildError> {
    if !config.store.enabled {
        return Ok(None);
    }
    let path = config
        .store
        .path
        .clone()
        .unwrap_or_else(|| config.coder_workspace.join("data/conversations.db"));

    match ConversationStore::open(&path) {
        Ok(store) => {
            tracing::info!(
                path = %path.display(),
                conversations = store.total().unwrap_or(0),
                "conversation store opened"
            );
            Ok(Some(store))
        }
        Err(error) if !config.store.required => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "conversation store unavailable; conversations stay in the browser"
            );
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn open_memory(
    config: &Config,
    workspace: &Workspace,
) -> Result<Option<Arc<HarnessMemory>>, BuildError> {
    let Some(directory) = &config.harness.memory_dir else {
        return Ok(None);
    };
    let memory = match HarnessMemory::open(directory, workspace.root()) {
        Ok(memory) => Arc::new(memory),
        // The default location is a guess, and the standard deployment mounts
        // the workspace read-only, so a failure there is a reason to run
        // without retrieval rather than a reason not to start. Say so loudly:
        // `/harness` will report no memory, and `/harness/index` will explain.
        Err(error) if !config.harness.memory_required => {
            tracing::warn!(
                %error,
                directory = %directory.display(),
                "codebase memory unavailable; the harness will run without retrieval. \
                 Set AI_HARNESS_MEMORY_DIR to a writable path to enable it"
            );
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    tracing::info!(
        records = memory.live_count(),
        directory = %directory.display(),
        "codebase memory opened"
    );

    if config.harness.index_on_start {
        let memory = Arc::clone(&memory);
        // `spawn_blocking` panics with no reactor, and `Config` is usable
        // outside one -- a test constructing a gateway directly, for instance.
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                tokio::task::spawn_blocking(move || match memory.index_workspace() {
                    Ok(stats) => tracing::info!(
                        files = stats.files,
                        records = stats.records,
                        skipped = stats.skipped,
                        "codebase index built"
                    ),
                    Err(error) => tracing::warn!(%error, "codebase indexing failed"),
                });
            }
            Err(_) => tracing::warn!("no reactor available; skipping startup indexing"),
        }
    }
    Ok(Some(memory))
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
    #[error("the harness workspace is unusable: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("codebase memory is unusable: {0}")]
    Memory(#[from] harness::memory::HarnessMemoryError),
    #[error("the conversation store is unusable: {0}")]
    Store(#[from] StoreError),
    #[error("the per-user workspaces are unusable: {0}")]
    Tenancy(#[from] workspaces::TenancyError),
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

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------

/// Conversations are the one thing here that belongs to a person rather than to
/// the service, so every handler below names the user it is acting for and the
/// store has no way to answer without one.
///
/// SQLite is synchronous, and the reactor is serving every other request on this
/// process, so each call goes to `spawn_blocking` the way the index and the file
/// tools do.
/// A workspace that could not be opened. Small on purpose: a `Response` in an
/// `Err` makes every `Result` in the call chain the size of a response.
struct WorkspaceRejection;

impl IntoResponse for WorkspaceRejection {
    fn into_response(self) -> Response {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "workspace_unavailable",
            "your workspace could not be opened",
        )
    }
}

enum StoreRejection {
    Unavailable,
    Invalid(String),
    Failed,
}

impl IntoResponse for StoreRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Unavailable => json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "store_unavailable",
                "server-side conversations are not configured; set AI_STORE_PATH to a writable path",
            ),
            Self::Invalid(message) => {
                json_error(StatusCode::BAD_REQUEST, "invalid_request", message)
            }
            Self::Failed => json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store_failed",
                "the conversation store failed",
            ),
        }
    }
}

async fn with_store<T, F>(state: &AppState, work: F) -> Result<T, StoreRejection>
where
    T: Send + 'static,
    F: FnOnce(ConversationStore) -> Result<T, StoreError> + Send + 'static,
{
    let store = state.store.clone().ok_or(StoreRejection::Unavailable)?;
    match tokio::task::spawn_blocking(move || work(store)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(StoreError::Invalid(message))) => Err(StoreRejection::Invalid(message)),
        Ok(Err(error)) => {
            tracing::error!(%error, "the conversation store failed");
            Err(StoreRejection::Failed)
        }
        Err(error) => {
            tracing::error!(%error, "the conversation store task did not finish");
            Err(StoreRejection::Failed)
        }
    }
}

async fn list_conversations(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Response {
    match with_store(&state, move |store| store.list(&user.id)).await {
        Ok(conversations) => Json(json!({ "conversations": conversations })).into_response(),
        Err(rejection) => rejection.into_response(),
    }
}

/// A conversation belonging to someone else is answered as missing rather than
/// as forbidden: telling a caller that an id exists but is not theirs is itself
/// something they should not learn.
async fn read_conversation(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Response {
    match with_store(&state, move |store| store.get(&user.id, &id)).await {
        Ok(Some(conversation)) => Json(conversation).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "not_found", "no such conversation"),
        Err(rejection) => rejection.into_response(),
    }
}

/// The client owns the transcript and sends it whole, so this replaces rather
/// than merges. Anything else races the stream that is still being written.
async fn save_conversation(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    payload: Result<Json<StoredConversation>, JsonRejection>,
) -> Response {
    let Json(mut conversation) = match payload {
        Ok(value) => value,
        Err(rejection) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                rejection.body_text(),
            );
        }
    };
    // The path is the identifier; a body disagreeing with it is a bug in the
    // client, not a second opinion about which conversation this is.
    if !conversation.id.is_empty() && conversation.id != id {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the conversation id in the body does not match the one in the path",
        );
    }
    conversation.id = id;

    match with_store(&state, move |store| {
        store.save(&user.id, &conversation)?;
        Ok(conversation)
    })
    .await
    {
        Ok(saved) => (StatusCode::OK, Json(saved)).into_response(),
        Err(rejection) => rejection.into_response(),
    }
}

async fn remove_conversation(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Response {
    match with_store(&state, move |store| store.delete(&user.id, &id)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_error(StatusCode::NOT_FOUND, "not_found", "no such conversation"),
        Err(rejection) => rejection.into_response(),
    }
}

async fn clear_conversations(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Response {
    match with_store(&state, move |store| store.delete_all(&user.id)).await {
        Ok(deleted) => Json(json!({ "deleted": deleted })).into_response(),
        Err(rejection) => rejection.into_response(),
    }
}

/// Who the gateway thinks is asking, and whether it is serving more than one
/// person. The client needs both: a name to show, and whether to offer anything
/// that only makes sense when accounts exist.
async fn current_user(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Json<Value> {
    Json(json!({
        "user": user,
        "multi_user": state.identity.is_multi_user(),
        // Whether this deployment keeps conversations. The client asks here
        // rather than by trying the store and reading a 503, which every browser
        // reports as a failed request on an ordinary page load.
        "conversations": state.store.is_some(),
        "quota": state.config.quotas.enabled().then(|| {
            let (requests, tokens) = state.quotas.usage(&user.id);
            json!({
                "requests_used": requests,
                "requests_limit": state.config.quotas.requests,
                "tokens_used": tokens,
                "tokens_limit": state.config.quotas.tokens,
                "window_seconds": state.config.quotas.window.as_secs(),
            })
        }),
    }))
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
/// Whether this instance can serve.
///
/// Compose gates the web container on this, so echoing configuration back was
/// worse than useless: it put a broken deployment behind a healthy-looking
/// front door. The store is the one dependency whose loss is silent -- a volume
/// can be unmounted or truncated under a running process -- so it is the one
/// this asks about.
///
/// Providers are deliberately not probed. They are remote, they fail
/// independently, and a readiness check that goes unready because somebody
/// else's API is slow takes down a service that could still answer from a
/// native coder.
async fn ready(State(state): State<AppState>) -> Response {
    let store = match &state.store {
        None => "disabled",
        Some(store) if store.is_usable() => "ok",
        Some(_) => "failing",
    };
    let healthy = store != "failing";
    let body = Json(json!({
        "status": if healthy { "ready" } else { "unavailable" },
        "store": store,
        "default_ai_coder": state.config.default_coder,
        "default_provider": state.config.default_provider,
    }));
    if healthy {
        body.into_response()
    } else {
        tracing::error!("the conversation store has stopped answering; reporting unready");
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}
async fn metrics(State(state): State<AppState>) -> Response {
    let body = format!(
        concat!(
            "# TYPE lumen_chat_requests_total counter\n",
            "lumen_chat_requests_total {}\n",
            "# TYPE lumen_agent_requests_total counter\n",
            "lumen_agent_requests_total {}\n",
            "# TYPE lumen_agent_tool_calls_total counter\n",
            "lumen_agent_tool_calls_total {}\n",
            "# TYPE lumen_memory_records gauge\n",
            "lumen_memory_records {}\n",
            "# TYPE lumen_quota_rejections_total counter\n",
            "lumen_quota_rejections_total {}\n",
            "# TYPE lumen_tokens_charged_total counter\n",
            "lumen_tokens_charged_total {}\n",
            "# TYPE lumen_agent_busy_rejections_total counter\n",
            "lumen_agent_busy_rejections_total {}\n",
            "# TYPE lumen_agent_slots_available gauge\n",
            "lumen_agent_slots_available {}\n",
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
        state.metrics.agent_requests.load(Ordering::Relaxed),
        state.metrics.tool_calls.load(Ordering::Relaxed),
        state.workspaces.live_records(),
        state.metrics.quota_rejections.load(Ordering::Relaxed),
        state.metrics.tokens_charged.load(Ordering::Relaxed),
        state.metrics.agent_busy_rejections.load(Ordering::Relaxed),
        state.agent_slots.available_permits(),
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

/// Refuse a request that would spend more than this user is allowed.
///
/// 429 with `Retry-After`, because the caller can succeed later without changing
/// anything -- which is exactly what that status is for.
fn over_quota(state: &AppState, user: &identity::User) -> Option<Response> {
    let refused = state.quotas.admit(&user.id).err()?;
    state
        .metrics
        .quota_rejections
        .fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        user = user.id,
        limit = refused.limit,
        used = refused.used,
        "quota exceeded"
    );

    let mut response = json_error(
        StatusCode::TOO_MANY_REQUESTS,
        "quota_exceeded",
        refused.message(),
    );
    if let Ok(value) = HeaderValue::from_str(&refused.retry_after.as_secs().max(1).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    Some(response)
}

async fn chat(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
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
    if let Some(refusal) = over_quota(&state, &user) {
        return refusal;
    }
    state.metrics.chat_requests.fetch_add(1, Ordering::Relaxed);
    // Resolved up front: a native coder runs in this user's workspace, and in a
    // per-user deployment that is the only thing keeping it out of everyone
    // else's.
    let coder_root = match state.tenancy(&user) {
        Ok(tenancy) => tenancy.workspace.root().to_path_buf(),
        Err(rejection) => return rejection.into_response(),
    };
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
            &coder_root,
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
    let (provider_kind, upstream) = match open_upstream(&state, &request).await {
        Ok(pair) => pair,
        Err(error) => return sse_error(&error.to_string()),
    };

    let kind = provider_kind;
    let source = upstream.bytes_stream().eventsource();
    let quotas = Arc::clone(&state.quotas);
    let metrics = Arc::clone(&state.metrics);
    let charged_to = user.id.clone();
    let output = stream! {
        pin_mut!(source);
        let mut sent_done = false;
        let mut counted = 0u64;
        while let Some(event) = source.next().await {
            match event {
                Ok(event) => {
                    if let Some(usage) = usage_from_event(kind, &event.data) {
                        counted = apply_usage(counted, usage);
                    }
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
        // Charged at the end rather than up front: this is what the run actually
        // cost, and a run that reported nothing is charged nothing.
        if counted > 0 {
            quotas.charge(&charged_to, counted);
            metrics.tokens_charged.fetch_add(counted, Ordering::Relaxed);
        }
    };
    let mut response = Response::new(Body::from_stream(output));
    add_sse_headers(response.headers_mut());
    response
}

/// Run the agent harness, streaming what it does as it does it.
///
/// `/chat` streams a model's text and hands any tool calls back for the caller
/// to run. This runs them, which is the whole difference: the response is a
/// record of an agent working, not a transcript of one asking.
///
/// The frames are JSON objects tagged by `event`, terminated by `[DONE]` like
/// every other stream this service produces, so a client that only knows how to
/// read text can still follow the `text` frames and ignore the rest.
async fn agent(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
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
    if let Some(refusal) = over_quota(&state, &user) {
        return refusal;
    }
    state.metrics.agent_requests.fetch_add(1, Ordering::Relaxed);

    let tenancy = match state.tenancy(&user) {
        Ok(tenancy) => tenancy,
        Err(rejection) => return rejection.into_response(),
    };

    // Held for the whole run, not just its first turn. The permit moves into
    // the task below so the slot comes back when the loop ends -- including
    // when it ends by failing.
    let Ok(permit) = Arc::clone(&state.agent_slots).try_acquire_owned() else {
        state
            .metrics
            .agent_busy_rejections
            .fetch_add(1, Ordering::Relaxed);
        tracing::info!(user = user.id, "agent slots exhausted");
        let mut response = json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_busy",
            "all agent slots are busy; try again shortly",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        return response;
    };

    let metrics = Arc::clone(&state.metrics);
    let harness = state.harness_for(&tenancy);
    let (sender, mut receiver) = mpsc::unbounded_channel();
    // The run owns the sender, so the channel closes when the run ends and the
    // loop below terminates without needing a sentinel.
    let run = tokio::spawn(async move {
        let outcome = harness.run(request, Some(sender)).await;
        drop(permit);
        outcome
    });

    let output = stream! {
        while let Some(event) = receiver.recv().await {
            if matches!(event, HarnessEvent::ToolResult { .. }) {
                metrics.tool_calls.fetch_add(1, Ordering::Relaxed);
            }
            yield Ok::<Bytes, std::io::Error>(frame(&event));
        }
        let closing = match run.await {
            Ok(Ok(outcome)) => json!({
                "event": "outcome",
                "text": outcome.text,
                "steps": outcome.steps,
                "tool_calls": outcome.tool_calls,
                "stopped_at_step_limit": outcome.stopped_at_step_limit,
            }),
            Ok(Err(error)) => {
                tracing::warn!(%error, "harness run failed");
                error_payload(&error.to_string())
            }
            Err(error) => {
                tracing::error!(%error, "harness task did not finish");
                error_payload("The agent harness stopped unexpectedly")
            }
        };
        yield Ok(frame(&closing));
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };

    let mut response = Response::new(Body::from_stream(output));
    add_sse_headers(response.headers_mut());
    response
}

/// What the harness can do here, so a client can show it rather than guess.
async fn harness_status(State(state): State<AppState>, CurrentUser(user): CurrentUser) -> Response {
    let tenancy = match state.tenancy(&user) {
        Ok(tenancy) => tenancy,
        Err(rejection) => return rejection.into_response(),
    };
    let context = ToolContext {
        workspace: tenancy.workspace.clone(),
        memory: tenancy.memory.clone(),
    };
    Json(json!({
        "workspace": tenancy.workspace.root().display().to_string(),
        "writes_enabled": tenancy.workspace.allows_writes(),
        "max_steps": state.config.harness.max_steps,
        "memory": tenancy.memory.as_ref().map(|memory| json!({
            "records": memory.live_count(),
            "retrieved_per_turn": state.config.harness.memory_hits,
            // An index the harness has not been told about has silently stopped
            // describing the tree. Saying so is the difference between a stale
            // answer and a wrong one.
            "stale": !memory.is_fresh(),
        })),
        "tools": ToolRegistry::builtin(&context).names(),
    }))
    .into_response()
}

/// Rebuild the codebase index.
///
/// Idempotent: `index_file` tombstones a path's old rows before writing new
/// ones, so running this twice replaces rows rather than duplicating them.
async fn reindex(State(state): State<AppState>, CurrentUser(user): CurrentUser) -> Response {
    let tenancy = match state.tenancy(&user) {
        Ok(tenancy) => tenancy,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(memory) = tenancy.memory.clone() else {
        return json_error(
            StatusCode::CONFLICT,
            "memory_disabled",
            "codebase memory is disabled; set AI_HARNESS_MEMORY_DIR to enable it",
        );
    };
    // A whole-tree index is minutes of CPU on a large repository, and the
    // reactor is also serving every other request on this process.
    match tokio::task::spawn_blocking(move || memory.index_workspace()).await {
        Ok(Ok(stats)) => Json(json!({
            "files": stats.files,
            "records": stats.records,
            "skipped": stats.skipped,
        }))
        .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "codebase indexing failed");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "index_failed",
                "codebase indexing failed",
            )
        }
        Err(error) => {
            tracing::error!(%error, "indexing task did not finish");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "index_failed",
                "codebase indexing did not finish",
            )
        }
    }
}

/// One SSE frame carrying a JSON value.
fn frame(value: &impl Serialize) -> Bytes {
    match serde_json::to_string(value) {
        Ok(json) => Bytes::from(format!("data: {json}\n\n")),
        Err(error) => Bytes::from(format!(
            "data: {}\n\n",
            error_payload(&format!("failed to encode a harness event: {error}"))
        )),
    }
}

/// Why a completion could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum CompletionError {
    #[error("invalid chat request: {0}")]
    InvalidRequest(&'static str),
    #[error("ai_coder/provider fields conflict with the canonical model path")]
    ConflictingRoute,
    #[error("AI coder/provider route `{0}` is not configured")]
    UnknownRoute(String),
    #[error("model is invalid")]
    InvalidModel,
    #[error("The selected AI provider is unavailable")]
    Unavailable,
    #[error("The selected AI provider timed out")]
    TimedOut,
    #[error("The selected AI provider returned {0}")]
    Upstream(StatusCode),
    #[error("The selected AI provider returned an invalid stream")]
    NotStreaming,
    #[error("The selected AI provider stream was interrupted")]
    StreamInterrupted,
    #[error("The selected AI provider returned invalid arguments for tool `{0}`")]
    InvalidToolArguments(String),
    #[error("The selected AI provider named a tool that cannot be routed: `{0}`")]
    InvalidToolName(String),
    #[error("Tool calling is not supported on AI coder routes; select a hosted provider")]
    ToolsOnCoderRoute,
    #[error("All AI coder execution slots are busy; try again shortly")]
    CoderBusy,
    #[error("The selected AI coder timed out")]
    CoderTimedOut,
    #[error("The selected AI coder failed; verify its native CLI authentication")]
    CoderFailed,
}

/// Resolve the route, build the payload and open the upstream stream.
///
/// Shared by the streaming endpoint and [`Gateway::complete`] so the two cannot
/// disagree about which provider a request reaches or what it is sent.
async fn open_upstream(
    state: &AppState,
    request: &ChatRequest,
) -> Result<(ProviderKind, reqwest::Response), CompletionError> {
    let embedded = request.model.as_deref().and_then(parse_model_path);
    let requested_coder = embedded
        .map(|(coder, _, _)| coder)
        .or(request.ai_coder.as_deref())
        .unwrap_or(&state.config.default_coder);
    let requested_provider = embedded
        .map(|(_, provider, _)| provider)
        .or(request.provider.as_deref())
        .or(Some(state.config.default_provider.as_str()));
    let requested_model = embedded
        .map(|(_, _, model)| model)
        .or(request.model.as_deref());

    let provider = requested_provider
        .and_then(|provider_id| state.config.provider_route(requested_coder, provider_id))
        .ok_or_else(|| {
            CompletionError::UnknownRoute(format!(
                "{requested_coder}/{}",
                requested_provider.unwrap_or("*")
            ))
        })?;

    let model = requested_model.unwrap_or(&provider.default_model);
    if model.trim().is_empty()
        || model.len() > MAX_MODEL_BYTES
        || model.chars().any(char::is_control)
    {
        return Err(CompletionError::InvalidModel);
    }

    let payload = provider_payload(
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
            .json(&payload),
        provider,
    );
    let upstream = match tokio::time::timeout(Duration::from_secs(30), upstream_request.send())
        .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::warn!(provider = provider.id, error = %error, "chat upstream request failed");
            return Err(CompletionError::Unavailable);
        }
        Err(_) => return Err(CompletionError::TimedOut),
    };
    if !upstream.status().is_success() {
        let status = upstream.status();
        let detail = limited_error_body(upstream).await;
        tracing::warn!(provider = provider.id, %status, upstream_detail = %detail, "chat upstream rejected request");
        return Err(CompletionError::Upstream(status));
    }
    let is_sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
    if !is_sse {
        return Err(CompletionError::NotStreaming);
    }
    Ok((provider.kind, upstream))
}

/// One model turn, assembled.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Completion {
    pub text: String,
    /// Empty when the model answered rather than asking for a tool.
    pub tool_calls: Vec<ToolCall>,
}

/// A programmatic handle on the gateway.
///
/// The HTTP endpoint streams, which is right for a person watching text appear
/// and wrong for an agent loop that has to see a whole turn before deciding
/// what to run. This assembles the stream and returns the finished turn.
pub struct Gateway {
    state: AppState,
    /// Where a native coder CLI runs. Per user when the deployment is per user,
    /// so a coder cannot be the way around the workspace boundary the tools keep.
    coder_root: PathBuf,
}

impl Gateway {
    pub fn new(config: Config) -> Result<Self, BuildError> {
        let coder_root = config.coder_workspace.clone();
        Ok(Self {
            state: gateway_state(config)?,
            coder_root,
        })
    }

    pub fn config(&self) -> &Config {
        &self.state.config
    }

    /// Whether this request would be served by a hosted API or by a native CLI.
    ///
    /// The harness asks before it decides whether to offer tools: a CLI coder
    /// is already an agent, and handing it a tool registry it cannot use would
    /// be rejected here and would be the wrong shape anyway.
    pub fn execution_for(&self, request: &ChatRequest) -> Execution {
        let route = self.state.config.route_for(request);
        match self.state.config.native_route(&route) {
            Some(_) => Execution::Native,
            None => Execution::Hosted,
        }
    }

    /// Run one turn to completion, returning the text and any tool calls.
    pub async fn complete(&self, request: ChatRequest) -> Result<Completion, CompletionError> {
        validate_chat(&request).map_err(CompletionError::InvalidRequest)?;
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
            return Err(CompletionError::ConflictingRoute);
        }

        let route = self.state.config.route_for(&request);
        let native = self.state.config.native_route(&route);
        if request.tools.as_ref().is_some_and(|t| !t.is_empty()) && native.is_some() {
            return Err(CompletionError::ToolsOnCoderRoute);
        }

        if let Some(route) = native {
            let permit = self
                .state
                .coder_slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| CompletionError::CoderBusy)?;
            let prompt = coder_prompt(&request.messages, self.state.config.coder_allow_writes);
            let result = coders::execute(
                route,
                &prompt,
                &self.coder_root,
                self.state.config.coder_allow_writes,
                self.state.config.coder_timeout,
            )
            .await;
            drop(permit);
            let execution = match result {
                Ok(execution) => execution,
                Err(coders::CoderError::Timeout { .. }) => {
                    return Err(CompletionError::CoderTimedOut);
                }
                Err(_) => return Err(CompletionError::CoderFailed),
            };
            return Ok(Completion {
                text: execution.output,
                tool_calls: Vec::new(),
            });
        }

        let (kind, upstream) = open_upstream(&self.state, &request).await?;
        let source = upstream.bytes_stream().eventsource();
        pin_mut!(source);

        let mut text = String::new();
        // Arguments arrive in pieces keyed by index, so calls are assembled
        // rather than read: id and name land once, the JSON accumulates.
        let mut calls: std::collections::BTreeMap<u64, (String, String, String)> =
            std::collections::BTreeMap::new();

        let mut saw_done = false;
        while let Some(event) = source.next().await {
            let event = event.map_err(|_| CompletionError::StreamInterrupted)?;
            let Some(data) = translate_event(kind, &event.event, &event.data) else {
                continue;
            };
            if data == "[DONE]" {
                saw_done = true;
                break;
            }
            let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if let Some(part) = chunk
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
            {
                text.push_str(part);
            }
            for call in chunk
                .pointer("/choices/0/delta/tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let entry = calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.0 = id.to_string();
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    entry.1 = name.to_string();
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.2.push_str(args);
                }
            }
        }

        if !saw_done {
            return Err(CompletionError::StreamInterrupted);
        }

        let tool_calls = calls
            .into_values()
            .filter(|(_, name, _)| !name.is_empty())
            .map(|(id, name, arguments)| {
                // Caught here rather than on the next turn: this transcript is
                // handed straight back to the provider by an agent loop, and a
                // name that cannot round-trip would fail validation there with
                // no indication that upstream was what produced it.
                if !valid_provider_id(&name) {
                    return Err(CompletionError::InvalidToolName(display_identifier(&name)));
                }
                let arguments = serde_json::from_str(&arguments)
                    .map_err(|_| CompletionError::InvalidToolArguments(name.clone()))?;
                Ok(ToolCall {
                    id,
                    name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>, CompletionError>>()?;
        Ok(Completion { text, tool_calls })
    }
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
                // Without this the stream carries no usage at all, and a token
                // budget over it would be a number nobody was ever charged.
                "stream_options": {"include_usage": true},
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
/// What a provider says a run cost, read off its own stream.
///
/// Protocols disagree about more than field names. OpenAI sends one usage object
/// at the end covering the whole call. Anthropic sends input tokens when the
/// message opens and output tokens as it closes, so they add up. Gemini repeats
/// a running total on every chunk, so they must not.
///
/// Nothing is estimated. A provider that reports no usage charges nothing,
/// because a made-up number in a budget is worse than an absent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    /// Add this to what has been counted so far.
    Delta(u64),
    /// A running total that replaces what has been counted so far.
    Total(u64),
}

fn usage_from_event(kind: ProviderKind, data: &str) -> Option<Usage> {
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return None;
    }
    let payload: Value = serde_json::from_str(data).ok()?;
    match kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => payload
            .pointer("/usage/total_tokens")
            .and_then(Value::as_u64)
            .map(Usage::Total),
        ProviderKind::Anthropic => {
            let input = payload
                .pointer("/message/usage/input_tokens")
                .and_then(Value::as_u64);
            let output = payload
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64);
            match (input, output) {
                (None, None) => None,
                (input, output) => Some(Usage::Delta(input.unwrap_or(0) + output.unwrap_or(0))),
            }
        }
        ProviderKind::Gemini => payload
            .pointer("/usageMetadata/totalTokenCount")
            .and_then(Value::as_u64)
            .map(Usage::Total),
    }
}

/// Fold one reported usage into a running count.
fn apply_usage(counted: u64, usage: Usage) -> u64 {
    match usage {
        Usage::Delta(value) => counted.saturating_add(value),
        Usage::Total(value) => value.max(counted),
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
        let has_tool_calls = message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty());
        if message.content.trim().is_empty() && !(message.role == Role::Assistant && has_tool_calls)
        {
            return Err("message content must not be empty");
        }
        if message.role == Role::Tool && message.tool_call_id.as_deref().is_none_or(str::is_empty) {
            return Err("tool messages must identify their tool call");
        }
        if message.role != Role::Tool && message.tool_call_id.is_some() {
            return Err("only tool messages may identify a tool call");
        }
        if message.role != Role::Assistant && message.tool_calls.is_some() {
            return Err("only assistant messages may contain tool calls");
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                if call.id.is_empty() || !valid_provider_id(&call.name) {
                    return Err("tool call is invalid");
                }
            }
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
            agent_max_concurrency: 2,
            workspace_mode: WorkspaceMode::Shared,
            quotas: Limits::default(),
            trusted_proxy: false,
            harness: HarnessSettings::default(),
            store: StoreSettings::default(),
            identity: Identity::single_user(),
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
    fn defaults_to_the_first_installed_coder_when_no_hosted_provider_exists() {
        let coders = vec![coders::CoderRoute {
            kind: coders::CoderKind::Claude,
            provider: "claude".into(),
            provider_name: "Claude".into(),
            model: "default".into(),
            model_name: "Default".into(),
            executable: PathBuf::from("/usr/bin/claude"),
        }];

        assert_eq!(default_coder_for_backends(&[], &coders), "claude");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn programmatic_gateway_executes_a_cli_coder_route() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().unwrap();
        let executable = workspace.path().join("hermes");
        std::fs::write(&executable, "#!/bin/sh\nprintf native-answer\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let gateway = Gateway::new(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            providers: Vec::new(),
            coders: vec![coders::CoderRoute {
                kind: coders::CoderKind::Hermes,
                provider: "auto".into(),
                provider_name: "Auto".into(),
                model: "default".into(),
                model_name: "Default".into(),
                executable,
            }],
            default_coder: "hermes".into(),
            default_provider: "auto".into(),
            coder_workspace: workspace.path().to_owned(),
            coder_timeout: Duration::from_secs(5),
            coder_allow_writes: true,
            coder_max_concurrency: 1,
            agent_max_concurrency: 1,
            workspace_mode: WorkspaceMode::Shared,
            quotas: Limits::default(),
            trusted_proxy: false,
            harness: HarnessSettings::default(),
            store: StoreSettings::default(),
            identity: Identity::single_user(),
        })
        .unwrap();

        let completion = gateway
            .complete(ChatRequest {
                ai_coder: Some("hermes".into()),
                provider: Some("auto".into()),
                model: Some("default".into()),
                temperature: None,
                max_tokens: None,
                tools: None,
                messages: vec![ChatMessage {
                    role: Role::User,
                    content: "hello".into(),
                    tool_call_id: None,
                    tool_calls: None,
                }],
            })
            .await
            .unwrap();

        assert_eq!(completion.text, "native-answer");
        assert!(completion.tool_calls.is_empty());
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

    #[tokio::test]
    async fn gateway_assembles_a_streamed_tool_call() {
        use axum::body::Body;
        use axum::http::Response as HttpResponse;

        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"search_feed\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"IT\\\"}\"}}]}}]}\n\n",
                    "data: [DONE]\n\n"
                );
                HttpResponse::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(body))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut config = Config {
            bind: "127.0.0.1:41788".parse().unwrap(),
            providers: vec![provider("openai", ProviderKind::Openai)],
            coders: Vec::new(),
            default_coder: "hermes".into(),
            default_provider: "openai".into(),
            coder_workspace: PathBuf::from("."),
            coder_timeout: Duration::from_secs(30),
            coder_allow_writes: false,
            coder_max_concurrency: 2,
            agent_max_concurrency: 2,
            workspace_mode: WorkspaceMode::Shared,
            quotas: Limits::default(),
            trusted_proxy: false,
            harness: HarnessSettings::default(),
            store: StoreSettings::default(),
            identity: Identity::single_user(),
        };
        config.providers[0].base_url = format!("http://{address}/v1");
        let gateway = Gateway::new(config).unwrap();
        let mut request = request();
        request.tools = Some(vec![tool()]);

        let completion = gateway.complete(request).await.unwrap();
        assert_eq!(completion.text, "");
        assert_eq!(
            completion.tool_calls,
            vec![ToolCall {
                id: "call_1".into(),
                name: "search_feed".into(),
                arguments: json!({"q": "IT"}),
            }]
        );
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

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn openai_reports_one_total_for_the_whole_call() {
        let usage = usage_from_event(
            ProviderKind::Openai,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":30,"total_tokens":42}}"#,
        );
        assert_eq!(usage, Some(Usage::Total(42)));
    }

    #[test]
    fn anthropic_reports_input_and_output_separately_and_they_add_up() {
        let opened = usage_from_event(
            ProviderKind::Anthropic,
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12}}}"#,
        );
        let closed = usage_from_event(
            ProviderKind::Anthropic,
            r#"{"type":"message_delta","usage":{"output_tokens":30}}"#,
        );
        assert_eq!(opened, Some(Usage::Delta(12)));
        assert_eq!(closed, Some(Usage::Delta(30)));
        assert_eq!(
            apply_usage(apply_usage(0, opened.unwrap()), closed.unwrap()),
            42
        );
    }

    /// Gemini repeats a running total on every chunk, so adding them up would
    /// charge several times over for the same tokens.
    #[test]
    fn a_repeated_running_total_is_not_counted_twice() {
        let first = usage_from_event(
            ProviderKind::Gemini,
            r#"{"candidates":[],"usageMetadata":{"totalTokenCount":20}}"#,
        );
        let second = usage_from_event(
            ProviderKind::Gemini,
            r#"{"candidates":[],"usageMetadata":{"totalTokenCount":42}}"#,
        );
        assert_eq!(first, Some(Usage::Total(20)));
        assert_eq!(
            apply_usage(apply_usage(0, first.unwrap()), second.unwrap()),
            42
        );
    }

    /// A late chunk that reports less than has already been counted must not
    /// refund tokens that were really spent.
    #[test]
    fn a_total_never_walks_backwards() {
        assert_eq!(apply_usage(42, Usage::Total(10)), 42);
    }

    #[test]
    fn a_stream_that_reports_nothing_charges_nothing() {
        for kind in [
            ProviderKind::Openai,
            ProviderKind::OpenaiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Gemini,
        ] {
            assert_eq!(
                usage_from_event(kind, r#"{"choices":[{"delta":{"content":"hi"}}]}"#),
                None,
                "{kind:?} invented usage from a plain content chunk"
            );
            assert_eq!(usage_from_event(kind, "[DONE]"), None);
            assert_eq!(usage_from_event(kind, "   "), None);
            assert_eq!(usage_from_event(kind, "not json"), None);
        }
    }

    #[test]
    fn openai_compatible_providers_are_asked_to_report_usage() {
        let payload = provider_payload(
            ProviderKind::Openai,
            "test-model",
            &[ChatMessage {
                role: Role::User,
                content: "hello".into(),
                tool_calls: None,
                tool_call_id: None,
            }],
            None,
            None,
            None,
        );
        assert_eq!(payload["stream_options"]["include_usage"], true);
    }
}

#[cfg(test)]
mod bind_tests {
    use super::*;

    fn config(bind: &str) -> Config {
        Config {
            bind: bind.parse().unwrap(),
            providers: vec![ProviderConfig {
                id: "openai".into(),
                name: "OpenAI".into(),
                ai_coder: "api".into(),
                ai_coder_name: "API".into(),
                provider_id: "openai".into(),
                kind: ProviderKind::Openai,
                base_url: "https://example.test/v1".into(),
                default_model: "test-model".into(),
                api_key: Some("test-key".into()),
            }],
            coders: Vec::new(),
            default_coder: "api".into(),
            default_provider: "openai".into(),
            coder_workspace: PathBuf::from("."),
            coder_timeout: Duration::from_secs(30),
            coder_allow_writes: false,
            coder_max_concurrency: 2,
            agent_max_concurrency: 2,
            workspace_mode: WorkspaceMode::Shared,
            quotas: Limits::default(),
            trusted_proxy: false,
            harness: HarnessSettings::default(),
            store: StoreSettings::default(),
            identity: Identity::single_user(),
        }
    }

    #[test]
    fn loopback_needs_no_permission() {
        assert!(config("127.0.0.1:41788").validate().is_ok());
        assert!(config("[::1]:41788").validate().is_ok());
    }

    #[test]
    fn leaving_loopback_is_refused_by_default() {
        assert!(matches!(
            config("0.0.0.0:41788").validate(),
            Err(ConfigError::RemoteBind)
        ));
    }

    /// Saying "there is a proxy" is not enough on its own: without proxy
    /// identity, every request would arrive as the same local user, and the port
    /// would be an unauthenticated way in for anyone who can reach it.
    #[test]
    fn acknowledging_a_proxy_is_not_enough_without_identity_from_it() {
        let mut settings = config("0.0.0.0:41788");
        settings.trusted_proxy = true;
        assert!(matches!(settings.validate(), Err(ConfigError::RemoteBind)));
    }

    /// And identity alone is not enough either: a header is only worth something
    /// when the operator has said nothing but the proxy can set it.
    #[test]
    fn identity_from_a_proxy_is_not_enough_without_acknowledging_one() {
        let mut settings = config("0.0.0.0:41788");
        settings.identity = Identity::proxy(identity::ProxyHeaders::default());
        assert!(matches!(settings.validate(), Err(ConfigError::RemoteBind)));
    }

    /// Acknowledging a proxy and taking identity from it are still not enough
    /// off loopback: without something only the proxy knows, every host that can
    /// reach the port can claim to be anyone.
    #[test]
    fn a_remote_bind_needs_the_proxy_to_prove_itself() {
        let mut settings = config("0.0.0.0:41788");
        settings.trusted_proxy = true;
        settings.identity = Identity::proxy(identity::ProxyHeaders::default());
        assert!(matches!(
            settings.validate(),
            Err(ConfigError::UnprovenProxy)
        ));
    }

    #[test]
    fn all_three_together_are_what_it_takes() {
        let mut settings = config("0.0.0.0:41788");
        settings.trusted_proxy = true;
        settings.identity = Identity::proxy(identity::ProxyHeaders {
            secret: Some(identity::Secret::new(
                "x-lumen-proxy-secret",
                "correct-horse",
            )),
            ..identity::ProxyHeaders::default()
        });
        assert!(settings.validate().is_ok());
    }

    /// A secret buys nothing on loopback, where the machine is the boundary, so
    /// it is not demanded there.
    #[test]
    fn loopback_still_needs_no_secret() {
        let mut settings = config("127.0.0.1:41788");
        settings.identity = Identity::proxy(identity::ProxyHeaders::default());
        assert!(settings.validate().is_ok());
    }

    /// The refusal has to be actionable: an operator reading it should know both
    /// switches to reach for.
    #[test]
    fn the_refusal_says_what_to_do_about_it() {
        let message = ConfigError::RemoteBind.to_string();
        assert!(message.contains("AI_IDENTITY_MODE=proxy"), "{message}");
        assert!(message.contains("AI_TRUSTED_PROXY=true"), "{message}");
    }

    /// Relaxing the bind must not relax anything else: a remote deployment with
    /// several users still cannot share one writable workspace.
    #[test]
    fn a_remote_deployment_is_still_held_to_the_tenancy_rules() {
        let mut settings = config("0.0.0.0:41788");
        settings.trusted_proxy = true;
        settings.identity = Identity::proxy(identity::ProxyHeaders {
            secret: Some(identity::Secret::new(
                "x-lumen-proxy-secret",
                "correct-horse",
            )),
            ..identity::ProxyHeaders::default()
        });
        settings.coder_allow_writes = true;
        settings.workspace_mode = WorkspaceMode::Shared;

        assert!(matches!(
            settings.validate(),
            Err(ConfigError::UnsafeTenancy(_))
        ));

        settings.workspace_mode = WorkspaceMode::PerUser;
        assert!(settings.validate().is_ok());
    }
}
