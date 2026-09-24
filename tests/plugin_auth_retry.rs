use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use kinetix::{
    app::AppState,
    config::Config,
    credentials::{
        CredentialHealth, CredentialRotationError, CredentialStrategy, ResolvedCredential,
    },
    crypto::Crypto,
    db,
    frontends::FrontendFormat,
    logqueue::UsageLogQueue,
    paths::Paths,
    pipeline,
    registry::Registry,
    types::{AuthScheme, InternalRequest, Message, Part, Role, WireFormat},
};
use serde_json::json;
use tokio::sync::{Barrier, Mutex};

#[derive(Clone, Default)]
struct MockUpstream {
    attempts: Arc<Mutex<Vec<String>>>,
    stale_barrier: Option<Arc<Barrier>>,
}

async fn chat_completions(State(state): State<MockUpstream>, headers: HeaderMap) -> Response {
    let auth = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.attempts.lock().await.push(auth.clone());

    match auth.as_str() {
        "Bearer stale-token" => {
            if let Some(barrier) = &state.stale_barrier {
                barrier.wait().await;
            }
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "message": "invalid authentication credentials",
                        "code": "invalid_api_key"
                    }
                })),
            )
                .into_response()
        }
        "Bearer fresh-token" | "Bearer fallback-token" => (
            StatusCode::OK,
            Json(json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "ok"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1
                }
            })),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Clone, Copy)]
enum RotationMode {
    Success { delay_ms: u64 },
    RetryableFailure,
    TerminalExpired,
}

struct TestCredential {
    secret: Mutex<String>,
    rotations: AtomicUsize,
    mode: RotationMode,
}

impl TestCredential {
    fn new(mode: RotationMode) -> Self {
        Self {
            secret: Mutex::new("stale-token".into()),
            rotations: AtomicUsize::new(0),
            mode,
        }
    }
}

#[async_trait]
impl CredentialStrategy for TestCredential {
    fn name(&self) -> &'static str {
        "test_rotating_credential"
    }

    async fn resolve(&self, account: &db::AccountRow) -> anyhow::Result<ResolvedCredential> {
        let secret = if account.label == "fallback-account" {
            "fallback-token".to_string()
        } else {
            self.secret.lock().await.clone()
        };
        Ok(ResolvedCredential {
            secret,
            expires_at: None,
            rotated: self.rotations.load(Ordering::Relaxed) > 0,
        })
    }

    async fn health(&self, _account: &db::AccountRow) -> CredentialHealth {
        CredentialHealth::Healthy
    }

    async fn rotate(
        &self,
        _account: &db::AccountRow,
    ) -> std::result::Result<(), CredentialRotationError> {
        self.rotations.fetch_add(1, Ordering::Relaxed);
        match self.mode {
            RotationMode::Success { delay_ms } => {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                *self.secret.lock().await = "fresh-token".into();
                Ok(())
            }
            RotationMode::RetryableFailure => Err(CredentialRotationError::new(
                "upstream_unavailable",
                "refresh endpoint unavailable",
                true,
                Some(5),
            )),
            RotationMode::TerminalExpired => Err(CredentialRotationError::new(
                "credential_expired",
                "refresh token revoked",
                false,
                None,
            )),
        }
    }
}

struct Harness {
    state: AppState,
    pool: db::Pool,
    account_id: String,
    fallback_account_id: String,
    request: InternalRequest,
    upstream: MockUpstream,
    server: tokio::task::JoinHandle<()>,
    root: PathBuf,
}

async fn setup(
    strategy: Arc<dyn CredentialStrategy>,
    stale_barrier: Option<Arc<Barrier>>,
    max_attempts: i64,
) -> Harness {
    let upstream = MockUpstream {
        attempts: Arc::new(Mutex::new(Vec::new())),
        stale_barrier,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/chat/completions", post(chat_completions))
        .with_state(upstream.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "kinetix-plugin-auth-retry-{}",
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

    let crypto = Arc::new(Crypto::new(&[7_u8; 32]));
    let base_url = format!("http://{addr}");
    let provider_id = db::insert_provider(
        &pool,
        &db::NewProvider {
            name: "mock-oauth",
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
            credential_plugin: "plugin:test.oauth/oauth",
            model_source_plugin: "",
        },
    )
    .await
    .unwrap();

    let account_id = db::insert_account(
        &pool,
        &provider_id,
        "oauth-account",
        &crypto.encrypt("ignored").unwrap(),
        "oauth",
        1,
        1,
        None,
        "none",
    )
    .await
    .unwrap();

    let fallback_account_id = db::insert_account(
        &pool,
        &provider_id,
        "fallback-account",
        &crypto.encrypt("ignored-fallback").unwrap(),
        "oauth-fallback",
        2,
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
            upstream_id: "test-model",
            display_name: "Test Model",
            enabled: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: json!({}),
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
            name: "oauth-route",
            description: "",
            strategy: "priority",
            fallback_triggers: json!({
                "on429": true,
                "onQuota": true,
                "on5xx": true,
                "onTimeout": true
            }),
            portability_policy: "strip_with_warning",
            sticky_routing: false,
            cache_affinity: false,
            max_attempts: Some(max_attempts),
        },
    )
    .await
    .unwrap();
    db::insert_route_target(&pool, &route_id, None, &model_id, 1, 1, "{}", "{}")
        .await
        .unwrap();

    let registry = Arc::new(Registry::new());
    registry.reload(&pool).await.unwrap();

    let config = Arc::new(Config {
        bind: "127.0.0.1:0".into(),
        public_base_url: "http://127.0.0.1".into(),
        database_url,
        master_key: [7_u8; 32],
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
    });

    let state = AppState::new(
        config,
        pool.clone(),
        registry,
        crypto,
        reqwest::Client::new(),
        UsageLogQueue::new(pool.clone(), 16),
        0,
    );
    state.register_plugin_credential_strategy("test.oauth", strategy);

    let request = InternalRequest {
        requested_model: "oauth-route".into(),
        system: vec![],
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::Text("hi".into())],
        }],
        tools: vec![],
        tool_choice: None,
        tool_choice_name: None,
        params: Default::default(),
        stream: false,
        include_usage: false,
        thinking: None,
        extra: Default::default(),
        raw_body: Some(
            json!({
                "model": "oauth-route",
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ),
    };

    Harness {
        state,
        pool,
        account_id,
        fallback_account_id,
        request,
        upstream,
        server,
        root,
    }
}

async fn consume(response: Response) {
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
}

async fn cleanup(harness: Harness) {
    harness.server.abort();
    let _ = std::fs::remove_dir_all(harness.root);
}

#[tokio::test]
async fn plugin_auth_error_rotates_and_retries_same_account_before_disable() {
    let strategy = Arc::new(TestCredential::new(RotationMode::Success { delay_ms: 0 }));
    let harness = setup(strategy.clone(), None, 1).await;

    let response = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retry".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("rotated credential should succeed on the same account");
    consume(response).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "healthy");
    assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness.upstream.attempts.lock().await.as_slice(),
        ["Bearer stale-token", "Bearer fresh-token"]
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn retryable_rotation_failure_cools_down_and_falls_back_without_disabling() {
    let strategy = Arc::new(TestCredential::new(RotationMode::RetryableFailure));
    let harness = setup(strategy.clone(), None, 2).await;

    let response = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retryable_rotation_failure".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("temporary refresh failure should fall back to the sibling account");
    consume(response).await;

    let second = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retryable_rotation_failure_immediate".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("immediate next request should skip the cooled account");
    consume(second).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "cooldown");
    assert_ne!(account.status, "disabled");
    let cooldown_until = account
        .cooldown_until
        .as_deref()
        .and_then(db::parse_dt)
        .expect("retryable rotation failure should set a cooldown");
    let remaining = (cooldown_until - chrono::Utc::now()).num_seconds();
    assert!(
        (1..=5).contains(&remaining),
        "plugin retry-after should drive the cooldown, got {remaining}s"
    );

    let fallback = db::get_account(&harness.pool, &harness.fallback_account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.status, "healthy");
    assert_eq!(
        strategy.rotations.load(Ordering::Relaxed),
        1,
        "cooled account must not refresh again on the immediate next request"
    );
    assert_eq!(
        harness.upstream.attempts.lock().await.as_slice(),
        [
            "Bearer stale-token",
            "Bearer fallback-token",
            "Bearer fallback-token",
        ]
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn terminal_expired_rotation_disables_account_then_falls_back() {
    let strategy = Arc::new(TestCredential::new(RotationMode::TerminalExpired));
    let harness = setup(strategy.clone(), None, 2).await;

    let response = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_terminal_rotation_failure".into(),
        true,
        None,
        vec![],
    )
    .await
    .expect("terminal credential failure should fall back to the sibling account");
    consume(response).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "disabled");

    let fallback = db::get_account(&harness.pool, &harness.fallback_account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.status, "healthy");
    assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness.upstream.attempts.lock().await.as_slice(),
        ["Bearer stale-token", "Bearer fallback-token"]
    );

    cleanup(harness).await;
}

#[tokio::test]
async fn concurrent_auth_failures_singleflight_forced_rotation_per_account() {
    let strategy = Arc::new(TestCredential::new(RotationMode::Success { delay_ms: 50 }));
    let barrier = Arc::new(Barrier::new(2));
    let harness = setup(strategy.clone(), Some(barrier), 1).await;

    let first = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retry_concurrent_1".into(),
        true,
        None,
        vec![],
    );
    let second = pipeline::run(
        &harness.state,
        FrontendFormat::OpenAi,
        None,
        harness.request.clone(),
        "req_plugin_auth_retry_concurrent_2".into(),
        true,
        None,
        vec![],
    );

    let (first, second) = tokio::join!(first, second);
    consume(first.expect("first request should recover")).await;
    consume(second.expect("second request should share the rotated credential")).await;

    let account = db::get_account(&harness.pool, &harness.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.status, "healthy");
    assert_eq!(
        strategy.rotations.load(Ordering::Relaxed),
        1,
        "concurrent 401s must perform one forced rotation"
    );

    let attempts = harness.upstream.attempts.lock().await;
    assert_eq!(
        attempts
            .iter()
            .filter(|value| value.as_str() == "Bearer stale-token")
            .count(),
        2
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|value| value.as_str() == "Bearer fresh-token")
            .count(),
        2
    );
    drop(attempts);

    cleanup(harness).await;
}
