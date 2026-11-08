use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::alerts::AlertService;
use crate::chain::runner::ChainHandle;
use crate::charts::recorder::Recorder;
use crate::market::events::IndexStore;
use crate::market::market_clock::SharedSessionState;
use crate::market::symbols::FnoIndex;
use crate::settings::{AppConfig, AppPaths};
use crate::stats::SharedStats;
use crate::trading::ltp::LtpStore;
use crate::trading::service::TradingService;

pub struct AppState {
    pub config: AppConfig,
    pub paths: AppPaths,
    pub db: SqlitePool,
    pub stats: SharedStats,
    pub session: SharedSessionState,
    pub indices: IndexStore,
    pub fno: RwLock<Vec<FnoIndex>>,
    pub chain: Mutex<Option<ChainHandle>>,
    pub recorder: Arc<Recorder>,
    /// Latest option prices from whichever stream is running.
    pub ltp: Arc<LtpStore>,
    pub trading: Arc<TradingService>,
    pub alerts: Arc<AlertService>,
}

pub type SharedState = Arc<AppState>;
