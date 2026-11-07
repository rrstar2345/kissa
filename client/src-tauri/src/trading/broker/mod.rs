//! The broker abstraction.
//!
//! Every order, paper or live, goes through the same `Broker` trait, so the
//! service (validation, ledger, dashboard, exit, copier, algos...) never
//! branches on the account kind. A broker only has to:
//!
//! * accept / modify / cancel an order and say what happened (`Reply`),
//! * report later changes (fills, rejections, expiry) by sending a
//!   `BrokerEvent` on the shared channel.
//!
//! To add a broker: create `broker/<name>/`, implement `Broker`, and register
//! it in `TradingService::new`. Account kind `<name>` then routes to it.

pub mod dhan;
pub mod paper;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use super::OrderRow;

// ------------------------------------------------------------------ errors ---

/// A broker call that failed.
#[derive(Debug, Clone)]
pub struct BrokerError {
    pub message: String,
    /// The request may or may not have reached the broker (timeout, 5xx).
    /// The service then checks the order book instead of assuming a result.
    pub unknown: bool,
}

impl BrokerError {
    pub fn rejected(message: impl Into<String>) -> Self {
        Self { message: message.into(), unknown: false }
    }

    #[allow(dead_code)]
    pub fn unknown(message: impl Into<String>) -> Self {
        Self { message: message.into(), unknown: true }
    }
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BrokerError {}

impl From<anyhow::Error> for BrokerError {
    fn from(e: anyhow::Error) -> Self {
        Self::rejected(e.to_string())
    }
}

pub type BrokerResult<T> = std::result::Result<T, BrokerError>;

// ------------------------------------------------------------------- types ---

/// The broker's immediate answer to place / modify / cancel.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub broker_order_id: String,
    /// Dhan vocabulary: PENDING, TRIGGERED, TRADED, CANCELLED, REJECTED...
    pub status: String,
    /// Cumulative filled quantity.
    pub filled_qty: i64,
    /// Average fill price (0 = none).
    pub avg_price: f64,
    pub message: String,
}

/// Contract details resolved before an order is sent.
#[derive(Debug, Clone)]
pub struct Contract {
    pub lot_size: i64,
    pub security_id: Option<String>,
    pub trading_symbol: Option<String>,
}

/// A later change to an order, pushed by the broker.
#[derive(Debug, Clone, Default)]
pub struct OrderUpdate {
    pub account_id: String,
    /// Our own order id, when the broker knows it (paper).
    pub local_id: Option<String>,
    pub broker_order_id: String,
    pub correlation_id: String,
    pub status: String,
    /// Cumulative filled quantity.
    pub filled_qty: i64,
    pub avg_price: f64,
    pub message: String,
    /// Price / trigger edited at the broker (0 = unchanged).
    pub price: f64,
    pub trigger_price: f64,
}

#[derive(Debug, Clone)]
pub enum BrokerEvent {
    Order(OrderUpdate),
    /// Re-read the broker's order book (first subscribe, or events were missed).
    Resync { account_id: String },
    /// `connecting` | `connected` | `disconnected` | `feed_down` | `error`
    Status { account_id: String, state: String, detail: String },
}

pub type EventTx = UnboundedSender<BrokerEvent>;

// ------------------------------------------------------------------- trait ---

#[async_trait]
pub trait Broker: Send + Sync {
    /// Account kind this broker serves (`paper`, `dhan`).
    fn kind(&self) -> &'static str;

    /// Does placing an order need the exchange security id (instrument list)?
    fn needs_instrument_ids(&self) -> bool {
        false
    }

    /// Checks that must pass before the order is written anywhere: market
    /// hours, funds, credentials. An error here leaves no trace in the ledger.
    async fn pre_trade(&self, _order: &OrderRow, _contract: &Contract) -> Result<()> {
        Ok(())
    }

    /// Send a new order. `Err` with `unknown` set means the outcome is not known.
    async fn place(&self, order: &OrderRow, contract: &Contract) -> BrokerResult<Reply>;

    /// Change price / trigger of an open order.
    async fn modify(&self, order: &OrderRow, price: f64, trigger_price: f64) -> BrokerResult<Reply>;

    async fn cancel(&self, order: &OrderRow) -> BrokerResult<Reply>;

    /// Brokerage for a fill (rupees). Statutory charges are added by the ledger.
    async fn brokerage(&self, order: &OrderRow, price: f64, qty: i64) -> f64;

    /// Open the broker session / live-update channel for an account.
    async fn connect(&self, _account_id: &str) -> Result<()> {
        Ok(())
    }

    /// Re-read the broker's order book and push every order as an event.
    /// Returns how many orders were read.
    async fn sync(&self, _account_id: &str) -> Result<usize> {
        Ok(0)
    }

    /// Drop cached sessions for an account (edited or deleted).
    async fn forget(&self, _account_id: &str) {}

    /// (symbol, expiry ISO) pairs the broker needs priced right now
    /// (pending simulated orders). The background price feed streams them.
    fn watched_contracts(&self) -> HashSet<(String, String)> {
        HashSet::new()
    }
}

// ---------------------------------------------------------------- registry ---

/// Brokers by account kind.
#[derive(Default)]
pub struct Brokers {
    map: HashMap<&'static str, Arc<dyn Broker>>,
}

impl Brokers {
    pub fn register(&mut self, broker: Arc<dyn Broker>) {
        self.map.insert(broker.kind(), broker);
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn Broker>> {
        self.map.get(kind).cloned()
    }

    pub fn has(&self, kind: &str) -> bool {
        self.map.contains_key(kind)
    }

    /// Kinds sorted by name, for messages.
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut k: Vec<&'static str> = self.map.keys().copied().collect();
        k.sort_unstable();
        k
    }

    pub fn watched_contracts(&self) -> HashSet<(String, String)> {
        self.map.values().flat_map(|b| b.watched_contracts()).collect()
    }
}
