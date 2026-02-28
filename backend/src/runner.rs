//! Async runner that drives a [`Strategy`] from the broadcast channel and
//! feeds resulting signals into the existing order pipeline.

use crate::strategy::{SignalAction, SignalOrderType, Signal, Strategy, StrategyContext};
use crate::{ContractConfig, OrderCommand, OrderRequest, PositionData, ServerMessage};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch, RwLock};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Sent from `handle_socket` → `subscription_manager` when a user changes strategy.
#[derive(Debug)]
pub enum StrategyCommand {
    Set { symbol: String, strategy_name: String },
}

/// Runs a single strategy instance for one symbol.
pub struct StrategyRunner {
    symbol: String,
    strategy: Box<dyn Strategy>,
    rx: broadcast::Receiver<ServerMessage>,
    order_tx: mpsc::Sender<OrderCommand>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
    shutdown: watch::Receiver<bool>,
}

impl StrategyRunner {
    pub fn new(
        symbol: String,
        strategy: Box<dyn Strategy>,
        rx: broadcast::Receiver<ServerMessage>,
        order_tx: mpsc::Sender<OrderCommand>,
        position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
        config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            symbol,
            strategy,
            rx,
            order_tx,
            position_cache,
            config_cache,
            shutdown,
        }
    }

    pub async fn run(mut self) {
        info!("Strategy '{}' started for {}", self.strategy.name(), self.symbol);
        loop {
            tokio::select! {
                result = self.rx.recv() => {
                    match result {
                        Ok(msg) => self.handle_message(msg).await,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            debug!("Strategy runner for {} lagged, skipped {n} messages", self.symbol);
                        }
                        Err(_) => break,
                    }
                }
                _ = self.shutdown.changed() => {
                    if *self.shutdown.borrow() {
                        info!("Strategy '{}' shutting down for {}", self.strategy.name(), self.symbol);
                        break;
                    }
                }
            }
        }
    }

    async fn handle_message(&mut self, msg: ServerMessage) {
        let signals = match msg {
            ServerMessage::PriceUpdate {
                ref symbol,
                last_price,
                timestamp,
                best_bid_price,
                best_ask_price,
                ..
            } if symbol == &self.symbol => {
                let ctx = self.build_context().await;
                self.strategy
                    .on_tick(&ctx, last_price, best_bid_price, best_ask_price, timestamp)
            }
            ServerMessage::RealtimeBar {
                ref symbol,
                ref bar,
            } if symbol == &self.symbol => {
                let ctx = self.build_context().await;
                self.strategy.on_bar(&ctx, bar)
            }
            ServerMessage::OrderUpdate(ref update)
                if update.symbol == self.symbol
                    && update.filled > 0.0
                    && matches!(
                        update.status.as_str(),
                        "Filled" | "PartiallyFilled"
                    ) =>
            {
                let ctx = self.build_context().await;
                self.strategy.on_fill(
                    &ctx,
                    &update.action,
                    update.filled,
                    update.average_fill_price,
                )
            }
            _ => return,
        };

        self.emit_signals(signals).await;
    }

    async fn build_context(&self) -> StrategyContext {
        let pos = self.position_cache.read().await;
        let cfg = self.config_cache.read().await;

        let (position_size, average_cost) = pos
            .get(&self.symbol)
            .map(|p| (p.position_size, p.average_cost))
            .unwrap_or((0.0, 0.0));

        let config = cfg
            .get(&self.symbol)
            .cloned()
            .unwrap_or(ContractConfig {
                symbol: self.symbol.clone(),
                autotrade: false,
                max_pos_size: 0,
                min_pos_size: 0,
                max_order_size: 0,
                multiplier: 1.0,
                lot_size: 1,
                strategy: "none".to_string(),
            });

        StrategyContext {
            symbol: self.symbol.clone(),
            position_size,
            average_cost,
            config,
        }
    }

    async fn emit_signals(&self, signals: Vec<Signal>) {
        for signal in signals {
            match signal {
                Signal::PlaceOrder {
                    action,
                    order_type,
                    quantity,
                } => {
                    let (action_str, order_type_str, limit_price, stop_price) =
                        match (&action, &order_type) {
                            (SignalAction::Buy, SignalOrderType::Market) => {
                                ("BUY".to_string(), "MKT".to_string(), None, None)
                            }
                            (SignalAction::Sell, SignalOrderType::Market) => {
                                ("SELL".to_string(), "MKT".to_string(), None, None)
                            }
                            (SignalAction::Buy, SignalOrderType::Limit { price }) => {
                                ("BUY".to_string(), "LMT".to_string(), Some(*price), None)
                            }
                            (SignalAction::Sell, SignalOrderType::Limit { price }) => {
                                ("SELL".to_string(), "LMT".to_string(), Some(*price), None)
                            }
                            (SignalAction::Buy, SignalOrderType::Stop { price }) => {
                                ("BUY".to_string(), "STP".to_string(), None, Some(*price))
                            }
                            (SignalAction::Sell, SignalOrderType::Stop { price }) => {
                                ("SELL".to_string(), "STP".to_string(), None, Some(*price))
                            }
                        };

                    let req = OrderRequest {
                        client_order_id: Uuid::new_v4(),
                        symbol: self.symbol.clone(),
                        security_type: String::new(),
                        exchange: String::new(),
                        currency: String::new(),
                        primary_exchange: String::new(),
                        last_trade_date_or_contract_month: String::new(),
                        strike: 0.0,
                        right: String::new(),
                        contract_id: 0,
                        action: action_str,
                        order_type: order_type_str,
                        quantity,
                        limit_price,
                        stop_price,
                        time_in_force: "DAY".to_string(),
                    };

                    info!(
                        "Strategy '{}' signal: {} {} {} x {}",
                        self.strategy.name(),
                        req.action,
                        req.order_type,
                        req.quantity,
                        self.symbol,
                    );

                    if let Err(e) = self.order_tx.send(OrderCommand::Place(req)).await {
                        warn!("Failed to send strategy order for {}: {e}", self.symbol);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BarData;
    use crate::strategy::{self, PingStrategy};

    fn make_caches() -> (
        Arc<RwLock<HashMap<String, PositionData>>>,
        Arc<RwLock<HashMap<String, ContractConfig>>>,
    ) {
        let pos = Arc::new(RwLock::new(HashMap::new()));
        let cfg = Arc::new(RwLock::new(HashMap::from([(
            "TEST".to_string(),
            ContractConfig {
                symbol: "TEST".to_string(),
                autotrade: true,
                max_pos_size: 100,
                min_pos_size: 0,
                max_order_size: 100,
                multiplier: 1.0,
                lot_size: 1,
                strategy: "ping".to_string(),
            },
        )])));
        (pos, cfg)
    }

    #[tokio::test]
    async fn runner_filters_by_symbol() {
        let (msg_tx, _) = broadcast::channel::<ServerMessage>(16);
        let (order_tx, mut order_rx) = mpsc::channel::<OrderCommand>(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (pos_cache, cfg_cache) = make_caches();

        let rx = msg_tx.subscribe();
        let runner = StrategyRunner::new(
            "TEST".to_string(),
            Box::new(PingStrategy::new()),
            rx,
            order_tx,
            pos_cache,
            cfg_cache,
            shutdown_rx,
        );

        let handle = tokio::spawn(runner.run());

        // Send a bar for the WRONG symbol
        let _ = msg_tx.send(ServerMessage::RealtimeBar {
            symbol: "OTHER".to_string(),
            bar: BarData {
                timestamp: 1_000_000,
                open: 10.0,
                high: 11.0,
                low: 9.0,
                close: 10.5,
                volume: 100.0,
            },
        });

        // Give it a moment to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Shut down
        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        // No orders should have been sent
        assert!(order_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn runner_converts_signal_to_order_command() {
        let (msg_tx, _) = broadcast::channel::<ServerMessage>(16);
        let (order_tx, mut order_rx) = mpsc::channel::<OrderCommand>(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (pos_cache, cfg_cache) = make_caches();

        let rx = msg_tx.subscribe();
        let runner = StrategyRunner::new(
            "TEST".to_string(),
            Box::new(PingStrategy::new()),
            rx,
            order_tx,
            pos_cache,
            cfg_cache,
            shutdown_rx,
        );

        let handle = tokio::spawn(runner.run());

        // Send a bar for the correct symbol — PingStrategy should fire
        let _ = msg_tx.send(ServerMessage::RealtimeBar {
            symbol: "TEST".to_string(),
            bar: BarData {
                timestamp: 1_000_000,
                open: 10.0,
                high: 11.0,
                low: 9.0,
                close: 10.5,
                volume: 100.0,
            },
        });

        // Receive the order command
        let cmd = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            order_rx.recv(),
        )
        .await
        .expect("timed out waiting for order")
        .expect("channel closed");

        match cmd {
            OrderCommand::Place(req) => {
                assert_eq!(req.symbol, "TEST");
                assert_eq!(req.action, "BUY");
                assert_eq!(req.order_type, "MKT");
                assert_eq!(req.quantity, 1.0);
            }
            other => panic!("Expected Place, got {:?}", std::mem::discriminant(&other)),
        }

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn runner_shuts_down_on_signal() {
        let (msg_tx, _) = broadcast::channel::<ServerMessage>(16);
        let (order_tx, _order_rx) = mpsc::channel::<OrderCommand>(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (pos_cache, cfg_cache) = make_caches();

        let rx = msg_tx.subscribe();
        let runner = StrategyRunner::new(
            "TEST".to_string(),
            Box::new(strategy::NoopStrategy),
            rx,
            order_tx,
            pos_cache,
            cfg_cache,
            shutdown_rx,
        );

        let handle = tokio::spawn(runner.run());

        // Signal shutdown
        let _ = shutdown_tx.send(true);

        // Runner should exit promptly
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("runner did not shut down in time")
            .unwrap();
    }
}
