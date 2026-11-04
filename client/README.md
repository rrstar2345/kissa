# KISSA

A desktop app for stock and options traders, built with **Tauri v2** (Rust) and **SvelteKit + Tailwind CSS v4 + DaisyUI**.

KISSA streams option prices, lets you place orders through your own [`broker-proxy`](../broker-proxy) (a static-IP Go server), and keeps every credential, order and account detail **on your machine**.

## Status

| Area | State |
|---|---|
| Accounts (paper, Dhan, broker-proxy, Fyers data) | ✅ Phase 1 |
| Option chain (NSE live stream / Fyers polling, IV + Greeks, configurable columns) | ✅ Phase 1 |
| Charts (lightweight-charts, Rust-computed indicators) | Phase 2 |
| Orders (broker-proxy REST + WSS, paper fills, local order log) | Phase 3 |
| Dashboard (live P&L, trader's diary) | Phase 3 |

## Data sources

- **NSE (default)** – public NSE endpoints and WebSocket streams, no credentials. Ported from `kstocks_egui`: browser-style cookie warm-up, NSE-synced IST market clock (Active/Idle session mode), F&O symbol/expiry resolution. NSE streams ticks only during market hours (≈ 9:00–15:30 IST, Mon–Fri).
- **Fyers (optional)** – your own API app ID + access token (stored in the OS keychain). The chain is polled (default every 5 s) from the Fyers v3 [Option Chain API](https://fyers.in/api-docs/v3/api/market-data/option-chain/) with `greeks=1`, so it also works after hours. Fyers access tokens expire daily; paste a fresh one under **Accounts** when needed.

Choose the source on the Option Chain page or under **Accounts → Market data source**.

### Greeks

Fyers supplies IV/Greeks. For NSE they are computed in Rust from LTP: Black-76 priced off a forward derived from put-call parity at ATM (no dividend assumption), r = 7% (`risk_free_rate`), time measured to 15:30 IST on expiry. Theta is per calendar day, vega per 1 vol point. NSE's stream may not include OI change, so *OI Chg* is measured from the first OI seen in the session unless NSE provides it.

## Security model

- **Secrets** (Dhan access token, proxy API key, Fyers token) are stored **only in the OS keychain** (macOS Keychain, Windows Credential Manager, Secret Service on Linux). They are never written to SQLite or `settings.json`, and the UI never receives them back.
- **SQLite** (`kissa.db`) holds non-secret account fields, preferences (columns, theme, symbol) and, in later phases, the order log and diary.
- **Proxy URL must be `https://`** (plain `http://` is accepted for `localhost` only). The proxy keeps credentials in memory per session and persists nothing; the client is the system of record.
- Data lives in `<local data dir>/.kissa/` (`kissa.db`, `settings.json`).

## Broker-proxy

Configure under **Accounts → Broker-proxy** (URL, API key from `proxy keygen`, whitelisted static IP for reference). **Test** checks `/healthz`, then opens and closes a `paper` session to validate the API key. A **Dhan** account links to a proxy and **Test** opens and closes a real Dhan session through it. See the [broker-proxy README](../broker-proxy/README.md) for the API contract.

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

`settings.json` is created on first run and may be edited while the app is closed: NSE endpoints, reconnect/idle-poll timings, `risk_free_rate`, and Fyers (`chain_url`, `poll_secs`, `strike_count`).

## Layout

```
src/                     SvelteKit SPA (Svelte 5 runes)
  app.css                Tailwind + DaisyUI themes and all custom styling
  lib/columns.ts         option-chain column definitions
  lib/components/        ChainTable, ColumnPicker, AccountForm
  routes/                dashboard, chain, charts, orders, accounts
src-tauri/src/
  market/                NSE http client, clock, symbols, WSS streamers (from kstocks_egui)
  chain/                 chain model, engine, Black-76 greeks, Fyers client, runner
  accounts.rs secrets.rs db.rs settings.rs commands.rs
```
