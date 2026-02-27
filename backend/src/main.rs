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

mod order;

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
use ibapi::orders::{Action, OrderBuilder, PlaceOrder as IbPlaceOrder};
use ibapi::prelude::*;
use ibapi::orders::CancelOrder as IbCancelOrder;
use order::{map_cancel_order_to_event, map_ibkr_to_event, OrderEvent, OrderStateMachine};
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

/// Client-submitted order with contract identification and order parameters.
#[derive(Clone, Debug, Deserialize)]
struct OrderRequest {
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
    action: String,     // "BUY" or "SELL"
    order_type: String, // "MKT", "LMT", "STP"
    quantity: f64,
    #[serde(default)]
    limit_price: Option<f64>,
    #[serde(default)]
    stop_price: Option<f64>,
    #[serde(default)]
    time_in_force: String, // "DAY", "GTC", "IOC" — defaults to DAY
}

/// Client-submitted modification: updates quantity and/or prices for an existing order.
#[derive(Clone, Debug, Deserialize)]
struct ModifyOrderRequest {
    order_id: i32,
    quantity: f64,
    #[serde(default)]
    limit_price: Option<f64>,
    #[serde(default)]
    stop_price: Option<f64>,
}

/// Tracks order lifecycle from submission through fill/cancel.
#[derive(Clone, Debug, Serialize)]
struct OrderUpdate {
    order_id: i32,
    symbol: String,
    action: String,
    order_type: String,
    quantity: f64,
    limit_price: Option<f64>,
    stop_price: Option<f64>,
    status: String,
    filled: f64,
    remaining: f64,
    average_fill_price: f64,
}

/// Order metadata used to reconstruct `OrderUpdate` from the state machine and to
/// rebuild IBKR orders on modification.
#[derive(Clone, Debug)]
struct OrderStaticFields {
    symbol: String,
    action: String,
    order_type: String,
    quantity: f64,
    limit_price: Option<f64>,
    stop_price: Option<f64>,
    time_in_force: String,
}

fn build_order_update(
    order_id: i32,
    sf: &OrderStaticFields,
    sm: &OrderStateMachine,
) -> OrderUpdate {
    OrderUpdate {
        order_id,
        symbol: sf.symbol.clone(),
        action: sf.action.clone(),
        order_type: sf.order_type.clone(),
        quantity: sf.quantity,
        limit_price: sf.limit_price,
        stop_price: sf.stop_price,
        status: sm.state.as_str().to_string(),
        filled: sm.filled,
        remaining: sm.remaining,
        average_fill_price: sm.avg_fill_price,
    }
}

/// Funnels place, cancel, and modify requests through a single channel so the
/// subscription manager (which owns the ibapi Client) processes them sequentially.
enum OrderCommand {
    Place(OrderRequest),
    Cancel { order_id: i32 },
    Modify(ModifyOrderRequest),
}

/// Bundles the state machine with its static metadata so the shared store has
/// everything needed to build `OrderUpdate`s and rebuild IBKR orders on modify.
struct OrderEntry {
    sm: OrderStateMachine,
    static_fields: OrderStaticFields,
}

type OrderStore = Arc<RwLock<HashMap<i32, OrderEntry>>>;

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
    OrderUpdate(OrderUpdate),
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
    PlaceOrder(OrderRequest),
    CancelOrder { order_id: i32 },
    ModifyOrder(ModifyOrderRequest),
}

/// Caches use `Arc<RwLock<>>` because many WebSocket tasks read concurrently while only
/// the subscription manager writes.
#[derive(Clone)]
struct AppState {
    msg_tx: broadcast::Sender<ServerMessage>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
    order_tx: mpsc::Sender<OrderCommand>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
    order_cache: Arc<RwLock<HashMap<i32, OrderUpdate>>>,
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

    // Order requests funneled to the subscription manager where the ibapi Client lives.
    let (order_tx, order_rx) = mpsc::channel::<OrderCommand>(32);

    // Cached server-side so clients connecting after the initial broadcast still get full bar history.
    let bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Cached so late-joining clients see current positions without waiting for the next IBKR update.
    let position_cache: Arc<RwLock<HashMap<String, PositionData>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Per-contract autotrader settings, cached so late-joining clients get current configs.
    let config_cache: Arc<RwLock<HashMap<String, ContractConfig>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Cached so late-joining clients see active/recent orders.
    let order_cache: Arc<RwLock<HashMap<i32, OrderUpdate>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Maps symbol → Contract so order submission can look up contract details.
    let contract_cache: Arc<RwLock<HashMap<String, Contract>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Runs in a dedicated task so the HTTP server can start accepting connections immediately.
    let msg_tx_clone = msg_tx.clone();
    let bar_cache_clone = bar_cache.clone();
    let position_cache_clone = position_cache.clone();
    let config_cache_clone = config_cache.clone();
    let order_cache_clone = order_cache.clone();
    let contract_cache_clone = contract_cache.clone();
    let subscribe_tx_clone = subscribe_tx.clone();
    tokio::spawn(async move {
        subscription_manager(subscribe_rx, subscribe_tx_clone, order_rx, msg_tx_clone, bar_cache_clone, position_cache_clone, config_cache_clone, order_cache_clone, contract_cache_clone)
            .await;
    });

    let state = AppState {
        msg_tx,
        subscribe_tx,
        order_tx,
        bar_cache,
        position_cache,
        config_cache,
        order_cache,
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
    mut order_rx: mpsc::Receiver<OrderCommand>,
    msg_tx: broadcast::Sender<ServerMessage>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
    order_cache: Arc<RwLock<HashMap<i32, OrderUpdate>>>,
    contract_cache: Arc<RwLock<HashMap<String, Contract>>>,
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
    let order_store: OrderStore = Arc::new(RwLock::new(HashMap::new()));

    loop {
        tokio::select! {
            Some(req) = subscribe_rx.recv() => {
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

                contract_cache.write().await.insert(symbol.clone(), contract.clone());

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
            Some(cmd) = order_rx.recv() => {
                match cmd {
                    OrderCommand::Place(req) => {
                        // Prefer cached contract; fall back to building from request fields.
                        let contract = if let Some(cached) = contract_cache.read().await.get(&req.symbol) {
                            cached.clone()
                        } else {
                            Contract {
                                contract_id: req.contract_id,
                                symbol: Symbol(req.symbol.clone()),
                                security_type: SecurityType::from(req.security_type.as_str()),
                                exchange: Exchange(req.exchange.clone()),
                                currency: Currency(req.currency.clone()),
                                primary_exchange: Exchange(req.primary_exchange.clone()),
                                last_trade_date_or_contract_month: req.last_trade_date_or_contract_month.clone(),
                                strike: req.strike,
                                right: req.right.clone(),
                                ..Default::default()
                            }
                        };

                        let action = match req.action.to_uppercase().as_str() {
                            "SELL" => Action::Sell,
                            _ => Action::Buy,
                        };

                        let builder = OrderBuilder::new(client.as_ref(), &contract);
                        let builder = match action {
                            Action::Buy => builder.buy(req.quantity),
                            _ => builder.sell(req.quantity),
                        };
                        let builder = match req.order_type.to_uppercase().as_str() {
                            "LMT" => builder.limit(req.limit_price.unwrap_or(0.0)),
                            "STP" => builder.stop(req.stop_price.unwrap_or(0.0)),
                            _ => builder.market(),
                        };
                        let tif = if req.time_in_force.is_empty() { "DAY".to_string() } else { req.time_in_force.to_uppercase() };
                        let builder = match tif.as_str() {
                            "GTC" => builder.good_till_cancel(),
                            "IOC" => builder.immediate_or_cancel(),
                            _ => builder.day_order(),
                        };

                        let order = match builder.build() {
                            Ok(o) => o,
                            Err(e) => {
                                error!("Failed to build order for {}: {e}", req.symbol);
                                continue;
                            }
                        };

                        let order_id = client.next_order_id();
                        info!(
                            "Placing {} {} order #{order_id} for {} x {} {}",
                            req.order_type, req.action, req.quantity, req.symbol,
                            req.limit_price.map(|p| format!("@ {p}")).unwrap_or_default()
                        );

                        let static_fields = OrderStaticFields {
                            symbol: req.symbol.clone(),
                            action: req.action.clone(),
                            order_type: req.order_type.clone(),
                            quantity: req.quantity,
                            limit_price: req.limit_price,
                            stop_price: req.stop_price,
                            time_in_force: tif,
                        };

                        let mut sm = OrderStateMachine::new(order_id, req.quantity);
                        let _ = sm.apply(OrderEvent::Submit);

                        let update = build_order_update(order_id, &static_fields, &sm);
                        order_cache.write().await.insert(order_id, update.clone());
                        let _ = msg_tx.send(ServerMessage::OrderUpdate(update));

                        order_store.write().await.insert(order_id, OrderEntry {
                            sm,
                            static_fields: static_fields.clone(),
                        });

                        spawn_order_monitor(
                            order_id,
                            &client,
                            &contract,
                            &order,
                            order_store.clone(),
                            order_cache.clone(),
                            msg_tx.clone(),
                        );
                    }
                    OrderCommand::Cancel { order_id } => {
                        // Validate order exists and is not terminal.
                        {
                            let mut store = order_store.write().await;
                            match store.get_mut(&order_id) {
                                Some(entry) if entry.sm.state.is_terminal() => {
                                    info!("Order #{order_id}: ignoring cancel — already {}", entry.sm.state.as_str());
                                    continue;
                                }
                                Some(entry) => {
                                    if entry.sm.apply(OrderEvent::CancelRequested).is_ok() {
                                        let update = build_order_update(order_id, &entry.static_fields, &entry.sm);
                                        order_cache.write().await.insert(order_id, update.clone());
                                        let _ = msg_tx.send(ServerMessage::OrderUpdate(update));
                                    }
                                }
                                None => {
                                    info!("Order #{order_id}: ignoring cancel — not found");
                                    continue;
                                }
                            }
                        }

                        let client_cancel = client.clone();
                        let order_store_cancel = order_store.clone();
                        let order_cache_cancel = order_cache.clone();
                        let msg_tx_cancel = msg_tx.clone();
                        tokio::spawn(async move {
                            match client_cancel.cancel_order(order_id, "").await {
                                Ok(mut subscription) => {
                                    while let Some(result) = subscription.next().await {
                                        match result {
                                            Ok(ref cancel_event) => {
                                                if let IbCancelOrder::Notice(notice) = cancel_event {
                                                    info!("Order #{order_id} cancel notice: {notice}");
                                                }
                                                if let Some(event) = map_cancel_order_to_event(cancel_event) {
                                                    let mut store = order_store_cancel.write().await;
                                                    if let Some(entry) = store.get_mut(&order_id) {
                                                        if entry.sm.apply(event).is_ok() {
                                                            let update = build_order_update(order_id, &entry.static_fields, &entry.sm);
                                                            order_cache_cancel.write().await.insert(order_id, update.clone());
                                                            let _ = msg_tx_cancel.send(ServerMessage::OrderUpdate(update));
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                error!("Order #{order_id} cancel error: {e}");
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to cancel order #{order_id}: {e}");
                                }
                            }
                        });
                    }
                    OrderCommand::Modify(req) => {
                        let sf_snapshot;
                        {
                            let mut store = order_store.write().await;
                            match store.get_mut(&req.order_id) {
                                Some(entry) if matches!(entry.sm.state, order::OrderState::Working | order::OrderState::PartiallyFilled) => {
                                    if entry.sm.apply(OrderEvent::AmendRequested).is_ok() {
                                        // Update static fields with new values.
                                        entry.static_fields.quantity = req.quantity;
                                        if req.limit_price.is_some() {
                                            entry.static_fields.limit_price = req.limit_price;
                                        }
                                        if req.stop_price.is_some() {
                                            entry.static_fields.stop_price = req.stop_price;
                                        }
                                        let update = build_order_update(req.order_id, &entry.static_fields, &entry.sm);
                                        order_cache.write().await.insert(req.order_id, update.clone());
                                        let _ = msg_tx.send(ServerMessage::OrderUpdate(update));
                                        sf_snapshot = entry.static_fields.clone();
                                    } else {
                                        continue;
                                    }
                                }
                                Some(entry) => {
                                    info!("Order #{}: ignoring modify — state is {}", req.order_id, entry.sm.state.as_str());
                                    continue;
                                }
                                None => {
                                    info!("Order #{}: ignoring modify — not found", req.order_id);
                                    continue;
                                }
                            }
                        }

                        // Look up contract from cache using the order's symbol.
                        let contract = match contract_cache.read().await.get(&sf_snapshot.symbol) {
                            Some(c) => c.clone(),
                            None => {
                                error!("Order #{}: cannot modify — contract for {} not in cache", req.order_id, sf_snapshot.symbol);
                                continue;
                            }
                        };

                        // Rebuild IBKR order with same action/type/tif but new qty/prices.
                        let action = match sf_snapshot.action.to_uppercase().as_str() {
                            "SELL" => Action::Sell,
                            _ => Action::Buy,
                        };
                        let builder = OrderBuilder::new(client.as_ref(), &contract);
                        let builder = match action {
                            Action::Buy => builder.buy(sf_snapshot.quantity),
                            _ => builder.sell(sf_snapshot.quantity),
                        };
                        let builder = match sf_snapshot.order_type.to_uppercase().as_str() {
                            "LMT" => builder.limit(sf_snapshot.limit_price.unwrap_or(0.0)),
                            "STP" => builder.stop(sf_snapshot.stop_price.unwrap_or(0.0)),
                            _ => builder.market(),
                        };
                        let builder = match sf_snapshot.time_in_force.as_str() {
                            "GTC" => builder.good_till_cancel(),
                            "IOC" => builder.immediate_or_cancel(),
                            _ => builder.day_order(),
                        };
                        let modified_order = match builder.build() {
                            Ok(o) => o,
                            Err(e) => {
                                error!("Failed to build modified order #{}: {e}", req.order_id);
                                continue;
                            }
                        };

                        info!(
                            "Modifying order #{} — qty={} limit={:?} stop={:?}",
                            req.order_id, sf_snapshot.quantity, sf_snapshot.limit_price, sf_snapshot.stop_price
                        );

                        // Same order_id replaces the old subscription in ibapi's message bus.
                        spawn_order_monitor(
                            req.order_id,
                            &client,
                            &contract,
                            &modified_order,
                            order_store.clone(),
                            order_cache.clone(),
                            msg_tx.clone(),
                        );
                    }
                }
            }
            else => break,
        }
    }
}

/// Spawns a task that monitors an IBKR `place_order` subscription and drives the
/// shared state machine. Used for both initial placement and modification (same
/// order_id triggers IBKR modify; the old subscription is replaced in ibapi's
/// message bus, so the old monitoring task naturally stops receiving messages).
fn spawn_order_monitor(
    order_id: i32,
    client: &Arc<Client>,
    contract: &Contract,
    order: &ibapi::orders::Order,
    order_store: OrderStore,
    order_cache: Arc<RwLock<HashMap<i32, OrderUpdate>>>,
    msg_tx: broadcast::Sender<ServerMessage>,
) {
    let client = client.clone();
    let contract = contract.clone();
    let order = order.clone();
    tokio::spawn(async move {
        match client.place_order(order_id, &contract, &order).await {
            Ok(mut subscription) => {
                while let Some(result) = subscription.next().await {
                    match result {
                        Ok(ref ibkr_event) => {
                            match ibkr_event {
                                IbPlaceOrder::ExecutionData(exec) => {
                                    info!(
                                        "Order #{order_id}: execution {} shares @ {}",
                                        exec.execution.shares, exec.execution.price
                                    );
                                }
                                IbPlaceOrder::CommissionReport(cr) => {
                                    debug!("Order #{order_id}: commission {}", cr.commission);
                                }
                                IbPlaceOrder::Message(notice) => {
                                    info!("Order #{order_id} notice: {notice}");
                                }
                                _ => {}
                            }

                            if let Some(event) = map_ibkr_to_event(ibkr_event) {
                                let mut store = order_store.write().await;
                                if let Some(entry) = store.get_mut(&order_id) {
                                    if entry.sm.apply(event).is_ok() {
                                        let update = build_order_update(order_id, &entry.static_fields, &entry.sm);
                                        order_cache.write().await.insert(order_id, update.clone());
                                        let _ = msg_tx.send(ServerMessage::OrderUpdate(update));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Order #{order_id} error: {e}");
                            let mut store = order_store.write().await;
                            if let Some(entry) = store.get_mut(&order_id) {
                                let _ = entry.sm.apply(OrderEvent::AckReject {
                                    reason: e.to_string(),
                                });
                                let update = build_order_update(order_id, &entry.static_fields, &entry.sm);
                                order_cache.write().await.insert(order_id, update.clone());
                                let _ = msg_tx.send(ServerMessage::OrderUpdate(update));
                            }
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to place order #{order_id}: {e}");
                let mut store = order_store.write().await;
                if let Some(entry) = store.get_mut(&order_id) {
                    let _ = entry.sm.apply(OrderEvent::AckReject {
                        reason: e.to_string(),
                    });
                    let update = build_order_update(order_id, &entry.static_fields, &entry.sm);
                    order_cache.write().await.insert(order_id, update.clone());
                    let _ = msg_tx.send(ServerMessage::OrderUpdate(update));
                }
            }
        }
    });
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
    let order_tx = state.order_tx.clone();
    let bar_cache = state.bar_cache.clone();
    let position_cache = state.position_cache.clone();
    let config_cache = state.config_cache.clone();
    let order_cache = state.order_cache.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, rx, msg_tx, subscribe_tx, order_tx, bar_cache, position_cache, config_cache, order_cache))
}

/// Single task handles both directions via `tokio::select!` so we detect client disconnect
/// from either side and clean up immediately, rather than coordinating two separate tasks.
async fn handle_socket(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<ServerMessage>,
    msg_tx: broadcast::Sender<ServerMessage>,
    subscribe_tx: mpsc::Sender<SubscribeRequest>,
    order_tx: mpsc::Sender<OrderCommand>,
    bar_cache: Arc<RwLock<HashMap<String, Vec<BarData>>>>,
    position_cache: Arc<RwLock<HashMap<String, PositionData>>>,
    config_cache: Arc<RwLock<HashMap<String, ContractConfig>>>,
    order_cache: Arc<RwLock<HashMap<i32, OrderUpdate>>>,
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

    {
        let cache = order_cache.read().await;
        for update in cache.values() {
            let msg = ServerMessage::OrderUpdate(update.clone());
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
                            Ok(ClientMessage::PlaceOrder(req)) => {
                                info!("Client order request: {} {} {} x {}", req.action, req.order_type, req.quantity, req.symbol);
                                let _ = order_tx.send(OrderCommand::Place(req)).await;
                            }
                            Ok(ClientMessage::CancelOrder { order_id }) => {
                                info!("Client cancel request: order #{order_id}");
                                let _ = order_tx.send(OrderCommand::Cancel { order_id }).await;
                            }
                            Ok(ClientMessage::ModifyOrder(req)) => {
                                info!("Client modify request: order #{} qty={} limit={:?} stop={:?}", req.order_id, req.quantity, req.limit_price, req.stop_price);
                                let _ = order_tx.send(OrderCommand::Modify(req)).await;
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
