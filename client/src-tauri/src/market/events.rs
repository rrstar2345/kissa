//! Tick types shared by the streamers, the chain engine and the frontend.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// One message from the NSE indices stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexTick {
    pub time: String,
    pub index_name: String,
    pub current_price: f64,
    pub change: f64,
    pub per_change: f64,
    pub previous_close: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub mkt_status: String,
}

/// One option leg as received from NSE. All fields optional: NSE sends
/// nulls for strikes with no activity.
#[derive(Debug, Clone, Default)]
pub struct LegTick {
    pub ltp: Option<f64>,
    pub change: Option<f64>,
    pub volume: Option<f64>,
    pub oi: Option<f64>,
    /// Only present if NSE supplies it; otherwise the chain engine derives
    /// a session baseline.
    pub oi_change: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct OptionTick {
    pub strike: f64,
    pub ce: LegTick,
    pub pe: LegTick,
}

/// Latest tick per index name, shared between the indices streamer (writer)
/// and the chain engine / UI (readers).
pub type IndexStore = Arc<RwLock<HashMap<String, IndexTick>>>;

pub fn new_index_store() -> IndexStore {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Callback invoked for each real (non-heartbeat) index tick.
pub type IndexSink = Arc<dyn Fn(IndexTick) + Send + Sync>;
