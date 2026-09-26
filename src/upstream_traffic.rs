//! Adaptive upstream concurrency and target-local congestion telemetry.
//!
//! This is deliberately separate from virtual-key admission: key RPM/TPM/budget
//! limits decide whether a client may send work, while this module protects an
//! upstream provider/account/model target from queue collapse.
//!
//! The controller is Gradient2-inspired rather than a byte-for-byte port of
//! Netflix concurrency-limits. LLM streams hold a permit for their full lifetime,
//! but congestion is learned from target-attempt-local time-to-first-event (TTFT)
//! so long completions do not look like upstream queueing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::Notify;

const INITIAL_LIMIT: f64 = 4.0;
const MIN_LIMIT: f64 = 1.0;
const MAX_LIMIT: f64 = 64.0;
const GRADIENT_TOLERANCE: f64 = 1.5;
const LIMIT_SMOOTHING: f64 = 0.2;
const QUEUE_ALLOWANCE_RATIO: f64 = 0.2;
const MAX_QUEUE_ALLOWANCE: f64 = 4.0;
const LIMIT_UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const OVERLOAD_BACKOFF: f64 = 0.7;
const TIMEOUT_BACKOFF: f64 = 0.8;
const FAST_TAU: Duration = Duration::from_secs(10);
const SLOW_TAU: Duration = Duration::from_secs(120);
const FAILURE_TAU: Duration = Duration::from_secs(60);
const FAILURE_DECAY_HORIZON: Duration = Duration::from_secs(5 * 60);
const TTFT_STALE_HORIZON: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetKey {
    pub provider_id: String,
    pub account_id: String,
    pub model_id: String,
}

impl TargetKey {
    pub fn new(
        provider_id: impl Into<String>,
        account_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            account_id: account_id.into(),
            model_id: model_id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficOutcome {
    Success,
    Overload,
    Timeout,
    Error,
    Neutral,
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct TrafficSnapshot {
    pub estimated_limit: u32,
    pub inflight: u32,
    pub has_capacity: bool,
    pub fast_ttft_ms: Option<f64>,
    pub slow_ttft_ms: Option<f64>,
    pub overload_ewma: f64,
    pub error_ewma: f64,
    pub ttft_samples: u64,
    pub overload_observations: u64,
    pub error_observations: u64,
}

#[derive(Clone, Default)]
pub struct UpstreamTraffic {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    targets: DashMap<TargetKey, Arc<TargetState>>,
}

struct TargetState {
    controller: Mutex<Controller>,
    notify: Notify,
}

impl Default for TargetState {
    fn default() -> Self {
        Self {
            controller: Mutex::new(Controller::default()),
            notify: Notify::new(),
        }
    }
}

struct Controller {
    limit: f64,
    inflight: u32,
    fast_ttft: TimeEwma,
    slow_ttft: TimeEwma,
    overload: TimeEwma,
    error: TimeEwma,
    ttft_samples: u64,
    overload_observations: u64,
    error_observations: u64,
    provisional_healthy: std::collections::BTreeMap<u64, Instant>,
    next_provisional_id: u64,
    overload_failure_at: Option<Instant>,
    error_failure_at: Option<Instant>,
    last_limit_update: Option<Instant>,
    window_max_inflight: u32,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            limit: INITIAL_LIMIT,
            inflight: 0,
            fast_ttft: TimeEwma::default(),
            slow_ttft: TimeEwma::default(),
            overload: TimeEwma::default(),
            error: TimeEwma::default(),
            ttft_samples: 0,
            overload_observations: 0,
            error_observations: 0,
            provisional_healthy: std::collections::BTreeMap::new(),
            next_provisional_id: 0,
            overload_failure_at: None,
            error_failure_at: None,
            last_limit_update: None,
            window_max_inflight: 0,
        }
    }
}

#[derive(Default)]
struct TimeEwma {
    value: Option<f64>,
    updated_at: Option<Instant>,
}

impl TimeEwma {
    fn update(&mut self, sample: f64, tau: Duration, now: Instant) {
        self.value = Some(match (self.value, self.updated_at) {
            (Some(current), Some(previous)) => {
                let dt = now.saturating_duration_since(previous).as_secs_f64();
                let tau = tau.as_secs_f64().max(f64::EPSILON);
                let alpha = 1.0 - (-dt / tau).exp();
                current + alpha * (sample - current)
            }
            _ => sample,
        });
        self.updated_at = Some(now);
    }

    fn value_or(&self, default: f64) -> f64 {
        self.value.unwrap_or(default)
    }

    fn update_failure(&mut self, sample: f64, now: Instant) {
        if let Some(updated_at) = self.updated_at {
            if now.saturating_duration_since(updated_at) >= FAILURE_DECAY_HORIZON {
                self.value = Some(sample);
                self.updated_at = Some(now);
                return;
            }
        }
        self.update(sample, FAILURE_TAU, now);
    }

    fn decayed_toward_zero(&self, tau: Duration, now: Instant) -> f64 {
        let Some(value) = self.value else {
            return 0.0;
        };
        let Some(updated_at) = self.updated_at else {
            return value;
        };
        let elapsed = now.saturating_duration_since(updated_at);
        if elapsed >= FAILURE_DECAY_HORIZON {
            return 0.0;
        }
        let tau = tau.as_secs_f64().max(f64::EPSILON);
        value * (-elapsed.as_secs_f64() / tau).exp()
    }
}

impl Controller {
    fn capacity(&self) -> u32 {
        self.limit.floor().clamp(MIN_LIMIT, MAX_LIMIT) as u32
    }

    fn snapshot(&self) -> TrafficSnapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> TrafficSnapshot {
        let capacity = self.capacity();
        let ttft_recent = self.fast_ttft.updated_at.is_some_and(|updated_at| {
            now.saturating_duration_since(updated_at) < TTFT_STALE_HORIZON
        });
        let latest_provisional = self.provisional_healthy.values().copied().max();
        let provisional_masks = |failure_at: Option<Instant>| {
            latest_provisional.is_some_and(|provisional_at| {
                failure_at.is_none_or(|failure_at| provisional_at > failure_at)
            })
        };
        let overload_ewma = self.overload.decayed_toward_zero(FAILURE_TAU, now);
        let error_ewma = self.error.decayed_toward_zero(FAILURE_TAU, now);
        TrafficSnapshot {
            estimated_limit: capacity,
            inflight: self.inflight,
            has_capacity: self.inflight < capacity,
            fast_ttft_ms: ttft_recent.then_some(self.fast_ttft.value).flatten(),
            slow_ttft_ms: ttft_recent.then_some(self.slow_ttft.value).flatten(),
            overload_ewma: if provisional_masks(self.overload_failure_at) {
                0.0
            } else {
                overload_ewma
            },
            error_ewma: if provisional_masks(self.error_failure_at) {
                0.0
            } else {
                error_ewma
            },
            ttft_samples: self.ttft_samples,
            overload_observations: self.overload_observations,
            error_observations: self.error_observations,
        }
    }

    fn release(&mut self) {
        self.inflight = self.inflight.saturating_sub(1);
    }

    fn begin_provisional_healthy(&mut self, now: Instant) -> u64 {
        self.next_provisional_id = self.next_provisional_id.wrapping_add(1).max(1);
        let id = self.next_provisional_id;
        self.provisional_healthy.insert(id, now);
        id
    }

    fn end_provisional_healthy(&mut self, id: u64) {
        self.provisional_healthy.remove(&id);
    }

    fn record_healthy(&mut self, now: Instant) {
        self.error.update_failure(0.0, now);
        self.error_observations = self.error_observations.saturating_add(1);
        self.overload.update_failure(0.0, now);
        self.overload_observations = self.overload_observations.saturating_add(1);
    }

    fn record_ttft(&mut self, inflight_at_start: u32, ttft: Duration, now: Instant) {
        let sample_ms = ttft.as_secs_f64() * 1000.0;
        if self.fast_ttft.updated_at.is_some_and(|updated_at| {
            now.saturating_duration_since(updated_at) >= TTFT_STALE_HORIZON
        }) {
            self.fast_ttft = TimeEwma::default();
            self.slow_ttft = TimeEwma::default();
            self.last_limit_update = None;
            self.window_max_inflight = 0;
        }
        self.fast_ttft.update(sample_ms, FAST_TAU, now);
        self.slow_ttft.update(sample_ms, SLOW_TAU, now);
        self.ttft_samples = self.ttft_samples.saturating_add(1);
        self.window_max_inflight = self.window_max_inflight.max(inflight_at_start);

        let Some(last_update) = self.last_limit_update else {
            self.last_limit_update = Some(now);
            return;
        };
        if now.saturating_duration_since(last_update) < LIMIT_UPDATE_INTERVAL {
            return;
        }

        // Gradient2-style limit movement is window-clocked, not completion-
        // clocked. A burst of samples inside one wall-clock window therefore
        // cannot grow the limit faster than sparse traffic over the same time.
        if (self.window_max_inflight as f64) * 2.0 >= self.limit {
            let short = self.fast_ttft.value_or(sample_ms).max(f64::EPSILON);
            let long = self.slow_ttft.value_or(sample_ms).max(f64::EPSILON);
            let gradient = (GRADIENT_TOLERANCE * long / short).clamp(0.5, 1.0);
            let queue_allowance = (self.limit * QUEUE_ALLOWANCE_RATIO).min(MAX_QUEUE_ALLOWANCE);
            let next = self.limit * gradient + queue_allowance;
            self.limit = (self.limit * (1.0 - LIMIT_SMOOTHING) + next * LIMIT_SMOOTHING)
                .clamp(MIN_LIMIT, MAX_LIMIT);
        }

        self.last_limit_update = Some(now);
        self.window_max_inflight = 0;
    }

    fn complete(&mut self, inflight_at_start: u32, outcome: TrafficOutcome, now: Instant) {
        match outcome {
            TrafficOutcome::Success => {
                self.record_healthy(now);
            }
            TrafficOutcome::Overload => {
                self.overload_failure_at = Some(now);
                self.error_failure_at = Some(now);
                self.overload.update_failure(1.0, now);
                self.overload_observations = self.overload_observations.saturating_add(1);
                self.error.update_failure(1.0, now);
                self.error_observations = self.error_observations.saturating_add(1);
                self.limit = (self.limit * OVERLOAD_BACKOFF).clamp(MIN_LIMIT, MAX_LIMIT);
            }
            TrafficOutcome::Timeout => {
                self.error_failure_at = Some(now);
                self.error.update_failure(1.0, now);
                self.error_observations = self.error_observations.saturating_add(1);
                if (inflight_at_start as f64) * 2.0 >= self.limit {
                    self.limit = (self.limit * TIMEOUT_BACKOFF).clamp(MIN_LIMIT, MAX_LIMIT);
                }
            }
            TrafficOutcome::Error => {
                self.error_failure_at = Some(now);
                self.error.update_failure(1.0, now);
                self.error_observations = self.error_observations.saturating_add(1);
            }
            TrafficOutcome::Neutral | TrafficOutcome::Cancelled => {}
        }
        self.release();
    }
}

impl UpstreamTraffic {
    fn target(&self, key: &TargetKey) -> Arc<TargetState> {
        self.inner
            .targets
            .entry(key.clone())
            .or_insert_with(|| Arc::new(TargetState::default()))
            .clone()
    }

    pub fn snapshot(&self, key: &TargetKey) -> TrafficSnapshot {
        self.snapshot_at(key, Instant::now())
    }

    pub(crate) fn snapshot_at(&self, key: &TargetKey, now: Instant) -> TrafficSnapshot {
        if let Some(target) = self.inner.targets.get(key) {
            return target.controller.lock().snapshot_at(now);
        }
        Controller::default().snapshot_at(now)
    }

    pub async fn acquire(
        &self,
        key: TargetKey,
        wait: Duration,
    ) -> Result<TrafficPermit, TrafficSnapshot> {
        let target = self.target(&key);
        let deadline = tokio::time::Instant::now() + wait;

        loop {
            let notified = target.notify.notified();
            {
                let mut controller = target.controller.lock();
                if controller.inflight < controller.capacity() {
                    controller.inflight += 1;
                    let inflight_at_start = controller.inflight;
                    return Ok(TrafficPermit {
                        target: target.clone(),
                        inflight_at_start,
                        first_event_recorded: AtomicBool::new(false),
                        provisional_id: AtomicU64::new(0),
                        finished: AtomicBool::new(false),
                    });
                }
                if wait.is_zero() {
                    return Err(controller.snapshot());
                }
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(target.controller.lock().snapshot());
            }
        }
    }
}

pub struct TrafficPermit {
    target: Arc<TargetState>,
    inflight_at_start: u32,
    first_event_recorded: AtomicBool,
    provisional_id: AtomicU64,
    finished: AtomicBool,
}

impl TrafficPermit {
    pub fn mark_first_event(&self, ttft: Duration) {
        if self.first_event_recorded.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut controller = self.target.controller.lock();
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        controller.record_ttft(self.inflight_at_start, ttft, now);
        let provisional_id = controller.begin_provisional_healthy(now);
        self.provisional_id.store(provisional_id, Ordering::Release);
    }

    pub fn finish(&self, outcome: TrafficOutcome) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut controller = self.target.controller.lock();
            let provisional_id = self.provisional_id.swap(0, Ordering::AcqRel);
            if provisional_id != 0 {
                controller.end_provisional_healthy(provisional_id);
            }
            controller.complete(self.inflight_at_start, outcome, Instant::now());
        }
        self.target.notify.notify_one();
    }
}

impl Drop for TrafficPermit {
    fn drop(&mut self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut controller = self.target.controller.lock();
            let provisional_id = self.provisional_id.swap(0, Ordering::AcqRel);
            if provisional_id != 0 {
                controller.end_provisional_healthy(provisional_id);
            }
            controller.release();
        }
        self.target.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> TargetKey {
        TargetKey::new("provider", "account", "model")
    }

    #[tokio::test]
    async fn saturation_is_bounded_and_drop_releases_capacity() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        let mut permits = Vec::new();
        for _ in 0..INITIAL_LIMIT as usize {
            permits.push(
                traffic
                    .acquire(key.clone(), Duration::ZERO)
                    .await
                    .expect("initial capacity"),
            );
        }

        let saturated = match traffic.acquire(key.clone(), Duration::ZERO).await {
            Ok(_) => panic!("limit must reject an extra permit"),
            Err(snapshot) => snapshot,
        };
        assert!(!saturated.has_capacity);

        permits.pop();
        traffic
            .acquire(key, Duration::from_millis(20))
            .await
            .expect("released capacity should wake a waiter");
    }

    #[tokio::test]
    async fn overload_reduces_limit_immediately() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        let before = traffic.snapshot(&key).estimated_limit;
        let permit = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();
        permit.finish(TrafficOutcome::Overload);
        let after = traffic.snapshot(&key).estimated_limit;
        assert!(after < before);
    }

    #[tokio::test]
    async fn first_event_updates_ttft_before_stream_finishes() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        let permit = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();

        permit.mark_first_event(Duration::from_millis(100));

        let snapshot = traffic.snapshot(&key);
        assert_eq!(snapshot.ttft_samples, 1);
        assert_eq!(snapshot.fast_ttft_ms, Some(100.0));
        assert_eq!(snapshot.inflight, 1);
        assert_eq!(snapshot.error_observations, 0);
        assert_eq!(snapshot.error_ewma, 0.0);

        permit.finish(TrafficOutcome::Success);
        assert_eq!(
            traffic.snapshot(&key).error_observations,
            1,
            "terminal success commits the provisional first-event health sample"
        );
    }

    #[tokio::test]
    async fn cancellation_after_first_event_keeps_ttft_sample() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        let before = traffic.snapshot(&key).estimated_limit;
        let permit = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();

        permit.mark_first_event(Duration::from_millis(125));
        permit.finish(TrafficOutcome::Cancelled);

        let snapshot = traffic.snapshot(&key);
        assert_eq!(snapshot.estimated_limit, before);
        assert_eq!(snapshot.inflight, 0);
        assert_eq!(snapshot.ttft_samples, 1);
        assert_eq!(snapshot.fast_ttft_ms, Some(125.0));
        assert_eq!(snapshot.error_observations, 0);
        assert_eq!(snapshot.error_ewma, 0.0);
    }

    #[tokio::test]
    async fn post_commit_error_after_first_event_adds_error_observation() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        let permit = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();

        permit.mark_first_event(Duration::from_millis(100));
        assert_eq!(traffic.snapshot(&key).error_observations, 0);

        permit.finish(TrafficOutcome::Error);

        let snapshot = traffic.snapshot(&key);
        assert_eq!(snapshot.error_observations, 1);
        assert!(
            snapshot.error_ewma > 0.9,
            "an immediate post-commit stream error must produce a meaningful penalty"
        );
        assert_eq!(snapshot.inflight, 0);
    }

    #[tokio::test]
    async fn concurrent_failure_is_visible_while_provisional_stream_remains_open() {
        let traffic = UpstreamTraffic::default();
        let key = key();

        let open_stream = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();
        open_stream.mark_first_event(Duration::from_millis(100));
        assert_eq!(traffic.snapshot(&key).error_ewma, 0.0);

        let failed_request = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();
        failed_request.finish(TrafficOutcome::Error);

        let snapshot = traffic.snapshot(&key);
        assert_eq!(snapshot.inflight, 1);
        assert_eq!(snapshot.error_observations, 1);
        assert!(
            snapshot.error_ewma > 0.9,
            "a newer concurrent failure must not be masked by an older provisional stream"
        );

        open_stream.finish(TrafficOutcome::Success);
    }

    #[test]
    fn limit_growth_is_wall_clock_gated_not_sample_density_gated() {
        let start = Instant::now();
        let mut sparse = Controller::default();
        let mut dense = Controller::default();

        for second in 0..=2 {
            sparse.record_ttft(
                INITIAL_LIMIT as u32,
                Duration::from_millis(100),
                start + Duration::from_secs(second),
            );
        }

        for step in 0..=20 {
            dense.record_ttft(
                INITIAL_LIMIT as u32,
                Duration::from_millis(100),
                start + Duration::from_millis(step * 100),
            );
        }

        assert_eq!(sparse.capacity(), dense.capacity());
        assert!((sparse.limit - dense.limit).abs() < f64::EPSILON);
        assert_eq!(sparse.ttft_samples, 3);
        assert_eq!(dense.ttft_samples, 21);
    }

    #[tokio::test]
    async fn generic_error_records_outcome_without_ttft() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        let permit = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();

        permit.finish(TrafficOutcome::Error);

        let snapshot = traffic.snapshot(&key);
        assert_eq!(snapshot.ttft_samples, 0);
        assert_eq!(snapshot.error_observations, 1);
        assert!(snapshot.error_ewma > 0.99);
    }

    #[tokio::test]
    async fn failure_penalty_decays_to_neutral_without_new_traffic() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        traffic
            .acquire(key.clone(), Duration::ZERO)
            .await
            .unwrap()
            .finish(TrafficOutcome::Error);

        assert!(traffic.snapshot(&key).error_ewma > 0.0);

        let future = Instant::now() + FAILURE_DECAY_HORIZON + Duration::from_secs(1);
        let recovered = traffic.snapshot_at(&key, future);
        assert_eq!(recovered.error_ewma, 0.0);
        assert_eq!(recovered.overload_ewma, 0.0);
    }

    #[test]
    fn stale_ttft_becomes_cold_and_new_sample_resets_baseline() {
        let start = Instant::now();
        let mut controller = Controller::default();

        controller.record_ttft(INITIAL_LIMIT as u32, Duration::from_secs(5), start);
        assert_eq!(controller.snapshot_at(start).fast_ttft_ms, Some(5_000.0));

        let stale_at = start + TTFT_STALE_HORIZON + Duration::from_secs(1);
        let stale = controller.snapshot_at(stale_at);
        assert_eq!(stale.fast_ttft_ms, None);
        assert_eq!(stale.slow_ttft_ms, None);

        controller.record_ttft(INITIAL_LIMIT as u32, Duration::from_millis(100), stale_at);
        let recovered = controller.snapshot_at(stale_at);
        assert_eq!(recovered.fast_ttft_ms, Some(100.0));
        assert_eq!(recovered.slow_ttft_ms, Some(100.0));
    }

    #[test]
    fn strong_ttft_congestion_reduces_limit() {
        let start = Instant::now();
        let mut controller = Controller::default();

        for second in 0..=2 {
            controller.record_ttft(
                INITIAL_LIMIT as u32,
                Duration::from_millis(100),
                start + Duration::from_secs(second),
            );
        }
        let before = controller.limit;

        controller.record_ttft(
            INITIAL_LIMIT as u32,
            Duration::from_secs(5),
            start + Duration::from_secs(3),
        );

        assert!(
            controller.limit < before,
            "strong TTFT congestion must not increase the concurrency limit"
        );
    }

    #[tokio::test]
    async fn idle_timeout_does_not_collapse_limit() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        let before = traffic.snapshot(&key).estimated_limit;
        let permit = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();
        permit.finish(TrafficOutcome::Timeout);
        assert_eq!(traffic.snapshot(&key).estimated_limit, before);
    }

    #[tokio::test]
    async fn cancellation_releases_without_penalty() {
        let traffic = UpstreamTraffic::default();
        let key = key();
        let before = traffic.snapshot(&key).estimated_limit;
        let permit = traffic.acquire(key.clone(), Duration::ZERO).await.unwrap();
        permit.finish(TrafficOutcome::Cancelled);
        let snapshot = traffic.snapshot(&key);
        assert_eq!(snapshot.estimated_limit, before);
        assert_eq!(snapshot.inflight, 0);
        assert_eq!(snapshot.error_ewma, 0.0);
        assert_eq!(snapshot.error_observations, 0);
    }
}
