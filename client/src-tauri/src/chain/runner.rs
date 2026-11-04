//! Starts/stops the background tasks that feed the option chain view.

use chrono::NaiveDate;
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::JoinHandle;

use super::engine::ChainEngine;
use super::fyers::{self, FyersCreds};
use super::ExpiryItem;
use crate::market::events::IndexStore;
use crate::market::streamers::options;
use crate::market::market_clock::SharedSessionState;
use crate::settings::AppConfig;
use crate::stats::SharedStats;

pub const SNAPSHOT_EVENT: &str = "chain-snapshot";
pub const STATUS_EVENT: &str = "chain-status";

#[derive(Debug, Clone, Serialize)]
pub struct ChainStatus {
    pub state: &'static str, // "started" | "error"
    pub message: String,
}

pub struct ChainHandle {
    tasks: Vec<JoinHandle<()>>,
    stream_key: Option<String>,
}

impl ChainHandle {
    pub async fn stop(self, stats: &SharedStats) {
        for t in &self.tasks {
            t.abort();
        }
        if let Some(k) = self.stream_key {
            stats.write().await.remove_stream(&k);
        }
    }
}

/// Look up the first candidate name present on the indices stream.
pub fn find_index(
    store: &IndexStore,
    candidates: &[String],
) -> Option<crate::market::events::IndexTick> {
    let map = store.read().ok()?;
    for c in candidates {
        if let Some(t) = map.values().find(|t| t.index_name.eq_ignore_ascii_case(c)) {
            return Some(t.clone());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_nse(
    app: AppHandle,
    cfg: AppConfig,
    stats: SharedStats,
    session: SharedSessionState,
    store: IndexStore,
    symbol: String,
    expiry: ExpiryItem,
    index_names: Vec<String>,
) -> anyhow::Result<ChainHandle> {
    let date = NaiveDate::parse_from_str(&expiry.date, "%Y-%m-%d")?;
    let (tx, mut rx) = unbounded_channel();
    let stream_key = format!("options:{}:{}", symbol, expiry.value);

    let streamer = tokio::spawn(options::run(
        cfg.clone(),
        symbol.clone(),
        expiry.value.clone(),
        tx,
        stats,
        session,
    ));

    let engine_task = tokio::spawn(async move {
        let mut engine = ChainEngine::new(symbol, expiry.label.clone(), date, cfg.risk_free_rate);
        let mut ticker = tokio::time::interval(Duration::from_millis(300));
        loop {
            tokio::select! {
                Some(t) = rx.recv() => engine.apply(t),
                _ = ticker.tick() => {
                    if engine.is_empty() { continue; }
                    let spot = find_index(&store, &index_names);
                    let vix = find_index(&store, &["INDIA VIX".to_string()]).map(|t| t.current_price);
                    let _ = app.emit(SNAPSHOT_EVENT, engine.snapshot(spot.as_ref(), vix));
                }
            }
        }
    });

    Ok(ChainHandle { tasks: vec![streamer, engine_task], stream_key: Some(stream_key) })
}

pub fn spawn_fyers(
    app: AppHandle,
    cfg: AppConfig,
    creds: FyersCreds,
    symbol: String,
    expiry: ExpiryItem,
) -> ChainHandle {
    let task = tokio::spawn(async move {
        let poll = Duration::from_secs(cfg.fyers.poll_secs.max(1));
        loop {
            match fyers::fetch(&cfg.fyers, &creds, &symbol, cfg.fyers.strike_count, Some(&expiry.value)).await {
                Ok(data) => {
                    let _ = app.emit(SNAPSHOT_EVENT, fyers::to_snapshot(&data, &symbol, &expiry, cfg.risk_free_rate));
                }
                Err(e) => {
                    let _ = app.emit(STATUS_EVENT, ChainStatus { state: "error", message: e.to_string() });
                }
            }
            tokio::time::sleep(poll).await;
        }
    });
    ChainHandle { tasks: vec![task], stream_key: None }
}
