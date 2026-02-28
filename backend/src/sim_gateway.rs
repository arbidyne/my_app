//! Simulated subscription manager for backtest mode.
//!
//! Replays bars from SQLite through the same broadcast channel that the live
//! subscription manager uses. Strategies, risk checks, order state machines,
//! WebSocket handlers, and the frontend are completely unaware it's a backtest.

use crate::sim_orders::SimOrderEngine;
use crate::{
    db, runner, strategy, BarData, ContractConfig, OrderCommand, OrderUpdate, PositionData,
    ServerMessage, TradingState,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch, RwLock};
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub struct BacktestConfig {
    pub symbol: String,
    pub from_ms: u64,
    pub to_ms: u64,
    pub db_path: String,
}

/// Drop-in replacement for the live `subscription_manager`. Replays bars from
/// SQLite, routes orders through [`SimOrderEngine`], and manages strategy
/// runners identically to the live path.
pub async fn sim_subscription_manager(
    config: BacktestConfig,
    mut order_rx: mpsc::Receiver<OrderCommand>,
    msg_tx: broadcast::Sender<ServerMessage>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
    order_cache: Arc<RwLock<HashMap<i32, OrderUpdate>>>,
    trading_state: Arc<RwLock<TradingState>>,
    order_tx: mpsc::Sender<OrderCommand>,
    mut strategy_rx: mpsc::Receiver<runner::StrategyCommand>,
) {
    // Open SQLite and load bars.
    let bar_db = match db::BarDb::open(&config.db_path) {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("Failed to open bar database at {}: {e}", config.db_path);
            return;
        }
    };
    let bars = match bar_db.load_bars(&config.symbol, config.from_ms, config.to_ms) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("Failed to load bars for {}: {e}", config.symbol);
            return;
        }
    };
    if bars.is_empty() {
        warn!(
            "No bars found for {} in range {}..{}",
            config.symbol, config.from_ms, config.to_ms
        );
        return;
    }
    info!(
        "Backtest: loaded {} bars for {} ({} → {})",
        bars.len(),
        config.symbol,
        bars.first().unwrap().timestamp,
        bars.last().unwrap().timestamp,
    );

    // Set trading state to Active and create default config.
    *trading_state.write().await = TradingState::Active;
    let _ = msg_tx.send(ServerMessage::TradingState {
        state: TradingState::Active,
    });
    {
        let mut cache = config_cache.write().await;
        if !cache.contains_key(&config.symbol) {
            let cfg = ContractConfig {
                symbol: config.symbol.clone(),
                autotrade: false,
                max_pos_size: 0,
                min_pos_size: 0,
                max_order_size: 0,
                multiplier: 1.0,
                lot_size: 1,
                strategy: crate::default_strategy(),
            };
            cache.insert(config.symbol.clone(), cfg.clone());
            let _ = msg_tx.send(ServerMessage::ContractConfig(cfg));
        }
    }

    // Broadcast full historical bars for chart rendering.
    bar_cache
        .write()
        .await
        .insert(config.symbol.clone(), bars.clone());
    let _ = msg_tx.send(ServerMessage::HistoricalBars {
        symbol: config.symbol.clone(),
        bars: bars.clone(),
    });

    // Create simulated order engine.
    let mut sim_engine = SimOrderEngine::new(
        position_cache.clone(),
        config_cache.clone(),
        trading_state.clone(),
        order_cache.clone(),
        msg_tx.clone(),
    );

    let mut strategy_runners: HashMap<String, (watch::Sender<bool>, JoinHandle<()>)> =
        HashMap::new();

    let total_bars = bars.len();
    info!("Backtest: replaying {total_bars} bars for {}", config.symbol);

    // Bar replay loop.
    for bar in &bars {
        // Check pending limit/stop orders against this bar.
        sim_engine.on_bar(bar).await;

        // Broadcast bar to strategies and frontend.
        let price_msg = ServerMessage::PriceUpdate {
            symbol: config.symbol.clone(),
            last_price: bar.close,
            timestamp: bar.timestamp,
            best_bid_price: bar.close,
            best_bid_size: 0.0,
            best_ask_price: bar.close,
            best_ask_size: 0.0,
        };
        let bar_msg = ServerMessage::RealtimeBar {
            symbol: config.symbol.clone(),
            bar: bar.clone(),
        };
        let _ = msg_tx.send(price_msg);
        let _ = msg_tx.send(bar_msg);

        // Yield to let strategy runners process the bar and emit orders.
        tokio::task::yield_now().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

        // Drain order commands from strategies and frontend.
        while let Ok(cmd) = order_rx.try_recv() {
            sim_engine.handle_command(cmd).await;
        }

        // Drain strategy commands (user may change strategy mid-backtest).
        while let Ok(cmd) = strategy_rx.try_recv() {
            handle_strategy_command(
                cmd,
                &mut strategy_runners,
                &msg_tx,
                &order_tx,
                &position_cache,
                &config_cache,
            );
        }
    }

    info!(
        "Backtest complete: {} bars replayed for {}",
        total_bars, config.symbol
    );
    let _ = msg_tx.send(ServerMessage::BacktestComplete {
        symbol: config.symbol.clone(),
        total_bars,
    });

    // Keep running so the user can interact with the results via the frontend.
    loop {
        tokio::select! {
            Some(cmd) = order_rx.recv() => {
                sim_engine.handle_command(cmd).await;
            }
            Some(cmd) = strategy_rx.recv() => {
                handle_strategy_command(
                    cmd,
                    &mut strategy_runners,
                    &msg_tx,
                    &order_tx,
                    &position_cache,
                    &config_cache,
                );
            }
            else => break,
        }
    }
}

/// Shared strategy runner lifecycle management used by both live and sim paths.
fn handle_strategy_command(
    cmd: runner::StrategyCommand,
    strategy_runners: &mut HashMap<String, (watch::Sender<bool>, JoinHandle<()>)>,
    msg_tx: &broadcast::Sender<ServerMessage>,
    order_tx: &mpsc::Sender<OrderCommand>,
    position_cache: &Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: &Arc<RwLock<HashMap<String, ContractConfig>>>,
) {
    match cmd {
        runner::StrategyCommand::Set {
            symbol,
            strategy_name,
        } => {
            // Stop existing runner for this symbol.
            if let Some((shutdown_tx, _handle)) = strategy_runners.remove(&symbol) {
                let _ = shutdown_tx.send(true);
            }

            if strategy_name == "none" {
                info!("Strategy cleared for {symbol}");
                return;
            }

            match strategy::create_strategy(&strategy_name) {
                Some(strat) => {
                    let (shutdown_tx, shutdown_rx) = watch::channel(false);
                    let runner = runner::StrategyRunner::new(
                        symbol.clone(),
                        strat,
                        msg_tx.subscribe(),
                        order_tx.clone(),
                        position_cache.clone(),
                        config_cache.clone(),
                        shutdown_rx,
                    );
                    let handle = tokio::spawn(runner.run());
                    strategy_runners.insert(symbol, (shutdown_tx, handle));
                }
                None => {
                    warn!("Unknown strategy '{strategy_name}' for {symbol}");
                }
            }
        }
    }
}
