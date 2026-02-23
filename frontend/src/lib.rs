use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

#[derive(Clone, Debug, Deserialize)]
struct PriceUpdate {
    symbol: String,
    last_price: f64,
    timestamp: u64,
    best_bid_price: f64,
    best_bid_size: f64,
    best_ask_price: f64,
    best_ask_size: f64,
}

#[component]
fn App() -> impl IntoView {
    let price = RwSignal::new(None::<PriceUpdate>);
    let connected = RwSignal::new(false);

    // Open WebSocket inside an effect so it runs once on mount.
    Effect::new(move |_| {
        let ws = WebSocket::new("ws://127.0.0.1:3000/ws").expect("failed to create WebSocket");

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
                    Ok(update) => price.set(Some(update)),
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

        // Keep the WebSocket alive — dropping it would release the JS reference
        // before the connection is established.
        std::mem::forget(ws);
    });

    view! {
        <div style="font-family: 'Segoe UI', system-ui, sans-serif; max-width: 480px; margin: 40px auto; padding: 24px;">
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

            // Price card
            {move || {
                if let Some(p) = price.get() {
                    let ts_secs = (p.timestamp / 1000) as f64;
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
                                <h1 style="margin: 0; font-size: 1.6em;">{p.symbol.clone()}</h1>
                                <span style="color: #888; font-size: 0.85em;">{time_str}</span>
                            </div>

                            <div style="font-size: 2em; font-weight: 700; margin-bottom: 20px;">
                                {format!("{:.2}", p.last_price)}
                            </div>

                            <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 12px;">
                                <div style="background: #f0fdf4; border-radius: 8px; padding: 12px;">
                                    <div style="font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Bid"</div>
                                    <div style="font-size: 1.25em; font-weight: 600; color: #16a34a;">
                                        {format!("{:.2}", p.best_bid_price)}
                                    </div>
                                    <div style="font-size: 0.85em; color: #666;">
                                        {format!("Size: {:.0}", p.best_bid_size)}
                                    </div>
                                </div>
                                <div style="background: #fef2f2; border-radius: 8px; padding: 12px;">
                                    <div style="font-size: 0.75em; color: #888; text-transform: uppercase; margin-bottom: 4px;">"Ask"</div>
                                    <div style="font-size: 1.25em; font-weight: 600; color: #dc2626;">
                                        {format!("{:.2}", p.best_ask_price)}
                                    </div>
                                    <div style="font-size: 0.85em; color: #666;">
                                        {format!("Size: {:.0}", p.best_ask_size)}
                                    </div>
                                </div>
                            </div>
                        </div>
                    }
                        .into_any()
                } else {
                    view! {
                        <div style="text-align: center; padding: 60px 0; color: #999;">
                            "Waiting for data..."
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
