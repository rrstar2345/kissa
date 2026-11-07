//! Order orchestration, independent of the broker.
//!
//! * `orders`: validation, routing to the right `Broker`, cancel / modify.
//! * `exit`: Exit position / Exit all.
//! * `squareoff`: paper INTRADAY auto square-off.
//! * `views`: dashboard, diary, order / trade lists, quotes.
//!
//! Every state change, whichever broker it came from, goes through
//! `apply_state`, which updates the local order, books new fills in the
//! ledger and emits `order-update`.

mod exit;
mod orders;
mod squareoff;
mod views;

use anyhow::{anyhow, Result};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::broker::dhan::DhanBroker;
use super::broker::paper::PaperBroker;
use super::broker::{Broker, BrokerEvent, Brokers, EventTx, OrderUpdate, Reply};
use super::ltp::LtpStore;
use super::server::ServerAuth;
use super::store::{self, Fill};
use super::{BrokerStatus, OrderRow};
use crate::accounts;
use crate::settings::{AppConfig, ServerConfig};

fn is_terminal(s: &str) -> bool {
    matches!(s, "TRADED" | "CANCELLED" | "REJECTED" | "EXPIRED")
}

fn norm_status(s: &str) -> String {
    match s.trim().to_uppercase().as_str() {
        "" | "CONFIRM" => "PENDING".to_string(),
        other => other.to_string(),
    }
}

pub struct TradingService {
    pool: SqlitePool,
    cfg: AppConfig,
    ltp: Arc<LtpStore>,
    app: AppHandle,
    /// Serialises ledger writes: position maths reads, then writes.
    ledger: Mutex<()>,
    brokers: Brokers,
    /// Kept for `start` (it consumes the price-change notifications).
    paper: Arc<PaperBroker>,
    /// Brokers push order updates here.
    events_tx: EventTx,
    /// Server (broker-proxy) connection; every order needs it to authenticate.
    server: std::sync::RwLock<ServerConfig>,
    server_auth: ServerAuth,
}

impl TradingService {
    pub fn new(
        pool: SqlitePool,
        cfg: AppConfig,
        ltp: Arc<LtpStore>,
        app: AppHandle,
    ) -> (Arc<Self>, UnboundedReceiver<BrokerEvent>) {
        let (events_tx, events_rx) = unbounded_channel();

        // Register every broker here: account kind -> implementation.
        let paper = PaperBroker::new(pool.clone(), &cfg.trading, ltp.clone(), events_tx.clone());
        let dhan = DhanBroker::new(pool.clone(), &cfg.trading, events_tx.clone());
        let mut brokers = Brokers::default();
        brokers.register(paper.clone());
        brokers.register(dhan);

        let server = std::sync::RwLock::new(cfg.server.clone());
        let server_auth = ServerAuth::new(cfg.trading.request_timeout_secs);
        let svc = Arc::new(Self {
            pool,
            cfg,
            ltp,
            app,
            ledger: Mutex::new(()),
            brokers,
            paper,
            events_tx,
            server,
            server_auth,
        });
        (svc, events_rx)
    }

    /// Start the background tasks: the paper matcher, broker event handling,
    /// the periodic sweeper and reconnection of accounts with open orders.
    pub fn start(self: &Arc<Self>, ltp_rx: UnboundedReceiver<String>, mut events_rx: UnboundedReceiver<BrokerEvent>) {
        self.paper.start(ltp_rx);

        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = events_rx.recv().await {
                me.on_event(ev).await;
            }
        });
        let me = self.clone();
        tauri::async_runtime::spawn(async move { me.sweeper().await });
        let me = self.clone();
        tauri::async_runtime::spawn(async move { me.reconnect_open_accounts().await });
    }

    /// (symbol, expiry) pairs the background feed must keep priced.
    pub fn watched_contracts(&self) -> HashSet<(String, String)> {
        self.brokers.watched_contracts()
    }

    fn emit_order(&self, o: &OrderRow) {
        let _ = self.app.emit("order-update", o);
    }

    fn emit_status(&self, account_id: &str, state: &str, detail: &str) {
        let _ = self.app.emit(
            "broker-status",
            BrokerStatus { account_id: account_id.into(), state: state.into(), detail: detail.into() },
        );
    }

    fn broker_for_kind(&self, kind: &str) -> Result<Arc<dyn Broker>> {
        self.brokers
            .get(kind)
            .ok_or_else(|| anyhow!("Orders can only be placed on a {} account", self.brokers.kinds().join(" or ")))
    }

    async fn broker_for_account(&self, account_id: &str) -> Result<Arc<dyn Broker>> {
        let acct = accounts::get(&self.pool, account_id).await?;
        self.broker_for_kind(&acct.kind)
    }

    // --------------------------------------------------------------- server ---

    fn server_cfg(&self) -> ServerConfig {
        self.server.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// Apply new server details (after the user saved them) and drop the
    /// cached server session.
    pub async fn set_server(&self, cfg: ServerConfig) {
        if let Ok(mut s) = self.server.write() {
            *s = cfg;
        }
        self.server_auth.reset().await;
    }

    /// Every order (paper or live) is placed only for a client the server
    /// authenticates.
    async fn require_server_auth(&self) -> Result<()> {
        self.server_auth.verify(&self.server_cfg()).await
    }

    /// Check the saved server details and API key (used by the Test button).
    pub async fn test_server(&self) -> Result<String> {
        self.server_auth.reset().await;
        let cfg = self.server_cfg();
        self.server_auth.verify(&cfg).await?;
        Ok(format!("Connected to {} and the API key was accepted.", cfg.base_url().unwrap_or_default()))
    }

    // -------------------------------------------------------------- accounts ---

    /// Open the broker session / live-update channel for an account.
    pub async fn connect_broker(&self, account_id: &str) -> Result<()> {
        self.broker_for_account(account_id).await?.connect(account_id).await
    }

    /// Re-read the broker's order book now.
    pub async fn sync_account(&self, account_id: &str) -> Result<usize> {
        self.broker_for_account(account_id).await?.sync(account_id).await
    }

    /// Drop cached broker sessions for an account (deleted or edited).
    pub async fn forget_account(&self, account_id: &str) {
        for kind in self.brokers.kinds() {
            if let Some(b) = self.brokers.get(kind) {
                b.forget(account_id).await;
            }
        }
    }

    async fn open_order_accounts(&self) -> HashSet<String> {
        match store::open_orders(&self.pool, None).await {
            Ok(open) => open.into_iter().map(|o| o.account_id).collect(),
            Err(_) => HashSet::new(),
        }
    }

    async fn reconnect_open_accounts(&self) {
        for id in self.open_order_accounts().await {
            if let Err(e) = self.connect_broker(&id).await {
                self.emit_status(&id, "error", &e.to_string());
            }
        }
    }

    // ---------------------------------------------------------------- events ---

    async fn on_event(&self, ev: BrokerEvent) {
        match ev {
            BrokerEvent::Order(u) => {
                let id = u.broker_order_id.clone();
                if let Err(e) = self.apply_update(u).await {
                    warn!("Could not apply order update for {id}: {e}");
                }
            }
            BrokerEvent::Resync { account_id } => {
                if let Err(e) = self.sync_account(&account_id).await {
                    warn!("Order book sync failed for {account_id}: {e}");
                }
            }
            BrokerEvent::Status { account_id, state, detail } => self.emit_status(&account_id, &state, &detail),
        }
    }

    /// Apply a broker-pushed update. Orders not placed from this app (no
    /// matching local order) are ignored.
    async fn apply_update(&self, u: OrderUpdate) -> Result<()> {
        let mut local = None;
        if let Some(id) = &u.local_id {
            local = store::get_order(&self.pool, id).await?;
        }
        if local.is_none() && !u.broker_order_id.is_empty() {
            local = store::find_order_by_broker_id(&self.pool, &u.account_id, &u.broker_order_id).await?;
        }
        if local.is_none() && !u.correlation_id.is_empty() {
            local = store::find_order_by_correlation(&self.pool, &u.account_id, &u.correlation_id).await?;
        }
        let Some(mut o) = local else {
            debug!("Ignoring broker order {} (not placed from this app)", u.broker_order_id);
            return Ok(());
        };
        if !is_terminal(&o.status) {
            // Price / trigger edits made elsewhere.
            if u.price > 0.0 {
                o.price = u.price;
            }
            if u.trigger_price > 0.0 {
                o.trigger_price = u.trigger_price;
            }
        }
        self.apply_state(o, &u.status, u.filled_qty, u.avg_price, &u.message, &u.broker_order_id).await?;
        Ok(())
    }

    async fn apply_reply(&self, o: OrderRow, r: Reply) -> Result<OrderRow> {
        self.apply_state(o, &r.status, r.filled_qty, r.avg_price, &r.message, &r.broker_order_id).await
    }

    /// Merge the broker's view of an order into the local order: book any new
    /// fill, update status, persist and emit. Terminal statuses are sticky.
    async fn apply_state(
        &self,
        mut o: OrderRow,
        status: &str,
        filled: i64,
        avg: f64,
        message: &str,
        broker_id: &str,
    ) -> Result<OrderRow> {
        if !broker_id.is_empty() && o.broker_order_id.as_deref() != Some(broker_id) {
            o.broker_order_id = Some(broker_id.to_string());
        }
        let status = norm_status(status);
        let mut filled = filled.clamp(0, o.qty);
        let mut avg = if avg.is_finite() && avg > 0.0 { avg } else { 0.0 };
        if status == "TRADED" && filled == 0 {
            filled = o.qty;
        }
        if filled > o.filled_qty && avg <= 0.0 {
            avg = o.price; // no price information: assume the limit price
        }
        if filled > o.filled_qty {
            let dq = filled - o.filled_qty;
            let mut dp = (filled as f64 * avg - o.filled_qty as f64 * o.avg_price) / dq as f64;
            if !(dp.is_finite() && dp > 0.0) {
                dp = avg;
            }
            let brokerage = match self.brokers.get(&o.account_kind) {
                Some(b) => b.brokerage(&o, dp, dq).await,
                None => 0.0,
            };
            {
                let _guard = self.ledger.lock().await;
                store::record_fill(
                    &self.pool,
                    &self.cfg.trading.charges,
                    Fill { order: &o, qty: dq, price: dp, brokerage, ext_id: Some(format!("{}:{}", o.id, filled)) },
                )
                .await?;
            }
            o.filled_qty = filled;
            o.avg_price = avg;
        }
        if !is_terminal(&o.status) {
            o.status = status;
        }
        if !message.is_empty() {
            o.message = message.to_string();
        } else if o.status == "TRADED" {
            o.message.clear();
        }
        store::save_order_state(&self.pool, &o).await?;
        let row = store::get_order(&self.pool, &o.id).await?.unwrap_or(o);
        self.emit_order(&row);
        Ok(row)
    }

    // --------------------------------------------------------------- sweeper ---

    /// Every 5 s: paper auto square-off; every minute: re-read the order
    /// books of accounts with open orders (catches missed live updates).
    async fn sweeper(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            self.auto_square_off(n == 1).await;
            if n % 12 == 0 {
                for id in self.open_order_accounts().await {
                    if let Err(e) = self.sync_account(&id).await {
                        debug!("Order book sync for {id} failed: {e}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_normalised() {
        assert_eq!(norm_status(""), "PENDING");
        assert_eq!(norm_status(" traded "), "TRADED");
        assert_eq!(norm_status("CONFIRM"), "PENDING");
        assert!(is_terminal("CANCELLED") && !is_terminal("PART_TRADED"));
    }
}
