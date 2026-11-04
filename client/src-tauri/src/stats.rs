//! Shared in-memory stream stats (ported from kstocks_egui, DB stats dropped).

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use chrono::{DateTime, Local};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnState {
    Connecting,
    Connected,
    Reconnecting,
    Idle,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStat {
    pub name: String,
    pub state: ConnState,
    pub ticks_received: u64,
    pub last_tick_at: Option<DateTime<Local>>,
    pub last_error: Option<String>,
    pub reconnect_count: u64,
}

impl StreamStat {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: ConnState::Connecting,
            ticks_received: 0,
            last_tick_at: None,
            last_error: None,
            reconnect_count: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct AppStats {
    pub streams: HashMap<String, StreamStat>,
}

pub type SharedStats = Arc<RwLock<AppStats>>;

pub fn new_shared_stats() -> SharedStats {
    Arc::new(RwLock::new(AppStats::default()))
}

impl AppStats {
    pub fn ensure_stream(&mut self, name: &str) -> &mut StreamStat {
        self.streams
            .entry(name.to_string())
            .or_insert_with(|| StreamStat::new(name))
    }

    pub fn remove_stream(&mut self, name: &str) {
        self.streams.remove(name);
    }
}
