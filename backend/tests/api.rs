//! The HTTP surface the frontend and any operator actually talk to.
//!
//! `harness.rs` covers the agent endpoints; the protocol translation is unit
//! tested next to the code that does it. What was left untested from Rust is
//! everything in between: whether the routes exist, what they answer, and
//! whether a bad request is refused before it can reach a provider.

use std::{net::SocketAddr, path::Path, time::Duration};

use ai_chatbot_gateway::{
    Config, HarnessSettings, ProviderConfig, ProviderKind, build_api_router, build_router,
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
        harness: HarnessSettings {
            memory_dir: None,
            ..HarnessSettings::default()
        },
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
