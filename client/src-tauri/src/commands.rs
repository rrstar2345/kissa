use chrono::{NaiveDate, Timelike};
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::accounts::{self, Account, SaveAccountInput, TestResult};
use crate::chain::runner;
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
    let out = indicators::compute(&bars, &req.indicators);
    Ok(ChartResponse { bars, lines: out.lines, signals: out.signals })
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

use crate::trading::{ContractInfo, Dashboard, DiaryDay, OrderRow, PlaceOrderInput, TradeRow};

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

/// Open the proxy session and live order updates for a Dhan account.
#[tauri::command]
pub async fn connect_broker(account_id: String, state: State<'_, SharedState>) -> Res<()> {
    state.trading.connect_broker(&account_id).await.map_err(err)
}

/// Re-read the broker's order book now.
#[tauri::command]
pub async fn sync_orders(account_id: String, state: State<'_, SharedState>) -> Res<usize> {
    state.trading.sync_dhan(&account_id).await.map_err(err)
}
