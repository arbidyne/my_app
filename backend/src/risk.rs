//! Pre-trade risk checks.
//!
//! Pure validation that runs before any order reaches IBKR. Zero values for
//! `max_order_size` and `max_pos_size` mean "zero tolerance" (reject everything),
//! not "disabled". Set large values to effectively disable a limit.
//! `min_pos_size=0` means "no minimum" (disabled).

use crate::{ContractConfig, TradingState};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;

/// Sliding-window rate limiter that triggers a kill switch when too many orders
/// arrive within a time window. Accepts `now: Instant` for deterministic testing.
pub struct OrderRateLimiter {
    timestamps: VecDeque<Instant>,
    max_orders: usize,
    window: Duration,
}

impl OrderRateLimiter {
    pub fn new(max_orders: usize, window: Duration) -> Self {
        Self {
            timestamps: VecDeque::new(),
            max_orders,
            window,
        }
    }

    /// Records an order and returns `Err` if the rate limit is breached.
    pub fn check(&mut self, now: Instant) -> Result<(), String> {
        // Prune timestamps that have fallen outside the window.
        while let Some(&front) = self.timestamps.front() {
            if now.duration_since(front) > self.window {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }

        self.timestamps.push_back(now);

        if self.timestamps.len() > self.max_orders {
            Err(format!(
                "Order rate limit breached: {} orders in {:?}",
                self.timestamps.len(),
                self.window
            ))
        } else {
            Ok(())
        }
    }
}

/// Returns `true` if the order would reduce (or exactly close) the current position.
fn is_reducing(action: &str, quantity: f64, current_position: f64) -> bool {
    match action.to_uppercase().as_str() {
        "BUY" => current_position < 0.0 && quantity <= current_position.abs(),
        "SELL" => current_position > 0.0 && quantity <= current_position.abs(),
        _ => false,
    }
}

/// Validates an order against risk limits before submission to IBKR.
/// Returns `Ok(())` if the order passes all checks, or `Err(reason)`
/// describing which limit was breached.
pub fn check_risk(
    action: &str,
    quantity: f64,
    config: &ContractConfig,
    current_position: f64,
    trading_state: &TradingState,
) -> Result<(), String> {
    // 0. Global trading state gate
    match trading_state {
        TradingState::Halted => return Err("Trading is halted".to_string()),
        TradingState::ReducingOnly => {
            if !is_reducing(action, quantity, current_position) {
                return Err(format!(
                    "Reducing-only mode: {action} {quantity} would not reduce position {current_position}"
                ));
            }
        }
        TradingState::Active => {}
    }

    // 1. Autotrade gate
    if !config.autotrade {
        return Err("Autotrade is disabled for this contract".to_string());
    }

    // 2. Max order size
    if quantity > config.max_order_size as f64 {
        return Err(format!(
            "Order size {quantity} exceeds max_order_size {}",
            config.max_order_size
        ));
    }

    // 3. Compute resultant position
    let resultant = match action.to_uppercase().as_str() {
        "BUY" => current_position + quantity,
        "SELL" => current_position - quantity,
        other => return Err(format!("Unknown action: {other}")),
    };

    // 4. Max position size
    if resultant.abs() > config.max_pos_size as f64 {
        return Err(format!(
            "Resultant position {resultant} exceeds max_pos_size {}",
            config.max_pos_size
        ));
    }

    // 5. Min position size — closing to zero is always allowed
    if config.min_pos_size != 0
        && resultant.abs() != 0.0
        && resultant.abs() < (config.min_pos_size as f64).abs()
    {
        return Err(format!(
            "Resultant position {resultant} below min_pos_size {}",
            config.min_pos_size
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE: TradingState = TradingState::Active;

    fn config(max_order: u32, max_pos: u32, min_pos: i32) -> ContractConfig {
        ContractConfig {
            symbol: "TEST".to_string(),
            autotrade: true,
            max_pos_size: max_pos,
            min_pos_size: min_pos,
            max_order_size: max_order,
            multiplier: 1.0,
            lot_size: 1,
            strategy: "none".to_string(),
        }
    }

    // --- Existing tests (with trading_state param) ---

    #[test]
    fn autotrade_disabled_rejects() {
        let mut cfg = config(100, 100, 0);
        cfg.autotrade = false;
        let err = check_risk("BUY", 1.0, &cfg, 0.0, &ACTIVE).unwrap_err();
        assert!(err.contains("Autotrade is disabled"), "got: {err}");
    }

    #[test]
    fn autotrade_enabled_passes() {
        let cfg = config(100, 100, 0);
        assert!(check_risk("BUY", 50.0, &cfg, 0.0, &ACTIVE).is_ok());
    }

    #[test]
    fn order_size_within_limit() {
        let cfg = config(100, 1_000_000, 0);
        assert!(check_risk("BUY", 50.0, &cfg, 0.0, &ACTIVE).is_ok());
    }

    #[test]
    fn order_size_exceeds_limit() {
        let cfg = config(10, 1_000_000, 0);
        let err = check_risk("BUY", 50.0, &cfg, 0.0, &ACTIVE).unwrap_err();
        assert!(err.contains("max_order_size"), "got: {err}");
    }

    #[test]
    fn max_order_size_zero_rejects() {
        let cfg = config(0, 1_000_000, 0);
        let err = check_risk("BUY", 1.0, &cfg, 0.0, &ACTIVE).unwrap_err();
        assert!(err.contains("max_order_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_within_limit() {
        let cfg = config(1_000_000, 100, 0);
        assert!(check_risk("BUY", 50.0, &cfg, 30.0, &ACTIVE).is_ok());
    }

    #[test]
    fn resultant_position_exceeds_max_long() {
        let cfg = config(1_000_000, 100, 0);
        let err = check_risk("BUY", 80.0, &cfg, 50.0, &ACTIVE).unwrap_err();
        assert!(err.contains("max_pos_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_exceeds_max_short() {
        let cfg = config(1_000_000, 100, 0);
        let err = check_risk("SELL", 80.0, &cfg, -50.0, &ACTIVE).unwrap_err();
        assert!(err.contains("max_pos_size"), "got: {err}");
    }

    #[test]
    fn max_pos_size_zero_rejects() {
        let cfg = config(1_000_000, 0, 0);
        let err = check_risk("BUY", 1.0, &cfg, 0.0, &ACTIVE).unwrap_err();
        assert!(err.contains("max_pos_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_below_min() {
        let cfg = config(1_000_000, 1_000_000, 10);
        let err = check_risk("BUY", 5.0, &cfg, 0.0, &ACTIVE).unwrap_err();
        assert!(err.contains("min_pos_size"), "got: {err}");
    }

    #[test]
    fn closing_to_zero_allowed() {
        let cfg = config(1_000_000, 1_000_000, 10);
        // Selling full position to reach zero should pass even with min_pos_size set.
        assert!(check_risk("SELL", 50.0, &cfg, 50.0, &ACTIVE).is_ok());
    }

    // --- Trading state tests ---

    #[test]
    fn halted_rejects_all_orders() {
        let cfg = config(100, 100, 0);
        let err = check_risk("BUY", 1.0, &cfg, 0.0, &TradingState::Halted).unwrap_err();
        assert!(err.contains("halted"), "got: {err}");
    }

    #[test]
    fn active_allows_normal_orders() {
        let cfg = config(100, 100, 0);
        assert!(check_risk("BUY", 10.0, &cfg, 0.0, &TradingState::Active).is_ok());
    }

    #[test]
    fn reducing_only_allows_sell_when_long() {
        let cfg = config(100, 100, 0);
        assert!(check_risk("SELL", 5.0, &cfg, 10.0, &TradingState::ReducingOnly).is_ok());
    }

    #[test]
    fn reducing_only_allows_buy_when_short() {
        let cfg = config(100, 100, 0);
        assert!(check_risk("BUY", 5.0, &cfg, -10.0, &TradingState::ReducingOnly).is_ok());
    }

    #[test]
    fn reducing_only_rejects_buy_when_long() {
        let cfg = config(100, 100, 0);
        let err = check_risk("BUY", 5.0, &cfg, 10.0, &TradingState::ReducingOnly).unwrap_err();
        assert!(err.contains("Reducing-only"), "got: {err}");
    }

    #[test]
    fn reducing_only_rejects_sell_when_short() {
        let cfg = config(100, 100, 0);
        let err = check_risk("SELL", 5.0, &cfg, -10.0, &TradingState::ReducingOnly).unwrap_err();
        assert!(err.contains("Reducing-only"), "got: {err}");
    }

    #[test]
    fn reducing_only_rejects_when_flat() {
        let cfg = config(100, 100, 0);
        let err = check_risk("BUY", 1.0, &cfg, 0.0, &TradingState::ReducingOnly).unwrap_err();
        assert!(err.contains("Reducing-only"), "got: {err}");
    }

    #[test]
    fn reducing_only_rejects_oversized_close() {
        let cfg = config(100, 100, 0);
        // Position is +5, selling 10 would flip to -5 — not allowed.
        let err = check_risk("SELL", 10.0, &cfg, 5.0, &TradingState::ReducingOnly).unwrap_err();
        assert!(err.contains("Reducing-only"), "got: {err}");
    }

    #[test]
    fn reducing_only_allows_exact_close() {
        let cfg = config(100, 100, 0);
        assert!(check_risk("SELL", 10.0, &cfg, 10.0, &TradingState::ReducingOnly).is_ok());
    }

    // --- Rate limiter tests ---

    #[test]
    fn rate_limit_allows_within_limit() {
        let mut rl = OrderRateLimiter::new(5, Duration::from_secs(2));
        let start = Instant::now();
        for i in 0..5 {
            assert!(
                rl.check(start + Duration::from_millis(i * 100)).is_ok(),
                "order {i} should pass"
            );
        }
    }

    #[test]
    fn rate_limit_breaches_on_sixth() {
        let mut rl = OrderRateLimiter::new(5, Duration::from_secs(2));
        let start = Instant::now();
        for i in 0..5 {
            rl.check(start + Duration::from_millis(i * 100)).unwrap();
        }
        let err = rl
            .check(start + Duration::from_millis(500))
            .unwrap_err();
        assert!(err.contains("rate limit"), "got: {err}");
    }

    #[test]
    fn rate_limit_resets_after_window() {
        let mut rl = OrderRateLimiter::new(5, Duration::from_secs(2));
        let start = Instant::now();
        for i in 0..5 {
            rl.check(start + Duration::from_millis(i * 100)).unwrap();
        }
        // Jump past the window — all old entries should be pruned.
        assert!(rl.check(start + Duration::from_secs(3)).is_ok());
    }

    #[test]
    fn rate_limit_sliding_window() {
        let mut rl = OrderRateLimiter::new(5, Duration::from_secs(2));
        let start = Instant::now();
        // Place 3 orders at t=0, t=100ms, t=200ms.
        for i in 0..3 {
            rl.check(start + Duration::from_millis(i * 100)).unwrap();
        }
        // At t=2.1s the first 3 have expired, so 2 more + the next 3 should fit (5 total).
        let t = start + Duration::from_millis(2100);
        for i in 0..2 {
            assert!(
                rl.check(t + Duration::from_millis(i * 50)).is_ok(),
                "post-expiry order {i} should pass"
            );
        }
        // We now have 2 in window. Add 3 more → 5 total, still ok.
        for i in 0..3 {
            assert!(
                rl.check(t + Duration::from_millis(100 + i * 50)).is_ok(),
                "filling to limit order {i} should pass"
            );
        }
        // 6th in window → breach.
        assert!(rl.check(t + Duration::from_millis(300)).is_err());
    }
}
