//! WebSocket server that streams real-time price updates from Interactive Brokers (IBKR)
//! to connected clients. Connects to IB Gateway for tick-by-tick bid/ask data and
//! broadcasts best bid/ask and last price over WebSocket at ws://127.0.0.1:3000/ws.

use anyhow::{Context, Result};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use ibapi::contracts::Contract;
use ibapi::market_data::realtime::BidAsk;
use ibapi::prelude::*;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    // Broadcast channel: IBKR task publishes, each WebSocket client gets a subscriber.
    let (tx, _) = broadcast::channel::<PriceUpdate>(100);
    let tx = Arc::new(tx);

    // Run the IBKR connection and bar subscription in a background task.
    let tx_clone = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = ibkr_task(tx_clone).await {
            error!("IBKR task failed: {}", e);
        }
    });

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(tx.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    info!("WebSocket server listening on ws://127.0.0.1:3000/ws");
    axum::serve(listener, app).await?;

    Ok(())
}

/// Connects to IB Gateway, subscribes to tick-by-tick bid/ask for BHP on ASX,
/// and broadcasts each tick as best bid/ask price and size to WebSocket subscribers.
async fn ibkr_task(tx: Arc<broadcast::Sender<PriceUpdate>>) -> Result<()> {
    let client = Client::connect("127.0.0.1:7496", 42).await
        .context("Failed to connect to IB Gateway")?;

    info!("Connected to IBKR");

    let contract = Contract {
        symbol: Symbol("BHP".to_string()),
        security_type: SecurityType::Stock,
        exchange: Exchange("ASX".to_string()),
        currency: Currency("AUD".to_string()),
        ..Default::default()
    };

    // Tick-by-tick bid/ask: each event is top-of-book best bid & ask (and sizes).
    // number_of_ticks: 0 = no historical ticks, ignore_size: false = include sizes.
    let mut subscription = client
        .tick_by_tick_bid_ask(&contract, 0, false)
        .await?;

    info!("Subscribed to tick-by-tick bid/ask for BHP.ASX");

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
                    symbol: "BHP".to_string(),
                    last_price,
                    timestamp: timestamp_ms,
                    best_bid_price: bid_price,
                    best_bid_size: bid_size,
                    best_ask_price: ask_price,
                    best_ask_size: ask_size,
                };

                let _ = tx.send(update.clone());
                info!(
                    "BHP tick: bid {} x {} | ask {} x {} @ {}",
                    update.best_bid_price,
                    update.best_bid_size,
                    update.best_ask_price,
                    update.best_ask_size,
                    update.timestamp
                );
            }
            Err(e) => {
                error!("Tick subscription error: {}", e);
                break;
            }
        }
    }

    Ok(())
}

/// Handles WebSocket upgrade at GET /ws; passes a broadcast receiver into the socket handler.
async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(tx): axum::extract::State<Arc<broadcast::Sender<PriceUpdate>>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, tx.subscribe()))
}

/// For a single WebSocket connection: receives PriceUpdates from the broadcast channel
/// and sends them as JSON text messages. Stops when send fails (e.g. client disconnect).
async fn handle_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<PriceUpdate>) {
    while let Ok(update) = rx.recv().await {
        if let Ok(json) = serde_json::to_string(&update) {
            if socket.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    }
}