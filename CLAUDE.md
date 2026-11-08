# KISSA project handoff

## Working preferences (from the user)
- Keep answers concise and to the point.
- Do NOT create extra documents (summaries, quick references, etc.) unless asked; confirm first. Updating `CLAUDE.md` and the client `README.md` is expected when the user asks.
- Ask clarifying questions with options plus a recommendation before big work.
- **Do not install Rust/Cargo in the sandbox, and do not compile.** The user builds and tests manually. Verify the frontend only (`pnpm check`, `pnpm build`), and re-read Rust carefully since it cannot be compiled here.
- When finished: present the edited/created files; zip them if there are more than 5 files.

## Repos
- `https://github.com/rrstar2345/kissa.git` has `client/` (Tauri v2 app, the thing being built) and `broker-proxy/` (Go order proxy, already written; do not modify unless asked).
- `https://github.com/rrstar2345/kstocks_egui` is a reference-only older egui app (NSE streaming, storage, OHLC aggregation).
- Fyers option chain docs: https://fyers.in/api-docs/v3/api/market-data/option-chain/ (web_fetch). For history/backfill also fetch the Historical Data page.

## Current focus
The user has **not yet set up** the Oracle Cloud VPS, static IP, port rules, Dhan account/IP whitelisting/app credentials, or a Fyers account/API subscription. Nothing involving a real broker or Fyers can be validated yet. **Strategy: harden the core product with paper trading first**, so switching to a real broker later is a minor change (see Roadmap: keep one broker interface; paper must behave like a real broker).

For development without a VPS: run `broker-proxy` locally (`http://localhost:<port>` is allowed), generate a key with `proxy keygen`, and enter it under Settings > Server connection. Paper orders require this (see Order gate).

## Product
Stock/options trading desktop app (Tauri v2: Rust backend + SvelteKit/Svelte 5 + Tailwind v4 + DaisyUI 5). Shows option prices from NSE public API (default) or Fyers (user's own creds). Places orders via the user's `broker-proxy` (Go, static-IP VM) which fronts Dhan. **All credentials, order log and account details stay client-side.**

Components: (1) Accounts, (2) Option chain, (3) Charts (lightweight-charts, multiple per page, Rust-computed indicators/signals), (4) Orders, (5) Dashboard (live P&L, trader's diary with orders/charges/P&L).

Decisions made: phased build; secrets in OS keychain (`keyring`), everything else in SQLite (`sqlx`); proxy contract = the broker-proxy README.

## Status
| Area | State |
|---|---|
| Phase 1: foundation, accounts, option chain | Done, user-verified |
| Phase 2: charts, OHLC storage, indicators, Fyers backfill | Written and compiling; Fyers backfill never run live |
| Phase 3: orders, paper executor, Dhan via proxy, dashboard/diary | Compiles and runs for paper (user-verified). **Dhan/proxy order flow never run live** |
| Enhancements: Exit button, Settings page, server connection, order gate | Done, user-tested (Nov 2026) |
| Roadmap item 0: Broker trait refactor + paper fidelity | Written; builds clean (user-verified build, Nov 2026); paper behaviour not yet exercised |

## Enhancements delivered after Phase 3
- **Exit button** (`lib/components/ExitButton.svelte`, used on Orders > Positions and Dashboard): confirm dialog, then `place_order` with the opposite side, full quantity, LIMIT at LTP rounded to 0.05, same product, DAY. Disabled without LTP or when qty is not a whole number of lots.
- **Settings page** (`routes/settings`): Server connection form (URL, port, API key; Save, Test connection) and an in-app `settings.json` editor (Format, Revert, Save). Commands: `get_settings_json, save_settings_json, get_server_settings, save_server_settings, test_server`. Saving writes atomically (tmp file + rename), validates by parsing into `AppConfig`, and drops keys not in the struct. **Only the `server` section applies live**; `AppState.config` and `TradingService.cfg` are immutable, so everything else needs an app restart.
- **Server config**: `settings.json` `server {url, port}` (`ServerConfig` in `settings.rs`; `base_url()` enforces https except localhost and appends `port` unless the URL has a port or path). The API key is in the keychain (account id `server`, field `apiKey`), not in the file. This is separate from `proxy` accounts, which Dhan accounts still use.
- **Order gate** (`trading/server.rs`, `ServerAuth`): `TradingService::place` and `modify` call `require_server_auth()` before touching the DB. It keeps one cached `paper` session on the proxy and re-validates it with `GET /v1/orders` on every call (401 = key rejected; 404 = session lost, reopen). So **paper orders are only accepted for a client the proxy authenticates**. Cancel is not gated. Settings changes call `set_server`, which drops the cached session (the old one idles out on the server).

## Item 0 addendum (written, uncompiled)
- **Expiry settlement** (`service/settle.rs`): paper only; positions in an expired contract (today's after 15:30 IST) are closed by a synthetic order `role=SETTLE` at the contract's own last 1m close on expiry day (fallback 1d bar, then last recorded close), dated the expiry day, zero brokerage. Runs at start-up and every 12th sweep.
- **Bracket/OCO** (`service/bracket.rs`): `PlaceOrderInput.bracket {target, stopTrigger, stopPrice}` (paper only). Entry has `role=ENTRY` and legs JSON in `orders.bracket`; on fill, TARGET (LIMIT) and STOP (STOP_LOSS) legs share `group_id` = entry id; a TRADED leg cancels its sibling. New order columns `group_id, role, bracket` (added via `ensure_column`).
- **Real-time UI**: pushed `quotes` event (100 ms batches from `LtpStore::drain_changed`), `lib/quotes.svelte.ts` + `lib/live.ts` overlay live prices on dashboard/orders/ticket/exit; chain snapshots 150 ms and only when dirty; bar-update UI tick 250 ms; market status poll 1 s. Old 3-4 s polls remain only as 30 s fallbacks.

## Phase 1 architecture (client/)
Frontend `src/`: `app.css` (all theme/styles; themes `kissa-dark`/`kissa-light`; up=`success`, down=`error`), `lib/api.ts` (typed invoke wrappers + `getJson/setJson` pref helpers), `lib/types.ts`, `lib/columns.ts` (chain column defs), `lib/market.svelte.ts` (status + index ticks store), `lib/theme.svelte.ts`, `lib/components/{ChainTable,ColumnPicker,AccountForm,ComingSoon}.svelte`, routes `/`, `/chain`, `/charts`, `/orders`, `/accounts` (dashboard/charts/orders are `ComingSoon` placeholders).

Backend `src-tauri/src/`:
- `lib.rs`: setup (folders, config, DB, state), market-clock supervisor, always-on index streamer, command registration.
- `state.rs`: `AppState {config, db, stats, session, indices (std RwLock map), fno, chain, recorder, ltp, trading}`; managed as `Arc<AppState>`.
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

## Phase 3 notes (as built)
Decisions: Dhan `securityId` from Dhan's instrument CSV (cached in table `scrip`, refreshed daily); global `LtpStore` fed by chain engines plus a background feed (`trading/feed.rs`) for symbols with open positions or pending paper orders; full charge estimate with rates in `settings.json` under `trading.charges`; order entry by clicking bid/ask/LTP on the chain (ticket drawer).
Backend `src-tauri/src/trading/` (layout since item 0): `mod.rs` (types, camelCase serde), `ltp.rs`, `charges.rs`, `scrip.rs`, `pricing.rs` (tick maths, `exit_price`, `slipped`), `store.rs` (tables `orders`, `trades`; positions folded from trades by average cost, `OpenPos.opened_on`, `account_pnl`; a stale `orders`/`trades` table without `trade_date` is renamed `*_legacy_<ts>` at startup), `feed.rs`, `server.rs`, `broker/` and `service/` (see Item 0 notes).
Commands: `place_order, cancel_order, modify_order, exit_position, exit_all, get_quote, list_orders, list_trades, get_dashboard, get_diary, contract_info, refresh_instruments, connect_broker, sync_orders`. Events: `order-update`, `broker-status`.
Frontend: `lib/trading.svelte.ts`, `lib/components/OrderTicket.svelte`, ChainTable `onpick`, `routes/orders` (Orders/Trades/Positions), `routes/+page.svelte` (dashboard + diary).
SQLx 0.9 note: `format!`-built SQL must be wrapped in `sqlx::AssertSqlSafe(...)` (only constants are interpolated).
Chain/index fixes made after first run: ATM now comes only from the real index price (no put-call-parity guess); the snapshot is held until the index price arrives, with a 30 s fallback that shows rows without ATM; the indices streamer's idle wait is interruptible and the supervisor sets the session mode before the NSE time request (previously the index price could take up to ~2 min after start). The NSE homepage warm-up often returns 403; cookies are optional for the streamers (Origin/Referer/User-Agent are what they need), so this is only log noise.

## Item 0 notes (as built: broker interface and paper fidelity)
Decisions: refactor + core fidelity now; margin model = simple estimate; **not built**: expiry settlement, bracket/OCO orders (next pass).
**Broker trait** (`trading/broker/mod.rs`, uses the `async-trait` crate): `kind, needs_instrument_ids, pre_trade, place, modify, cancel, brokerage, connect, sync, forget, watched_contracts`. Brokers answer with a `Reply` and push later changes (fills, trigger, expiry, Dhan WSS/order-book) as `BrokerEvent::{Order(OrderUpdate), Resync, Status}` on one unbounded channel consumed by `TradingService::on_event`. `Brokers` registry maps account kind to `Arc<dyn Broker>`; registration is in `TradingService::new`. **To add a broker**: new folder under `broker/`, implement the trait, register it; account kind = `kind()`. `BrokerError{message, unknown}`: `unknown` means outcome unknown, so the service marks the order UNKNOWN and resyncs.
**Order flow** (`service/orders.rs`): validate (pure `validate`, tested) → expiry check → server gate → account/broker → `resolve_contract` → build `OrderRow` (status TRANSIT) → `broker.pre_trade` (errors leave no row) → insert → `broker.place` → `apply_reply`. All state goes through `apply_state` (service/mod.rs): books fills via `store::record_fill`, terminal statuses sticky. Broker pushes go through `apply_update` (match by local id, broker id, then correlation id; unknown orders ignored).
**Paper broker** (`broker/paper/`): `matching.rs` (`evaluate` with slippage), `book.rs` (open orders in memory, reloaded from SQLite at start), `funds.rs` (pure margin maths, tested), `mod.rs` (`PaperBroker`: owns the price-change loop fed by the `LtpStore` notify channel and a 5 s day-order expiry loop). `pre_trade` = market hours, INTRADAY cut-off, `check_funds`. Latency is a sleep inside `place`/`modify`. Paper reply uses the local order id as `broker_order_id`. Fills arriving later are events; fills at place time come back in the `Reply`.
**Funds model**: cash = `startingCapital` + all-time realised − charges; long position blocks avg x qty, short blocks `strike x qty x short_margin_pct/100`; pending orders reserve the exposure-increasing part only. Skipped without `startingCapital` or if `paper.enforce_margin` is false. Expired contracts are not settled, so their premium is never lost.
**Auto square-off** (`service/squareoff.rs`, run by the 5 s sweeper): paper accounts only, INTRADAY only, at `paper.square_off_time` or for positions opened on an earlier date (start-up catch-up). Needs a price (live/stale quote, else last bar close); cancels the contract's open orders, writes a synthetic filled LIMIT order (message "Auto square-off", correlation `sq...`) through `apply_state`. Off when `paper_allow_off_hours` is true.
**Exit** (`service/exit.rs`): `exit_position` / `exit_all` cancel open orders on the contract, then `place` a LIMIT at `exit_price` (LTP ± `trading.exit_buffer_ticks`). `Position.exitPrice` is computed in the dashboard so the UI shows the same price. The frontend does no exit maths. Exit All continues past failures and returns `skipped` reasons.
**Quote**: `get_quote` returns `QuoteView{ltp, ageSecs, live}` from the `LtpStore`; the ticket polls it every second.
**Config** (`settings.json` `trading`): `exit_buffer_ticks` (2), `paper {enforce_margin true, short_margin_pct 10, slippage_ticks 0, latency_ms 0, auto_square_off true, square_off_time "15:20"}`. Existing keys unchanged. Like the rest of `trading`, applied at restart.
**Behaviour changes vs before**: paper orders start as TRANSIT (were PENDING); Exit cancels the contract's open orders and uses a 2-tick buffer; the sweeper is now 5 s (was 20 s) and only handles square-off and the 60 s order-book resync, since paper expiry moved into `PaperBroker`.
**Known gaps / unverified**: nothing here has run live; if saving a paper fill fails the order is not re-queued (old code retried); `sync` for Dhan now pushes events (applied asynchronously), so `sync_orders` returns before the ledger updates; no tests for `PaperBroker`/service flows beyond the pure helpers; square-off uses the live price only when one exists in memory after restart (else last bar).

### Phase 3 behaviour gaps
- Positions are keyed by account + instrument + product; no INTRADAY to MARGIN conversion, no expiry settlement (expired contracts just drop out). Paper INTRADAY auto square-off exists; live Dhan is squared off by the broker.
- Paper margin is a rough estimate (see Item 0 notes); expiry settlement is not implemented.
- Dhan partial fills are booked by cumulative-quantity deltas (unverified against a real feed); cancel of a partly filled order is allowed, modify is blocked.
- An order left `UNKNOWN` after retries is resolved only by the 60 s order-book resync.
- `save_account` does not explicitly drop a cached Dhan connection; credential changes are detected on next use (verify).
- `last_close` position fallback reads `bars`; confirm the instrument key matches what the recorder stores.

## Dev environment notes
- The user has Rust 1.94+ (sqlx 0.9) and builds with `pnpm tauri dev` / `cargo test --lib`. The sandbox does **not** build Rust (user preference). Verify the frontend with `npm i -g pnpm`, `pnpm install --frozen-lockfile`, `pnpm check`, `pnpm build` in `client/`.
- Sandbox network cannot reach nseindia.com or Fyers; live streaming cannot be tested there. Prefer unit tests with fixtures for engines and parsers (the user runs them).
- Moved/deleted Rust files: tell the user which old files to delete (a stale `service.rs` next to `service/mod.rs` breaks the build). Zip only edited/created files: `git status --porcelain --untracked-files=all | grep -v node_modules`.

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
- Settings other than `server` need a restart. The settings editor rewrites the file from `AppConfig`, so unknown keys are dropped.
- The order gate makes a proxy round trip per order/modify, and the proxy must be up even for paper trading.
- Exit places a LIMIT at LTP ± `exit_buffer_ticks`, which may still not fill in a fast market; there is no market order by design.

## Roadmap (not started; user's items first, then suggestions)
Suggested build order: finish 0 (bracket/OCO, expiry settlement), then 1, 4, 5 (risk), 2, 3. Confirm scope with the user before starting each; ask with options plus a recommendation.

**0. Foundation: broker interface and paper fidelity: DONE** (expiry settlement and bracket/OCO added after the first pass; see Item 0 addendum; not yet compiled or run). Done: `Broker` trait, margin/capital check, INTRADAY auto square-off, slippage/latency, order ticket live price, Exit All.

**1. Configurable signal rules (user).** Rules, defined in the UI and stored in SQLite, that produce buy/sell signals on charts. Example: BUY when RSI(14) crosses above its own EMA(9) and price > SMA(20) and price > SMA(50); SELL when RSI(14) crosses below its EMA(9). Design notes: a rule is a JSON condition tree (operands = indicator series with params or price; operators `crosses_above`, `crosses_below`, `>`, `<`; AND/OR) per instrument and interval, evaluated on bar close. `charts/indicators.rs` needs **indicators of indicators** (EMA of RSI) and a generic series-reference model. Signals should appear on charts, and the same engine should feed alerts and algos (items 3 and 4), so build it as a reusable rule engine, not chart-only code.

**2. Trade copier (user).** One master account, N child accounts, each with a factor (lot multiplier plus rounding rule, e.g. round down, minimum 1 lot or skip). A master execution replicates to children. Decide with the user: replicate on master order placement or on fill (recommend on fill), how modify/cancel propagate, behaviour when a child is rejected (continue others, alert), and per-child enable/disable. Children may be paper or Dhan and each child order must pass the order gate and risk checks. Keep a link table master order to child orders for the diary.

**3. Algo trading (user).** Autonomous orders from rules (item 1) with an action (side, product, lots, price rule, exit rule via stop/target). Needs arm/disarm per algo, paper-only until explicitly promoted, an event journal of every decision, and the risk controls in item 5. Limitation to state: the app must stay running (desktop app); no headless mode yet.

**4. Alerts (user).** Telegram bot (bot token in keychain, one or more chat ids) and email (SMTP, password in keychain). Events: signals, fills, rejections, algo actions, copier failures, connection loss, token expiry, daily P&L summary. Needs per-event toggles, throttling/de-duplication, and a test button.

**5. Risk controls (suggested, needed before 2 and 3).** Max daily loss (with auto-disarm of algos and optional square-off), max lots per order, max open positions, allowed trading window, and a global kill switch. Apply in `TradingService::place` so every path (manual, copier, algo) is covered.

**6. Backtesting (suggested).** Run a rule/algo over stored 1m/daily bars (Fyers backfill helps) and report trades, P&L after estimated charges, win rate, max drawdown. Lets rules be validated before arming.

**7. Smaller items (suggested).** Event/audit journal; CSV export of orders/trades/diary and a DB backup button; reminders for daily Fyers/Dhan token expiry; live-reload of non-server settings; tighten `tauri.conf.json` CSP before release; unit tests for `ServerConfig::base_url` and exit-order maths; remove unused template assets.