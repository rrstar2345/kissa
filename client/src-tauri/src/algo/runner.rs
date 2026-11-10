//! The algo loop. Every 5 s, for every armed algo (and every disarmed one that
//! still holds a position):
//!
//! 1. Disarm when the kill switch or a daily-loss halt applies to its accounts.
//! 2. Keep the position in step with the ledger: when every leg is closed
//!    (exit order, stop-loss, target, or an entry that never filled) the algo
//!    is flat again and the next scan starts from the then-current spot.
//! 3. Watch the stop-loss / target of legs whose broker has no bracket orders.
//! 4. On a *new closed bar* of the underlying: cancel unfilled entry orders,
//!    then `eval::decide` (flat -> Entry, in a position -> Exit). Entry places
//!    BUY limit orders; Exit closes the legs.
//!
//! The first look after arming only sets the baseline bar, so a signal that
//! was already on the chart never trades. Every decision goes to the journal;
//! entries, exits, failures and disarming also go to the alert channels.
//! The app must be running for any of this (there is no headless mode).

use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tracing::warn;

use super::{
    atm_strike, bracket_levels, closed_count, entry_limit, interval_secs, lots_for_capital, pick_expiry,
    select_accounts, strike_step, target_strike, Algo, AlgoState, ExpirySel, Leg, BAR_LIMIT, PHASE_IN,
};
use super::store;
use crate::accounts;
use crate::alerts::{Alert, Kind};
use crate::charts::instrument::{index_key, option_key};
use crate::charts::resample::{self, Interval};
use crate::charts::rules::eval::{self, Decision};
use crate::charts::rules::{self, Ruleset};
use crate::charts::{store as chart_store, Bar};
use crate::market::{market_clock, symbols};
use crate::state::SharedState;
use crate::trading::copier;
use crate::trading::{is_open_status, store as trade_store, today_ist, BracketInput, PlaceOrderInput};

const TICK_SECS: u64 = 5;
/// After a failed expiry lookup, wait this long before asking again.
const EXPIRY_RETRY: Duration = Duration::from_secs(60);
/// A stop-loss / target exit is not repeated within this time.
const STOP_REFIRE: Duration = Duration::from_secs(30);
/// An option price older than this is not used for an entry.
const MAX_PRICE_AGE_SECS: i64 = 120;
const MAX_LOTS: i64 = 10_000;

struct Runner {
    state: SharedState,
    app: AppHandle,
    /// Upcoming expiries per underlying: (IST day fetched, ISO dates).
    expiries: HashMap<String, (String, Vec<String>)>,
    expiry_failed: HashMap<String, Instant>,
    /// Last error written per algo, so a repeating one is journaled once.
    last_error: HashMap<String, String>,
    /// Entry order id -> when its stop-loss / target exit was last sent.
    stop_fired: HashMap<String, Instant>,
    ticks: u64,
}

pub fn spawn(state: SharedState, app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut runner = Runner {
            state,
            app,
            expiries: HashMap::new(),
            expiry_failed: HashMap::new(),
            last_error: HashMap::new(),
            stop_fired: HashMap::new(),
            ticks: 0,
        };
        let mut tick = tokio::time::interval(Duration::from_secs(TICK_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            runner.tick().await;
        }
    });
}

fn expiry_label(sel: ExpirySel) -> &'static str {
    match sel {
        ExpirySel::Current => "current",
        ExpirySel::Next => "next",
        ExpirySel::Next1 => "next +1",
        ExpirySel::Next2 => "next +2",
        ExpirySel::Next3 => "next +3",
    }
}

/// `ATM`, `ATM+2`, `ATM-1`
fn offset_label(offset: i32) -> String {
    match offset {
        0 => "ATM".into(),
        n if n > 0 => format!("ATM+{n}"),
        n => format!("ATM{n}"),
    }
}

/// `HH:MM` IST of a bar start.
fn hhmm(ts: i64) -> String {
    DateTime::from_timestamp(ts + 5 * 3600 + 30 * 60, 0)
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn leg_label(l: &Leg) -> String {
    format!("{} {:.0} {} {}", l.symbol, l.strike, if l.is_call { "CE" } else { "PE" }, l.expiry)
}

impl Runner {
    async fn tick(&mut self) {
        self.ticks += 1;
        let pool = self.state.db.clone();
        let (algos, states, signals) =
            match (store::list_algos(&pool).await, store::list_states(&pool).await, rules::store::list(&pool).await) {
                (Ok(a), Ok(s), Ok(r)) => (a, s, r),
                _ => return,
            };
        let mut watch: HashSet<(String, String)> = HashSet::new();
        for algo in &algos {
            let st = states.get(&algo.id).cloned().unwrap_or_else(|| AlgoState::fresh(&algo.id));
            if !st.armed && !st.in_position() {
                self.last_error.remove(&algo.id);
                continue;
            }
            match self.process(algo, st, &signals, &mut watch).await {
                Ok(()) => {
                    self.last_error.remove(&algo.id);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if self.last_error.get(&algo.id) != Some(&msg) {
                        self.last_error.insert(algo.id.clone(), msg.clone());
                        self.log(algo, "error", "error", &msg).await;
                    }
                }
            }
        }
        self.state.trading.set_algo_watch(watch);
        // Hourly housekeeping.
        if self.ticks % 720 == 1 {
            if let Err(e) = store::journal_purge(&pool).await {
                warn!("Algo journal purge failed: {e}");
            }
        }
    }

    // ------------------------------------------------------------ helpers ---

    async fn log(&self, algo: &Algo, level: &str, kind: &str, msg: &str) {
        store::journal_add(&self.state.db, &algo.id, &algo.name, level, kind, msg).await;
        let _ = self.app.emit("algo-update", true);
    }

    fn notify(&self, key: String, title: String, body: String) {
        self.state.alerts.notify(Alert::new(Kind::Algo, key, title, body));
    }

    async fn acct_name(&self, id: &str) -> String {
        accounts::get(&self.state.db, id).await.map(|a| a.name).unwrap_or_else(|_| id.to_string())
    }

    /// Why the algo must stop trading now: the kill switch or a daily-loss halt on one of its accounts.
    async fn halted(&self, algo: &Algo) -> Option<String> {
        for id in &algo.accounts {
            if let Some(reason) = self.state.trading.halt_reason(id).await {
                return Some(format!("{}: {reason}", self.acct_name(id).await));
            }
        }
        None
    }

    /// Upcoming expiries (ISO) of an underlying: the local Dhan list when it
    /// has enough, merged with NSE's. Cached per IST day.
    async fn expiries(&mut self, symbol: &str) -> Result<Vec<String>> {
        let today = today_ist();
        if let Some((day, list)) = self.expiries.get(symbol) {
            if *day == today && !list.is_empty() {
                return Ok(list.clone());
            }
        }
        if let Some(at) = self.expiry_failed.get(symbol) {
            if at.elapsed() < EXPIRY_RETRY {
                return Err(anyhow!("The expiry list for {symbol} is not available yet (retrying shortly)"));
            }
        }
        let mut list = store::scrip_expiries(&self.state.db, symbol, &today).await.unwrap_or_default();
        if list.len() < 5 {
            match symbols::fetch_expiries(&self.state.config, symbol).await {
                Ok(labels) => {
                    for l in labels {
                        if let Ok(d) = NaiveDate::parse_from_str(&l, "%d-%b-%Y") {
                            list.push(d.format("%Y-%m-%d").to_string());
                        }
                    }
                }
                Err(e) => {
                    if list.is_empty() {
                        self.expiry_failed.insert(symbol.to_string(), Instant::now());
                        return Err(anyhow!("Could not load the expiry list for {symbol}: {e}"));
                    }
                }
            }
        }
        list.retain(|d| d.as_str() >= today.as_str());
        list.sort();
        list.dedup();
        if list.is_empty() {
            self.expiry_failed.insert(symbol.to_string(), Instant::now());
            return Err(anyhow!("No upcoming expiry is listed for {symbol}"));
        }
        self.expiry_failed.remove(symbol);
        self.expiries.insert(symbol.to_string(), (today, list.clone()));
        Ok(list)
    }

    async fn expiry_for(&mut self, algo: &Algo) -> Result<String> {
        let list = self.expiries(&algo.instrument.underlying).await?;
        pick_expiry(algo.instrument.expiry, &list, &today_ist()).ok_or_else(|| {
            anyhow!(
                "The {} expiry of {} is not listed ({} upcoming expiries known)",
                expiry_label(algo.instrument.expiry),
                algo.instrument.underlying,
                list.len()
            )
        })
    }

    /// Bars of the underlying's index at the algo's interval.
    async fn underlying_bars(&self, algo: &Algo, iv: Interval) -> Result<Vec<Bar>> {
        let symbol = &algo.instrument.underlying;
        let mut names: Vec<String> = Vec::new();
        {
            let fno = self.state.fno.read().await;
            if let Some(i) = fno.iter().find(|i| &i.symbol == symbol) {
                names.extend(i.index_names.iter().cloned());
            }
        }
        names.extend(symbols::builtin_index_names(symbol).iter().map(|s| s.to_string()));
        names.push(symbol.clone());
        let mut seen = HashSet::new();
        names.retain(|n| seen.insert(n.clone()));
        for n in names {
            let bars = chart_store::chart_bars(&self.state.db, &index_key(&n), iv, BAR_LIMIT).await?;
            if !bars.is_empty() {
                return Ok(bars);
            }
        }
        Err(anyhow!(
            "No {symbol} bars are stored yet. The index stream records them during market hours."
        ))
    }

    // ------------------------------------------------------------ process ---

    async fn process(
        &mut self,
        algo: &Algo,
        mut st: AlgoState,
        signals: &[Ruleset],
        watch: &mut HashSet<(String, String)>,
    ) -> Result<()> {
        let pool = self.state.db.clone();

        // 1. Safety: the kill switch / a loss halt disarms the algo.
        if st.armed {
            if let Some(reason) = self.halted(algo).await {
                store::set_armed(&pool, &algo.id, false).await?;
                st.armed = false;
                self.log(algo, "warn", "disarmed", &format!("Disarmed automatically: {reason}. Arm it again when trading may resume."))
                    .await;
                self.notify(
                    format!("{}:disarm:{}", algo.id, today_ist()),
                    format!("Algo disarmed: {}", algo.name),
                    format!("{reason}. An open position (if any) is still managed. Arm the algo again when trading may resume."),
                );
            }
        }

        // The price feed must keep the contracts of this algo live.
        if st.armed {
            if let Ok(expiry) = self.expiry_for(algo).await {
                watch.insert((algo.instrument.underlying.clone(), expiry));
            }
        }
        for l in &st.legs {
            watch.insert((l.symbol.clone(), l.expiry.clone()));
        }

        // 2 + 3. Position bookkeeping and software stop-loss / target.
        if st.in_position() {
            if self.sync_position(algo, &st).await? {
                return Ok(());
            }
            self.watch_stops(algo, &st).await;
        }

        // 4. The signal, on a new closed bar of the underlying.
        let rs = signals
            .iter()
            .find(|r| r.id == algo.signal_id)
            .ok_or_else(|| anyhow!("The signal this algo uses no longer exists"))?;
        let iv = resample::parse_interval(&algo.interval).ok_or_else(|| anyhow!("Invalid interval {}", algo.interval))?;
        let secs = interval_secs(&algo.interval).ok_or_else(|| anyhow!("Invalid interval {}", algo.interval))?;
        self.state.recorder.flush_now().await;
        let bars = self.underlying_bars(algo, iv).await?;
        let closed = closed_count(&bars, secs, Utc::now().timestamp());
        if closed == 0 {
            return Ok(());
        }
        let bar_ts = bars[closed - 1].ts;
        if st.last_bar_ts == 0 {
            store::set_last_bar(&pool, &algo.id, bar_ts).await?;
            return Ok(());
        }
        if bar_ts <= st.last_bar_ts {
            return Ok(());
        }
        // Consume the bar first: a failure below is journaled once, not retried every tick.
        store::set_last_bar(&pool, &algo.id, bar_ts).await?;

        let in_pos = st.in_position();
        let decision = eval::decide(rs, &bars, closed, in_pos).map_err(|e| anyhow!("Signal error: {e}"))?;
        if in_pos {
            self.cancel_unfilled(algo, &st).await;
            if decision == Some(Decision::Exit) {
                self.log(algo, "info", "signal", &format!("Exit signal on the {} bar of {} ({}).", algo.interval, hhmm(bar_ts), rs.name))
                    .await;
                self.close_legs(algo, &st, "exit signal").await;
            }
        } else if st.armed && decision == Some(Decision::Entry) {
            let spot = bars[closed - 1].close;
            self.enter(algo, spot, bar_ts, &rs.name).await?;
        }
        Ok(())
    }

    // -------------------------------------------------------------- entry ---

    async fn enter(&mut self, algo: &Algo, spot: f64, bar_ts: i64, signal_name: &str) -> Result<()> {
        let pool = self.state.db.clone();
        let symbol = algo.instrument.underlying.clone();
        let expiry = self.expiry_for(algo).await?;
        let step = strike_step(&symbol);
        let strike = target_strike(spot, step, algo.instrument.strike_offset);
        if strike <= 0.0 {
            return Err(anyhow!("Could not work out a strike from the spot {spot:.2}"));
        }
        self.log(
            algo,
            "info",
            "signal",
            &format!(
                "Entry signal on the {} bar of {} ({signal_name}): spot {spot:.2}, ATM {:.0}, {} = {strike:.0}, expiry {expiry}.",
                algo.interval,
                hhmm(bar_ts),
                atm_strike(spot, step),
                offset_label(algo.instrument.strike_offset),
            ),
        )
        .await;

        // Which accounts place orders (trade-copier override).
        let cfg = copier::load_config(&pool).await.unwrap_or_default();
        let sel = select_accounts(&algo.accounts, algo.copier, &cfg);
        for (child, master) in &sel.covered {
            let (c, m) = (self.acct_name(child).await, self.acct_name(master).await);
            self.log(algo, "info", "skip", &format!("{c} is not traded directly: its master {m} places the orders and the trade copier replicates them.")).await;
        }

        let sides = algo.instrument.side.legs();
        let per_leg = algo.action.capital / sides.len() as f64;
        let mut legs: Vec<Leg> = Vec::new();
        let mut summary: Vec<String> = Vec::new();
        let mut failures = 0usize;

        for is_call in sides {
            let ce_pe = if is_call { "CE" } else { "PE" };
            let key = option_key(&symbol, &expiry, strike, is_call);
            let Some(q) = self.state.ltp.get(&key) else {
                self.log(algo, "warn", "skip", &format!("No live price for {symbol} {strike:.0} {ce_pe} {expiry}: the option price feed is not running for it yet. Leg skipped.")).await;
                failures += 1;
                continue;
            };
            let age = market_clock::get_ist_now().timestamp() - q.at;
            if age > MAX_PRICE_AGE_SECS {
                self.log(algo, "warn", "skip", &format!("The last price of {symbol} {strike:.0} {ce_pe} is {age}s old. Leg skipped.")).await;
                failures += 1;
                continue;
            }
            let lot_size = match self.state.trading.contract_info(&symbol, &expiry, strike, is_call).await {
                Ok(i) => i.lot_size,
                Err(e) => {
                    self.log(algo, "error", "error", &format!("Contract lookup failed for {symbol} {strike:.0} {ce_pe}: {e}")).await;
                    failures += 1;
                    continue;
                }
            };
            let Some(lot_size) = lot_size else {
                self.log(algo, "error", "error", &format!("Lot size of {symbol} is unknown. Refresh the instrument list or set trading.fallback_lot_sizes in settings.json.")).await;
                failures += 1;
                continue;
            };
            let price = entry_limit(q.ltp, algo.action.buffer_ticks);
            let lots = lots_for_capital(per_leg, price, lot_size).min(MAX_LOTS);
            if lots < 1 {
                self.log(algo, "warn", "skip", &format!("Capital ₹{per_leg:.0} per leg is below the cost of one lot of {symbol} {strike:.0} {ce_pe} (₹{:.0} = {price:.2} x {lot_size}). Leg skipped.", price * lot_size as f64)).await;
                failures += 1;
                continue;
            }
            let (stop, target) = bracket_levels(price, algo.action.stop_loss_pct, algo.action.target_pct);

            for acct_id in &sel.run {
                let acct = match accounts::get(&pool, acct_id).await {
                    Ok(a) => a,
                    Err(_) => {
                        self.log(algo, "warn", "skip", "An account of this algo no longer exists. Skipped.").await;
                        continue;
                    }
                };
                if acct.kind != "paper" && acct.kind != "dhan" {
                    continue;
                }
                if acct.kind == "dhan" && !algo.allow_live {
                    self.log(algo, "warn", "skip", &format!("{} is a live account and this algo is not allowed to trade live. Skipped.", acct.name)).await;
                    continue;
                }
                if let Some(reason) = self.state.trading.halt_reason(acct_id).await {
                    self.log(algo, "warn", "skip", &format!("{} is halted ({reason}). Skipped.", acct.name)).await;
                    continue;
                }
                let paper = acct.kind == "paper";
                let bracket = (paper && (stop.is_some() || target.is_some()))
                    .then(|| BracketInput { target, stop_trigger: stop, stop_price: None });
                let input = PlaceOrderInput {
                    account_id: acct_id.clone(),
                    symbol: symbol.clone(),
                    expiry: expiry.clone(),
                    strike,
                    is_call,
                    side: "BUY".into(),
                    product: algo.action.product.clone(),
                    order_type: "LIMIT".into(),
                    validity: "DAY".into(),
                    lots,
                    price,
                    trigger_price: None,
                    bracket: bracket.clone(),
                };
                match self.state.trading.place_algo(input, algo.copier).await {
                    Ok(o) if o.status == "REJECTED" => {
                        failures += 1;
                        self.log(algo, "error", "entry", &format!("BUY {lots} lot(s) {symbol} {strike:.0} {ce_pe} @ {price:.2} on {} was rejected: {}", acct.name, o.message)).await;
                    }
                    Ok(o) => {
                        self.log(algo, "info", "entry", &format!("BUY {lots} lot(s) {symbol} {strike:.0} {ce_pe} {expiry} @ {price:.2} on {} ({}){}.", acct.name, o.status, sl_tp_note(paper, stop, target))).await;
                        summary.push(format!("{} BUY {lots} x {symbol} {strike:.0} {ce_pe} @ {price:.2}", acct.name));
                        legs.push(Leg {
                            account_id: acct_id.clone(),
                            instrument: key.clone(),
                            symbol: symbol.clone(),
                            expiry: expiry.clone(),
                            strike,
                            is_call,
                            product: algo.action.product.clone(),
                            lots,
                            order_id: o.id,
                            entry_price: price,
                            stop: if paper { None } else { stop },
                            target: if paper { None } else { target },
                            bracket: bracket.is_some(),
                            propagate: algo.copier,
                        });
                    }
                    Err(e) => {
                        failures += 1;
                        self.log(algo, "error", "error", &format!("Could not place BUY {lots} lot(s) {symbol} {strike:.0} {ce_pe} on {}: {e}", acct.name)).await;
                    }
                }
            }
        }

        if legs.is_empty() {
            self.notify(
                format!("{}:entry-failed:{bar_ts}", algo.id),
                format!("Algo could not enter: {}", algo.name),
                format!("The entry signal fired but no order was placed ({failures} problem(s)). See the algo's journal."),
            );
            return Ok(());
        }
        store::set_position(&pool, &algo.id, PHASE_IN, &legs, bar_ts).await?;
        self.notify(
            format!("{}:entry:{bar_ts}", algo.id),
            format!("Algo entered: {}", algo.name),
            format!("{}\n{}", summary.join("\n"), if failures > 0 { format!("{failures} leg(s)/account(s) skipped or failed.") } else { String::new() }),
        );
        Ok(())
    }

    // ----------------------------------------------------------- position ---

    /// True when the algo has just gone flat (every leg closed or never filled).
    async fn sync_position(&self, algo: &Algo, st: &AlgoState) -> Result<bool> {
        let pool = &self.state.db;
        let today = today_ist();
        let mut held_by_account: HashMap<String, Vec<crate::trading::store::OpenPos>> = HashMap::new();
        let mut alive = false;
        let mut notes: Vec<String> = Vec::new();
        for leg in &st.legs {
            if !held_by_account.contains_key(&leg.account_id) {
                let pos = trade_store::open_positions(pool, Some(&leg.account_id), &today).await?;
                held_by_account.insert(leg.account_id.clone(), pos);
            }
            let held = held_by_account[&leg.account_id]
                .iter()
                .any(|p| p.instrument == leg.instrument && p.product == leg.product && p.net_qty != 0);
            let order = trade_store::get_order(pool, &leg.order_id).await?;
            let open = order.as_ref().map(|o| is_open_status(&o.status)).unwrap_or(false);
            if held || open {
                alive = true;
            } else if let Some(o) = order {
                if o.filled_qty == 0 {
                    notes.push(format!("{}: the entry order was {} and nothing was bought", leg_label(leg), o.status));
                }
            }
        }
        if alive {
            return Ok(false);
        }
        store::set_flat(pool, &algo.id).await?;
        let detail = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join("; ")) };
        self.log(algo, "info", "exit", &format!("Position closed{detail}. Scanning for the next entry from the next bar.")).await;
        if notes.is_empty() {
            self.notify(
                format!("{}:closed:{}", algo.id, st.entry_bar_ts),
                format!("Algo position closed: {}", algo.name),
                "Every leg is closed (exit signal, stop-loss or target). See Orders / Trades for the P&L.".into(),
            );
        }
        Ok(true)
    }

    /// Stop-loss / target of legs the broker does not hold (live accounts).
    async fn watch_stops(&mut self, algo: &Algo, st: &AlgoState) {
        for leg in st.legs.iter().filter(|l| !l.bracket && (l.stop.is_some() || l.target.is_some())) {
            if let Some(at) = self.stop_fired.get(&leg.order_id) {
                if at.elapsed() < STOP_REFIRE {
                    continue;
                }
            }
            let Some(ltp) = self.state.ltp.ltp(&leg.instrument) else { continue };
            let reason = match (leg.stop, leg.target) {
                (Some(sl), _) if ltp <= sl => format!("stop-loss {sl:.2} (price {ltp:.2})"),
                (_, Some(t)) if ltp >= t => format!("target {t:.2} (price {ltp:.2})"),
                _ => continue,
            };
            self.stop_fired.insert(leg.order_id.clone(), Instant::now());
            self.close_leg(algo, leg, &reason).await;
        }
    }

    /// Cancel entry orders that are still unfilled when the next bar closes
    /// (a stale limit order must not fill later at a price the signal no longer wants).
    async fn cancel_unfilled(&self, algo: &Algo, st: &AlgoState) {
        for leg in &st.legs {
            let Ok(Some(o)) = trade_store::get_order(&self.state.db, &leg.order_id).await else { continue };
            if is_open_status(&o.status) && o.filled_qty == 0 {
                match self.state.trading.cancel(&o.id).await {
                    Ok(_) => self.log(algo, "info", "info", &format!("Cancelled the unfilled entry order for {} at the next bar.", leg_label(leg))).await,
                    Err(e) => self.log(algo, "warn", "error", &format!("Could not cancel the unfilled entry order for {}: {e}", leg_label(leg))).await,
                }
            }
        }
    }

    async fn close_legs(&mut self, algo: &Algo, st: &AlgoState, reason: &str) {
        let mut sent = 0;
        for leg in &st.legs {
            if self.close_leg(algo, leg, reason).await {
                sent += 1;
            }
        }
        if sent > 0 {
            self.notify(
                format!("{}:exit:{}", algo.id, Utc::now().timestamp() / 60),
                format!("Algo exiting: {}", algo.name),
                format!("{sent} exit order(s) placed ({reason})."),
            );
        }
    }

    /// Close one leg: cancel an unfilled entry, then exit what is held.
    /// Returns true when an exit order was placed.
    async fn close_leg(&self, algo: &Algo, leg: &Leg, reason: &str) -> bool {
        if let Ok(Some(o)) = trade_store::get_order(&self.state.db, &leg.order_id).await {
            if is_open_status(&o.status) && o.filled_qty == 0 {
                let _ = self.state.trading.cancel(&o.id).await;
            }
        }
        let name = self.acct_name(&leg.account_id).await;
        match self.state.trading.algo_close(&leg.account_id, &leg.instrument, &leg.product, leg.propagate).await {
            Ok(o) => {
                self.log(algo, "info", "exit", &format!("Exit ({reason}): SELL {} lot(s) {} @ {:.2} on {name} ({}).", leg.lots, leg_label(leg), o.price, o.status)).await;
                true
            }
            Err(e) => {
                let m = e.to_string();
                if !(m.contains("no open position") || m.contains("already closed")) {
                    self.log(algo, "error", "error", &format!("Could not exit {} on {name} ({reason}): {m}", leg_label(leg))).await;
                    self.notify(
                        format!("{}:exit-failed:{}:{}", algo.id, leg.order_id, Utc::now().timestamp() / 60),
                        format!("Algo could not exit: {}", algo.name),
                        format!("{} on {name}: {m}. Close it manually if needed.", leg_label(leg)),
                    );
                }
                false
            }
        }
    }
}

fn sl_tp_note(paper: bool, stop: Option<f64>, target: Option<f64>) -> String {
    let mut parts = Vec::new();
    if let Some(s) = stop {
        parts.push(format!("stop-loss {s:.2}"));
    }
    if let Some(t) = target {
        parts.push(format!("target {t:.2}"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(", {} ({})", parts.join(", "), if paper { "bracket orders" } else { "watched by the algo" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(offset_label(0), "ATM");
        assert_eq!(offset_label(2), "ATM+2");
        assert_eq!(offset_label(-1), "ATM-1");
        assert_eq!(expiry_label(ExpirySel::Next2), "next +2");
        // 2026-11-09 04:00 UTC = 09:30 IST
        assert_eq!(hhmm(1_794_196_800), "09:30");
    }

    #[test]
    fn sl_tp_note_describes_who_holds_the_exits() {
        assert_eq!(sl_tp_note(true, None, None), "");
        assert_eq!(sl_tp_note(true, Some(80.0), Some(130.0)), ", stop-loss 80.00, target 130.00 (bracket orders)");
        assert_eq!(sl_tp_note(false, Some(80.0), None), ", stop-loss 80.00 (watched by the algo)");
    }
}
