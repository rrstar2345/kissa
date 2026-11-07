//! Orders, brokers, the local ledger of orders/trades, positions, charges
//! and the live price store.
//!
//! Layout:
//! * `broker/`: the `Broker` trait and one folder per implementation
//!   (`paper`, `dhan`). Adding a broker means adding a folder and registering
//!   it in `service::TradingService::new`.
//! * `service/`: broker-independent order flow (validation, routing, fills
//!   into the ledger, exit orders, auto square-off, dashboard views).
//! * `store.rs`, `charges.rs`, `pricing.rs`, `ltp.rs`, `feed.rs`, `scrip.rs`,
//!   `server.rs`: persistence, costs, tick maths, prices, instrument list and
//!   the server (broker-proxy) authentication gate.
//!
//! The client is the system of record: the proxy persists nothing, so every
//! order update and fill is written to SQLite here.

pub mod broker;
pub mod charges;
pub mod feed;
pub mod ltp;
pub mod pricing;
pub mod scrip;
pub mod server;
pub mod service;
pub mod store;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// Dhan exchange segment for index options.
pub const SEGMENT_FNO: &str = "NSE_FNO";

/// Order statuses that can still change (Dhan vocabulary; paper uses the same).
pub const OPEN_STATUSES: [&str; 5] = ["TRANSIT", "PENDING", "TRIGGERED", "PART_TRADED", "UNKNOWN"];

pub fn is_open_status(s: &str) -> bool {
    OPEN_STATUSES.contains(&s)
}

/// UTC timestamp with a fixed format so string ordering equals time ordering.
pub fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// IST calendar date (`YYYY-MM-DD`) of an instant.
pub fn ist_date(t: DateTime<Utc>) -> String {
    (t + chrono::Duration::seconds(5 * 3600 + 30 * 60)).format("%Y-%m-%d").to_string()
}

/// Today's IST date from the NSE-synced clock.
pub fn today_ist() -> String {
    ist_date(crate::market::market_clock::get_ist_now())
}

// ---------------------------------------------------------------------------
// Stored rows (also sent to the UI)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct OrderRow {
    pub id: String,
    pub account_id: String,
    /// `paper` | `dhan`
    pub account_kind: String,
    pub correlation_id: String,
    pub broker_order_id: Option<String>,
    /// Instrument key, e.g. `OPT:NIFTY:2026-11-24:24000:CE`.
    pub instrument: String,
    pub symbol: String,
    /// ISO date `YYYY-MM-DD`.
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    pub security_id: Option<String>,
    pub lot_size: i64,
    pub side: String,
    pub product: String,
    pub order_type: String,
    pub validity: String,
    /// Quantity in units (lots x lot size).
    pub qty: i64,
    pub price: f64,
    pub trigger_price: f64,
    pub status: String,
    pub filled_qty: i64,
    pub avg_price: f64,
    pub message: String,
    pub created_at: String,
    pub updated_at: String,
    /// IST date the order was placed.
    pub trade_date: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct TradeRow {
    pub id: String,
    pub account_id: String,
    pub order_id: String,
    pub instrument: String,
    pub symbol: String,
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    pub lot_size: i64,
    pub side: String,
    pub product: String,
    pub qty: i64,
    pub price: f64,
    /// Total charges for this fill (brokerage + statutory).
    pub charges: f64,
    /// JSON breakdown of `charges` (see `charges::Charges`).
    pub charge_detail: String,
    /// Realised P&L booked by this fill (before charges).
    pub realised: f64,
    pub ts: String,
    pub trade_date: String,
    pub ext_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Requests / views
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderInput {
    pub account_id: String,
    pub symbol: String,
    /// ISO date `YYYY-MM-DD`.
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    /// BUY | SELL
    pub side: String,
    /// INTRADAY | MARGIN
    pub product: String,
    /// LIMIT | STOP_LOSS
    pub order_type: String,
    /// DAY | IOC
    pub validity: String,
    pub lots: i64,
    pub price: f64,
    #[serde(default)]
    pub trigger_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractInfo {
    pub lot_size: Option<i64>,
    pub security_id: Option<String>,
    pub trading_symbol: Option<String>,
    /// `dhan` (instrument list), `fallback` (settings.json) or `none`.
    pub lot_source: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub account_id: String,
    pub account_name: String,
    pub account_kind: String,
    pub instrument: String,
    pub symbol: String,
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    pub product: String,
    pub lot_size: i64,
    /// Signed: negative = short.
    pub net_qty: i64,
    pub avg_price: f64,
    pub ltp: Option<f64>,
    /// `live` (streaming), `last` (last stored bar) or `none`.
    pub ltp_source: String,
    pub unrealised: Option<f64>,
    /// Limit price an Exit order would use right now (last price plus the
    /// configured buffer); `None` without a price.
    pub exit_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub account_id: String,
    pub name: String,
    pub kind: String,
    pub starting_capital: Option<f64>,
    pub realised_today: f64,
    pub charges_today: f64,
    pub unrealised: f64,
    /// realised + unrealised - charges, today.
    pub net_today: f64,
    pub open_positions: i64,
    pub trades_today: i64,
    pub orders_today: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dashboard {
    pub as_of: String,
    pub trade_date: String,
    pub accounts: Vec<AccountSummary>,
    pub positions: Vec<Position>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiaryDay {
    pub date: String,
    pub orders: i64,
    pub trades: i64,
    pub realised: f64,
    pub charges: f64,
    /// realised - charges.
    pub net: f64,
    pub rows: Vec<TradeRow>,
}

/// Pushed on the `broker-status` event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrokerStatus {
    pub account_id: String,
    /// `connecting` | `connected` | `disconnected` | `error`
    pub state: String,
    pub detail: String,
}

/// Latest price of one contract, for the order ticket.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteView {
    pub ltp: Option<f64>,
    /// Seconds since the price last changed hands in our feed.
    pub age_secs: Option<i64>,
    /// Fresh (updated within two minutes).
    pub live: bool,
}

/// Outcome of "Exit all".
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExitAllResult {
    pub placed: usize,
    /// Open orders cancelled first so they cannot reopen the position.
    pub cancelled: usize,
    /// One line per position that could not be exited, with the reason.
    pub skipped: Vec<String>,
    pub orders: Vec<OrderRow>,
}
