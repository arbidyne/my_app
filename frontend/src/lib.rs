use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use leptos::prelude::*;
use send_wrapper::SendWrapper;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
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

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
struct ContractConfig {
    symbol: String,
    autotrade: bool,
    max_pos_size: u32,
    min_pos_size: i32,
    max_order_size: u32,
    multiplier: f64,
    lot_size: u32,
}

/// Tagged enum matching the backend's ServerMessage.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    PriceUpdate(PriceUpdate),
    HistoricalBars { symbol: String, bars: Vec<BarData> },
    RealtimeBar { symbol: String, bar: BarData },
    PositionUpdate(PositionData),
    ContractConfig(ContractConfig),
    OrderUpdate(OrderUpdate),
}

#[derive(Serialize)]
struct UpdateContractConfigMsg {
    r#type: String,
    symbol: String,
    autotrade: bool,
    max_pos_size: u32,
    min_pos_size: i32,
    max_order_size: u32,
    multiplier: f64,
    lot_size: u32,
}

#[derive(Clone, Debug, Serialize)]
struct SubscribeRequest {
    r#type: String,
    symbol: String,
    security_type: String,
    exchange: String,
    currency: String,
    primary_exchange: String,
    last_trade_date_or_contract_month: String,
    strike: f64,
    right: String,
    contract_id: i32,
}

#[derive(Serialize)]
struct RequestBarsMsg {
    r#type: String,
    symbol: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct OrderUpdate {
    client_order_id: Uuid,
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

#[derive(Serialize)]
struct PlaceOrderMsg {
    r#type: String,
    client_order_id: Uuid,
    symbol: String,
    security_type: String,
    exchange: String,
    currency: String,
    primary_exchange: String,
    last_trade_date_or_contract_month: String,
    strike: f64,
    right: String,
    contract_id: i32,
    action: String,
    order_type: String,
    quantity: f64,
    limit_price: Option<f64>,
    stop_price: Option<f64>,
    time_in_force: String,
}

#[derive(Serialize)]
struct CancelOrderMsg {
    r#type: String,
    client_order_id: Uuid,
}

#[derive(Serialize)]
struct ModifyOrderMsg {
    r#type: String,
    client_order_id: Uuid,
    quantity: f64,
    limit_price: Option<f64>,
    stop_price: Option<f64>,
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
                    "xaxis": { "type": "datetime", "labels": { "datetimeUTC": false } },
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

fn send_config_update(ws: &SendWrapper<Rc<RefCell<Option<WebSocket>>>>, cfg: &ContractConfig) {
    let msg = UpdateContractConfigMsg {
        r#type: "update_contract_config".to_string(),
        symbol: cfg.symbol.clone(),
        autotrade: cfg.autotrade,
        max_pos_size: cfg.max_pos_size,
        min_pos_size: cfg.min_pos_size,
        max_order_size: cfg.max_order_size,
        multiplier: cfg.multiplier,
        lot_size: cfg.lot_size,
    };
    if let Ok(json) = serde_json::to_string(&msg) {
        if let Some(ws) = ws.borrow().as_ref() {
            let _ = ws.send_with_str(&json);
        }
    }
}

#[component]
fn ContractRow(
    symbol: String,
    prices: RwSignal<HashMap<String, PriceUpdate>>,
    bar_history: RwSignal<HashMap<String, Vec<BarData>>>,
    positions: RwSignal<HashMap<String, PositionData>>,
    configs: RwSignal<HashMap<String, ContractConfig>>,
    orders: RwSignal<HashMap<Uuid, OrderUpdate>>,
    ws_ref: SendWrapper<Rc<RefCell<Option<WebSocket>>>>,
) -> impl IntoView {
    let sym_for_price = symbol.clone();
    let sym_for_chart = symbol.clone();
    let sym_for_config = symbol.clone();
    let sym_for_orders = symbol.clone();

    let label_style = "display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;";
    let input_style = "width: 100%; padding: 6px 10px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.9em; box-sizing: border-box;";

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
        // Config panel
        {
            let ws = ws_ref.clone();
            let sym = sym_for_config.clone();
            move || {
                configs.get().get(&sym).cloned().map(|cfg| {
                    let sym = sym.clone();
                    let ws = ws.clone();

                    let ws_at = ws.clone();
                    let sym_at = sym.clone();
                    let on_autotrade = move |e: web_sys::Event| {
                        let checked = e.target().unwrap().dyn_into::<web_sys::HtmlInputElement>().unwrap().checked();
                        configs.update(|map| {
                            if let Some(c) = map.get_mut(&sym_at) {
                                c.autotrade = checked;
                                send_config_update(&ws_at, c);
                            }
                        });
                    };

                    let ws_mps = ws.clone();
                    let sym_mps = sym.clone();
                    let on_max_pos = move |e: web_sys::Event| {
                        let val: u32 = event_target_value(&e).parse().unwrap_or(0);
                        configs.update(|map| {
                            if let Some(c) = map.get_mut(&sym_mps) {
                                c.max_pos_size = val;
                                send_config_update(&ws_mps, c);
                            }
                        });
                    };

                    let ws_mnps = ws.clone();
                    let sym_mnps = sym.clone();
                    let on_min_pos = move |e: web_sys::Event| {
                        let val: i32 = event_target_value(&e).parse().unwrap_or(0);
                        configs.update(|map| {
                            if let Some(c) = map.get_mut(&sym_mnps) {
                                c.min_pos_size = val;
                                send_config_update(&ws_mnps, c);
                            }
                        });
                    };

                    let ws_mos = ws.clone();
                    let sym_mos = sym.clone();
                    let on_max_order = move |e: web_sys::Event| {
                        let val: u32 = event_target_value(&e).parse().unwrap_or(0);
                        configs.update(|map| {
                            if let Some(c) = map.get_mut(&sym_mos) {
                                c.max_order_size = val;
                                send_config_update(&ws_mos, c);
                            }
                        });
                    };

                    let ws_mul = ws.clone();
                    let sym_mul = sym.clone();
                    let on_multiplier = move |e: web_sys::Event| {
                        let val: f64 = event_target_value(&e).parse().unwrap_or(1.0);
                        configs.update(|map| {
                            if let Some(c) = map.get_mut(&sym_mul) {
                                c.multiplier = val;
                                send_config_update(&ws_mul, c);
                            }
                        });
                    };

                    let ws_ls = ws.clone();
                    let sym_ls = sym.clone();
                    let on_lot_size = move |e: web_sys::Event| {
                        let val: u32 = event_target_value(&e).parse().unwrap_or(1);
                        configs.update(|map| {
                            if let Some(c) = map.get_mut(&sym_ls) {
                                c.lot_size = val;
                                send_config_update(&ws_ls, c);
                            }
                        });
                    };

                    view! {
                        <div style="background: #f9fafb; border: 1px solid #e5e7eb; border-radius: 12px; padding: 16px; margin-top: 12px;">
                            <div style="display: flex; align-items: center; gap: 12px; margin-bottom: 12px;">
                                <h4 style="margin: 0; font-size: 0.95em; color: #333;">"Config"</h4>
                                <label style="display: flex; align-items: center; gap: 6px; cursor: pointer; font-size: 0.9em;">
                                    <input
                                        type="checkbox"
                                        prop:checked=cfg.autotrade
                                        on:change=on_autotrade
                                        style="width: 16px; height: 16px; cursor: pointer;"
                                    />
                                    "Autotrade"
                                </label>
                            </div>
                            <div style="display: grid; grid-template-columns: repeat(5, 1fr); gap: 10px;">
                                <div>
                                    <label style=label_style>"Max Pos Size"</label>
                                    <input
                                        type="number"
                                        min="0"
                                        prop:value=cfg.max_pos_size.to_string()
                                        on:change=on_max_pos
                                        style=input_style
                                    />
                                </div>
                                <div>
                                    <label style=label_style>"Min Pos Size"</label>
                                    <input
                                        type="number"
                                        prop:value=cfg.min_pos_size.to_string()
                                        on:change=on_min_pos
                                        style=input_style
                                    />
                                </div>
                                <div>
                                    <label style=label_style>"Max Order Size"</label>
                                    <input
                                        type="number"
                                        min="0"
                                        prop:value=cfg.max_order_size.to_string()
                                        on:change=on_max_order
                                        style=input_style
                                    />
                                </div>
                                <div>
                                    <label style=label_style>"Multiplier"</label>
                                    <input
                                        type="number"
                                        min="0"
                                        step="0.01"
                                        prop:value=cfg.multiplier.to_string()
                                        on:change=on_multiplier
                                        style=input_style
                                    />
                                </div>
                                <div>
                                    <label style=label_style>"Lot Size"</label>
                                    <input
                                        type="number"
                                        min="1"
                                        prop:value=cfg.lot_size.to_string()
                                        on:change=on_lot_size
                                        style=input_style
                                    />
                                </div>
                            </div>
                        </div>
                    }
                })
            }
        }
        // Order entry panel
        {
            let ws = ws_ref.clone();
            let sym = symbol.clone();
            let order_action = RwSignal::new("BUY".to_string());
            let order_type = RwSignal::new("MKT".to_string());
            let order_qty = RwSignal::new("1".to_string());
            let order_limit = RwSignal::new(String::new());
            let order_stop = RwSignal::new(String::new());
            let order_tif = RwSignal::new("DAY".to_string());

            let ws_submit = ws.clone();
            let sym_submit = sym.clone();
            let on_submit_order = move |_| {
                let qty: f64 = order_qty.get().parse().unwrap_or(0.0);
                if qty <= 0.0 { return; }
                let ot = order_type.get();
                let lp = order_limit.get().parse::<f64>().ok();
                let sp = order_stop.get().parse::<f64>().ok();
                let act = order_action.get();

                let client_order_id = Uuid::new_v4();

                // Optimistic UI: insert a PendingAck row immediately so the user
                // sees feedback before the backend round-trip completes.
                let optimistic = OrderUpdate {
                    client_order_id,
                    order_id: 0,
                    symbol: sym_submit.clone(),
                    action: act.clone(),
                    order_type: ot.clone(),
                    quantity: qty,
                    limit_price: lp,
                    stop_price: sp,
                    status: "PendingAck".to_string(),
                    filled: 0.0,
                    remaining: qty,
                    average_fill_price: 0.0,
                };
                orders.update(|map| {
                    map.insert(client_order_id, optimistic);
                });

                let msg = PlaceOrderMsg {
                    r#type: "place_order".to_string(),
                    client_order_id,
                    symbol: sym_submit.clone(),
                    security_type: String::new(),
                    exchange: String::new(),
                    currency: String::new(),
                    primary_exchange: String::new(),
                    last_trade_date_or_contract_month: String::new(),
                    strike: 0.0,
                    right: String::new(),
                    contract_id: 0,
                    action: act,
                    order_type: ot,
                    quantity: qty,
                    limit_price: lp,
                    stop_price: sp,
                    time_in_force: order_tif.get(),
                };
                if let Ok(json) = serde_json::to_string(&msg) {
                    if let Some(ws) = ws_submit.borrow().as_ref() {
                        let _ = ws.send_with_str(&json);
                    }
                }
            };

            view! {
                <div style="background: #f0f4ff; border: 1px solid #c7d2fe; border-radius: 12px; padding: 16px; margin-top: 12px;">
                    <h4 style="margin: 0 0 12px 0; font-size: 0.95em; color: #333;">"Order Entry"</h4>
                    <div style="display: flex; gap: 10px; flex-wrap: wrap; align-items: end;">
                        <div>
                            <label style=label_style>"Action"</label>
                            <select
                                prop:value=move || order_action.get()
                                on:change=move |e| order_action.set(event_target_value(&e))
                                style="padding: 6px 10px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.9em; background: #fff;"
                            >
                                <option value="BUY">"BUY"</option>
                                <option value="SELL">"SELL"</option>
                            </select>
                        </div>
                        <div>
                            <label style=label_style>"Type"</label>
                            <select
                                prop:value=move || order_type.get()
                                on:change=move |e| order_type.set(event_target_value(&e))
                                style="padding: 6px 10px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.9em; background: #fff;"
                            >
                                <option value="MKT">"MKT"</option>
                                <option value="LMT">"LMT"</option>
                                <option value="STP">"STP"</option>
                            </select>
                        </div>
                        <div>
                            <label style=label_style>"Quantity"</label>
                            <input
                                type="number"
                                min="1"
                                step="1"
                                prop:value=move || order_qty.get()
                                on:input=move |e| order_qty.set(event_target_value(&e))
                                style="width: 80px; padding: 6px 10px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.9em; box-sizing: border-box;"
                            />
                        </div>
                        <div>
                            <label style=label_style>"Limit Price"</label>
                            <input
                                type="number"
                                step="0.01"
                                placeholder="—"
                                prop:value=move || order_limit.get()
                                on:input=move |e| order_limit.set(event_target_value(&e))
                                disabled=move || order_type.get() != "LMT"
                                style="width: 100px; padding: 6px 10px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.9em; box-sizing: border-box;"
                            />
                        </div>
                        <div>
                            <label style=label_style>"Stop Price"</label>
                            <input
                                type="number"
                                step="0.01"
                                placeholder="—"
                                prop:value=move || order_stop.get()
                                on:input=move |e| order_stop.set(event_target_value(&e))
                                disabled=move || order_type.get() != "STP"
                                style="width: 100px; padding: 6px 10px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.9em; box-sizing: border-box;"
                            />
                        </div>
                        <div>
                            <label style=label_style>"TIF"</label>
                            <select
                                prop:value=move || order_tif.get()
                                on:change=move |e| order_tif.set(event_target_value(&e))
                                style="padding: 6px 10px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.9em; background: #fff;"
                            >
                                <option value="DAY">"DAY"</option>
                                <option value="GTC">"GTC"</option>
                                <option value="IOC">"IOC"</option>
                            </select>
                        </div>
                        <button
                            on:click=on_submit_order
                            style="padding: 6px 16px; background: #2563eb; color: #fff; border: none; border-radius: 6px; font-size: 0.9em; cursor: pointer;"
                        >
                            "Submit Order"
                        </button>
                    </div>
                </div>
            }
        }
        // Active orders for this contract
        {
            let sym = sym_for_orders;
            let ws_orders = ws_ref;
            let editing_order_id: RwSignal<Option<Uuid>> = RwSignal::new(None);
            let edit_qty = RwSignal::new(String::new());
            let edit_price = RwSignal::new(String::new());

            move || {
                let all = orders.get();
                let mut contract_orders: Vec<_> = all.values()
                    .filter(|o| o.symbol == sym)
                    .cloned()
                    .collect();
                contract_orders.sort_by(|a, b| b.order_id.cmp(&a.order_id));

                if contract_orders.is_empty() {
                    view! { <div></div> }.into_any()
                } else {
                    let ws = ws_orders.clone();
                    view! {
                        <div style="background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 16px; margin-top: 12px;">
                            <h4 style="margin: 0 0 8px 0; font-size: 0.95em; color: #333;">"Orders"</h4>
                            <table style="width: 100%; font-size: 0.85em; border-collapse: collapse;">
                                <thead>
                                    <tr style="text-align: left; color: #888; border-bottom: 1px solid #e5e7eb;">
                                        <th style="padding: 4px 8px;">"ID"</th>
                                        <th style="padding: 4px 8px;">"Action"</th>
                                        <th style="padding: 4px 8px;">"Type"</th>
                                        <th style="padding: 4px 8px;">"Qty"</th>
                                        <th style="padding: 4px 8px;">"Price"</th>
                                        <th style="padding: 4px 8px;">"Status"</th>
                                        <th style="padding: 4px 8px;">"Filled"</th>
                                        <th style="padding: 4px 8px;">"Avg Fill"</th>
                                        <th style="padding: 4px 8px;">"Actions"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {contract_orders.into_iter().map(|o| {
                                        let status_color = match o.status.as_str() {
                                            "Filled" => "#16a34a",
                                            "Cancelled" | "Rejected" => "#dc2626",
                                            "CancelPending" | "AmendPending" => "#ea580c",
                                            "Unsent" | "PendingAck" => "#4f46e5",
                                            "Working" | "PartiallyFilled" => "#ca8a04",
                                            _ => "#ca8a04",
                                        };
                                        let status_style = format!("padding: 4px 8px; color: {status_color}; font-weight: 600;");
                                        let avg_fill = if o.average_fill_price > 0.0 { format!("{:.2}", o.average_fill_price) } else { "\u{2014}".to_string() };

                                        let can_cancel = matches!(o.status.as_str(), "Working" | "PartiallyFilled" | "PendingAck");
                                        let can_modify = matches!(o.status.as_str(), "Working" | "PartiallyFilled")
                                            && matches!(o.order_type.as_str(), "LMT" | "STP");

                                        let is_editing = editing_order_id.get() == Some(o.client_order_id);

                                        // Qty cell: show input when editing, text otherwise
                                        let qty_display = if is_editing {
                                            view! {
                                                <td style="padding: 4px 8px;">
                                                    <input
                                                        type="number"
                                                        min="1"
                                                        step="1"
                                                        prop:value=move || edit_qty.get()
                                                        on:input=move |e| edit_qty.set(event_target_value(&e))
                                                        style="width: 60px; padding: 2px 4px; border: 1px solid #93c5fd; border-radius: 4px; font-size: 0.95em;"
                                                    />
                                                </td>
                                            }.into_any()
                                        } else {
                                            view! {
                                                <td style="padding: 4px 8px;">{format!("{:.0}", o.quantity)}</td>
                                            }.into_any()
                                        };

                                        // Price cell: show input when editing, text otherwise
                                        let price_val = match o.order_type.as_str() {
                                            "LMT" => o.limit_price.map(|p| format!("{p:.2}")).unwrap_or_default(),
                                            "STP" => o.stop_price.map(|p| format!("{p:.2}")).unwrap_or_default(),
                                            _ => "MKT".to_string(),
                                        };
                                        let price_display = if is_editing {
                                            view! {
                                                <td style="padding: 4px 8px;">
                                                    <input
                                                        type="number"
                                                        step="0.01"
                                                        prop:value=move || edit_price.get()
                                                        on:input=move |e| edit_price.set(event_target_value(&e))
                                                        style="width: 80px; padding: 2px 4px; border: 1px solid #93c5fd; border-radius: 4px; font-size: 0.95em;"
                                                    />
                                                </td>
                                            }.into_any()
                                        } else {
                                            view! {
                                                <td style="padding: 4px 8px;">{price_val}</td>
                                            }.into_any()
                                        };

                                        // Actions cell
                                        let coid = o.client_order_id;
                                        let o_type = o.order_type.clone();
                                        let o_qty = o.quantity;
                                        let o_lp = o.limit_price;
                                        let o_sp = o.stop_price;
                                        let ws_cancel = ws.clone();
                                        let ws_save = ws.clone();
                                        let actions = if is_editing {
                                            // Editing mode: Save + X buttons
                                            let o_type_save = o_type.clone();
                                            let on_save = move |_| {
                                                let qty: f64 = edit_qty.get().parse().unwrap_or(o_qty);
                                                let price: Option<f64> = edit_price.get().parse().ok();
                                                let (lp, sp) = match o_type_save.as_str() {
                                                    "LMT" => (price, None),
                                                    "STP" => (None, price),
                                                    _ => (None, None),
                                                };
                                                let msg = ModifyOrderMsg {
                                                    r#type: "modify_order".to_string(),
                                                    client_order_id: coid,
                                                    quantity: qty,
                                                    limit_price: lp,
                                                    stop_price: sp,
                                                };
                                                if let Ok(json) = serde_json::to_string(&msg) {
                                                    if let Some(ws) = ws_save.borrow().as_ref() {
                                                        let _ = ws.send_with_str(&json);
                                                    }
                                                }
                                                editing_order_id.set(None);
                                            };
                                            let on_discard = move |_| {
                                                editing_order_id.set(None);
                                            };
                                            view! {
                                                <td style="padding: 4px 8px; white-space: nowrap;">
                                                    <button
                                                        on:click=on_save
                                                        style="padding: 2px 8px; background: #16a34a; color: #fff; border: none; border-radius: 4px; cursor: pointer; font-size: 0.85em; margin-right: 4px;"
                                                    >"Save"</button>
                                                    <button
                                                        on:click=on_discard
                                                        style="padding: 2px 8px; background: #6b7280; color: #fff; border: none; border-radius: 4px; cursor: pointer; font-size: 0.85em;"
                                                    >"X"</button>
                                                </td>
                                            }.into_any()
                                        } else {
                                            // Normal mode: Cancel + Modify buttons
                                            let on_cancel = move |_| {
                                                let msg = CancelOrderMsg {
                                                    r#type: "cancel_order".to_string(),
                                                    client_order_id: coid,
                                                };
                                                if let Ok(json) = serde_json::to_string(&msg) {
                                                    if let Some(ws) = ws_cancel.borrow().as_ref() {
                                                        let _ = ws.send_with_str(&json);
                                                    }
                                                }
                                            };
                                            let on_modify = move |_| {
                                                edit_qty.set(format!("{:.0}", o_qty));
                                                let price_str = match o_type.as_str() {
                                                    "LMT" => o_lp.map(|p| format!("{p:.2}")).unwrap_or_default(),
                                                    "STP" => o_sp.map(|p| format!("{p:.2}")).unwrap_or_default(),
                                                    _ => String::new(),
                                                };
                                                edit_price.set(price_str);
                                                editing_order_id.set(Some(coid));
                                            };
                                            view! {
                                                <td style="padding: 4px 8px; white-space: nowrap;">
                                                    {if can_cancel {
                                                        Some(view! {
                                                            <button
                                                                on:click=on_cancel
                                                                style="padding: 2px 8px; background: none; color: #dc2626; border: 1px solid #dc2626; border-radius: 4px; cursor: pointer; font-size: 0.85em; margin-right: 4px;"
                                                            >"Cancel"</button>
                                                        })
                                                    } else {
                                                        None
                                                    }}
                                                    {if can_modify {
                                                        Some(view! {
                                                            <button
                                                                on:click=on_modify
                                                                style="padding: 2px 8px; background: none; color: #2563eb; border: 1px solid #2563eb; border-radius: 4px; cursor: pointer; font-size: 0.85em;"
                                                            >"Modify"</button>
                                                        })
                                                    } else {
                                                        None
                                                    }}
                                                </td>
                                            }.into_any()
                                        };

                                        view! {
                                            <tr style="border-bottom: 1px solid #f3f4f6;">
                                                <td style="padding: 4px 8px;">{
                                                    if o.order_id > 0 {
                                                        o.order_id.to_string()
                                                    } else {
                                                        o.client_order_id.to_string()[..8].to_string()
                                                    }
                                                }</td>
                                                <td style="padding: 4px 8px; font-weight: 600;">{o.action.clone()}</td>
                                                <td style="padding: 4px 8px;">{o.order_type.clone()}</td>
                                                {qty_display}
                                                {price_display}
                                                <td style=status_style>{o.status.clone()}</td>
                                                <td style="padding: 4px 8px;">{format!("{:.0}", o.filled)}</td>
                                                <td style="padding: 4px 8px;">{avg_fill}</td>
                                                {actions}
                                            </tr>
                                        }
                                    }).collect::<Vec<_>>()}
                                </tbody>
                            </table>
                        </div>
                    }.into_any()
                }
            }
        }
    }
}

#[component]
fn App() -> impl IntoView {
    let prices: RwSignal<HashMap<String, PriceUpdate>> = RwSignal::new(HashMap::new());
    let bar_history: RwSignal<HashMap<String, Vec<BarData>>> = RwSignal::new(HashMap::new());
    let positions: RwSignal<HashMap<String, PositionData>> = RwSignal::new(HashMap::new());
    let configs: RwSignal<HashMap<String, ContractConfig>> = RwSignal::new(HashMap::new());
    let orders: RwSignal<HashMap<Uuid, OrderUpdate>> = RwSignal::new(HashMap::new());
    let connected = RwSignal::new(false);

    // Form input signals with defaults
    let symbol_input = RwSignal::new(String::new());
    let sec_type_input = RwSignal::new("STK".to_string());
    let exchange_input = RwSignal::new("SMART".to_string());
    let currency_input = RwSignal::new("USD".to_string());
    let primary_exchange_input = RwSignal::new(String::new());
    let contract_month_input = RwSignal::new(String::new());
    let strike_input = RwSignal::new(String::new());
    let right_input = RwSignal::new(String::new());
    let contract_id_input = RwSignal::new(String::new());

    // Store WebSocket reference for sending messages.
    // SendWrapper is safe because WASM is single-threaded; it satisfies the Send bound
    // that Leptos reactive closures require.
    let ws_ref: SendWrapper<Rc<RefCell<Option<WebSocket>>>> = SendWrapper::new(Rc::new(RefCell::new(None)));

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
                    Ok(ServerMessage::ContractConfig(cfg)) => {
                        configs.update(|map| {
                            map.insert(cfg.symbol.clone(), cfg);
                        });
                    }
                    Ok(ServerMessage::OrderUpdate(update)) => {
                        orders.update(|map| {
                            map.insert(update.client_order_id, update);
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
            primary_exchange: primary_exchange_input.get().trim().to_uppercase(),
            last_trade_date_or_contract_month: contract_month_input.get().trim().to_string(),
            strike: strike_input.get().trim().parse().unwrap_or(0.0),
            right: right_input.get().trim().to_uppercase(),
            contract_id: contract_id_input.get().trim().parse().unwrap_or(0),
        };
        if let Ok(json) = serde_json::to_string(&req) {
            if let Some(ws) = ws_ref_submit.borrow().as_ref() {
                let _ = ws.send_with_str(&json);
            }
        }
        symbol_input.set(String::new());
        primary_exchange_input.set(String::new());
        contract_month_input.set(String::new());
        strike_input.set(String::new());
        right_input.set(String::new());
        contract_id_input.set(String::new());
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
                            list="exchanges"
                            prop:value=move || exchange_input.get()
                            on:input=move |e| exchange_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; box-sizing: border-box;"
                        />
                    </div>
                    <div style="min-width: 80px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Currency"</label>
                        <input
                            type="text"
                            list="currencies"
                            prop:value=move || currency_input.get()
                            on:input=move |e| currency_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; box-sizing: border-box;"
                        />
                    </div>
                </div>
                <div style="display: flex; gap: 10px; flex-wrap: wrap; align-items: end; margin-top: 10px;">
                    <div style="flex: 1; min-width: 90px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Primary Exch"</label>
                        <input
                            type="text"
                            list="exchanges"
                            placeholder="e.g. ASX"
                            prop:value=move || primary_exchange_input.get()
                            on:input=move |e| primary_exchange_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; box-sizing: border-box;"
                        />
                    </div>
                    <div style="flex: 1; min-width: 110px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Contract Month"</label>
                        <input
                            type="text"
                            placeholder="YYYYMM"
                            prop:value=move || contract_month_input.get()
                            on:input=move |e| contract_month_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; box-sizing: border-box;"
                        />
                    </div>
                    <div style="min-width: 80px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Strike"</label>
                        <input
                            type="number"
                            step="0.01"
                            placeholder="0.00"
                            prop:value=move || strike_input.get()
                            on:input=move |e| strike_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; box-sizing: border-box;"
                        />
                    </div>
                    <div style="min-width: 70px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Right"</label>
                        <select
                            prop:value=move || right_input.get()
                            on:change=move |e| right_input.set(event_target_value(&e))
                            style="width: 100%; padding: 8px 12px; border: 1px solid #d1d5db; border-radius: 6px; font-size: 0.95em; background: #fff; box-sizing: border-box;"
                        >
                            <option value="">"—"</option>
                            <option value="C">"Call"</option>
                            <option value="P">"Put"</option>
                        </select>
                    </div>
                    <div style="min-width: 80px;">
                        <label style="display: block; font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Con ID"</label>
                        <input
                            type="number"
                            placeholder="optional"
                            prop:value=move || contract_id_input.get()
                            on:input=move |e| contract_id_input.set(event_target_value(&e))
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

            <datalist id="exchanges">
                <option value="SMART" />
                <option value="ASX" />
                <option value="NYSE" />
                <option value="NASDAQ" />
                <option value="ARCA" />
                <option value="AMEX" />
                <option value="BATS" />
                <option value="LSE" />
                <option value="HKEX" />
                <option value="TSE" />
                <option value="OSE" />
                <option value="SGX" />
                <option value="CME" />
                <option value="CBOE" />
                <option value="NYMEX" />
                <option value="COMEX" />
                <option value="GLOBEX" />
                <option value="ECBOT" />
                <option value="IDEALPRO" />
                <option value="IBKRATS" />
            </datalist>
            <datalist id="currencies">
                <option value="USD" />
                <option value="AUD" />
                <option value="EUR" />
                <option value="GBP" />
                <option value="JPY" />
                <option value="HKD" />
                <option value="CAD" />
                <option value="CHF" />
                <option value="SGD" />
                <option value="NZD" />
                <option value="SEK" />
                <option value="NOK" />
                <option value="DKK" />
                <option value="KRW" />
                <option value="INR" />
                <option value="MXN" />
                <option value="ZAR" />
            </datalist>

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
                    let ws_ref_rows = ws_ref.clone();
                    view! {
                        <div style="display: flex; flex-direction: column; gap: 16px;">
                            {symbols
                                .into_iter()
                                .map(|symbol| {
                                    let ws = ws_ref_rows.clone();
                                    view! {
                                        <ContractRow
                                            symbol=symbol
                                            prices=prices
                                            bar_history=bar_history
                                            positions=positions
                                            configs=configs
                                            orders=orders
                                            ws_ref=ws
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
