//! End-to-end coverage for validated OpenAI Responses requests dispatched to a
//! native Responses target through the API frontend and streaming pipeline.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use kinetix::{
    api,
    app::AppState,
    config::Config,
    crypto::{self, Crypto},
    db,
    logqueue::UsageLogQueue,
    paths::Paths,
    registry::Registry,
    types::{AuthScheme, WireFormat},
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const CLIENT_KEY: &str = "sk-kinetix-responses-pipeline-test";
const UPSTREAM_MODEL: &str = "upstream-responses-model";

#[derive(Clone)]
struct CapturedRequest {
    method: Method,
    path: String,
    body: Value,
}

#[derive(Clone, Default)]
struct MockUpstream {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

async fn upstream(
    State(mock): State<MockUpstream>,
    method: Method,
    uri: Uri,
    Json(body): Json<Value>,
) -> Response {
    let input = body.get("input");
    let test_case = input
        .and_then(Value::as_str)
        .and_then(|input| input.strip_prefix("case:"))
        .or_else(|| {
            input
                .and_then(|input| input.pointer("/0/content/0/text"))
                .and_then(Value::as_str)
                .and_then(|input| input.strip_prefix("case:"))
        })
        .unwrap_or_default()
        .to_string();
    mock.requests.lock().await.push(CapturedRequest {
        method,
        path: uri.path().to_string(),
        body,
    });

    match test_case.as_str() {
        "stream_refusal" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"type\":\"response.refusal.delta\",\"delta\":\"I cannot help with that.\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":6}}}\n\n"
            )))
            .unwrap(),
        "incomplete" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial answer\"}\n\n",
                "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":11,\"output_tokens\":12}}}\n\n"
            )))
            .unwrap(),
        "full_refusal" => Json(json!({
            "id": "resp_full_refusal",
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "refusal", "refusal": "I cannot help with that."}]
            }],
            "usage": {"input_tokens": 7, "output_tokens": 8}
        }))
        .into_response(),
        _ => (StatusCode::BAD_REQUEST, "unexpected test case").into_response(),
    }
}

async fn call_responses(state: &AppState, test_case: &str, stream: bool) -> (StatusCode, String) {
    let raw_body = json!({
        "model": "responses-route",
        "input": format!("case:{test_case}"),
        "stream": stream,
        "stream_options": {"include_obfuscation": false},
        "text": {"format": {"type": "text"}}
    })
    .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    let response = api::responses(State(state.clone()), headers, raw_body).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn call_chat(state: &AppState) -> (StatusCode, String) {
    let raw_body = json!({
        "model": "responses-route",
        "messages": [{"role": "user", "content": "case:incomplete"}],
        "stream": true
    })
    .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    let response = api::chat_completions(State(state.clone()), headers, raw_body).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn responses_passthrough_and_cross_format_incomplete_events_work_end_to_end() {
    let mock = MockUpstream::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_capture = mock.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(any(upstream))
                .with_state(server_capture),
        )
        .await
        .unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-responses-pipeline-{}",
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
    let crypto = Arc::new(Crypto::new(&[17_u8; 32]));

    let base_url = format!("http://{addr}/v1");
    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-openai-responses",
            base_url: &base_url,
            wire_format: WireFormat::Openai,
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
        "mock-account",
        &crypto.encrypt("mock-upstream-key").unwrap(),
        "mock-upstream-key",
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
            upstream_id: UPSTREAM_MODEL,
            display_name: "Responses model",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!({"text": true}),
            prices: json!({}),
            parameters: json!({}),
            thinking_map: json!({}),
            extra_request: json!({}),
            discovery: json!({"configured_transport": "openai-responses"}),
        },
    )
    .await
    .unwrap();
    let route_id = db::insert_route(
        &pool,
        &db::NewRoute {
            name: "responses-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({}),
            portability_policy: "reject",
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
    db::insert_virtual_key(
        &pool,
        &db::VirtualKeyRow {
            id: "responses-test-key".into(),
            key_hash: crypto::hash_virtual_key(CLIENT_KEY),
            name: "Responses pipeline test".into(),
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
        },
    )
    .await
    .unwrap();

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();
    let state = AppState::new(
        Arc::new(Config {
            bind: "127.0.0.1:0".into(),
            public_base_url: "http://127.0.0.1".into(),
            database_url,
            master_key: [17_u8; 32],
            admin_token: "test-admin-token".into(),
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
        registry,
        crypto,
        reqwest::Client::new(),
        UsageLogQueue::new(pool, 16),
        0,
    );

    let (status, streamed_refusal) = call_responses(&state, "stream_refusal", true).await;
    assert_eq!(status, StatusCode::OK, "{streamed_refusal}");
    assert!(streamed_refusal.contains("I cannot help with that."));
    assert!(streamed_refusal.contains("response.completed"));
    assert!(!streamed_refusal.contains("response.failed"));

    let (status, full_refusal) = call_responses(&state, "full_refusal", false).await;
    assert_eq!(status, StatusCode::OK, "{full_refusal}");
    let full_refusal: Value = serde_json::from_str(&full_refusal).unwrap();
    assert_eq!(
        full_refusal.pointer("/output/0/content/0/text"),
        Some(&json!("I cannot help with that."))
    );

    let (status, incomplete) = call_chat(&state).await;
    assert_eq!(status, StatusCode::OK, "{incomplete}");
    assert!(incomplete.contains("partial answer"));
    assert!(
        incomplete.contains("\"finish_reason\":\"length\""),
        "{incomplete}"
    );
    assert!(incomplete.contains("[DONE]"));
    assert!(!incomplete.contains("response.failed"));

    let requests = mock.requests.lock().await;
    assert_eq!(requests.len(), 3);
    for (request, test_case) in requests
        .iter()
        .take(2)
        .zip(["stream_refusal", "full_refusal"])
    {
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(request.body["model"], UPSTREAM_MODEL);
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["store"], false);
        assert_eq!(request.body["input"], format!("case:{test_case}"));
        assert_eq!(
            request.body.pointer("/stream_options/include_obfuscation"),
            Some(&json!(false))
        );
        assert_eq!(
            request.body.pointer("/text/format/type"),
            Some(&json!("text"))
        );
    }
    let incomplete_request = &requests[2];
    assert_eq!(incomplete_request.method, Method::POST);
    assert_eq!(incomplete_request.path, "/v1/responses");
    assert_eq!(incomplete_request.body["model"], UPSTREAM_MODEL);
    assert_eq!(incomplete_request.body["store"], false);
    assert_eq!(
        incomplete_request.body.pointer("/input/0/content/0/text"),
        Some(&json!("case:incomplete"))
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&root);
}
