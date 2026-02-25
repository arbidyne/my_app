# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Real-time stock price streaming app for Interactive Brokers (IBKR). Rust workspace with two crates:

- **backend** — Axum WebSocket server that connects to IB Gateway, subscribes to tick-by-tick bid/ask data (currently BHP on ASX), and broadcasts `PriceUpdate` JSON to all connected WebSocket clients at `ws://127.0.0.1:3000/ws`.
- **frontend** — Leptos CSR (client-side rendering) app compiled to WASM via Trunk. Runs entirely in the browser.

## Build & Run Commands

```bash
# Build entire workspace
cargo build

# Build/check a single crate
cargo build -p backend
cargo check -p frontend

# Run the backend server (requires IB Gateway running on 127.0.0.1:7496)
cargo run -p backend

# Build frontend WASM with Trunk (from frontend/ directory)
trunk build            # production build → frontend/dist/
trunk serve            # dev server with hot-reload at http://127.0.0.1:8080

# Run tests
cargo test                    # all workspace tests
cargo test -p backend         # single crate
cargo test test_name          # single test by name
```

## Architecture

- Backend uses `tokio::sync::broadcast` channel: one IBKR producer task sends `PriceUpdate` structs, each WebSocket connection gets its own receiver.
- IBKR connection uses the `ibapi` crate (async feature) connecting to IB Gateway on port 7496 with client ID 42.
- Frontend uses Trunk as the WASM build tool. `frontend/index.html` has `<link data-trunk rel="rust" />` which Trunk uses to compile the crate and inject the JS/WASM loader. Built output goes to `frontend/dist/` (and is also copied to top-level `dist/`).
- Rust edition 2024 is used across both crates.

## Key Dependencies

- **Backend**: axum (with ws feature), ibapi (async), tokio, serde/serde_json, tracing
- **Frontend**: leptos 0.8 (csr), wasm-bindgen, web-sys, console_error_panic_hook

## Prerequisites

- [Trunk](https://trunkrs.dev/) must be installed for frontend builds (`cargo install trunk`)
- IB Gateway or TWS must be running on `127.0.0.1:7496` for the backend to connect
- `wasm32-unknown-unknown` target must be installed (`rustup target add wasm32-unknown-unknown`)

# Documentation Standards
- follow [API Guidelines](https://rust-lang.github.io).
- In general, explain the "why" not the "what".
- Do not comment trivial getter/setter methods.
- Reference other modules if the function depends on them.