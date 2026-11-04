use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::chain::runner::ChainHandle;
use crate::market::events::IndexStore;
use crate::market::market_clock::SharedSessionState;
use crate::market::symbols::FnoIndex;
use crate::settings::AppConfig;
use crate::stats::SharedStats;

pub struct AppState {
    pub config: AppConfig,
    pub db: SqlitePool,
    pub stats: SharedStats,
    pub session: SharedSessionState,
    pub indices: IndexStore,
    pub fno: RwLock<Vec<FnoIndex>>,
    pub chain: Mutex<Option<ChainHandle>>,
}

pub type SharedState = Arc<AppState>;
