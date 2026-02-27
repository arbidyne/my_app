# Roadmap: NautilusTrader-inspired features

## Phase 1 — Order Execution
- ~~**Order submission** — Market, Limit, Stop orders via IBKR API~~ **DONE**
- ~~**Order lifecycle tracking** — open/filled/cancelled/rejected states, displayed in UI~~ **DONE**
- ~~**Order lifecycle state machine** — validated state transitions, race condition handling (fill during cancel/amend), IBKR event mapping, terminal state enforcement~~ **DONE**
- ~~**Order modification/cancellation** — modify price/quantity of working orders~~ **DONE**
- ~~**UUID tracking for each order** — Every order request should be given a UUID by frontend.  All messages sent back to the frontend relating to that order should have that UUID in them.  This includes Order Insert Reply, Trade feeds, Amend replies etc.  This means a frontend can send an order to backend, and then send a cancel or modify order BEFORE the backend has replied with the IB order id.~~ **DONE**

## Phase 2 — Risk Management
- ~~**Pre-trade risk checks** — enforce max_pos_size, max_order_size, min_pos_size before submitting.  All these checks should be done at the backend only.  Any failures should put the order into Rejected status and must not be submitted to IB.  Autotrade gate rejects orders when autotrade=false.  Zero limits mean "zero tolerance" (fail-closed).~~ **DONE**
- **Trading states** — Active / Halted / Reducing-only mode (global kill switch)
- **Max notional limits** — cap dollar exposure per contract
- **P&L tracking** — realized and unrealized P&L per position, displayed on price cards
- **Daily loss limit** — auto-halt trading if drawdown exceeds threshold
- **Order Rate Limit Check** - build a rate limiter.  If more than 5 orders are sent in 2 seconds, the global kill switch should trigger and stop trading. The rates (number of orders and time window) will be hardcoded for now but later we will add them to global configuration.

## Phase 3 — Autotrader Framework
- **Strategy trait/interface** — define a standard interface strategies implement (on_tick, on_bar, on_fill)
- **Signal generation** — strategies emit buy/sell signals, routed through risk engine before execution
- **TWAP execution algorithm** — split large orders across time intervals
- **Per-contract strategy assignment** — bind a strategy to a contract via the config panel
- **Backtesting mode** — replay historical bars through the same strategy code (backtest-live parity)

## Phase 4 — Enhanced Market Data
- **Order book depth** — L2 data display (IBKR supports market depth)
- **Multiple bar aggregations** — 1min, 5min, 15min, 1h, daily selectable from UI
- **Additional price types** — mid, last, bid, ask bar series
- **Custom indicators** — SMA, EMA, RSI computed on bar data, overlaid on charts
- **Multi-day historical data** — configurable lookback period (currently 1 day)

## Phase 5 — Portfolio & Multi-Account
- **Portfolio view** — aggregate positions, P&L, exposure across all contracts
- **Multi-currency support** — track and display positions in native currencies with FX conversion
- **Account summary** — net liquidation, buying power, margin usage from IBKR
- **Multi-account support** — manage positions across multiple IBKR accounts

## Phase 6 — Data Persistence & Replay
- **Tick/bar recording** — persist streaming data to Parquet or SQLite
- **Trade journal** — log all orders, fills, and P&L with timestamps
- **Data catalog** — browse and replay historical sessions
- **Strategy performance reports** — win rate, Sharpe ratio, max drawdown, etc.

## Phase 7 — Advanced Order Types
- **Trailing stops** — trailing stop market and trailing stop limit
- **Bracket orders** — entry + take-profit + stop-loss as a group
- **OCO orders** — one-cancels-other
- **OTO orders** — one-triggers-other
- **Iceberg orders** — display only a fraction of total quantity
- **Time-in-force options** — GTC, GTD, IOC, FOK, DAY
