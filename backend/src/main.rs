//! WebSocket server that streams real-time price updates from Interactive Brokers (IBKR)
//! to connected clients. Connects to IB Gateway for tick-by-tick bid/ask data and
//! broadcasts best bid/ask and last price over WebSocket at ws://127.0.0.1:3000/ws.
//!
//! Clients send subscribe requests (symbol, security_type, exchange, currency) and the
//! server manages one subscription per unique contract, broadcasting updates to all clients.

use anyhow::Result;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use ibapi::contracts::Contract;
use ibapi::market_data::realtime::BidAsk;
use ibapi::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info};

/// Payload sent to WebSocket clients for each tick update (best bid/ask from top of book).
#[derive(Clone, Debug, Serialize)]
struct PriceUpdate {
    symbol: String,
    /// Mid price (bid + ask) / 2 for display; tick stream is bid/ask only.
    last_price: f64,
    timestamp: u64, // Unix time in milliseconds
    best_bid_price: f64,
    best_bid_size: f64,
    best_ask_price: f64,
    best_ask_size: f64,
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
}

/// Shared application state passed to all handlers.
#[derive(Clone)]
struct AppState {
    price_tx: broadcast::Sender<PriceUpdate>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    // Broadcast channel: subscription tasks publish, each WebSocket client gets a receiver.
    let (price_tx, _) = broadcast::channel::<PriceUpdate>(100);

    // mpsc channel: WebSocket handlers send subscribe requests to the subscription manager.
    let (subscribe_tx, subscribe_rx) = mpsc::channel::<SubscribeRequest>(32);

    // Spawn subscription manager that connects to IBKR and handles subscribe requests.
    let price_tx_clone = price_tx.clone();
    tokio::spawn(async move {
        subscription_manager(subscribe_rx, price_tx_clone).await;
    });

    let state = AppState {
        price_tx,
        subscribe_tx,
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
/// Listens for `SubscribeRequest` messages; for each unique contract, spawns a task
/// that streams tick-by-tick bid/ask data into the broadcast channel.
async fn subscription_manager(
    mut subscribe_rx: mpsc::Receiver<SubscribeRequest>,
    price_tx: broadcast::Sender<PriceUpdate>,
) {
    let client = match Client::connect("127.0.0.1:7496", 42).await {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("Failed to connect to IB Gateway: {e}");
            return;
        }
    };

    info!("Connected to IBKR, waiting for subscription requests");

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

        let client = client.clone();
        let price_tx = price_tx.clone();
        let symbol = req.symbol.clone();

        tokio::spawn(async move {
            let contract = Contract {
                symbol: Symbol(req.symbol.clone()),
                security_type: SecurityType::from(req.security_type.as_str()),
                exchange: Exchange(req.exchange),
                currency: Currency(req.currency),
                ..Default::default()
            };

            match client.tick_by_tick_bid_ask(&contract, 0, false).await {
                Ok(mut subscription) => {
                    info!("Subscribed to tick-by-tick bid/ask for {symbol}");

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

                                let update = PriceUpdate {
                                    symbol: symbol.clone(),
                                    last_price,
                                    timestamp: timestamp_ms,
                                    best_bid_price: bid_price,
                                    best_bid_size: bid_size,
                                    best_ask_price: ask_price,
                                    best_ask_size: ask_size,
                                };

                                let _ = price_tx.send(update.clone());
                                info!(
                                    "{symbol} tick: bid {bid_price} x {bid_size} | ask {ask_price} x {ask_size} @ {timestamp_ms}"
                                );
                            }
                            Err(e) => {
                                error!("{symbol} tick subscription error: {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to subscribe to {symbol}: {e}");
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
    let rx = state.price_tx.subscribe();
    let subscribe_tx = state.subscribe_tx.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, rx, subscribe_tx))
}

/// Bidirectional WebSocket handler:
/// - Forwards PriceUpdates from broadcast channel to client
/// - Reads subscribe requests from client and forwards to subscription manager
async fn handle_socket(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<PriceUpdate>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
) {
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(update) => {
                        if let Ok(json) = serde_json::to_string(&update) {
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
