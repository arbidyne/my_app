//! WebSocket server that streams real-time price updates from Interactive Brokers (IBKR)
//! to connected clients. Connects to IB Gateway for tick-by-tick bid/ask data and
//! broadcasts best bid/ask and last price over WebSocket at ws://127.0.0.1:3000/ws.
//!
//! Clients send subscribe requests (symbol, security_type, exchange, currency) and the
//! server manages one subscription per unique contract, broadcasting updates to all clients.
//! On subscription, also fetches 1 day of historical minute bars and broadcasts them.
//!
//! IBKR limits concurrent gateway connections, so this server maintains a single connection
//! and fans out data to many browser clients via WebSocket. A broadcast channel lets each
//! client receive all messages independently without per-client send logic.

use anyhow::Result;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use ibapi::accounts::PositionUpdate as IbPositionUpdate;
use ibapi::contracts::Contract;
use ibapi::market_data::historical::HistoricalBarUpdate;
use ibapi::market_data::realtime::BidAsk;
use ibapi::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, error, info};

/// Decoupled from ibapi's `Bar` so we control serde serialization and normalize
/// timestamps to milliseconds (JS/frontend convention).
#[derive(Clone, Debug, Serialize)]
struct BarData {
    timestamp: u64, // Unix time in milliseconds
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

/// Flattened from ibapi's nested `Position` to include only the fields the UI needs.
#[derive(Clone, Debug, Serialize)]
struct PositionData {
    symbol: String,
    position_size: f64,
    average_cost: f64,
    account: String,
}

/// Per-contract settings that a future autotrader will check before placing trades.
/// In-memory only for now; serde derives allow adding persistence later without
/// changing the WebSocket protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContractConfig {
    symbol: String,
    autotrade: bool,
    max_pos_size: u32,
    min_pos_size: i32,
    max_order_size: u32,
    multiplier: f64,
    lot_size: u32,
}

/// Single enum for all server→client messages so one broadcast channel carries everything.
/// `serde(tag = "type")` emits a discriminator field so the frontend can route by message kind.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    PriceUpdate {
        symbol: String,
        last_price: f64,
        timestamp: u64,
        best_bid_price: f64,
        best_bid_size: f64,
        best_ask_price: f64,
        best_ask_size: f64,
    },
    HistoricalBars {
        symbol: String,
        bars: Vec<BarData>,
    },
    RealtimeBar {
        symbol: String,
        bar: BarData,
    },
    PositionUpdate(PositionData),
    ContractConfig(ContractConfig),
}

/// IBKR rejects duplicate subscriptions and charges per active subscription, so we dedup
/// by the four fields that uniquely identify a contract.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct ContractKey {
    symbol: String,
    security_type: String,
    exchange: String,
    currency: String,
    last_trade_date_or_contract_month: String,
    strike: String, // f64 stored as String for Hash/Eq
    right: String,
    contract_id: i32,
}

/// Fields IBKR needs to identify a contract. The base four (symbol, security_type,
/// exchange, currency) suffice for stocks/forex; the optional fields enable futures
/// and options where expiry, strike, and right are required for disambiguation.
#[derive(Clone, Debug, Deserialize)]
struct SubscribeRequest {
    symbol: String,
    security_type: String,
    exchange: String,
    currency: String,
    #[serde(default)]
    primary_exchange: String,
    #[serde(default)]
    last_trade_date_or_contract_month: String,
    #[serde(default)]
    strike: f64,
    #[serde(default)]
    right: String,
    #[serde(default)]
    contract_id: i32,
}

/// `RequestBars` exists as a fallback for clients that connect before bar data is cached.
/// Tagged enum so the same WebSocket carries both subscription and data requests.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Subscribe(SubscribeRequest),
    RequestBars { symbol: String },
    UpdateContractConfig(ContractConfig),
}

/// Caches use `Arc<RwLock<>>` because many WebSocket tasks read concurrently while only
/// the subscription manager writes.
#[derive(Clone)]
struct AppState {
    msg_tx: broadcast::Sender<ServerMessage>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    // Broadcast (not mpsc) so each client gets its own receiver — fan-out without explicit per-client send logic.
    let (msg_tx, _) = broadcast::channel::<ServerMessage>(100);

    // Funneled through mpsc so a single manager task can dedup and prevent duplicate IBKR subscriptions.
    let (subscribe_tx, subscribe_rx) = mpsc::channel::<SubscribeRequest>(32);

    // Cached server-side so clients connecting after the initial broadcast still get full bar history.
    let bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Cached so late-joining clients see current positions without waiting for the next IBKR update.
    let position_cache: Arc<RwLock<HashMap<String, PositionData>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Per-contract autotrader settings, cached so late-joining clients get current configs.
    let config_cache: Arc<RwLock<HashMap<String, ContractConfig>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Runs in a dedicated task so the HTTP server can start accepting connections immediately.
    let msg_tx_clone = msg_tx.clone();
    let bar_cache_clone = bar_cache.clone();
    let position_cache_clone = position_cache.clone();
    let config_cache_clone = config_cache.clone();
    let subscribe_tx_clone = subscribe_tx.clone();
    tokio::spawn(async move {
        subscription_manager(subscribe_rx, subscribe_tx_clone, msg_tx_clone, bar_cache_clone, position_cache_clone, config_cache_clone)
            .await;
    });

    let state = AppState {
        msg_tx,
        subscribe_tx,
        bar_cache,
        position_cache,
        config_cache,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    info!("WebSocket server listening on ws://127.0.0.1:3000/ws");
    axum::serve(listener, app).await?;

    Ok(())
}

/// Single long-lived IBKR connection shared across all subscriptions — IBKR limits concurrent
/// gateway connections, so one connection avoids exhausting connection slots.
async fn subscription_manager(
    mut subscribe_rx: mpsc::Receiver<SubscribeRequest>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
    msg_tx: broadcast::Sender<ServerMessage>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
) {
    let client = match Client::connect("127.0.0.1:7496", 42).await {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("Failed to connect to IB Gateway: {e}");
            return;
        }
    };

    info!("Connected to IBKR, waiting for subscription requests");

    // Positions auto-subscribe to price data so users see live prices for their holdings
    // without manually subscribing to each one.
    let client_pos = client.clone();
    let msg_tx_pos = msg_tx.clone();
    let position_cache_pos = position_cache.clone();
    let subscribe_tx_pos = subscribe_tx.clone();
    tokio::spawn(async move {
        match client_pos.positions().await {
            Ok(mut subscription) => {
                info!("Subscribed to positions");
                while let Some(result) = subscription.next().await {
                    match result {
                        Ok(IbPositionUpdate::Position(pos)) => {
                            let symbol = pos.contract.symbol.0.clone();
                            let data = PositionData {
                                symbol: symbol.clone(),
                                position_size: pos.position,
                                average_cost: pos.average_cost,
                                account: pos.account,
                            };
                            position_cache_pos
                                .write()
                                .await
                                .insert(symbol.clone(), data.clone());
                            let _ = msg_tx_pos.send(ServerMessage::PositionUpdate(data));
                            let _ = subscribe_tx_pos.send(SubscribeRequest {
                                symbol: pos.contract.symbol.0.clone(),
                                security_type: pos.contract.security_type.to_string(),
                                exchange: pos.contract.exchange.0.clone(),
                                currency: pos.contract.currency.0.clone(),
                                primary_exchange: pos.contract.primary_exchange.0.clone(),
                                last_trade_date_or_contract_month: pos.contract.last_trade_date_or_contract_month.clone(),
                                strike: pos.contract.strike,
                                right: pos.contract.right.clone(),
                                contract_id: pos.contract.contract_id,
                            }).await;
                            debug!("Position update: {symbol}");
                        }
                        Ok(IbPositionUpdate::PositionEnd) => {
                            info!("Initial position snapshot complete");
                        }
                        Err(e) => {
                            error!("Position subscription error: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to subscribe to positions: {e}");
            }
        }
    });

    let mut subscribed: HashSet<ContractKey> = HashSet::new();

    while let Some(req) = subscribe_rx.recv().await {
        let key = ContractKey {
            symbol: req.symbol.clone(),
            security_type: req.security_type.clone(),
            exchange: req.exchange.clone(),
            currency: req.currency.clone(),
            last_trade_date_or_contract_month: req.last_trade_date_or_contract_month.clone(),
            strike: req.strike.to_string(),
            right: req.right.clone(),
            contract_id: req.contract_id,
        };

        if !subscribed.insert(key) {
            info!(
                "Already subscribed to {} ({}) on {}",
                req.symbol, req.security_type, req.exchange
            );
            continue;
        }

        // Create default config for newly subscribed contracts so the autotrader
        // has safe defaults (disabled, zero sizes) until the user explicitly configures it.
        {
            let mut cache = config_cache.write().await;
            if !cache.contains_key(&req.symbol) {
                let cfg = ContractConfig {
                    symbol: req.symbol.clone(),
                    autotrade: false,
                    max_pos_size: 0,
                    min_pos_size: 0i32,
                    max_order_size: 0,
                    multiplier: 1.0,
                    lot_size: 1,
                };
                cache.insert(req.symbol.clone(), cfg.clone());
                let _ = msg_tx.send(ServerMessage::ContractConfig(cfg));
            }
        }

        let contract = Contract {
            contract_id: req.contract_id,
            symbol: Symbol(req.symbol.clone()),
            security_type: SecurityType::from(req.security_type.as_str()),
            exchange: Exchange(req.exchange),
            currency: Currency(req.currency),
            primary_exchange: Exchange(req.primary_exchange),
            last_trade_date_or_contract_month: req.last_trade_date_or_contract_month,
            strike: req.strike,
            right: req.right,
            ..Default::default()
        };
        let symbol = req.symbol.clone();

        // Tick-by-tick streaming (not snapshot polling) to minimize latency and API rate limit usage.
        let client_tick = client.clone();
        let msg_tx_tick = msg_tx.clone();
        let symbol_tick = symbol.clone();
        let contract_tick = contract.clone();
        tokio::spawn(async move {
            match client_tick
                .tick_by_tick_bid_ask(&contract_tick, 0, false)
                .await
            {
                Ok(mut subscription) => {
                    info!("Subscribed to tick-by-tick bid/ask for {symbol_tick}");

                    while let Some(tick_result) = subscription.next().await {
                        match tick_result {
                            Ok(BidAsk {
                                time,
                                bid_price,
                                ask_price,
                                bid_size,
                                ask_size,
                                ..
                            }) => {
                                let timestamp_ms = time.unix_timestamp() as u64 * 1000;
                                // Mid-price when both sides are quoted; fall back to whichever
                                // side exists (pre/post-market often has only one side).
                                let last_price = if bid_price > 0.0 && ask_price > 0.0 {
                                    (bid_price + ask_price) / 2.0
                                } else if ask_price > 0.0 {
                                    ask_price
                                } else {
                                    bid_price
                                };

                                let msg = ServerMessage::PriceUpdate {
                                    symbol: symbol_tick.clone(),
                                    last_price,
                                    timestamp: timestamp_ms,
                                    best_bid_price: bid_price,
                                    best_bid_size: bid_size,
                                    best_ask_price: ask_price,
                                    best_ask_size: ask_size,
                                };

                                let _ = msg_tx_tick.send(msg);
                                debug!(
                                    "{symbol_tick} tick: bid {bid_price} x {bid_size} | ask {ask_price} x {ask_size} @ {timestamp_ms}"
                                );
                            }
                            Err(e) => {
                                error!("{symbol_tick} tick subscription error: {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to subscribe to tick data for {symbol_tick}: {e}");
                }
            }
        });

        // Streaming historical API delivers the initial batch then continues with live bar updates
        // on the same subscription — chart backfill and live candles from a single API call.
        let client_hist = client.clone();
        let msg_tx_hist = msg_tx.clone();
        let symbol_hist = symbol.clone();
        let contract_hist = contract.clone();
        let bar_cache_hist = bar_cache.clone();
        tokio::spawn(async move {
            match client_hist
                .historical_data_streaming(
                    &contract_hist,
                    1.days(),
                    HistoricalBarSize::Min,
                    Some(HistoricalWhatToShow::Trades),
                    TradingHours::Extended,
                    true,
                )
                .await
            {
                Ok(mut subscription) => {
                    info!("{symbol_hist}: streaming historical data started");
                    while let Some(update) = subscription.next().await {
                        match update {
                            HistoricalBarUpdate::Historical(data) => {
                                let bars: Vec<BarData> = data
                                    .bars
                                    .iter()
                                    .map(|b| BarData {
                                        timestamp: b.date.unix_timestamp() as u64 * 1000,
                                        open: b.open,
                                        high: b.high,
                                        low: b.low,
                                        close: b.close,
                                        volume: b.volume,
                                    })
                                    .collect();
                                info!(
                                    "{symbol_hist}: caching and broadcasting {} historical bars",
                                    bars.len()
                                );
                                bar_cache_hist
                                    .write()
                                    .await
                                    .insert(symbol_hist.clone(), bars.clone());
                                let _ = msg_tx_hist.send(ServerMessage::HistoricalBars {
                                    symbol: symbol_hist.clone(),
                                    bars,
                                });
                            }
                            HistoricalBarUpdate::Update(bar) => {
                                let bar_data = BarData {
                                    timestamp: bar.date.unix_timestamp() as u64 * 1000,
                                    open: bar.open,
                                    high: bar.high,
                                    low: bar.low,
                                    close: bar.close,
                                    volume: bar.volume,
                                };

                                // IBKR sends repeated updates for the in-progress bar as trades occur;
                                // same timestamp means the bar is still forming — update in place, don't duplicate.
                                {
                                    let mut cache = bar_cache_hist.write().await;
                                    let bars =
                                        cache.entry(symbol_hist.clone()).or_default();
                                    if let Some(last) = bars.last_mut() {
                                        if last.timestamp == bar_data.timestamp {
                                            *last = bar_data.clone();
                                        } else {
                                            bars.push(bar_data.clone());
                                        }
                                    } else {
                                        bars.push(bar_data.clone());
                                    }
                                }

                                let _ = msg_tx_hist.send(ServerMessage::RealtimeBar {
                                    symbol: symbol_hist.clone(),
                                    bar: bar_data,
                                });
                            }
                            HistoricalBarUpdate::End { .. } => {
                                info!("{symbol_hist}: historical backfill complete, continuing with live bars");
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("{symbol_hist}: historical data streaming failed: {e}");
                }
            }
        });
    }
}

/// Subscribes to the broadcast channel at upgrade time (before the socket loop starts) so no
/// messages are missed between the HTTP upgrade and the first recv call.
async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    let rx = state.msg_tx.subscribe();
    let msg_tx = state.msg_tx.clone();
    let subscribe_tx = state.subscribe_tx.clone();
    let bar_cache = state.bar_cache.clone();
    let position_cache = state.position_cache.clone();
    let config_cache = state.config_cache.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, rx, msg_tx, subscribe_tx, bar_cache, position_cache, config_cache))
}

/// Single task handles both directions via `tokio::select!` so we detect client disconnect
/// from either side and clean up immediately, rather than coordinating two separate tasks.
async fn handle_socket(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<ServerMessage>,
    msg_tx: broadcast::Sender<ServerMessage>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
) {
    // Broadcast receivers only see messages sent after they subscribe, so late-joining clients
    // would have no positions without this eager send from the cache.
    {
        let cache = position_cache.read().await;
        for pos in cache.values() {
            let msg = ServerMessage::PositionUpdate(pos.clone());
            if let Ok(json) = serde_json::to_string(&msg) {
                if socket.send(Message::Text(json)).await.is_err() {
                    return;
                }
            }
        }
    }

    // Same rationale as positions above — without this, late-joining clients would have no
    // chart data until the next live bar update arrives.
    {
        let cache = bar_cache.read().await;
        for (symbol, bars) in cache.iter() {
            let msg = ServerMessage::HistoricalBars {
                symbol: symbol.clone(),
                bars: bars.clone(),
            };
            if let Ok(json) = serde_json::to_string(&msg) {
                if socket.send(Message::Text(json)).await.is_err() {
                    return;
                }
            }
        }
    }

    {
        let cache = config_cache.read().await;
        for cfg in cache.values() {
            let msg = ServerMessage::ContractConfig(cfg.clone());
            if let Ok(json) = serde_json::to_string(&msg) {
                if socket.send(Message::Text(json)).await.is_err() {
                    return;
                }
            }
        }
    }

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(msg) => {
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if socket.send(Message::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        info!("Client lagged, skipped {n} messages");
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Subscribe(req)) => {
                                info!("Client subscribe request: {} on {}", req.symbol, req.exchange);
                                let _ = subscribe_tx.send(req).await;
                            }
                            Ok(ClientMessage::RequestBars { symbol }) => {
                                let cache = bar_cache.read().await;
                                if let Some(bars) = cache.get(&symbol) {
                                    info!("Sending {} cached bars for {symbol}", bars.len());
                                    let msg = ServerMessage::HistoricalBars {
                                        symbol,
                                        bars: bars.clone(),
                                    };
                                    if let Ok(json) = serde_json::to_string(&msg) {
                                        if socket.send(Message::Text(json)).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(ClientMessage::UpdateContractConfig(cfg)) => {
                                info!(
                                    "Config update for {}: autotrade={}, max_pos={}, min_pos={}, max_order={}, multiplier={}, lot_size={}",
                                    cfg.symbol, cfg.autotrade, cfg.max_pos_size, cfg.min_pos_size, cfg.max_order_size, cfg.multiplier, cfg.lot_size
                                );
                                config_cache.write().await.insert(cfg.symbol.clone(), cfg.clone());
                                let _ = msg_tx.send(ServerMessage::ContractConfig(cfg));
                            }
                            Err(e) => {
                                info!("Invalid client message: {e}");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}
