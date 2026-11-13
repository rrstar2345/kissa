//! App folders and non-secret runtime configuration. Secrets never live
//! here — see `secrets.rs` (OS keychain).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{env, fs, io, path::PathBuf};
use tracing::{error, info};

pub const APP_NAME: &str = "kissa";

#[derive(Clone)]
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
#[serde(default)]
pub struct SystemConfig {
    pub indices_streamer: ApiEndpoint,
    pub option_ticks: ApiEndpoint,
    pub indices_info: ApiEndpoint,
    pub option_info: ApiEndpoint,
    pub current_time: ApiEndpoint,
    pub market_status: ApiEndpoint,
    pub holidays: ApiEndpoint,
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
            market_status: ep(
                "https://www.nseindia.com/api/marketStatus",
                "NSE market open/closed state (Capital Market row; F&O follows it)",
            ),
            holidays: ep(
                "https://www.nseindia.com/api/holiday-master?type=trading",
                "NSE trading holidays (FO array); not used yet",
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
    /// Login page the browser is sent to (https://fyers.in/api-docs/v3/api/authentication/individual-app/generate-auth-code/).
    pub auth_url: String,
    /// Exchanges the auth code for an access token (.../validate-auth-code/).
    pub validate_url: String,
    /// Option chain endpoint. Confirm against Fyers' API reference if it changes.
    pub chain_url: String,
    /// Historical candles endpoint (not yet verified against live Fyers).
    pub history_url: String,
    /// Exchange open/closed state (https://fyers.in/api-docs/v3/api/market-information/market-status/).
    pub status_url: String,
    /// Seconds between chain polls (mind your plan's rate limits).
    pub poll_secs: u64,
    /// Strikes each side of ATM requested from Fyers (max 50).
    pub strike_count: u32,
    /// Stream prices over the Fyers data socket. Off = REST polling only.
    pub use_socket: bool,
    /// Data socket URL (binary HSM protocol).
    pub socket_url: String,
    /// Symbol to token lookup, used when the chain response has no `fyToken`.
    pub symbol_token_url: String,
    /// Most symbols subscribed at once: 100 on the Standard plan, 5000 on Prime.
    pub max_symbols: usize,
    /// While the socket streams, seconds between chain calls that refresh the
    /// strike list and open interest.
    pub chain_refresh_secs: u64,
}

impl Default for FyersConfig {
    fn default() -> Self {
        Self {
            auth_url: "https://api-t1.fyers.in/api/v3/generate-authcode".into(),
            validate_url: "https://api-t1.fyers.in/api/v3/validate-authcode".into(),
            chain_url: "https://api-t1.fyers.in/data/options-chain-v3".into(),
            history_url: "https://api-t1.fyers.in/data/history".into(),
            status_url: "https://api-t1.fyers.in/data/marketStatus".into(),
            poll_secs: 5,
            strike_count: 25,
            use_socket: true,
            socket_url: "wss://socket.fyers.in/hsm/v1-5/prod".into(),
            symbol_token_url: "https://api-t1.fyers.in/data/symbol-token".into(),
            max_symbols: 100,
            chain_refresh_secs: 60,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ChartsConfig {
    /// Option strikes each side of ATM whose ticks are stored for charts.
    pub persist_strikes_each_side: usize,
    /// Seconds between DB flushes of in-memory 1m bars.
    pub flush_secs: u64,
    /// Days of 1m bars kept (daily bars are kept indefinitely).
    pub bars_1m_keep_days: i64,
    /// Days after expiry before an option's bars are deleted.
    pub option_expiry_grace_days: i64,
}

impl Default for ChartsConfig {
    fn default() -> Self {
        Self { persist_strikes_each_side: 20, flush_secs: 5, bars_1m_keep_days: 120, option_expiry_grace_days: 7 }
    }
}

/// Statutory charges for index options, as percentages (0.15 = 0.15 %).
/// These are estimates: check them against your broker's contract note.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ChargesConfig {
    /// STT on the premium of options *sold*.
    pub stt_sell_pct: f64,
    /// NSE transaction charge on premium turnover (both sides).
    pub exchange_txn_pct: f64,
    /// SEBI turnover fee (both sides).
    pub sebi_pct: f64,
    /// Stamp duty on the *buy* side.
    pub stamp_buy_pct: f64,
    /// GST on brokerage + exchange charge + SEBI fee.
    pub gst_pct: f64,
}

impl Default for ChargesConfig {
    fn default() -> Self {
        Self { stt_sell_pct: 0.15, exchange_txn_pct: 0.03503, sebi_pct: 0.0001, stamp_buy_pct: 0.003, gst_pct: 18.0 }
    }
}

/// How the paper broker imitates a real one.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct PaperConfig {
    /// Reject orders the account cannot fund (needs `startingCapital` on the
    /// paper account; without it there is no limit).
    pub enforce_margin: bool,
    /// Margin blocked for a short option, as % of strike x quantity.
    /// A rough stand-in for SPAN + exposure margin.
    pub short_margin_pct: f64,
    /// Adverse price slip on fills, in 0.05 ticks (0 = fill at the last price).
    pub slippage_ticks: u32,
    /// Delay before the paper broker accepts an order (milliseconds).
    pub latency_ms: u64,
    /// Close INTRADAY positions and cancel their orders at the cut-off, as
    /// real brokers do. Ignored when `trading.paper_allow_off_hours` is on.
    pub auto_square_off: bool,
    /// Cut-off, IST, `HH:MM`. New INTRADAY orders are refused from this time.
    pub square_off_time: String,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            enforce_margin: true,
            short_margin_pct: 10.0,
            slippage_ticks: 0,
            latency_ms: 0,
            auto_square_off: true,
            square_off_time: "15:20".into(),
        }
    }
}

impl PaperConfig {
    /// Slippage in rupees.
    pub fn slippage(&self) -> f64 {
        self.slippage_ticks as f64 * 0.05
    }

    /// Cut-off as minutes since midnight IST (falls back to 15:20 if the
    /// text is not `HH:MM`).
    pub fn cutoff_minutes(&self) -> u32 {
        let parsed = self.square_off_time.trim().split_once(':').and_then(|(h, m)| {
            let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
            (h < 24 && m < 60).then_some(h * 60 + m)
        });
        parsed.unwrap_or(15 * 60 + 20)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct TradingConfig {
    /// Let paper orders be placed and filled outside 09:15-15:30 IST (fills
    /// use the last known price). Off by default so paper behaves like the market.
    pub paper_allow_off_hours: bool,
    /// Flat brokerage per executed Dhan order (rupees).
    pub dhan_brokerage_per_order: f64,
    /// Dhan brokerage is the lower of the flat fee and this % of turnover.
    pub dhan_brokerage_cap_pct: f64,
    /// Dhan instrument list (CSV) used to find a contract's securityId and lot size.
    pub scrip_master_url: String,
    /// Re-download the instrument list when older than this many hours.
    pub scrip_refresh_hours: i64,
    /// HTTP timeout for broker-proxy calls.
    pub request_timeout_secs: u64,
    /// Used for paper trading only when the instrument list has no lot size.
    pub fallback_lot_sizes: HashMap<String, i64>,
    /// Exit / Exit All use a LIMIT this many ticks (0.05) beyond the last
    /// price, so the order is marketable without being a market order.
    pub exit_buffer_ticks: u32,
    pub charges: ChargesConfig,
    pub paper: PaperConfig,
}

impl TradingConfig {
    /// Paper auto square-off cut-off (minutes since midnight IST), or `None`
    /// when the feature is off.
    pub fn square_off_minutes(&self) -> Option<u32> {
        (self.paper.auto_square_off && !self.paper_allow_off_hours).then(|| self.paper.cutoff_minutes())
    }
}

impl Default for TradingConfig {
    fn default() -> Self {
        let fallback_lot_sizes = [("NIFTY", 65), ("BANKNIFTY", 30), ("FINNIFTY", 60), ("MIDCPNIFTY", 120), ("NIFTYNXT50", 25)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        Self {
            paper_allow_off_hours: false,
            dhan_brokerage_per_order: 20.0,
            dhan_brokerage_cap_pct: 0.03,
            scrip_master_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv".into(),
            scrip_refresh_hours: 24,
            request_timeout_secs: 30,
            fallback_lot_sizes,
            exit_buffer_ticks: 2,
            charges: ChargesConfig::default(),
            paper: PaperConfig::default(),
        }
    }
}

/// The user's broker-proxy server. Every order, paper included, requires the
/// server to accept the API key. The API key itself lives in the OS keychain
/// (account id `server`, field `apiKey`), never in this file.
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
#[serde(default)]
pub struct ServerConfig {
    /// e.g. `https://proxy.example.com` (a port may be included here).
    pub url: String,
    /// Appended to `url` when it has no port of its own. 0 = none.
    pub port: u16,
}

impl ServerConfig {
    pub fn is_set(&self) -> bool {
        !self.url.trim().is_empty()
    }

    /// Check the URL and return the full base URL (scheme, host, port).
    pub fn base_url(&self) -> Result<String, String> {
        let url = self.url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return Err("Server URL is required".into());
        }
        let lower = url.to_lowercase();
        let rest = if let Some(r) = lower.strip_prefix("https://") {
            r
        } else if lower.strip_prefix("http://").is_some() {
            let local = lower.contains("://localhost") || lower.contains("://127.0.0.1");
            if !local {
                return Err("Server URL must use https:// (the API key travels in requests); plain http is only allowed for localhost".into());
            }
            &lower["http://".len()..]
        } else {
            return Err("Server URL must start with https://".into());
        };
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() {
            return Err("Server URL has no host".into());
        }
        // A port is already present when the authority ends in `:digits`.
        let has_port = authority
            .rsplit_once(':')
            .map(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false);
        if has_port || self.port == 0 || url[url.find("://").unwrap_or(0) + 3..].contains('/') {
            Ok(url)
        } else {
            Ok(format!("{url}:{}", self.port))
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    pub system: SystemConfig,
    pub market_runtime: MarketRuntimeConfig,
    pub fyers: FyersConfig,
    pub charts: ChartsConfig,
    pub trading: TradingConfig,
    pub server: ServerConfig,
    /// Annualised risk-free rate used for Greeks/IV (decimal, 0.07 = 7%).
    pub risk_free_rate: f64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            system: SystemConfig::default(),
            market_runtime: MarketRuntimeConfig::default(),
            fyers: FyersConfig::default(),
            charts: ChartsConfig::default(),
            trading: TradingConfig::default(),
            server: ServerConfig::default(),
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

/// The settings file as pretty JSON. Missing sections are filled with defaults,
/// so new options show up in the editor.
pub fn read_settings_json(paths: &AppPaths) -> Result<String, String> {
    let cfg = match fs::read_to_string(&paths.settings_file) {
        Ok(s) => serde_json::from_str::<AppConfig>(&s).map_err(|e| format!("settings.json is invalid: {e}"))?,
        Err(_) => AppConfig::default(),
    };
    serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())
}

/// Validate and write the settings file. Returns the parsed config.
pub fn write_settings_json(paths: &AppPaths, json: &str) -> Result<AppConfig, String> {
    let cfg: AppConfig = serde_json::from_str(json).map_err(|e| format!("Invalid settings JSON: {e}"))?;
    if cfg.server.is_set() {
        cfg.server.base_url()?;
    }
    let pretty = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    // Write to a temp file first so a crash cannot leave a half-written file.
    let tmp = paths.settings_file.with_extension("json.tmp");
    fs::write(&tmp, pretty).map_err(|e| format!("Could not write settings: {e}"))?;
    fs::rename(&tmp, &paths.settings_file).map_err(|e| format!("Could not save settings: {e}"))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_off_time_parses_or_falls_back() {
        let mut p = PaperConfig::default();
        assert_eq!(p.cutoff_minutes(), 15 * 60 + 20);
        p.square_off_time = "14:45".into();
        assert_eq!(p.cutoff_minutes(), 14 * 60 + 45);
        p.square_off_time = "nonsense".into();
        assert_eq!(p.cutoff_minutes(), 15 * 60 + 20);
        p.square_off_time = "25:00".into();
        assert_eq!(p.cutoff_minutes(), 15 * 60 + 20);
    }

    #[test]
    fn square_off_is_off_when_practising_after_hours() {
        let mut t = TradingConfig::default();
        assert_eq!(t.square_off_minutes(), Some(15 * 60 + 20));
        t.paper_allow_off_hours = true;
        assert_eq!(t.square_off_minutes(), None);
    }

    #[test]
    fn server_url_rules() {
        let s = ServerConfig { url: "https://proxy.example.com".into(), port: 8443 };
        assert_eq!(s.base_url().unwrap(), "https://proxy.example.com:8443");
        let s = ServerConfig { url: "http://example.com".into(), port: 0 };
        assert!(s.base_url().is_err());
        let s = ServerConfig { url: "http://localhost".into(), port: 9000 };
        assert_eq!(s.base_url().unwrap(), "http://localhost:9000");
    }
}
