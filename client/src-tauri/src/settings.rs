//! App folders and non-secret runtime configuration. Secrets never live
//! here — see `secrets.rs` (OS keychain).

use serde::{Deserialize, Serialize};
use std::{env, fs, io, path::PathBuf};
use tracing::{error, info};

pub const APP_NAME: &str = "kissa";

pub struct AppPaths {
    pub root: PathBuf,
    pub db_file: PathBuf,
    pub settings_file: PathBuf,
}

pub fn setup_app_folders() -> io::Result<AppPaths> {
    let base = dirs::data_local_dir()
        .or_else(|| env::current_dir().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no storage location"))?;
    let root = base.join(format!(".{}", APP_NAME));
    fs::create_dir_all(&root)?;
    Ok(AppPaths {
        db_file: root.join("kissa.db"),
        settings_file: root.join("settings.json"),
        root,
    })
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ApiEndpoint {
    pub base: String,
    pub desc: String,
}

fn ep(base: &str, desc: &str) -> ApiEndpoint {
    ApiEndpoint { base: base.into(), desc: desc.into() }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SystemConfig {
    pub indices_streamer: ApiEndpoint,
    pub option_ticks: ApiEndpoint,
    pub indices_info: ApiEndpoint,
    pub option_info: ApiEndpoint,
    pub current_time: ApiEndpoint,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            indices_streamer: ep(
                "wss://streamer.nseindia.com/streams/indices/high/windices",
                "Real-time index stream",
            ),
            option_ticks: ep(
                "wss://streamer.nseindia.com/streams/fo/mbp",
                "CE/PE price stream (symbol + expiry appended)",
            ),
            indices_info: ep(
                "https://www.nseindia.com/api/NextApi/apiClient/indexTrackerApi?functionName=getAllIndices",
                "F&O index names",
            ),
            option_info: ep(
                "https://www.nseindia.com/api/option-chain-contract-info",
                "Expiry dates per symbol",
            ),
            current_time: ep(
                "https://www.nseindia.com/api/NextApi/dynamicApi?functionName=getCurrentTime",
                "NSE IST server time",
            ),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct MarketRuntimeConfig {
    pub reconnect_delay_seconds: u64,
    /// Seconds without a real tick inside the trading window before the
    /// streamers fall back to Idle (polling) mode.
    pub inactive_switch_after_secs: i64,
    /// Seconds between idle polls (a short connect-and-listen).
    pub idle_poll_interval_secs: u64,
    pub idle_poll_listen_secs: u64,
}

impl Default for MarketRuntimeConfig {
    fn default() -> Self {
        Self {
            reconnect_delay_seconds: 5,
            inactive_switch_after_secs: 3600,
            idle_poll_interval_secs: 120,
            idle_poll_listen_secs: 15,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct FyersConfig {
    /// Option chain endpoint. Confirm against Fyers' API reference if it changes.
    pub chain_url: String,
    /// Seconds between chain polls (mind your plan's rate limits).
    pub poll_secs: u64,
    /// Strikes each side of ATM requested from Fyers (max 50).
    pub strike_count: u32,
}

impl Default for FyersConfig {
    fn default() -> Self {
        Self {
            chain_url: "https://api-t1.fyers.in/data/options-chain-v3".into(),
            poll_secs: 5,
            strike_count: 25,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    pub system: SystemConfig,
    pub market_runtime: MarketRuntimeConfig,
    pub fyers: FyersConfig,
    /// Annualised risk-free rate used for Greeks/IV (decimal, 0.07 = 7%).
    pub risk_free_rate: f64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            system: SystemConfig::default(),
            market_runtime: MarketRuntimeConfig::default(),
            fyers: FyersConfig::default(),
            risk_free_rate: 0.07,
        }
    }
}

pub fn load_or_create_config(paths: &AppPaths) -> io::Result<AppConfig> {
    if paths.settings_file.exists() {
        match fs::read_to_string(&paths.settings_file)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<AppConfig>(&s).map_err(|e| e.to_string()))
        {
            Ok(c) => {
                info!("Loaded configuration from {}", paths.settings_file.display());
                return Ok(c);
            }
            Err(e) => error!("Could not load settings.json ({e}); using defaults"),
        }
    }
    let cfg = AppConfig::default();
    let json = serde_json::to_string_pretty(&cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    fs::write(&paths.settings_file, json)?;
    Ok(cfg)
}
