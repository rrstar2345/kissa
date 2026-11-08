# KISSA project handoff

## Working preferences (from the user)
- Keep answers concise. Do NOT create extra documents (summaries, quick references) unless asked; confirm first. Updating `CLAUDE.md` and the client `README.md` is fine when asked.
- Before big work, confirm scope: ask with options plus a recommendation, and review designs with the user before coding.
- **Do not install Rust/Cargo or compile.** The user builds and tests. Verify the frontend only (`npm i -g pnpm`, `pnpm install --frozen-lockfile`, `pnpm check`, `pnpm build` in `client/`) and re-read Rust carefully.
- When finished: present the edited/created files; zip them if more than 5 (`git status --porcelain --untracked-files=all | grep -v node_modules`). If files are moved or deleted, say which old ones to remove.

## Repos
- `https://github.com/rrstar2345/kissa.git`: `client/` (Tauri v2 app, the thing being built) and `broker-proxy/` (Go order proxy, done; do not modify unless asked; read its README for the contract).
- `https://github.com/rrstar2345/kstocks_egui`: reference-only older egui app.
- Fyers docs (web_fetch): option chain `https://fyers.in/api-docs/v3/api/market-data/option-chain/`, plus the Historical Data page.

## Objective
Desktop trading app for stock/options: live option prices from NSE public API (default) or Fyers (user's own creds), charts with indicators and rule-based signals, and order placement through the user's `broker-proxy` (static-IP VM) fronting Dhan. **Credentials, order log and account data stay client-side** (secrets in OS keychain, everything else in SQLite).

**Strategy: harden the product with paper trading first.** The user has not set up the VPS, Dhan or Fyers, so nothing real-broker or Fyers is validated. Paper must behave like a real broker so switching later is a small change. For dev without a VPS: run `broker-proxy` locally (localhost is allowed), `proxy keygen`, enter the key under Settings > Server connection (paper orders require it, see Order gate).

Stack: Tauri v2 (Rust) + SvelteKit/Svelte 5 + Tailwind v4 + DaisyUI 5 (themes `kissa-dark`/`kissa-light`, all styling in `src/app.css`), `sqlx` 0.9 SQLite, lightweight-charts.

## What is built
- **Accounts**: kinds `paper` (startingCapital, brokeragePerOrder), `dhan` (clientId, proxyId, accessToken secret), `proxy`, `fyers` (appId, accessToken secret). Frontend never receives secrets.
- **Option chain**: NSE or Fyers source, live ticks, derived OI change and Black-76 Greeks, configurable columns, click bid/ask/LTP to open the order ticket. NSE chain is empty outside market hours.
- **Charts**: multiple panels per page, instruments = indices and option strikes streamed on the chain page (ATM +-20). Ticks fold into 1m bars (`bars` table), other intervals resampled on read, daily rollup, Fyers backfill (never run live). Indicators: SMA, EMA, RSI, MACD, Bollinger, VWAP, Supertrend, plus auto signals (MA cross, RSI 30/70, MACD, BB, Supertrend flip).
- **Signals (configurable rules, Roadmap #1 done)**: `/signals` screen with editor (left) and live chart preview (right). Every chart panel has a Signals dropdown to apply saved rulesets to any instrument/interval.
  - Ruleset JSON in SQLite table `rulesets`: `vars[]` (rsi, ema, sma, macd, bb, atr, vwap, supertrend, formula; `src` = price source or an earlier var for chaining; `output` picks macd/bb/supertrend line; `plot` flag), `entry` (BUY) and `exit` (SELL) condition trees (`and`/`or` groups of `left op right`; ops `crosses_above crosses_below > >= < <= = <>`; operands = price source, var id or number).
  - Semantics: closed candles only; Entry fires when flat, Exit only after an Entry; with no Exit conditions Entry is ungated.
  - Code: `charts/rules/{mod.rs (model + validate), eval.rs (`evaluate(&Ruleset, &[Bar], closed)` returns lines + signals), store.rs}`; commands `list_rulesets, save_ruleset, delete_ruleset, validate_ruleset`; `get_chart` takes `rulesets` ids and an unsaved `draft`. Frontend `routes/signals`, `components/RulesetEditor.svelte`. **The engine is pure; reuse it for alerts, algos and backtests.**
- **Orders / trading** (`trading/`): order ticket, Orders/Trades/Positions pages, Dashboard (live P&L) and trader's diary with estimated charges (rates in `settings.json` `trading.charges`).
  - **Order flow** (`service/orders.rs`): validate -> server gate -> account/broker -> resolve contract -> `broker.pre_trade` -> insert -> `broker.place` -> `apply_reply`. All state changes go through `apply_state` (books fills via `store::record_fill`). Broker pushes (fills, WSS, resync) arrive as `BrokerEvent` and go through `apply_update`.
  - **Broker trait** (`trading/broker/mod.rs`, `async-trait`): `kind, needs_instrument_ids, pre_trade, place, modify, cancel, brokerage, connect, sync, forget, watched_contracts`. Registered in `TradingService::new`. To add a broker: new folder, implement trait, register. `BrokerError.unknown` = outcome unknown (order marked UNKNOWN, resynced).
  - **Paper broker** (`broker/paper/`): matching with slippage/latency, margin/funds check (simple estimate), day-order expiry, bracket/OCO (`PlaceOrderInput.bracket`, paper only), expiry settlement (`service/settle.rs`), INTRADAY auto square-off (`service/squareoff.rs`).
  - **Dhan via proxy**: `securityId` from Dhan instrument CSV (table `scrip`). **Never run live.**
  - **Exit / Exit All** (`service/exit.rs`): cancels the contract's open orders, then LIMIT at LTP +- `trading.exit_buffer_ticks`.
  - **Order gate** (`trading/server.rs`): `place`/`modify` require the proxy to authenticate the client (cached `paper` session re-validated per call). Cancel is not gated.
- **Settings page**: server connection (URL, port, API key in keychain) and a `settings.json` editor. Only the `server` section applies live; everything else needs a restart. `trading.paper {enforce_margin, short_margin_pct, slippage_ticks, latency_ms, auto_square_off, square_off_time}`.
- **Real-time UI**: `quotes` event (100 ms batches), `bar-update`, `chain-snapshot`, `order-update`, `broker-status`, `index-tick`.

## Roadmap (not started)
Confirm scope with the user before starting each.

**2. Risk controls (needed before 2 and 3).** Max daily loss (auto-disarm algos, optional square-off), max lots per order, max open positions, allowed trading window, global kill switch. Apply in `TradingService::place` so manual, copier and algo paths are all covered.

**3. Alerts.** Telegram bot (token in keychain, chat ids) and email (SMTP, password in keychain). Events: signals, fills, rejections, algo actions, copier failures, connection loss, token expiry, daily P&L summary. Per-event toggles, throttling/de-duplication, test button.

**4. Trade copier.** One master account, N child accounts, each with a factor (lot multiplier plus rounding rule, e.g. round down, minimum 1 lot or skip). A master execution replicates to children. Decide with the user: replicate on placement or on fill (recommend on fill), how modify/cancel propagate, behaviour when a child is rejected (continue others, alert), per-child enable/disable. Children may be paper or Dhan; every child order must pass the order gate and risk checks. Keep a link table master order -> child orders for the diary.

**5. Algo trading.** Autonomous orders from rulesets (item 1) with an action (side, product, lots, price rule, stop/target exit). Needs arm/disarm per algo, paper-only until explicitly promoted, an event journal of every decision, and the risk controls in item 5. Evaluate on bar close using `rules::eval`. Limitation to state: the app must stay running (no headless mode).

**6. Backtesting.** Run a ruleset/algo over stored 1m/daily bars (Fyers backfill helps): trades, P&L after estimated charges, win rate, max drawdown. Reuse `rules::eval::evaluate`.

**7. Smaller items.** Event/audit journal; CSV export of orders/trades/diary and a DB backup button; token-expiry reminders (Fyers/Dhan); live-reload of non-server settings; tighten `tauri.conf.json` CSP (currently null); unit tests for `ServerConfig::base_url` and exit maths; remove unused template assets in `client/static/`.

## Notes relevant to the backlog
- **broker-proxy contract**: REST + one WSS per client. Header `X-API-Key`; `POST /v1/sessions` (`{broker:"dhan",credentials:{clientId,accessToken}}` or `{broker:"paper"}`) returns `sessionId`, then also send `X-Session-Id`. Endpoints: `POST/PUT/DELETE /v1/orders[/{id}]`, `POST /v1/orders/slicing`, `GET /v1/orders`, `/v1/orders/{id}`, `/v1/orders/external/{correlationId}`, `GET /v1/trades`, `GET /v1/ws?since=N` (events `order_update`, `feed_status`, `session`, per-session `seq` with replay and `gap`). Rules: only LIMIT and STOP_LOSS with positive `price` (no MARKET), strict JSON, idempotency via `correlationId` (`409 outcome_unknown` = check `/orders/external/{id}` first). Server persists nothing, so **the client is the system of record**. One session per Dhan account; a proxy restart drops sessions (reopen and resync).
- Paper margin is a rough estimate; positions are keyed by account + instrument + product (no INTRADAY->MARGIN conversion).
- An order left `UNKNOWN` after retries is resolved only by the 60 s order-book resync. Dhan partial fills (cumulative-quantity deltas) are unverified. `save_account` does not explicitly drop a cached Dhan connection.
- Unverified against live systems: Fyers option-chain URL (`https://api-t1.fyers.in/data/options-chain-v3`, from memory), Fyers history URL/option symbol formats, Dhan CSV columns/date format, NSE OI change (derived from first OI seen unless NSE sends it). Fyers tokens expire daily.
- Charges rates and lot sizes (NIFTY 65, BANKNIFTY 30, FINNIFTY 60, MIDCPNIFTY 120, NIFTYNXT50 25) are estimates as of Nov 2026; verify.
- Sandbox cannot reach nseindia.com or Fyers; prefer unit tests with fixtures (the user runs `cargo test --lib`). SQLx 0.9: `format!`-built SQL must be wrapped in `sqlx::AssertSqlSafe(...)`.