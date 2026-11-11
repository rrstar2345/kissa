//! Starts/stops the background tasks that feed the option chain view.

use chrono::NaiveDate;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::JoinHandle;

use super::engine::ChainEngine;
use super::fyers::{self, FyersCreds};
use super::ExpiryItem;
use crate::charts::instrument::option_key;
use crate::charts::recorder::Recorder;
use crate::market::events::IndexStore;
use crate::market::streamers::options;
use crate::market::market_clock::{self, SharedSessionState};
use crate::market::status::SharedMarketStatus;
use crate::settings::AppConfig;
use crate::stats::SharedStats;
use crate::trading::ltp::LtpStore;

pub const SNAPSHOT_EVENT: &str = "chain-snapshot";
pub const STATUS_EVENT: &str = "chain-status";

/// How often a changed chain is pushed to the UI (about 7 updates a second).
const SNAPSHOT_MS: u64 = 150;

#[derive(Debug, Clone, Serialize)]
pub struct ChainStatus {
    pub state: &'static str, // "started" | "error"
    pub message: String,
}

pub struct ChainHandle {
    tasks: Vec<JoinHandle<()>>,
    stream_key: Option<String>,
    /// (symbol, expiry ISO date) this handle streams.
    target: (String, String),
}

impl ChainHandle {
    pub fn target(&self) -> (String, String) {
        self.target.clone()
    }

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
    recorder: Arc<Recorder>,
    ltp: Arc<LtpStore>,
    symbol: String,
    expiry: ExpiryItem,
    index_names: Vec<String>,
) -> anyhow::Result<ChainHandle> {
    let date = NaiveDate::parse_from_str(&expiry.date, "%Y-%m-%d")?;
    let target = (symbol.clone(), expiry.date.clone());
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
        let mut engine = ChainEngine::new(symbol.clone(), expiry.label.clone(), date, cfg.risk_free_rate);
        let mut ticker = tokio::time::interval(Duration::from_millis(SNAPSHOT_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let started = tokio::time::Instant::now();
        // Something changed since the last snapshot (ticks, or a new index price).
        let mut dirty = false;
        let mut last_spot_px = 0.0_f64;
        // Strikes near ATM whose ticks are stored for charts.
        let mut window: Option<(f64, f64)> = None;
        let each_side = cfg.charts.persist_strikes_each_side;
        loop {
            tokio::select! {
                Some(t) = rx.recv() => {
                    // Every strike feeds the shared price store (positions, paper fills).
                    for (is_call, leg) in [(true, &t.ce), (false, &t.pe)] {
                        if let Some(px) = leg.ltp {
                            ltp.set(&option_key(&symbol, &expiry.date, t.strike, is_call), px);
                        }
                    }
                    if let Some((lo, hi)) = window {
                        if t.strike >= lo - 1e-6 && t.strike <= hi + 1e-6 {
                            let now = market_clock::get_ist_now().timestamp();
                            for (is_call, leg) in [(true, &t.ce), (false, &t.pe)] {
                                if let Some(px) = leg.ltp {
                                    recorder.record(
                                        &option_key(&symbol, &expiry.date, t.strike, is_call),
                                        now, px, leg.volume, leg.oi,
                                    );
                                }
                            }
                        }
                    }
                    engine.apply(t);
                    dirty = true;
                }
                _ = ticker.tick() => {
                    if engine.is_empty() { continue; }
                    let spot = find_index(&store, &index_names);
                    // Hold the table back until the index price is known; ATM and the
                    // strike window depend on it and would otherwise be wrong.
                    // After 30 s without it, show the rows anyway (no ATM highlight) so a
                    // dead index stream never hides the whole chain.
                    if spot.as_ref().map_or(true, |s| s.current_price <= 0.0)
                        && started.elapsed() < Duration::from_secs(30)
                    {
                        continue;
                    }
                    // Send only when something changed: option ticks, or the underlying
                    // moved (it drives ATM, the summary strip and the Greeks).
                    let spot_px = spot.as_ref().map_or(0.0, |s| s.current_price);
                    if !dirty && (spot_px - last_spot_px).abs() < 1e-9 {
                        continue;
                    }
                    dirty = false;
                    last_spot_px = spot_px;
                    window = spot.as_ref().and_then(|s| engine.strike_window(s.current_price, each_side));
                    let vix = find_index(&store, &["INDIA VIX".to_string()]).map(|t| t.current_price);
                    crate::emit_stats::count("chain-snapshot");
                    let _ = app.emit(SNAPSHOT_EVENT, engine.snapshot(spot.as_ref(), vix));
                }
            }
        }
    });

    Ok(ChainHandle { tasks: vec![streamer, engine_task], stream_key: Some(stream_key), target })
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_fyers(
    app: AppHandle,
    cfg: AppConfig,
    creds: FyersCreds,
    status: SharedMarketStatus,
    recorder: Arc<Recorder>,
    ltp: Arc<LtpStore>,
    symbol: String,
    expiry: ExpiryItem,
) -> ChainHandle {
    let target = (symbol.clone(), expiry.date.clone());
    let task = tokio::spawn(async move {
        let poll = Duration::from_secs(cfg.fyers.poll_secs.max(1));
        // Set once the closed-market data has been fetched: no more API calls
        // until Fyers reports the market open.
        let mut fetched_closed = false;
        loop {
            // Subscribe before reading the status, so a change right after is not missed.
            let mut changes = status.subscribe_fyers();
            let polling = changes
                .borrow_and_update()
                .allows_polling(market_clock::is_within_trading_window(market_clock::get_ist_now()));

            if polling || !fetched_closed {
                let mut ok = false;
                match fyers::fetch(&cfg.fyers, &creds, &symbol, cfg.fyers.strike_count, Some(&expiry.value)).await {
                    Ok(data) => {
                        ok = true;
                        let snap = fyers::to_snapshot(&data, &symbol, &expiry, cfg.risk_free_rate);
                        let now = market_clock::get_ist_now().timestamp();
                        for row in &snap.rows {
                            for (is_call, leg) in [(true, &row.ce), (false, &row.pe)] {
                                if let Some(px) = leg.ltp {
                                    let key = option_key(&symbol, &expiry.date, row.strike, is_call);
                                    ltp.set(&key, px);
                                    recorder.record(&key, now, px, leg.volume, leg.oi);
                                }
                            }
                        }
                        let _ = app.emit(SNAPSHOT_EVENT, snap);
                    }
                    Err(e) => {
                        let _ = app.emit(STATUS_EVENT, ChainStatus { state: "error", message: e.to_string() });
                    }
                }
                fetched_closed = !polling && ok;
            }

            if polling {
                tokio::time::sleep(poll).await;
            } else {
                // Closed or pre-open: prices do not move. Wake when Fyers reports a
                // change; re-check hourly, or in a minute after a failed fetch.
                let wait = Duration::from_secs(if fetched_closed { 3600 } else { 60 });
                let _ = tokio::time::timeout(wait, changes.changed()).await;
            }
        }
    });
    ChainHandle { tasks: vec![task], stream_key: None, target }
}
