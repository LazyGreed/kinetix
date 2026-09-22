//! Atomic per-key request/token/budget admission.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate, Utc};
use dashmap::DashMap;

use crate::db::{self, ModelRow, Pool, VirtualKeyRow};
use crate::registry::{Registry, Resolved, Snapshot};
use crate::types::{InternalRequest, Prices, ProxyError, TokenUsage};

const MINUTE_WINDOW: Duration = Duration::from_secs(60);
const DEFAULT_OUTPUT_RESERVATION: u64 = 8_192;

#[derive(Clone, Default)]
pub struct AdmissionController {
    keys: Arc<DashMap<String, Arc<KeyAdmission>>>,
    next_id: Arc<AtomicU64>,
}

struct KeyAdmission {
    initialized: AtomicBool,
    init_lock: tokio::sync::Mutex<()>,
    ledger: parking_lot::Mutex<KeyLedger>,
}

impl Default for KeyAdmission {
    fn default() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            init_lock: tokio::sync::Mutex::new(()),
            ledger: parking_lot::Mutex::new(KeyLedger::default()),
        }
    }
}

#[derive(Default)]
struct KeyLedger {
    minute: VecDeque<MinuteUse>,
    active: HashMap<u64, ActiveReservation>,
    daily_day: Option<NaiveDate>,
    daily_spend: f64,
    monthly_key: Option<(i32, u32)>,
    monthly_spend: f64,
}

struct MinuteUse {
    at: Instant,
    tokens: u64,
}

struct ActiveReservation {
    at: Instant,
    tokens: u64,
    cost: Option<f64>,
    day: NaiveDate,
    month: (i32, u32),
}

#[derive(Debug, Clone, Copy)]
pub struct AdmissionEstimate {
    pub tokens: u64,
    pub cost: Option<f64>,
}

pub struct AdmissionReservation {
    entry: Arc<KeyAdmission>,
    id: u64,
    settled: bool,
}

impl AdmissionReservation {
    pub fn reconcile(mut self, usage: &TokenUsage, actual_cost: Option<f64>) {
        let actual_tokens = match (usage.input, usage.output) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            _ => None,
        };
        let actual_cost = actual_tokens.and(actual_cost);
        self.entry.ledger.lock().reconcile(
            self.id,
            actual_tokens,
            actual_cost,
            Utc::now(),
            Instant::now(),
        );
        self.settled = true;
    }
}

impl Drop for AdmissionReservation {
    fn drop(&mut self) {
        if !self.settled {
            self.entry.ledger.lock().cancel(self.id);
        }
    }
}

impl AdmissionController {
    fn entry(&self, key_id: &str) -> Arc<KeyAdmission> {
        self.keys
            .entry(key_id.to_string())
            .or_insert_with(|| Arc::new(KeyAdmission::default()))
            .clone()
    }

    async fn ensure_initialized(&self, pool: &Pool, key_id: &str, entry: &Arc<KeyAdmission>) {
        if entry.initialized.load(Ordering::Acquire) {
            return;
        }

        let _guard = entry.init_lock.lock().await;
        if entry.initialized.load(Ordering::Acquire) {
            return;
        }

        let wall_now = Utc::now();
        let instant_now = Instant::now();
        let minute_since = (wall_now - chrono::Duration::seconds(60)).to_rfc3339();
        let daily_since = crate::pool::window_start("daily", None);
        let monthly_since = crate::pool::window_start("monthly", None);

        let (minute, daily, monthly) = tokio::join!(
            db::key_usage_entries_since(pool, key_id, &minute_since),
            db::key_spend_since(pool, key_id, &daily_since),
            db::key_spend_since(pool, key_id, &monthly_since),
        );

        let mut ledger = entry.ledger.lock();
        ledger.roll_periods(wall_now);
        match minute {
            Ok(rows) => {
                for (ts, tokens) in rows {
                    let Some(at) = db::parse_dt(&ts) else {
                        continue;
                    };
                    let age = wall_now
                        .signed_duration_since(at)
                        .to_std()
                        .unwrap_or_default()
                        .min(MINUTE_WINDOW);
                    ledger.minute.push_back(MinuteUse {
                        at: instant_now.checked_sub(age).unwrap_or(instant_now),
                        tokens: tokens.max(0) as u64,
                    });
                }
            }
            Err(error) => tracing::warn!(%error, key_id, "could not seed admission minute window"),
        }
        match daily {
            Ok(spend) => ledger.daily_spend = spend.max(0.0),
            Err(error) => tracing::warn!(%error, key_id, "could not seed daily admission spend"),
        }
        match monthly {
            Ok(spend) => ledger.monthly_spend = spend.max(0.0),
            Err(error) => tracing::warn!(%error, key_id, "could not seed monthly admission spend"),
        }
        ledger.prune(instant_now);
        entry.initialized.store(true, Ordering::Release);
    }

    pub async fn reserve(
        &self,
        pool: &Pool,
        snapshot: &Snapshot,
        key: &VirtualKeyRow,
        req: &InternalRequest,
    ) -> Result<AdmissionReservation, ProxyError> {
        let estimate = estimate_request(snapshot, key, req)?;
        let entry = self.entry(&key.id);
        self.ensure_initialized(pool, &key.id, &entry).await;
        self.reserve_initialized(entry, key, estimate)
    }

    pub async fn check_current(&self, pool: &Pool, key: &VirtualKeyRow) -> Result<(), ProxyError> {
        let entry = self.entry(&key.id);
        self.ensure_initialized(pool, &key.id, &entry).await;
        let result = entry
            .ledger
            .lock()
            .check_current(key, Utc::now(), Instant::now());
        result
    }

    fn reserve_initialized(
        &self,
        entry: Arc<KeyAdmission>,
        key: &VirtualKeyRow,
        estimate: AdmissionEstimate,
    ) -> Result<AdmissionReservation, ProxyError> {
        let id = self
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        entry
            .ledger
            .lock()
            .reserve(id, key, estimate, Utc::now(), Instant::now())?;
        Ok(AdmissionReservation {
            entry,
            id,
            settled: false,
        })
    }
}

impl KeyLedger {
    fn roll_periods(&mut self, now: chrono::DateTime<Utc>) {
        let day = now.date_naive();
        if self.daily_day != Some(day) {
            self.daily_day = Some(day);
            self.daily_spend = 0.0;
        }
        let month = (now.year(), now.month());
        if self.monthly_key != Some(month) {
            self.monthly_key = Some(month);
            self.monthly_spend = 0.0;
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .minute
            .front()
            .map(|entry| now.saturating_duration_since(entry.at) >= MINUTE_WINDOW)
            .unwrap_or(false)
        {
            self.minute.pop_front();
        }
    }

    fn current(&mut self, now_wall: chrono::DateTime<Utc>, now: Instant) -> (u64, u64, f64, f64) {
        self.roll_periods(now_wall);
        self.prune(now);
        let day = now_wall.date_naive();
        let month = (now_wall.year(), now_wall.month());

        let mut requests = self.minute.len() as u64;
        let mut tokens: u64 = self.minute.iter().map(|entry| entry.tokens).sum();
        let mut daily = self.daily_spend;
        let mut monthly = self.monthly_spend;

        for active in self.active.values() {
            if now.saturating_duration_since(active.at) < MINUTE_WINDOW {
                requests = requests.saturating_add(1);
                tokens = tokens.saturating_add(active.tokens);
            }
            if let Some(cost) = active.cost {
                if active.day == day {
                    daily += cost;
                }
                if active.month == month {
                    monthly += cost;
                }
            }
        }
        (requests, tokens, daily, monthly)
    }

    fn check_current(
        &mut self,
        key: &VirtualKeyRow,
        now_wall: chrono::DateTime<Utc>,
        now: Instant,
    ) -> Result<(), ProxyError> {
        let (requests, tokens, daily, monthly) = self.current(now_wall, now);
        if let Some(rpm) = key.rpm_limit.filter(|value| *value > 0) {
            if requests >= rpm as u64 {
                return Err(ProxyError::rate_limited(
                    format!("rate limit exceeded: {rpm} requests per minute"),
                    Some(60),
                ));
            }
        }
        if let Some(tpm) = key.tpm_limit.filter(|value| *value > 0) {
            if tokens >= tpm as u64 {
                return Err(ProxyError::rate_limited(
                    format!("token rate limit exceeded: {tpm} tokens per minute"),
                    Some(60),
                ));
            }
        }
        if let Some(limit) = key.daily_budget.filter(|value| *value > 0.0) {
            if daily >= limit {
                return Err(ProxyError::budget_exceeded(format!(
                    "daily budget exceeded (USD {} of USD {}); resets at 00:00 UTC",
                    crate::cost::format_usd(daily),
                    crate::cost::format_usd(limit),
                )));
            }
        }
        if let Some(limit) = key.monthly_budget.filter(|value| *value > 0.0) {
            if monthly >= limit {
                return Err(ProxyError::budget_exceeded(format!(
                    "monthly budget exceeded (USD {} of USD {}); resets on the 1st",
                    crate::cost::format_usd(monthly),
                    crate::cost::format_usd(limit),
                )));
            }
        }
        Ok(())
    }

    fn reserve(
        &mut self,
        id: u64,
        key: &VirtualKeyRow,
        estimate: AdmissionEstimate,
        now_wall: chrono::DateTime<Utc>,
        now: Instant,
    ) -> Result<(), ProxyError> {
        let (requests, tokens, daily, monthly) = self.current(now_wall, now);

        if let Some(rpm) = key.rpm_limit.filter(|value| *value > 0) {
            if requests.saturating_add(1) > rpm as u64 {
                return Err(ProxyError::rate_limited(
                    format!("rate limit exceeded: {rpm} requests per minute"),
                    Some(60),
                ));
            }
        }
        if let Some(tpm) = key.tpm_limit.filter(|value| *value > 0) {
            if tokens.saturating_add(estimate.tokens) > tpm as u64 {
                return Err(ProxyError::rate_limited(
                    format!(
                        "token rate limit exceeded: reserving {} tokens would exceed {tpm} tokens per minute",
                        estimate.tokens
                    ),
                    Some(60),
                ));
            }
        }
        if let Some(cost) = estimate.cost {
            if let Some(limit) = key.daily_budget.filter(|value| *value > 0.0) {
                if daily + cost > limit {
                    return Err(ProxyError::budget_exceeded(format!(
                        "daily budget would be exceeded (USD {} reserved/spent + USD {} request > USD {}); resets at 00:00 UTC",
                        crate::cost::format_usd(daily),
                        crate::cost::format_usd(cost),
                        crate::cost::format_usd(limit),
                    )));
                }
            }
            if let Some(limit) = key.monthly_budget.filter(|value| *value > 0.0) {
                if monthly + cost > limit {
                    return Err(ProxyError::budget_exceeded(format!(
                        "monthly budget would be exceeded (USD {} reserved/spent + USD {} request > USD {}); resets on the 1st",
                        crate::cost::format_usd(monthly),
                        crate::cost::format_usd(cost),
                        crate::cost::format_usd(limit),
                    )));
                }
            }
        }

        self.active.insert(
            id,
            ActiveReservation {
                at: now,
                tokens: estimate.tokens,
                cost: estimate.cost,
                day: now_wall.date_naive(),
                month: (now_wall.year(), now_wall.month()),
            },
        );
        Ok(())
    }

    fn reconcile(
        &mut self,
        id: u64,
        actual_tokens: Option<u64>,
        actual_cost: Option<f64>,
        now_wall: chrono::DateTime<Utc>,
        now: Instant,
    ) {
        self.roll_periods(now_wall);
        self.prune(now);
        let Some(active) = self.active.remove(&id) else {
            return;
        };

        let tokens = actual_tokens.unwrap_or(active.tokens);
        if now.saturating_duration_since(active.at) < MINUTE_WINDOW {
            self.minute.push_back(MinuteUse {
                at: active.at,
                tokens,
            });
        }

        let cost = actual_cost.or(active.cost);
        if let Some(cost) = cost {
            if self.daily_day == Some(active.day) {
                self.daily_spend += cost;
            }
            if self.monthly_key == Some(active.month) {
                self.monthly_spend += cost;
            }
        }
    }

    fn cancel(&mut self, id: u64) {
        self.active.remove(&id);
    }
}

fn estimated_input_tokens(req: &InternalRequest) -> u64 {
    let tool_chars: u64 = req
        .tools
        .iter()
        .map(|tool| {
            tool.name.len() as u64
                + tool.description.as_deref().map(str::len).unwrap_or(0) as u64
                + tool.parameters.to_string().len() as u64
                + 32
        })
        .sum();
    req.approx_input_tokens()
        .saturating_add(tool_chars.div_ceil(4))
        .max(1)
}

fn output_reservation(req: &InternalRequest, model: &ModelRow) -> u64 {
    let model_max = model
        .max_output_tokens
        .filter(|value| *value > 0)
        .map(|value| value as u64);
    match (req.params.max_tokens.map(u64::from), model_max) {
        (Some(requested), Some(maximum)) => requested.min(maximum),
        (Some(requested), None) => requested,
        (None, Some(maximum)) => maximum,
        (None, None) => DEFAULT_OUTPUT_RESERVATION,
    }
}

fn conservative_cost(prices: &Prices, input: u64, output: u64) -> Option<f64> {
    if !prices.is_configured() {
        return None;
    }
    let input_base = prices.input_per_1m.unwrap_or(0.0);
    let input_rate = input_base
        .max(prices.cached_per_1m.unwrap_or(input_base))
        .max(prices.cache_write_per_1m.unwrap_or(input_base));
    let output_base = prices.output_per_1m.unwrap_or(0.0);
    let output_rate = output_base.max(prices.thinking_per_1m.unwrap_or(output_base));
    Some((input as f64 * input_rate + output as f64 * output_rate) / 1_000_000.0)
}

pub fn estimate_request(
    snapshot: &Snapshot,
    key: &VirtualKeyRow,
    req: &InternalRequest,
) -> Result<AdmissionEstimate, ProxyError> {
    let resolved = Registry::resolve_in(snapshot, &req.requested_model).ok_or_else(|| {
        ProxyError::not_found(format!(
            "model '{}' is not configured. Use GET /v1/models to list available models.",
            req.requested_model
        ))
    })?;
    let allowed_providers = key.allowed_providers();
    let input = estimated_input_tokens(req);

    let mut models = Vec::new();
    let mut seen = HashSet::new();
    match resolved {
        Resolved::Single {
            provider_id,
            model_id,
        } => {
            if !allowed_providers.is_empty() && !allowed_providers.contains(&provider_id) {
                return Err(ProxyError::new(
                    crate::types::ErrorKind::Forbidden,
                    "this key is not allowed to use the resolved provider",
                ));
            }
            if let Some(model) = snapshot.models.get(&model_id) {
                models.push(model.clone());
            }
        }
        Resolved::Route { targets, .. } => {
            for target in &targets {
                if !allowed_providers.is_empty() && !allowed_providers.contains(&target.provider.id)
                {
                    continue;
                }
                if seen.insert(target.model.id.clone()) {
                    models.push(target.model.clone());
                }
            }
            if models.is_empty() {
                return Err(ProxyError::new(
                    crate::types::ErrorKind::Forbidden,
                    "this key is not allowed to use any provider in the resolved route",
                ));
            }
        }
    }

    let mut tokens = 0u64;
    let mut max_cost = 0.0f64;
    let mut cost_known = true;
    for model in models {
        let output = output_reservation(req, &model);
        tokens = tokens.max(input.saturating_add(output));
        match conservative_cost(&model.prices(), input, output) {
            Some(cost) => max_cost = max_cost.max(cost),
            None => cost_known = false,
        }
    }

    Ok(AdmissionEstimate {
        tokens: tokens.max(input),
        cost: cost_known.then_some(max_cost),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> VirtualKeyRow {
        VirtualKeyRow {
            id: "key".into(),
            key_hash: "hash".into(),
            name: "test".into(),
            owner: String::new(),
            tag: String::new(),
            allowed_models: "[\"*\"]".into(),
            allowed_providers: "[]".into(),
            rpm_limit: None,
            tpm_limit: None,
            daily_budget: None,
            monthly_budget: None,
            expires_at: None,
            status: "active".into(),
            allowed_ips: "[]".into(),
            body_logging: 0,
            created_at: String::new(),
            revoked_at: None,
        }
    }

    fn initialized_controller() -> (AdmissionController, Arc<KeyAdmission>) {
        let controller = AdmissionController::default();
        let entry = controller.entry("key");
        entry.initialized.store(true, Ordering::Release);
        (controller, entry)
    }

    fn burst(
        controller: AdmissionController,
        entry: Arc<KeyAdmission>,
        key: VirtualKeyRow,
        estimate: AdmissionEstimate,
        count: usize,
    ) -> Vec<AdmissionReservation> {
        let barrier = Arc::new(std::sync::Barrier::new(count + 1));
        let mut handles = Vec::new();
        for _ in 0..count {
            let controller = controller.clone();
            let entry = entry.clone();
            let key = key.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                controller.reserve_initialized(entry, &key, estimate)
            }));
        }
        barrier.wait();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap().ok())
            .collect()
    }

    #[test]
    fn concurrent_rpm_burst_admits_only_capacity() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.rpm_limit = Some(4);
        let reservations = burst(
            controller,
            entry,
            key,
            AdmissionEstimate {
                tokens: 1,
                cost: Some(0.0),
            },
            32,
        );
        assert_eq!(reservations.len(), 4);
    }

    #[test]
    fn concurrent_tpm_burst_reserves_estimated_tokens_atomically() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.tpm_limit = Some(100);
        let reservations = burst(
            controller,
            entry,
            key,
            AdmissionEstimate {
                tokens: 30,
                cost: Some(0.0),
            },
            16,
        );
        assert_eq!(reservations.len(), 3);
    }

    #[test]
    fn concurrent_budget_burst_reserves_spend_atomically() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.daily_budget = Some(1.0);
        let reservations = burst(
            controller,
            entry,
            key,
            AdmissionEstimate {
                tokens: 1,
                cost: Some(0.4),
            },
            16,
        );
        assert_eq!(reservations.len(), 2);
    }

    #[test]
    fn reconciliation_replaces_estimate_with_complete_usage() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.tpm_limit = Some(100);
        let reservation = controller
            .reserve_initialized(
                entry.clone(),
                &key,
                AdmissionEstimate {
                    tokens: 80,
                    cost: Some(0.8),
                },
            )
            .unwrap();
        reservation.reconcile(
            &TokenUsage {
                input: Some(10),
                output: Some(10),
                ..Default::default()
            },
            Some(0.2),
        );
        let second = controller.reserve_initialized(
            entry,
            &key,
            AdmissionEstimate {
                tokens: 70,
                cost: Some(0.1),
            },
        );
        assert!(second.is_ok());
    }

    #[test]
    fn incomplete_usage_keeps_conservative_reservation() {
        let (controller, entry) = initialized_controller();
        let mut key = key();
        key.tpm_limit = Some(100);
        let reservation = controller
            .reserve_initialized(
                entry.clone(),
                &key,
                AdmissionEstimate {
                    tokens: 80,
                    cost: Some(0.8),
                },
            )
            .unwrap();
        reservation.reconcile(
            &TokenUsage {
                input: Some(10),
                output: None,
                ..Default::default()
            },
            None,
        );
        let second = controller.reserve_initialized(
            entry,
            &key,
            AdmissionEstimate {
                tokens: 30,
                cost: Some(0.1),
            },
        );
        assert!(second.is_err());
    }
}
