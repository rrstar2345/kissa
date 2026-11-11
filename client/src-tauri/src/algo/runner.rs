//! The algo loop. Every 5 s, for every armed algo (and every disarmed one that
//! still holds a position):
//!
//! 1. Disarm when the kill switch or a daily-loss halt applies to its accounts.
//! 2. Keep the legs in step with the ledger: a leg whose position is closed
//!    (exit order, stop-loss, target, or an entry that never filled) is
//!    dropped; the algo is flat when no leg is left.
//! 3. Watch the stop-loss / target of legs whose broker has no bracket orders.
//! 4. Once per *newly closed bar* (the algo interval, on the session grid),
//!    for each leg in the order CE, PE: a held leg cancels its unfilled entry
//!    and checks the signal's Exit on its own option bars; a flat leg (armed
//!    algos only) resolves ATM + offset from the underlying's spot and checks
//!    the signal's Entry on that strike's bars. Entries are BUY limit orders
//!    sized from the account's free capital (pool minus open legs).
//!
//! A bar counts only if it exists for the closed interval: an option that did
//! not trade in it has no bar and no signal. Option bars are recorded by the
//! price feed (`trading::feed`) or the Option Chain page, and a signal needs
//! `warmup_bars` of history before it can fire.
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
    atm_strike, bracket_levels, closed_count, entry_limit, free_capital, interval_secs, last_closed_bucket,
    lots_for_capital, pick_expiry, select_accounts, strike_step, target_strike, warmup_bars, Algo, AlgoState,
    ExpirySel, Leg, BAR_LIMIT, PHASE_FLAT, PHASE_IN,
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
    /// `algo:option` pairs already told they are waiting for bar history.
    warned: HashSet<String>,
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
            warned: HashSet::new(),
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

        // 2 + 3. Keep the legs in step with the ledger; watch software stop-loss / target.
        if st.in_position() {
            self.sync_legs(algo, &mut st).await?;
            if st.in_position() {
                self.watch_stops(algo, &st).await;
            }
        }
        if !st.armed && !st.in_position() {
            return Ok(());
        }

        // 4. The signal, once per newly closed bar, on every leg's own option bars.
        let rs = signals
            .iter()
            .find(|r| r.id == algo.signal_id)
            .ok_or_else(|| anyhow!("The signal this algo uses no longer exists"))?;
        let iv = resample::parse_interval(&algo.interval).ok_or_else(|| anyhow!("Invalid interval {}", algo.interval))?;
        let secs = interval_secs(&algo.interval).ok_or_else(|| anyhow!("Invalid interval {}", algo.interval))?;
        let bucket = last_closed_bucket(market_clock::get_ist_now().timestamp(), iv)
            .ok_or_else(|| anyhow!("Invalid interval {}", algo.interval))?;
        if st.last_bar_ts == 0 {
            store::set_last_bar(&pool, &algo.id, bucket).await?;
            return Ok(());
        }
        if bucket <= st.last_bar_ts {
            return Ok(());
        }
        // Consume the bar first: a failure below is journaled once, not retried every tick.
        store::set_last_bar(&pool, &algo.id, bucket).await?;
        self.state.recorder.flush_now().await;

        // CE before PE. A leg still held after the side was changed is managed (exit) too.
        let side_legs = algo.instrument.side.legs();
        let mut order = side_legs.clone();
        for l in &st.legs {
            if !order.contains(&l.is_call) {
                order.push(l.is_call);
            }
        }
        // Spot and expiry for entries: looked up once, and only when a leg is flat.
        let mut ctx: Option<(f64, String)> = None;
        let mut ctx_failed = false;
        let symbol = algo.instrument.underlying.clone();
        let step = strike_step(&symbol);

        for is_call in order {
            let ce_pe = if is_call { "CE" } else { "PE" };
            let held: Vec<Leg> = st.legs.iter().filter(|l| l.is_call == is_call).cloned().collect();
            if !held.is_empty() {
                self.cancel_unfilled(algo, &held).await;
                let label = leg_label(&held[0]);
                let d = self.leg_decision(algo, rs, &held[0].instrument, &label, iv, secs, bucket, true).await?;
                if d == Some(Decision::Exit) {
                    self.log(algo, "info", "signal", &format!("Exit signal on the {} bar of {} ({}) for {label}.", algo.interval, hhmm(bucket), rs.name))
                        .await;
                    self.close_legs(algo, &held, "exit signal").await;
                }
                continue;
            }
            if !st.armed || !side_legs.contains(&is_call) {
                continue;
            }
            if ctx.is_none() && !ctx_failed {
                match self.entry_context(algo, iv, secs).await {
                    Ok(c) => ctx = Some(c),
                    Err(e) => {
                        ctx_failed = true;
                        self.log(algo, "error", "error", &e.to_string()).await;
                    }
                }
            }
            let Some((spot, expiry)) = ctx.clone() else { continue };
            let strike = target_strike(spot, step, algo.instrument.strike_offset);
            if strike <= 0.0 {
                self.log(algo, "error", "error", &format!("Could not work out a strike from the spot {spot:.2}")).await;
                continue;
            }
            let key = option_key(&symbol, &expiry, strike, is_call);
            let label = format!("{symbol} {strike:.0} {ce_pe} {expiry}");
            let d = self.leg_decision(algo, rs, &key, &label, iv, secs, bucket, false).await?;
            if d == Some(Decision::Entry) {
                self.enter_leg(algo, &mut st, is_call, strike, &expiry, spot, bucket, &rs.name).await?;
            }
        }
        Ok(())
    }

    /// Spot (last closed bar of the underlying) and expiry for entries.
    async fn entry_context(&mut self, algo: &Algo, iv: Interval, secs: i64) -> Result<(f64, String)> {
        let expiry = self.expiry_for(algo).await?;
        let bars = self.underlying_bars(algo, iv).await?;
        let closed = closed_count(&bars, secs, market_clock::get_ist_now().timestamp());
        if closed == 0 {
            return Err(anyhow!("No closed {} bar of {} yet, so no strike can be chosen", algo.interval, algo.instrument.underlying));
        }
        Ok((bars[closed - 1].close, expiry))
    }

    /// The signal's decision for one option on the bar that just closed.
    /// `None` while the option has too little history or no bar for that interval.
    #[allow(clippy::too_many_arguments)]
    async fn leg_decision(
        &mut self,
        algo: &Algo,
        rs: &Ruleset,
        key: &str,
        label: &str,
        iv: Interval,
        secs: i64,
        bucket: i64,
        in_pos: bool,
    ) -> Result<Option<Decision>> {
        let bars = chart_store::chart_bars(&self.state.db, key, iv, BAR_LIMIT).await?;
        let closed = closed_count(&bars, secs, market_clock::get_ist_now().timestamp());
        let need = warmup_bars(rs);
        let wkey = format!("{}:{key}", algo.id);
        if closed < need {
            if self.warned.insert(wkey) {
                self.log(
                    algo,
                    "warn",
                    "skip",
                    &format!(
                        "{label}: {closed} closed {} bar(s) stored, the signal needs about {need} before it can fire. Bars are recorded while the price feed runs; waiting for history.",
                        algo.interval
                    ),
                )
                .await;
            }
            return Ok(None);
        }
        self.warned.remove(&wkey);
        // No bar for the closed interval (the option did not trade): nothing to decide on.
        if closed == 0 || bars[closed - 1].ts != bucket {
            return Ok(None);
        }
        eval::decide(rs, &bars, closed, in_pos).map_err(|e| anyhow!("Signal error: {e}"))
    }

    // -------------------------------------------------------------- entry ---

    /// BUY one leg (CE or PE) on every account that trades for this algo,
    /// each sized from its own free capital.
    #[allow(clippy::too_many_arguments)]
    async fn enter_leg(
        &mut self,
        algo: &Algo,
        st: &mut AlgoState,
        is_call: bool,
        strike: f64,
        expiry: &str,
        spot: f64,
        bar_ts: i64,
        signal_name: &str,
    ) -> Result<()> {
        let pool = self.state.db.clone();
        let symbol = algo.instrument.underlying.clone();
        let step = strike_step(&symbol);
        let ce_pe = if is_call { "CE" } else { "PE" };
        self.log(
            algo,
            "info",
            "signal",
            &format!(
                "Entry signal on the {} bar of {} ({signal_name}) for {symbol} {strike:.0} {ce_pe} {expiry}: spot {spot:.2}, ATM {:.0}, {} = {strike:.0}.",
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

        let mut placed: Vec<Leg> = Vec::new();
        let mut summary: Vec<String> = Vec::new();
        let mut failures = 0usize;

        let key = option_key(&symbol, expiry, strike, is_call);
        let Some(q) = self.state.ltp.get(&key) else {
            self.log(algo, "warn", "skip", &format!("No live price for {symbol} {strike:.0} {ce_pe} {expiry}: the option price feed is not running for it yet. Leg skipped.")).await;
            return Ok(());
        };
        let age = market_clock::get_ist_now().timestamp() - q.at;
        if age > MAX_PRICE_AGE_SECS {
            self.log(algo, "warn", "skip", &format!("The last price of {symbol} {strike:.0} {ce_pe} is {age}s old. Leg skipped.")).await;
            return Ok(());
        }
        let lot_size = match self.state.trading.contract_info(&symbol, expiry, strike, is_call).await {
            Ok(i) => i.lot_size,
            Err(e) => {
                self.log(algo, "error", "error", &format!("Contract lookup failed for {symbol} {strike:.0} {ce_pe}: {e}")).await;
                return Ok(());
            }
        };
        let Some(lot_size) = lot_size else {
            self.log(algo, "error", "error", &format!("Lot size of {symbol} is unknown. Refresh the instrument list or set trading.fallback_lot_sizes in settings.json.")).await;
            return Ok(());
        };
        let price = entry_limit(q.ltp, algo.action.buffer_ticks);
        let one_lot = price * lot_size as f64;
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
            // Capital is one pool per account: what the open legs committed is not free.
            let committed: f64 = st.legs.iter().filter(|l| l.account_id == *acct_id).map(|l| l.cost).sum();
            let free = free_capital(algo.action.capital, committed);
            let lots = lots_for_capital(free, price, lot_size).min(MAX_LOTS);
            if lots < 1 {
                let msg = if committed > 0.0 {
                    format!(
                        "Insufficient funds on {}: ₹{free:.0} free of ₹{:.0} (₹{committed:.0} is in open legs), one lot of {symbol} {strike:.0} {ce_pe} costs ₹{one_lot:.0} ({price:.2} x {lot_size}). Leg skipped.",
                        acct.name, algo.action.capital
                    )
                } else {
                    format!(
                        "Capital ₹{:.0} is below the cost of one lot of {symbol} {strike:.0} {ce_pe} (₹{one_lot:.0} = {price:.2} x {lot_size}). Leg skipped.",
                        algo.action.capital
                    )
                };
                self.log(algo, "warn", "skip", &msg).await;
                failures += 1;
                continue;
            }
            let paper = acct.kind == "paper";
            let bracket = (paper && (stop.is_some() || target.is_some()))
                .then(|| BracketInput { target, stop_trigger: stop, stop_price: None });
            let input = PlaceOrderInput {
                account_id: acct_id.clone(),
                symbol: symbol.clone(),
                expiry: expiry.to_string(),
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
                    let leg = Leg {
                        account_id: acct_id.clone(),
                        instrument: key.clone(),
                        symbol: symbol.clone(),
                        expiry: expiry.to_string(),
                        strike,
                        is_call,
                        product: algo.action.product.clone(),
                        lots,
                        order_id: o.id,
                        entry_price: price,
                        cost: price * lot_size as f64 * lots as f64,
                        stop: if paper { None } else { stop },
                        target: if paper { None } else { target },
                        bracket: bracket.is_some(),
                        propagate: algo.copier,
                    };
                    st.legs.push(leg.clone());
                    placed.push(leg);
                }
                Err(e) => {
                    failures += 1;
                    self.log(algo, "error", "error", &format!("Could not place BUY {lots} lot(s) {symbol} {strike:.0} {ce_pe} on {}: {e}", acct.name)).await;
                }
            }
        }

        if placed.is_empty() {
            self.notify(
                format!("{}:entry-failed:{bar_ts}:{ce_pe}", algo.id),
                format!("Algo could not enter: {}", algo.name),
                format!("The {ce_pe} entry signal fired but no order was placed ({failures} problem(s)). See the algo's journal."),
            );
            return Ok(());
        }
        st.phase = PHASE_IN.to_string();
        st.entry_bar_ts = bar_ts;
        store::set_position(&pool, &algo.id, PHASE_IN, &st.legs, bar_ts).await?;
        self.notify(
            format!("{}:entry:{bar_ts}:{ce_pe}", algo.id),
            format!("Algo entered: {}", algo.name),
            format!("{}\n{}", summary.join("\n"), if failures > 0 { format!("{failures} account(s) skipped or failed.") } else { String::new() }),
        );
        Ok(())
    }

    // ----------------------------------------------------------- position ---

    /// Drop the legs whose position is closed (exit order, stop-loss, target,
    /// or an entry that never filled). The algo is flat when none is left.
    async fn sync_legs(&self, algo: &Algo, st: &mut AlgoState) -> Result<()> {
        let pool = &self.state.db;
        let today = today_ist();
        let mut held_by_account: HashMap<String, Vec<crate::trading::store::OpenPos>> = HashMap::new();
        let mut keep: Vec<Leg> = Vec::new();
        let mut closed: Vec<(Leg, String)> = Vec::new();
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
                keep.push(leg.clone());
            } else {
                let note = match order {
                    Some(o) if o.filled_qty == 0 => format!(" (the entry order was {} and nothing was bought)", o.status),
                    _ => String::new(),
                };
                closed.push((leg.clone(), note));
            }
        }
        if closed.is_empty() {
            return Ok(());
        }
        let flat = keep.is_empty();
        let phase = if flat { PHASE_FLAT } else { PHASE_IN };
        store::set_position(pool, &algo.id, phase, &keep, if flat { 0 } else { st.entry_bar_ts }).await?;
        let first_order = closed[0].0.order_id.clone();
        let mut filled: Vec<String> = Vec::new();
        for (leg, note) in &closed {
            let acct = self.acct_name(&leg.account_id).await;
            self.log(algo, "info", "exit", &format!("Leg closed: {} on {acct}{note}.", leg_label(leg))).await;
            if note.is_empty() {
                filled.push(format!("{} on {acct}", leg_label(leg)));
            }
        }
        if flat {
            self.log(algo, "info", "exit", "Position closed. Scanning for the next entry from the next bar.").await;
        }
        if !filled.is_empty() {
            self.notify(
                format!("{}:closed:{first_order}", algo.id),
                format!("Algo leg closed: {}", algo.name),
                format!("{} (exit signal, stop-loss or target). See Orders / Trades for the P&L.", filled.join("; ")),
            );
        }
        st.legs = keep;
        st.phase = phase.to_string();
        if flat {
            st.entry_bar_ts = 0;
        }
        Ok(())
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
    async fn cancel_unfilled(&self, algo: &Algo, legs: &[Leg]) {
        for leg in legs {
            let Ok(Some(o)) = trade_store::get_order(&self.state.db, &leg.order_id).await else { continue };
            if is_open_status(&o.status) && o.filled_qty == 0 {
                match self.state.trading.cancel(&o.id).await {
                    Ok(_) => self.log(algo, "info", "info", &format!("Cancelled the unfilled entry order for {} at the next bar.", leg_label(leg))).await,
                    Err(e) => self.log(algo, "warn", "error", &format!("Could not cancel the unfilled entry order for {}: {e}", leg_label(leg))).await,
                }
            }
        }
    }

    async fn close_legs(&mut self, algo: &Algo, legs: &[Leg], reason: &str) {
        let mut sent = 0;
        for leg in legs {
            if self.close_leg(algo, leg, reason).await {
                sent += 1;
            }
        }
        if sent > 0 {
            self.notify(
                format!("{}:exit:{}:{}", algo.id, legs.first().map(|l| l.instrument.as_str()).unwrap_or(""), Utc::now().timestamp() / 60),
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
