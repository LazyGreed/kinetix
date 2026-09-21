//! Key-pool health state and account/soft-quota bookkeeping (FR-4, FR-12).
//!
//! Selection is health-aware: disabled/cooldown/exhausted accounts are skipped,
//! and selection order depends on the route strategy. State changes are
//! persisted so they survive restarts (open issue: rate-limit state location).

use chrono::{DateTime, Duration, Utc};

use crate::db::{self, AccountRow, Pool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Healthy,
    Cooldown,
    Exhausted,
    Disabled,
    CircuitOpen,
}

impl AccountStatus {
    pub fn parse(s: &str) -> Self {
        match s {
            "cooldown" => AccountStatus::Cooldown,
            "exhausted" => AccountStatus::Exhausted,
            "disabled" => AccountStatus::Disabled,
            "circuit_open" => AccountStatus::CircuitOpen,
            _ => AccountStatus::Healthy,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            AccountStatus::Healthy => "healthy",
            AccountStatus::Cooldown => "cooldown",
            AccountStatus::Exhausted => "exhausted",
            AccountStatus::Disabled => "disabled",
            AccountStatus::CircuitOpen => "circuit_open",
        }
    }
}

/// Effective status accounting for elapsed cooldown/quota-reset/circuit windows.
pub fn effective_status(account: &AccountRow) -> AccountStatus {
    let now = Utc::now();
    let configured = AccountStatus::parse(&account.status);

    // Administrative/quota state always wins over circuit state. Half-open
    // probing is only a circuit-breaker recovery mechanism; it must never make
    // disabled, cooling-down, or exhausted credentials eligible.
    match configured {
        AccountStatus::Disabled => return AccountStatus::Disabled,
        AccountStatus::Cooldown => match account.cooldown_until.as_deref().and_then(db::parse_dt) {
            Some(until) if until <= now => {}
            _ => return AccountStatus::Cooldown,
        },
        AccountStatus::Exhausted => {
            match account.quota_reset_at.as_deref().and_then(db::parse_dt) {
                Some(reset) if reset <= now => {}
                _ => return AccountStatus::Exhausted,
            }
        }
        AccountStatus::Healthy | AccountStatus::CircuitOpen => {}
    }

    // A circuit remains logically open after its timer elapses until a
    // half-open probe succeeds and clears it.
    if account.circuit_open_until.is_some() || configured == AccountStatus::CircuitOpen {
        AccountStatus::CircuitOpen
    } else {
        AccountStatus::Healthy
    }
}

/// Bump the failure counter and open the circuit breaker when the threshold is
/// reached (FR-4.7). Returns the new consecutive-failure count.
pub async fn record_failure(
    pool: &Pool,
    account_id: &str,
    threshold: i64,
    open_secs: i64,
) -> anyhow::Result<i64> {
    db::record_account_failure(pool, account_id, threshold, open_secs).await
}

/// Clear the circuit breaker after a successful attempt or manual reset.
pub async fn clear_circuit(pool: &Pool, account_id: &str) -> anyhow::Result<()> {
    db::reset_account_failures(pool, account_id).await
}

/// Default interval between half-open recovery probes (FR-4.7, SHOULD).
pub const HALF_OPEN_PROBE_SECS: i64 = 5;

/// Minimum spacing between successive half-open probes for one account.
pub const HALF_OPEN_PROBE_MIN_GAP_SECS: i64 = 2;

/// Bounded half-open recovery probing (FR-4.7): once an account's circuit-open
/// window has elapsed it may be tried again, but at most one probe per
/// [`HALF_OPEN_PROBE_MIN_GAP_SECS`] so a still-broken upstream is not hammered
/// by every concurrent request. Returns `false` while the circuit is still open.
pub fn should_probe(account: &AccountRow) -> bool {
    // Only circuit-breaker state is probeable. Other non-healthy states have
    // their own explicit recovery/reset conditions.
    if matches!(
        AccountStatus::parse(&account.status),
        AccountStatus::Disabled | AccountStatus::Cooldown | AccountStatus::Exhausted
    ) {
        return false;
    }

    let Some(until) = account.circuit_open_until.as_deref().and_then(db::parse_dt) else {
        return false;
    };
    if Utc::now() < until {
        return false; // circuit still open: do not probe
    }
    match account.last_probe_at.as_deref().and_then(db::parse_dt) {
        Some(last) => Utc::now() >= last + Duration::seconds(HALF_OPEN_PROBE_MIN_GAP_SECS),
        None => true,
    }
}

pub fn is_available(account: &AccountRow) -> bool {
    effective_status(account) == AccountStatus::Healthy
}

/// Order accounts by priority while using account weight to choose the order
/// within each priority tier. Sampling is without replacement so every account
/// remains available for fallback while higher-weight accounts lead more often.
pub fn order_accounts(mut accounts: Vec<AccountRow>, preferred: Option<&str>) -> Vec<AccountRow> {
    use rand::Rng;

    accounts.sort_by_key(|a| a.priority);

    let mut rng = rand::thread_rng();
    let mut start = 0usize;
    while start < accounts.len() {
        let priority = accounts[start].priority;
        let mut end = start + 1;
        while end < accounts.len() && accounts[end].priority == priority {
            end += 1;
        }

        for i in start..end {
            let total: i64 = accounts[i..end].iter().map(|a| a.weight.max(1)).sum();
            let mut pick = rng.gen_range(0..total);
            let mut selected = i;
            for (offset, account) in accounts[i..end].iter().enumerate() {
                pick -= account.weight.max(1);
                if pick < 0 {
                    selected = i + offset;
                    break;
                }
            }
            accounts.swap(i, selected);
        }
        start = end;
    }

    if let Some(preferred) = preferred {
        if let Some(pos) = accounts.iter().position(|a| a.id == preferred) {
            let account = accounts.remove(pos);
            accounts.insert(0, account);
        }
    }

    accounts
}

/// Mark an account rate-limited for `cooldown` seconds.
pub async fn mark_rate_limited(
    pool: &Pool,
    account_id: &str,
    cooldown_secs: u64,
    error: &str,
) -> anyhow::Result<DateTime<Utc>> {
    let until = Utc::now() + Duration::seconds(cooldown_secs as i64);
    db::set_account_status(
        pool,
        account_id,
        "cooldown",
        Some(&until.to_rfc3339()),
        None,
        Some(&crate::crypto::redact(error)),
    )
    .await?;
    Ok(until)
}

/// Mark an account quota-exhausted until `reset_at` (or a default window).
pub async fn mark_exhausted(
    pool: &Pool,
    account_id: &str,
    reset_at: Option<DateTime<Utc>>,
    default_window_secs: i64,
    error: &str,
) -> anyhow::Result<DateTime<Utc>> {
    let reset = reset_at.unwrap_or_else(|| Utc::now() + Duration::seconds(default_window_secs));
    db::set_account_status(
        pool,
        account_id,
        "exhausted",
        None,
        Some(&reset.to_rfc3339()),
        Some(&crate::crypto::redact(error)),
    )
    .await?;
    Ok(reset)
}

pub async fn mark_healthy(pool: &Pool, account_id: &str) -> anyhow::Result<()> {
    db::set_account_status(pool, account_id, "healthy", None, None, None).await
}

/// Clear a cooldown and put the account back in service.
pub async fn clear_cooldown(pool: &Pool, account_id: &str) -> anyhow::Result<()> {
    db::set_account_status(pool, account_id, "healthy", None, None, None).await
}

/// The soonest recovery time across a set of accounts (for `Retry-After`).
pub fn soonest_recovery(accounts: &[AccountRow]) -> Option<DateTime<Utc>> {
    let mut soonest: Option<DateTime<Utc>> = None;
    for a in accounts {
        let t = match AccountStatus::parse(&a.status) {
            AccountStatus::Cooldown => a.cooldown_until.as_deref().and_then(db::parse_dt),
            AccountStatus::Exhausted => a.quota_reset_at.as_deref().and_then(db::parse_dt),
            _ => None,
        }
        .or_else(|| a.circuit_open_until.as_deref().and_then(db::parse_dt));
        if let Some(t) = t {
            soonest = Some(match soonest {
                Some(cur) if cur < t => cur,
                _ => t,
            });
        }
    }
    soonest
}

/// Whether a soft quota (FR-12.5) is reached for an account.
pub async fn soft_quota_reached(pool: &Pool, account: &AccountRow) -> anyhow::Result<bool> {
    let Some(limit) = account.soft_quota_usd else {
        return Ok(false);
    };
    if limit <= 0.0 {
        return Ok(false);
    }
    let since = window_start(&account.quota_type, account.quota_window_s);
    let spent = db::account_spend_since(pool, &account.id, &since).await?;
    Ok(spent >= limit)
}

/// Compute the start of a quota window as an ISO timestamp.
pub fn window_start(quota_type: &str, window_secs: Option<i64>) -> String {
    let now = Utc::now();
    let start = match quota_type {
        "daily" => now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|d| d.and_utc())
            .unwrap_or(now),
        "monthly" => {
            let first = now.date_naive().with_day(1).unwrap_or(now.date_naive());
            first
                .and_hms_opt(0, 0, 0)
                .map(|d| d.and_utc())
                .unwrap_or(now)
        }
        "rolling" => now - Duration::seconds(window_secs.unwrap_or(86400)),
        _ => now - Duration::days(1),
    };
    start.to_rfc3339()
}

use chrono::Datelike;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn account(
        status: &str,
        circuit_until: Option<String>,
        last_probe: Option<String>,
    ) -> AccountRow {
        AccountRow {
            id: "acc".into(),
            provider_id: "p".into(),
            label: "l".into(),
            secret_enc: String::new(),
            key_mask: String::new(),
            status: status.into(),
            cooldown_until: None,
            quota_reset_at: None,
            quota_type: "none".into(),
            quota_window_s: None,
            soft_quota_usd: None,
            priority: 1,
            weight: 1,
            last_error: None,
            last_probe_at: last_probe,
            circuit_open_until: circuit_until,
            consecutive_failures: 0,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn circuit_open_takes_precedence_and_expires() {
        let future = (Utc::now() + Duration::seconds(60)).to_rfc3339();
        let a = account("healthy", Some(future), None);
        assert_eq!(effective_status(&a), AccountStatus::CircuitOpen);
        assert!(!should_probe(&a), "open circuit must not be probed");

        // Once the window has elapsed the circuit is half-open and probeable.
        let past = (Utc::now() - Duration::seconds(1)).to_rfc3339();
        let b = account("healthy", Some(past), None);
        assert_eq!(effective_status(&b), AccountStatus::CircuitOpen);
        assert!(should_probe(&b), "half-open circuit should be probed");
    }

    #[test]
    fn probe_is_throttled_by_min_gap() {
        let past = (Utc::now() - Duration::seconds(30)).to_rfc3339();
        let just_probed = Utc::now().to_rfc3339();
        let a = account("healthy", Some(past.clone()), Some(just_probed));
        assert!(
            !should_probe(&a),
            "a recent probe must throttle the next one"
        );

        let old = (Utc::now() - Duration::seconds(HALF_OPEN_PROBE_MIN_GAP_SECS + 1)).to_rfc3339();
        let b = account("healthy", Some(past), Some(old));
        assert!(should_probe(&b));
    }

    #[test]
    fn no_circuit_history_is_healthy_but_not_a_probe() {
        let a = account("healthy", None, None);
        assert_eq!(effective_status(&a), AccountStatus::Healthy);
        assert!(!should_probe(&a));
    }

    #[test]
    fn non_circuit_states_are_not_half_open_probes() {
        for status in ["cooldown", "exhausted", "disabled"] {
            let a = account(status, None, None);
            assert_eq!(effective_status(&a), AccountStatus::parse(status));
            assert!(!should_probe(&a), "{status} must not be probeable");
        }
    }

    #[test]
    fn disabled_state_beats_an_expired_circuit() {
        let past = (Utc::now() - Duration::seconds(30)).to_rfc3339();
        let a = account("disabled", Some(past), None);
        assert_eq!(effective_status(&a), AccountStatus::Disabled);
        assert!(!should_probe(&a));
    }

    #[test]
    fn expired_cooldown_recovers_without_half_open_probe() {
        let mut a = account("cooldown", None, None);
        a.cooldown_until = Some((Utc::now() - Duration::seconds(1)).to_rfc3339());
        assert_eq!(effective_status(&a), AccountStatus::Healthy);
        assert!(!should_probe(&a));
    }

    #[test]
    fn account_weight_biases_first_choice_within_priority_tier() {
        let mut light = account("healthy", None, None);
        light.id = "light".into();
        light.weight = 1;

        let mut heavy = account("healthy", None, None);
        heavy.id = "heavy".into();
        heavy.weight = 9;

        let mut heavy_first = 0usize;
        for _ in 0..2000 {
            let ordered = order_accounts(vec![light.clone(), heavy.clone()], None);
            if ordered[0].id == "heavy" {
                heavy_first += 1;
            }
        }

        assert!(
            heavy_first > 1500,
            "weight 9 account should lead most selections, got {heavy_first}/2000"
        );
    }
}
