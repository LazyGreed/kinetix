//! Cost computation from admin-supplied prices (FR-6.3).
//!
//! No vendor prices are bundled. Models with no price configured produce
//! `None` (unknown), never `0.0`.

use crate::types::{Prices, TokenUsage};

/// Compute USD cost for a request. Returns `None` when prices are not configured.
/// Format a USD amount for human-facing messages, keeping precision for tiny
/// amounts so a budget of $0.0001 does not read as "0.00".
pub fn format_usd(v: f64) -> String {
    if v != 0.0 && v.abs() < 0.01 {
        format!("{v:.6}")
    } else {
        format!("{v:.2}")
    }
}

pub fn compute_cost(prices: &Prices, usage: &TokenUsage) -> Option<f64> {
    if !prices.is_configured() {
        return None;
    }
    let input = usage.input.unwrap_or(0) as f64;
    let cached = usage.cached.unwrap_or(0) as f64;
    let cache_write = usage.cache_write.unwrap_or(0) as f64;
    let output = usage.output.unwrap_or(0) as f64;
    let thinking = usage.thinking.unwrap_or(0) as f64;

    // Canonical input/output are inclusive totals. Breakdown dimensions must be
    // subtracted before their provider-specific rates are applied so no token
    // is charged twice.
    let regular_input = (input - cached - cache_write).max(0.0);
    let regular_output = (output - thinking).max(0.0);

    let input_price = prices.input_per_1m.unwrap_or(0.0);
    let cached_price = prices.cached_per_1m.unwrap_or(input_price);
    let cache_write_price = prices.cache_write_per_1m.unwrap_or(input_price);
    let output_price = prices.output_per_1m.unwrap_or(0.0);
    let thinking_price = prices.thinking_per_1m.unwrap_or(output_price);

    let cost = (regular_input * input_price
        + cached * cached_price
        + cache_write * cache_write_price
        + regular_output * output_price
        + thinking * thinking_price)
        / 1_000_000.0;

    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_when_unpriced() {
        let p = Prices::default();
        let u = TokenUsage {
            input: Some(100),
            output: Some(50),
            ..Default::default()
        };
        assert!(compute_cost(&p, &u).is_none());
    }

    #[test]
    fn prices_inclusive_totals_without_double_charging_breakdowns() {
        let p = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            cached_per_1m: Some(0.1),
            cache_write_per_1m: Some(1.25),
            thinking_per_1m: Some(3.0),
        };
        let u = TokenUsage {
            input: Some(1_000_000),
            output: Some(1_000_000),
            cached: Some(200_000),
            cache_write: Some(100_000),
            thinking: Some(250_000),
        };
        // 700k*1 + 200k*0.1 + 100k*1.25 + 750k*2 + 250k*3 = 3.095
        let cost = compute_cost(&p, &u).unwrap();
        assert!((cost - 3.095).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn openai_reasoning_breakdown_is_not_added_to_completion_total() {
        let p = Prices {
            input_per_1m: Some(1.0),
            output_per_1m: Some(2.0),
            cached_per_1m: None,
            cache_write_per_1m: None,
            thinking_per_1m: None,
        };
        let u = TokenUsage {
            input: Some(0),
            output: Some(1_000_000),
            thinking: Some(250_000),
            ..Default::default()
        };
        assert!((compute_cost(&p, &u).unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn formats_tiny_amounts_with_precision() {
        assert_eq!(format_usd(0.0001), "0.000100");
        assert_eq!(format_usd(2.5), "2.50");
        assert_eq!(format_usd(0.0), "0.00");
    }
}
