//! Simulated order execution for backtest mode.
//!
//! Mirrors the live order pipeline (risk checks, state machine, fills) without
//! any ibapi dependency. Market orders fill at the current bar's close price;
//! limit and stop orders fill when a subsequent bar's OHLC touches their trigger.

use crate::order::{OrderEvent, OrderState, OrderStateMachine};
use crate::risk::{self, OrderRateLimiter};
use crate::{
    build_order_update, BarData, ContractConfig, OrderCommand, OrderStaticFields, OrderUpdate,
    PositionData, ServerMessage, TradingState,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tokio::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;

struct SimOrderEntry {
    sm: OrderStateMachine,
    sf: OrderStaticFields,
}

pub struct SimOrderEngine {
    next_order_id: i32,
    orders: HashMap<i32, SimOrderEntry>,
    client_id_map: HashMap<Uuid, i32>,
    current_price: f64,
    rate_limiter: OrderRateLimiter,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
    trading_state: Arc<RwLock<TradingState>>,
    order_cache: Arc<RwLock<HashMap<i32, OrderUpdate>>>,
    msg_tx: broadcast::Sender<ServerMessage>,
}

impl SimOrderEngine {
    pub fn new(
        position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
        config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
        trading_state: Arc<RwLock<TradingState>>,
        order_cache: Arc<RwLock<HashMap<i32, OrderUpdate>>>,
        msg_tx: broadcast::Sender<ServerMessage>,
    ) -> Self {
        Self {
            next_order_id: 1,
            orders: HashMap::new(),
            client_id_map: HashMap::new(),
            current_price: 0.0,
            rate_limiter: OrderRateLimiter::new(5, Duration::from_secs(2)),
            position_cache,
            config_cache,
            trading_state,
            order_cache,
            msg_tx,
        }
    }

    pub async fn handle_command(&mut self, cmd: OrderCommand) {
        match cmd {
            OrderCommand::Place(req) => self.place_order(req).await,
            OrderCommand::Cancel { client_order_id } => self.cancel_order(client_order_id).await,
            OrderCommand::Modify(req) => self.modify_order(req).await,
        }
    }

    /// Updates current price and checks all working limit/stop orders against
    /// the bar's OHLC range.
    pub async fn on_bar(&mut self, bar: &BarData) {
        self.current_price = bar.close;

        // Collect fills first to avoid borrowing self.orders while mutating.
        let mut fills: Vec<(i32, f64)> = Vec::new();

        for (&order_id, entry) in &self.orders {
            if !matches!(
                entry.sm.state,
                OrderState::Working | OrderState::PartiallyFilled
            ) {
                continue;
            }

            let fill_price = match entry.sf.order_type.as_str() {
                "LMT" => {
                    let limit = entry.sf.limit_price.unwrap_or(0.0);
                    match entry.sf.action.as_str() {
                        "BUY" if bar.low <= limit => Some(limit),
                        "SELL" if bar.high >= limit => Some(limit),
                        _ => None,
                    }
                }
                "STP" => {
                    let stop = entry.sf.stop_price.unwrap_or(0.0);
                    match entry.sf.action.as_str() {
                        "BUY" if bar.high >= stop => Some(stop),
                        "SELL" if bar.low <= stop => Some(stop),
                        _ => None,
                    }
                }
                _ => None,
            };

            if let Some(price) = fill_price {
                fills.push((order_id, price));
            }
        }

        for (order_id, fill_price) in fills {
            self.fill_order(order_id, fill_price).await;
        }
    }

    async fn place_order(&mut self, req: crate::OrderRequest) {
        // Rate limit check
        if let Err(reason) = self.rate_limiter.check(Instant::now()) {
            warn!("SIM rate limit triggered for {}: {reason}", req.symbol);
            *self.trading_state.write().await = TradingState::Halted;
            let _ = self
                .msg_tx
                .send(ServerMessage::TradingState {
                    state: TradingState::Halted,
                });
            self.reject_order(&req, reason).await;
            return;
        }

        // Risk check — scoped so locks drop before proceeding.
        let risk_result = {
            let ts = self.trading_state.read().await;
            let cfg = self.config_cache.read().await;
            let pos = self.position_cache.read().await;
            match cfg.get(&req.symbol) {
                Some(contract_cfg) => {
                    let current = pos
                        .get(&req.symbol)
                        .map(|p| p.position_size)
                        .unwrap_or(0.0);
                    risk::check_risk(&req.action, req.quantity, contract_cfg, current, &ts)
                }
                None => Err("No risk config for this contract".to_string()),
            }
        };
        if let Err(reason) = risk_result {
            warn!("SIM risk check failed for {}: {reason}", req.symbol);
            self.reject_order(&req, reason).await;
            return;
        }

        let order_id = self.next_order_id;
        self.next_order_id += 1;

        let tif = if req.time_in_force.is_empty() {
            "DAY".to_string()
        } else {
            req.time_in_force.clone()
        };
        let sf = OrderStaticFields {
            client_order_id: req.client_order_id,
            symbol: req.symbol.clone(),
            action: req.action.clone(),
            order_type: req.order_type.clone(),
            quantity: req.quantity,
            limit_price: req.limit_price,
            stop_price: req.stop_price,
            time_in_force: tif,
        };

        self.client_id_map.insert(req.client_order_id, order_id);

        let mut sm = OrderStateMachine::new(order_id, req.quantity);
        let _ = sm.apply(OrderEvent::Submit);
        let _ = sm.apply(OrderEvent::AckValid);

        let update = build_order_update(order_id, &sf, &sm);
        self.order_cache
            .write()
            .await
            .insert(order_id, update.clone());
        let _ = self.msg_tx.send(ServerMessage::OrderUpdate(update));

        info!(
            "SIM order #{order_id}: {} {} {} x {} @ {:?}",
            sf.action,
            sf.order_type,
            sf.quantity,
            sf.symbol,
            sf.limit_price.or(sf.stop_price)
        );

        let is_market = req.order_type == "MKT";
        self.orders.insert(order_id, SimOrderEntry { sm, sf });

        if is_market {
            self.fill_order(order_id, self.current_price).await;
        }
    }

    async fn reject_order(&self, req: &crate::OrderRequest, reason: String) {
        let sf = OrderStaticFields {
            client_order_id: req.client_order_id,
            symbol: req.symbol.clone(),
            action: req.action.clone(),
            order_type: req.order_type.clone(),
            quantity: req.quantity,
            limit_price: req.limit_price,
            stop_price: req.stop_price,
            time_in_force: if req.time_in_force.is_empty() {
                "DAY".to_string()
            } else {
                req.time_in_force.clone()
            },
        };
        let mut sm = OrderStateMachine::new(0, req.quantity);
        let _ = sm.apply(OrderEvent::Submit);
        let _ = sm.apply(OrderEvent::AckReject { reason });
        let update = build_order_update(0, &sf, &sm);
        self.order_cache.write().await.insert(0, update.clone());
        let _ = self.msg_tx.send(ServerMessage::OrderUpdate(update));
    }

    async fn cancel_order(&mut self, client_order_id: Uuid) {
        let order_id = match self.client_id_map.get(&client_order_id) {
            Some(&id) => id,
            None => {
                info!("SIM cancel: client_order_id {client_order_id} not found");
                return;
            }
        };

        // Scope the mutable borrow of self.orders so we can use self.order_cache/msg_tx after.
        let update = {
            let entry = match self.orders.get_mut(&order_id) {
                Some(e) => e,
                None => return,
            };
            if entry.sm.state.is_terminal() {
                info!(
                    "SIM order #{order_id}: ignoring cancel — already {}",
                    entry.sm.state.as_str()
                );
                return;
            }
            let _ = entry.sm.apply(OrderEvent::CancelRequested);
            let _ = entry.sm.apply(OrderEvent::CancelConfirmed);
            build_order_update(order_id, &entry.sf, &entry.sm)
        };

        self.order_cache
            .write()
            .await
            .insert(order_id, update.clone());
        let _ = self.msg_tx.send(ServerMessage::OrderUpdate(update));
        info!("SIM order #{order_id}: cancelled");
    }

    async fn modify_order(&mut self, req: crate::ModifyOrderRequest) {
        let order_id = match self.client_id_map.get(&req.client_order_id) {
            Some(&id) => id,
            None => {
                info!(
                    "SIM modify: client_order_id {} not found",
                    req.client_order_id
                );
                return;
            }
        };

        let update = {
            let entry = match self.orders.get_mut(&order_id) {
                Some(e) => e,
                None => return,
            };
            if !matches!(
                entry.sm.state,
                OrderState::Working | OrderState::PartiallyFilled
            ) {
                info!(
                    "SIM order #{order_id}: ignoring modify — state is {}",
                    entry.sm.state.as_str()
                );
                return;
            }

            if entry.sm.apply(OrderEvent::AmendRequested).is_err() {
                return;
            }
            entry.sf.quantity = req.quantity;
            if req.limit_price.is_some() {
                entry.sf.limit_price = req.limit_price;
            }
            if req.stop_price.is_some() {
                entry.sf.stop_price = req.stop_price;
            }
            let _ = entry.sm.apply(OrderEvent::AmendAccepted {
                filled: entry.sm.filled,
                remaining: req.quantity - entry.sm.filled,
                avg_fill_price: entry.sm.avg_fill_price,
            });
            build_order_update(order_id, &entry.sf, &entry.sm)
        };

        self.order_cache
            .write()
            .await
            .insert(order_id, update.clone());
        let _ = self.msg_tx.send(ServerMessage::OrderUpdate(update));
        info!(
            "SIM order #{order_id}: modified qty={} limit={:?} stop={:?}",
            req.quantity, req.limit_price, req.stop_price
        );
    }

    async fn fill_order(&mut self, order_id: i32, fill_price: f64) {
        // Phase 1: update state machine, extract data needed for position update.
        let (update, symbol, action, fill_qty) = {
            let entry = match self.orders.get_mut(&order_id) {
                Some(e) => e,
                None => return,
            };
            if entry.sm.state.is_terminal() {
                return;
            }
            let fill_qty = entry.sm.remaining;
            let cumulative_filled = entry.sm.filled + fill_qty;
            let _ = entry.sm.apply(OrderEvent::Fill {
                filled: cumulative_filled,
                remaining: 0.0,
                avg_fill_price: fill_price,
            });
            let update = build_order_update(order_id, &entry.sf, &entry.sm);
            (
                update,
                entry.sf.symbol.clone(),
                entry.sf.action.clone(),
                fill_qty,
            )
        };

        // Phase 2: broadcast order update and update position (no longer borrowing self.orders).
        self.order_cache
            .write()
            .await
            .insert(order_id, update.clone());
        let _ = self.msg_tx.send(ServerMessage::OrderUpdate(update));

        // Update position cache.
        let mut pos_cache = self.position_cache.write().await;
        let pos = pos_cache
            .entry(symbol.clone())
            .or_insert(PositionData {
                symbol: symbol.clone(),
                position_size: 0.0,
                average_cost: 0.0,
                account: "SIM".to_string(),
            });

        let signed_qty = if action == "BUY" {
            fill_qty
        } else {
            -fill_qty
        };
        let old_pos = pos.position_size;
        let new_pos = old_pos + signed_qty;

        if new_pos == 0.0 {
            pos.average_cost = 0.0;
        } else if old_pos == 0.0 || (old_pos.signum() != new_pos.signum()) {
            // Starting fresh or flipped sides.
            pos.average_cost = fill_price;
        } else if new_pos.abs() > old_pos.abs() {
            // Adding to position — weighted average.
            pos.average_cost =
                (old_pos.abs() * pos.average_cost + fill_qty * fill_price) / new_pos.abs();
        }
        // else: reducing position, keep old average.

        pos.position_size = new_pos;
        let pos_update = pos.clone();
        drop(pos_cache);

        let _ = self
            .msg_tx
            .send(ServerMessage::PositionUpdate(pos_update));

        info!(
            "SIM order #{order_id}: filled {action} {fill_qty} x {symbol} @ {fill_price}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OrderRequest;

    struct TestHarness {
        engine: SimOrderEngine,
        rx: broadcast::Receiver<ServerMessage>,
        position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
        trading_state: Arc<RwLock<TradingState>>,
    }

    fn make_harness() -> TestHarness {
        let (msg_tx, rx) = broadcast::channel::<ServerMessage>(64);
        let position_cache = Arc::new(RwLock::new(HashMap::new()));
        let config_cache = Arc::new(RwLock::new(HashMap::from([(
            "TEST".to_string(),
            ContractConfig {
                symbol: "TEST".to_string(),
                autotrade: true,
                max_pos_size: 100,
                min_pos_size: 0,
                max_order_size: 100,
                multiplier: 1.0,
                lot_size: 1,
                strategy: "none".to_string(),
            },
        )])));
        let trading_state = Arc::new(RwLock::new(TradingState::Active));
        let order_cache = Arc::new(RwLock::new(HashMap::new()));

        let engine = SimOrderEngine::new(
            position_cache.clone(),
            config_cache,
            trading_state.clone(),
            order_cache,
            msg_tx,
        );
        TestHarness {
            engine,
            rx,
            position_cache,
            trading_state,
        }
    }

    fn test_order(
        action: &str,
        order_type: &str,
        quantity: f64,
        limit_price: Option<f64>,
        stop_price: Option<f64>,
    ) -> OrderRequest {
        OrderRequest {
            client_order_id: Uuid::new_v4(),
            symbol: "TEST".to_string(),
            security_type: String::new(),
            exchange: String::new(),
            currency: String::new(),
            primary_exchange: String::new(),
            last_trade_date_or_contract_month: String::new(),
            strike: 0.0,
            right: String::new(),
            contract_id: 0,
            action: action.to_string(),
            order_type: order_type.to_string(),
            quantity,
            limit_price,
            stop_price,
            time_in_force: "DAY".to_string(),
        }
    }

    fn bar(low: f64, high: f64, close: f64) -> BarData {
        BarData {
            timestamp: 1_000_000,
            open: 10.0,
            high,
            low,
            close,
            volume: 100.0,
        }
    }

    fn drain_messages(rx: &mut broadcast::Receiver<ServerMessage>) -> Vec<ServerMessage> {
        let mut msgs = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

    fn find_order_update<'a>(msgs: &'a [ServerMessage], status: &str) -> Option<&'a OrderUpdate> {
        msgs.iter().find_map(|m| match m {
            ServerMessage::OrderUpdate(u) if u.status == status => Some(u),
            _ => None,
        })
    }

    #[tokio::test]
    async fn market_order_fills_immediately() {
        let mut h = make_harness();
        h.engine.on_bar(&bar(9.0, 11.0, 10.5)).await;
        drain_messages(&mut h.rx);

        h.engine
            .handle_command(OrderCommand::Place(test_order("BUY", "MKT", 1.0, None, None)))
            .await;
        let msgs = drain_messages(&mut h.rx);

        assert!(find_order_update(&msgs, "Working").is_some());
        let filled = find_order_update(&msgs, "Filled").expect("expected Filled update");
        assert_eq!(filled.average_fill_price, 10.5);
        assert_eq!(filled.filled, 1.0);
    }

    #[tokio::test]
    async fn limit_buy_fills_when_bar_low_touches() {
        let mut h = make_harness();
        h.engine.on_bar(&bar(9.0, 11.0, 10.5)).await;
        drain_messages(&mut h.rx);

        h.engine
            .handle_command(OrderCommand::Place(test_order(
                "BUY",
                "LMT",
                1.0,
                Some(9.5),
                None,
            )))
            .await;
        drain_messages(&mut h.rx);

        // Bar whose low touches the limit.
        h.engine.on_bar(&bar(9.0, 11.0, 10.0)).await;
        let msgs = drain_messages(&mut h.rx);

        let filled = find_order_update(&msgs, "Filled").expect("expected Filled");
        assert_eq!(filled.average_fill_price, 9.5);
    }

    #[tokio::test]
    async fn limit_sell_fills_when_bar_high_touches() {
        let mut h = make_harness();
        h.engine.on_bar(&bar(9.0, 11.0, 10.5)).await;
        drain_messages(&mut h.rx);

        h.engine
            .handle_command(OrderCommand::Place(test_order(
                "SELL",
                "LMT",
                1.0,
                Some(11.5),
                None,
            )))
            .await;
        drain_messages(&mut h.rx);

        h.engine.on_bar(&bar(9.0, 12.0, 11.0)).await;
        let msgs = drain_messages(&mut h.rx);

        let filled = find_order_update(&msgs, "Filled").expect("expected Filled");
        assert_eq!(filled.average_fill_price, 11.5);
    }

    #[tokio::test]
    async fn stop_buy_fills() {
        let mut h = make_harness();
        h.engine.on_bar(&bar(9.0, 10.0, 9.5)).await;
        drain_messages(&mut h.rx);

        h.engine
            .handle_command(OrderCommand::Place(test_order(
                "BUY",
                "STP",
                1.0,
                None,
                Some(11.0),
            )))
            .await;
        drain_messages(&mut h.rx);

        h.engine.on_bar(&bar(9.0, 11.5, 11.0)).await;
        let msgs = drain_messages(&mut h.rx);

        let filled = find_order_update(&msgs, "Filled").expect("expected Filled");
        assert_eq!(filled.average_fill_price, 11.0);
    }

    #[tokio::test]
    async fn stop_sell_fills() {
        let mut h = make_harness();
        h.engine.on_bar(&bar(9.0, 11.0, 10.0)).await;
        drain_messages(&mut h.rx);

        h.engine
            .handle_command(OrderCommand::Place(test_order(
                "SELL",
                "STP",
                1.0,
                None,
                Some(9.0),
            )))
            .await;
        drain_messages(&mut h.rx);

        h.engine.on_bar(&bar(8.5, 11.0, 9.5)).await;
        let msgs = drain_messages(&mut h.rx);

        let filled = find_order_update(&msgs, "Filled").expect("expected Filled");
        assert_eq!(filled.average_fill_price, 9.0);
    }

    #[tokio::test]
    async fn risk_check_rejection() {
        let mut h = make_harness();

        // Order size 200 exceeds max_order_size of 100.
        h.engine
            .handle_command(OrderCommand::Place(test_order(
                "BUY",
                "MKT",
                200.0,
                None,
                None,
            )))
            .await;
        let msgs = drain_messages(&mut h.rx);

        let rejected = find_order_update(&msgs, "Rejected").expect("expected Rejected");
        assert_eq!(rejected.quantity, 200.0);
    }

    #[tokio::test]
    async fn cancel_working_order() {
        let mut h = make_harness();
        h.engine.on_bar(&bar(9.0, 11.0, 10.5)).await;
        drain_messages(&mut h.rx);

        let req = test_order("BUY", "LMT", 1.0, Some(9.5), None);
        let client_id = req.client_order_id;
        h.engine
            .handle_command(OrderCommand::Place(req))
            .await;
        drain_messages(&mut h.rx);

        h.engine
            .handle_command(OrderCommand::Cancel {
                client_order_id: client_id,
            })
            .await;
        let msgs = drain_messages(&mut h.rx);

        assert!(find_order_update(&msgs, "Cancelled").is_some());
    }

    #[tokio::test]
    async fn modify_working_limit_order() {
        let mut h = make_harness();
        h.engine.on_bar(&bar(9.0, 11.0, 10.5)).await;
        drain_messages(&mut h.rx);

        let req = test_order("BUY", "LMT", 1.0, Some(9.0), None);
        let client_id = req.client_order_id;
        h.engine
            .handle_command(OrderCommand::Place(req))
            .await;
        drain_messages(&mut h.rx);

        // Modify limit price to 10.0.
        h.engine
            .handle_command(OrderCommand::Modify(crate::ModifyOrderRequest {
                client_order_id: client_id,
                quantity: 1.0,
                limit_price: Some(10.0),
                stop_price: None,
            }))
            .await;
        drain_messages(&mut h.rx);

        // Bar with low=9.8 now touches the new limit of 10.0.
        h.engine.on_bar(&bar(9.8, 11.0, 10.5)).await;
        let msgs = drain_messages(&mut h.rx);

        let filled = find_order_update(&msgs, "Filled").expect("expected Filled at new price");
        assert_eq!(filled.average_fill_price, 10.0);
    }

    #[tokio::test]
    async fn position_updates_on_fill() {
        let mut h = make_harness();
        h.engine.on_bar(&bar(9.0, 11.0, 10.5)).await;
        drain_messages(&mut h.rx);

        h.engine
            .handle_command(OrderCommand::Place(test_order("BUY", "MKT", 3.0, None, None)))
            .await;

        let pos = h.position_cache.read().await;
        let p = pos.get("TEST").expect("position should exist");
        assert_eq!(p.position_size, 3.0);
        assert_eq!(p.average_cost, 10.5);
    }

    #[tokio::test]
    async fn halted_trading_state_rejects() {
        let mut h = make_harness();
        *h.trading_state.write().await = TradingState::Halted;

        h.engine
            .handle_command(OrderCommand::Place(test_order("BUY", "MKT", 1.0, None, None)))
            .await;
        let msgs = drain_messages(&mut h.rx);

        let rejected = find_order_update(&msgs, "Rejected").expect("expected Rejected");
        assert_eq!(rejected.symbol, "TEST");
    }
}
