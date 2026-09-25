//! Integration coverage for Gemini opaque `thoughtSignature` replay through a
//! translated frontend (OpenAI Chat Completions). The mock upstream fails
//! closed exactly like the real Gemini API: once a signed function call is
//! continued, every historical function-call part must carry its signature.
//! Anything less returns the provider's real "Function call is missing a
//! thought_signature" 400. That makes every passing assertion below evidence
//! that Kinetix really captured, persisted, and replayed the signature rather
//! than accidentally satisfying the mock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use kinetix::{
    app::AppState,
    config::Config,
    crypto::Crypto,
    db,
    frontends::FrontendFormat,
    logqueue::UsageLogQueue,
    paths::Paths,
    pipeline,
    registry::Registry,
    types::{AuthScheme, InternalRequest, Message, Part, Role, WireFormat},
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const SIGNATURE: &str = "MOCK_GEMINI_SIGNATURE";
const TOOL_CALL_ID: &str = "call_mock_1";
const TOOL_NAME: &str = "get_weather";

#[derive(Clone, Default)]
struct MockUpstream {
    requests: Arc<Mutex<Vec<Value>>>,
    /// Continuations that arrived with the expected signature on every
    /// function-call part.
    signed_continuations: Arc<AtomicUsize>,
    /// Continuations that arrived unsigned (or with the wrong signature).
    unsigned_continuations: Arc<AtomicUsize>,
}

impl MockUpstream {
    async fn record(&self, body: &Value) -> bool {
        self.requests.lock().await.push(body.clone());
        let calls = function_call_parts(body);
        let missing = calls.is_empty()
            || calls.iter().any(|part| {
                part.get("thoughtSignature").and_then(Value::as_str) != Some(SIGNATURE)
            });
        if contains_key(body, "functionResponse") {
            if missing {
                self.unsigned_continuations.fetch_add(1, Ordering::SeqCst);
            } else {
                self.signed_continuations.fetch_add(1, Ordering::SeqCst);
            }
        }
        missing
    }
}

fn contains_key(value: &Value, name: &str) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| k == name || contains_key(v, name)),
        Value::Array(items) => items.iter().any(|item| contains_key(item, name)),
        _ => false,
    }
}

fn function_call_parts(value: &Value) -> Vec<Value> {
    // Return the enclosing part so the sibling `thoughtSignature` is visible.
    let mut found = Vec::new();
    fn walk(node: &Value, found: &mut Vec<Value>) {
        match node {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "functionCall" && child.is_object() {
                        found.push(Value::Object(map.clone()));
                    }
                    walk(child, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, found)),
            _ => {}
        }
    }
    walk(value, &mut found);
    found
}

async fn gemini_upstream(State(mock): State<MockUpstream>, Json(body): Json<Value>) -> Response {
    let missing = mock.record(&body).await;
    let has_response = contains_key(&body, "functionResponse");
    if has_response && missing {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "Function call is missing a thought_signature in functionCall parts. \
                                This is required for tools to work correctly.",
                    "status": "INVALID_ARGUMENT"
                }
            })),
        )
            .into_response();
    }

    let frames = if has_response {
        vec![
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "done"}]}}]}),
            json!({
                "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
            }),
        ]
    } else {
        vec![
            json!({"candidates": [{"content": {"role": "model", "parts": [{
                "functionCall": {"id": TOOL_CALL_ID, "name": TOOL_NAME, "args": {"city": "Paris"}},
                "thoughtSignature": SIGNATURE
            }]}}]}),
            json!({
                "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
            }),
        ]
    };

    let mut sse = String::new();
    for frame in frames {
        sse.push_str("data: ");
        sse.push_str(&frame.to_string());
        sse.push_str("\r\n\r\n");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

struct Harness {
    state: AppState,
    pool: db::Pool,
    crypto: Arc<Crypto>,
    registry: Arc<Registry>,
    database_url: String,
    mock: MockUpstream,
    server: tokio::task::JoinHandle<()>,
    root: std::path::PathBuf,
}

async fn build_state(harness: &Harness) -> AppState {
    let config = Arc::new(Config {
        bind: "127.0.0.1:0".into(),
        public_base_url: "http://127.0.0.1".into(),
        database_url: harness.database_url.clone(),
        master_key: [11_u8; 32],
        admin_token: "test-admin".into(),
        cf_access_aud: None,
        cf_access_team_domain: None,
        log_json: false,
        bootstrap_file: None,
        allow_private_upstreams: true,
        allow_insecure_tls: true,
        data_dir: harness.root.join("data"),
        shutdown_grace_secs: 1,
        alert_webhook_url: None,
        alert_fallback_rate: 1.0,
        alert_error_rate: 1.0,
        alert_min_requests: 1,
        alert_interval_secs: 60,
        alert_p95_latency_ms: 1_000,
        ip_rate_limit_per_min: 0,
        session_ttl_minutes: 60,
        export_retention_days: 1,
        paths: Paths {
            config_dir: harness.root.join("config"),
            data_dir: harness.root.join("data"),
            state_dir: harness.root.join("state"),
        },
        generated_admin_password: None,
    });
    AppState::new(
        config,
        harness.pool.clone(),
        harness.registry.clone(),
        harness.crypto.clone(),
        reqwest::Client::new(),
        UsageLogQueue::new(harness.pool.clone(), 16),
        0,
    )
}

async fn setup() -> Harness {
    let mock = MockUpstream::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(post(gemini_upstream))
        .with_state(mock.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-opaque-gemini-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let paths = Paths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
    };
    paths.ensure_dirs().unwrap();
    let database_url = paths.database_url();
    let pool = db::connect(&database_url).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let crypto = Arc::new(Crypto::new(&[11_u8; 32]));
    let base_url = format!("http://{addr}");
    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-gemini",
            base_url: &base_url,
            wire_format: WireFormat::Gemini,
            auth_scheme: AuthScheme::Bearer,
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: json!({}),
            timeout_ms: 2_000,
            capability_mode: "permissive",
            models_path: None,
            rate_limit_rules: json!({}),
            follow_redirects: false,
            credential_hosts: "",
            allow_insecure_tls: true,
            wire_plugin: "",
            credential_plugin: "",
            model_source_plugin: "",
            credential_mode: "manual",
            source_plugin_id: None,
            source_integration_id: None,
        },
    )
    .await
    .unwrap();

    db::insert_account(
        &pool,
        &provider_id,
        "mock-gemini-account",
        &crypto.encrypt("mock-key").unwrap(),
        "mock-key",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();

    let model_id = db::insert_model(
        &pool,
        &db::NewModel {
            provider_id: &provider_id,
            upstream_id: "gemini-mock",
            display_name: "Gemini Mock",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!(["text", "tool_calling"]),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({}),
        },
    )
    .await
    .unwrap();

    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "opaque-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "strip_with_warning",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(1),
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &route_id, None, &model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();

    Harness {
        state: AppState::new(
            Arc::new(Config {
                bind: "127.0.0.1:0".into(),
                public_base_url: "http://127.0.0.1".into(),
                database_url: database_url.clone(),
                master_key: [11_u8; 32],
                admin_token: "test-admin".into(),
                cf_access_aud: None,
                cf_access_team_domain: None,
                log_json: false,
                bootstrap_file: None,
                allow_private_upstreams: true,
                allow_insecure_tls: true,
                data_dir: paths.data_dir.clone(),
                shutdown_grace_secs: 1,
                alert_webhook_url: None,
                alert_fallback_rate: 1.0,
                alert_error_rate: 1.0,
                alert_min_requests: 1,
                alert_interval_secs: 60,
                alert_p95_latency_ms: 1_000,
                ip_rate_limit_per_min: 0,
                session_ttl_minutes: 60,
                export_retention_days: 1,
                paths,
                generated_admin_password: None,
            }),
            pool.clone(),
            registry.clone(),
            crypto.clone(),
            reqwest::Client::new(),
            UsageLogQueue::new(pool.clone(), 16),
            0,
        ),
        pool,
        crypto,
        registry,
        database_url,
        mock,
        server,
        root,
    }
}

async fn cleanup(harness: Harness) {
    harness.server.abort();
    let _ = std::fs::remove_dir_all(&harness.root);
}

fn virtual_key(id: &str) -> db::VirtualKeyRow {
    db::VirtualKeyRow {
        id: id.into(),
        key_hash: format!("hash-{id}"),
        name: id.into(),
        owner: "test".into(),
        tag: String::new(),
        allowed_models: json!(["*"]).to_string(),
        allowed_providers: json!([]).to_string(),
        rpm_limit: None,
        tpm_limit: None,
        daily_budget: None,
        monthly_budget: None,
        expires_at: None,
        status: "active".into(),
        allowed_ips: json!([]).to_string(),
        body_logging: 0,
        created_at: db::now_iso(),
        revoked_at: None,
    }
}

fn first_turn(stream: bool) -> InternalRequest {
    InternalRequest {
        requested_model: "opaque-route".into(),
        system: vec![],
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::Text("what is the weather in Paris?".into())],
        }],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: Default::default(),
        stream,
        include_usage: false,
        thinking: None,
        extra: serde_json::Map::new(),
        raw_body: None,
    }
}

fn second_turn(stream: bool, tool_call_id: &str, tool_name: &str) -> InternalRequest {
    InternalRequest {
        requested_model: "opaque-route".into(),
        system: vec![],
        messages: vec![
            Message {
                role: Role::User,
                parts: vec![Part::Text("what is the weather in Paris?".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall {
                    id: Some(tool_call_id.into()),
                    name: tool_name.into(),
                    arguments: json!({"city": "Paris"}).to_string(),
                    // The client protocol cannot carry the Gemini signature, so
                    // it always arrives absent; only Kinetix can restore it.
                    signature: None,
                }],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    tool_call_id: tool_call_id.into(),
                    name: Some(tool_name.into()),
                    content: "18C".into(),
                    is_error: false,
                }],
            },
        ],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: Default::default(),
        stream,
        include_usage: false,
        thinking: None,
        extra: serde_json::Map::new(),
        raw_body: None,
    }
}

/// Run one request and fully drain the response body, so any capture that
/// happens while the response is streamed has completed before the caller
/// inspects the mock's counters.
async fn run(
    state: &AppState,
    key: Option<&db::VirtualKeyRow>,
    req: InternalRequest,
    request_id: &str,
) -> Result<u16, String> {
    let response = pipeline::run(
        state,
        FrontendFormat::OpenAi,
        key.cloned(),
        req,
        request_id.into(),
        true,
        None,
        vec![],
    )
    .await
    .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;
    Ok(status)
}

#[tokio::test]
async fn translated_tool_signature_is_replayed_after_restart() {
    let harness = setup().await;
    let key = virtual_key("key-restart");

    // Turn 1 captures the signature.
    run(
        &harness.state,
        Some(&key),
        first_turn(false),
        "req_opaque_restart_1",
    )
    .await
    .expect("turn 1 should dispatch and capture the signature");
    assert_eq!(harness.mock.requests.lock().await.len(), 1);

    // A fresh AppState over the same database has an empty RAM cache: this is
    // the process-restart case where only SQLite survives.
    let restarted = build_state(&harness).await;
    let status = run(
        &restarted,
        Some(&key),
        second_turn(false, TOOL_CALL_ID, TOOL_NAME),
        "req_opaque_restart_2",
    )
    .await
    .expect("turn 2 should replay the persisted signature");
    assert_eq!(status, 200);

    assert_eq!(
        harness.mock.signed_continuations.load(Ordering::SeqCst),
        1,
        "the continuation must have reached Gemini with its signature restored"
    );
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        0
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn translated_tool_signature_is_replayed_on_streaming_path() {
    let harness = setup().await;
    let key = virtual_key("key-stream");

    run(
        &harness.state,
        Some(&key),
        first_turn(true),
        "req_opaque_stream_1",
    )
    .await
    .expect("streaming turn 1 should capture the signature");
    run(
        &harness.state,
        Some(&key),
        second_turn(true, TOOL_CALL_ID, TOOL_NAME),
        "req_opaque_stream_2",
    )
    .await
    .expect("streaming turn 2 should replay the signature");

    assert_eq!(harness.mock.signed_continuations.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        0
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn cross_scope_replay_is_never_served_to_another_key() {
    let harness = setup().await;
    let owner = virtual_key("key-owner");
    let stranger = virtual_key("key-stranger");

    run(
        &harness.state,
        Some(&owner),
        first_turn(false),
        "req_opaque_scope_1",
    )
    .await
    .expect("owner turn 1 should capture the signature");

    // The stranger reuses the same client-visible tool-call id. It must not be
    // handed the owner's signature.
    let response = run(
        &harness.state,
        Some(&stranger),
        second_turn(false, TOOL_CALL_ID, TOOL_NAME),
        "req_opaque_scope_2",
    )
    .await;
    assert!(
        response.is_err(),
        "a cross-scope continuation must not silently succeed"
    );
    assert_eq!(harness.mock.signed_continuations.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness.mock.unsigned_continuations.load(Ordering::SeqCst),
        1,
        "the stranger's continuation must have reached Gemini unsigned"
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn reusing_a_tool_call_id_with_a_different_name_is_rejected_before_dispatch() {
    let harness = setup().await;
    let key = virtual_key("key-name-mismatch");

    run(
        &harness.state,
        Some(&key),
        first_turn(false),
        "req_opaque_name_1",
    )
    .await
    .expect("turn 1 should capture the signature");
    assert_eq!(harness.mock.requests.lock().await.len(), 1);

    // Same id, different tool name: the stored signature belongs to
    // `get_weather`, so replaying it onto `read_file` would be wrong.
    let error = run(
        &harness.state,
        Some(&key),
        second_turn(false, TOOL_CALL_ID, "read_file"),
        "req_opaque_name_2",
    )
    .await
    .expect_err("a reused id with a different tool name must be rejected");
    assert!(
        error.contains("different tool name"),
        "unexpected rejection message: {error}"
    );
    assert_eq!(
        harness.mock.requests.lock().await.len(),
        1,
        "the mismatch must be rejected before any upstream dispatch"
    );

    cleanup(harness).await;
}
