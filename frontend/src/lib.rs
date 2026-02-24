use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use leptos::prelude::*;
use send_wrapper::SendWrapper;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct PriceUpdate {
    symbol: String,
    last_price: f64,
    timestamp: u64,
    best_bid_price: f64,
    best_bid_size: f64,
    best_ask_price: f64,
    best_ask_size: f64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct BarData {
    timestamp: u64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct PositionData {
    symbol: String,
    position_size: f64,
    average_cost: f64,
    account: String,
}

/// Tagged enum matching the backend's ServerMessage.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    PriceUpdate(PriceUpdate),
    HistoricalBars { symbol: String, bars: Vec<BarData> },
    RealtimeBar { symbol: String, bar: BarData },
    PositionUpdate(PositionData),
}

#[derive(Clone, Debug, Serialize)]
struct SubscribeRequest {
    r#type: String,
    symbol: String,
    security_type: String,
    exchange: String,
    currency: String,
}

#[derive(Serialize)]
struct RequestBarsMsg {
    r#type: String,
    symbol: String,
}

// ---------------------------------------------------------------------------
// ApexCharts JS interop helpers
// ---------------------------------------------------------------------------

fn create_apex_chart(el: &JsValue, options: &JsValue) -> JsValue {
    let global = js_sys::global();
    let apex_class: js_sys::Function = js_sys::Reflect::get(&global, &"ApexCharts".into())
        .unwrap()
        .dyn_into()
        .unwrap();
    let chart =
        js_sys::Reflect::construct(&apex_class, &js_sys::Array::of2(el, options)).unwrap();
    let render: js_sys::Function = js_sys::Reflect::get(&chart, &"render".into())
        .unwrap()
        .dyn_into()
        .unwrap();
    let _ = render.call0(&chart);
    chart
}

fn destroy_apex_chart(chart: &JsValue) {
    if let Ok(f) = js_sys::Reflect::get(chart, &"destroy".into())
        .and_then(|v| v.dyn_into::<js_sys::Function>())
    {
        let _ = f.call0(chart);
    }
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

#[component]
fn PriceCard(price: PriceUpdate, position: Option<PositionData>) -> impl IntoView {
    let ts_secs = (price.timestamp / 1000) as f64;
    let date = js_sys::Date::new(&JsValue::from_f64(ts_secs * 1000.0));
    let time_str = format!(
        "{:02}:{:02}:{:02}",
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds(),
    );

    let position_section = position.map(|pos| {
        let size_color = if pos.position_size >= 0.0 { "#16a34a" } else { "#dc2626" };
        let size_str = if pos.position_size == pos.position_size.trunc() {
            format!("{:+.0}", pos.position_size)
        } else {
            format!("{:+}", pos.position_size)
        };
        view! {
            <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 12px; margin-top: 12px;">
                <div style="background: #f0f4ff; border-radius: 8px; padding: 12px;">
                    <div style="font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Position"</div>
                    <div style=format!("font-size: 1.25em; font-weight: 600; color: {size_color};")>
                        {size_str}
                    </div>
                </div>
                <div style="background: #f0f4ff; border-radius: 8px; padding: 12px;">
                    <div style="font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Avg Cost"</div>
                    <div style="font-size: 1.25em; font-weight: 600; color: #333;">
                        {format!("{:.2}", pos.average_cost)}
                    </div>
                </div>
            </div>
        }
    });

    view! {
        <div style="background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 24px; box-shadow: 0 1px 3px rgba(0,0,0,0.08);">
            <div style="display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 16px;">
                <h2 style="margin: 0; font-size: 1.6em;">{price.symbol.clone()}</h2>
                <span style="color: #888; font-size: 0.85em;">{time_str}</span>
            </div>

            <div style="font-size: 2em; font-weight: 700; margin-bottom: 20px;">
                {format!("{:.2}", price.last_price)}
            </div>

            <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 12px;">
                <div style="background: #f0fdf4; border-radius: 8px; padding: 12px;">
                    <div style="font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Bid"</div>
                    <div style="font-size: 1.25em; font-weight: 600; color: #16a34a;">
                        {format!("{:.2}", price.best_bid_price)}
                    </div>
                    <div style="font-size: 0.85em; color: #666;">
                        {format!("Size: {:.0}", price.best_bid_size)}
                    </div>
                </div>
                <div style="background: #fef2f2; border-radius: 8px; padding: 12px;">
                    <div style="font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Ask"</div>
                    <div style="font-size: 1.25em; font-weight: 600; color: #dc2626;">
                        {format!("{:.2}", price.best_ask_price)}
                    </div>
                    <div style="font-size: 0.85em; color: #666;">
                        {format!("Size: {:.0}", price.best_ask_size)}
                    </div>
                </div>
            </div>

            {position_section}
        </div>
    }
}

#[component]
fn CandlestickChart(
    symbol: String,
    bar_history: RwSignal<HashMap<String, Vec<BarData>>>,
) -> impl IntoView {
    let chart_id = format!("chart-{}", symbol);
    let chart_instance: Rc<RefCell<Option<JsValue>>> = Rc::new(RefCell::new(None));
    let prev_fingerprint: Rc<RefCell<(usize, u64, u64)>> = Rc::new(RefCell::new((0, 0, 0)));
    let chart_ready = RwSignal::new(false);

    // Effect: create/update chart when bar data for this symbol changes
    {
        let ci = chart_instance.clone();
        let pfp = prev_fingerprint;
        let id = chart_id.clone();
        let sym = symbol.clone();

        Effect::new(move |_| {
            let bars = bar_history.get().get(&sym).cloned().unwrap_or_default();
            let fingerprint = (
                bars.len(),
                bars.last().map_or(0, |b| b.close.to_bits()),
                bars.last().map_or(0, |b| b.timestamp),
            );

            // Recreate chart when bar count, last close, or last timestamp changes
            if fingerprint == *pfp.borrow() {
                return;
            }
            *pfp.borrow_mut() = fingerprint;

            // Destroy previous chart instance
            if let Some(old) = ci.borrow_mut().take() {
                destroy_apex_chart(&old);
                chart_ready.set(false);
            }

            if bars.is_empty() {
                return;
            }

            let window = web_sys::window().unwrap();
            let document = window.document().unwrap();
            if let Some(el) = document.get_element_by_id(&id) {
                let series_data: Vec<serde_json::Value> = bars
                    .iter()
                    .map(|b| {
                        serde_json::json!({
                            "x": b.timestamp,
                            "y": [b.open, b.high, b.low, b.close]
                        })
                    })
                    .collect();

                let options = serde_json::json!({
                    "chart": {
                        "type": "candlestick",
                        "height": 300,
                    },
                    "series": [{
                        "name": &sym,
                        "data": series_data,
                    }],
                    "xaxis": { "type": "datetime" },
                    "yaxis": { "tooltip": { "enabled": true } },
                    "plotOptions": {
                        "candlestick": {
                            "colors": {
                                "upward": "#22c55e",
                                "downward": "#ef4444",
                            }
                        }
                    },
                    "grid": { "borderColor": "#e5e7eb" },
                    "title": {
                        "text": format!("{} - 1 Min", &sym),
                        "align": "left",
                        "style": { "fontSize": "14px", "color": "#333" },
                    },
                });

                let opts_str = serde_json::to_string(&options).unwrap();
                let opts_js = js_sys::JSON::parse(&opts_str).unwrap();
                let el_js: JsValue = el.into();
                let chart = create_apex_chart(&el_js, &opts_js);
                *ci.borrow_mut() = Some(chart);
                chart_ready.set(true);
            }
        });
    }

    // Cleanup: destroy chart when component unmounts
    // SendWrapper is safe here because WASM is single-threaded.
    {
        let ci = SendWrapper::new(chart_instance);
        on_cleanup(move || {
            if let Some(chart) = ci.borrow_mut().take() {
                destroy_apex_chart(&chart);
            }
        });
    }

    view! {
        <div style="min-height: 300px; position: relative; background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; overflow: hidden;">
            <div id=chart_id></div>
            <div style=move || {
                if chart_ready.get() {
                    "display: none;"
                } else {
                    "position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; color: #999; font-size: 0.9em;"
                }
            }>
                "Loading chart data..."
            </div>
        </div>
    }
}

#[component]
fn ContractRow(
    symbol: String,
    prices: RwSignal<HashMap<String, PriceUpdate>>,
    bar_history: RwSignal<HashMap<String, Vec<BarData>>>,
    positions: RwSignal<HashMap<String, PositionData>>,
) -> impl IntoView {
    let sym_for_price = symbol.clone();
    let sym_for_chart = symbol.clone();

    view! {
        <div style="display: flex; gap: 16px; align-items: flex-start;">
            <div style="flex-shrink: 0; width: 320px;">
                {move || {
                    prices.get().get(&sym_for_price).cloned().map(|p| {
                        let pos = positions.get().get(&sym_for_price).cloned();
                        view! { <PriceCard price=p position=pos /> }
                    })
                }}
            </div>
            <div style="flex: 1; min-width: 0;">
                <CandlestickChart symbol=sym_for_chart bar_history=bar_history />
            </div>
        </div>
    }
}

#[component]
fn App() -> impl IntoView {
    let prices: RwSignal<HashMap<String, PriceUpdate>> = RwSignal::new(HashMap::new());
    let bar_history: RwSignal<HashMap<String, Vec<BarData>>> = RwSignal::new(HashMap::new());
    let positions: RwSignal<HashMap<String, PositionData>> = RwSignal::new(HashMap::new());
    let connected = RwSignal::new(false);

    // Form input signals with defaults
    let symbol_input = RwSignal::new(String::new());
    let sec_type_input = RwSignal::new("STK".to_string());
    let exchange_input = RwSignal::new("SMART".to_string());
    let currency_input = RwSignal::new("USD".to_string());

    // Store WebSocket reference for sending messages
    let ws_ref: Rc<RefCell<Option<WebSocket>>> = Rc::new(RefCell::new(None));

    // Open WebSocket inside an effect so it runs once on mount.
    let ws_ref_clone = ws_ref.clone();
    Effect::new(move |_| {
        let ws = WebSocket::new("ws://127.0.0.1:3000/ws").expect("failed to create WebSocket");

        // Store reference for sending
        *ws_ref_clone.borrow_mut() = Some(ws.clone());

        // --- onopen ---
        let onopen = Closure::<dyn Fn()>::new(move || {
            leptos::logging::log!("WebSocket connected");
            connected.set(true);
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        // --- onmessage ---
        let onmessage = Closure::<dyn Fn(MessageEvent)>::new(move |e: MessageEvent| {
            if let Some(text) = e.data().as_string() {
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(ServerMessage::PriceUpdate(update)) => {
                        prices.update(|map| {
                            map.insert(update.symbol.clone(), update);
                        });
                    }
                    Ok(ServerMessage::HistoricalBars { symbol, mut bars }) => {
                        bars.sort_by_key(|b| b.timestamp);
                        leptos::logging::log!(
                            "Received {} historical bars for {}",
                            bars.len(),
                            symbol
                        );
                        bar_history.update(|map| {
                            map.insert(symbol, bars);
                        });
                    }
                    Ok(ServerMessage::RealtimeBar { symbol, bar }) => {
                        bar_history.update(|map| {
                            let bars = map.entry(symbol).or_default();
                            if let Some(last) = bars.last_mut() {
                                if last.timestamp == bar.timestamp {
                                    *last = bar;
                                } else {
                                    bars.push(bar);
                                }
                            } else {
                                bars.push(bar);
                            }
                        });
                    }
                    Ok(ServerMessage::PositionUpdate(pos)) => {
                        positions.update(|map| {
                            map.insert(pos.symbol.clone(), pos);
                        });
                    }
                    Err(err) => leptos::logging::log!("parse error: {err}"),
                }
            }
        });
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        // --- onerror ---
        let onerror = Closure::<dyn Fn(JsValue)>::new(move |e: JsValue| {
            leptos::logging::log!("WebSocket error: {:?}", e);
            connected.set(false);
        });
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();

        // --- onclose ---
        let onclose = Closure::<dyn Fn(CloseEvent)>::new(move |_: CloseEvent| {
            leptos::logging::log!("WebSocket closed");
            connected.set(false);
        });
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();

        // Keep the WebSocket alive — dropping it would release the JS reference.
        std::mem::forget(ws);
    });

    // Auto-request historical bars for symbols that have price data but no chart data.
    // This handles reconnection: after a page refresh, incoming PriceUpdates reveal
    // which symbols are active, and this effect requests their cached bars from the backend.
    let requested_bars: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
    let ws_ref_bars = ws_ref.clone();
    let requested_bars_clone = requested_bars.clone();
    Effect::new(move |_| {
        let price_map = prices.get();
        let bar_map = bar_history.get();
        let mut requested = requested_bars_clone.borrow_mut();

        for symbol in price_map.keys() {
            if !bar_map.contains_key(symbol) && requested.insert(symbol.clone()) {
                let msg = RequestBarsMsg {
                    r#type: "request_bars".to_string(),
                    symbol: symbol.clone(),
                };
                if let Ok(json) = serde_json::to_string(&msg) {
                    if let Some(ws) = ws_ref_bars.borrow().as_ref() {
                        let _ = ws.send_with_str(&json);
                        leptos::logging::log!("Requested cached bars for {}", symbol);
                    }
                }
            }
        }
    });

    // Submit handler: send subscribe request over WebSocket
    let ws_ref_submit = ws_ref.clone();
    let on_subscribe = move |_| {
        let symbol = symbol_input.get().trim().to_uppercase();
        if symbol.is_empty() {
            return;
        }
        let req = SubscribeRequest {
            r#type: "subscribe".to_string(),
            symbol,
            security_type: sec_type_input.get(),
            exchange: exchange_input.get().trim().to_uppercase(),
            currency: currency_input.get().trim().to_uppercase(),
        };
        if let Ok(json) = serde_json::to_string(&req) {
            if let Some(ws) = ws_ref_submit.borrow().as_ref() {
                let _ = ws.send_with_str(&json);
            }
        }
        symbol_input.set(String::new());
    };

    // Sorted symbol list — only changes when a new contract is added/removed.
    // Price ticks update values but not keys, so this memo stays stable.
    let sorted_symbols = Memo::new(move |_| {
        let map = prices.get();
        let mut symbols: Vec<String> = map.keys().cloned().collect();
        symbols.sort();
        symbols
    });

    view! {
        <div style="font-family: 'Segoe UI', system-ui, sans-serif; max-width: 1200px; margin: 40px auto; padding: 24px;">
            // Connection status
            <div style="margin-bottom: 20px; display: flex; align-items: center; gap: 8px;">
                <span style=move || {
                    let color = if connected.get() { "#22c55e" } else { "#ef4444" };
                    format!(
                        "display:inline-block;width:10px;height:10px;border-radius:50%;background:{color}",
                    )
                }></span>
                <span style="font-size: 0.9em; color: #666;">
                    {move || if connected.get() { "Connected" } else { "Disconnected" }}
                </span>
            </div>

            // Subscription form
            <div style="background: #f9fafb; border: 1px solid #e5e7eb; border-radius: 12px; padding: 20px; margin-bottom: 24px;">
                <h3 style="margin: 0 0 16px 0; font-size: 1.1em; color: #333;">"Subscribe to Contract"</h3>
                <div style="display: flex; gap: 10px; flex-wrap: wrap; align-items: end;">
                    <div style="flex: 1; min-width: 100px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Symbol"</label>
                        <input
                            type="text"
                            placeholder="e.g. BHP"
                            prop:value=move || symbol_input.get()
                            on:input=move |e| symbol_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; box-sizing: border-box;"
                        />
                    </div>
                    <div style="min-width: 90px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Type"</label>
                        <select
                            prop:value=move || sec_type_input.get()
                            on:change=move |e| sec_type_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; background: #fff; box-sizing: border-box;"
                        >
                            <option value="STK">"STK"</option>
                            <option value="OPT">"OPT"</option>
                            <option value="FUT">"FUT"</option>
                            <option value="IND">"IND"</option>
                            <option value="CASH">"CASH"</option>
                            <option value="CRYPTO">"CRYPTO"</option>
                            <option value="CFD">"CFD"</option>
                        </select>
                    </div>
                    <div style="flex: 1; min-width: 100px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Exchange"</label>
                        <input
                            type="text"
                            prop:value=move || exchange_input.get()
                            on:input=move |e| exchange_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; box-sizing: border-box;"
                        />
                    </div>
                    <div style="min-width: 80px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Currency"</label>
                        <input
                            type="text"
                            prop:value=move || currency_input.get()
                            on:input=move |e| currency_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; box-sizing: border-box;"
                        />
                    </div>
                    <button
                        on:click=on_subscribe
                        style="padding: 8px 20px; background: #2563eb; color: #fff; border: none; border-radius: 6px; font-size: 0.95em; cursor: pointer; white-space: nowrap;"
                    >
                        "Subscribe"
                    </button>
                </div>
            </div>

            // Contract rows (price card + candlestick chart)
            {move || {
                let symbols = sorted_symbols.get();
                if symbols.is_empty() {
                    view! {
                        <div style="text-align: center; padding: 60px 0; color: #999;">
                            "No subscriptions yet. Add a contract above."
                        </div>
                    }
                        .into_any()
                } else {
                    view! {
                        <div style="display: flex; flex-direction: column; gap: 16px;">
                            {symbols
                                .into_iter()
                                .map(|symbol| {
                                    view! {
                                        <ContractRow
                                            symbol=symbol
                                            prices=prices
                                            bar_history=bar_history
                                            positions=positions
                                        />
                                    }
                                })
                                .collect::<Vec<_>>()}
                        </div>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

#[wasm_bindgen(start)]
pub fn run() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
