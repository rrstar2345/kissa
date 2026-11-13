//! Starts/stops the background tasks that feed the option chain view.
//! Each window has its own chain (keyed by window label) and receives only its own events.

use chrono::NaiveDate;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::JoinHandle;

use super::engine::ChainEngine;
use super::fyers::{self, FyersCreds, StreamPlan};
use super::ExpiryItem;
use crate::charts::instrument::option_key;
use crate::charts::recorder::Recorder;
use crate::market::events::{IndexStore, IndexTick, LegTick, OptionTick};
use crate::market::fyers_socket::Hub;
use crate::market::streamers::options;
use crate::market::market_clock::{self, SharedSessionState};
use crate::market::status::{SharedMarketStatus, SourceStatus};
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

    /// Stats key of the NSE stream this handle runs (None for Fyers polling).
    pub fn stream_key(&self) -> Option<&str> {
        self.stream_key.as_deref()
    }

    pub async fn stop(self, stats: &SharedStats, remove_stats: bool) {
        for t in &self.tasks {
            t.abort();
        }
        if remove_stats {
            if let Some(k) = self.stream_key {
                stats.write().await.remove_stream(&k);
            }
        }
    }
}

/// Stops a chain handle that was already taken out of `rest`'s map. Two
/// windows can stream the same contract and share one stats entry, so the
/// entry is removed only when no remaining chain uses it.
pub async fn stop_handle(stats: &SharedStats, rest: &HashMap<String, ChainHandle>, h: ChainHandle) {
    let shared = h.stream_key().is_some_and(|k| rest.values().any(|o| o.stream_key() == Some(k)));
    h.stop(stats, !shared).await;
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
    label: String,
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
                    // Strikes near ATM, plus contracts held in an open position
                    // (pinned), are stored for charts.
                    let in_window = window
                        .map_or(false, |(lo, hi)| t.strike >= lo - 1e-6 && t.strike <= hi + 1e-6);
                    let now = market_clock::get_ist_now().timestamp();
                    for (is_call, leg) in [(true, &t.ce), (false, &t.pe)] {
                        if let Some(px) = leg.ltp {
                            let key = option_key(&symbol, &expiry.date, t.strike, is_call);
                            if in_window || recorder.is_pinned(&key) {
                                recorder.record(&key, now, px, leg.volume, leg.oi);
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
                    let _ = app.emit_to(label.as_str(), SNAPSHOT_EVENT, engine.snapshot(spot.as_ref(), vix));
                }
            }
        }
    });

    Ok(ChainHandle { tasks: vec![streamer, engine_task], stream_key: Some(stream_key), target })
}

/// Fyers chain. While the exchange is open and the data socket works, prices
/// stream over it (`live`); a REST chain call every `fyers.chain_refresh_secs`
/// refreshes the strike list and open interest. Otherwise (socket off, failing,
/// or market closed) the chain is fetched over REST as before.
#[allow(clippy::too_many_arguments)]
pub fn spawn_fyers(
    app: AppHandle,
    label: String,
    cfg: AppConfig,
    creds: FyersCreds,
    status: SharedMarketStatus,
    recorder: Arc<Recorder>,
    ltp: Arc<LtpStore>,
    hub: Arc<Hub>,
    symbol: String,
    expiry: ExpiryItem,
) -> ChainHandle {
    let target = (symbol.clone(), expiry.date.clone());
    let task = tokio::spawn(async move {
        let poll = Duration::from_secs(cfg.fyers.poll_secs.max(1));
        // Set once the closed-market data has been fetched: no more API calls
        // until Fyers reports the market open.
        let mut fetched_closed = false;
        // After a socket failure, REST polling runs until this time.
        let mut socket_retry_at: Option<tokio::time::Instant> = None;
        loop {
            // Subscribe before reading the status, so a change right after is not missed.
            let mut changes = status.subscribe_fyers();
            let polling = open_now(&changes.borrow_and_update());

            let try_socket = cfg.fyers.use_socket
                && polling
                && socket_retry_at.is_none_or(|t| tokio::time::Instant::now() >= t);
            if try_socket {
                let began = tokio::time::Instant::now();
                match live(&app, &label, &cfg, &creds, &status, &recorder, &ltp, &hub, &symbol, &expiry).await {
                    Ok(()) => {
                        // The market closed: one REST fetch below for the closing prices.
                        fetched_closed = false;
                        if began.elapsed() < Duration::from_secs(1) {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("Fyers chain {symbol}: socket unavailable ({e}); polling over REST");
                        socket_retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(60));
                    }
                }
            }

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
                        let _ = app.emit_to(label.as_str(), SNAPSHOT_EVENT, snap);
                    }
                    Err(e) => {
                        let _ = app.emit_to(label.as_str(), STATUS_EVENT, ChainStatus { state: "error", message: e.to_string() });
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

/// Should Fyers prices be fetched/streamed now? (Unknown status uses the clock window.)
fn open_now(st: &SourceStatus) -> bool {
    st.allows_polling(market_clock::is_within_trading_window(market_clock::get_ist_now()))
}

type OiBase = HashMap<String, (Option<f64>, Option<f64>)>;

/// Applies the REST chain to the engine. New contracts get everything; known
/// ones only open interest (prices come from the socket and are fresher).
/// `bases` remembers each contract's (OI, OI change) at this refresh, so live
/// OI changes can be derived as `oich + (oi - oi_at_refresh)`.
fn seed(engine: &mut ChainEngine, bases: &mut OiBase, plan: &StreamPlan) {
    for (topic, c) in &plan.options {
        let leg = if bases.contains_key(topic) {
            LegTick { oi: c.oi, oi_change: c.oich, ..Default::default() }
        } else {
            LegTick {
                ltp: c.ltp,
                change: c.change,
                volume: c.volume,
                oi: c.oi,
                oi_change: c.oich,
                bid: c.bid,
                ask: c.ask,
            }
        };
        bases.insert(topic.clone(), (c.oi, c.oich));
        let (ce, pe) = if c.is_call { (leg, LegTick::default()) } else { (LegTick::default(), leg) };
        engine.apply(OptionTick { strike: c.strike, ce, pe });
    }
}

fn index_tick(name: &str, px: f64, prev: Option<f64>, t: Option<&crate::market::fyers_socket::Tick>) -> IndexTick {
    let change = prev.map_or(0.0, |p| px - p);
    IndexTick {
        time: chrono::Utc::now().to_rfc3339(),
        index_name: name.to_string(),
        current_price: px,
        change,
        per_change: prev.filter(|p| *p > 0.0).map_or(0.0, |p| change / p * 100.0),
        previous_close: prev.unwrap_or(0.0),
        open: t.and_then(|t| t.open).unwrap_or(0.0),
        high: t.and_then(|t| t.high).unwrap_or(0.0),
        low: t.and_then(|t| t.low).unwrap_or(0.0),
        mkt_status: "Open".into(),
    }
}

/// Streams the chain over the data socket until the market closes (`Ok`) or
/// the socket stays down (`Err`, the caller falls back to REST).
#[allow(clippy::too_many_arguments)]
async fn live(
    app: &AppHandle,
    label: &str,
    cfg: &AppConfig,
    creds: &FyersCreds,
    status: &SharedMarketStatus,
    recorder: &Arc<Recorder>,
    ltp: &Arc<LtpStore>,
    hub: &Arc<Hub>,
    symbol: &str,
    expiry: &ExpiryItem,
) -> anyhow::Result<()> {
    let fy = &cfg.fyers;
    let date = NaiveDate::parse_from_str(&expiry.date, "%Y-%m-%d")?;
    let data = fyers::fetch(fy, creds, symbol, fy.strike_count, Some(&expiry.value)).await?;
    let mut plan = fyers::stream_plan(fy, creds, &data).await;
    if plan.options.is_empty() {
        return Err(anyhow::anyhow!("the chain response has no streamable contracts"));
    }

    let mut engine = ChainEngine::new(symbol.to_string(), expiry.label.clone(), date, cfg.risk_free_rate);
    engine.set_source("fyers");
    let mut bases = OiBase::new();
    seed(&mut engine, &mut bases, &plan);
    for c in plan.options.values() {
        if let Some(px) = c.ltp {
            ltp.set(&option_key(symbol, &expiry.date, c.strike, c.is_call), px);
        }
    }

    // Underlying and VIX start from the REST values, then follow the socket.
    let mut spot_prev: Option<f64> = None;
    let mut spot: Option<IndexTick> = plan.underlying.as_ref().and_then(|(_, u)| {
        let px = u.ltp.filter(|p| *p > 0.0)?;
        spot_prev = u.change.map(|c| px - c);
        Some(index_tick(&u.symbol, px, spot_prev, None))
    });
    let mut vix = fyers::vix_ltp(&data);

    // Subscribe to ticks first, so nothing between the two is missed.
    let mut rx = hub.subscribe();
    let lease = hub.lease();
    lease.set(plan.topics());
    let hub_status = hub.status();
    let mut changes = status.subscribe_fyers();
    let _ = changes.borrow_and_update();

    let refresh_every = Duration::from_secs(fy.chain_refresh_secs.max(10));
    let mut refresh = tokio::time::interval_at(tokio::time::Instant::now() + refresh_every, refresh_every);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticker = tokio::time::interval(Duration::from_millis(SNAPSHOT_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let each_side = cfg.charts.persist_strikes_each_side;
    let mut window: Option<(f64, f64)> = None;
    let mut dirty = true;
    let mut last_spot_px = 0.0_f64;
    let mut down_since: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            t = rx.recv() => {
                let t = match t {
                    Ok(t) => t,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(anyhow::anyhow!("socket hub stopped"));
                    }
                };
                if plan.underlying.as_ref().is_some_and(|(u, _)| u.eq_ignore_ascii_case(&t.topic)) {
                    if let Some(px) = t.ltp.filter(|p| *p > 0.0) {
                        spot_prev = t.prev_close.or(spot_prev);
                        let name = plan.underlying.as_ref().map_or("", |(_, u)| u.symbol.as_str());
                        spot = Some(index_tick(name, px, spot_prev, Some(&t)));
                    }
                } else if plan.vix_topic.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(&t.topic)) {
                    if let Some(px) = t.ltp.filter(|p| *p > 0.0) {
                        vix = Some(px);
                        dirty = true;
                    }
                } else if let Some(c) = plan.options.get(&t.topic) {
                    let base = bases.get(&t.topic).copied().unwrap_or((None, None));
                    let oi_change = match (t.oi, base.0, base.1) {
                        (Some(o), Some(b0), Some(ch)) => Some(ch + (o - b0)),
                        _ => None,
                    };
                    let leg = LegTick {
                        ltp: t.ltp,
                        change: t.ltp.zip(t.prev_close).map(|(l, p)| l - p),
                        volume: t.volume,
                        oi: t.oi,
                        oi_change,
                        bid: t.bid,
                        ask: t.ask,
                    };
                    if let Some(px) = t.ltp {
                        let key = option_key(symbol, &expiry.date, c.strike, c.is_call);
                        ltp.set(&key, px);
                        let in_window = window
                            .is_some_and(|(lo, hi)| c.strike >= lo - 1e-6 && c.strike <= hi + 1e-6);
                        if in_window || recorder.is_pinned(&key) {
                            recorder.record(&key, market_clock::get_ist_now().timestamp(), px, t.volume, t.oi);
                        }
                    }
                    let (ce, pe) = if c.is_call { (leg, LegTick::default()) } else { (LegTick::default(), leg) };
                    engine.apply(OptionTick { strike: c.strike, ce, pe });
                    dirty = true;
                }
            }
            _ = ticker.tick() => {
                // The socket must come back within 20 s, or the caller polls over REST.
                let (connected, err) = {
                    let s = hub_status.borrow();
                    (s.connected, s.error.clone())
                };
                if connected {
                    down_since = None;
                } else {
                    let since = *down_since.get_or_insert_with(tokio::time::Instant::now);
                    if since.elapsed() > Duration::from_secs(20) {
                        return Err(anyhow::anyhow!(err.unwrap_or_else(|| "not connected".into())));
                    }
                }
                if engine.is_empty() {
                    continue;
                }
                let spot_px = spot.as_ref().map_or(0.0, |s| s.current_price);
                if !dirty && (spot_px - last_spot_px).abs() < 1e-9 {
                    continue;
                }
                dirty = false;
                last_spot_px = spot_px;
                window = spot.as_ref().and_then(|s| engine.strike_window(s.current_price, each_side));
                crate::emit_stats::count("chain-snapshot");
                let _ = app.emit_to(label, SNAPSHOT_EVENT, engine.snapshot(spot.as_ref(), vix));
            }
            _ = refresh.tick() => {
                match fyers::fetch(fy, creds, symbol, fy.strike_count, Some(&expiry.value)).await {
                    Ok(d) => {
                        let np = fyers::stream_plan(fy, creds, &d).await;
                        if !np.options.is_empty() {
                            plan = np;
                            seed(&mut engine, &mut bases, &plan);
                            lease.set(plan.topics());
                            dirty = true;
                        }
                    }
                    Err(e) => tracing::warn!("Fyers chain {symbol}: refresh failed: {e}"),
                }
            }
            _ = changes.changed() => {
                let ok = open_now(&changes.borrow_and_update());
                if !ok {
                    return Ok(());
                }
            }
        }
    }
}

/// Stops the chain owned by the window with this label, if any.
pub async fn stop_window_chain(state: &crate::state::AppState, label: &str) {
    let mut slot = state.chain.lock().await;
    if let Some(old) = slot.remove(label) {
        stop_handle(&state.stats, &slot, old).await;
    }
}
