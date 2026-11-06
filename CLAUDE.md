# KISSA project handoff

## Working preferences (from the user)
- Keep answers concise and to the point.
- Do NOT create extra documents (summaries, quick references, etc.) unless asked; confirm first. README.md updates for the client are expected.
- Ask clarifying questions with options plus a recommendation before big work.
- When finished: zip all edited/created files and present the zip.

## Repos
- `https://github.com/rrstar2345/kissa.git` has `client/` (Tauri v2 app, the thing being built) and `broker-proxy/` (Go order proxy, already written; do not modify unless asked).
- `https://github.com/rrstar2345/kstocks_egui` is a reference-only older egui app. Source for NSE streaming, storage, OHLC aggregation, backfill.
- Fyers option chain docs: https://fyers.in/api-docs/v3/api/market-data/option-chain/ (web_fetch). For phase 2 also fetch the Historical Data page.
- Phase 1 was delivered as a zip of changed files under `client/`. **Before starting, confirm the user has unzipped it over the repo and pushed it.** If the clone has no `client/src/lib` or `client/src-tauri/src/chain`, ask the user for the zip.

## Product
Stock/options trading desktop app (Tauri v2: Rust backend + SvelteKit/Svelte 5 + Tailwind v4 + DaisyUI 5). Shows option prices from NSE public API (default) or Fyers (user's own creds). Places orders via the user's `broker-proxy` (Go, static-IP VM) which fronts Dhan. **All credentials, order log and account details stay client-side.**

Components: (1) Accounts, (2) Option chain, (3) Charts (lightweight-charts, multiple per page, Rust-computed indicators/signals), (4) Orders, (5) Dashboard (live P&L, trader's diary with orders/charges/P&L).

Decisions made: phased build; secrets in OS keychain (`keyring`), everything else in SQLite (`sqlx`); proxy contract = the broker-proxy README.

## Status
| Phase | Scope | State |
|---|---|---|
| 1 | Foundation, Accounts, Option chain | Done (compiles, 5 unit tests pass, frontend builds; **never run live**) |
| 2 | Charts + OHLC storage + indicators + Fyers backfill | Written, **not compiled or run** (no Rust in sandbox); see Phase 2 notes |
| 3 | Orders + Dashboard/diary | Backend and frontend written; compiles and runs (user-verified). **Never run against live Dhan/proxy**; see Phase 3 notes |

## Phase 1 architecture (client/)
Frontend `src/`: `app.css` (all theme/styles; themes `kissa-dark`/`kissa-light`; up=`success`, down=`error`), `lib/api.ts` (typed invoke wrappers + `getJson/setJson` pref helpers), `lib/types.ts`, `lib/columns.ts` (chain column defs), `lib/market.svelte.ts` (status + index ticks store), `lib/theme.svelte.ts`, `lib/components/{ChainTable,ColumnPicker,AccountForm,ComingSoon}.svelte`, routes `/`, `/chain`, `/charts`, `/orders`, `/accounts` (dashboard/charts/orders are `ComingSoon` placeholders).

Backend `src-tauri/src/`:
- `lib.rs`: setup (folders, config, DB, state), market-clock supervisor, always-on index streamer, command registration.
- `state.rs`: `AppState {config, db, stats, session, indices (std RwLock map), fno, chain}`; managed as `Arc<AppState>`.
- `market/`: `http.rs` (NSE cookie-warmed reqwest + WSS connect), `market_clock.rs` (NSE-synced IST, Active/Idle `SessionState`), `symbols.rs` (F&O indices + expiries), `events.rs` (`IndexTick`, `OptionTick`, `LegTick`), `streamers/{indices,options}.rs`. Streamers were decoupled from storage; they emit via a callback (`IndexSink`) or `UnboundedSender<OptionTick>`.
- `chain/`: `engine.rs` (aggregates ticks, derives OI change and Greeks, emits snapshots every 300 ms), `greeks.rs` (Black-76 IV/Greeks, tested), `fyers.rs` (REST poller/parser), `runner.rs` (spawns/aborts tasks; events `chain-snapshot`, `chain-status`), `mod.rs` (models).
- `accounts.rs` (CRUD + connection tests), `secrets.rs` (keychain; service `kissa`, key `<accountId>:<field>`), `db.rs` (tables `app_settings` kv, `accounts`), `settings.rs` (`~/.local/share/.kissa/{kissa.db,settings.json}` equivalents via `dirs`), `stats.rs`, `commands.rs`.
- Tauri events to the UI: `index-tick`, `chain-snapshot`, `chain-status`. Commands: `market_status, indices_snapshot, list_fno_symbols, list_expiries, start_chain, stop_chain, get_setting, set_setting, list_accounts, save_account, delete_account, test_account`.
- Settings keys in use: `data.source` (nse|fyers), `data.fyers_account`, `chain.columns`, `chain.window`, `chain.symbol`, `ui.theme`.
- Accounts: kinds `paper` (meta startingCapital, brokeragePerOrder), `dhan` (clientId, proxyId; secret accessToken), `proxy` (baseUrl https-only except localhost, staticIp, notes; secret apiKey), `fyers` (appId; secret accessToken). The frontend never receives secrets, only `secretsSet`.

## Key external facts
**Fyers option chain**: `GET` with header `Authorization: <appId>:<accessToken>`, params `symbol` (e.g. `NSE:NIFTY50-INDEX`), `strikecount` (max 50), `timestamp` (expiry epoch), `greeks=1`. Response `data.{callOi,putOi,expiryData[{date DD-MM-YYYY,expiry epoch}],indiavixData,optionsChain[]}`; first chain entry is the underlying (`strike_price` -1, empty `option_type`); legs have `ltp,ltpch,ltpchp,oi,oich,volume,bid,ask,greeks{delta,gamma,theta,vega,iv}`. **The endpoint URL (`https://api-t1.fyers.in/data/options-chain-v3`) is from memory, not the docs; verify.** Tokens expire daily.

**broker-proxy** (read `broker-proxy/README.md` for full detail): REST + one WSS per client. Header `X-API-Key`; after `POST /v1/sessions` (`{broker:"dhan",credentials:{clientId,accessToken}}` or `{broker:"paper"}` returns `sessionId`) also `X-Session-Id`. Endpoints: `POST/PUT/DELETE /v1/orders[/{id}]`, `POST /v1/orders/slicing`, `GET /v1/orders`, `/v1/orders/{id}`, `/v1/orders/external/{correlationId}`, `GET /v1/trades[/{orderId}]`, `GET /v1/ws?since=N` (events `order_update`, `feed_status`, `session`; per-session `seq` with replay buffer and `gap` notice), `GET /healthz`. Rules: **no MARKET/STOP_LOSS_MARKET** (only LIMIT and STOP_LOSS with positive `price`), strict JSON (unknown fields rejected), idempotency via `correlationId` (`409 outcome_unknown` means check `/orders/external/{id}` first), bodies mirror Dhan v2 Orders, errors `{error:{code,message,detail}}`. Server persists nothing, so **the client is the system of record**. Reuse one session per Dhan account (two sessions = two upstream feeds). A proxy restart drops sessions: reopen and resync from the order book.

**kstocks_egui reusable for phase 2**: `storage/ticks.rs` (batched tick writers), `storage/ohlc.rs` (1m aggregation + daily rollup loops), `storage/retention.rs`, `storage/backfill.rs` (gap-fill from an old server API; probably not needed), `storage/db.rs` (index_ticks/option_ticks/ohlc schemas), `storage/watchlists.rs`, `storage/paper_trades.rs`, `gui/paper.rs`. NSE option WS message fields: `strikePrice, expiryDates, CE/PE{lastPrice, change, openInterest, totalTradedVolume, buyPrice1, sellPrice1}`, `flag:"HEARTBEAT"` off-hours.

## Phase 2 notes (as built)
Decisions: persist indices + option strikes within ±20 of ATM of the streamed chain; Fyers backfill included; full indicator set. Deviation from the plan: no raw tick tables; ticks are folded into 1m bars in memory (`charts/recorder.rs`) and upserted to one `bars(instrument, tf, ts)` table (`charts/store.rs`). Instrument keys: `IDX:NIFTY 50`, `OPT:NIFTY:2026-11-24:24000:CE`. Other modules: `charts/{resample,indicators,backfill,instrument}.rs`; commands `get_chart, list_chart_instruments, backfill_chart`; event `bar-update`; frontend `routes/charts`, `lib/components/{ChartPanel,InstrumentPicker}.svelte`, `lib/charts.ts`; settings key `charts.layout`; config section `charts` + `fyers.history_url` in settings.json. Unverified: Fyers history URL/`oi_flag`, Fyers option symbol formats (weekly vs monthly guess), daily-candle timestamps (normalised to IST day), lightweight-charts pane APIs at runtime. Also fixed: options WSS 403 (now uses the cookie-warmed `connect_nse_ws`).

## Phase 2 plan (original proposal)
1. Port tick persistence + 1m/daily OHLC aggregation + retention into the client (SQLite via sqlx). Hook it to the index sink and to the option engine (currently nothing is persisted).
2. Tauri commands to fetch bars (index and option strike CE/PE, with interval resampling); optional Fyers historical backfill.
3. Indicators in Rust (EMA/SMA, RSI, MACD, Bollinger, VWAP, Supertrend) returning series plus buy/sell signal markers; keep the module pluggable.
4. Frontend: `lightweight-charts` panels, multiple per page, layouts saved in a `layouts` table (or settings JSON), live updates from tick events.

## Phase 3 notes (as built)
Decisions: Dhan `securityId` from Dhan's instrument CSV (cached in table `scrip`, refreshed daily); global `LtpStore` fed by chain engines plus a background feed (`trading/feed.rs`) for symbols with open positions or pending paper orders; full charge estimate with rates in `settings.json` under `trading.charges`; order entry by clicking bid/ask/LTP on the chain (ticket drawer).
Backend `src-tauri/src/trading/`: `mod.rs` (types, camelCase serde), `ltp.rs`, `charges.rs`, `scrip.rs`, `store.rs` (tables `orders`, `trades`; positions folded from trades by average cost; a stale `orders`/`trades` table without `trade_date` is renamed `*_legacy_<ts>` at startup), `paper.rs`, `dhan.rs` (proxy REST + WSS with `since` replay), `service.rs` (`TradingService`), `feed.rs`.
Commands: `place_order, cancel_order, modify_order, list_orders, list_trades, get_dashboard, get_diary, contract_info, refresh_instruments, connect_broker, sync_orders`. Events: `order-update`, `broker-status`.
Frontend: `lib/trading.svelte.ts`, `lib/components/OrderTicket.svelte`, ChainTable `onpick`, `routes/orders` (Orders/Trades/Positions), `routes/+page.svelte` (dashboard + diary).
SQLx 0.9 note: `format!`-built SQL must be wrapped in `sqlx::AssertSqlSafe(...)` (only constants are interpolated).
Chain/index fixes made after first run: ATM now comes only from the real index price (no put-call-parity guess); the snapshot is held until the index price arrives, with a 30 s fallback that shows rows without ATM; the indices streamer's idle wait is interruptible and the supervisor sets the session mode before the NSE time request (previously the index price could take up to ~2 min after start). The NSE homepage warm-up often returns 403; cookies are optional for the streamers (Origin/Referer/User-Agent are what they need), so this is only log noise.

### Phase 3 behaviour gaps
- Positions are keyed by account + instrument + product; no INTRADAY to MARGIN conversion, no auto square-off at day end, no expiry settlement (expired contracts just drop out).
- Paper has no margin or capital check; `startingCapital` is informational.
- Dhan partial fills are booked by cumulative-quantity deltas (unverified against a real feed); cancel of a partly filled order is allowed, modify is blocked.
- An order left `UNKNOWN` after retries is resolved only by the 60 s order-book resync.
- `save_account` does not explicitly drop a cached Dhan connection; credential changes are detected on next use (verify).
- `last_close` position fallback reads `bars`; confirm the instrument key matches what the recorder stores.
- Order ticket shows no live price; price field holds the clicked value.

## Phase 3 plan (original proposal)
Rust `broker` client trait with `ProxyDhan` (REST + WSS with `since` replay, correlationId generation, no market orders) and a local `Paper` executor (fills priced from live ticks, brokerage from account meta). Tables: orders/order log, trades, positions, charges. Orders page (LIMIT/SL only, tied to option-chain strike clicks). Dashboard: live P&L from open positions plus LTP, trader's diary with charges and realised P&L.

## Dev environment notes (sandbox and user)
- User needs Rust 1.94+ (sqlx 0.9; same as kstocks). Sandbox has only apt Rust: `apt-get update`, then `rustc-1.91 cargo-1.91`, `export PATH=/usr/lib/rust-1.91/bin:$PATH`, and compile with `--ignore-rust-version`. Tauri Linux libs needed: `libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev librsvg2-dev libayatana-appindicator3-dev libssl-dev libdbus-1-dev libsecret-1-dev pkg-config build-essential`. `pnpm` via `npm i -g pnpm`.
- First `cargo check` takes about 6 min and `cargo test` about 10; launch with `setsid nohup cargo ... > /tmp/x.log 2>&1 < /dev/null &` and poll (plain `nohup &` dies when the tool call returns).
- Sandbox network cannot reach nseindia.com or fyers; live streaming cannot be tested there. Unit-test engines and parsers with fixtures instead.
- Verify with: `pnpm check`, `pnpm build`, `cd src-tauri && cargo test --lib`.
- Zip only edited/created files (`git status --porcelain --untracked-files=all`, excluding `node_modules`), then present the zip.

## Known gaps and unverified assumptions
- Never run against live NSE/Fyers/proxy/keychain; expect first-run fixes.
- Fyers endpoint URL and the F&O symbol to stream index name mapping (e.g. NIFTY to "NIFTY 50") are unverified.
- NSE OI change is derived from the first OI seen this session unless NSE sends `changeinOpenInterest`.
- NSE chain is empty outside market hours (idle poll every 120 s); Fyers works after hours.
- Idle/active streamer logic is ported as-is; chain stream runs only while the Option Chain page is open (index streamer is always on).
- `tauri.conf.json` `csp` is null and default capabilities are in use; tighten before release.
- Phase 3 never run against the real proxy, Dhan, NSE order flow or Fyers. Dhan instrument CSV column names/date format and proxy field names come from docs and code reading only; compare against the Go validators in `broker-proxy/` if a request is rejected.
- Background feed assumes NSE expiry label format `DD-Mon-YYYY`.
- Charges rates (e.g. STT 0.15% on option sell premium) and lot sizes (NIFTY 65, BANKNIFTY 30, FINNIFTY 60, MIDCPNIFTY 120, NIFTYNXT50 25) are estimates as of Nov 2026; verify.
- Unused template assets remain in `client/static/` (vite/svelte/tauri svgs).