//! Option chain model, shared by the NSE-stream engine and the Fyers poller.

pub mod engine;
pub mod fyers;
pub mod greeks;
pub mod runner;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Leg {
    pub ltp: Option<f64>,
    pub change: Option<f64>,
    pub change_pct: Option<f64>,
    pub oi: Option<f64>,
    pub oi_change: Option<f64>,
    pub volume: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub iv: Option<f64>,
    pub delta: Option<f64>,
    pub gamma: Option<f64>,
    pub theta: Option<f64>,
    pub vega: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrikeRow {
    pub strike: f64,
    pub ce: Leg,
    pub pe: Leg,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainSnapshot {
    pub source: String,
    pub symbol: String,
    pub expiry: String,
    pub underlying_name: Option<String>,
    pub underlying: Option<f64>,
    pub underlying_change: Option<f64>,
    pub underlying_change_pct: Option<f64>,
    pub vix: Option<f64>,
    pub atm: Option<f64>,
    pub call_oi: f64,
    pub put_oi: f64,
    pub pcr: Option<f64>,
    /// "computed" (Black-76 from LTP) or "broker" (supplied by Fyers).
    pub greeks_source: String,
    pub updated_at: String,
    pub rows: Vec<StrikeRow>,
}

/// An expiry choice for the UI. `value` is what the source expects back in
/// `start_chain`; `date` is ISO `YYYY-MM-DD` for time-to-expiry maths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpiryItem {
    pub value: String,
    pub label: String,
    pub date: String,
}

pub fn pct(change: Option<f64>, ltp: Option<f64>) -> Option<f64> {
    match (change, ltp) {
        (Some(c), Some(l)) if (l - c).abs() > f64::EPSILON => Some(c / (l - c) * 100.0),
        _ => None,
    }
}
