use chrono::{NaiveDate, Timelike};
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::accounts::{self, Account, SaveAccountInput, TestResult};
use crate::chain::runner;
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

async fn active_fyers_creds(state: &SharedState) -> anyhow::Result<fyers::FyersCreds> {
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
        runner::spawn_fyers(app, state.config.clone(), creds, symbol, expiry)
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
    accounts::delete(&state.db, &id).await.map_err(err)
}

#[tauri::command]
pub async fn test_account(id: String, state: State<'_, SharedState>) -> Res<TestResult> {
    Ok(match accounts::test(&state.db, &state.config, &id).await {
        Ok(r) => r,
        Err(e) => TestResult { ok: false, message: e.to_string() },
    })
}
