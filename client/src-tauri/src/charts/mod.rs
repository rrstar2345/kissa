//! Charts backend: bar storage, live recording, resampling, indicators and
//! Fyers historical backfill.

pub mod backfill;
pub mod indicators;
pub mod instrument;
pub mod recorder;
pub mod resample;
pub mod rules;
pub mod store;

use serde::{Deserialize, Serialize};

/// One OHLCV bar. `ts` is the bucket start in epoch seconds (UTC).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Bar {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub oi: Option<f64>,
}

impl Bar {
    pub fn new(ts: i64, px: f64) -> Self {
        Self { ts, open: px, high: px, low: px, close: px, volume: 0.0, oi: None }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartRequest {
    pub instrument: String,
    pub interval: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub indicators: Vec<indicators::IndicatorSpec>,
    /// Saved rulesets (by id) whose signals and plotted variables to add.
    #[serde(default)]
    pub rulesets: Vec<String>,
    /// An unsaved ruleset to preview (the Signals editor).
    #[serde(default)]
    pub draft: Option<rules::Ruleset>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartResponse {
    pub bars: Vec<Bar>,
    pub lines: Vec<indicators::Line>,
    pub signals: Vec<indicators::Signal>,
    /// Problems with a selected/draft ruleset (it is skipped).
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionExpiry {
    pub symbol: String,
    pub expiry: String,
    pub strikes: Vec<f64>,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentList {
    pub indices: Vec<String>,
    pub options: Vec<OptionExpiry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillResult {
    pub symbol: String,
    pub fetched: usize,
    pub inserted: usize,
}
