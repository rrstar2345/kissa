//! Aggregates per-strike NSE ticks into a full chain and derives IV/Greeks.

use chrono::{NaiveDate, TimeZone, Utc};
use std::collections::{BTreeMap, HashMap};

use super::greeks::{self, Greeks};
use super::{pct, ChainSnapshot, Leg, StrikeRow};
use crate::market::events::{IndexTick, LegTick, OptionTick};
use crate::market::market_clock;

const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;

fn key(strike: f64) -> i64 {
    (strike * 100.0).round() as i64
}

/// Years until 15:30 IST on `expiry`, measured from NSE-synced "now".
pub fn years_to_expiry(expiry: NaiveDate) -> f64 {
    let close_ist = expiry.and_hms_opt(15, 30, 0).unwrap();
    let close_utc = Utc.from_utc_datetime(&(close_ist - chrono::Duration::seconds(IST_OFFSET_SECS)));
    let secs = (close_utc - market_clock::get_ist_now()).num_seconds() as f64;
    (secs / (365.0 * 24.0 * 3600.0)).max(0.0)
}

pub struct ChainEngine {
    symbol: String,
    expiry_label: String,
    expiry: NaiveDate,
    rate: f64,
    rows: BTreeMap<i64, StrikeRow>,
    /// First OI seen per (strike, is_call) this session; used when NSE
    /// doesn't send an OI change.
    oi_base: HashMap<(i64, bool), f64>,
}

impl ChainEngine {
    pub fn new(symbol: String, expiry_label: String, expiry: NaiveDate, rate: f64) -> Self {
        Self { symbol, expiry_label, expiry, rate, rows: BTreeMap::new(), oi_base: HashMap::new() }
    }

    pub fn apply(&mut self, tick: OptionTick) {
        let k = key(tick.strike);
        let row = self
            .rows
            .entry(k)
            .or_insert_with(|| StrikeRow { strike: tick.strike, ..Default::default() });
        for (is_call, leg_tick, leg) in [(true, &tick.ce, &mut row.ce), (false, &tick.pe, &mut row.pe)] {
            merge_leg(leg, leg_tick);
            if let Some(oi) = leg_tick.oi {
                let base = *self.oi_base.entry((k, is_call)).or_insert(oi);
                leg.oi_change = leg_tick.oi_change.or(Some(oi - base));
            }
        }
    }

    /// Strike range (lo, hi) covering `n` known strikes either side of the
    /// one nearest `spot`; used to limit which strikes are stored.
    pub fn strike_window(&self, spot: f64, n: usize) -> Option<(f64, f64)> {
        let strikes: Vec<f64> = self.rows.values().map(|r| r.strike).collect();
        let i = strikes
            .iter()
            .enumerate()
            .min_by(|a, b| (a.1 - spot).abs().partial_cmp(&(b.1 - spot).abs()).unwrap())?
            .0;
        Some((strikes[i.saturating_sub(n)], strikes[(i + n).min(strikes.len() - 1)]))
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn snapshot(&self, spot: Option<&IndexTick>, vix: Option<f64>) -> ChainSnapshot {
        let t = years_to_expiry(self.expiry);
        let mut rows: Vec<StrikeRow> = self.rows.values().cloned().collect();

        // Reference price for ATM / forward: index spot if known, else the
        // strike where |CE - PE| is smallest.
        let spot_px = spot.map(|s| s.current_price);
        let ref_px = spot_px.or_else(|| parity_strike(&rows));
        let atm = ref_px.and_then(|p| nearest_strike(&rows, p));

        // Forward from put-call parity at ATM, falling back to S·e^{rT}.
        let forward = atm
            .and_then(|a| rows.iter().find(|r| (r.strike - a).abs() < 1e-6))
            .and_then(|r| match (r.ce.ltp, r.pe.ltp) {
                (Some(c), Some(p)) if c > 0.0 && p > 0.0 => {
                    Some(r.strike + (c - p) * (self.rate * t).exp())
                }
                _ => None,
            })
            .or_else(|| spot_px.map(|s| s * (self.rate * t).exp()));

        if let Some(f) = forward {
            for r in rows.iter_mut() {
                for (is_call, leg) in [(true, &mut r.ce), (false, &mut r.pe)] {
                    if let Some(px) = leg.ltp.filter(|p| *p > 0.0) {
                        if let Some(Greeks { iv, delta, gamma, theta, vega }) =
                            greeks::greeks(is_call, px, f, r.strike, t, self.rate)
                        {
                            leg.iv = Some(iv);
                            leg.delta = Some(delta);
                            leg.gamma = Some(gamma);
                            leg.theta = Some(theta);
                            leg.vega = Some(vega);
                        }
                    }
                }
            }
        }

        let call_oi: f64 = rows.iter().filter_map(|r| r.ce.oi).sum();
        let put_oi: f64 = rows.iter().filter_map(|r| r.pe.oi).sum();

        ChainSnapshot {
            source: "nse".into(),
            symbol: self.symbol.clone(),
            expiry: self.expiry_label.clone(),
            underlying_name: spot.map(|s| s.index_name.clone()),
            underlying: spot_px,
            underlying_change: spot.map(|s| s.change),
            underlying_change_pct: spot.map(|s| s.per_change),
            vix,
            atm,
            call_oi,
            put_oi,
            pcr: (call_oi > 0.0).then(|| put_oi / call_oi),
            greeks_source: "computed".into(),
            updated_at: Utc::now().to_rfc3339(),
            rows,
        }
    }
}

fn merge_leg(leg: &mut Leg, t: &LegTick) {
    // NSE nulls mean "no update"; keep the last known value.
    if t.ltp.is_some() {
        leg.ltp = t.ltp;
    }
    if t.change.is_some() {
        leg.change = t.change;
    }
    leg.change_pct = pct(leg.change, leg.ltp);
    if t.oi.is_some() {
        leg.oi = t.oi;
    }
    if t.volume.is_some() {
        leg.volume = t.volume;
    }
    if t.bid.is_some() {
        leg.bid = t.bid;
    }
    if t.ask.is_some() {
        leg.ask = t.ask;
    }
}

fn nearest_strike(rows: &[StrikeRow], px: f64) -> Option<f64> {
    rows.iter()
        .map(|r| r.strike)
        .min_by(|a, b| (a - px).abs().partial_cmp(&(b - px).abs()).unwrap())
}

fn parity_strike(rows: &[StrikeRow]) -> Option<f64> {
    rows.iter()
        .filter_map(|r| Some((r.strike, (r.ce.ltp? - r.pe.ltp?).abs())))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(k, _)| k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(strike: f64, ce: f64, pe: f64, ce_oi: f64) -> OptionTick {
        OptionTick {
            strike,
            ce: LegTick { ltp: Some(ce), oi: Some(ce_oi), ..Default::default() },
            pe: LegTick { ltp: Some(pe), ..Default::default() },
        }
    }

    #[test]
    fn strike_window_covers_n_each_side() {
        let exp = Utc::now().date_naive() + chrono::Duration::days(10);
        let mut e = ChainEngine::new("NIFTY".into(), "x".into(), exp, 0.07);
        for k in 0..10 {
            e.apply(tick(24000.0 + k as f64 * 50.0, 1.0, 1.0, 1.0));
        }
        assert_eq!(e.strike_window(24230.0, 2), Some((24150.0, 24350.0)));
        assert_eq!(e.strike_window(10.0, 2), Some((24000.0, 24100.0)));
    }

    #[test]
    fn aggregates_and_derives_oi_change() {
        let exp = Utc::now().date_naive() + chrono::Duration::days(10);
        let mut e = ChainEngine::new("NIFTY".into(), "x".into(), exp, 0.07);
        e.apply(tick(24000.0, 150.0, 140.0, 1000.0));
        e.apply(tick(24000.0, 155.0, 138.0, 1300.0));
        e.apply(OptionTick { strike: 24000.0, ce: LegTick::default(), pe: LegTick::default() });
        let snap = e.snapshot(None, None);
        assert_eq!(snap.rows.len(), 1);
        let ce = &snap.rows[0].ce;
        assert_eq!(ce.ltp, Some(155.0)); // null update keeps last value
        assert_eq!(ce.oi_change, Some(300.0));
        assert_eq!(snap.atm, Some(24000.0));
        assert!(ce.iv.is_some() || ce.ltp.is_some());
    }
}
