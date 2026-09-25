//! Shared application state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

use crate::adapters::AdapterRegistry;
use crate::config::Config;
use crate::credentials::{CredentialRotationError, StaticKeyStrategy};
use crate::crypto::Crypto;
use crate::db::Pool;
use crate::logqueue::UsageLogQueue;
use crate::registry::Registry;
use crate::trace::FlightRecorder;

#[derive(Clone)]
pub struct AppState {
    /// Live in-flight request view (FR-8.3), bounded and metadata-only.
    pub live: crate::live::LiveRequests,
    pub config: Arc<Config>,
    pub pool: Pool,
    pub registry: Arc<Registry>,
    pub crypto: Arc<Crypto>,
    pub credentials: Arc<StaticKeyStrategy>,
    /// Per-plugin credential strategies, keyed by plugin id (§6.0). When a
    /// provider binds `credential_plugin`, the pipeline resolves through here
    /// before falling back to the static strategy.
    pub plugin_credentials:
        Arc<dashmap::DashMap<String, Arc<dyn crate::credentials::CredentialStrategy>>>,
    /// Account-scoped forced-rotation singleflight gates. The key set is
    /// bounded by configured plugin-backed accounts.
    credential_rotation_gates: Arc<DashMap<String, Arc<CredentialRotationGate>>>,
    pub adapters: AdapterRegistry,
    pub http: reqwest::Client,
    /// Pinned provider clients keyed by validated host/address set. This keeps
    /// connection pooling without reopening a DNS rebinding window.
    pub outbound_clients: Arc<dashmap::DashMap<String, reqwest::Client>>,
    pub log_queue: UsageLogQueue,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Diagnostic flight recorder (FR-13).
    pub flight: Arc<FlightRecorder>,
    /// Session id -> last successful route target. Used for optional
    /// sticky/cache affinity and FR-2.11 opaque-state provenance. TTL-bounded.
    sticky: Arc<DashMap<String, StickyEntry>>,
    rr_counters: Arc<DashMap<String, Arc<AtomicU64>>>,
    /// Before-commit / after-commit failure counters (FR-4.9).
    pub failures_pre_commit: Arc<AtomicU64>,
    pub failures_post_commit: Arc<AtomicU64>,
    pub cancellations: Arc<AtomicU64>,
    pub cancellation_latency_ms_total: Arc<AtomicU64>,
    /// Requests rejected because the provider was at its RPM/TPM or the key hit
    /// a budget (dashboard counters).
    pub total_requests: Arc<AtomicU64>,
    /// Route targets skipped during eligibility filtering (FR-12.11, NFR-4.2).
    pub route_skips: Arc<AtomicU64>,
    /// Requests that used at least one fallback hop (FR-12.6, NFR-4.2).
    pub route_fallbacks: Arc<AtomicU64>,
    /// Last successful scheduled backup timestamp (RFC3339), for backup-failure
    /// alerting (Monitoring). None until a backup has run.
    pub last_backup_at: Arc<parking_lot::Mutex<Option<String>>>,
    /// Whether the last scheduled backup attempt failed.
    pub last_backup_failed: Arc<std::sync::atomic::AtomicBool>,
    /// Atomic per-key RPM/TPM/budget admission state.
    pub admission: crate::admission::AdmissionController,
    /// Per-IP abuse limiter (NFR-3.6), applied before virtual-key auth.
    pub ip_limiter: crate::ratelimit::IpLimiter,
    /// In-memory admin sessions (dropped on restart; TTL-bounded).
    pub sessions: Arc<crate::auth::Sessions>,
    /// One-time browser sessions for plugin-provided account authorization.
    pub plugin_auth_sessions: Arc<crate::auth::PluginAuthSessions>,
    /// Post-v1 plugin host (docs/KINETIX-PLUGIN-ARCHITECTURE.md).
    pub plugins: Option<Arc<crate::plugins::PluginManager>>,
    /// Bounded fire-and-forget queue for plugin hook side effects (§6.6). Hooks
    /// run off the request path; if the queue is full a hook is dropped rather
    /// than delaying a client request.
    hook_tx: tokio::sync::mpsc::Sender<HookJob>,
}

struct CredentialRotationGate {
    lock: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    last_result: parking_lot::Mutex<Option<std::result::Result<bool, CredentialRotationError>>>,
}

impl CredentialRotationGate {
    fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            last_result: parking_lot::Mutex::new(None),
        }
    }
}

#[derive(Clone)]
pub struct StickyEntry {
    /// The selected route target key (route id + account id + model id).
    pub target_key: String,
    pub at: std::time::Instant,
}

/// A deferred, side-effect-only plugin hook invocation (§6.6).
type HookJob =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

const HOOK_QUEUE_CAPACITY: usize = 1024;
const MAX_CONCURRENT_HOOK_JOBS: usize = 32;

fn spawn_hook_worker(mut hook_rx: tokio::sync::mpsc::Receiver<HookJob>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HOOK_JOBS));
    tokio::spawn(async move {
        while let Some(job) = hook_rx.recv().await {
            let permit = slots
                .clone()
                .acquire_owned()
                .await
                .expect("hook dispatcher semaphore is never closed");
            tokio::spawn(async move {
                let _permit = permit;
                job().await;
            });
        }
    });
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        pool: Pool,
        registry: Arc<Registry>,
        crypto: Arc<Crypto>,
        http: reqwest::Client,
        log_queue: UsageLogQueue,
        config_ip_limit: u64,
    ) -> Self {
        let credentials = Arc::new(StaticKeyStrategy::new(crypto.clone()));
        let sessions = Arc::new(crate::auth::Sessions::new(config.session_ttl_minutes));
        let plugin_auth_sessions = Arc::new(crate::auth::PluginAuthSessions::new());
        let (hook_tx, hook_rx) = tokio::sync::mpsc::channel::<HookJob>(HOOK_QUEUE_CAPACITY);
        spawn_hook_worker(hook_rx);
        AppState {
            config,
            pool,
            registry,
            crypto,
            credentials,
            plugin_credentials: Arc::new(DashMap::new()),
            credential_rotation_gates: Arc::new(DashMap::new()),
            adapters: AdapterRegistry::new(),
            http,
            outbound_clients: Arc::new(DashMap::new()),
            log_queue,
            started_at: chrono::Utc::now(),
            flight: Arc::new(FlightRecorder::new(512, 128)),
            live: crate::live::LiveRequests::new(512),
            sticky: Arc::new(DashMap::new()),
            rr_counters: Arc::new(DashMap::new()),
            failures_pre_commit: Arc::new(AtomicU64::new(0)),
            failures_post_commit: Arc::new(AtomicU64::new(0)),
            cancellations: Arc::new(AtomicU64::new(0)),
            cancellation_latency_ms_total: Arc::new(AtomicU64::new(0)),
            total_requests: Arc::new(AtomicU64::new(0)),
            route_skips: Arc::new(AtomicU64::new(0)),
            route_fallbacks: Arc::new(AtomicU64::new(0)),
            last_backup_at: Arc::new(parking_lot::Mutex::new(None)),
            last_backup_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            admission: crate::admission::AdmissionController::default(),
            ip_limiter: crate::ratelimit::IpLimiter::new(config_ip_limit),
            sessions,
            plugin_auth_sessions,
            plugins: None,
            hook_tx,
        }
    }

    /// Attach the plugin host (called during server startup).
    pub fn with_plugins(mut self, plugins: Arc<crate::plugins::PluginManager>) -> Self {
        self.plugins = Some(plugins);
        self
    }

    /// Enqueue a plugin hook side effect (§6.6). Never blocks: if the queue is
    /// saturated the hook is dropped, because a hook must not affect the client
    /// request.
    pub fn spawn_hook<F, Fut>(&self, make: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let job: HookJob = Box::new(move || Box::pin(make()));
        if self.hook_tx.try_send(job).is_err() {
            tracing::debug!("plugin hook queue saturated; dropping hook");
        }
    }

    /// The plugin manager, when the host is enabled.
    pub fn plugin_manager(&self) -> Option<&Arc<crate::plugins::PluginManager>> {
        self.plugins.as_ref()
    }

    /// Register the built-in capability implementations a plugin host provides
    /// (§6.0): credential strategies and wire-format adapters, keyed by plugin
    /// id / namespaced reference. Called once at startup, before the state is
    /// shared, so every capability resolution fails closed until a plugin is
    /// both installed and enabled.
    pub fn register_plugin_capabilities(&self) {
        // Credential strategies and adapters are registered lazily by the
        // enable path (`register_plugin_credential_strategy` /
        // `register_plugin_adapter`); this hook exists so callers have one
        // obvious place to do so and so the intent is documented.
    }

    /// Register a plugin-backed credential strategy for a plugin id (§6.1).
    pub fn register_plugin_credential_strategy(
        &self,
        plugin_id: impl Into<String>,
        strategy: Arc<dyn crate::credentials::CredentialStrategy>,
    ) {
        self.plugin_credentials.insert(plugin_id.into(), strategy);
    }

    /// Register a plugin-backed outbound adapter under its namespaced
    /// reference `plugin:<id>/<cap>` (§6.0).
    pub fn register_plugin_adapter(
        &self,
        reference: impl Into<String>,
        adapter: Arc<dyn crate::adapters::Adapter>,
    ) {
        self.adapters.register_plugin(reference, adapter);
    }

    /// Drop every capability a plugin registered (credential strategy and
    /// adapters). Called when a plugin is disabled, removed, rolled back, or
    /// has a permission revoked, so a disabled plugin cannot keep resolving
    /// credentials or serving adapters.
    pub fn unregister_plugin_capabilities(&self, plugin_id: &str) {
        self.plugin_credentials.remove(plugin_id);
        self.adapters.unregister_plugin(plugin_id);
    }

    /// Resolve the credential for an account, honouring a provider's plugin
    /// credential binding (§6.0). A bound-but-unavailable plugin fails closed.
    pub async fn credential_for(
        &self,
        provider: &crate::db::ProviderRow,
        account: &crate::db::AccountRow,
    ) -> anyhow::Result<crate::credentials::ResolvedCredential> {
        if let Some(r) = provider.credential_plugin_ref() {
            let Some(strategy) = self.plugin_credentials.get(&r.plugin_id) else {
                anyhow::bail!(
                    "provider '{}' is bound to unavailable plugin credential strategy '{}'",
                    provider.name,
                    r.to_string_ref()
                );
            };
            return strategy.resolve(account).await;
        }
        self.credentials.resolve(account).await
    }

    /// Force renewal for a plugin-backed credential after an upstream auth
    /// failure. Calls are serialized per account. A waiter re-resolves under
    /// the lock and skips a second rotation when another request already
    /// replaced the credential that failed upstream.
    pub async fn rotate_credential_after_auth_error(
        &self,
        provider: &crate::db::ProviderRow,
        account: &crate::db::AccountRow,
        failed_secret: &str,
    ) -> std::result::Result<bool, CredentialRotationError> {
        let Some(r) = provider.credential_plugin_ref() else {
            return Ok(false);
        };
        let strategy = self
            .plugin_credentials
            .get(&r.plugin_id)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or_else(|| {
                CredentialRotationError::new(
                    "plugin_internal",
                    format!(
                        "provider '{}' is bound to unavailable plugin credential strategy '{}'",
                        provider.name,
                        r.to_string_ref()
                    ),
                    true,
                    None,
                )
            })?;

        let gate_key = format!("{}:{}", provider.id, account.id);
        let gate = self
            .credential_rotation_gates
            .entry(gate_key)
            .or_insert_with(|| Arc::new(CredentialRotationGate::new()))
            .clone();

        // Capture the generation before waiting. If it changes while this
        // request is queued, the in-flight caller already completed the
        // singleflight operation and we reuse that exact result.
        let observed_generation = gate.generation.load(Ordering::Acquire);
        let _guard = gate.lock.lock().await;
        if gate.generation.load(Ordering::Acquire) != observed_generation {
            if let Some(result) = gate.last_result.lock().clone() {
                return result;
            }
        }

        let result = match strategy.resolve(account).await {
            Ok(current) if current.secret != failed_secret => Ok(true),
            _ => strategy.rotate(account).await.map(|()| true),
        };

        *gate.last_result.lock() = Some(result.clone());
        gate.generation.fetch_add(1, Ordering::Release);
        result
    }

    /// Count a route target skipped during eligibility filtering.
    pub fn record_skip(&self) {
        self.route_skips.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request that required at least one fallback hop.
    pub fn record_fallback(&self) {
        self.route_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    /// Round-robin counter for a route.
    pub fn rr_counter(&self, route_id: &str) -> Arc<AtomicU64> {
        self.rr_counters
            .entry(route_id.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone()
    }

    /// Remember the last successful target for affinity and state provenance.
    pub fn sticky_remember(&self, session: &str, target_key: String) {
        self.sticky.insert(
            session.to_string(),
            StickyEntry {
                target_key,
                at: std::time::Instant::now(),
            },
        );
    }

    /// Look up a session's previously-selected target, if still fresh.
    pub fn sticky_lookup(&self, session: &str, ttl: std::time::Duration) -> Option<String> {
        self.sticky.get(session).and_then(|e| {
            if e.at.elapsed() <= ttl {
                Some(e.target_key.clone())
            } else {
                None
            }
        })
    }

    /// Drop sticky entries older than `ttl` (bounded memory).
    pub fn sticky_sweep(&self, ttl: std::time::Duration) {
        self.sticky.retain(|_, e| e.at.elapsed() <= ttl);
    }

    pub fn uptime_secs(&self) -> i64 {
        (chrono::Utc::now() - self.started_at).num_seconds()
    }

    pub fn bump_counter(&self, _k: &str) {
        let _ = Ordering::Relaxed;
    }
}

#[cfg(test)]
mod hook_dispatch_tests {
    use super::*;

    #[tokio::test]
    async fn blocked_hook_job_does_not_block_next_job() {
        let (tx, rx) = tokio::sync::mpsc::channel::<HookJob>(HOOK_QUEUE_CAPACITY);
        spawn_hook_worker(rx);

        let gate = Arc::new(tokio::sync::Notify::new());
        let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
        let first_gate = gate.clone();
        let first: HookJob = Box::new(move || {
            Box::pin(async move {
                let _ = first_started_tx.send(());
                first_gate.notified().await;
            })
        });
        tx.send(first).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), first_started_rx)
            .await
            .expect("first hook should start")
            .unwrap();

        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
        let second: HookJob = Box::new(move || {
            Box::pin(async move {
                let _ = second_started_tx.send(());
            })
        });
        tx.send(second).await.unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(100), second_started_rx)
            .await
            .expect("second hook should start while first hook is blocked")
            .unwrap();

        gate.notify_waiters();
    }
}
