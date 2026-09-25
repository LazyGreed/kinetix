//! Host-owned opaque provider continuation-state subsystem.
//!
//! Some upstream providers hand back an opaque continuation token alongside a
//! tool/function call that must be replayed verbatim on the next turn (e.g.
//! Gemini's `thoughtSignature`). Kinetix's canonical `Part::ToolCall.signature`
//! and `StreamEvent::ToolCallStart.signature` slots already round-trip this
//! value *inside a single request/response pair*, but a translated client
//! protocol (OpenAI Chat Completions, OpenAI Responses, Anthropic Messages)
//! has no field guaranteed to carry it back on the client's next request.
//!
//! This module is the persistence/recovery layer that closes that gap. It:
//!
//!   * stores the exact opaque value under the **client-visible** tool-call id
//!     (after `ToolStreamState` normalization), scoped by client identity so
//!     one virtual key can never resolve another's continuation state;
//!   * encrypts the value at rest under a dedicated derived cipher
//!     (`Crypto::encrypt_opaque_state` / `decrypt_opaque_state`);
//!   * enforces provider/family/producer/session/tool-name compatibility
//!     before ever handing a stored value back to a pipeline caller;
//!   * never invents, merges, or copies a signature between unrelated tool
//!     calls or provider families.
//!
//! Routing/portability *policy* (reject vs strip-with-warning) is decided by
//! `src/pipeline.rs`, not here. This module only answers "is there compatible
//! stored state for this tool call?" — it never chooses whether to use it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::mpsc;

use crate::crypto::Crypto;
use crate::db::Pool;

// ---------------------------------------------------------------------------
// Kind / target / lookup / record types (internal data-plane state only; none
// of this is exposed through the public HTTP/admin API).
// ---------------------------------------------------------------------------

/// The provider protocol family an opaque continuation value belongs to.
/// Only one variant exists today; the enum exists so future opaque-state
/// kinds (should another provider need one) do not need a schema migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpaqueStateKind {
    GeminiThoughtSignature,
}

impl OpaqueStateKind {
    fn as_str(&self) -> &'static str {
        match self {
            OpaqueStateKind::GeminiThoughtSignature => "gemini_thought_signature",
        }
    }
}

/// What an adapter/target is prepared to produce and accept opaque state as.
/// Compatibility between a stored record and a candidate target requires an
/// exact match on `provider_id`, `family`, `producer`, and the originating
/// model id (§6, §41).
///
/// The model id is part of the key deliberately. Google's `generateContent`
/// contract only guarantees a thought signature is accepted by the model that
/// produced it; switching models is documented to require dummy signatures,
/// not reuse of the original one. Cross-model restoration is therefore not
/// assumed until it has real acceptance evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueStateTarget {
    pub kind: OpaqueStateKind,
    pub provider_id: String,
    /// The opaque-state protocol family (e.g. `"gemini"`); a coarse
    /// provenance tag retained for diagnostics and future protocol variants.
    /// It is not sufficient on its own to authorize restoration.
    pub family: String,
    /// Adapter encoding-version provenance, e.g. `"native:gemini:v1"` or
    /// `"plugin:<plugin-id>@<version>/<capability>"` (§41). A version bump
    /// intentionally invalidates old rows rather than risk misinterpreting
    /// them.
    pub producer: String,
    /// Exact upstream model id that produced the state (e.g.
    /// `"gemini-2.5-flash"`). State captured on one model is never restored
    /// onto a different model.
    pub model_id: String,
}

/// A resolved client scope: identifies *who* is allowed to read back opaque
/// state. Ordinary API traffic scopes by virtual-key id; admin/internal
/// testing without a virtual key uses the fixed `"internal"` scope (§9).
#[derive(Debug, Clone)]
pub struct OpaqueClientScope(String);

impl OpaqueClientScope {
    pub fn for_key(key_id: &str) -> Self {
        OpaqueClientScope(key_id.to_string())
    }

    pub fn internal() -> Self {
        OpaqueClientScope("internal".to_string())
    }

    fn hash(&self) -> String {
        sha256_hex(self.0.as_bytes())
    }
}

/// Outcome of a stored-state lookup. Never returns the underlying DB record —
/// only the information the pipeline needs to hydrate or reject (§38).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpaqueLookupResult {
    /// No stored state exists for this tool-call id (or it expired).
    Missing,
    /// Compatible stored state exists; here is the exact opaque value.
    Compatible(String),
    /// Stored state exists but targets a different provider/family/producer.
    Incompatible,
    /// Stored state exists but was captured under a different, conflicting
    /// session identity (§10).
    SessionMismatch,
    /// Stored state exists but was captured for a different tool name (§20).
    ToolNameMismatch,
    /// Stored state exists but could not be decrypted (crypto/key mismatch,
    /// or corrupt row); treated as unusable, never surfaced to the client
    /// verbatim (§19 case D).
    Unavailable,
}

// ---------------------------------------------------------------------------
// Hashing helpers (§8, §9): raw client tool-call ids and raw session values
// are never persisted, only SHA-256 hashes scoped by client identity.
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn tool_call_hash(scope: &OpaqueClientScope, tool_call_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(scope.0.as_bytes());
    hasher.update(b"\0");
    hasher.update(tool_call_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn session_hash(session: &str) -> String {
    sha256_hex(session.as_bytes())
}

fn tool_name_hash(tool_name: &str) -> String {
    sha256_hex(tool_name.as_bytes())
}

// ---------------------------------------------------------------------------
// Retention constants (§12).
// ---------------------------------------------------------------------------

const MEMORY_TTL: Duration = Duration::from_secs(60 * 60);
const PERSISTENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_MEMORY_ROWS: usize = 2_048;
const MAX_DB_ROWS: i64 = 10_000;
const PRUNE_EVERY_N_WRITES: u64 = 100;
/// Bound on queued SQLite durability jobs. The RAM cache is the synchronous
/// hot path; when this queue saturates, writes are dropped and counted rather
/// than blocking the streaming response (§13, §63).
const DURABILITY_QUEUE_CAPACITY: usize = 4_096;

// ---------------------------------------------------------------------------
// In-memory hot-path cache (§13, §40).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OpaqueCacheKey {
    kind: &'static str,
    scope_hash: String,
    tool_call_hash: String,
    provider_id: String,
    family: String,
    producer: String,
    model_id: String,
}

#[derive(Debug, Clone)]
struct CachedOpaqueState {
    signature: String,
    session_hash: Option<String>,
    tool_name_hash: String,
    expires_at: Instant,
}

// ---------------------------------------------------------------------------
// Observability counters (§36). No secrets, ids, or tool names — coarse
// outcome counts only.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Counters {
    capture_stored: std::sync::atomic::AtomicU64,
    capture_replaced: std::sync::atomic::AtomicU64,
    capture_dropped: std::sync::atomic::AtomicU64,
    capture_storage_error: std::sync::atomic::AtomicU64,
    lookup_hit: std::sync::atomic::AtomicU64,
    lookup_miss: std::sync::atomic::AtomicU64,
    lookup_expired: std::sync::atomic::AtomicU64,
    lookup_incompatible: std::sync::atomic::AtomicU64,
    lookup_session_mismatch: std::sync::atomic::AtomicU64,
    lookup_tool_name_mismatch: std::sync::atomic::AtomicU64,
    lookup_decrypt_error: std::sync::atomic::AtomicU64,
}

/// A point-in-time snapshot of opaque-state counters, safe to render on the
/// admin metrics surface (no secrets, no ids).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct OpaqueStateMetrics {
    pub capture_stored: u64,
    pub capture_replaced: u64,
    pub capture_dropped: u64,
    pub capture_storage_error: u64,
    pub lookup_hit: u64,
    pub lookup_miss: u64,
    pub lookup_expired: u64,
    pub lookup_incompatible: u64,
    pub lookup_session_mismatch: u64,
    pub lookup_tool_name_mismatch: u64,
    pub lookup_decrypt_error: u64,
    pub entries: u64,
}

// ---------------------------------------------------------------------------
// The store.
// ---------------------------------------------------------------------------

pub struct OpaqueStateStore {
    pool: Pool,
    crypto: Arc<Crypto>,
    cache: DashMap<OpaqueCacheKey, CachedOpaqueState>,
    /// Insertion order for bounded LRU-ish eviction (oldest first). Kept
    /// separate from the DashMap so eviction never needs to scan.
    order: Mutex<VecDeque<OpaqueCacheKey>>,
    counters: Arc<Counters>,
    durability: mpsc::Sender<DurabilityJob>,
    /// Bound on RAM-cache rows. A field (not just the const) so the eviction
    /// regression can exercise capacity without filling thousands of entries.
    max_memory_rows: usize,
}

/// A queued SQLite durability job. Encryption and the SQLite UPSERT happen on
/// the worker task, never inline in a streaming response, so a slow or locked
/// database cannot stall a tool-call event before the client receives it.
enum DurabilityJob {
    Write(Box<DurabilityWrite>),
    /// Ordering barrier: mpsc preserves order, so once the worker receives
    /// this it has processed every previously queued write. Used by tests and
    /// graceful shutdown, never on the request path.
    Flush(tokio::sync::oneshot::Sender<()>),
}

struct DurabilityWrite {
    kind: &'static str,
    scope_hash: String,
    tool_call_hash: String,
    session_hash: String,
    provider_id: String,
    family: String,
    producer: String,
    origin_model: String,
    tool_name_hash: String,
    signature: String,
}

impl OpaqueStateStore {
    pub fn new(pool: Pool, crypto: Arc<Crypto>) -> Self {
        let (tx, rx) = mpsc::channel::<DurabilityJob>(DURABILITY_QUEUE_CAPACITY);
        let counters = Arc::new(Counters::default());
        spawn_durability_worker(pool.clone(), crypto.clone(), rx, counters.clone());
        Self {
            pool,
            crypto,
            cache: DashMap::new(),
            order: Mutex::new(VecDeque::new()),
            counters,
            durability: tx,
            max_memory_rows: MAX_MEMORY_ROWS,
        }
    }

    #[cfg(test)]
    fn set_max_memory_rows_for_test(&mut self, rows: usize) {
        self.max_memory_rows = rows;
    }

    /// Wait until every queued durability write has been processed. Used by
    /// tests and graceful shutdown; never called on the request path.
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.durability.send(DurabilityJob::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    pub fn metrics(&self) -> OpaqueStateMetrics {
        use std::sync::atomic::Ordering::Relaxed;
        OpaqueStateMetrics {
            capture_stored: self.counters.capture_stored.load(Relaxed),
            capture_replaced: self.counters.capture_replaced.load(Relaxed),
            capture_dropped: self.counters.capture_dropped.load(Relaxed),
            capture_storage_error: self.counters.capture_storage_error.load(Relaxed),
            lookup_hit: self.counters.lookup_hit.load(Relaxed),
            lookup_miss: self.counters.lookup_miss.load(Relaxed),
            lookup_expired: self.counters.lookup_expired.load(Relaxed),
            lookup_incompatible: self.counters.lookup_incompatible.load(Relaxed),
            lookup_session_mismatch: self.counters.lookup_session_mismatch.load(Relaxed),
            lookup_tool_name_mismatch: self.counters.lookup_tool_name_mismatch.load(Relaxed),
            lookup_decrypt_error: self.counters.lookup_decrypt_error.load(Relaxed),
            entries: self.cache.len() as u64,
        }
    }

    /// Record an opaque continuation value under the client-visible tool-call
    /// id. The RAM cache is written synchronously so an immediate next request
    /// never races ahead of it (§39); the SQLite UPSERT/pruning is handed to a
    /// bounded background worker so a slow or locked database cannot stall the
    /// streaming response that carries the tool call (§13, §63). A saturated
    /// queue drops the durability write (counted) rather than blocking the
    /// data plane; a storage failure is likewise swallowed and counted.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_tool_signature(
        &self,
        scope: &OpaqueClientScope,
        target: &OpaqueStateTarget,
        session: Option<&str>,
        tool_call_id: &str,
        tool_name: &str,
        signature: &str,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        if signature.is_empty() || tool_call_id.is_empty() {
            return;
        }
        let scope_hash = scope.hash();
        let call_hash = tool_call_hash(scope, tool_call_id);
        let sess_hash = session.map(session_hash);
        let name_hash = tool_name_hash(tool_name);

        let key = self.cache_key(target, &scope_hash, &call_hash);
        let now = Instant::now();
        let replaced = self
            .cache
            .get(&key)
            .map(|existing| existing.signature != signature)
            .unwrap_or(false);

        self.cache.insert(
            key.clone(),
            CachedOpaqueState {
                signature: signature.to_string(),
                session_hash: sess_hash.clone(),
                tool_name_hash: name_hash.clone(),
                expires_at: now + MEMORY_TTL,
            },
        );
        self.touch_order(key);

        if replaced {
            self.counters.capture_replaced.fetch_add(1, Relaxed);
            tracing::warn!(kind = target.kind.as_str(), "opaque_state_replaced");
        }

        let job = DurabilityWrite {
            kind: target.kind.as_str(),
            scope_hash,
            tool_call_hash: call_hash,
            session_hash: sess_hash.unwrap_or_default(),
            provider_id: target.provider_id.clone(),
            family: target.family.clone(),
            producer: target.producer.clone(),
            origin_model: target.model_id.clone(),
            tool_name_hash: name_hash,
            signature: signature.to_string(),
        };
        if self
            .durability
            .try_send(DurabilityJob::Write(Box::new(job)))
            .is_err()
        {
            self.counters.capture_dropped.fetch_add(1, Relaxed);
            tracing::warn!(
                kind = target.kind.as_str(),
                "opaque_state_durability_queue_full"
            );
        }
    }

    /// Resolve stored state for a tool call.
    ///
    /// `capability` is the candidate target's declared opaque-state capability
    /// (from `Adapter::opaque_state_target`), or `None` when the target cannot
    /// carry opaque continuation state at all. Passing `None` still performs a
    /// lookup so the pipeline can distinguish "no stored state" from "stored
    /// state exists but this target cannot carry it" (§19 case C, §22): a row
    /// found under a target with no capability is reported as `Incompatible`.
    ///
    /// RAM is checked first (only possible when the capability is known, since
    /// the cache key is scoped by provider/family/producer); a miss falls back
    /// to SQLite, decrypts, and repopulates RAM (§13). Never returns a raw DB
    /// record — only a caller-safe result (§38).
    pub async fn resolve_tool_signature(
        &self,
        scope: &OpaqueClientScope,
        capability: Option<&OpaqueStateTarget>,
        session: Option<&str>,
        tool_call_id: &str,
        tool_name: &str,
    ) -> OpaqueLookupResult {
        let scope_hash = scope.hash();
        let call_hash = tool_call_hash(scope, tool_call_id);

        if let Some(target) = capability {
            let key = self.cache_key(target, &scope_hash, &call_hash);
            if let Some(entry) = self.cache.get(&key) {
                if entry.expires_at < Instant::now() {
                    drop(entry);
                    self.cache.remove(&key);
                    self.counters
                        .lookup_expired
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return OpaqueLookupResult::Missing;
                }
                return self.evaluate_cached(&entry, session, tool_name);
            }
        }

        let rows = self.fetch_rows(&scope_hash, &call_hash).await;
        if rows.is_empty() {
            self.counters
                .lookup_miss
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::Missing;
        }

        // Prefer the row this target can actually carry (kind/provider/family/
        // producer/model all match). With no capability, the newest row is
        // inspected only so the pipeline can tell "stored but non-portable"
        // apart from "nothing stored" (§19 case C, §22).
        let candidate = match capability {
            Some(target) => rows.iter().find(|row| row.matches(target)),
            None => rows.first(),
        };
        let Some(row) = candidate else {
            self.counters
                .lookup_incompatible
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::Incompatible;
        };

        // Tool-name and session identity are checked before provider
        // compatibility so a reused tool-call id is caught even when the
        // target happens to differ (§10, §20).
        if row.tool_name_hash != tool_name_hash(tool_name) {
            self.counters
                .lookup_tool_name_mismatch
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::ToolNameMismatch;
        }
        if let (Some(stored), Some(incoming)) = (&row.session_hash, session) {
            if *stored != session_hash(incoming) {
                self.counters
                    .lookup_session_mismatch
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return OpaqueLookupResult::SessionMismatch;
            }
        }

        let Some(target) = capability else {
            self.counters
                .lookup_incompatible
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::Incompatible;
        };

        let signature = match self.crypto.decrypt_opaque_state(&row.value_enc) {
            Ok(sig) => sig,
            Err(_) => {
                self.counters
                    .lookup_decrypt_error
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Never surface crypto detail to the client; treat as
                // unavailable local state (§19 case D, §63).
                tracing::error!("opaque_state_decrypt_failed");
                return OpaqueLookupResult::Unavailable;
            }
        };

        // Repopulate the RAM hot path with the decrypted value.
        let key = self.cache_key(target, &scope_hash, &call_hash);
        let now = Instant::now();
        self.cache.insert(
            key.clone(),
            CachedOpaqueState {
                signature: signature.clone(),
                session_hash: row.session_hash.clone(),
                tool_name_hash: row.tool_name_hash.clone(),
                expires_at: now + MEMORY_TTL,
            },
        );
        self.touch_order(key);

        self.counters
            .lookup_hit
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpaqueLookupResult::Compatible(signature)
    }

    fn cache_key(
        &self,
        target: &OpaqueStateTarget,
        scope_hash: &str,
        call_hash: &str,
    ) -> OpaqueCacheKey {
        OpaqueCacheKey {
            kind: target.kind.as_str(),
            scope_hash: scope_hash.to_string(),
            tool_call_hash: call_hash.to_string(),
            provider_id: target.provider_id.clone(),
            family: target.family.clone(),
            producer: target.producer.clone(),
            model_id: target.model_id.clone(),
        }
    }

    fn touch_order(&self, key: OpaqueCacheKey) {
        let mut order = self.order.lock();
        // Move-to-back: a re-captured key must not leave a stale occurrence at
        // the front. Otherwise, once the deque is at capacity, evicting that
        // stale occurrence would `cache.remove()` the *newly written* value and
        // silently drop a live signature (an immediate continuation would then
        // miss RAM). `retain` keeps `order` free of duplicates so one cache
        // entry always has exactly one eviction slot.
        order.retain(|existing| existing != &key);
        order.push_back(key);
        while order.len() > self.max_memory_rows {
            if let Some(evicted) = order.pop_front() {
                self.cache.remove(&evicted);
            }
        }
    }

    fn evaluate_cached(
        &self,
        entry: &CachedOpaqueState,
        session: Option<&str>,
        tool_name: &str,
    ) -> OpaqueLookupResult {
        // Tool-name validation (§20): a reused tool-call id bound to a
        // different tool name is malformed/reused conversation history.
        if entry.tool_name_hash != tool_name_hash(tool_name) {
            self.counters
                .lookup_tool_name_mismatch
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return OpaqueLookupResult::ToolNameMismatch;
        }

        // Session compatibility (§10): both present and different => refuse.
        if let (Some(stored), Some(incoming)) = (&entry.session_hash, session) {
            if *stored != session_hash(incoming) {
                self.counters
                    .lookup_session_mismatch
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return OpaqueLookupResult::SessionMismatch;
            }
        }

        self.counters
            .lookup_hit
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpaqueLookupResult::Compatible(entry.signature.clone())
    }

    /// Fetch every non-expired row for a tool-call id, newest first. The query
    /// is scoped by `scope_hash` + `tool_call_hash` only (not by provider/
    /// family/producer/model) so a target with no opaque-state capability can
    /// still observe that *some* non-portable continuation state exists for
    /// this tool call; the caller selects the compatible row.
    async fn fetch_rows(&self, scope_hash: &str, call_hash: &str) -> Vec<StoredRow> {
        let now = chrono::Utc::now().to_rfc3339();
        let rows = sqlx::query(
            "SELECT kind, provider_id, family, producer, session_hash, tool_name_hash,
                    origin_model, value_enc, expires_at
             FROM opaque_provider_state
             WHERE scope_hash = ? AND tool_call_hash = ?
             ORDER BY created_at DESC",
        )
        .bind(scope_hash)
        .bind(call_hash)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .filter(|row| {
                let expires_at: String = row.get("expires_at");
                expires_at.as_str() >= now.as_str()
            })
            .map(|row| {
                let session_hash: String = row.get("session_hash");
                StoredRow {
                    kind: row.get("kind"),
                    provider_id: row.get("provider_id"),
                    family: row.get("family"),
                    producer: row.get("producer"),
                    session_hash: if session_hash.is_empty() {
                        None
                    } else {
                        Some(session_hash)
                    },
                    tool_name_hash: row.get("tool_name_hash"),
                    origin_model: row.get("origin_model"),
                    value_enc: row.get("value_enc"),
                }
            })
            .collect()
    }

    /// Delete expired rows, and if still over the row cap, delete the oldest
    /// rows above the limit (§12). Called lazily by the durability worker every
    /// `PRUNE_EVERY_N_WRITES` writes; kept public so tests can invoke it.
    pub async fn prune(&self) {
        prune_pool(&self.pool).await;
    }

    /// Row count, exposed for tests/diagnostics only.
    pub async fn count_rows(&self) -> i64 {
        sqlx::query("SELECT COUNT(*) as n FROM opaque_provider_state")
            .fetch_one(&self.pool)
            .await
            .map(|row| row.get::<i64, _>("n"))
            .unwrap_or(0)
    }

    /// Drop the RAM cache only, leaving SQLite intact. Used by restart-
    /// durability tests to prove the SQLite fallback path works without a
    /// full process restart (§52).
    #[cfg(test)]
    pub fn clear_memory_cache_for_test(&self) {
        self.cache.clear();
        self.order.lock().clear();
    }

    #[cfg(test)]
    pub fn memory_entry_count_for_test(&self) -> usize {
        self.cache.len()
    }

    /// Test-only: read a value from the RAM cache only, never SQLite. Used by
    /// the eviction regression to observe the hot path deterministically
    /// (a SQLite read could race the asynchronous durability worker).
    #[cfg(test)]
    fn cached_signature_for_test(
        &self,
        scope: &OpaqueClientScope,
        target: &OpaqueStateTarget,
        tool_call_id: &str,
    ) -> Option<String> {
        let key = self.cache_key(target, &scope.hash(), &tool_call_hash(scope, tool_call_id));
        self.cache.get(&key).map(|entry| entry.signature.clone())
    }

    /// Test-only: mark every stored row expired and drop the RAM cache so the
    /// next lookup exercises the expiry path deterministically.
    #[cfg(test)]
    pub async fn expire_all_for_test(&self) {
        // Flush first so the expiry UPDATE lands on the rows that were just
        // queued; otherwise the worker would persist them with a future
        // expiry after this returns.
        self.flush().await;
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let _ = sqlx::query("UPDATE opaque_provider_state SET expires_at = ?")
            .bind(past)
            .execute(&self.pool)
            .await;
        self.clear_memory_cache_for_test();
    }
}

/// A decrypted-adjacent row as read from SQLite (value still encrypted).
struct StoredRow {
    kind: String,
    provider_id: String,
    family: String,
    producer: String,
    session_hash: Option<String>,
    tool_name_hash: String,
    origin_model: String,
    value_enc: String,
}

impl StoredRow {
    fn matches(&self, target: &OpaqueStateTarget) -> bool {
        self.kind == target.kind.as_str()
            && self.provider_id == target.provider_id
            && self.family == target.family
            && self.producer == target.producer
            && self.origin_model == target.model_id
    }
}

// ---------------------------------------------------------------------------
// Durability worker: encryption + SQLite UPSERT + periodic pruning, entirely
// off the data plane.
// ---------------------------------------------------------------------------

fn spawn_durability_worker(
    pool: Pool,
    crypto: Arc<Crypto>,
    mut rx: mpsc::Receiver<DurabilityJob>,
    counters: Arc<Counters>,
) {
    tokio::spawn(async move {
        let mut writes: u64 = 0;
        while let Some(job) = rx.recv().await {
            match job {
                DurabilityJob::Write(job) => {
                    persist_opaque_state(&pool, &crypto, &counters, &job).await;
                    writes += 1;
                    if writes.is_multiple_of(PRUNE_EVERY_N_WRITES) {
                        prune_pool(&pool).await;
                    }
                }
                DurabilityJob::Flush(reply) => {
                    let _ = reply.send(());
                }
            }
        }
    });
}

async fn persist_opaque_state(
    pool: &Pool,
    crypto: &Crypto,
    counters: &Counters,
    job: &DurabilityWrite,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let value_enc = match crypto.encrypt_opaque_state(&job.signature) {
        Ok(enc) => enc,
        Err(_) => {
            counters.capture_storage_error.fetch_add(1, Relaxed);
            tracing::error!(kind = job.kind, "opaque_state_encrypt_failed");
            return;
        }
    };

    let created_at = crate::db::now_iso();
    let expires_at =
        (chrono::Utc::now() + chrono::Duration::from_std(PERSISTENT_TTL).unwrap()).to_rfc3339();

    let write = sqlx::query(
        "INSERT INTO opaque_provider_state
            (kind, scope_hash, tool_call_hash, session_hash, provider_id, family,
             producer, origin_model, tool_name_hash, value_enc, created_at, expires_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)
         ON CONFLICT (kind, scope_hash, tool_call_hash, provider_id, family, producer, origin_model)
         DO UPDATE SET
            session_hash = excluded.session_hash,
            tool_name_hash = excluded.tool_name_hash,
            value_enc = excluded.value_enc,
            created_at = excluded.created_at,
            expires_at = excluded.expires_at",
    )
    .bind(job.kind)
    .bind(&job.scope_hash)
    .bind(&job.tool_call_hash)
    .bind(&job.session_hash)
    .bind(&job.provider_id)
    .bind(&job.family)
    .bind(&job.producer)
    .bind(&job.origin_model)
    .bind(&job.tool_name_hash)
    .bind(&value_enc)
    .bind(&created_at)
    .bind(&expires_at)
    .execute(pool)
    .await;

    match write {
        Ok(_) => {
            counters.capture_stored.fetch_add(1, Relaxed);
        }
        Err(error) => {
            counters.capture_storage_error.fetch_add(1, Relaxed);
            // Never crash the client response on a storage failure; the next
            // provider turn may fail naturally instead (§13, §63).
            tracing::error!(error = %error, "opaque_state_capture_failed");
        }
    }
}

async fn prune_pool(pool: &Pool) {
    let now = chrono::Utc::now().to_rfc3339();
    let _ = sqlx::query("DELETE FROM opaque_provider_state WHERE expires_at < ?")
        .bind(&now)
        .execute(pool)
        .await;

    if let Ok(row) = sqlx::query("SELECT COUNT(*) as n FROM opaque_provider_state")
        .fetch_one(pool)
        .await
    {
        let count: i64 = row.get("n");
        if count > MAX_DB_ROWS {
            let overflow = count - MAX_DB_ROWS;
            let _ = sqlx::query(
                "DELETE FROM opaque_provider_state WHERE rowid IN (
                    SELECT rowid FROM opaque_provider_state
                    ORDER BY created_at ASC
                    LIMIT ?
                )",
            )
            .bind(overflow)
            .execute(pool)
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> OpaqueStateStore {
        let dir = std::env::temp_dir().join(format!(
            "kinetix-opaque-state-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("t.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let crypto = Arc::new(Crypto::new(&[11u8; 32]));
        OpaqueStateStore::new(pool, crypto)
    }

    fn gemini_target() -> OpaqueStateTarget {
        OpaqueStateTarget {
            kind: OpaqueStateKind::GeminiThoughtSignature,
            provider_id: "ai-studio".into(),
            family: "gemini".into(),
            producer: "native:gemini:v1".into(),
            model_id: "gemini-3".into(),
        }
    }

    fn capture(
        store: &OpaqueStateStore,
        scope: &OpaqueClientScope,
        target: &OpaqueStateTarget,
        session: Option<&str>,
        id: &str,
        name: &str,
        sig: &str,
    ) {
        store.capture_tool_signature(scope, target, session, id, name, sig);
    }

    #[tokio::test]
    async fn ram_hit_after_capture() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn sqlite_fallback_hit_after_ram_clear() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        store.clear_memory_cache_for_test();
        assert_eq!(store.memory_entry_count_for_test(), 0);
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
        // The fallback lookup must repopulate RAM.
        assert_eq!(store.memory_entry_count_for_test(), 1);
    }

    #[tokio::test]
    async fn durability_write_is_queued_and_flushable() {
        // `capture_tool_signature` is a synchronous `fn` (it cannot be awaited),
        // so the data plane never blocks on SQLite. The RAM hit below is
        // therefore immediate, and the durable copy appears after the worker
        // drains the queue (observed via the `flush()` barrier). This is the
        // invariant that keeps a locked DB off the streaming path.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_A".into())
        );
        // Drop RAM, wait for the durability barrier, then read back from SQLite.
        store.clear_memory_cache_for_test();
        store.flush().await;
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_A".into())
        );
    }

    /// Regression: re-capturing the oldest key while the RAM cache is at
    /// capacity must move that key to the back, not leave a stale duplicate at
    /// the front. Before the fix, the stale occurrence was evicted and its
    /// `cache.remove()` deleted the value that had just been written, so an
    /// immediate continuation missed RAM (and could read stale SQLite state).
    #[tokio::test]
    async fn recapturing_oldest_key_at_capacity_keeps_the_new_value_in_ram() {
        let mut store = test_store().await;
        store.set_max_memory_rows_for_test(3);
        let scope = OpaqueClientScope::for_key("key_lru");
        let target = gemini_target();

        // Fill exactly to capacity: call_1 is now the oldest entry.
        for index in 1..=3 {
            capture(
                &store,
                &scope,
                &target,
                None,
                &format!("call_{index}"),
                "bash",
                &format!("SIG_{index}"),
            );
        }
        assert_eq!(store.memory_entry_count_for_test(), 3);

        // Replace the oldest entry with a new value. Pre-fix this removed the
        // freshly inserted value instead of the stale ordering slot.
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_1_NEW");

        // Observe the RAM hot path directly (no SQLite read, so the async
        // durability worker cannot make the assertion race).
        assert_eq!(
            store.cached_signature_for_test(&scope, &target, "call_1"),
            Some("SIG_1_NEW".to_string()),
            "re-capturing the oldest key must keep the new value resident"
        );
        assert_eq!(
            store.memory_entry_count_for_test(),
            3,
            "re-capturing must not grow the cache beyond capacity"
        );

        // An immediate continuation resolves the new value without touching
        // SQLite or missing the cache.
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_1_NEW".into())
        );
    }

    #[tokio::test]
    async fn missing_returns_missing_not_synthesized() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_unknown", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Missing);
    }

    #[tokio::test]
    async fn scope_isolation_across_virtual_keys() {
        let store = test_store().await;
        let target = gemini_target();
        let scope_a = OpaqueClientScope::for_key("key_a");
        let scope_b = OpaqueClientScope::for_key("key_b");
        capture(
            &store,
            &scope_a,
            &target,
            None,
            "call_same",
            "bash",
            "SIG_A",
        );
        store.flush().await;
        let result = store
            .resolve_tool_signature(&scope_b, Some(&target), None, "call_same", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Missing);
    }

    #[tokio::test]
    async fn session_match_allows_restore() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(
            &store,
            &scope,
            &target,
            Some("sess_1"),
            "call_1",
            "bash",
            "SIG_A",
        );
        let result = store
            .resolve_tool_signature(&scope, Some(&target), Some("sess_1"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn session_mismatch_refuses_restore() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(
            &store,
            &scope,
            &target,
            Some("sess_1"),
            "call_1",
            "bash",
            "SIG_A",
        );
        let result = store
            .resolve_tool_signature(&scope, Some(&target), Some("sess_2"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::SessionMismatch);
    }

    #[tokio::test]
    async fn no_stored_session_allows_lookup_by_tool_id_alone() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        // Request now carries a session even though none was stored: allowed.
        let result = store
            .resolve_tool_signature(&scope, Some(&target), Some("sess_new"), "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn tool_name_mismatch_refuses_restore() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "read")
            .await;
        assert_eq!(result, OpaqueLookupResult::ToolNameMismatch);
    }

    #[tokio::test]
    async fn producer_mismatch_is_incompatible() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        let mut other = gemini_target();
        other.producer = "native:gemini:v2".into();
        let result = store
            .resolve_tool_signature(&scope, Some(&other), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn family_mismatch_is_incompatible() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        let mut other = gemini_target();
        other.family = "antigravity-claude".into();
        let result = store
            .resolve_tool_signature(&scope, Some(&other), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn different_model_in_same_family_is_incompatible() {
        // generateContent signatures are only guaranteed on the originating
        // model, so a different model must never be handed stored state.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        let mut other_model = gemini_target();
        other_model.model_id = "gemini-3-pro".into();
        let result = store
            .resolve_tool_signature(&scope, Some(&other_model), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn account_change_within_same_provider_stays_compatible() {
        // Compatibility is keyed on provider/model/family/producer only — not
        // account. Same-provider account failover must not destroy state.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn duplicate_upsert_same_signature_is_idempotent() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        assert_eq!(store.count_rows().await, 1);
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_A".into()));
    }

    #[tokio::test]
    async fn duplicate_write_different_signature_newest_wins() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_B");
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Compatible("SIG_B".into()));
        store.flush().await;
        assert_eq!(store.count_rows().await, 1);
    }

    #[tokio::test]
    async fn bytes_for_a_different_model_do_not_collide() {
        // Two models may share a tool-call id; each keeps its own row.
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let flash = gemini_target();
        let mut pro = gemini_target();
        pro.model_id = "gemini-3-pro".into();
        capture(&store, &scope, &flash, None, "call_1", "bash", "SIG_FLASH");
        capture(&store, &scope, &pro, None, "call_1", "bash", "SIG_PRO");
        store.flush().await;
        assert_eq!(store.count_rows().await, 2);
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&flash), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_FLASH".into())
        );
        assert_eq!(
            store
                .resolve_tool_signature(&scope, Some(&pro), None, "call_1", "bash")
                .await,
            OpaqueLookupResult::Compatible("SIG_PRO".into())
        );
    }

    #[tokio::test]
    async fn pruning_removes_rows_over_db_cap() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        // Insert a handful of rows and force a prune; cheap smoke test that
        // prune() does not error and respects the expiry predicate.
        for i in 0..5 {
            capture(
                &store,
                &scope,
                &target,
                None,
                &format!("call_{i}"),
                "bash",
                "SIG",
            );
        }
        store.flush().await;
        store.prune().await;
        assert_eq!(store.count_rows().await, 5);
    }

    #[tokio::test]
    async fn empty_signature_or_id_is_never_persisted() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "");
        capture(&store, &scope, &target, None, "", "bash", "SIG");
        store.flush().await;
        assert_eq!(store.count_rows().await, 0);
    }

    #[tokio::test]
    async fn target_without_capability_reports_incompatible_not_missing() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        // A target that cannot carry Gemini opaque state (capability = None)
        // must still observe that stored state exists, so portability policy
        // can act on it.
        let result = store
            .resolve_tool_signature(&scope, None, None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Incompatible);
    }

    #[tokio::test]
    async fn expired_state_is_treated_as_missing() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.expire_all_for_test().await;
        let result = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        assert_eq!(result, OpaqueLookupResult::Missing);
    }

    #[tokio::test]
    async fn metrics_track_capture_and_lookup_outcomes() {
        let store = test_store().await;
        let scope = OpaqueClientScope::for_key("key_a");
        let target = gemini_target();
        capture(&store, &scope, &target, None, "call_1", "bash", "SIG_A");
        store.flush().await;
        let _ = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_1", "bash")
            .await;
        let _ = store
            .resolve_tool_signature(&scope, Some(&target), None, "call_missing", "bash")
            .await;
        let metrics = store.metrics();
        assert_eq!(metrics.capture_stored, 1);
        assert_eq!(metrics.capture_dropped, 0);
        assert_eq!(metrics.lookup_hit, 1);
        assert_eq!(metrics.lookup_miss, 1);
        assert_eq!(metrics.entries, 1);
    }
}
