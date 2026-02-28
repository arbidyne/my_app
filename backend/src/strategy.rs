//! Strategy trait and built-in implementations.
//!
//! Strategies receive market data callbacks and return [`Signal`]s that the
//! [`runner`](crate::runner) converts into order requests. All callbacks are
//! synchronous `&mut self` — the async runtime drives them.

use crate::{BarData, ContractConfig};

/// Instruction emitted by a strategy callback.
#[derive(Clone, Debug, PartialEq)]
pub enum Signal {
    PlaceOrder {
        action: SignalAction,
        order_type: SignalOrderType,
        quantity: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum SignalAction {
    Buy,
    Sell,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SignalOrderType {
    Market,
    Limit { price: f64 },
    Stop { price: f64 },
}

/// Read-only snapshot of contract state built by the runner before each callback.
pub struct StrategyContext {
    pub symbol: String,
    pub position_size: f64,
    pub average_cost: f64,
    pub config: ContractConfig,
}

/// Core strategy interface. Implement this to define trading logic.
pub trait Strategy: Send + Sync {
    fn name(&self) -> &str;

    fn on_tick(
        &mut self,
        _ctx: &StrategyContext,
        _last_price: f64,
        _best_bid: f64,
        _best_ask: f64,
        _timestamp: u64,
    ) -> Vec<Signal> {
        Vec::new()
    }

    fn on_bar(&mut self, _ctx: &StrategyContext, _bar: &BarData) -> Vec<Signal> {
        Vec::new()
    }

    fn on_fill(
        &mut self,
        _ctx: &StrategyContext,
        _action: &str,
        _filled_qty: f64,
        _fill_price: f64,
    ) -> Vec<Signal> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Built-in strategies
// ---------------------------------------------------------------------------

/// Default strategy that never emits signals.
pub struct NoopStrategy;

impl Strategy for NoopStrategy {
    fn name(&self) -> &str {
        "none"
    }
}

/// Test strategy: buys 1 lot on the first bar when flat, then stops.
pub struct PingStrategy {
    fired: bool,
}

impl PingStrategy {
    pub fn new() -> Self {
        Self { fired: false }
    }
}

impl Strategy for PingStrategy {
    fn name(&self) -> &str {
        "ping"
    }

    fn on_bar(&mut self, ctx: &StrategyContext, _bar: &BarData) -> Vec<Signal> {
        if self.fired || ctx.position_size != 0.0 {
            return Vec::new();
        }
        self.fired = true;
        vec![Signal::PlaceOrder {
            action: SignalAction::Buy,
            order_type: SignalOrderType::Market,
            quantity: ctx.config.lot_size as f64,
        }]
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

pub fn create_strategy(name: &str) -> Option<Box<dyn Strategy>> {
    match name {
        "none" => Some(Box::new(NoopStrategy)),
        "ping" => Some(Box::new(PingStrategy::new())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(position_size: f64) -> StrategyContext {
        StrategyContext {
            symbol: "TEST".to_string(),
            position_size,
            average_cost: 0.0,
            config: ContractConfig {
                symbol: "TEST".to_string(),
                autotrade: true,
                max_pos_size: 100,
                min_pos_size: 0,
                max_order_size: 100,
                multiplier: 1.0,
                lot_size: 1,
                strategy: "none".to_string(),
            },
        }
    }

    fn dummy_bar() -> BarData {
        BarData {
            timestamp: 1_000_000,
            open: 10.0,
            high: 11.0,
            low: 9.0,
            close: 10.5,
            volume: 100.0,
        }
    }

    #[test]
    fn noop_strategy_emits_no_signals() {
        let mut s = NoopStrategy;
        let ctx = test_ctx(0.0);
        assert!(s.on_tick(&ctx, 10.0, 9.9, 10.1, 1000).is_empty());
        assert!(s.on_bar(&ctx, &dummy_bar()).is_empty());
        assert!(s.on_fill(&ctx, "BUY", 1.0, 10.0).is_empty());
    }

    #[test]
    fn ping_strategy_fires_once_on_first_bar() {
        let mut s = PingStrategy::new();
        let ctx = test_ctx(0.0);
        let signals = s.on_bar(&ctx, &dummy_bar());
        assert_eq!(signals.len(), 1);
        assert_eq!(
            signals[0],
            Signal::PlaceOrder {
                action: SignalAction::Buy,
                order_type: SignalOrderType::Market,
                quantity: 1.0,
            }
        );
    }

    #[test]
    fn ping_strategy_does_not_fire_twice() {
        let mut s = PingStrategy::new();
        let ctx = test_ctx(0.0);
        s.on_bar(&ctx, &dummy_bar());
        let signals = s.on_bar(&ctx, &dummy_bar());
        assert!(signals.is_empty());
    }

    #[test]
    fn ping_strategy_does_not_fire_when_positioned() {
        let mut s = PingStrategy::new();
        let ctx = test_ctx(5.0);
        let signals = s.on_bar(&ctx, &dummy_bar());
        assert!(signals.is_empty());
    }

    #[test]
    fn create_strategy_returns_none_for_unknown() {
        assert!(create_strategy("nonexistent").is_none());
    }

    #[test]
    fn create_strategy_returns_known_strategies() {
        assert!(create_strategy("none").is_some());
        assert!(create_strategy("ping").is_some());
    }
}
