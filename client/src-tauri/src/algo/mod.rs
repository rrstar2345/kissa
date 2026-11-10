//! Algo trading: autonomous option orders from a saved signal.
//!
//! * An `Algo` is JSON in SQLite (`algos`). It uses exactly one saved signal
//!   (ruleset, `signal_id`); a signal can serve many algos and an account
//!   many algos. Signals are loaded by id on every evaluation, so editing a
//!   signal changes every algo that uses it. A signal in use cannot be deleted.
//! * The signal is evaluated on the *underlying's* bars (e.g. `IDX:NIFTY 50`)
//!   at every bar close (`runner`). Flat + Entry: the strike is resolved from
//!   the spot at that bar (ATM = nearest strike, ATM+n = n steps up) and held
//!   fixed until the position is closed; BUY orders go to the CE, PE or both.
//!   Exit (signal, stop-loss or target, whichever comes first) closes it, and
//!   the next scan starts again from the then-current spot.
//! * Capital decides the lots: `floor(capital per leg / (limit price x lot size))`.
//! * Accounts: any number, with a trade-copier switch (see `select_accounts`).
//! * This module is pure (model, validation, maths); `store` and `runner` do
//!   the I/O. Every decision is written to the journal.

pub mod runner;
pub mod service;
pub mod store;

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::charts::resample::{self, Interval};
use crate::charts::Bar;
use crate::trading::copier::CopierConfig;
use crate::trading::pricing::{round_to_tick, TICK};

pub const KIND_OPTION: &str = "option";
pub const PHASE_FLAT: &str = "flat";
pub const PHASE_IN: &str = "in_position";
/// How many underlying bars are loaded for every evaluation.
pub const BAR_LIMIT: usize = 1000;
pub const MAX_STRIKE_OFFSET: i32 = 20;

// ---------------------------------------------------------------- model ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExpirySel {
    #[default]
    Current,
    Next,
    Next1,
    Next2,
    Next3,
}

impl ExpirySel {
    /// Position in the list of upcoming expiries (0 = the nearest).
    pub fn index(self) -> usize {
        match self {
            ExpirySel::Current => 0,
            ExpirySel::Next => 1,
            ExpirySel::Next1 => 2,
            ExpirySel::Next2 => 3,
            ExpirySel::Next3 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideSel {
    #[default]
    Ce,
    Pe,
    Both,
}

impl SideSel {
    /// `is_call` of every leg.
    pub fn legs(self) -> Vec<bool> {
        match self {
            SideSel::Ce => vec![true],
            SideSel::Pe => vec![false],
            SideSel::Both => vec![true, false],
        }
    }
}

/// Which contract(s) the algo trades. Only index options exist for now
/// (`kind` is kept so stocks can be added without changing the format).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstrumentRule {
    #[serde(rename = "type")]
    pub kind: String,
    /// F&O underlying, e.g. `NIFTY`.
    pub underlying: String,
    pub expiry: ExpirySel,
    /// Strikes above (+) or below (-) the at-the-money strike; 0 = ATM.
    pub strike_offset: i32,
    pub side: SideSel,
}

impl Default for InstrumentRule {
    fn default() -> Self {
        Self {
            kind: KIND_OPTION.into(),
            underlying: "NIFTY".into(),
            expiry: ExpirySel::Current,
            strike_offset: 0,
            side: SideSel::Ce,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Action {
    /// INTRADAY | MARGIN
    pub product: String,
    /// Rupees the algo may commit per account. With `Both` it is split
    /// equally between the CE and the PE leg.
    pub capital: f64,
    /// Entry LIMIT = last price + this many ticks (0.05 each).
    pub buffer_ticks: u32,
    /// Stop-loss, percent below the entry price of each leg.
    pub stop_loss_pct: Option<f64>,
    /// Target, percent above the entry price of each leg.
    pub target_pct: Option<f64>,
}

impl Default for Action {
    fn default() -> Self {
        Self { product: "INTRADAY".into(), capital: 0.0, buffer_ticks: 2, stop_loss_pct: None, target_pct: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Algo {
    pub id: String,
    pub name: String,
    pub signal_id: String,
    pub instrument: InstrumentRule,
    /// Bar size the signal is evaluated on, minutes only (`1m`, `5m`, `15m`...).
    pub interval: String,
    pub action: Action,
    /// Accounts the algo trades on (many algos can share an account).
    pub accounts: Vec<String>,
    /// Trade-copier switch, see `select_accounts`.
    pub copier: bool,
    /// Per-algo permission to trade live (Dhan) accounts. Never exported.
    pub allow_live: bool,
}

impl Default for Algo {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            signal_id: String::new(),
            instrument: InstrumentRule::default(),
            interval: "5m".into(),
            action: Action::default(),
            accounts: Vec::new(),
            copier: false,
            allow_live: false,
        }
    }
}

/// One open leg of an algo position (kept in `algo_state`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Leg {
    pub account_id: String,
    pub instrument: String,
    pub symbol: String,
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    pub product: String,
    pub lots: i64,
    /// The entry order.
    pub order_id: String,
    /// Entry limit price.
    pub entry_price: f64,
    /// Stop-loss / target watched by the algo itself (accounts whose broker
    /// has no bracket orders). Paper accounts use bracket legs instead.
    #[serde(default)]
    pub stop: Option<f64>,
    #[serde(default)]
    pub target: Option<f64>,
    /// True when the broker holds the stop-loss / target legs.
    pub bracket: bool,
    /// Orders for this leg go through the trade copier.
    pub propagate: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlgoState {
    pub algo_id: String,
    pub armed: bool,
    pub phase: String,
    pub legs: Vec<Leg>,
    /// Start of the last closed bar that was looked at (0 = none yet: the
    /// next look only sets the baseline, so arming never trades a stale bar).
    pub last_bar_ts: i64,
    /// Bar the current position was entered on.
    pub entry_bar_ts: i64,
    pub updated_at: String,
}

impl AlgoState {
    pub fn fresh(algo_id: &str) -> Self {
        Self {
            algo_id: algo_id.to_string(),
            armed: false,
            phase: PHASE_FLAT.into(),
            legs: Vec::new(),
            last_bar_ts: 0,
            entry_bar_ts: 0,
            updated_at: String::new(),
        }
    }

    pub fn in_position(&self) -> bool {
        self.phase == PHASE_IN
    }
}

/// What the Algos page shows for one algo.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlgoView {
    pub algo: Algo,
    pub signal_name: Option<String>,
    pub armed: bool,
    pub phase: String,
    pub legs: Vec<Leg>,
    pub last_bar_ts: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct JournalRow {
    pub id: i64,
    pub ts: String,
    pub algo_id: String,
    pub algo_name: String,
    /// info | warn | error
    pub level: String,
    /// armed | disarmed | signal | entry | exit | skip | error | info
    pub kind: String,
    pub message: String,
}

// ----------------------------------------------------------- validation ---

fn valid_underlying(s: &str) -> bool {
    !s.is_empty() && s.len() <= 24 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '&' || c == '-' || c == '_')
}

impl Algo {
    /// Trim and normalise the fields that have one right spelling.
    pub fn normalize(&mut self) {
        self.id = self.id.trim().to_string();
        self.name = self.name.trim().to_string();
        self.signal_id = self.signal_id.trim().to_string();
        self.instrument.kind = self.instrument.kind.trim().to_lowercase();
        self.instrument.underlying = self.instrument.underlying.trim().to_uppercase();
        self.interval = self.interval.trim().to_lowercase();
        self.action.product = self.action.product.trim().to_uppercase();
        let mut seen = HashSet::new();
        self.accounts = self
            .accounts
            .iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty() && seen.insert(a.clone()))
            .collect();
    }

    /// Structure checks that need no database. Whether the signal and the
    /// accounts exist is checked by `runner::validate_full`.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.name.trim().is_empty() {
            errs.push("Name is required".into());
        }
        if self.signal_id.trim().is_empty() {
            errs.push("Choose the signal the algo runs on".into());
        }
        if self.instrument.kind != KIND_OPTION {
            errs.push("Only option algos are supported for now".into());
        }
        if !valid_underlying(&self.instrument.underlying) {
            errs.push("Underlying must be an F&O symbol such as NIFTY".into());
        }
        if self.instrument.strike_offset.abs() > MAX_STRIKE_OFFSET {
            errs.push(format!("Strike offset must be between -{MAX_STRIKE_OFFSET} and {MAX_STRIKE_OFFSET}"));
        }
        match resample::parse_interval(&self.interval) {
            Some(Interval::Min(_)) => {}
            Some(Interval::Day) => errs.push("Daily bars are not supported: use a minute interval such as 5m".into()),
            None => errs.push("Interval must be in minutes, e.g. 1m, 5m, 15m".into()),
        }
        if self.action.product != "INTRADAY" && self.action.product != "MARGIN" {
            errs.push("Product must be INTRADAY or MARGIN".into());
        }
        let cap = self.action.capital;
        if !(cap.is_finite() && cap > 0.0 && cap <= 1e9) {
            errs.push("Capital must be greater than 0".into());
        }
        if self.action.buffer_ticks > 100 {
            errs.push("Buffer must be at most 100 ticks".into());
        }
        if let Some(sl) = self.action.stop_loss_pct {
            if !(sl.is_finite() && sl > 0.0 && sl < 100.0) {
                errs.push("Stop-loss must be above 0% and below 100%".into());
            }
        }
        if let Some(t) = self.action.target_pct {
            if !(t.is_finite() && t > 0.0 && t <= 1000.0) {
                errs.push("Target must be above 0% and at most 1000%".into());
            }
        }
        errs
    }
}

// ---------------------------------------------------------------- maths ---

/// Distance between strikes. Estimates as of Nov 2026 (like the lot sizes):
/// verify against the exchange.
pub fn strike_step(symbol: &str) -> f64 {
    match symbol.trim().to_uppercase().as_str() {
        "BANKNIFTY" | "NIFTYNXT50" => 100.0,
        "MIDCPNIFTY" => 25.0,
        _ => 50.0, // NIFTY, FINNIFTY and anything unknown
    }
}

/// The strike nearest to the spot (a tie goes up).
pub fn atm_strike(spot: f64, step: f64) -> f64 {
    if step <= 0.0 || !spot.is_finite() {
        return 0.0;
    }
    (spot / step).round() * step
}

/// ATM shifted by `offset` strikes (+ above spot, - below).
pub fn target_strike(spot: f64, step: f64, offset: i32) -> f64 {
    atm_strike(spot, step) + offset as f64 * step
}

/// The expiry picked from ISO dates: only dates from `today` on count,
/// sorted, `Current` = the nearest.
pub fn pick_expiry(sel: ExpirySel, expiries: &[String], today: &str) -> Option<String> {
    let mut upcoming: Vec<&String> = expiries.iter().filter(|e| e.as_str() >= today).collect();
    upcoming.sort();
    upcoming.dedup();
    upcoming.get(sel.index()).map(|s| (*s).clone())
}

/// Entry LIMIT: last price plus `buffer_ticks` ticks (marketable, never a market order).
pub fn entry_limit(ltp: f64, buffer_ticks: u32) -> f64 {
    round_to_tick(ltp + buffer_ticks as f64 * TICK).max(TICK)
}

/// Whole lots that fit in `capital` at `price` per unit.
pub fn lots_for_capital(capital: f64, price: f64, lot_size: i64) -> i64 {
    let cost = price * lot_size as f64;
    if !(capital.is_finite() && cost.is_finite() && cost > 0.0 && lot_size > 0) {
        return 0;
    }
    ((capital / cost) + 1e-9).floor() as i64
}

/// Stop-loss trigger and target prices for a BUY at `entry`, on the tick
/// grid and always on the right side of the entry price. `None` = not set
/// (or no valid price exists, e.g. a stop below one tick).
pub fn bracket_levels(entry: f64, stop_loss_pct: Option<f64>, target_pct: Option<f64>) -> (Option<f64>, Option<f64>) {
    let stop = stop_loss_pct.and_then(|p| {
        let raw = round_to_tick(entry * (1.0 - p / 100.0));
        let s = if raw >= entry { round_to_tick(entry - TICK) } else { raw };
        (s >= TICK && s < entry).then_some(s)
    });
    let target = target_pct.and_then(|p| {
        let raw = round_to_tick(entry * (1.0 + p / 100.0));
        let t = if raw <= entry { round_to_tick(entry + TICK) } else { raw };
        (t > entry).then_some(t)
    });
    (stop, target)
}

/// Does a closed bar exist at the end of `bars`? The last bar is still
/// forming while its start + `step_secs` is in the future.
pub fn closed_count(bars: &[Bar], step_secs: i64, now: i64) -> usize {
    match bars.last() {
        Some(b) if b.ts + step_secs > now => bars.len() - 1,
        _ => bars.len(),
    }
}

/// Minutes interval as seconds (`None` for anything but a valid minute interval).
pub fn interval_secs(interval: &str) -> Option<i64> {
    match resample::parse_interval(interval)? {
        Interval::Min(n) => Some(n * 60),
        Interval::Day => None,
    }
}

// ------------------------------------------------------ account choice ---

/// Which of the algo's accounts place orders themselves.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Selection {
    pub run: Vec<String>,
    /// `(child, master)`: selected children that are left out because their
    /// master places the orders and the trade copier replicates them.
    pub covered: Vec<(String, String)>,
}

/// Trade-copier rules for an algo.
///
/// * Algo copier **on**, copier set-up enabled and its master selected in the
///   algo: the master places the orders (the copier replicates them to its
///   active children) and every selected child is ignored: the copier
///   overrides the algo's own account list.
/// * Otherwise every selected account places its own orders. With the algo
///   copier **off** its orders are never replicated, even from a master
///   (`place_algo(.., propagate = false)`).
pub fn select_accounts(selected: &[String], algo_copier: bool, cfg: &CopierConfig) -> Selection {
    let master = cfg.master_account_id.as_str();
    let override_on = algo_copier && cfg.enabled && !master.is_empty() && selected.iter().any(|a| a == master);
    if !override_on {
        return Selection { run: selected.to_vec(), covered: Vec::new() };
    }
    let kids: HashSet<String> = cfg.active_children().into_iter().map(|c| c.account_id).collect();
    let mut out = Selection::default();
    for a in selected {
        if kids.contains(a) {
            out.covered.push((a.clone(), master.to_string()));
        } else {
            out.run.push(a.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::copier::ChildConfig;

    fn algo() -> Algo {
        Algo {
            name: "A".into(),
            signal_id: "s1".into(),
            action: Action { capital: 15_000.0, ..Action::default() },
            ..Algo::default()
        }
    }

    fn cfg(enabled: bool, master: &str, kids: &[(&str, bool)]) -> CopierConfig {
        CopierConfig {
            enabled,
            master_account_id: master.into(),
            children: kids
                .iter()
                .map(|(id, on)| ChildConfig { account_id: (*id).into(), enabled: *on, ..ChildConfig::default() })
                .collect(),
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn strikes_follow_the_spot() {
        assert_eq!(strike_step("nifty"), 50.0);
        assert_eq!(strike_step("BANKNIFTY"), 100.0);
        assert_eq!(strike_step("MIDCPNIFTY"), 25.0);
        assert_eq!(atm_strike(24_037.0, 50.0), 24_050.0);
        assert_eq!(atm_strike(24_024.0, 50.0), 24_000.0);
        assert_eq!(atm_strike(24_025.0, 50.0), 24_050.0); // tie goes up
        assert_eq!(target_strike(24_037.0, 50.0, 0), 24_050.0);
        assert_eq!(target_strike(24_037.0, 50.0, 2), 24_150.0);
        assert_eq!(target_strike(24_037.0, 50.0, -1), 24_000.0);
        assert_eq!(atm_strike(24_000.0, 0.0), 0.0);
    }

    #[test]
    fn expiry_selection_skips_the_past() {
        let e = ids(&["2026-11-17", "2026-11-10", "2026-11-24", "2026-12-01", "2026-12-08", "2026-12-15", "2026-11-03"]);
        assert_eq!(pick_expiry(ExpirySel::Current, &e, "2026-11-09").as_deref(), Some("2026-11-10"));
        assert_eq!(pick_expiry(ExpirySel::Next, &e, "2026-11-09").as_deref(), Some("2026-11-17"));
        assert_eq!(pick_expiry(ExpirySel::Next3, &e, "2026-11-09").as_deref(), Some("2026-12-08"));
        // Expiry day itself still counts as the current expiry.
        assert_eq!(pick_expiry(ExpirySel::Current, &e, "2026-11-10").as_deref(), Some("2026-11-10"));
        assert_eq!(pick_expiry(ExpirySel::Next3, &e[..2], "2026-11-09"), None);
    }

    #[test]
    fn capital_decides_the_lots() {
        // 15k at 100 x 65 = 6500 per lot: 2 lots (13k), as in the spec.
        assert_eq!(lots_for_capital(15_000.0, 100.0, 65), 2);
        assert_eq!(lots_for_capital(6_500.0, 100.0, 65), 1); // exact fit, no float loss
        assert_eq!(lots_for_capital(6_499.0, 100.0, 65), 0);
        assert_eq!(lots_for_capital(15_000.0, 0.0, 65), 0);
        assert_eq!(lots_for_capital(15_000.0, 100.0, 0), 0);
    }

    #[test]
    fn entry_limit_adds_the_buffer() {
        assert!((entry_limit(100.0, 2) - 100.10).abs() < 1e-9);
        assert!((entry_limit(100.02, 0) - 100.0).abs() < 1e-9);
        assert!((entry_limit(0.0, 0) - TICK).abs() < 1e-9);
    }

    #[test]
    fn bracket_levels_stay_on_the_right_side() {
        let (s, t) = bracket_levels(100.0, Some(20.0), Some(30.0));
        assert_eq!((s, t), (Some(80.0), Some(130.0)));
        // Tiny percentages still move one tick away.
        let (s, t) = bracket_levels(100.0, Some(0.01), Some(0.01));
        assert_eq!((s, t), (Some(99.95), Some(100.05)));
        // No stop is possible below one tick.
        assert_eq!(bracket_levels(0.05, Some(50.0), None), (None, None));
        assert_eq!(bracket_levels(100.0, None, None), (None, None));
    }

    #[test]
    fn closed_bars_exclude_the_forming_one() {
        let b = |ts| Bar::new(ts, 1.0);
        let bars = vec![b(0), b(300), b(600)];
        assert_eq!(closed_count(&bars, 300, 700), 2); // 600 + 300 > 700: forming
        assert_eq!(closed_count(&bars, 300, 900), 3);
        assert_eq!(closed_count(&[], 300, 900), 0);
        assert_eq!(interval_secs("5m"), Some(300));
        assert_eq!(interval_secs("1d"), None);
        assert_eq!(interval_secs("x"), None);
    }

    #[test]
    fn copier_on_ignores_selected_children_of_a_selected_master() {
        let c = cfg(true, "m", &[("c1", true), ("c2", false)]);
        let s = select_accounts(&ids(&["m", "c1", "c2", "x"]), true, &c);
        // c1 is covered by the master; c2 is a disabled child (gets no copies), so it trades itself.
        assert_eq!(s.run, ids(&["m", "c2", "x"]));
        assert_eq!(s.covered, vec![("c1".to_string(), "m".to_string())]);
    }

    #[test]
    fn copier_rules_do_not_apply_without_all_three_conditions() {
        let c = cfg(true, "m", &[("c1", true)]);
        let all = ids(&["m", "c1"]);
        // Algo copier off: everyone places their own orders.
        assert_eq!(select_accounts(&all, false, &c).run, all);
        // Copier set-up disabled: nothing is replicated, so children are not ignored.
        assert_eq!(select_accounts(&all, true, &cfg(false, "m", &[("c1", true)])).run, all);
        // Master not in the algo: the child trades itself.
        assert_eq!(select_accounts(&ids(&["c1"]), true, &c).run, ids(&["c1"]));
        assert!(select_accounts(&all, true, &cfg(true, "", &[])).covered.is_empty());
    }

    #[test]
    fn validation_reports_every_problem() {
        assert!(algo().validate().is_empty());
        let mut a = algo();
        a.name = " ".into();
        a.signal_id.clear();
        a.interval = "1d".into();
        a.action.capital = 0.0;
        a.action.product = "CNC".into();
        a.action.stop_loss_pct = Some(100.0);
        a.action.target_pct = Some(0.0);
        a.instrument.strike_offset = 21;
        a.instrument.underlying = "NI FTY".into();
        a.instrument.kind = "stock".into();
        assert_eq!(a.validate().len(), 10);
        let mut b = algo();
        b.interval = "abc".into();
        assert!(b.validate().iter().any(|e| e.contains("minutes")));
    }

    #[test]
    fn normalize_cleans_inputs() {
        let mut a = algo();
        a.name = "  X ".into();
        a.instrument.underlying = " nifty ".into();
        a.interval = " 5M ".into();
        a.action.product = "intraday".into();
        a.accounts = ids(&[" a ", "a", "", "b"]);
        a.normalize();
        assert_eq!((a.name.as_str(), a.instrument.underlying.as_str(), a.interval.as_str()), ("X", "NIFTY", "5m"));
        assert_eq!(a.action.product, "INTRADAY");
        assert_eq!(a.accounts, ids(&["a", "b"]));
    }

    #[test]
    fn json_roundtrip_and_defaults() {
        let json = r#"{
          "name":"Nifty ATM",
          "signalId":"s1",
          "instrument":{"type":"option","underlying":"NIFTY","expiry":"next1","strikeOffset":-2,"side":"both"},
          "interval":"15m",
          "action":{"product":"MARGIN","capital":50000,"bufferTicks":3,"stopLossPct":25,"targetPct":50},
          "accounts":["a","b"],
          "copier":true
        }"#;
        let a: Algo = serde_json::from_str(json).unwrap();
        assert_eq!(a.instrument.expiry, ExpirySel::Next1);
        assert_eq!(a.instrument.side, SideSel::Both);
        assert_eq!(a.instrument.strike_offset, -2);
        assert!(!a.allow_live);
        assert!(a.validate().is_empty(), "{:?}", a.validate());
        let again: Algo = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(a, again);
        // Missing sections fall back to defaults.
        let min: Algo = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert_eq!(min.interval, "5m");
        assert_eq!(SideSel::Both.legs(), vec![true, false]);
    }
}
