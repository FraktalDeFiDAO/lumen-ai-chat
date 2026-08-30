//! End-to-end tests for the agent harness.
//!
//! The unit tests cover the parts in isolation; these cover the claim that
//! matters, which is that a model's tool call becomes real work and the result
//! of that work comes back to the model in a form it can read. A mock upstream
//! stands in for the provider, and the assertions are made against what the
//! harness actually sent it on the second turn.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use ai_chatbot_gateway::{
    ChatMessage, ChatRequest, Config, Execution, Gateway, HarnessSettings, ProviderConfig,
    ProviderKind, Role, StoreSettings,
    harness::{
        Harness, HarnessConfig, HarnessEvent, memory::HarnessMemory, tools::ToolContext,
        transcript::Budget, workspace::Workspace,
    },
    identity::Identity,
    quota::Limits,
    workspaces::WorkspaceMode,
};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Response, header},
    routing::post,
};
use serde_json::{Value, json};

/// A scripted OpenAI-compatible upstream that records what it was asked.
#[derive(Clone, Default)]
struct Upstream {
    requests: Arc<Mutex<Vec<Value>>>,
    script: Arc<Mutex<VecDeque<String>>>,
}

impl Upstream {
    fn with(turns: &[String]) -> Self {
        Self {
            requests: Arc::default(),
            script: Arc::new(Mutex::new(turns.iter().cloned().collect())),
        }
    }

    fn request(&self, index: usize) -> Value {
        self.requests
            .lock()
            .unwrap()
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("no upstream request at index {index}"))
    }

    fn tool_result(&self, index: usize) -> String {
        self.request(index)["messages"]
            .as_array()
            .expect("the request carries messages")
            .iter()
            .find(|message| message["role"] == "tool")
            .map(|message| message["content"].as_str().unwrap_or_default().to_string())
            .expect("a tool result is sent back")
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

async fn serve(upstream: Upstream) -> SocketAddr {
    async fn respond(State(upstream): State<Upstream>, body: String) -> Response<Body> {
        upstream
            .requests
            .lock()
            .unwrap()
            .push(serde_json::from_str(&body).unwrap_or(Value::Null));
        // An exhausted script answers with a bare stop rather than hanging, so
        // a loop that runs longer than the test expected fails on an assertion
        // instead of on a timeout.
        let next = upstream
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_turn("(script exhausted)"));
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(next))
            .unwrap()
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(upstream);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

fn text_turn(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": text}}]})
    )
}

fn tool_turn(id: &str, name: &str, arguments: Value) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()},
        }]}}]})
    )
}

fn config(address: SocketAddr, workspace: &std::path::Path) -> Config {
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
        coder_timeout: std::time::Duration::from_secs(30),
        coder_allow_writes: true,
        coder_max_concurrency: 2,
        workspace_mode: WorkspaceMode::Shared,
        quotas: Limits::default(),
        trusted_proxy: false,
        harness: HarnessSettings::default(),
        store: StoreSettings {
            enabled: false,
            ..StoreSettings::default()
        },
        identity: Identity::single_user(),
    }
}

fn harness(
    address: SocketAddr,
    workspace: &std::path::Path,
    memory: Option<Arc<HarnessMemory>>,
    max_steps: usize,
) -> Harness {
    let gateway = Arc::new(Gateway::new(config(address, workspace)).unwrap());
    let context = ToolContext {
        workspace: Workspace::new(workspace, true).unwrap(),
        memory,
    };
    Harness::new(
        gateway,
        context,
        HarnessConfig {
            max_steps,
            budget: Budget::default(),
            // Retrieval is exercised on its own below; leaving it on here would
            // put an extra system message into every assertion about the payload.
            memory_hits: 0,
            inject_memory: false,
        },
    )
}

fn ask(question: &str) -> ChatRequest {
    ChatRequest {
        ai_coder: None,
        provider: None,
        model: None,
        temperature: None,
        max_tokens: None,
        tools: None,
        messages: vec![ChatMessage {
            role: Role::User,
            content: question.into(),
            tool_calls: None,
            tool_call_id: None,
        }],
    }
}

fn events(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>) -> Vec<HarnessEvent> {
    let mut collected = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        collected.push(event);
    }
    collected
}

/// The claim the whole harness rests on.
#[tokio::test]
async fn a_tool_call_becomes_work_and_the_result_reaches_the_model() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("notes.md"),
        "the answer is forty-two\n",
    )
    .unwrap();

    let upstream = Upstream::with(&[
        tool_turn("call_1", "read_file", json!({"path": "notes.md"})),
        text_turn("The note says forty-two."),
    ]);
    let address = serve(upstream.clone()).await;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let outcome = harness(address, workspace.path(), None, 6)
        .run(ask("what is in notes.md?"), Some(sender))
        .await
        .unwrap();

    assert_eq!(outcome.text, "The note says forty-two.");
    assert_eq!(outcome.tool_calls, 1);
    assert_eq!(outcome.steps, 2);
    assert!(!outcome.stopped_at_step_limit);
    assert_eq!(upstream.count(), 2, "the loop must go round exactly twice");

    // The first request offers the tools; the second carries the result of the
    // one the model picked, in the role the provider expects.
    let offered: Vec<String> = upstream.request(0)["tools"]
        .as_array()
        .expect("tools are offered")
        .iter()
        .map(|tool| {
            tool["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(offered.contains(&"read_file".to_string()), "{offered:?}");

    let second = upstream.request(1);
    let result = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .cloned()
        .expect("the tool result is sent back");
    assert_eq!(result["tool_call_id"], "call_1");
    assert!(
        result["content"]
            .as_str()
            .unwrap()
            .contains("the answer is forty-two"),
        "the model must see what the tool actually read: {result}"
    );

    let collected = events(&mut receiver);
    assert!(collected.iter().any(|event| matches!(
        event,
        HarnessEvent::ToolCall { name, .. } if name == "read_file"
    )));
    assert!(
        collected
            .iter()
            .any(|event| matches!(event, HarnessEvent::ToolResult { failed: false, .. }))
    );
}

/// A failing tool is information, not an outage. Ending the run on one would
/// deny the model the chance to correct a path it got slightly wrong.
#[tokio::test]
async fn a_failed_tool_is_reported_to_the_model_and_the_run_continues() {
    let workspace = tempfile::tempdir().unwrap();
    let upstream = Upstream::with(&[
        tool_turn("call_1", "read_file", json!({"path": "absent.md"})),
        text_turn("That file does not exist."),
    ]);
    let address = serve(upstream.clone()).await;

    let outcome = harness(address, workspace.path(), None, 6)
        .run(ask("read absent.md"), None)
        .await
        .unwrap();

    assert_eq!(outcome.text, "That file does not exist.");
    assert!(
        upstream.tool_result(1).starts_with("Error:"),
        "{}",
        upstream.tool_result(1)
    );
}

/// A model that invents a tool has to be told so, in the same channel it asked.
#[tokio::test]
async fn an_unknown_tool_is_answered_rather_than_crashing_the_run() {
    let workspace = tempfile::tempdir().unwrap();
    let upstream = Upstream::with(&[
        tool_turn("call_1", "deploy_to_production", json!({})),
        text_turn("I do not have that tool."),
    ]);
    let address = serve(upstream.clone()).await;

    let outcome = harness(address, workspace.path(), None, 6)
        .run(ask("deploy"), None)
        .await
        .unwrap();

    assert_eq!(outcome.text, "I do not have that tool.");
    let content = upstream.tool_result(1);
    assert!(content.contains("no tool named"), "{content}");
    assert!(
        content.contains("read_file"),
        "it must say what does exist: {content}"
    );
}

/// A model that only ever calls tools would otherwise loop until the budget ran
/// out. The run ends, and says why.
#[tokio::test]
async fn the_loop_stops_at_the_step_limit_and_says_so() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a.txt"), "a").unwrap();
    let turns: Vec<String> = (0..10)
        .map(|n| tool_turn(&format!("call_{n}"), "read_file", json!({"path": "a.txt"})))
        .collect();
    let upstream = Upstream::with(&turns);
    let address = serve(upstream.clone()).await;

    let outcome = harness(address, workspace.path(), None, 3)
        .run(ask("keep reading"), None)
        .await
        .unwrap();

    assert!(outcome.stopped_at_step_limit);
    assert_eq!(outcome.steps, 3);
    assert_eq!(outcome.tool_calls, 3);
    assert_eq!(upstream.count(), 3, "the limit bounds upstream calls too");
}

/// The retrieval layer was write-only from the agent's perspective until a tool
/// read it. This is that tool, over a real index.
#[tokio::test]
async fn the_agent_can_search_the_codebase_index() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("src")).unwrap();
    std::fs::write(
        workspace.path().join("src/router.rs"),
        "/// Chooses which provider serves a request.\npub fn resolve_provider_route(coder: &str) -> Option<Route> {\n    None\n}\n",
    )
    .unwrap();

    let store = tempfile::tempdir().unwrap();
    let memory = Arc::new(HarnessMemory::open(store.path(), workspace.path()).unwrap());
    let stats = memory.index_workspace().unwrap();
    assert_eq!(stats.files, 1, "the workspace has one indexable file");
    assert!(stats.records > 0);

    let upstream = Upstream::with(&[
        tool_turn(
            "call_1",
            "search_memory",
            json!({"query": "resolve provider route"}),
        ),
        text_turn("It is resolve_provider_route in src/router.rs."),
    ]);
    let address = serve(upstream.clone()).await;

    let outcome = harness(address, workspace.path(), Some(memory), 6)
        .run(ask("where is provider routing decided?"), None)
        .await
        .unwrap();

    assert_eq!(outcome.tool_calls, 1);
    let content = upstream.tool_result(1);
    assert!(
        content.contains("resolve_provider_route"),
        "the index must return the symbol it holds: {content}"
    );
    assert!(content.contains("src/router.rs"), "{content}");
}

/// An index that keeps serving the body a file had *before* the agent edited it
/// is worse than no index: the agent reads its own stale work back.
#[tokio::test]
async fn an_edit_is_visible_to_the_next_search() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("seed.rs"), "pub fn seed() {}\n").unwrap();

    let store = tempfile::tempdir().unwrap();
    let memory = Arc::new(HarnessMemory::open(store.path(), workspace.path()).unwrap());
    memory.index_workspace().unwrap();
    assert!(
        memory
            .search("compact_transcript", 5)
            .unwrap()
            .iter()
            .all(|hit| hit.record.symbol.name != "compact_transcript"),
        "the symbol must not exist before the agent writes it"
    );

    let upstream = Upstream::with(&[
        tool_turn(
            "call_1",
            "write_file",
            json!({
                "path": "seed.rs",
                "content": "/// Prunes a transcript by whole turns.\npub fn compact_transcript() {}\n",
            }),
        ),
        text_turn("Written."),
    ]);
    let address = serve(upstream.clone()).await;

    harness(address, workspace.path(), Some(Arc::clone(&memory)), 6)
        .run(ask("add compact_transcript to seed.rs"), None)
        .await
        .unwrap();

    let hits = memory.search("compact_transcript", 5).unwrap();
    let found: Vec<String> = hits
        .iter()
        .map(|hit| hit.record.symbol.name.clone())
        .collect();
    assert!(
        found.iter().any(|name| name == "compact_transcript"),
        "the write must be indexed: {found:?}"
    );
    // And the row it replaced is gone, rather than both versions being live.
    assert!(
        !found.iter().any(|name| name == "seed"),
        "the previous version of the file must be tombstoned: {found:?}"
    );
}

/// A native CLI coder is already an agent loop. Offering it a tool registry it
/// cannot drive would be rejected by the gateway and wrong in principle.
#[cfg(unix)]
#[tokio::test]
async fn a_native_coder_route_is_delegated_rather_than_driven() {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir().unwrap();
    let executable = workspace.path().join("hermes");
    std::fs::write(&executable, "#!/bin/sh\nprintf 'the CLI answered'\n").unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).unwrap();

    let mut settings = config("127.0.0.1:1".parse().unwrap(), workspace.path());
    settings.providers.clear();
    settings.coders = vec![ai_chatbot_gateway::coders::CoderRoute {
        kind: ai_chatbot_gateway::coders::CoderKind::Hermes,
        provider: "auto".into(),
        provider_name: "Auto".into(),
        model: "default".into(),
        model_name: "Default".into(),
        executable,
    }];
    settings.default_coder = "hermes".into();
    settings.default_provider = "auto".into();

    let gateway = Arc::new(Gateway::new(settings).unwrap());
    assert_eq!(
        gateway.execution_for(&ask("hello")),
        Execution::Native,
        "the harness must recognise a CLI route before it offers tools"
    );

    let context = ToolContext {
        workspace: Workspace::new(workspace.path(), true).unwrap(),
        memory: None,
    };
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let outcome = Harness::new(gateway, context, HarnessConfig::default())
        .run(ask("hello"), Some(sender))
        .await
        .unwrap();

    assert_eq!(outcome.text, "the CLI answered");
    assert_eq!(outcome.tool_calls, 0);
    assert!(
        events(&mut receiver)
            .iter()
            .any(|event| matches!(event, HarnessEvent::Delegated { .. })),
        "delegation is reported, not silent"
    );
}

// ---------------------------------------------------------------------------
// Provider neutrality
//
// The loop above ran over OpenAI's wire format. The claim is that it runs over
// every format the gateway speaks, and the interesting half is the return
// journey: each provider names a tool result differently, and a harness that
// only got the *call* right would loop forever without noticing.
// ---------------------------------------------------------------------------

/// One provider's dialect, reduced to what a tool loop needs from it.
struct Protocol {
    kind: ProviderKind,
    /// The path the gateway will POST to, given a base URL of `http://addr/v1`.
    path: &'static str,
    tool_turn: fn(&str, &str, Value) -> String,
    text_turn: fn(&str) -> String,
    /// The tool result, dug out of wherever this provider puts it.
    tool_result: fn(&Value) -> Option<String>,
}

fn anthropic_tool_turn(id: &str, name: &str, arguments: Value) -> String {
    format!(
        "event: content_block_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": id, "name": name},
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": arguments.to_string()},
        }),
        json!({"type": "message_stop"})
    )
}

fn anthropic_text_turn(text: &str) -> String {
    format!(
        "event: content_block_delta\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": text},
        }),
        json!({"type": "message_stop"})
    )
}

/// Gemini sends a whole function call in one chunk and finishes in another.
fn gemini_tool_turn(_id: &str, name: &str, arguments: Value) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": name, "args": arguments}}]}}]}),
        json!({"candidates": [{"finishReason": "STOP"}]})
    )
}

fn gemini_text_turn(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"candidates": [{"content": {"parts": [{"text": text}]}}]}),
        json!({"candidates": [{"finishReason": "STOP"}]})
    )
}

fn openai_tool_result(request: &Value) -> Option<String> {
    request["messages"]
        .as_array()?
        .iter()
        .find(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap_or_default().to_string())
}

/// Anthropic carries a result as a `tool_result` block on a user turn.
fn anthropic_tool_result(request: &Value) -> Option<String> {
    request["messages"]
        .as_array()?
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result")
        .map(|block| block["content"].as_str().unwrap_or_default().to_string())
}

/// Gemini keys a result by the tool's *name*, which the gateway has to recover
/// from an id the model never sent back.
fn gemini_tool_result(request: &Value) -> Option<String> {
    let response = request["contents"]
        .as_array()?
        .iter()
        .filter_map(|content| content["parts"].as_array())
        .flatten()
        .find(|part| part.get("functionResponse").is_some())?;
    assert_eq!(
        response["functionResponse"]["name"], "read_file",
        "Gemini needs the tool's name, not its id"
    );
    Some(
        response["functionResponse"]["response"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

async fn serve_at(path: &'static str, upstream: Upstream) -> SocketAddr {
    async fn respond(State(upstream): State<Upstream>, body: String) -> Response<Body> {
        upstream
            .requests
            .lock()
            .unwrap()
            .push(serde_json::from_str(&body).unwrap_or(Value::Null));
        let next = upstream
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_turn("(script exhausted)"));
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(next))
            .unwrap()
    }

    let app = Router::new()
        .route(path, post(respond))
        .with_state(upstream);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

/// The same tool loop, over every protocol the gateway speaks.
#[tokio::test]
async fn the_tool_loop_runs_over_every_provider_protocol() {
    let protocols = [
        Protocol {
            kind: ProviderKind::Openai,
            path: "/v1/chat/completions",
            tool_turn,
            text_turn,
            tool_result: openai_tool_result,
        },
        Protocol {
            kind: ProviderKind::OpenaiCompatible,
            path: "/v1/chat/completions",
            tool_turn,
            text_turn,
            tool_result: openai_tool_result,
        },
        Protocol {
            kind: ProviderKind::Anthropic,
            path: "/v1/v1/messages",
            tool_turn: anthropic_tool_turn,
            text_turn: anthropic_text_turn,
            tool_result: anthropic_tool_result,
        },
        Protocol {
            kind: ProviderKind::Gemini,
            path: "/v1/models/test-model:streamGenerateContent",
            tool_turn: gemini_tool_turn,
            text_turn: gemini_text_turn,
            tool_result: gemini_tool_result,
        },
    ];

    for protocol in protocols {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("notes.md"),
            "the answer is forty-two\n",
        )
        .unwrap();

        let upstream = Upstream::with(&[
            (protocol.tool_turn)("call_1", "read_file", json!({"path": "notes.md"})),
            (protocol.text_turn)("The note says forty-two."),
        ]);
        let address = serve_at(protocol.path, upstream.clone()).await;

        let mut settings = config(address, workspace.path());
        settings.providers[0].kind = protocol.kind;
        let gateway = Arc::new(Gateway::new(settings).unwrap());
        let context = ToolContext {
            workspace: Workspace::new(workspace.path(), true).unwrap(),
            memory: None,
        };
        let outcome = Harness::new(
            gateway,
            context,
            HarnessConfig {
                max_steps: 4,
                budget: Budget::default(),
                memory_hits: 0,
                inject_memory: false,
            },
        )
        .run(ask("what is in notes.md?"), None)
        .await
        .unwrap_or_else(|error| panic!("{:?} run failed: {error}", protocol.kind));

        assert_eq!(
            outcome.text, "The note says forty-two.",
            "{:?} did not finish the loop",
            protocol.kind
        );
        assert_eq!(outcome.tool_calls, 1, "{:?}", protocol.kind);
        assert_eq!(upstream.count(), 2, "{:?}", protocol.kind);

        let result = (protocol.tool_result)(&upstream.request(1)).unwrap_or_else(|| {
            panic!(
                "{:?} did not carry the tool result back: {}",
                protocol.kind,
                upstream.request(1)
            )
        });
        assert!(
            result.contains("the answer is forty-two"),
            "{:?} lost the tool's output: {result}",
            protocol.kind
        );

        // The tools themselves have to survive translation, or the model never
        // learns it has any.
        let first = upstream.request(0);
        let offered = match protocol.kind {
            ProviderKind::Gemini => first["tools"][0]["functionDeclarations"].clone(),
            _ => first["tools"].clone(),
        };
        assert!(
            offered.to_string().contains("read_file"),
            "{:?} was not offered the tools: {offered}",
            protocol.kind
        );
    }
}

// ---------------------------------------------------------------------------
// The HTTP surface
// ---------------------------------------------------------------------------

use ai_chatbot_gateway::build_api_router;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn call(router: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn post_json(path: &str, payload: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

/// A client should be able to render the harness without guessing at it.
#[tokio::test]
async fn the_status_endpoint_describes_what_the_harness_can_do() {
    let workspace = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let mut settings = config("127.0.0.1:1".parse().unwrap(), workspace.path());
    settings.harness = HarnessSettings {
        memory_dir: Some(store.path().to_path_buf()),
        ..HarnessSettings::default()
    };

    let (status, body) = call(
        build_api_router(settings).unwrap(),
        Request::builder()
            .uri("/harness")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["writes_enabled"], true);
    let tools = value["tools"].as_array().unwrap();
    for expected in ["read_file", "write_file", "edit_file", "search_memory"] {
        assert!(
            tools.iter().any(|tool| tool == expected),
            "{expected} missing from {tools:?}"
        );
    }
    assert_eq!(value["memory"]["records"], 0);
}

/// The stream is the product: a client watches the agent work.
#[tokio::test]
async fn the_agent_endpoint_streams_the_work_it_did() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("notes.md"),
        "the answer is forty-two\n",
    )
    .unwrap();

    let upstream = Upstream::with(&[
        tool_turn("call_1", "read_file", json!({"path": "notes.md"})),
        text_turn("The note says forty-two."),
    ]);
    let address = serve(upstream.clone()).await;
    let mut settings = config(address, workspace.path());
    // Retrieval off: this test is about the stream, not about what it retrieved.
    settings.harness.memory_dir = None;

    let (status, body) = call(
        build_api_router(settings).unwrap(),
        post_json(
            "/agent",
            json!({"messages": [{"role": "user", "content": "what is in notes.md?"}]}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let frames: Vec<&str> = body
        .split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .collect();
    assert_eq!(*frames.last().unwrap(), "[DONE]", "{body}");

    let parsed: Vec<Value> = frames
        .iter()
        .filter(|frame| **frame != "[DONE]")
        .map(|frame| serde_json::from_str(frame).unwrap())
        .collect();
    assert!(
        parsed
            .iter()
            .any(|frame| frame["event"] == "tool_call" && frame["name"] == "read_file"),
        "{body}"
    );
    assert!(
        parsed.iter().any(|frame| frame["event"] == "tool_result"
            && frame["output"]
                .as_str()
                .unwrap_or_default()
                .contains("forty-two")),
        "{body}"
    );
    let outcome = parsed
        .iter()
        .find(|frame| frame["event"] == "outcome")
        .expect("the run reports an outcome");
    assert_eq!(outcome["text"], "The note says forty-two.");
    assert_eq!(outcome["tool_calls"], 1);
    assert_eq!(outcome["stopped_at_step_limit"], false);
}

#[tokio::test]
async fn the_index_endpoint_reports_what_it_indexed() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("lib.rs"),
        "/// Routes a request.\npub fn route() {}\n",
    )
    .unwrap();
    let store = tempfile::tempdir().unwrap();
    let mut settings = config("127.0.0.1:1".parse().unwrap(), workspace.path());
    settings.harness.memory_dir = Some(store.path().to_path_buf());

    let (status, body) = call(
        build_api_router(settings).unwrap(),
        post_json("/harness/index", json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["files"], 1);
    assert!(value["records"].as_u64().unwrap() >= 1, "{body}");
}

/// Asking to index when there is no index is a configuration mistake, and
/// answering "0 files" would hide it.
#[tokio::test]
async fn indexing_without_a_configured_store_is_refused_clearly() {
    let workspace = tempfile::tempdir().unwrap();
    let mut settings = config("127.0.0.1:1".parse().unwrap(), workspace.path());
    settings.harness.memory_dir = None;

    let (status, body) = call(
        build_api_router(settings).unwrap(),
        post_json("/harness/index", json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("memory_disabled"), "{body}");
}

/// The standard deployment mounts the workspace read-only, so the default index
/// location is routinely unwritable. That must degrade, not refuse to start.
#[tokio::test]
async fn an_unwritable_default_index_location_does_not_stop_the_service() {
    let workspace = tempfile::tempdir().unwrap();
    let mut settings = config("127.0.0.1:1".parse().unwrap(), workspace.path());
    settings.harness.memory_dir = Some(workspace.path().join("lib.rs/memory"));
    settings.harness.memory_required = false;
    std::fs::write(workspace.path().join("lib.rs"), "pub fn route() {}\n").unwrap();

    let (status, body) = call(
        build_api_router(settings).unwrap(),
        Request::builder()
            .uri("/harness")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert!(value["memory"].is_null(), "{body}");
    // And the tool is not advertised, rather than advertised and always failing.
    let tools = value["tools"].as_array().unwrap();
    assert!(
        !tools.iter().any(|tool| tool == "search_memory"),
        "{tools:?}"
    );
}

/// An operator who names the directory has asked for retrieval. Starting
/// without it would hide the misconfiguration.
#[test]
fn a_configured_index_location_that_fails_is_a_startup_error() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("lib.rs"), "pub fn route() {}\n").unwrap();
    let mut settings = config("127.0.0.1:1".parse().unwrap(), workspace.path());
    settings.harness.memory_dir = Some(workspace.path().join("lib.rs/memory"));
    settings.harness.memory_required = true;

    assert!(build_api_router(settings).is_err());
}

/// The metrics endpoint is how an operator sees the harness working at all.
#[tokio::test]
async fn harness_activity_reaches_the_metrics_endpoint() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.md"), "forty-two\n").unwrap();
    let upstream = Upstream::with(&[
        tool_turn("call_1", "read_file", json!({"path": "notes.md"})),
        text_turn("Done."),
    ]);
    let address = serve(upstream).await;
    let mut settings = config(address, workspace.path());
    settings.harness.memory_dir = None;
    let router = build_api_router(settings).unwrap();

    let (status, _) = call(
        router.clone(),
        post_json(
            "/agent",
            json!({"messages": [{"role": "user", "content": "read notes.md"}]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = call(
        router,
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body.contains("lumen_agent_requests_total 1"), "{body}");
    assert!(body.contains("lumen_agent_tool_calls_total 1"), "{body}");
    assert!(body.contains("lumen_memory_records 0"), "{body}");
}
