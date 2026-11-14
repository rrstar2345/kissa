use chrono::{NaiveDate, Timelike};
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::alerts::{config::AlertConfig, store::AlertRow, AlertSettings};
use crate::accounts::{self, Account, SaveAccountInput, TestResult};
use crate::chain::runner;
use crate::charts::rules::{self, eval as rule_eval, Ruleset};
use crate::charts::{backfill, indicators, resample, store, BackfillResult, ChartRequest, ChartResponse, InstrumentList};
use crate::chain::{fyers, ExpiryItem};
use crate::db;
use crate::market::events::IndexTick;
use crate::market::status::{self, DataSource, SourceStatus};
use crate::market::streamers::indices::STREAM_NAME as INDICES_STREAM;
use crate::market::{market_clock, symbols};
use crate::state::{AppState, SharedState};
use crate::stats::{ConnState, StreamStat};

type Res<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

const FALLBACK_SYMBOLS: [&str; 5] = ["NIFTY", "BANKNIFTY", "FINNIFTY", "MIDCPNIFTY", "NIFTYNXT50"];

// ---------------------------------------------------------------- market ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketStatus {
    pub ist_time: String,
    /// UTC epoch milliseconds (NSE-synced clock) at the time of the snapshot, so
    /// the UI can keep its own clock running between pushes.
    pub now_ms: i64,
    /// Clock only (weekday 08:30-16:00 IST); it does not know holidays. Use `exchange`.
    pub within_window: bool,
    /// ACTIVE while the selected source's feed is connected, else IDLE.
    pub mode: String,
    /// The selected data source. The header shows this one only.
    pub source: DataSource,
    /// The selected source's exchange status (open, pre-open, closed, unknown).
    pub exchange: SourceStatus,
    /// Each source's own status; the one that is not selected reads `notUsed`.
    pub nse: SourceStatus,
    pub fyers: SourceStatus,
    /// Prices are ticking from the selected source in an open session.
    pub live: bool,
    /// Short label for the feed indicator (Live, No ticks, Pre-open, Closed, ...).
    pub feed: String,
    /// Tooltip for the feed indicator: which stream, and its error if any.
    pub feed_detail: String,
    /// Login state of the Fyers data account; only set while Fyers is the data source.
    pub fyers_auth: Option<accounts::FyersAuth>,
    pub streams: Vec<StreamStat>,
}

/// A feed with no real tick for this long is not "live" any more.
const LIVE_STALE_SECS: i64 = 60;

impl MarketStatus {
    /// Everything the UI shows except the clock and tick counters. The status
    /// push task emits only when this changes.
    pub fn change_key(&self) -> String {
        let streams: Vec<_> = self
            .streams
            .iter()
            .map(|s| (&s.name, &s.state, &s.last_error, s.reconnect_count))
            .collect();
        serde_json::json!({
            "w": self.within_window,
            "m": &self.mode,
            "src": &self.source,
            "n": &self.nse,
            "f": &self.fyers,
            "l": self.live,
            "fd": &self.feed,
            "fx": &self.feed_detail,
            "fa": &self.fyers_auth,
            "s": streams,
        })
        .to_string()
    }
}

pub async fn build_market_status(state: &AppState) -> MarketStatus {
    let now = market_clock::get_ist_now();
    let ist = now + chrono::Duration::seconds(5 * 3600 + 30 * 60);
    let mut streams: Vec<StreamStat> = state.stats.read().await.streams.values().cloned().collect();
    streams.sort_by(|a, b| a.name.cmp(&b.name));

    let source = state.market_status.source();
    let exchange = state.market_status.active();

    // The index feed of the selected source: NSE's indices stream, or the Fyers socket.
    let (connected, ticking, errored, detail) = match source {
        DataSource::Nse => {
            let s = streams.iter().find(|s| s.name == INDICES_STREAM);
            let connected = s.is_some_and(|s| s.state == ConnState::Connected);
            let age = s
                .and_then(|s| s.last_tick_at)
                .map(|t| chrono::Local::now().signed_duration_since(t).num_seconds());
            let ticking = connected && age.is_some_and(|a| a <= LIVE_STALE_SECS);
            let errored = s.is_some_and(|s| s.state == ConnState::Reconnecting);
            let word = match s.map(|s| &s.state) {
                Some(ConnState::Connected) => "connected",
                Some(ConnState::Connecting) => "connecting",
                Some(ConnState::Reconnecting) => "reconnecting",
                Some(ConnState::Idle) | None => "idle",
            };
            let mut detail = format!("NSE index stream: {word}");
            if let Some(e) = s.and_then(|s| s.last_error.as_deref()).filter(|_| errored) {
                detail.push_str(&format!(" ({e})"));
            }
            (connected, ticking, errored, detail)
        }
        DataSource::Fyers => {
            let hub = &state.fyers_hub;
            let connected = hub.is_connected();
            let ticking = connected && hub.last_tick_age_secs().is_some_and(|a| a <= LIVE_STALE_SECS);
            let error = hub.last_error();
            let mut detail = format!("Fyers data socket: {}", if connected { "connected" } else { "not connected" });
            if let Some(e) = &error {
                detail.push_str(&format!(" ({e})"));
            }
            (connected, ticking, error.is_some(), detail)
        }
    };
    let (live, feed) = status::feed_label(exchange.state, connected, ticking, errored, status::is_session_hours(now));
    let mode = match source {
        DataSource::Nse => state.session.mode().await.label().to_string(),
        DataSource::Fyers => (if connected { "ACTIVE" } else { "IDLE" }).to_string(),
    };

    MarketStatus {
        ist_time: format!("{:02}:{:02}:{:02}", ist.hour(), ist.minute(), ist.second()),
        now_ms: now.timestamp_millis(),
        within_window: market_clock::is_within_trading_window(now),
        mode,
        source,
        exchange,
        nse: state.market_status.nse(),
        fyers: state.market_status.fyers(),
        live,
        feed: feed.to_string(),
        feed_detail: detail,
        fyers_auth: if source == DataSource::Fyers { Some(accounts::fyers_auth(&state.db).await) } else { None },
        streams,
    }
}

#[tauri::command]
pub async fn market_status(state: State<'_, SharedState>) -> Res<MarketStatus> {
    Ok(build_market_status(&state).await)
}

#[tauri::command]
pub fn indices_snapshot(state: State<'_, SharedState>) -> Vec<IndexTick> {
    state.indices.read().map(|m| m.values().cloned().collect()).unwrap_or_default()
}

// ---------------------------------------------------------- symbols/chain ---

#[tauri::command]
pub async fn list_fno_symbols(state: State<'_, SharedState>) -> Res<Vec<String>> {
    match symbols::fetch_fno_indices(&state.config).await {
        Ok(list) => {
            let names = list.iter().map(|i| i.symbol.clone()).collect();
            *state.fno.write().await = list;
            Ok(names)
        }
        Err(e) => {
            tracing::warn!("F&O symbol lookup failed ({e}); using built-in list");
            Ok(FALLBACK_SYMBOLS.iter().map(|s| s.to_string()).collect())
        }
    }
}

pub(crate) async fn active_fyers_creds(state: &SharedState) -> anyhow::Result<fyers::FyersCreds> {
    let id = db::get_setting(&state.db, "data.fyers_account")
        .await?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("No Fyers account selected. Add one under Accounts and choose it as the data source."))?;
    accounts::fyers_creds(&state.db, &id).await
}

#[tauri::command]
pub async fn list_expiries(source: String, symbol: String, state: State<'_, SharedState>) -> Res<Vec<ExpiryItem>> {
    if source == "fyers" {
        let creds = active_fyers_creds(&state).await.map_err(err)?;
        let data = fyers::fetch(&state.config.fyers, &creds, &symbol, 1, None).await.map_err(err)?;
        return Ok(fyers::expiries(&data));
    }
    let list = symbols::fetch_expiries(&state.config, &symbol).await.map_err(err)?;
    Ok(list
        .into_iter()
        .filter_map(|label| {
            let d = NaiveDate::parse_from_str(&label, "%d-%b-%Y").ok()?;
            Some(ExpiryItem { value: label.clone(), label, date: d.format("%Y-%m-%d").to_string() })
        })
        .collect())
}

#[tauri::command]
pub async fn start_chain(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, SharedState>,
    source: String,
    symbol: String,
    expiry: ExpiryItem,
) -> Res<()> {
    // One chain per window: starting replaces only this window's chain.
    let label = window.label().to_string();
    let mut slot = state.chain.lock().await;
    if let Some(old) = slot.remove(&label) {
        runner::stop_handle(&state.stats, &slot, old).await;
    }
    let handle = if source == "fyers" {
        let creds = active_fyers_creds(&state).await.map_err(err)?;
        runner::spawn_fyers(
            app,
            label.clone(),
            state.config.clone(),
            creds,
            state.market_status.clone(),
            state.recorder.clone(),
            state.ltp.clone(),
            state.fyers_hub.clone(),
            symbol,
            expiry,
        )
    } else {
        let index_names = state
            .fno
            .read()
            .await
            .iter()
            .find(|i| i.symbol == symbol)
            .map(|i| i.index_names.clone())
            .unwrap_or_default();
        runner::spawn_nse(
            app,
            label.clone(),
            state.config.clone(),
            state.stats.clone(),
            state.session.clone(),
            state.indices.clone(),
            state.recorder.clone(),
            state.ltp.clone(),
            symbol,
            expiry,
            index_names,
        )
        .map_err(err)?
    };
    slot.insert(label, handle);
    Ok(())
}

#[tauri::command]
pub async fn stop_chain(window: tauri::WebviewWindow, state: State<'_, SharedState>) -> Res<()> {
    runner::stop_window_chain(&state, window.label()).await;
    Ok(())
}

// -------------------------------------------------------------- settings ---

#[tauri::command]
pub async fn get_setting(key: String, state: State<'_, SharedState>) -> Res<Option<String>> {
    db::get_setting(&state.db, &key).await.map_err(err)
}

#[tauri::command]
pub async fn set_setting(key: String, value: String, state: State<'_, SharedState>) -> Res<()> {
    db::set_setting(&state.db, &key, &value).await.map_err(err)?;
    if key == "data.source" {
        // Move the exchange status and the streams over to the new source now.
        state.market_status.set_source(DataSource::from_setting(Some(&value)));
    }
    Ok(())
}

// -------------------------------------------------------------- accounts ---

#[tauri::command]
pub async fn list_accounts(state: State<'_, SharedState>) -> Res<Vec<Account>> {
    accounts::list(&state.db).await.map_err(err)
}

#[tauri::command]
pub async fn save_account(input: SaveAccountInput, state: State<'_, SharedState>) -> Res<Account> {
    accounts::save(&state.db, input).await.map_err(err)
}

#[tauri::command]
pub async fn delete_account(id: String, state: State<'_, SharedState>) -> Res<()> {
    accounts::delete(&state.db, &id).await.map_err(err)?;
    state.trading.forget_account(&id).await;
    Ok(())
}

#[tauri::command]
pub async fn test_account(id: String, state: State<'_, SharedState>) -> Res<TestResult> {
    Ok(match accounts::test(&state.db, &state.config, &id).await {
        Ok(r) => r,
        Err(e) => TestResult { ok: false, message: e.to_string() },
    })
}

/// Fyers login step 1: the URL to open in the browser.
#[tauri::command]
pub async fn fyers_login_url(id: String, state: State<'_, SharedState>) -> Res<String> {
    accounts::fyers_login_url(&state.db, &state.config, &id).await.map_err(err)
}

/// Fyers one-click login: listens on the redirect URL's local port, opens the
/// browser and returns once the access token is saved.
#[tauri::command]
pub async fn fyers_login_local(id: String, app: tauri::AppHandle, state: State<'_, SharedState>) -> Res<()> {
    use tauri_plugin_opener::OpenerExt;
    accounts::fyers_login_local(&state.db, &state.config, &id, |url| {
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| anyhow::anyhow!("Could not open the browser: {e}"))
    })
    .await
    .map_err(err)
}

#[tauri::command]
pub fn fyers_login_cancel(id: String) {
    accounts::fyers_login_cancel(&id);
}

/// After a Fyers login: runs the account's test call and, if it passes, seeds
/// the header indices again (and lets the data socket retry at once).
#[tauri::command]
pub async fn fyers_login_finished(id: String, state: State<'_, SharedState>) -> Res<TestResult> {
    let result = match accounts::test(&state.db, &state.config, &id).await {
        Ok(r) => r,
        Err(e) => TestResult { ok: false, message: e.to_string() },
    };
    if result.ok {
        state.fyers_hub.token_renewed();
    }
    Ok(result)
}

/// Fyers login step 2: exchange the pasted redirect URL / auth code for an access token.
#[tauri::command]
pub async fn fyers_login(id: String, pasted: String, state: State<'_, SharedState>) -> Res<()> {
    accounts::fyers_login(&state.db, &state.config, &id, &pasted).await.map_err(err)
}

// ---------------------------------------------------------------- charts ---

#[tauri::command]
pub async fn get_chart(req: ChartRequest, state: State<'_, SharedState>) -> Res<ChartResponse> {
    let interval = resample::parse_interval(&req.interval)
        .ok_or_else(|| format!("Unsupported interval: {}", req.interval))?;
    state.recorder.flush_now().await;
    let bars = store::chart_bars(&state.db, &req.instrument, interval, req.limit.unwrap_or(1500))
        .await
        .map_err(err)?;
    let mut out = indicators::compute(&bars, &req.indicators);

    // Rulesets: saved ones by id plus an optional unsaved draft. Only closed
    // candles can signal, so the forming one is left out of evaluation.
    let mut warnings = Vec::new();
    let mut sets: Vec<Ruleset> = Vec::new();
    if !req.rulesets.is_empty() {
        let all = rules::store::list(&state.db).await.map_err(err)?;
        sets.extend(all.into_iter().filter(|r| req.rulesets.contains(&r.id)));
    }
    if let Some(d) = req.draft.clone() {
        sets.push(d);
    }
    if !sets.is_empty() {
        let step = match interval {
            resample::Interval::Min(n) => n * 60,
            resample::Interval::Day => 86_400,
        };
        let now = chrono::Utc::now().timestamp();
        let closed = match bars.last() {
            Some(b) if b.ts + step > now => bars.len() - 1,
            _ => bars.len(),
        };
        for rs in &sets {
            match rule_eval::evaluate(rs, &bars, closed) {
                Ok(o) => {
                    out.lines.extend(o.lines);
                    out.signals.extend(o.signals);
                }
                Err(e) => warnings.push(format!("{}: {e}", rs.name)),
            }
        }
        out.signals.sort_by_key(|s| s.time);
    }
    Ok(ChartResponse { bars, lines: out.lines, signals: out.signals, warnings })
}

#[tauri::command]
pub async fn list_rulesets(state: State<'_, SharedState>) -> Res<Vec<Ruleset>> {
    rules::store::list(&state.db).await.map_err(err)
}

#[tauri::command]
pub async fn save_ruleset(ruleset: Ruleset, state: State<'_, SharedState>) -> Res<Ruleset> {
    rules::store::save(&state.db, ruleset).await.map_err(err)
}

#[tauri::command]
pub async fn delete_ruleset(id: String, state: State<'_, SharedState>) -> Res<()> {
    rules::store::delete(&state.db, &id).await.map_err(err)
}

#[tauri::command]
pub fn validate_ruleset(ruleset: Ruleset) -> Vec<String> {
    ruleset.validate()
}

#[tauri::command]
pub async fn list_chart_instruments(state: State<'_, SharedState>) -> Res<InstrumentList> {
    store::list_instruments(&state.db).await.map_err(err)
}

/// Fill history gaps from Fyers (needs the Fyers data account to be set).
#[tauri::command]
pub async fn backfill_chart(
    instrument: String,
    interval: String,
    days: u32,
    state: State<'_, SharedState>,
) -> Res<BackfillResult> {
    let creds = active_fyers_creds(&state).await.map_err(err)?;
    backfill::backfill(&state.config.fyers, &creds, &state.db, &instrument, &interval, days)
        .await
        .map_err(err)
}

// --------------------------------------------------------------- trading ---

use crate::trading::period::Performance;
use crate::trading::{ContractInfo, Dashboard, ExitAllResult, OrderRow, PlaceOrderInput, QuoteView, TradeRow};

#[tauri::command]
pub async fn place_order(input: PlaceOrderInput, state: State<'_, SharedState>) -> Res<OrderRow> {
    state.trading.place(input).await.map_err(err)
}

#[tauri::command]
pub async fn cancel_order(id: String, state: State<'_, SharedState>) -> Res<OrderRow> {
    state.trading.cancel(&id).await.map_err(err)
}

#[tauri::command]
pub async fn modify_order(
    id: String,
    price: f64,
    trigger_price: Option<f64>,
    state: State<'_, SharedState>,
) -> Res<OrderRow> {
    state.trading.modify(&id, price, trigger_price).await.map_err(err)
}

#[tauri::command]
pub async fn list_orders(
    account_id: Option<String>,
    date: Option<String>,
    state: State<'_, SharedState>,
) -> Res<Vec<OrderRow>> {
    state.trading.list_orders(account_id.as_deref(), date.as_deref()).await.map_err(err)
}

#[tauri::command]
pub async fn list_trades(
    account_id: Option<String>,
    date: Option<String>,
    state: State<'_, SharedState>,
) -> Res<Vec<TradeRow>> {
    state.trading.list_trades(account_id.as_deref(), date.as_deref()).await.map_err(err)
}

#[tauri::command]
pub async fn get_dashboard(account_id: Option<String>, state: State<'_, SharedState>) -> Res<Dashboard> {
    state.trading.dashboard(account_id.as_deref()).await.map_err(err)
}

#[tauri::command]
pub async fn get_performance(
    view: String,
    anchor: String,
    account_id: Option<String>,
    state: State<'_, SharedState>,
) -> Res<Performance> {
    state.trading.performance(&view, &anchor, account_id.as_deref()).await.map_err(err)
}

#[tauri::command]
pub async fn contract_info(
    symbol: String,
    expiry: String,
    strike: f64,
    is_call: bool,
    state: State<'_, SharedState>,
) -> Res<ContractInfo> {
    state.trading.contract_info(&symbol, &expiry, strike, is_call).await.map_err(err)
}

/// Download Dhan's instrument list. Returns the number of option contracts stored.
#[tauri::command]
pub async fn refresh_instruments(state: State<'_, SharedState>) -> Res<usize> {
    state.trading.refresh_instruments().await.map_err(err)
}

/// Exit one open position (cancels the contract's open orders, then a
/// marketable LIMIT on the opposite side).
#[tauri::command]
pub async fn exit_position(
    account_id: String,
    instrument: String,
    product: String,
    state: State<'_, SharedState>,
) -> Res<OrderRow> {
    state.trading.exit_position(&account_id, &instrument, &product).await.map_err(err)
}

/// Exit every open position, of one account or all accounts.
#[tauri::command]
pub async fn exit_all(account_id: Option<String>, state: State<'_, SharedState>) -> Res<ExitAllResult> {
    state.trading.exit_all(account_id.as_deref()).await.map_err(err)
}

// ---------------------------------------------------------------- risk ---

use crate::trading::risk::{KillResult, RiskLimits, RiskOverview};

/// Limits, halts and today's P&L of every account, plus the kill switch.
#[tauri::command]
pub async fn get_risk(state: State<'_, SharedState>) -> Res<RiskOverview> {
    state.trading.risk_overview().await.map_err(err)
}

#[tauri::command]
pub async fn save_risk_limits(
    account_id: String,
    limits: RiskLimits,
    state: State<'_, SharedState>,
) -> Res<RiskLimits> {
    state.trading.save_risk_limits(&account_id, limits).await.map_err(err)
}

/// Kill switch on (optionally flattening every position) or off.
#[tauri::command]
pub async fn set_kill_switch(on: bool, flatten: bool, state: State<'_, SharedState>) -> Res<KillResult> {
    state.trading.set_kill_switch(on, flatten).await.map_err(err)
}

/// Lift today's daily-loss halt of an account.
#[tauri::command]
pub async fn resume_account(account_id: String, state: State<'_, SharedState>) -> Res<()> {
    state.trading.resume_account(&account_id).await.map_err(err)
}

// ----------------------------------------------------------------- algos ---

use crate::algo::{self, Algo, AlgoView, JournalRow};
use tauri::Emitter;

fn algo_changed(app: &AppHandle) {
    let _ = app.emit("algo-update", true);
}

/// Every algo with its run state (armed, flat / in position, open legs).
#[tauri::command]
pub async fn list_algos(state: State<'_, SharedState>) -> Res<Vec<AlgoView>> {
    algo::service::list_views(&state.db).await.map_err(err)
}

/// Validate and save an algo (a blank id creates one).
#[tauri::command]
pub async fn save_algo(algo: Algo, app: AppHandle, state: State<'_, SharedState>) -> Res<Algo> {
    let saved = algo::service::save(&state.db, algo).await.map_err(err)?;
    algo_changed(&app);
    Ok(saved)
}

#[tauri::command]
pub async fn delete_algo(id: String, app: AppHandle, state: State<'_, SharedState>) -> Res<()> {
    algo::service::delete(&state.db, &id).await.map_err(err)?;
    algo_changed(&app);
    Ok(())
}

/// Arm (after safety checks) or disarm an algo.
#[tauri::command]
pub async fn set_algo_armed(id: String, armed: bool, app: AppHandle, state: State<'_, SharedState>) -> Res<AlgoView> {
    algo::service::set_armed(state.inner(), &id, armed).await.map_err(err)?;
    algo_changed(&app);
    algo::service::view(&state.db, &id).await.map_err(err)
}

/// Problems with an algo's own fields (the form shows them as you type).
#[tauri::command]
pub fn validate_algo(algo: Algo) -> Vec<String> {
    let mut a = algo;
    a.normalize();
    a.validate()
}

/// The decision journal, newest first; one algo or all.
#[tauri::command]
pub async fn list_algo_journal(algo_id: Option<String>, limit: Option<i64>, state: State<'_, SharedState>) -> Res<Vec<JournalRow>> {
    algo::store::journal_list(&state.db, algo_id.as_deref(), limit.unwrap_or(300)).await.map_err(err)
}

#[tauri::command]
pub async fn clear_algo_journal(algo_id: Option<String>, app: AppHandle, state: State<'_, SharedState>) -> Res<()> {
    algo::store::journal_clear(&state.db, algo_id.as_deref()).await.map_err(err)?;
    algo_changed(&app);
    Ok(())
}

// ---------------------------------------------------------------- copier ---

use crate::trading::copier::{CopierConfig, CopierOverview};

/// Copier settings, the accounts that can take part and recent copies.
#[tauri::command]
pub async fn get_copier(state: State<'_, SharedState>) -> Res<CopierOverview> {
    state.trading.copier_overview().await.map_err(err)
}

#[tauri::command]
pub async fn save_copier(config: CopierConfig, state: State<'_, SharedState>) -> Res<CopierConfig> {
    state.trading.save_copier_config(config).await.map_err(err)
}

// ---------------------------------------------------------------- alerts ---

/// Alert channels, event toggles and options. Secret values are never returned,
/// only whether they are set.
#[tauri::command]
pub async fn get_alert_settings(state: State<'_, SharedState>) -> Res<AlertSettings> {
    Ok(state.alerts.settings().await)
}

/// Validate and save. A blank token / password keeps the stored one.
#[tauri::command]
pub async fn save_alert_settings(
    config: AlertConfig,
    telegram_token: Option<String>,
    smtp_password: Option<String>,
    state: State<'_, SharedState>,
) -> Res<AlertSettings> {
    state.alerts.save_settings(config, telegram_token, smtp_password).await.map_err(err)
}

/// Send a test message (`telegram` | `email`) using the saved settings.
#[tauri::command]
pub async fn test_alert(channel: String, state: State<'_, SharedState>) -> Res<TestResult> {
    Ok(match state.alerts.send_test(&channel).await {
        Ok(message) => TestResult { ok: true, message },
        Err(e) => TestResult { ok: false, message: e.to_string() },
    })
}

#[tauri::command]
pub async fn list_alert_history(limit: Option<i64>, state: State<'_, SharedState>) -> Res<Vec<AlertRow>> {
    state.alerts.history(limit.unwrap_or(200)).await.map_err(err)
}

#[tauri::command]
pub async fn clear_alert_history(state: State<'_, SharedState>) -> Res<()> {
    state.alerts.clear_history().await.map_err(err)
}

/// Latest price of a contract, for the order ticket.
#[tauri::command]
pub fn get_quote(
    symbol: String,
    expiry: String,
    strike: f64,
    is_call: bool,
    state: State<'_, SharedState>,
) -> QuoteView {
    state.trading.quote_view(&symbol, &expiry, strike, is_call)
}

/// Open the broker session and live order updates for an account.
#[tauri::command]
pub async fn connect_broker(account_id: String, state: State<'_, SharedState>) -> Res<()> {
    state.trading.connect_broker(&account_id).await.map_err(err)
}

/// Re-read the broker's order book now.
#[tauri::command]
pub async fn sync_orders(account_id: String, state: State<'_, SharedState>) -> Res<usize> {
    state.trading.sync_account(&account_id).await.map_err(err)
}

// ------------------------------------------------- settings.json / server ---

use crate::settings::{self, ServerConfig};
use crate::trading::server as server_auth;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    pub url: String,
    pub port: u16,
    pub api_key_set: bool,
}

fn server_settings(cfg: &ServerConfig) -> ServerSettings {
    ServerSettings { url: cfg.url.clone(), port: cfg.port, api_key_set: server_auth::api_key_set() }
}

/// The settings file as editable JSON.
#[tauri::command]
pub fn get_settings_json(state: State<'_, SharedState>) -> Res<String> {
    settings::read_settings_json(&state.paths)
}

/// Validate and save the whole settings file. The server section applies
/// immediately; everything else is read at startup, so restart the app.
#[tauri::command]
pub async fn save_settings_json(json: String, state: State<'_, SharedState>) -> Res<()> {
    let cfg = settings::write_settings_json(&state.paths, &json)?;
    state.trading.set_server(cfg.server).await;
    Ok(())
}

#[tauri::command]
pub fn get_server_settings(state: State<'_, SharedState>) -> Res<ServerSettings> {
    let json = settings::read_settings_json(&state.paths)?;
    let cfg: settings::AppConfig = serde_json::from_str(&json).map_err(err)?;
    Ok(server_settings(&cfg.server))
}

/// Save the server URL/port to settings.json and the API key to the OS
/// keychain. A blank `api_key` keeps the stored key.
#[tauri::command]
pub async fn save_server_settings(
    url: String,
    port: u16,
    api_key: Option<String>,
    state: State<'_, SharedState>,
) -> Res<ServerSettings> {
    let server = ServerConfig { url: url.trim().to_string(), port };
    if server.is_set() {
        server.base_url()?;
    }
    let json = settings::read_settings_json(&state.paths)?;
    let mut cfg: settings::AppConfig = serde_json::from_str(&json).map_err(err)?;
    cfg.server = server.clone();
    let json = serde_json::to_string_pretty(&cfg).map_err(err)?;
    settings::write_settings_json(&state.paths, &json)?;
    if let Some(k) = api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        crate::secrets::set(server_auth::KEY_ACCOUNT, server_auth::KEY_FIELD, k).map_err(err)?;
    }
    state.trading.set_server(server.clone()).await;
    Ok(server_settings(&server))
}

/// Check the saved server details: reachable and the API key accepted.
#[tauri::command]
pub async fn test_server(state: State<'_, SharedState>) -> Res<TestResult> {
    Ok(match state.trading.test_server().await {
        Ok(message) => TestResult { ok: true, message },
        Err(e) => TestResult { ok: false, message: e.to_string() },
    })
}

// -------------------------------------------------------------- backtest ---

/// Run a saved signal over stored bars of one instrument.
#[tauri::command]
pub async fn run_backtest(
    req: crate::backtest::BacktestRequest,
    state: State<'_, SharedState>,
) -> Res<crate::backtest::Report> {
    state.recorder.flush_now().await;
    crate::backtest::run(&state.db, &state.config, req).await.map_err(err)
}
