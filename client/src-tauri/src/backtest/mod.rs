//! Backtesting: run a saved signal (ruleset) over stored bars of one
//! instrument and report the trades, P&L after estimated charges, win rate
//! and drawdown.
//!
//! Rules of the simulation (kept simple and conservative):
//! * Long only (BUY entry, SELL exit), one position at a time.
//! * Conditions are evaluated on a *closed* bar; the order fills at the
//!   **open of the next bar** (never at the close that produced the signal).
//! * Stop-loss / target (percent of the entry price, same maths as algos)
//!   are checked on every bar from the entry bar on. A stop fills at its
//!   trigger (or the bar's open if it gapped below); a target fills at its
//!   price (or the open if it gapped above). If one bar touches both, the
//!   stop wins.
//! * Slippage (ticks of 0.05) is adverse on entries and stop/signal exits.
//! * Minute intervals are intraday by default: the position is closed at the
//!   close of the day's last bar and no entry is placed from it. `Day` bars
//!   always carry positions.
//! * Bars before the start date are used only to warm the indicators.
//! * Sizing: fixed `lots`, or `capitalPerTrade / (fill price x lot size)`
//!   whole lots when `lots` is 0 (same as algos). Too little capital for one
//!   lot skips the trade.
//!
//! `simulate` and `summarize` are pure (unit-tested); `run` does the I/O.

use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::algo::{bracket_levels, closed_count, lots_for_capital};
use crate::charts::instrument::{self, Instrument};
use crate::charts::resample::{self, Interval};
use crate::charts::rules::{self, eval as rule_eval};
use crate::charts::{store, Bar};
use crate::settings::{AppConfig, ChargesConfig};
use crate::trading::charges;
use crate::trading::pricing::{round_to_tick, TICK};
use crate::trading::scrip;

/// Most bars loaded for one run.
const MAX_BARS: usize = 20_000;
const IST_SECS: i64 = 5 * 3600 + 30 * 60;

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRequest {
    pub ruleset_id: String,
    pub instrument: String,
    pub interval: String,
    /// IST dates, `YYYY-MM-DD`, inclusive. Empty = all stored data.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    /// Fixed lots per trade; 0 = size from `capital_per_trade`.
    #[serde(default)]
    pub lots: i64,
    #[serde(default)]
    pub capital_per_trade: f64,
    /// Units per lot; 0 = automatic (option: instrument list / fallback; index: 1).
    #[serde(default)]
    pub lot_size: i64,
    /// For return % and drawdown %; 0 = not shown.
    #[serde(default)]
    pub starting_capital: f64,
    #[serde(default)]
    pub stop_loss_pct: Option<f64>,
    #[serde(default)]
    pub target_pct: Option<f64>,
    #[serde(default)]
    pub slippage_ticks: u32,
    /// Deduct estimated charges (settings `trading.charges` + brokerage).
    #[serde(default = "yes")]
    pub charges: bool,
    /// Flat brokerage per order; `None` = the Dhan default from settings.
    #[serde(default)]
    pub brokerage_per_order: Option<f64>,
    /// Keep positions overnight on minute intervals.
    #[serde(default)]
    pub carry_overnight: bool,
}

/// Everything `simulate` needs besides the bars.
#[derive(Debug, Clone)]
pub struct Params {
    pub lots: i64,
    pub capital_per_trade: f64,
    pub lot_size: i64,
    pub stop_loss_pct: Option<f64>,
    pub target_pct: Option<f64>,
    pub slippage_ticks: u32,
    /// (rates, flat brokerage, brokerage cap % of turnover); `None` = no charges.
    pub charges: Option<(ChargesConfig, f64, f64)>,
    /// Keep positions overnight (always true for daily bars).
    pub carry: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Trade {
    pub entry_ts: i64,
    pub entry_price: f64,
    pub exit_ts: i64,
    pub exit_price: f64,
    pub lots: i64,
    pub qty: i64,
    /// `signal` | `stop_loss` | `target` | `eod` | `end` (data ended).
    pub reason: String,
    pub gross: f64,
    pub charges: f64,
    pub net: f64,
    pub bars: usize,
    pub return_pct: f64,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: f64,
    pub gross_pnl: f64,
    pub charges: f64,
    pub net_pnl: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    /// Gross profit / gross loss on net trade P&L; `None` when nothing was lost.
    pub profit_factor: Option<f64>,
    /// Average net P&L per trade.
    pub expectancy: f64,
    pub best: f64,
    pub worst: f64,
    /// Largest fall of closed-trade equity from its peak (rupees).
    pub max_drawdown: f64,
    pub max_drawdown_pct: Option<f64>,
    pub max_consecutive_losses: usize,
    pub avg_bars: f64,
    pub return_pct: Option<f64>,
    /// Entries skipped because the capital did not buy one lot.
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EquityPoint {
    pub ts: i64,
    pub equity: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub ruleset: String,
    pub instrument: String,
    pub interval: String,
    pub lot_size: i64,
    /// Bars in the tested range, and sessions (IST days) they cover.
    pub bars: usize,
    pub sessions: usize,
    pub first_ts: i64,
    pub last_ts: i64,
    pub summary: Summary,
    pub trades: Vec<Trade>,
    pub equity: Vec<EquityPoint>,
    pub warnings: Vec<String>,
}

struct Open {
    idx: usize,
    price: f64,
    lots: i64,
    qty: i64,
    stop: Option<f64>,
    target: Option<f64>,
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn charge_for(p: &Params, price: f64, qty: i64, is_sell: bool) -> f64 {
    let Some((cfg, flat, cap_pct)) = &p.charges else { return 0.0 };
    let turnover = price * qty as f64;
    let brokerage = charges::dhan_brokerage(*flat, *cap_pct, turnover);
    charges::compute(cfg, brokerage, is_sell, turnover).total
}

fn close_trade(o: &Open, bars: &[Bar], at: usize, exit_price: f64, reason: &str, p: &Params) -> Trade {
    let gross = (exit_price - o.price) * o.qty as f64;
    let fees = charge_for(p, o.price, o.qty, false) + charge_for(p, exit_price, o.qty, true);
    Trade {
        entry_ts: bars[o.idx].ts,
        entry_price: o.price,
        exit_ts: bars[at].ts,
        exit_price,
        lots: o.lots,
        qty: o.qty,
        reason: reason.into(),
        gross: r2(gross),
        charges: r2(fees),
        net: r2(gross - fees),
        bars: at.saturating_sub(o.idx) + 1,
        return_pct: if o.price > 0.0 { r2((exit_price / o.price - 1.0) * 100.0) } else { 0.0 },
    }
}

/// Trade the bars `[start, end)` using the per-bar `entry` / `exit` truth
/// values (computed on the whole series so indicators are warmed up).
/// Returns the trades and the number of entries skipped for lack of capital.
pub fn simulate(bars: &[Bar], entry: &[bool], exit: &[bool], start: usize, end: usize, p: &Params) -> (Vec<Trade>, usize) {
    let end = end.min(bars.len()).min(entry.len()).min(exit.len());
    let mut trades = Vec::new();
    let mut skipped = 0usize;
    let slip = p.slippage_ticks as f64 * TICK;
    let adverse = |px: f64, up: bool| {
        if slip == 0.0 {
            px
        } else {
            round_to_tick(if up { px + slip } else { (px - slip).max(TICK) })
        }
    };

    let mut pos: Option<Open> = None;
    // Order decided on the previous bar's close: Some(true) = entry, Some(false) = exit.
    let mut pending: Option<bool> = None;

    for i in start..end {
        let b = &bars[i];

        // 1. Fill the pending order at this bar's open.
        if let Some(is_entry) = pending.take() {
            if is_entry && pos.is_none() {
                let fill = adverse(b.open, true);
                let lots = if p.lots > 0 { p.lots } else { lots_for_capital(p.capital_per_trade, fill, p.lot_size) };
                if lots <= 0 || fill <= 0.0 {
                    skipped += 1;
                } else {
                    let (stop, target) = bracket_levels(fill, p.stop_loss_pct, p.target_pct);
                    pos = Some(Open { idx: i, price: fill, lots, qty: lots * p.lot_size, stop, target });
                }
            } else if !is_entry {
                if let Some(o) = pos.take() {
                    trades.push(close_trade(&o, bars, i, adverse(b.open, false), "signal", p));
                }
            }
        }

        // 2. Stop-loss / target inside this bar (also the entry bar).
        if let Some(o) = pos.as_ref() {
            let stop_hit = o.stop.filter(|s| b.low <= *s);
            let target_hit = o.target.filter(|t| b.high >= *t);
            let result = if let Some(s) = stop_hit {
                Some((adverse(s.min(b.open), false), "stop_loss"))
            } else {
                target_hit.map(|t| (t.max(b.open), "target"))
            };
            if let Some((px, why)) = result {
                let o = pos.take().expect("position checked above");
                trades.push(close_trade(&o, bars, i, px, why, p));
            }
        }

        // 3. End of the session / of the tested data.
        let last = i + 1 == end;
        let day_end = i + 1 >= bars.len() || resample::ist_day(bars[i + 1].ts) != resample::ist_day(b.ts);
        let flat_overnight = day_end && !p.carry;
        if (flat_overnight || last) && pos.is_some() {
            let o = pos.take().expect("position checked above");
            let why = if flat_overnight { "eod" } else { "end" };
            trades.push(close_trade(&o, bars, i, adverse(b.close, false), why, p));
        }

        // 4. Signals on this closed bar act at the next bar's open.
        if last || flat_overnight {
            continue;
        }
        if pos.is_some() {
            if exit[i] {
                pending = Some(false);
            }
        } else if entry[i] {
            pending = Some(true);
        }
    }
    (trades, skipped)
}

/// Statistics and the closed-trade equity curve.
pub fn summarize(trades: &[Trade], starting_capital: f64, skipped: usize) -> (Summary, Vec<EquityPoint>) {
    let mut s = Summary { skipped, ..Default::default() };
    let mut equity = Vec::with_capacity(trades.len());
    let (mut win_sum, mut loss_sum) = (0.0f64, 0.0f64);
    let (mut streak, mut peak, mut eq) = (0usize, starting_capital, starting_capital);
    s.best = f64::NEG_INFINITY;
    s.worst = f64::INFINITY;
    let mut bars_total = 0usize;
    let mut dd_peak = starting_capital;

    for t in trades {
        s.trades += 1;
        s.gross_pnl += t.gross;
        s.charges += t.charges;
        s.net_pnl += t.net;
        s.best = s.best.max(t.net);
        s.worst = s.worst.min(t.net);
        bars_total += t.bars;
        if t.net > 0.0 {
            s.wins += 1;
            win_sum += t.net;
            streak = 0;
        } else {
            s.losses += 1;
            loss_sum += -t.net;
            streak += 1;
            s.max_consecutive_losses = s.max_consecutive_losses.max(streak);
        }
        eq += t.net;
        if eq > peak {
            peak = eq;
        }
        let dd = peak - eq;
        if dd > s.max_drawdown {
            s.max_drawdown = dd;
            dd_peak = peak;
        }
        equity.push(EquityPoint { ts: t.exit_ts, equity: r2(eq) });
    }

    if s.trades == 0 {
        s.best = 0.0;
        s.worst = 0.0;
        return (s, equity);
    }
    let n = s.trades as f64;
    s.win_rate = r2(s.wins as f64 / n * 100.0);
    s.avg_win = if s.wins > 0 { r2(win_sum / s.wins as f64) } else { 0.0 };
    s.avg_loss = if s.losses > 0 { r2(-loss_sum / s.losses as f64) } else { 0.0 };
    s.profit_factor = (loss_sum > 0.0).then(|| r2(win_sum / loss_sum));
    s.expectancy = r2(s.net_pnl / n);
    s.avg_bars = r2(bars_total as f64 / n);
    s.gross_pnl = r2(s.gross_pnl);
    s.charges = r2(s.charges);
    s.net_pnl = r2(s.net_pnl);
    s.max_drawdown = r2(s.max_drawdown);
    if starting_capital > 0.0 {
        s.return_pct = Some(r2(s.net_pnl / starting_capital * 100.0));
        if dd_peak > 0.0 {
            s.max_drawdown_pct = Some(r2(s.max_drawdown / dd_peak * 100.0));
        }
    }
    (s, equity)
}

fn day_start_ts(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp()).unwrap_or(0) - IST_SECS
}

fn parse_day(s: &Option<String>, what: &str) -> Result<Option<NaiveDate>> {
    match s.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => NaiveDate::parse_from_str(v, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| anyhow!("{what} date must look like 2026-11-05")),
    }
}

/// Number of gaps inside one session that are longer than 3 bars.
fn count_gaps(bars: &[Bar], step: i64) -> usize {
    bars.windows(2)
        .filter(|w| resample::ist_day(w[0].ts) == resample::ist_day(w[1].ts) && w[1].ts - w[0].ts > 3 * step)
        .count()
}

pub async fn run(pool: &SqlitePool, cfg: &AppConfig, req: BacktestRequest) -> Result<Report> {
    let interval = resample::parse_interval(&req.interval).ok_or_else(|| anyhow!("Unsupported interval: {}", req.interval))?;
    let (step, daily) = match interval {
        Interval::Min(n) => (n * 60, false),
        Interval::Day => (86_400, true),
    };
    if req.lots < 0 || req.capital_per_trade < 0.0 || req.starting_capital < 0.0 || req.lot_size < 0 {
        return Err(anyhow!("Lots, lot size and capital cannot be negative"));
    }
    if req.lots == 0 && req.capital_per_trade <= 0.0 {
        return Err(anyhow!("Enter the lots per trade, or a capital per trade"));
    }
    for (name, v) in [("Stop-loss", req.stop_loss_pct), ("Target", req.target_pct)] {
        if let Some(v) = v {
            if !(v > 0.0 && v < 1000.0) {
                return Err(anyhow!("{name} must be a percentage above 0"));
            }
        }
    }
    if req.slippage_ticks > 100 {
        return Err(anyhow!("Slippage must be at most 100 ticks"));
    }
    let parsed = instrument::parse(&req.instrument).ok_or_else(|| anyhow!("Unknown instrument: {}", req.instrument))?;

    let rs = rules::store::list(pool)
        .await?
        .into_iter()
        .find(|r| r.id == req.ruleset_id)
        .ok_or_else(|| anyhow!("Signal not found"))?;

    let lot_size = if req.lot_size > 0 {
        req.lot_size
    } else {
        match &parsed {
            Instrument::Option { symbol, .. } => match scrip::lot_size_for_symbol(pool, symbol).await? {
                Some(l) if l > 0 => l,
                _ => *cfg
                    .trading
                    .fallback_lot_sizes
                    .get(&symbol.to_uppercase())
                    .ok_or_else(|| anyhow!("Lot size of {symbol} is unknown: enter it"))?,
            },
            Instrument::Index(_) => 1,
        }
    };

    let from = parse_day(&req.from, "From")?;
    let to = parse_day(&req.to, "To")?;
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            return Err(anyhow!("From date is after the To date"));
        }
    }

    let mut bars = store::chart_bars(pool, &req.instrument, interval, MAX_BARS).await?;
    let loaded = bars.len();
    let closed = closed_count(&bars, step, chrono::Utc::now().timestamp());
    bars.truncate(closed);
    let start = from.map_or(0, |d| {
        let t = day_start_ts(d);
        bars.partition_point(|b| b.ts < t)
    });
    let end = to.map_or(bars.len(), |d| {
        let t = day_start_ts(d) + 86_400;
        bars.partition_point(|b| b.ts < t)
    });
    if bars.is_empty() || start >= end {
        return Err(anyhow!("No stored bars for {} in that range. Record or backfill data on the Charts page first.", req.instrument));
    }

    let (entry, exit) = rule_eval::conditions(&rs, &bars).map_err(|e| anyhow!("{}: {e}", rs.name))?;
    let params = Params {
        lots: req.lots,
        capital_per_trade: req.capital_per_trade,
        lot_size,
        stop_loss_pct: req.stop_loss_pct,
        target_pct: req.target_pct,
        slippage_ticks: req.slippage_ticks,
        charges: req.charges.then(|| {
            (
                cfg.trading.charges.clone(),
                req.brokerage_per_order.unwrap_or(cfg.trading.dhan_brokerage_per_order).max(0.0),
                cfg.trading.dhan_brokerage_cap_pct,
            )
        }),
        carry: daily || req.carry_overnight,
    };
    let (trades, skipped) = simulate(&bars, &entry, &exit, start, end, &params);
    let (summary, equity) = summarize(&trades, req.starting_capital, skipped);

    let range = &bars[start..end];
    let mut sessions: Vec<i64> = range.iter().map(|b| resample::ist_day(b.ts)).collect();
    sessions.dedup();

    let mut warnings = Vec::new();
    if loaded >= MAX_BARS {
        warnings.push(format!("Only the latest {MAX_BARS} bars are loaded; older data is not tested."));
    }
    if sessions.len() < 5 && !daily {
        warnings.push(format!("Only {} session(s) of data: too little to judge a signal.", sessions.len()));
    }
    if !daily {
        let gaps = count_gaps(range, step);
        if gaps > 0 {
            warnings.push(format!("{gaps} gap(s) of more than 3 bars inside sessions (app not running?). Trades across a gap fill at the next stored bar."));
        }
    }
    if !has_signal_rules(&rs) {
        warnings.push("The signal has no Entry conditions, so nothing is traded.".into());
    }
    if matches!(parsed, Instrument::Index(_)) && req.charges {
        warnings.push("Index prices are tested like option premiums for charges: switch charges off to see index points only.".into());
    }

    Ok(Report {
        ruleset: rs.name.clone(),
        instrument: req.instrument,
        interval: req.interval,
        lot_size,
        bars: range.len(),
        sessions: sessions.len(),
        first_ts: range.first().map_or(0, |b| b.ts),
        last_ts: range.last().map_or(0, |b| b.ts),
        summary,
        trades,
        equity,
        warnings,
    })
}

fn has_signal_rules(rs: &rules::Ruleset) -> bool {
    rules::has_conditions(&rs.entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-05 09:15 IST.
    const OPEN: i64 = 1_793_850_300;

    fn bar(i: usize, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar { ts: OPEN + i as i64 * 300, open: o, high: h, low: l, close: c, volume: 1.0, oi: None }
    }

    fn flat(n: usize, px: f64) -> Vec<Bar> {
        (0..n).map(|i| bar(i, px, px, px, px)).collect()
    }

    fn params() -> Params {
        Params { lots: 1, capital_per_trade: 0.0, lot_size: 10, stop_loss_pct: None, target_pct: None, slippage_ticks: 0, charges: None, carry: false }
    }

    fn sig(n: usize, at: &[usize]) -> Vec<bool> {
        (0..n).map(|i| at.contains(&i)).collect()
    }

    #[test]
    fn fills_at_next_open_and_exits_on_signal() {
        let mut bars = flat(8, 100.0);
        bars[3] = bar(3, 100.0, 100.0, 100.0, 100.0);
        bars[4] = bar(4, 105.0, 105.0, 105.0, 105.0); // entry fill (signal on bar 3)
        bars[6] = bar(6, 110.0, 110.0, 110.0, 110.0); // exit fill (signal on bar 5)
        let (t, skipped) = simulate(&bars, &sig(8, &[3]), &sig(8, &[5]), 0, 8, &params());
        assert_eq!(skipped, 0);
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].entry_price, t[0].exit_price), (105.0, 110.0));
        assert_eq!(t[0].reason, "signal");
        assert_eq!(t[0].qty, 10);
        assert_eq!(t[0].gross, 50.0);
    }

    #[test]
    fn stop_wins_when_a_bar_touches_both() {
        let mut bars = flat(6, 100.0);
        bars[2] = bar(2, 100.0, 120.0, 90.0, 100.0);
        let p = Params { stop_loss_pct: Some(5.0), target_pct: Some(10.0), ..params() };
        let (t, _) = simulate(&bars, &sig(6, &[0]), &sig(6, &[]), 0, 6, &p);
        // Entry at bar 1 open (100): stop 95, target 110; bar 2 touches both.
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].reason, "stop_loss");
        assert_eq!(t[0].exit_price, 95.0);
    }

    #[test]
    fn target_fills_at_price_and_gap_fills_at_open() {
        let mut bars = flat(6, 100.0);
        bars[2] = bar(2, 100.0, 112.0, 99.0, 111.0);
        let p = Params { target_pct: Some(10.0), ..params() };
        let (t, _) = simulate(&bars, &sig(6, &[0]), &sig(6, &[]), 0, 6, &p);
        assert_eq!((t[0].reason.as_str(), t[0].exit_price), ("target", 110.0));

        let mut gap = flat(6, 100.0);
        gap[2] = bar(2, 120.0, 125.0, 118.0, 122.0);
        let (t, _) = simulate(&gap, &sig(6, &[0]), &sig(6, &[]), 0, 6, &p);
        assert_eq!(t[0].exit_price, 120.0);
    }

    #[test]
    fn intraday_position_is_closed_at_the_last_bar_of_the_day() {
        // Two sessions of 4 bars; bar 4 starts the next IST day.
        let mut bars = flat(4, 100.0);
        bars.extend((0..4).map(|i| Bar { ts: OPEN + 86_400 + i as i64 * 300, ..bar(0, 100.0, 100.0, 100.0, 100.0) }));
        bars[3] = bar(3, 100.0, 100.0, 100.0, 108.0);
        let (t, _) = simulate(&bars, &sig(8, &[0, 3]), &sig(8, &[]), 0, 8, &params());
        assert_eq!(t.len(), 1, "the signal on the day's last bar is not carried to the next day");
        assert_eq!((t[0].reason.as_str(), t[0].exit_price), ("eod", 108.0));
        let carry = Params { carry: true, ..params() };
        let (t, _) = simulate(&bars, &sig(8, &[0]), &sig(8, &[]), 0, 8, &carry);
        assert_eq!(t[0].reason, "end");
    }

    #[test]
    fn capital_sizing_skips_when_one_lot_does_not_fit() {
        let bars = flat(6, 100.0);
        let small = Params { lots: 0, capital_per_trade: 500.0, ..params() }; // 1 lot = 1000
        let (t, skipped) = simulate(&bars, &sig(6, &[0]), &sig(6, &[]), 0, 6, &small);
        assert_eq!((t.len(), skipped), (0, 1));
        let ok = Params { lots: 0, capital_per_trade: 2_500.0, ..params() };
        let (t, _) = simulate(&bars, &sig(6, &[0]), &sig(6, &[]), 0, 6, &ok);
        assert_eq!(t[0].lots, 2);
    }

    #[test]
    fn signals_before_the_start_bar_are_ignored() {
        let bars = flat(8, 100.0);
        let (t, _) = simulate(&bars, &sig(8, &[1, 4]), &sig(8, &[]), 3, 8, &params());
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].entry_ts, bars[5].ts);
    }

    #[test]
    fn slippage_is_adverse_and_charges_reduce_net() {
        let bars = flat(6, 100.0);
        let p = Params { slippage_ticks: 2, charges: Some((ChargesConfig::default(), 20.0, 0.03)), ..params() };
        let (t, _) = simulate(&bars, &sig(6, &[0]), &sig(6, &[2]), 0, 6, &p);
        assert_eq!((t[0].entry_price, t[0].exit_price), (100.1, 99.9));
        assert!(t[0].gross < 0.0 && t[0].charges > 0.0 && t[0].net < t[0].gross);
    }

    fn tr(net: f64) -> Trade {
        Trade {
            entry_ts: 0,
            entry_price: 100.0,
            exit_ts: 1,
            exit_price: 100.0,
            lots: 1,
            qty: 1,
            reason: "signal".into(),
            gross: net,
            charges: 0.0,
            net,
            bars: 2,
            return_pct: 0.0,
        }
    }

    #[test]
    fn summary_statistics() {
        let trades = [tr(100.0), tr(-40.0), tr(-60.0), tr(30.0)];
        let (s, eq) = summarize(&trades, 1_000.0, 0);
        assert_eq!((s.trades, s.wins, s.losses), (4, 2, 2));
        assert_eq!(s.win_rate, 50.0);
        assert_eq!(s.net_pnl, 30.0);
        assert_eq!(s.profit_factor, Some(1.3));
        assert_eq!(s.max_drawdown, 100.0);
        assert_eq!(s.max_drawdown_pct, Some(9.09));
        assert_eq!(s.max_consecutive_losses, 2);
        assert_eq!(s.return_pct, Some(3.0));
        assert_eq!(eq.last().map(|p| p.equity), Some(1_030.0));
    }

    #[test]
    fn empty_summary_is_zeroed() {
        let (s, eq) = summarize(&[], 0.0, 2);
        assert_eq!((s.trades, s.best, s.worst, s.skipped), (0, 0.0, 0.0, 2));
        assert!(eq.is_empty() && s.profit_factor.is_none());
    }
}
