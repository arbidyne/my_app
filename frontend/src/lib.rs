use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
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

#[derive(Clone, Debug, Serialize)]
struct SubscribeRequest {
    r#type: String,
    symbol: String,
    security_type: String,
    exchange: String,
    currency: String,
}

#[component]
fn PriceCard(price: PriceUpdate) -> impl IntoView {
    let ts_secs = (price.timestamp / 1000) as f64;
    let date = js_sys::Date::new(&JsValue::from_f64(ts_secs * 1000.0));
    let time_str = format!(
        "{:02}:{:02}:{:02}",
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds(),
    );

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
        </div>
    }
}

#[component]
fn App() -> impl IntoView {
    let prices: RwSignal<HashMap<String, PriceUpdate>> = RwSignal::new(HashMap::new());
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
                match serde_json::from_str::<PriceUpdate>(&text) {
                    Ok(update) => {
                        prices.update(|map| {
                            map.insert(update.symbol.clone(), update);
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

    // Sorted price list derived from the prices map
    let sorted_prices = Memo::new(move |_| {
        let map = prices.get();
        let mut entries: Vec<PriceUpdate> = map.into_values().collect();
        entries.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        entries
    });

    view! {
        <div style="font-family: 'Segoe UI', system-ui, sans-serif; max-width: 900px; margin: 40px auto; padding: 24px;">
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

            // Price cards grid
            {move || {
                let entries = sorted_prices.get();
                if entries.is_empty() {
                    view! {
                        <div style="text-align: center; padding: 60px 0; color: #999;">
                            "No subscriptions yet. Add a contract above."
                        </div>
                    }
                        .into_any()
                } else {
                    view! {
                        <div style="display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 16px;">
                            {entries
                                .into_iter()
                                .map(|p| view! { <PriceCard price=p /> })
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
