//! Background price feed.
//!
//! The option-chain page only streams while it is open. Open positions and
//! pending paper orders still need prices, so this supervisor starts a
//! price-only stream for every (symbol, expiry) that has one, unless the
//! chain page is already streaming it. Streams stop when no longer needed.
//!
//! The feed also records the strikes near the money as chart bars (like the
//! chain page does), so algos can evaluate their signal on an option's own
//! bars without the Option Chain page being open.

use chrono::NaiveDate;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::JoinHandle;
use tracing::debug;

use super::store;
use super::today_ist;
use crate::algo::{atm_strike, strike_step};
use crate::chain::fyers::{self, FyersCreds};
use crate::chain::runner::find_index;
use crate::charts::recorder::Recorder;
use crate::chain::ExpiryItem;
use crate::charts::instrument::option_key;
use crate::db;
use crate::market::events::{IndexStore, OptionTick};
use crate::market::market_clock;
use crate::market::streamers::options;
use crate::market::symbols;
use crate::state::SharedState;

/// (symbol, expiry ISO date)
type Target = (String, String);

struct Feed {
    tasks: Vec<JoinHandle<()>>,
    stats_key: Option<String>,
}

pub fn spawn(state: SharedState) {
    tauri::async_runtime::spawn(async move {
        let mut feeds: HashMap<Target, Feed> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let needed = needed_targets(&state).await;
            let covered: Option<Target> = state.chain.lock().await.as_ref().map(|h| h.target());

            let stale: Vec<Target> = feeds
                .keys()
                .filter(|k| !needed.contains(*k) || covered.as_ref() == Some(*k))
                .cloned()
                .collect();
            for k in stale {
                if let Some(f) = feeds.remove(&k) {
                    // The chain page may own the same stats entry; leave it alone then.
                    stop(f, &state, covered.as_ref() != Some(&k)).await;
                }
            }
            for t in needed {
                if covered.as_ref() == Some(&t) || feeds.contains_key(&t) {
                    continue;
                }
                if let Some(f) = start(&state, &t).await {
                    feeds.insert(t, f);
                }
            }
        }
    });
}

async fn needed_targets(state: &SharedState) -> HashSet<Target> {
    let mut set = state.trading.watched_contracts();
    if let Ok(positions) = store::open_positions(&state.db, None, &today_ist()).await {
        for p in positions {
            set.insert((p.symbol, p.expiry));
        }
    }
    set
}

/// Stores option ticks near the money as chart bars: the strikes within
/// ATM +- `charts.persist_strikes_each_side` steps of the underlying's spot.
struct Recording {
    recorder: Arc<Recorder>,
    indices: IndexStore,
    index_names: Vec<String>,
    step: f64,
    each_side: f64,
}

impl Recording {
    async fn new(state: &SharedState, symbol: &str) -> Self {
        let mut index_names: Vec<String> = state
            .fno
            .read()
            .await
            .iter()
            .find(|i| i.symbol == symbol)
            .map(|i| i.index_names.clone())
            .unwrap_or_default();
        index_names.extend(symbols::builtin_index_names(symbol).iter().map(|s| s.to_string()));
        index_names.push(symbol.to_string());
        Self {
            recorder: state.recorder.clone(),
            indices: state.indices.clone(),
            index_names,
            step: strike_step(symbol),
            each_side: state.config.charts.persist_strikes_each_side as f64,
        }
    }

    /// The nearest strike to the underlying's current price (None until it is known).
    fn atm(&self) -> Option<f64> {
        let spot = find_index(&self.indices, &self.index_names)?.current_price;
        (spot > 0.0).then(|| atm_strike(spot, self.step))
    }

    fn in_window(&self, atm: f64, strike: f64) -> bool {
        (strike - atm).abs() <= self.each_side * self.step + 1e-6
    }
}

async fn stop(feed: Feed, state: &SharedState, remove_stat: bool) {
    for t in &feed.tasks {
        t.abort();
    }
    if remove_stat {
        if let Some(k) = feed.stats_key {
            state.stats.write().await.remove_stream(&k);
        }
    }
}

async fn start(state: &SharedState, target: &Target) -> Option<Feed> {
    let (symbol, expiry) = target.clone();
    let date = NaiveDate::parse_from_str(&expiry, "%Y-%m-%d").ok()?;

    let source = db::get_setting(&state.db, "data.source").await.ok().flatten();
    if source.as_deref() == Some("fyers") {
        if let Ok(creds) = crate::commands::active_fyers_creds(state).await {
            let task = tokio::spawn(fyers_loop(state.clone(), creds, symbol, expiry));
            return Some(Feed { tasks: vec![task], stats_key: None });
        }
    }

    // NSE: NSE expiry labels look like 24-Nov-2026.
    let label = date.format("%d-%b-%Y").to_string();
    let stats_key = format!("options:{symbol}:{label}");
    let (tx, mut rx) = unbounded_channel::<OptionTick>();
    let streamer = tokio::spawn(options::run(
        state.config.clone(),
        symbol.clone(),
        label,
        tx,
        state.stats.clone(),
        state.session.clone(),
    ));
    let ltp = state.ltp.clone();
    let rec = Recording::new(state, &symbol).await;
    let reader = tokio::spawn(async move {
        while let Some(t) = rx.recv().await {
            let window = rec.atm().map(|atm| rec.in_window(atm, t.strike)).unwrap_or(false);
            let now = market_clock::get_ist_now().timestamp();
            for (is_call, leg) in [(true, &t.ce), (false, &t.pe)] {
                if let Some(px) = leg.ltp {
                    let key = option_key(&symbol, &expiry, t.strike, is_call);
                    ltp.set(&key, px);
                    if window {
                        rec.recorder.record(&key, now, px, leg.volume, leg.oi);
                    }
                }
            }
        }
    });
    Some(Feed { tasks: vec![streamer, reader], stats_key: Some(stats_key) })
}

async fn fyers_loop(state: SharedState, creds: FyersCreds, symbol: String, expiry_iso: String) {
    let cfg = state.config.clone();
    let rec = Recording::new(&state, &symbol).await;
    let mut epoch: Option<String> = None;
    // Set once the closed-market prices have been fetched: no more API calls
    // until Fyers reports the market open.
    let mut fetched_closed = false;
    loop {
        // Subscribe before reading the status, so a change right after is not missed.
        let mut changes = state.market_status.subscribe_fyers();
        let polling = changes
            .borrow_and_update()
            .allows_polling(market_clock::is_within_trading_window(market_clock::get_ist_now()));

        if polling || !fetched_closed {
            let mut ok = false;
            if epoch.is_none() {
                match fyers::fetch(&cfg.fyers, &creds, &symbol, 1, None).await {
                    Ok(d) => {
                        epoch = fyers::expiries(&d).into_iter().find(|e| e.date == expiry_iso).map(|e| e.value);
                    }
                    Err(e) => debug!("Price feed: expiry lookup failed: {e}"),
                }
            }
            if let Some(ep) = &epoch {
                match fyers::fetch(&cfg.fyers, &creds, &symbol, cfg.fyers.strike_count, Some(ep.as_str())).await {
                    Ok(data) => {
                        ok = true;
                        let item = ExpiryItem { value: ep.clone(), label: String::new(), date: expiry_iso.clone() };
                        let snap = fyers::to_snapshot(&data, &symbol, &item, cfg.risk_free_rate);
                        let now = market_clock::get_ist_now().timestamp();
                        for row in &snap.rows {
                            for (is_call, leg) in [(true, &row.ce), (false, &row.pe)] {
                                if let Some(px) = leg.ltp {
                                    let key = option_key(&symbol, &expiry_iso, row.strike, is_call);
                                    state.ltp.set(&key, px);
                                    rec.recorder.record(&key, now, px, leg.volume, leg.oi);
                                }
                            }
                        }
                    }
                    Err(e) => debug!("Price feed: Fyers fetch failed: {e}"),
                }
            }
            fetched_closed = !polling && ok;
        }

        if polling {
            tokio::time::sleep(Duration::from_secs(cfg.fyers.poll_secs.max(5))).await;
        } else {
            // Closed or pre-open: prices do not move. Wake when Fyers reports a
            // change; re-check hourly, or in 5 minutes after a failed fetch.
            let wait = Duration::from_secs(if fetched_closed { 3600 } else { 300 });
            let _ = tokio::time::timeout(wait, changes.changed()).await;
        }
    }
}
