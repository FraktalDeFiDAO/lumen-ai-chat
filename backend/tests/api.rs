//! The HTTP surface the frontend and any operator actually talk to.
//!
//! `harness.rs` covers the agent endpoints; the protocol translation is unit
//! tested next to the code that does it. What was left untested from Rust is
//! everything in between: whether the routes exist, what they answer, and
//! whether a bad request is refused before it can reach a provider.

use std::{net::SocketAddr, path::Path, time::Duration};

use ai_chatbot_gateway::{
    Config, HarnessSettings, ProviderConfig, ProviderKind, StoreSettings, build_api_router,
    build_router,
    identity::{Identity, ProxyHeaders, Secret},
    quota::Limits,
    workspaces::WorkspaceMode,
};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// An upstream that answers model discovery and one streamed completion, so the
/// endpoints under test have something real to talk to.
async fn upstream() -> SocketAddr {
    async fn models() -> Response {
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"data": [{"id": "discovered-model", "display_name": "Discovered"}]})
                    .to_string(),
            ))
            .unwrap()
    }
    async fn completions(State(()): State<()>, _body: String) -> Response {
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Upstream said hello\"}}]}\n\ndata: [DONE]\n\n",
            ))
            .unwrap()
    }
    let router = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(completions))
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

fn config(address: SocketAddr, workspace: &Path) -> Config {
    configured(address, workspace, None)
}

fn configured(address: SocketAddr, workspace: &Path, identity: Option<Identity>) -> Config {
    Config {
        bind: "127.0.0.1:41788".parse().unwrap(),
        providers: vec![ProviderConfig {
            id: "openai".into(),
            name: "OpenAI".into(),
            ai_coder: "hermes".into(),
            ai_coder_name: "Hermes".into(),
            provider_id: "openai".into(),
            kind: ProviderKind::Openai,
            base_url: format!("http://{address}/v1"),
            default_model: "test-model".into(),
            api_key: Some("test-key".into()),
        }],
        coders: Vec::new(),
        default_coder: "hermes".into(),
        default_provider: "openai".into(),
        coder_workspace: workspace.to_path_buf(),
        coder_timeout: Duration::from_secs(30),
        coder_allow_writes: false,
        coder_max_concurrency: 2,
        agent_max_concurrency: 2,
        workspace_mode: WorkspaceMode::Shared,
        quotas: Limits::default(),
        trusted_proxy: false,
        harness: HarnessSettings {
            memory_dir: None,
            ..HarnessSettings::default()
        },
        store: StoreSettings {
            enabled: false,
            ..StoreSettings::default()
        },
        identity: identity.unwrap_or_else(Identity::single_user),
    }
}

async fn call(router: Router, request: Request<Body>) -> (StatusCode, String) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn get_json(path: &str) -> (StatusCode, Value) {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = build_api_router(config(address, workspace.path())).unwrap();
    let (status, body) = call(
        router,
        Request::builder().uri(path).body(Body::empty()).unwrap(),
    )
    .await;
    (
        status,
        serde_json::from_str(&body)
            .unwrap_or_else(|_| panic!("{path} did not answer JSON: {body}")),
    )
}

fn post_json(path: &str, payload: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

async fn post_chat(payload: Value) -> (StatusCode, String) {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = build_api_router(config(address, workspace.path())).unwrap();
    call(router, post_json("/chat", payload)).await
}

#[tokio::test]
async fn health_and_readiness_describe_the_configuration_they_are_serving() {
    let (status, health) = get_json("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["status"], "ok");
    assert_eq!(health["default_ai_coder"], "hermes");
    assert_eq!(health["default_provider"], "openai");
    assert_eq!(health["provider_count"], 1);
    assert_eq!(health["coder_count"], 0);

    let (status, ready) = get_json("/ready").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["default_ai_coder"], "hermes");
}

#[tokio::test]
async fn metrics_are_prometheus_text_covering_every_counter_the_service_keeps() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = build_api_router(config(address, workspace.path())).unwrap();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let body = String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
        .into_owned();

    for metric in [
        "lumen_chat_requests_total",
        "lumen_agent_requests_total",
        "lumen_agent_tool_calls_total",
        "lumen_memory_records",
        "lumen_coder_busy_rejections_total",
        "lumen_coder_slots_available",
        "lumen_configured_coders",
        "lumen_configured_providers",
    ] {
        assert!(body.contains(&format!("# TYPE {metric}")), "{body}");
        assert!(body.contains(&format!("\n{metric} ")), "{body}");
    }
    assert!(body.contains("lumen_configured_providers 1"), "{body}");
    assert!(body.contains("lumen_coder_slots_available 2"), "{body}");
}

#[tokio::test]
async fn the_provider_list_names_the_default_route() {
    let (status, body) = get_json("/providers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_ai_coder"], "hermes");
    assert_eq!(body["default_provider"], "openai");

    let providers = body["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["id"], "openai");
    assert_eq!(providers[0]["ai_coder"], "hermes");
    assert_eq!(providers[0]["kind"], "openai");
    assert_eq!(providers[0]["default_model"], "test-model");
    assert_eq!(providers[0]["is_default"], true);
}

#[tokio::test]
async fn the_coder_list_covers_every_coder_and_reports_the_workspace_mode() {
    let (status, body) = get_json("/coders").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["allow_writes"], false);

    let coders = body["coders"].as_array().unwrap();
    assert_eq!(coders.len(), 5);
    let ids: Vec<&str> = coders
        .iter()
        .map(|coder| coder["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["hermes", "opencode", "kilocode", "claude", "codex"]);
    for coder in coders {
        assert_eq!(coder["route_count"], 0);
        assert!(coder["name"].is_string());
    }
}

#[tokio::test]
async fn the_model_catalog_carries_the_route_key_the_client_sends_back() {
    let (status, body) = get_json("/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_ai_coder"], "hermes");

    let models = body["models"].as_array().unwrap();
    let discovered = models
        .iter()
        .find(|model| model["id"] == "discovered-model")
        .unwrap_or_else(|| panic!("the upstream catalog was not surfaced: {body}"));
    assert_eq!(discovered["key"], "hermes/openai/discovered-model");
    assert_eq!(discovered["ai_coder"], "hermes");
    assert_eq!(discovered["ai_coder_name"], "Hermes");
    assert_eq!(discovered["provider"], "openai");
    assert_eq!(discovered["provider_name"], "OpenAI");
}

#[tokio::test]
async fn an_unreachable_provider_falls_back_to_its_configured_model_and_says_so() {
    let workspace = tempfile::tempdir().unwrap();
    // Port 1 on loopback refuses instantly, which is what an unavailable
    // provider looks like from here.
    let router =
        build_api_router(config("127.0.0.1:1".parse().unwrap(), workspace.path())).unwrap();
    let (status, body) = call(
        router,
        Request::builder()
            .uri("/models")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    let errors = value["provider_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{body}");
    assert_eq!(errors[0]["provider"], "openai");
    assert!(
        value["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "test-model"),
        "the configured default should still be offered: {body}"
    );
}

#[tokio::test]
async fn a_chat_request_is_streamed_back_as_server_sent_events() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = build_api_router(config(address, workspace.path())).unwrap();
    let response = router
        .oneshot(post_json(
            "/chat",
            json!({"messages": [{"role": "user", "content": "hello"}]}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream; charset=utf-8"
    );
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "no-cache, no-transform"
    );
    assert_eq!(headers.get("x-accel-buffering").unwrap(), "no");

    let body = String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
        .into_owned();
    assert!(body.contains("Upstream said hello"), "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
}

#[tokio::test]
async fn a_request_that_could_only_fail_is_refused_before_any_provider_is_called() {
    let cases = [
        (json!({"messages": []}), "at least one message"),
        (
            json!({"messages": [{"role": "user", "content": "hi"}], "temperature": 3.5}),
            "temperature must be between 0 and 2",
        ),
        (
            json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 0}),
            "max_tokens must be between 1 and 32768",
        ),
        (
            json!({"messages": [{"role": "tool", "content": "result"}]}),
            "tool messages must identify their tool call",
        ),
    ];

    for (payload, expected) in cases {
        let (status, body) = post_chat(payload.clone()).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{payload} was not refused: {body}"
        );
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["error"]["type"], "invalid_request", "{body}");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{payload} produced the wrong reason: {body}"
        );
    }
}

/// A request that got as far as the stream reports its failure inside the
/// stream: the client is already reading events, and a status code it will
/// never look at would leave the chat silent.
#[tokio::test]
async fn a_route_that_is_not_configured_is_named_in_an_error_frame() {
    let (status, body) = post_chat(json!({
        "messages": [{"role": "user", "content": "hi"}],
        "ai_coder": "hermes",
        "provider": "does-not-exist",
    }))
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let frames: Vec<&str> = body
        .split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .collect();
    let error: Value = serde_json::from_str(frames[0]).unwrap();
    assert_eq!(error["error"]["type"], "upstream_error", "{body}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("hermes/does-not-exist"),
        "the refusal should name the route that was asked for: {body}"
    );
    assert_eq!(*frames.last().unwrap(), "[DONE]", "{body}");
}

#[tokio::test]
async fn malformed_json_is_answered_as_json() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = build_api_router(config(address, workspace.path())).unwrap();
    let (status, body) = call(
        router,
        Request::builder()
            .method("POST")
            .uri("/chat")
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .unwrap(),
    )
    .await;

    assert!(status.is_client_error(), "{status} {body}");
}

#[tokio::test]
async fn an_unknown_route_is_a_json_404_rather_than_an_empty_body() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let (status, body) = call(
        build_router(config(address, workspace.path())).unwrap(),
        Request::builder()
            .uri("/api/nothing-here")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["error"]["type"], "not_found");
    assert_eq!(value["error"]["message"], "route not found");
}

#[tokio::test]
async fn the_standalone_router_serves_the_same_routes_under_api() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;

    let (mounted, _) = call(
        build_router(config(address, workspace.path())).unwrap(),
        Request::builder()
            .uri("/api/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(mounted, StatusCode::OK);

    // The bare router is the one an embedding host nests, so it must not carry
    // the prefix itself.
    let (bare, _) = call(
        build_api_router(config(address, workspace.path())).unwrap(),
        Request::builder()
            .uri("/api/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(bare, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_body_larger_than_the_limit_is_rejected() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = build_api_router(config(address, workspace.path())).unwrap();
    let huge = "x".repeat(4 * 1024 * 1024);
    let (status, _) = call(
        router,
        post_json(
            "/chat",
            json!({"messages": [{"role": "user", "content": huge}]}),
        ),
    )
    .await;

    assert!(
        status == StatusCode::PAYLOAD_TOO_LARGE || status == StatusCode::BAD_REQUEST,
        "an oversized body should be refused, got {status}"
    );
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

async fn me(identity: Option<Identity>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = build_api_router(configured(address, workspace.path(), identity)).unwrap();

    let mut request = Request::builder().uri("/me");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let (status, body) = call(router, request.body(Body::empty()).unwrap()).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("/me did not answer JSON: {body}")),
    )
}

/// The deployment Lumen has always been: one person, no header to forge.
#[tokio::test]
async fn a_single_user_deployment_names_the_local_user_and_says_it_is_not_shared() {
    let (status, body) = me(None, &[]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["id"], "local");
    assert_eq!(body["multi_user"], false);
}

/// Identity must come from configuration, never from the request. A header that
/// nothing was told to read cannot be allowed to decide who someone is.
#[tokio::test]
async fn a_single_user_deployment_ignores_an_identity_header_entirely() {
    let (status, body) = me(None, &[("x-forwarded-user", "someone-else")]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["id"], "local");
}

#[tokio::test]
async fn a_proxy_deployment_reports_the_user_the_proxy_asserted() {
    let (status, body) = me(
        Some(Identity::proxy(ProxyHeaders::default())),
        &[
            ("x-forwarded-user", "riley"),
            ("x-forwarded-email", "riley@example.com"),
            ("x-forwarded-preferred-username", "Riley Chen"),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["id"], "riley");
    assert_eq!(body["user"]["email"], "riley@example.com");
    assert_eq!(body["user"]["display_name"], "Riley Chen");
    assert_eq!(body["multi_user"], true);
}

/// A request that reaches a proxy deployment without an identity did not come
/// through the proxy, which is the one thing that must never be served.
#[tokio::test]
async fn a_proxy_deployment_refuses_a_request_that_carries_no_identity() {
    let (status, body) = me(Some(Identity::proxy(ProxyHeaders::default())), &[]).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "unauthenticated");
}

#[tokio::test]
async fn a_proxy_deployment_refuses_an_identity_it_cannot_use_as_a_key() {
    for value in ["", "  ", "with space", "a/b"] {
        let (status, _) = me(
            Some(Identity::proxy(ProxyHeaders::default())),
            &[("x-forwarded-user", value)],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{value:?} was accepted as an identity"
        );
    }
}

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------

/// A router with a real store on disk and proxy identity, so the isolation
/// assertions below are made through HTTP rather than against the store directly.
fn shared_router(store: &Path) -> Router {
    let workspace = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let mut settings = configured(
        "127.0.0.1:1".parse().unwrap(),
        workspace.path(),
        Some(Identity::proxy(ProxyHeaders::default())),
    );
    settings.store = StoreSettings {
        path: Some(store.to_path_buf()),
        required: true,
        enabled: true,
    };
    build_api_router(settings).unwrap()
}

fn conversation(id: &str, title: &str, updated_at: i64) -> Value {
    json!({
        "id": id,
        "title": title,
        "created_at": updated_at,
        "updated_at": updated_at,
        "messages": [
            {"id": "m1", "role": "user", "content": "a question", "created_at": updated_at},
            {"id": "m2", "role": "assistant", "content": "an answer", "created_at": updated_at},
        ],
    })
}

async fn as_user(router: Router, user: &str, request: Request<Body>) -> (StatusCode, String) {
    let (parts, body) = request.into_parts();
    let mut rebuilt = Request::builder().method(parts.method).uri(parts.uri);
    for (name, value) in parts.headers.iter() {
        rebuilt = rebuilt.header(name, value);
    }
    call(
        router,
        rebuilt.header("x-forwarded-user", user).body(body).unwrap(),
    )
    .await
}

fn put_request(path: &str, payload: Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

fn get_request(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

fn delete_request(path: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn a_conversation_saved_over_http_can_be_read_back() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    let (status, body) = as_user(
        shared_router(&database),
        "riley",
        put_request("/conversations/c1", conversation("c1", "First chat", 10)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = as_user(
        shared_router(&database),
        "riley",
        get_request("/conversations/c1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["title"], "First chat");
    assert_eq!(value["messages"].as_array().unwrap().len(), 2);
    assert_eq!(value["messages"][1]["content"], "an answer");
}

#[tokio::test]
async fn the_history_lists_only_summaries_newest_first() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    for (id, title, updated) in [("older", "Older", 10), ("newer", "Newer", 20)] {
        as_user(
            shared_router(&database),
            "riley",
            put_request(
                &format!("/conversations/{id}"),
                conversation(id, title, updated),
            ),
        )
        .await;
    }

    let (status, body) = as_user(
        shared_router(&database),
        "riley",
        get_request("/conversations"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = serde_json::from_str::<Value>(&body).unwrap();
    let items = listed["conversations"].as_array().unwrap().clone();

    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "newer");
    assert_eq!(items[0]["message_count"], 2);
    assert!(
        items[0].get("messages").is_none(),
        "the list must not carry transcripts"
    );
}

/// The property the whole hosted design rests on, asserted at the edge.
#[tokio::test]
async fn one_user_cannot_see_read_or_delete_anothers_conversation() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    as_user(
        shared_router(&database),
        "riley",
        put_request(
            "/conversations/private",
            conversation("private", "Riley's", 10),
        ),
    )
    .await;

    let (status, body) = as_user(
        shared_router(&database),
        "sam",
        get_request("/conversations"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        serde_json::from_str::<Value>(&body).unwrap()["conversations"]
            .as_array()
            .unwrap()
            .is_empty(),
        "another user's history leaked into the list: {body}"
    );

    let (status, _) = as_user(
        shared_router(&database),
        "sam",
        get_request("/conversations/private"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a guessed id was readable");

    let (status, _) = as_user(
        shared_router(&database),
        "sam",
        delete_request("/conversations/private"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a guessed id was deletable");

    let (status, body) = as_user(
        shared_router(&database),
        "riley",
        get_request("/conversations/private"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner lost their conversation: {body}"
    );
}

#[tokio::test]
async fn saving_over_someone_elses_id_is_refused_rather_than_silently_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    as_user(
        shared_router(&database),
        "riley",
        put_request(
            "/conversations/shared",
            conversation("shared", "Riley's", 10),
        ),
    )
    .await;

    let (status, _) = as_user(
        shared_router(&database),
        "sam",
        put_request("/conversations/shared", conversation("shared", "Sam's", 20)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, body) = as_user(
        shared_router(&database),
        "riley",
        get_request("/conversations/shared"),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["title"],
        "Riley's",
        "the original was overwritten"
    );
}

#[tokio::test]
async fn clearing_history_clears_only_the_callers_own() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    as_user(
        shared_router(&database),
        "riley",
        put_request("/conversations/r1", conversation("r1", "One", 10)),
    )
    .await;
    as_user(
        shared_router(&database),
        "riley",
        put_request("/conversations/r2", conversation("r2", "Two", 20)),
    )
    .await;
    as_user(
        shared_router(&database),
        "sam",
        put_request("/conversations/s1", conversation("s1", "Sam's", 30)),
    )
    .await;

    let (status, body) = as_user(
        shared_router(&database),
        "riley",
        delete_request("/conversations"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["deleted"], 2);

    let (_, body) = as_user(
        shared_router(&database),
        "sam",
        get_request("/conversations"),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["conversations"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "another user's history was cleared too"
    );
}

#[tokio::test]
async fn a_body_that_disagrees_with_the_path_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    let (status, body) = as_user(
        shared_router(&database),
        "riley",
        put_request(
            "/conversations/c1",
            conversation("something-else", "Confused", 10),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("does not match"), "{body}");
}

#[tokio::test]
async fn an_unknown_conversation_is_a_json_404() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    let (status, body) = as_user(
        shared_router(&database),
        "riley",
        get_request("/conversations/nope"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["type"],
        "not_found"
    );
}

#[tokio::test]
async fn conversations_require_an_identity_when_one_is_configured() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    let (status, _) = call(shared_router(&database), get_request("/conversations")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A deployment that keeps conversations in the browser says so, rather than
/// answering as though it saved something.
#[tokio::test]
async fn a_deployment_without_a_store_says_the_endpoint_is_unavailable() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = build_api_router(config(address, workspace.path())).unwrap();

    let (status, body) = call(router, get_request("/conversations")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["type"],
        "store_unavailable"
    );
}

#[tokio::test]
async fn a_conversation_keeps_the_tool_activity_the_client_put_on_it() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");

    let mut payload = conversation("c1", "With tools", 10);
    payload["messages"][1]["feedback"] = json!("up");
    payload["messages"][1]["extra"] = json!({
        "tools": [{"id": "call-1", "name": "read_file", "output": "ok", "failed": false}],
        "contextRecords": 4,
        "incomplete": true,
    });

    as_user(
        shared_router(&database),
        "riley",
        put_request("/conversations/c1", payload.clone()),
    )
    .await;
    let (_, body) = as_user(
        shared_router(&database),
        "riley",
        get_request("/conversations/c1"),
    )
    .await;

    let read: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(read["messages"][1]["feedback"], "up");
    assert_eq!(
        read["messages"][1]["extra"],
        payload["messages"][1]["extra"]
    );
}

// ---------------------------------------------------------------------------
// Per-user workspaces
// ---------------------------------------------------------------------------

fn per_user_router(base: &Path, memory: Option<&Path>) -> Router {
    let address = "127.0.0.1:1".parse().unwrap();
    let mut settings = configured(
        address,
        base,
        Some(Identity::proxy(ProxyHeaders::default())),
    );
    settings.workspace_mode = WorkspaceMode::PerUser;
    settings.coder_allow_writes = true;
    settings.harness.memory_dir = memory.map(|path| path.to_path_buf());
    build_api_router(settings).unwrap()
}

async fn harness_for(router: Router, user: &str) -> Value {
    let (status, body) = as_user(router, user, get_request("/harness")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn each_user_is_served_a_workspace_of_their_own() {
    let base = tempfile::tempdir().unwrap();

    let riley = harness_for(per_user_router(base.path(), None), "riley").await;
    let sam = harness_for(per_user_router(base.path(), None), "sam").await;

    let riley_root = riley["workspace"].as_str().unwrap();
    let sam_root = sam["workspace"].as_str().unwrap();
    assert_ne!(riley_root, sam_root);
    assert!(riley_root.ends_with("/riley"), "{riley_root}");
    assert!(sam_root.ends_with("/sam"), "{sam_root}");
    assert_eq!(riley["writes_enabled"], true);
}

/// The point of the whole arrangement: a file one user's agent wrote is not
/// something another user's agent can read.
#[tokio::test]
async fn a_file_written_in_one_workspace_is_unreachable_from_another() {
    let base = tempfile::tempdir().unwrap();
    let router = per_user_router(base.path(), None);

    let riley = harness_for(router, "riley").await;
    let riley_root = Path::new(riley["workspace"].as_str().unwrap());
    std::fs::write(riley_root.join("secret.txt"), "private").unwrap();

    // Sam's workspace resolves paths against Sam's root, so neither a relative
    // climb nor Riley's absolute path reaches it.
    let sam = harness_for(per_user_router(base.path(), None), "sam").await;
    let sam_root = Path::new(sam["workspace"].as_str().unwrap());
    assert!(!sam_root.join("secret.txt").exists());
    assert!(!sam_root.starts_with(riley_root));
}

#[tokio::test]
async fn each_user_indexes_only_their_own_files() {
    let base = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();

    let riley = harness_for(per_user_router(base.path(), Some(indexes.path())), "riley").await;
    let riley_root = Path::new(riley["workspace"].as_str().unwrap());
    std::fs::write(
        riley_root.join("notes.md"),
        "# Riley\nthe passphrase is hunter2\n",
    )
    .unwrap();

    let (status, body) = as_user(
        per_user_router(base.path(), Some(indexes.path())),
        "riley",
        post_json("/harness/index", json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let indexed: Value = serde_json::from_str(&body).unwrap();
    assert!(indexed["records"].as_i64().unwrap() > 0, "{body}");

    // Sam's index is a different index over a different tree, so it has nothing.
    let (status, body) = as_user(
        per_user_router(base.path(), Some(indexes.path())),
        "sam",
        post_json("/harness/index", json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sam_indexed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        sam_indexed["records"], 0,
        "another user's files were indexed: {body}"
    );

    let sam = harness_for(per_user_router(base.path(), Some(indexes.path())), "sam").await;
    assert_eq!(sam["memory"]["records"], 0);
}

#[tokio::test]
async fn a_shared_deployment_still_serves_everyone_the_one_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let mut settings = configured(
        address,
        workspace.path(),
        Some(Identity::proxy(ProxyHeaders::default())),
    );
    settings.workspace_mode = WorkspaceMode::Shared;

    let riley = harness_for(build_api_router(settings.clone()).unwrap(), "riley").await;
    let sam = harness_for(build_api_router(settings).unwrap(), "sam").await;
    assert_eq!(riley["workspace"], sam["workspace"]);
}

/// No runtime check makes this safe, so it is refused before the service is
/// listening rather than served and hoped about.
#[tokio::test]
async fn a_shared_writable_workspace_is_refused_for_a_multi_user_deployment() {
    let workspace = tempfile::tempdir().unwrap();
    let mut settings = configured(
        "127.0.0.1:1".parse().unwrap(),
        workspace.path(),
        Some(Identity::proxy(ProxyHeaders::default())),
    );
    settings.workspace_mode = WorkspaceMode::Shared;
    settings.coder_allow_writes = true;

    let error = build_api_router(settings).unwrap_err().to_string();
    assert!(
        error.contains("shared writable workspace"),
        "the refusal should explain itself: {error}"
    );

    // The same configuration for one person is exactly what it always was.
    let mut single = configured("127.0.0.1:1".parse().unwrap(), workspace.path(), None);
    single.workspace_mode = WorkspaceMode::Shared;
    single.coder_allow_writes = true;
    assert!(build_api_router(single).is_ok());
}

#[tokio::test]
async fn the_harness_endpoints_need_an_identity_when_one_is_configured() {
    let base = tempfile::tempdir().unwrap();

    for request in [
        get_request("/harness"),
        post_json("/harness/index", json!({})),
    ] {
        let (status, _) = call(per_user_router(base.path(), None), request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

// ---------------------------------------------------------------------------
// Quotas
// ---------------------------------------------------------------------------

fn metered_router(address: SocketAddr, workspace: &Path, limits: Limits) -> Router {
    let mut settings = configured(
        address,
        workspace,
        Some(Identity::proxy(ProxyHeaders::default())),
    );
    settings.quotas = limits;
    build_api_router(settings).unwrap()
}

fn chat_request() -> Value {
    json!({"messages": [{"role": "user", "content": "hello"}]})
}

#[tokio::test]
async fn nothing_is_metered_until_a_quota_is_configured() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;

    for _ in 0..5 {
        let (status, body) = as_user(
            metered_router(address, workspace.path(), Limits::default()),
            "riley",
            post_json("/chat", chat_request()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

#[tokio::test]
async fn a_request_budget_is_enforced_and_says_when_to_come_back() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let limits = Limits {
        requests: 2,
        tokens: 0,
        window: Duration::from_secs(3600),
    };
    // One router, so the windows are shared the way they are in a real process.
    let router = metered_router(address, workspace.path(), limits);

    for _ in 0..2 {
        let (status, _) =
            as_user(router.clone(), "riley", post_json("/chat", chat_request())).await;
        assert_eq!(status, StatusCode::OK);
    }

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat")
                .header("content-type", "application/json")
                .header("x-forwarded-user", "riley")
                .body(Body::from(chat_request().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response.headers().contains_key(header::RETRY_AFTER),
        "a refusal a caller can retry should say when"
    );
    let body = String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
        .into_owned();
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["error"]["type"], "quota_exceeded");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("2 of 2 requests"),
        "{body}"
    );
}

/// One person spending their budget must not stop anybody else working.
#[tokio::test]
async fn one_users_budget_does_not_refuse_another_user() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = metered_router(
        address,
        workspace.path(),
        Limits {
            requests: 1,
            tokens: 0,
            window: Duration::from_secs(3600),
        },
    );

    let (first, _) = as_user(router.clone(), "riley", post_json("/chat", chat_request())).await;
    assert_eq!(first, StatusCode::OK);
    let (second, _) = as_user(router.clone(), "riley", post_json("/chat", chat_request())).await;
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);

    let (other, body) = as_user(router, "sam", post_json("/chat", chat_request())).await;
    assert_eq!(
        other,
        StatusCode::OK,
        "sam was charged for riley's spending: {body}"
    );
}

#[tokio::test]
async fn the_agent_endpoint_is_metered_from_the_same_budget() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = metered_router(
        address,
        workspace.path(),
        Limits {
            requests: 1,
            tokens: 0,
            window: Duration::from_secs(3600),
        },
    );

    let (first, _) = as_user(router.clone(), "riley", post_json("/chat", chat_request())).await;
    assert_eq!(first, StatusCode::OK);

    let (second, body) = as_user(router, "riley", post_json("/agent", chat_request())).await;
    assert_eq!(
        second,
        StatusCode::TOO_MANY_REQUESTS,
        "the agent loop spends from the same budget: {body}"
    );
}

#[tokio::test]
async fn the_client_is_told_what_it_has_spent() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let limits = Limits {
        requests: 10,
        tokens: 5_000,
        window: Duration::from_secs(1800),
    };
    let router = metered_router(address, workspace.path(), limits);

    as_user(router.clone(), "riley", post_json("/chat", chat_request())).await;
    let (status, body) = as_user(router, "riley", get_request("/me")).await;

    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["quota"]["requests_used"], 1);
    assert_eq!(value["quota"]["requests_limit"], 10);
    assert_eq!(value["quota"]["tokens_limit"], 5_000);
    assert_eq!(value["quota"]["window_seconds"], 1800);
}

#[tokio::test]
async fn an_unmetered_deployment_reports_no_quota_at_all() {
    let (_, body) = me(None, &[]).await;
    assert!(body["quota"].is_null(), "{body}");
}

#[tokio::test]
async fn refusals_are_counted_for_an_operator_to_see() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let router = metered_router(
        address,
        workspace.path(),
        Limits {
            requests: 1,
            tokens: 0,
            window: Duration::from_secs(3600),
        },
    );

    as_user(router.clone(), "riley", post_json("/chat", chat_request())).await;
    as_user(router.clone(), "riley", post_json("/chat", chat_request())).await;

    let (_, body) = as_user(router, "riley", get_request("/metrics")).await;
    assert!(body.contains("lumen_quota_rejections_total 1"), "{body}");
}

/// A shared secret is what turns "nothing else can reach this port" from an
/// assertion into something that fails closed when it is wrong.
#[tokio::test]
async fn a_forged_identity_is_refused_when_the_proxy_has_a_secret() {
    let workspace = tempfile::tempdir().unwrap();
    let address = upstream().await;
    let identity = Identity::proxy(ProxyHeaders {
        secret: Some(Secret::new("x-lumen-proxy-secret", "correct-horse")),
        ..ProxyHeaders::default()
    });
    let router = || {
        build_api_router(configured(
            address,
            workspace.path(),
            Some(identity.clone()),
        ))
        .unwrap()
    };

    // Whoever holds the secret is the proxy, and what it says goes.
    let (status, body) = call(
        router(),
        Request::builder()
            .uri("/me")
            .header("x-lumen-proxy-secret", "correct-horse")
            .header("x-forwarded-user", "riley")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["user"]["id"],
        "riley"
    );

    // Anyone else claiming to be riley has identified nobody.
    for headers in [
        vec![("x-forwarded-user", "riley")],
        vec![
            ("x-lumen-proxy-secret", "wrong"),
            ("x-forwarded-user", "riley"),
        ],
    ] {
        let mut request = Request::builder().uri("/me");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let (status, _) = call(router(), request.body(Body::empty()).unwrap()).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a forged identity was served"
        );
    }
}

#[tokio::test]
async fn a_secret_guards_the_conversations_a_forged_identity_would_have_reached() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("conversations.db");
    let workspace = tempfile::tempdir().unwrap();
    let mut settings = configured(
        "127.0.0.1:1".parse().unwrap(),
        workspace.path(),
        Some(Identity::proxy(ProxyHeaders {
            secret: Some(Secret::new("x-lumen-proxy-secret", "correct-horse")),
            ..ProxyHeaders::default()
        })),
    );
    settings.store = StoreSettings {
        path: Some(database),
        required: true,
        enabled: true,
    };

    let (status, _) = call(
        build_api_router(settings).unwrap(),
        Request::builder()
            .uri("/conversations")
            .header("x-forwarded-user", "riley")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// An index that has silently stopped matching the tree answers with code that
/// is no longer there. The status endpoint says so rather than leaving a user to
/// wonder why retrieval is wrong.
#[tokio::test]
async fn the_harness_reports_an_index_the_tree_has_moved_past() {
    let workspace = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.md"), "# One\n").unwrap();

    let router = || {
        let mut settings = configured("127.0.0.1:1".parse().unwrap(), workspace.path(), None);
        settings.harness.memory_dir = Some(store.path().to_path_buf());
        build_api_router(settings).unwrap()
    };

    let (status, body) = call(router(), post_json("/harness/index", json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, body) = call(
        router(),
        Request::builder()
            .uri("/harness")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let fresh: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(fresh["memory"]["stale"], false, "{body}");

    // Somebody edits the tree without going through a tool.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(workspace.path().join("notes.md"), "# One, revised\n").unwrap();

    let (_, body) = call(
        router(),
        Request::builder()
            .uri("/harness")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let stale: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(stale["memory"]["stale"], true, "{body}");
}

/// Readiness that only echoes configuration is not readiness. Compose gates the
/// web container on this, so answering "ready" while the store is unopenable
/// puts a broken deployment behind a healthy-looking front door.
#[tokio::test]
async fn readiness_reports_the_store_it_depends_on() {
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let address = upstream().await;

    let mut settings = configured(address, workspace.path(), None);
    settings.store = StoreSettings {
        path: Some(directory.path().join("conversations.db")),
        required: true,
        enabled: true,
    };
    let (status, body) = call(
        build_api_router(settings).unwrap(),
        Request::builder()
            .uri("/ready")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["status"], "ready");
    assert_eq!(value["store"], "ok", "{body}");
}

/// A deployment that keeps conversations in the browser has no store to check,
/// and must not be called unhealthy for it.
#[tokio::test]
async fn readiness_is_content_without_a_store() {
    let (status, body) = get_json("/ready").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["store"], "disabled", "{body}");
}

/// Two requests must not share an id, or a shared log has no thread to pull.
#[test]
fn request_ids_are_unique_within_a_process() {
    use std::collections::HashSet;
    let ids: HashSet<String> = (0..5_000)
        .map(|_| ai_chatbot_gateway::new_request_id())
        .collect();
    assert_eq!(ids.len(), 5_000, "request ids collided");
}

/// Short enough to read in a log line, and free of anything that would need
/// quoting there.
#[test]
fn a_request_id_is_readable() {
    let id = ai_chatbot_gateway::new_request_id();
    assert!(id.len() <= 48, "{id} is too long to read in a log line");
    assert!(
        id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "{id} would need quoting in a log line"
    );
}

// ---------------------------------------------------------------------------
// Agent concurrency
// ---------------------------------------------------------------------------

/// An upstream that holds its answer until told to let go, so a run can be
/// observed while it is still in flight rather than after it has finished.
async fn slow_upstream(hold: std::sync::Arc<tokio::sync::Notify>) -> SocketAddr {
    async fn completions(
        State(hold): State<std::sync::Arc<tokio::sync::Notify>>,
        _body: String,
    ) -> Response {
        hold.notified().await;
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n",
            ))
            .unwrap()
    }
    let router = Router::new()
        .route("/v1/chat/completions", post(completions))
        .with_state(hold);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

/// A quota is a count over an hour. It says nothing about how many runs are in
/// flight at once, and an agent run is a dozen model calls -- so ten users
/// starting one together means ten loops competing for this process with no
/// backpressure and no queue.
#[tokio::test]
async fn a_second_agent_run_is_refused_while_the_first_is_still_going() {
    let workspace = tempfile::tempdir().unwrap();
    let hold = std::sync::Arc::new(tokio::sync::Notify::new());
    let address = slow_upstream(hold.clone()).await;

    let mut settings = config(address, workspace.path());
    settings.agent_max_concurrency = 1;
    let router = build_api_router(settings).unwrap();

    let first = tokio::spawn({
        let router = router.clone();
        async move {
            call(
                router,
                post_json(
                    "/agent",
                    json!({"messages": [{"role": "user", "content": "go"}]}),
                ),
            )
            .await
        }
    });

    // Let the first run reach the upstream and block there.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (status, body) = call(
        router.clone(),
        post_json(
            "/agent",
            json!({"messages": [{"role": "user", "content": "also go"}]}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let refused: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(refused["error"]["type"], "agent_busy");

    hold.notify_waiters();
    let (first_status, _) = first.await.unwrap();
    assert_eq!(
        first_status,
        StatusCode::OK,
        "the first run should have been served"
    );

    // And once it is done, the next one is admitted again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    hold.notify_waiters();
    let (after, _) = call(
        router,
        post_json(
            "/agent",
            json!({"messages": [{"role": "user", "content": "again"}]}),
        ),
    )
    .await;
    assert_eq!(after, StatusCode::OK, "the slot was never given back");
}

#[tokio::test]
async fn refusing_a_busy_agent_says_when_to_come_back_and_is_counted() {
    let workspace = tempfile::tempdir().unwrap();
    let hold = std::sync::Arc::new(tokio::sync::Notify::new());
    let address = slow_upstream(hold.clone()).await;

    let mut settings = config(address, workspace.path());
    settings.agent_max_concurrency = 1;
    let router = build_api_router(settings).unwrap();

    let first = tokio::spawn({
        let router = router.clone();
        async move {
            call(
                router,
                post_json(
                    "/agent",
                    json!({"messages": [{"role": "user", "content": "go"}]}),
                ),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let response = router
        .clone()
        .oneshot(post_json(
            "/agent",
            json!({"messages": [{"role": "user", "content": "also go"}]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        response.headers().contains_key(header::RETRY_AFTER),
        "a refusal a caller can retry should say when"
    );

    let (_, metrics) = call(
        router,
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        metrics.contains("lumen_agent_busy_rejections_total 1"),
        "an operator cannot see the pressure: {metrics}"
    );

    hold.notify_waiters();
    let _ = first.await;
}
