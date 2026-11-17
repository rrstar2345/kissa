# KISSA

A desktop app for stock and options traders, built with **Tauri v2** (Rust) and **SvelteKit + Tailwind CSS v4 + DaisyUI**.

KISSA streams option prices, draws charts with signals, runs algos and backtests, and places orders through your own [`broker-proxy`](../broker-proxy) (a static-IP Go server), while keeping every credential, order and account detail **on your machine**. It is hardened with paper trading first; live brokers (Dhan, Fyers) are implemented but not yet validated against the real services.

Project handoff notes (architecture, gotchas, unverified items, known gaps) are in [`CLAUDE.md`](../CLAUDE.md).

## Features

### Market data
- **NSE (default)**: public NSE endpoints and WebSocket streams, no credentials. Streams ticks only during market hours (about 9:00-15:30 IST, Mon-Fri), so the chain is empty outside them.
- **Fyers (optional)**: your own API app ID + secret (the secret goes to the OS keychain). **Log in** on the account opens the Fyers login and the app creates the access token (the redirect is caught on `http://127.0.0.1:<port>/`, or paste the code or URL). Live prices come over one shared Fyers data socket, with REST seeding and fallback, so the chain also works after hours. Tokens expire daily at 6:30 AM; the header shows a red "Log in" badge when needed. Every Fyers call is rate limited to your plan (Standard or Prime) with a usage card on the Settings page and an "API error / Rate limited" indicator.
- Choose the source on the Option Chain page or under **Accounts > Market data source**. Only the selected source is polled. The header shows its market status and a live-feed indicator.
- **Option chain**: configurable columns, live ticks, OI change, IV and Greeks (from Fyers; for NSE computed in Rust with Black-76, forward from put-call parity, r = `risk_free_rate`, theta per calendar day, vega per vol point). Click a bid, ask or LTP to open the order ticket.

### Charts, signals, backtests
- Live ticks fold into 1-minute bars (indices, plus option strikes around ATM on the chain being streamed and every held contract), stored in `bars.duckdb`. Only 09:15-15:30 IST is recorded; finished days roll up to daily bars, and old 1m bars and expired options are purged on schedule (`charts.*` settings). 3m-60m intervals are resampled on read, aligned to 09:15 IST.
- Indicators: SMA, EMA, RSI, MACD, Bollinger, VWAP, Supertrend, with automatic signals (MA cross, MACD cross, RSI 30/70, Bollinger re-entry, Supertrend flip). Fyers backfill fills gaps per chart without overwriting stored bars. Layout is saved per window.
- **Signals** page: build rule sets (variables plus entry / exit condition trees) and attach them to chart panels. Rules use closed candles only.
- **Backtest**: run one saved signal on one instrument over stored bars (fills at the next bar's open, long only).

### Orders and trading
- **Order ticket**: LIMIT and STOP_LOSS only (the proxy rejects market orders), DAY / IOC, quantity in lots, price in 0.05 ticks, live LTP with a **Use** button. Accounts are **Paper** (executed locally) or **Dhan** (via your broker-proxy).
- **Paper broker**: fills priced from live ticks (stop-limit for STOP_LOSS), optional slippage and latency, bracket / OCO orders, rough margin check (`startingCapital`, `short_margin_pct`), auto square-off of INTRADAY positions at 15:20 IST, expiry settlement. Orders are rejected outside market hours unless `trading.paper_allow_off_hours` is set.
- **Dhan**: one proxy session per account, live updates over the proxy WebSocket with replay, de-duplication by `correlationId`, a 60 s order-book resync, and instrument ids from Dhan's public CSV (**Refresh instruments** in the ticket if a contract is missing).
- **Order gate**: no order or modify is accepted unless the server under **Settings > Server connection** accepts your API key (checked on every order; cancel is not gated). For development run the proxy locally (`http://localhost:<port>` is allowed) with a key from `proxy keygen`.
- **Exit / Exit all**: cancels the contract's open orders, then places a LIMIT at the LTP pushed `trading.exit_buffer_ticks` ticks against you. Both ask for confirmation.
- **Orders page**: orders (cancel / modify), trades and positions by account and date. The Trades tab toggles **Round trips** (default; entry and exit as one trade, listed on the day it closes) and **Fills**. Positions are derived from trades (average cost); charges are an estimate per fill (brokerage, STT, exchange, SEBI, stamp duty, GST).
- **Risk controls**: per-account limits and daily-loss halt, plus a kill switch. Orders that only reduce a position always pass, so exits are never blocked.
- **Trade copier**: one master account, any number of child accounts (use them only for copying).
- **Algos**: option algos driven by one saved signal; each CE / PE leg is judged on its own option bars; one capital pool per account; paper brackets or watched SL / target; BUY only; Dhan needs "Allow live accounts". Algos run only while the app is running (desktop or headless).

### Dashboard and records
- **Dashboard**: Daily / Monthly / Yearly views with cards (net and gross P&L, trades, charges, ROI), a calendar heat map, a trader's diary and open positions. Realised P&L and charges belong to the IST hour / day / month of the fill; a trade is a round trip counted when it closes; ROI = net / entry premium. The top bar shows today's net P&L and a positions dock appears on the chain and chart pages.
- **CSV export** (Orders page and Dashboard): Orders, Trades or Trader's diary for an IST date range and account, saved through your system's dialog (text that could start a spreadsheet formula gets a leading `'`).
- **Journal**: an audit trail of orders, risk, algos, copier, accounts, settings and system events (who did it: user, system, algo, copier, broker or Telegram), kept 90 days.
- **Backup**: **Settings > Database backup** writes a timestamped folder with `kissa.db` and `bars.duckdb` while the app runs. Keychain secrets are not included. To restore, copy the files back with the app closed.

### Alerts and Telegram
- **Alerts page**: Telegram and email notifications for fills, rejections, risk halts, connection loss, token expiry, copier and algo events, data API problems and a daily P&L summary, each switchable, with de-duplication, retries and a history.
- **Telegram bot**: "Detect chat ID" finds your chat. With **Answer commands** on, allow-listed chats can send `/positions`, `/pnl`, `/algos`, `/status`, `/control` and `/help` (read-only).
- **Control buttons** (**Allow control buttons**, off by default): arm / disarm an algo, kill switch on / off, lift a loss halt, exit one position, exit all, under `/positions`, `/algos`, `/control` and on risk and algo alerts. They work only in a private chat from your list. Everything except disarm needs a second press that works once and expires after 30 seconds; actions go through the normal order checks and risk limits and are written to the Journal as "telegram". Anyone with access to that Telegram account can trade, so keep it locked.

### Headless mode
`kissa --headless` (or `KISSA_HEADLESS=1`) runs the same backend with no window, for a server or VM: algos, recorder, alerts and the Telegram bot keep working. Options: `--control-port N` (default 8787), `--no-control`, `--show-control-token`. A loopback HTTP control API (bearer token) offers status, arm / disarm, kill / resume, the daily Fyers re-login and shutdown (see `src-tauri/src/headless/control.rs`). Logs go to `kissa-headless.log`; only one instance can use a data folder. On Linux the window toolkit still needs GTK libraries and a display (Xvfb). Servers without a keychain use the encrypted file `secrets.enc` (`KISSA_SECRETS=file`, passphrase in `KISSA_PASSPHRASE`): prepare it on the desktop, then copy the whole data folder to the server.

### Windows
Any page can open in its own window (up to 8 pop-outs); they share one backend and are restored on start.

## Security model
- **Secrets** (Dhan token, proxy and server API keys, Fyers token and app secret, Telegram bot token, SMTP password) live **only in the OS keychain** (or the encrypted `secrets.enc` file in headless mode). They are never written to SQLite or `settings.json`, and the UI never receives them back.
- **SQLite** (`kissa.db`) holds non-secret account fields, preferences, orders, trades, risk, algos, alerts and the journal. **DuckDB** (`bars.duckdb`) holds chart bars.
- The **proxy URL must be `https://`** (plain `http://` only for localhost). The proxy keeps credentials in memory per session and stores nothing; the client is the system of record.
- The window loads nothing remote and makes no network calls itself (a strict CSP); all network access happens in Rust.
- Data lives in `<local data dir>/.kissa/`.

## Broker-proxy
The order gate uses the server under **Settings > Server connection**. Dhan accounts link to a proxy under **Accounts > Broker-proxy** (URL, API key from `proxy keygen`, whitelisted static IP for reference); both can point at the same server. **Test** checks `/healthz` and opens and closes a session to validate the key. See the [broker-proxy README](../broker-proxy/README.md) for the API contract.

## Development
Prerequisites: Rust **1.94+** (SQLx 0.9), a C++ toolchain (DuckDB is built from source on the first build, which is slow), Node 20+, pnpm, and the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS. On Linux also install `libdbus-1-dev` (keychain) and OpenSSL headers.

```bash
pnpm install
pnpm tauri dev      # run the app
pnpm check          # svelte-check
cd src-tauri && cargo test --lib
pnpm tauri build    # release bundle
```

### Configuration
The **Settings** page edits `settings.json` and the **Server connection** (URL, port, API key in the keychain; https required except localhost). These apply immediately: `trading.*` (except `request_timeout_secs`), `risk_free_rate` and `server`. Other sections (`system`, `market_runtime`, `fyers`, `charts`, `trading.request_timeout_secs`) are read at startup, and the page lists saved changes that still need a restart. Edits made to the file outside the app apply at the next start.

`settings.json` is created on first run. Main sections: NSE endpoints and timings; `risk_free_rate`; `fyers` (plan `standard` / `prime`, optional `limits`, `use_socket`, `socket_url`, `max_symbols`, `chain_refresh_secs`, URLs); `charts` (storage and retention); `trading` (`paper_allow_off_hours`, `exit_buffer_ticks`, `paper.*` such as `enforce_margin`, `short_margin_pct`, `slippage_ticks`, `latency_ms`, `auto_square_off`, `square_off_time`, Dhan brokerage, `fallback_lot_sizes`, and `charges` rates; verify the rates against current ones). Lot sizes and strike steps built into the app are estimates; check them.

## Layout
```
src/                     SvelteKit SPA (Svelte 5 runes)
  app.css                Tailwind + DaisyUI themes and all custom styling
  lib/                   api, types, columns, stores (trading, quotes, market), components/
  routes/                accounts, alerts, signals, algos, backtest, chain, charts, copier, journal, orders, risk, settings, signals
src-tauri/src/
  alerts/                alert service, rendering, Telegram bot (bot.rs) and control buttons (control.rs)
  algo/                  algo runner
  audit/                 journal
  backtest/              backtesting
  chain/                 chain model, engine, Black-76 greeks, Fyers client, runner
  charts/                recorder, DuckDB bar store, resampling, indicators, backfill, rules/ (signals engine)
  headless/              headless mode and control API
  market/                NSE client, clock, status, streamers, Fyers REST wrapper (fyers_api) and data socket (fyers_socket)
  trading/               order flow, ledger, risk, copier, period (dashboard), export (CSV)
    broker/              Broker trait + registry; paper/ and dhan/
    service/             orders, exit, square-off, risk, alerts hooks, views
  accounts.rs backup.rs commands.rs db.rs emit_stats.rs lib.rs main.rs popout.rs secrets.rs settings.rs state.rs stats.rs
```