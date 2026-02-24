//! WebSocket server that streams real-time price updates from Interactive Brokers (IBKR)
//! to connected clients. Connects to IB Gateway for tick-by-tick bid/ask data and
//! broadcasts best bid/ask and last price over WebSocket at ws://127.0.0.1:3000/ws.
//!
//! Clients send subscribe requests (symbol, security_type, exchange, currency) and the
//! server manages one subscription per unique contract, broadcasting updates to all clients.
//! On subscription, also fetches 1 day of historical minute bars and broadcasts them.

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

/// Single OHLCV bar converted from ibapi's historical Bar.
#[derive(Clone, Debug, Serialize)]
struct BarData {
    timestamp: u64, // Unix time in milliseconds
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

/// Position data for a single holding.
#[derive(Clone, Debug, Serialize)]
struct PositionData {
    symbol: String,
    position_size: f64,
    average_cost: f64,
    account: String,
}

/// All server-to-client WebSocket messages, tagged by "type".
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
}

/// Unique key for deduplicating contract subscriptions.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct ContractKey {
    symbol: String,
    security_type: String,
    exchange: String,
    currency: String,
}

/// Client subscribe request matching the JSON protocol.
#[derive(Clone, Debug, Deserialize)]
struct SubscribeRequest {
    symbol: String,
    security_type: String,
    exchange: String,
    currency: String,
}

/// Tagged enum for all client-to-server WebSocket messages.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Subscribe(SubscribeRequest),
    RequestBars { symbol: String },
}

/// Shared application state passed to all handlers.
#[derive(Clone)]
struct AppState {
    msg_tx: broadcast::Sender<ServerMessage>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    // Broadcast channel: subscription tasks publish, each WebSocket client gets a receiver.
    let (msg_tx, _) = broadcast::channel::<ServerMessage>(100);

    // mpsc channel: WebSocket handlers send subscribe requests to the subscription manager.
    let (subscribe_tx, subscribe_rx) = mpsc::channel::<SubscribeRequest>(32);

    // Shared cache of historical bars per symbol, populated by subscription_manager.
    let bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Shared cache of positions per symbol, populated by positions subscription.
    let position_cache: Arc<RwLock<HashMap<String, PositionData>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Spawn subscription manager that connects to IBKR and handles subscribe requests.
    let msg_tx_clone = msg_tx.clone();
    let bar_cache_clone = bar_cache.clone();
    let position_cache_clone = position_cache.clone();
    let subscribe_tx_clone = subscribe_tx.clone();
    tokio::spawn(async move {
        subscription_manager(subscribe_rx, subscribe_tx_clone, msg_tx_clone, bar_cache_clone, position_cache_clone)
            .await;
    });

    let state = AppState {
        msg_tx,
        subscribe_tx,
        bar_cache,
        position_cache,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    info!("WebSocket server listening on ws://127.0.0.1:3000/ws");
    axum::serve(listener, app).await?;

    Ok(())
}

/// Connects to IB Gateway once and manages subscriptions for all requested contracts.
/// For each unique contract, spawns a tick-by-tick task and a one-shot historical data fetch.
async fn subscription_manager(
    mut subscribe_rx: mpsc::Receiver<SubscribeRequest>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
    msg_tx: broadcast::Sender<ServerMessage>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
) {
    let client = match Client::connect("127.0.0.1:7496", 42).await {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("Failed to connect to IB Gateway: {e}");
            return;
        }
    };

    info!("Connected to IBKR, waiting for subscription requests");

    // Spawn positions subscription — streams all current positions then live updates.
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
        };

        if !subscribed.insert(key) {
            info!(
                "Already subscribed to {} ({}) on {}",
                req.symbol, req.security_type, req.exchange
            );
            continue;
        }

        let contract = Contract {
            symbol: Symbol(req.symbol.clone()),
            security_type: SecurityType::from(req.security_type.as_str()),
            exchange: Exchange(req.exchange),
            currency: Currency(req.currency),
            ..Default::default()
        };
        let symbol = req.symbol.clone();

        // Spawn tick-by-tick bid/ask streaming task
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

        // Spawn streaming historical data task (initial bars + live updates)
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

                                // Update cache: replace last bar if same timestamp, else append
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
                                info!("{symbol_hist}: historical streaming ended");
                                break;
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

/// Handles WebSocket upgrade at GET /ws; passes broadcast receiver and subscribe channel.
async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    let rx = state.msg_tx.subscribe();
    let subscribe_tx = state.subscribe_tx.clone();
    let bar_cache = state.bar_cache.clone();
    let position_cache = state.position_cache.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, rx, subscribe_tx, bar_cache, position_cache))
}

/// Bidirectional WebSocket handler:
/// - Forwards ServerMessages from broadcast channel to client
/// - Reads subscribe requests from client and forwards to subscription manager
async fn handle_socket(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<ServerMessage>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
) {
    // Send cached positions to newly connected client.
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
