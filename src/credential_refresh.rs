//! Proactive credential lease scheduling and refresh singleflight.
//!
//! Core owns *when* a credential should be renewed; credential strategies own
//! *how* renewal happens. The coordinator never stores plaintext credentials.
//! It keeps only account identity, lease deadlines, retry state, and the
//! account-scoped rotation gate shared by proactive and reactive refreshes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dashmap::DashMap;

use crate::credentials::{CredentialRotationError, CredentialStrategy, ResolvedCredential};
use crate::db::AccountRow;

const DEFAULT_REFRESH_LEAD_SECS: i64 = 5 * 60;
const MIN_RETRY_SECS: u64 = 5;
const MAX_RETRY_SECS: u64 = 5 * 60;
const CLAIM_GRACE_SECS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialKey {
    pub provider_id: String,
    pub account_id: String,
}

impl CredentialKey {
    pub fn new(provider_id: impl Into<String>, account_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            account_id: account_id.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct LeaseSchedule {
    next_attempt_at: DateTime<Utc>,
    failures: u32,
}

struct RotationGate {
    lock: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    last_auth_result:
        parking_lot::Mutex<Option<std::result::Result<bool, CredentialRotationError>>>,
}

impl RotationGate {
    fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            last_auth_result: parking_lot::Mutex::new(None),
        }
    }
}

/// Host-owned credential refresh scheduler.
///
/// Cloning this type is cheap. All state is shared and bounded by the number of
/// plugin-backed accounts Kinetix has observed.
#[derive(Clone, Default)]
pub struct RefreshCoordinator {
    schedules: Arc<DashMap<CredentialKey, LeaseSchedule>>,
    gates: Arc<DashMap<CredentialKey, Arc<RotationGate>>>,
}

impl RefreshCoordinator {
    /// Observe a freshly resolved credential lease and schedule its next
    /// proactive renewal. Explicit plugin-provided `refresh_after` wins;
    /// otherwise core derives a provider-neutral five-minute lead from expiry.
    pub fn observe(&self, provider_id: &str, account_id: &str, credential: &ResolvedCredential) {
        self.store_schedule(provider_id, account_id, credential, false);
    }

    fn observe_refreshed(
        &self,
        provider_id: &str,
        account_id: &str,
        credential: &ResolvedCredential,
    ) {
        self.store_schedule(provider_id, account_id, credential, true);
    }

    fn store_schedule(
        &self,
        provider_id: &str,
        account_id: &str,
        credential: &ResolvedCredential,
        reset_failures: bool,
    ) {
        let key = CredentialKey::new(provider_id, account_id);
        let Some(mut schedule) = schedule_from_credential(credential, Utc::now()) else {
            self.schedules.remove(&key);
            return;
        };

        if !reset_failures {
            if let Some(existing) = self.schedules.get(&key) {
                if existing.failures > 0 {
                    schedule.failures = existing.failures;
                    schedule.next_attempt_at = schedule
                        .next_attempt_at
                        .max(existing.next_attempt_at.to_owned());
                }
            }
        }

        self.schedules.insert(key, schedule);
    }

    /// Forget all proactive state for an account. Used when an account is
    /// removed, disabled, or a refresh token is confirmed invalid.
    pub fn forget(&self, provider_id: &str, account_id: &str) {
        let key = CredentialKey::new(provider_id, account_id);
        self.schedules.remove(&key);
        self.gates.remove(&key);
    }

    /// Atomically claim every lease whose refresh deadline has arrived.
    ///
    /// Claiming pushes the next attempt out briefly so the scheduler cannot
    /// enqueue duplicate work while a slow plugin refresh is still running.
    pub fn claim_due(&self, now: DateTime<Utc>) -> Vec<CredentialKey> {
        let mut due = Vec::new();
        for mut entry in self.schedules.iter_mut() {
            if entry.next_attempt_at <= now {
                entry.next_attempt_at = now.to_owned() + ChronoDuration::seconds(CLAIM_GRACE_SECS);
                due.push(entry.key().clone());
            }
        }
        due
    }

    /// Reactive 401/auth-error renewal. This preserves the existing semantic:
    /// concurrent failures for one account perform one rotation and all waiters
    /// reuse its result. It shares the exact same gate as proactive refresh.
    pub async fn rotate_after_auth_error(
        &self,
        provider_id: &str,
        strategy: Arc<dyn CredentialStrategy>,
        account: &AccountRow,
        failed_secret: &str,
    ) -> std::result::Result<bool, CredentialRotationError> {
        let key = CredentialKey::new(provider_id, &account.id);
        let gate = self.gate(&key);
        let observed_generation = gate.generation.load(Ordering::Acquire);
        let _guard = gate.lock.lock().await;

        if gate.generation.load(Ordering::Acquire) != observed_generation {
            if let Some(result) = gate.last_auth_result.lock().clone() {
                return result;
            }
        }

        let result = match strategy.resolve(account).await {
            Ok(current) if current.secret != failed_secret => {
                self.observe_refreshed(provider_id, &account.id, &current);
                Ok(true)
            }
            _ => match strategy.rotate(account).await {
                Ok(()) => match strategy.resolve(account).await {
                    Ok(current) => {
                        self.observe_refreshed(provider_id, &account.id, &current);
                        Ok(true)
                    }
                    Err(error) => {
                        let error = CredentialRotationError::new(
                            "plugin_internal",
                            format!("credential resolve failed after rotation: {error}"),
                            true,
                            None,
                        );
                        self.record_failure(&key, &error);
                        Err(error)
                    }
                },
                Err(error) => {
                    self.record_failure(&key, &error);
                    Err(error)
                }
            },
        };

        if result
            .as_ref()
            .err()
            .is_some_and(CredentialRotationError::invalid_credential)
        {
            self.schedules.remove(&key);
        }

        *gate.last_auth_result.lock() = Some(result.clone());
        gate.generation.fetch_add(1, Ordering::Release);
        result
    }

    /// Refresh one scheduler-claimed account. A re-resolve under the shared
    /// account lock prevents a proactive tick from rotating a credential that
    /// a concurrent reactive request already renewed.
    pub async fn rotate_scheduled(
        &self,
        provider_id: &str,
        strategy: Arc<dyn CredentialStrategy>,
        account: &AccountRow,
    ) -> std::result::Result<bool, CredentialRotationError> {
        let key = CredentialKey::new(provider_id, &account.id);
        let gate = self.gate(&key);
        let _guard = gate.lock.lock().await;

        let current = match strategy.resolve(account).await {
            Ok(current) => current,
            Err(error) => {
                let error = CredentialRotationError::new(
                    "plugin_internal",
                    format!("credential resolve failed before scheduled refresh: {error}"),
                    true,
                    None,
                );
                self.record_failure(&key, &error);
                return Err(error);
            }
        };

        // claim_due() temporarily pushes the stored deadline forward to keep a
        // slow refresh from being enqueued twice. Re-check the deadline from
        // the freshly resolved lease itself, not that claim marker. If another
        // path already refreshed the lease, its new future deadline wins and
        // this scheduled attempt becomes a no-op.
        let now = Utc::now();
        let Some(current_schedule) = schedule_from_credential(&current, now.to_owned()) else {
            self.schedules.remove(&key);
            return Ok(false);
        };
        if current_schedule.next_attempt_at > now {
            self.observe_refreshed(provider_id, &account.id, &current);
            return Ok(false);
        }

        match strategy.rotate(account).await {
            Ok(()) => {
                let current = match strategy.resolve(account).await {
                    Ok(current) => current,
                    Err(error) => {
                        let error = CredentialRotationError::new(
                            "plugin_internal",
                            format!("credential resolve failed after scheduled refresh: {error}"),
                            true,
                            None,
                        );
                        self.record_failure(&key, &error);
                        return Err(error);
                    }
                };
                self.observe_refreshed(provider_id, &account.id, &current);
                // A scheduled rotation changed the generation, but it did not
                // produce a reactive auth result. Clear any older auth result
                // so a queued 401 waiter re-resolves instead of reusing stale
                // outcome state from an earlier auth failure.
                *gate.last_auth_result.lock() = None;
                gate.generation.fetch_add(1, Ordering::Release);
                Ok(true)
            }
            Err(error) => {
                self.record_failure(&key, &error);
                if error.invalid_credential() {
                    self.schedules.remove(&key);
                }
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub fn next_attempt_at(&self, provider_id: &str, account_id: &str) -> Option<DateTime<Utc>> {
        self.schedules
            .get(&CredentialKey::new(provider_id, account_id))
            .map(|entry| entry.next_attempt_at.to_owned())
    }

    fn gate(&self, key: &CredentialKey) -> Arc<RotationGate> {
        self.gates
            .entry(key.clone())
            .or_insert_with(|| Arc::new(RotationGate::new()))
            .clone()
    }

    fn record_failure(&self, key: &CredentialKey, error: &CredentialRotationError) {
        let now = Utc::now();
        let mut schedule = self.schedules.entry(key.clone()).or_insert(LeaseSchedule {
            next_attempt_at: now.to_owned(),
            failures: 0,
        });
        schedule.failures = schedule.failures.saturating_add(1);
        let retry = error
            .retry_after_secs
            .unwrap_or_else(|| exponential_backoff_secs(schedule.failures));
        schedule.next_attempt_at = now + ChronoDuration::seconds(retry.min(MAX_RETRY_SECS) as i64);
    }
}

fn parse_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    value.and_then(crate::db::parse_dt)
}

fn schedule_from_credential(
    credential: &ResolvedCredential,
    now: DateTime<Utc>,
) -> Option<LeaseSchedule> {
    let expires_at = parse_time(credential.expires_at.as_deref());
    let refresh_after = parse_time(credential.refresh_after.as_deref());

    let requested = refresh_after.or_else(|| {
        expires_at.map(|expires| expires - ChronoDuration::seconds(DEFAULT_REFRESH_LEAD_SECS))
    })?;
    let next_attempt_at = requested.max(now);

    Some(LeaseSchedule {
        next_attempt_at,
        failures: 0,
    })
}

fn exponential_backoff_secs(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(6);
    MIN_RETRY_SECS
        .saturating_mul(1_u64 << shift)
        .min(MAX_RETRY_SECS)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    fn account() -> AccountRow {
        AccountRow {
            id: "a1".into(),
            provider_id: "p1".into(),
            label: "oauth".into(),
            secret_enc: String::new(),
            key_mask: "oauth:****".into(),
            status: "healthy".into(),
            cooldown_until: None,
            quota_reset_at: None,
            quota_type: "none".into(),
            quota_window_s: None,
            soft_quota_usd: None,
            priority: 1,
            weight: 1,
            last_error: None,
            last_probe_at: None,
            circuit_open_until: None,
            consecutive_failures: 0,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    struct RotatingStrategy {
        rotations: AtomicUsize,
        secret: tokio::sync::Mutex<String>,
    }

    impl RotatingStrategy {
        fn new() -> Self {
            Self {
                rotations: AtomicUsize::new(0),
                secret: tokio::sync::Mutex::new("stale".into()),
            }
        }
    }

    #[async_trait]
    impl CredentialStrategy for RotatingStrategy {
        fn name(&self) -> &'static str {
            "test_rotating"
        }

        async fn resolve(&self, _account: &AccountRow) -> anyhow::Result<ResolvedCredential> {
            let rotations = self.rotations.load(Ordering::Relaxed);
            let now = Utc::now();
            Ok(ResolvedCredential {
                secret: self.secret.lock().await.clone(),
                expires_at: Some((now.to_owned() + ChronoDuration::hours(2)).to_rfc3339()),
                refresh_after: Some(
                    if rotations == 0 {
                        now - ChronoDuration::seconds(1)
                    } else {
                        now + ChronoDuration::hours(1)
                    }
                    .to_rfc3339(),
                ),
                rotated: rotations > 0,
            })
        }

        async fn rotate(
            &self,
            _account: &AccountRow,
        ) -> std::result::Result<(), CredentialRotationError> {
            self.rotations.fetch_add(1, Ordering::Relaxed);
            *self.secret.lock().await = "fresh".into();
            Ok(())
        }
    }

    #[test]
    fn explicit_refresh_after_wins_over_derived_expiry_lead() {
        let now = Utc::now();
        let explicit = now.to_owned() + ChronoDuration::minutes(30);
        let credential = ResolvedCredential {
            secret: "secret".into(),
            expires_at: Some((now + ChronoDuration::hours(2)).to_rfc3339()),
            refresh_after: Some(explicit.to_rfc3339()),
            rotated: false,
        };
        let schedule = schedule_from_credential(&credential, now).unwrap();
        assert_eq!(schedule.next_attempt_at, explicit);
    }

    #[test]
    fn expiry_without_refresh_hint_uses_core_default_lead() {
        let now = Utc::now();
        let expiry = now.to_owned() + ChronoDuration::minutes(20);
        let credential = ResolvedCredential {
            secret: "secret".into(),
            expires_at: Some(expiry.to_rfc3339()),
            refresh_after: None,
            rotated: false,
        };
        let schedule = schedule_from_credential(&credential, now).unwrap();
        assert_eq!(
            schedule.next_attempt_at,
            expiry - ChronoDuration::seconds(DEFAULT_REFRESH_LEAD_SECS)
        );
    }

    #[test]
    fn static_or_unscheduled_credentials_are_not_tracked() {
        let coordinator = RefreshCoordinator::default();
        coordinator.observe(
            "p",
            "a",
            &ResolvedCredential {
                secret: "secret".into(),
                expires_at: None,
                refresh_after: None,
                rotated: false,
            },
        );
        assert!(coordinator.next_attempt_at("p", "a").is_none());
    }

    #[test]
    fn transient_failure_backoff_survives_normal_resolve_observation() {
        let coordinator = RefreshCoordinator::default();
        let credential = ResolvedCredential {
            secret: "secret".into(),
            expires_at: Some((Utc::now() + ChronoDuration::hours(1)).to_rfc3339()),
            refresh_after: Some((Utc::now() - ChronoDuration::seconds(1)).to_rfc3339()),
            rotated: false,
        };
        coordinator.observe("p", "a", &credential);
        let key = CredentialKey::new("p", "a");
        coordinator.record_failure(
            &key,
            &CredentialRotationError::new("upstream_unavailable", "temporary", true, None),
        );
        let retry_at = coordinator.next_attempt_at("p", "a").unwrap();

        coordinator.observe("p", "a", &credential);

        assert_eq!(
            coordinator.next_attempt_at("p", "a").unwrap(),
            retry_at,
            "ordinary credential resolution must not erase refresh backoff"
        );
    }

    #[tokio::test]
    async fn expired_backoff_claim_still_runs_the_due_refresh() {
        let coordinator = RefreshCoordinator::default();
        let strategy = Arc::new(RotatingStrategy::new());
        let account = account();
        let initial = strategy.resolve(&account).await.unwrap();
        coordinator.observe("p1", &account.id, &initial);

        let key = CredentialKey::new("p1", &account.id);
        coordinator.record_failure(
            &key,
            &CredentialRotationError::new("upstream_unavailable", "temporary", true, Some(5)),
        );
        coordinator.schedules.get_mut(&key).unwrap().next_attempt_at =
            Utc::now() - ChronoDuration::seconds(1);

        assert_eq!(coordinator.claim_due(Utc::now()).len(), 1);
        assert!(coordinator
            .rotate_scheduled("p1", strategy.clone(), &account)
            .await
            .unwrap());
        assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn scheduled_refresh_rotates_due_lease_and_reschedules() {
        let coordinator = RefreshCoordinator::default();
        let strategy = Arc::new(RotatingStrategy::new());
        let account = account();
        let initial = strategy.resolve(&account).await.unwrap();
        coordinator.observe("p1", &account.id, &initial);

        assert_eq!(coordinator.claim_due(Utc::now()).len(), 1);
        assert!(coordinator
            .rotate_scheduled("p1", strategy.clone(), &account)
            .await
            .unwrap());
        assert_eq!(strategy.rotations.load(Ordering::Relaxed), 1);
        assert!(
            coordinator.next_attempt_at("p1", &account.id).unwrap()
                > Utc::now() + ChronoDuration::minutes(30)
        );
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(exponential_backoff_secs(1), 5);
        assert_eq!(exponential_backoff_secs(2), 10);
        assert_eq!(exponential_backoff_secs(3), 20);
        assert_eq!(exponential_backoff_secs(50), MAX_RETRY_SECS);
    }
}
