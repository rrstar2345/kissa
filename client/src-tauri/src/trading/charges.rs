//! Estimated trading charges for NSE index options (per fill).
//!
//! * brokerage: flat per executed order (charged on an order's first fill)
//! * STT: on the premium of options sold
//! * exchange transaction charge and SEBI fee: both sides, on premium turnover
//! * stamp duty: buy side
//! * GST: on brokerage + exchange charge + SEBI fee
//!
//! Rates live in `settings.json` (`trading.charges`) and are estimates.

use serde::Serialize;

use crate::settings::ChargesConfig;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Charges {
    pub brokerage: f64,
    pub stt: f64,
    pub exchange: f64,
    pub sebi: f64,
    pub stamp: f64,
    pub gst: f64,
    pub total: f64,
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// `turnover` = fill price x quantity (premium value).
pub fn compute(cfg: &ChargesConfig, brokerage: f64, is_sell: bool, turnover: f64) -> Charges {
    let pct = |p: f64| turnover * p / 100.0;
    let stt = if is_sell { pct(cfg.stt_sell_pct) } else { 0.0 };
    let exchange = pct(cfg.exchange_txn_pct);
    let sebi = pct(cfg.sebi_pct);
    let stamp = if is_sell { 0.0 } else { pct(cfg.stamp_buy_pct) };
    let gst = (brokerage + exchange + sebi) * cfg.gst_pct / 100.0;
    let (brokerage, stt, exchange, sebi, stamp, gst) =
        (r2(brokerage), r2(stt), r2(exchange), r2(sebi), r2(stamp), r2(gst));
    Charges { brokerage, stt, exchange, sebi, stamp, gst, total: r2(brokerage + stt + exchange + sebi + stamp + gst) }
}

/// Dhan charges the lower of a flat fee and a % of turnover.
pub fn dhan_brokerage(flat: f64, cap_pct: f64, turnover: f64) -> f64 {
    flat.min(turnover * cap_pct / 100.0).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sell_side_carries_stt() {
        let c = compute(&ChargesConfig::default(), 20.0, true, 100_000.0);
        assert!((c.stt - 150.0).abs() < 0.011);
        assert!((c.exchange - 35.03).abs() < 0.011);
        assert_eq!(c.stamp, 0.0);
        assert!((c.gst - 9.92).abs() < 0.011);
        assert!((c.total - 215.05).abs() < 0.03);
    }

    #[test]
    fn buy_side_carries_stamp_duty() {
        let c = compute(&ChargesConfig::default(), 20.0, false, 100_000.0);
        assert_eq!(c.stt, 0.0);
        assert!((c.stamp - 3.0).abs() < 0.011);
        assert!((c.total - 68.05).abs() < 0.03);
    }

    #[test]
    fn dhan_brokerage_is_capped_by_turnover() {
        assert_eq!(dhan_brokerage(20.0, 0.03, 1_000_000.0), 20.0);
        assert!((dhan_brokerage(20.0, 0.03, 10_000.0) - 3.0).abs() < 1e-9);
    }
}
