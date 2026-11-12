# KISSA

A desktop app for stock and options traders, built with **Tauri v2** (Rust) and **SvelteKit + Tailwind CSS v4 + DaisyUI**.

KISSA streams option prices, lets you place orders through your own [`broker-proxy`](../broker-proxy) (a static-IP Go server), and keeps every credential, order and account detail **on your machine**.

Project status, the roadmap and design notes are kept in [`CLAUDE.md`](../CLAUDE.md).

## Data sources

- **NSE (default)** – public NSE endpoints and WebSocket streams, no credentials. Ported from `kstocks_egui`: browser-style cookie warm-up, NSE-synced IST market clock (Active/Idle session mode), F&O symbol/expiry resolution. NSE streams ticks only during market hours (≈ 9:00–15:30 IST, Mon–Fri).
- **Fyers (optional)** – your own API app ID + secret ID (create the app in the Fyers API dashboard, with a redirect URL; the secret is stored in the OS keychain). Use **Log in** on the account: it opens the Fyers login and the app generates the access token for you (the redirect is caught automatically on a `http://127.0.0.1:<port>/` redirect URL, or you can paste back the code or URL you land on). The chain is polled (default every 5 s) from the Fyers v3 [Option Chain API](https://fyers.in/api-docs/v3/api/market-data/option-chain/) with `greeks=1`, so it also works after hours. Fyers access tokens expire daily (6:30 AM); click **Log in** again under **Accounts** each trading day.

Choose the source on the Option Chain page or under **Accounts → Market data source**.

### Charts

- **Storage**: live ticks are folded in memory into 1-minute bars (index ticks, plus option strikes within ±`charts.persist_strikes_each_side` of ATM for the chain being streamed) and flushed to the `bars` table every few seconds. Only 09:15–15:30 IST is recorded. Finished days roll up to daily bars; 1m bars are kept `charts.bars_1m_keep_days` days, expired options are purged after `charts.option_expiry_grace_days`. Other intervals (3m–60m) are resampled on read, aligned to 09:15 IST.
- **Indicators** (computed in Rust): SMA, EMA, RSI, MACD, Bollinger, VWAP, Supertrend. Signals: fast/slow MA cross (when two of the same kind are enabled), MACD cross, RSI leaving 30/70, Bollinger re-entry, Supertrend flip. Index bars have no volume, so VWAP there is the running typical-price mean.
- **Backfill** (Fyers, per chart): fills 1m or daily gaps and never overwrites stored bars. Uses `fyers.history_url` from `settings.json` (unverified against the live API). Expired option contracts normally have no history.
- Chart layout (panels, intervals, indicators) is saved in the `charts.layout` setting.

### Orders and dashboard

- **Placing orders**: click a bid, ask or LTP on the option chain to open the order ticket (ask/LTP prefill BUY, bid prefills SELL). LIMIT and STOP_LOSS only (the proxy rejects market orders), DAY/IOC validity, quantity in lots, price in ₹0.05 ticks. Accounts: **Paper** (executed locally) or **Dhan** (via your broker-proxy).
- **Broker interface**: paper and Dhan sit behind one `Broker` trait (`trading/broker/`), so every order takes the same path (validation, server gate, broker, ledger). Adding a broker means adding a folder under `trading/broker/` and registering it in `TradingService::new`.
- **Paper broker**: fills are priced from live ticks (LIMIT; STOP_LOSS is stop-limit), with optional slippage (`trading.paper.slippage_ticks`) and acknowledgement latency (`latency_ms`). Orders are rejected outside 09:15–15:30 IST Mon–Fri unless `trading.paper_allow_off_hours` is true. Brokerage comes from the account's `brokeragePerOrder`.
- **Paper margin check**: needs `startingCapital` on the paper account (otherwise no limit). Cash = starting capital + realised P&L − charges. A long blocks its premium, a short blocks `short_margin_pct` % of strike × quantity, and pending orders reserve what they would block (the part that closes a position needs nothing). An order is rejected if it exceeds what is available. This is a rough estimate, not SPAN.
- **Paper auto square-off**: at `paper.square_off_time` (15:20 IST) open INTRADAY orders are cancelled and INTRADAY positions closed at the last price (with slippage); new INTRADAY orders are refused from then on. It also runs at start-up for positions left open by an earlier session. Disabled by `paper.auto_square_off: false` or `paper_allow_off_hours: true`. Live brokers do their own square-off.
- **Dhan**: one proxy session per account; live updates over the proxy WebSocket with `since` replay, orders de-duplicated by `correlationId`, and a periodic order-book resync. The contract's Dhan `securityId` comes from Dhan's public instrument CSV, cached in SQLite and refreshed daily (**Refresh instruments** in the ticket if a contract is missing).
- **Ledger**: `orders` and `trades` tables in `kissa.db`. Positions are derived from trades (average cost) and are not stored. Charges are an estimate per fill: flat brokerage per order, STT, exchange fee, SEBI, stamp duty and GST.
- **Prices**: a shared store fed by the chain engines, plus an automatic background stream for any symbol/expiry that has open positions or pending paper orders.
- **Orders page**: orders (cancel/modify), trades and positions, filtered by account and date; Dhan connection state and **Sync orders**.
- **Dashboard**: a `Daily | Monthly | Yearly` toggle (default Monthly), a period picker and an accounts filter drive everything below.
  - **Cards**: Net P&L, Gross P&L, Trades, Total charges and ROI for the period (Daily = the day, Monthly = the month, Yearly = the 12 months ending at the selected month).
  - **Calendar heat view**: columns of the last 4 days (one cell per hour), the last 4 months (calendar, one cell per day) or the last 12 months (calendar, coloured cells only), each with a net P&L pill. Green = net profit, red = net loss, yellow = net zero, grey = no activity.
  - **Trader's diary**: grouped by hour, day or month to match the view; expand a group to see its trades (entry and exit of each).
  - **Open positions** with live/last/none LTP badge (follows the accounts filter).
  - **How it is counted**: realised P&L and charges belong to the IST hour/day/month of the fill that booked them; open positions (unrealised) are not included. A **trade** is entry + exit (a round trip, counted when it closes). **ROI** = net P&L / entry premium (price x qty of the fills that opened or added to a position).
- **Exit / Exit all**: open positions have an **Exit** button, and Orders and Dashboard have **Exit all** (all accounts, or the selected one). The backend first cancels open orders on the contract (so a resting stop cannot reopen it), then places a LIMIT for the full quantity on the opposite side at the LTP pushed `trading.exit_buffer_ticks` (default 2) ticks against you, same product, DAY. Both ask for confirmation; Exit all lists any position it could not exit. Exit is disabled when there is no LTP.
- **Order ticket**: shows the contract's live LTP (updated every second, live/last badge) with a **Use** button to copy it into the price.
- **Server authentication (order gate)**: no order (paper or Dhan), and no modify, is accepted unless the server in `server` (see Configuration) accepts the API key. The key is checked on every order using a cached `paper` session on the proxy (`GET /v1/orders`), so a revoked or wrong key is caught immediately. Cancel is not gated. The proxy must therefore be reachable even for paper trading; for development run it locally (`http://localhost:<port>` is allowed) with a key from `proxy keygen`.
- If an `orders` or `trades` table with an older layout exists, it is renamed to `<table>_legacy_<timestamp>` at startup rather than deleted.

### Greeks

Fyers supplies IV/Greeks. For NSE they are computed in Rust from LTP: Black-76 priced off a forward derived from put-call parity at ATM (no dividend assumption), r = 7% (`risk_free_rate`), time measured to 15:30 IST on expiry. Theta is per calendar day, vega per 1 vol point. NSE's stream may not include OI change, so *OI Chg* is measured from the first OI seen in the session unless NSE provides it.

## Security model

- **Secrets** (Dhan access token, proxy API key, the server API key from Settings, Fyers token) are stored **only in the OS keychain** (macOS Keychain, Windows Credential Manager, Secret Service on Linux). They are never written to SQLite or `settings.json`, and the UI never receives them back.
- **SQLite** (`kissa.db`) holds non-secret account fields, preferences (columns, theme, symbol), the order log, trades and bars.
- **Proxy URL must be `https://`** (plain `http://` is accepted for `localhost` only). The proxy keeps credentials in memory per session and persists nothing; the client is the system of record.
- Data lives in `<local data dir>/.kissa/` (`kissa.db`, `settings.json`).

## Broker-proxy

The order gate uses the server set under **Settings → Server connection**. Dhan accounts link to a proxy configured under **Accounts → Broker-proxy** (URL, API key from `proxy keygen`, whitelisted static IP for reference). Both can point at the same server. **Test** checks `/healthz`, then opens and closes a `paper` session to validate the API key. A **Dhan** account links to a proxy and **Test** opens and closes a real Dhan session through it. See the [broker-proxy README](../broker-proxy/README.md) for the API contract.

## Development

Prerequisites: Rust (the SQLx 0.9 dependency needs **1.94+**), Node 20+, pnpm, and the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS. On Linux also install `libdbus-1-dev` (keychain) and OpenSSL headers.

```bash
pnpm install
pnpm tauri dev      # run the app
pnpm check          # svelte-check
cd src-tauri && cargo test --lib
pnpm tauri build    # release bundle
```

### Configuration

**Settings page**: edit `settings.json` in the app, and set the **Server connection** (URL, port, API key). URL and port are stored in `settings.json` under `server` (`url`, `port`; https required except localhost; the port is appended unless the URL has one). The API key is stored in the OS keychain, not in the file. Server changes apply immediately; other settings are read at startup, so restart the app after changing them.

`settings.json` is created on first run and may also be edited while the app is closed: NSE endpoints, reconnect/idle-poll timings, `risk_free_rate`, Fyers (`chain_url`, `poll_secs`, `strike_count`) and `trading`: `paper_allow_off_hours`, `exit_buffer_ticks` (2), `paper` (`enforce_margin`, `short_margin_pct` 10, `slippage_ticks` 0, `latency_ms` 0, `auto_square_off`, `square_off_time` "15:20"), `dhan_brokerage_per_order` (20), `dhan_brokerage_cap_pct`, `scrip_master_url`, `scrip_refresh_hours`, `request_timeout_secs`, `fallback_lot_sizes` (used only when Dhan's list has no entry) and `charges` (STT, exchange, SEBI, stamp duty, GST rates; verify against current rates).

## Layout

```
src/                     SvelteKit SPA (Svelte 5 runes)
  app.css                Tailwind + DaisyUI themes and all custom styling
  lib/columns.ts         option-chain column definitions
  lib/components/        ChainTable, ColumnPicker, AccountForm, OrderTicket, ChartPanel, ExitButton, ExitAllButton
  lib/trading.svelte.ts  order-update / broker-status listeners, status helpers
  routes/                dashboard, chain, charts, orders, accounts, settings
src-tauri/src/
  market/                NSE http client, clock, symbols, WSS streamers (from kstocks_egui)
  chain/                 chain model, engine, Black-76 greeks, Fyers client, runner
  charts/                bar recorder, store, indicators, backfill
  trading/               broker-independent order flow and the ledger
    broker/              Broker trait + registry; paper/ (matching, funds, book) and dhan/ (adapter + proxy client)
    service/             orders (place/modify/cancel), exit, squareoff, views (dashboard/quotes)
    store.rs pricing.rs charges.rs ltp.rs feed.rs scrip.rs server.rs  (SQLite ledger, tick maths, costs, prices, instruments, order gate)
    period.rs            dashboard aggregation: round-trip trades, cards, calendar heat view, grouped diary
  accounts.rs secrets.rs db.rs settings.rs commands.rs
```