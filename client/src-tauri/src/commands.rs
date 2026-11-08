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
use crate::market::{market_clock, symbols};
use crate::state::SharedState;
use crate::stats::StreamStat;

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
    pub within_window: bool,
    pub mode: String,
    pub streams: Vec<StreamStat>,
}

#[tauri::command]
pub async fn market_status(state: State<'_, SharedState>) -> Res<MarketStatus> {
    let now = market_clock::get_ist_now();
    let ist = now + chrono::Duration::seconds(5 * 3600 + 30 * 60);
    let mut streams: Vec<StreamStat> = state.stats.read().await.streams.values().cloned().collect();
    streams.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(MarketStatus {
        ist_time: format!("{:02}:{:02}:{:02}", ist.hour(), ist.minute(), ist.second()),
        within_window: market_clock::is_within_trading_window(now),
        mode: state.session.mode().await.label().to_string(),
        streams,
    })
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
    state: State<'_, SharedState>,
    source: String,
    symbol: String,
    expiry: ExpiryItem,
) -> Res<()> {
    let mut slot = state.chain.lock().await;
    if let Some(old) = slot.take() {
        old.stop(&state.stats).await;
    }
    let handle = if source == "fyers" {
        let creds = active_fyers_creds(&state).await.map_err(err)?;
        runner::spawn_fyers(app, state.config.clone(), creds, state.recorder.clone(), state.ltp.clone(), symbol, expiry)
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
    *slot = Some(handle);
    Ok(())
}

#[tauri::command]
pub async fn stop_chain(state: State<'_, SharedState>) -> Res<()> {
    if let Some(h) = state.chain.lock().await.take() {
        h.stop(&state.stats).await;
    }
    Ok(())
}

// -------------------------------------------------------------- settings ---

#[tauri::command]
pub async fn get_setting(key: String, state: State<'_, SharedState>) -> Res<Option<String>> {
    db::get_setting(&state.db, &key).await.map_err(err)
}

#[tauri::command]
pub async fn set_setting(key: String, value: String, state: State<'_, SharedState>) -> Res<()> {
    db::set_setting(&state.db, &key, &value).await.map_err(err)
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

use crate::trading::{ContractInfo, Dashboard, DiaryDay, ExitAllResult, OrderRow, PlaceOrderInput, QuoteView, TradeRow};

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
pub async fn get_diary(
    account_id: Option<String>,
    from: String,
    to: String,
    state: State<'_, SharedState>,
) -> Res<Vec<DiaryDay>> {
    state.trading.diary(account_id.as_deref(), &from, &to).await.map_err(err)
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
