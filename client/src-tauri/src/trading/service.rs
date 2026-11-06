//! Order orchestration: validation, routing to the paper engine or the Dhan
//! proxy, fill booking, and the dashboard / diary views.
//!
//! Every state change goes through `apply_broker_state`, which updates the
//! local order, books new fills in the ledger and emits `order-update`.

use anyhow::{anyhow, Result};
use chrono::{NaiveDate, Timelike};
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::charges;
use super::dhan::{DhanConn, DhanEvent, Hub, ModifyBody, PlaceBody, ProxyOrder};
use super::ltp::LtpStore;
use super::paper::{self, Decision, Pending, PaperBook};
use super::server::ServerAuth;
use super::store::{self, Fill};
use super::{
    ist_date, is_open_status, now_ts, scrip, today_ist, AccountSummary, BrokerStatus, ContractInfo, Dashboard,
    DiaryDay, OrderRow, PlaceOrderInput, Position, TradeRow, SEGMENT_FNO,
};
use crate::accounts;
use crate::charts::instrument::option_key;
use crate::charts::recorder::in_session;
use crate::market::market_clock;
use crate::secrets;
use crate::settings::{AppConfig, ServerConfig};

const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;

fn is_terminal(s: &str) -> bool {
    matches!(s, "TRADED" | "CANCELLED" | "REJECTED" | "EXPIRED")
}

fn norm_status(s: &str) -> String {
    match s.trim().to_uppercase().as_str() {
        "" | "CONFIRM" => "PENDING".to_string(),
        other => other.to_string(),
    }
}

/// NSE option prices move in steps of 0.05.
fn on_tick(p: f64) -> bool {
    ((p * 100.0).round() as i64) % 5 == 0
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub struct TradingService {
    pool: SqlitePool,
    cfg: AppConfig,
    ltp: Arc<LtpStore>,
    app: AppHandle,
    /// Serialises ledger writes: position maths reads, then writes.
    ledger: Mutex<()>,
    book: PaperBook,
    hub: Hub,
    dhan_tx: UnboundedSender<DhanEvent>,
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
    ) -> (Arc<Self>, UnboundedReceiver<DhanEvent>) {
        let (dhan_tx, dhan_rx) = unbounded_channel();
        let server = std::sync::RwLock::new(cfg.server.clone());
        let server_auth = ServerAuth::new(cfg.trading.request_timeout_secs);
        let svc = Arc::new(Self {
            pool,
            cfg,
            ltp,
            app,
            ledger: Mutex::new(()),
            book: PaperBook::default(),
            hub: Hub::default(),
            dhan_tx,
            server,
            server_auth,
        });
        (svc, dhan_rx)
    }

    /// Start the background tasks: paper matching on price changes, Dhan
    /// event handling, the day-order sweeper and reconnection of Dhan
    /// accounts that still have open orders.
    pub fn start(self: &Arc<Self>, mut ltp_rx: UnboundedReceiver<String>, mut dhan_rx: UnboundedReceiver<DhanEvent>) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            me.load_book().await;
            while let Some(key) = ltp_rx.recv().await {
                me.on_price(&key).await;
            }
        });
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = dhan_rx.recv().await {
                me.on_dhan_event(ev).await;
            }
        });
        let me = self.clone();
        tauri::async_runtime::spawn(async move { me.sweeper().await });
        let me = self.clone();
        tauri::async_runtime::spawn(async move { me.reconnect_open_accounts().await });
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

    fn refresh_watch(&self) {
        self.ltp.watch(self.book.instruments());
    }

    /// (symbol, expiry) pairs with pending paper orders: the background feed
    /// keeps these priced.
    pub fn paper_contracts(&self) -> HashSet<(String, String)> {
        self.book.contracts()
    }

    async fn load_book(&self) {
        match store::open_orders(&self.pool, Some("paper")).await {
            Ok(list) => {
                for o in list {
                    self.book.add(o);
                }
            }
            Err(e) => warn!("Could not load open paper orders: {e}"),
        }
        self.refresh_watch();
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

    // ------------------------------------------------------------ contracts ---

    pub async fn contract_info(&self, symbol: &str, expiry: &str, strike: f64, is_call: bool) -> Result<ContractInfo> {
        if let Some(c) = scrip::lookup(&self.pool, symbol, expiry, strike, is_call).await? {
            return Ok(ContractInfo {
                lot_size: Some(c.lot_size),
                security_id: Some(c.security_id),
                trading_symbol: Some(c.trading_symbol).filter(|s| !s.is_empty()),
                lot_source: "dhan".into(),
            });
        }
        if let Some(l) = scrip::lot_size_for_symbol(&self.pool, symbol).await? {
            return Ok(ContractInfo { lot_size: Some(l), security_id: None, trading_symbol: None, lot_source: "dhan".into() });
        }
        let fallback = self.cfg.trading.fallback_lot_sizes.get(&symbol.to_uppercase()).copied();
        Ok(ContractInfo {
            lot_size: fallback,
            security_id: None,
            trading_symbol: None,
            lot_source: if fallback.is_some() { "fallback" } else { "none" }.into(),
        })
    }

    pub async fn refresh_instruments(&self) -> Result<usize> {
        scrip::refresh(&self.pool, &self.cfg.trading).await
    }

    /// Lot size, plus the securityId and trading symbol when Dhan needs them.
    async fn resolve_contract(
        &self,
        symbol: &str,
        expiry: &str,
        strike: f64,
        is_call: bool,
        need_dhan: bool,
    ) -> Result<(i64, Option<String>, Option<String>)> {
        let mut found = scrip::lookup(&self.pool, symbol, expiry, strike, is_call).await?;
        if found.is_none() && need_dhan {
            if scrip::is_stale(&self.pool, self.cfg.trading.scrip_refresh_hours).await {
                scrip::refresh(&self.pool, &self.cfg.trading)
                    .await
                    .map_err(|e| anyhow!("Could not load the Dhan instrument list: {e}"))?;
                found = scrip::lookup(&self.pool, symbol, expiry, strike, is_call).await?;
            }
            if found.is_none() {
                return Err(anyhow!(
                    "Dhan lists no {} {} {} {} contract. Refresh the instrument list from the order ticket, or check that the contract exists.",
                    symbol,
                    expiry,
                    strike,
                    if is_call { "CE" } else { "PE" }
                ));
            }
        }
        if let Some(c) = found {
            let ts = Some(c.trading_symbol).filter(|s| !s.is_empty());
            return Ok((c.lot_size, Some(c.security_id), ts));
        }
        let info = self.contract_info(symbol, expiry, strike, is_call).await?;
        let lot = info
            .lot_size
            .ok_or_else(|| anyhow!("Lot size for {symbol} is unknown. Refresh the instrument list, or add it to trading.fallback_lot_sizes in settings.json."))?;
        Ok((lot, None, None))
    }

    // ---------------------------------------------------------------- place ---

    pub async fn place(&self, input: PlaceOrderInput) -> Result<OrderRow> {
        let side = input.side.trim().to_uppercase();
        let product = input.product.trim().to_uppercase();
        let order_type = input.order_type.trim().to_uppercase();
        let validity = input.validity.trim().to_uppercase();
        if side != "BUY" && side != "SELL" {
            return Err(anyhow!("Side must be BUY or SELL"));
        }
        if product != "INTRADAY" && product != "MARGIN" {
            return Err(anyhow!("Product must be INTRADAY or MARGIN"));
        }
        if order_type != "LIMIT" && order_type != "STOP_LOSS" {
            return Err(anyhow!("Only LIMIT and STOP_LOSS orders are supported (no market orders)"));
        }
        if validity != "DAY" && validity != "IOC" {
            return Err(anyhow!("Validity must be DAY or IOC"));
        }
        if input.lots < 1 || input.lots > 10_000 {
            return Err(anyhow!("Lots must be between 1 and 10,000"));
        }
        if !(input.price.is_finite() && input.price > 0.0) {
            return Err(anyhow!("Price must be greater than 0 (market orders are not supported)"));
        }
        let trigger = input.trigger_price.unwrap_or(0.0);
        if order_type == "STOP_LOSS" && !(trigger.is_finite() && trigger > 0.0) {
            return Err(anyhow!("A STOP_LOSS order needs a trigger price"));
        }
        if !on_tick(input.price) || (trigger > 0.0 && !on_tick(trigger)) {
            return Err(anyhow!("Prices must be a multiple of 0.05"));
        }
        if !(input.strike.is_finite() && input.strike > 0.0) {
            return Err(anyhow!("Invalid strike"));
        }
        NaiveDate::parse_from_str(&input.expiry, "%Y-%m-%d").map_err(|_| anyhow!("Invalid expiry date"))?;
        if input.expiry.as_str() < today_ist().as_str() {
            return Err(anyhow!("This contract has expired"));
        }
        let symbol = input.symbol.trim().to_uppercase();
        if symbol.is_empty() {
            return Err(anyhow!("Symbol is required"));
        }

        // No order of any kind (paper included) without an authenticated server.
        self.require_server_auth().await?;

        let acct = accounts::get(&self.pool, &input.account_id).await?;
        if acct.kind != "paper" && acct.kind != "dhan" {
            return Err(anyhow!("Orders can only be placed on a paper or Dhan account"));
        }
        let is_paper = acct.kind == "paper";
        if is_paper
            && !self.cfg.trading.paper_allow_off_hours
            && !in_session(market_clock::get_ist_now().timestamp())
        {
            return Err(anyhow!(
                "The market is closed (09:15–15:30 IST, Mon–Fri), so paper orders are rejected. To practise after hours, set trading.paper_allow_off_hours to true in settings.json."
            ));
        }

        let (lot, security_id, trading_symbol) =
            self.resolve_contract(&symbol, &input.expiry, input.strike, input.is_call, !is_paper).await?;
        // Connect before writing anything, so a bad token leaves no stray order.
        let conn = if is_paper { None } else { Some(self.dhan_conn(&acct.id).await?) };

        let uid = uuid::Uuid::new_v4();
        let now = now_ts();
        let mut order = OrderRow {
            id: uid.to_string(),
            account_id: acct.id.clone(),
            account_kind: acct.kind.clone(),
            correlation_id: format!("k{}", &uid.simple().to_string()[..20]),
            broker_order_id: None,
            instrument: option_key(&symbol, &input.expiry, input.strike, input.is_call),
            symbol,
            expiry: input.expiry.clone(),
            strike: input.strike,
            is_call: input.is_call,
            security_id: security_id.clone(),
            lot_size: lot,
            side,
            product,
            order_type,
            validity,
            qty: lot * input.lots,
            price: input.price,
            trigger_price: trigger,
            status: if is_paper { "PENDING" } else { "TRANSIT" }.into(),
            filled_qty: 0,
            avg_price: 0.0,
            message: String::new(),
            created_at: now.clone(),
            updated_at: now,
            trade_date: today_ist(),
        };
        store::insert_order(&self.pool, &order).await?;
        self.emit_order(&order);

        if is_paper {
            self.book.add(order.clone());
            self.refresh_watch();
            self.match_one(&order.id).await;
            if order.validity == "IOC" && self.book.get(&order.id).is_some() {
                self.cancel_paper(&order.id, "IOC order could not be executed immediately").await?;
            }
            return Ok(store::get_order(&self.pool, &order.id).await?.unwrap_or(order));
        }

        let (Some(conn), Some(security_id)) = (conn, security_id) else {
            return Err(anyhow!("Dhan securityId is missing"));
        };
        let body = PlaceBody {
            transaction_type: order.side.clone(),
            exchange_segment: SEGMENT_FNO.into(),
            product_type: order.product.clone(),
            order_type: order.order_type.clone(),
            validity: order.validity.clone(),
            security_id,
            trading_symbol: trading_symbol.unwrap_or_default(),
            quantity: order.qty,
            price: order.price,
            trigger_price: order.trigger_price,
            correlation_id: order.correlation_id.clone(),
        };
        match conn.place(&body).await {
            Ok(ack) => {
                let (status, filled, avg, msg) = match &ack.order {
                    Some(po) => (
                        if po.order_status.is_empty() { ack.order_status.clone() } else { po.order_status.clone() },
                        po.filled_qty,
                        po.average_traded_price,
                        po.oms_error_description.clone(),
                    ),
                    None => (ack.order_status.clone(), 0, 0.0, String::new()),
                };
                order = self.apply_broker_state(order, &status, filled, avg, &msg, &ack.order_id).await?;
            }
            Err(e) if e.unknown => {
                order.status = "UNKNOWN".into();
                order.message = format!("Outcome unknown ({e}). Checking with the broker…");
                store::save_order_state(&self.pool, &order).await?;
                order = store::get_order(&self.pool, &order.id).await?.unwrap_or(order);
                self.emit_order(&order);
                self.resolve_unknown_later(order.id.clone());
            }
            Err(e) => {
                order.status = "REJECTED".into();
                order.message = e.to_string();
                store::save_order_state(&self.pool, &order).await?;
                order = store::get_order(&self.pool, &order.id).await?.unwrap_or(order);
                self.emit_order(&order);
            }
        }
        self.hub.ensure_ws(&conn, &self.dhan_tx).await;
        Ok(order)
    }

    // --------------------------------------------------------- state merging ---

    async fn brokerage_for(&self, o: &OrderRow, price: f64, qty: i64) -> f64 {
        if o.account_kind == "paper" {
            accounts::get(&self.pool, &o.account_id)
                .await
                .ok()
                .and_then(|a| a.meta.get("brokeragePerOrder").and_then(Value::as_f64))
                .unwrap_or(20.0)
        } else {
            charges::dhan_brokerage(
                self.cfg.trading.dhan_brokerage_per_order,
                self.cfg.trading.dhan_brokerage_cap_pct,
                price * qty as f64,
            )
        }
    }

    /// Merge the broker's view of an order into the local order: book any new
    /// fill, update status, persist and emit. Terminal statuses are sticky.
    async fn apply_broker_state(
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
            let brokerage = self.brokerage_for(&o, dp, dq).await;
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

    // ---------------------------------------------------------------- paper ---

    async fn on_price(&self, key: &str) {
        for p in self.book.for_instrument(key) {
            self.match_pending(p).await;
        }
    }

    async fn match_one(&self, id: &str) {
        if let Some(p) = self.book.get(id) {
            self.match_pending(p).await;
        }
    }

    async fn match_pending(&self, p: Pending) {
        if !self.cfg.trading.paper_allow_off_hours && !in_session(market_clock::get_ist_now().timestamp()) {
            return;
        }
        let Some(ltp) = self.ltp.ltp(&p.order.instrument) else { return };
        match paper::evaluate(&p.order, p.triggered, ltp) {
            Decision::Wait => {}
            Decision::Trigger => {
                self.book.set_triggered(&p.order.id);
                if let Ok(Some(mut o)) = store::get_order(&self.pool, &p.order.id).await {
                    if is_open_status(&o.status) {
                        o.status = "TRIGGERED".into();
                        if store::save_order_state(&self.pool, &o).await.is_ok() {
                            if let Ok(Some(row)) = store::get_order(&self.pool, &o.id).await {
                                self.emit_order(&row);
                            }
                        }
                    }
                }
            }
            Decision::Fill(price) => self.paper_fill(&p.order.id, price).await,
        }
    }

    async fn paper_fill(&self, id: &str, price: f64) {
        let Some(p) = self.book.take(id) else { return };
        self.refresh_watch();
        let local = match store::get_order(&self.pool, id).await {
            Ok(Some(o)) if is_open_status(&o.status) => o,
            _ => return,
        };
        let qty = local.qty;
        if let Err(e) = self.apply_broker_state(local, "TRADED", qty, price, "", "").await {
            warn!("Paper fill failed for {id}: {e}");
            self.book.add(p.order); // try again on the next price change
            self.refresh_watch();
        }
    }

    async fn cancel_paper(&self, id: &str, message: &str) -> Result<OrderRow> {
        self.book.remove(id);
        self.refresh_watch();
        let mut o = store::get_order(&self.pool, id).await?.ok_or_else(|| anyhow!("Order not found"))?;
        if !is_open_status(&o.status) {
            return Ok(o);
        }
        o.status = "CANCELLED".into();
        o.message = message.to_string();
        store::save_order_state(&self.pool, &o).await?;
        let row = store::get_order(&self.pool, id).await?.unwrap_or(o);
        self.emit_order(&row);
        Ok(row)
    }

    /// Expire day orders and, every few ticks, re-read the Dhan order books.
    async fn sweeper(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(20));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            let now = market_clock::get_ist_now();
            let today = ist_date(now);
            let ist = now + chrono::Duration::seconds(IST_OFFSET_SECS);
            let after_close = ist.hour() * 60 + ist.minute() >= 15 * 60 + 30;
            for p in self.book.all() {
                let stale = p.order.trade_date.as_str() < today.as_str();
                if stale || (!self.cfg.trading.paper_allow_off_hours && after_close) {
                    self.book.remove(&p.order.id);
                    if let Ok(Some(mut o)) = store::get_order(&self.pool, &p.order.id).await {
                        if is_open_status(&o.status) {
                            o.status = "EXPIRED".into();
                            o.message = "Day order expired".into();
                            if store::save_order_state(&self.pool, &o).await.is_ok() {
                                if let Ok(Some(row)) = store::get_order(&self.pool, &o.id).await {
                                    self.emit_order(&row);
                                }
                            }
                        }
                    }
                }
            }
            self.refresh_watch();

            if n % 3 == 0 {
                if let Ok(open) = store::open_orders(&self.pool, Some("dhan")).await {
                    let ids: HashSet<String> = open.into_iter().map(|o| o.account_id).collect();
                    for id in ids {
                        if self.hub.get(&id).await.is_some() {
                            if let Err(e) = self.sync_dhan(&id).await {
                                debug!("Order book sync for {id} failed: {e}");
                            }
                        }
                    }
                }
            }
        }
    }

    // ----------------------------------------------------------------- dhan ---

    async fn dhan_conn(&self, account_id: &str) -> Result<Arc<DhanConn>> {
        let acct = accounts::get(&self.pool, account_id).await?;
        if acct.kind != "dhan" {
            return Err(anyhow!("Not a Dhan account"));
        }
        let meta_str = |m: &Value, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let proxy_id = meta_str(&acct.meta, "proxyId");
        let proxy = accounts::get(&self.pool, &proxy_id)
            .await
            .map_err(|_| anyhow!("The broker-proxy linked to this Dhan account no longer exists"))?;
        let base = meta_str(&proxy.meta, "baseUrl");
        let api_key = secrets::get(&proxy.id, "apiKey")?.ok_or_else(|| anyhow!("Proxy API key is not set"))?;
        let token = secrets::get(&acct.id, "accessToken")?.ok_or_else(|| anyhow!("Dhan access token is not set"))?;
        let client_id = meta_str(&acct.meta, "clientId");

        let creds_key = format!("{base}|{api_key}|{client_id}|{token}");
        if let Some(c) = self.hub.get(account_id).await {
            if c.creds_key == creds_key {
                return Ok(c);
            }
            self.hub.drop_account(account_id).await; // credentials changed
        }
        let conn = DhanConn::new(
            account_id.to_string(),
            base,
            api_key,
            client_id,
            token,
            self.cfg.trading.request_timeout_secs,
        )?;
        self.hub.insert(conn.clone()).await;
        Ok(conn)
    }

    /// Open the proxy session and live-update socket for a Dhan account; the
    /// first `subscribed` event then triggers an order-book sync.
    pub async fn connect_broker(&self, account_id: &str) -> Result<()> {
        let acct = accounts::get(&self.pool, account_id).await?;
        if acct.kind != "dhan" {
            return Ok(());
        }
        let conn = self.dhan_conn(account_id).await?;
        conn.ensure_session().await.map_err(|e| anyhow!("{e}"))?;
        self.hub.ensure_ws(&conn, &self.dhan_tx).await;
        Ok(())
    }

    async fn reconnect_open_accounts(&self) {
        let Ok(open) = store::open_orders(&self.pool, Some("dhan")).await else { return };
        let ids: HashSet<String> = open.into_iter().map(|o| o.account_id).collect();
        for id in ids {
            if let Err(e) = self.connect_broker(&id).await {
                self.emit_status(&id, "error", &e.to_string());
            }
        }
    }

    /// Drop the cached proxy session for an account (deleted or edited).
    pub async fn forget_account(&self, account_id: &str) {
        self.hub.drop_account(account_id).await;
    }

    async fn on_dhan_event(&self, ev: DhanEvent) {
        match ev {
            DhanEvent::Order { account_id, order } => {
                if let Err(e) = self.apply_proxy_order(&account_id, &order).await {
                    warn!("Could not apply order update for {}: {e}", order.order_id);
                }
            }
            DhanEvent::Resync { account_id } => {
                if let Err(e) = self.sync_dhan(&account_id).await {
                    warn!("Order book sync failed for {account_id}: {e}");
                }
            }
            DhanEvent::Status { account_id, state, detail } => self.emit_status(&account_id, &state, &detail),
        }
    }

    /// Apply one order from the broker. Orders not placed from this app
    /// (no matching local order) are ignored.
    async fn apply_proxy_order(&self, account_id: &str, po: &ProxyOrder) -> Result<()> {
        let mut local = None;
        if !po.order_id.is_empty() {
            local = store::find_order_by_broker_id(&self.pool, account_id, &po.order_id).await?;
        }
        if local.is_none() && !po.correlation_id.is_empty() {
            local = store::find_order_by_correlation(&self.pool, account_id, &po.correlation_id).await?;
        }
        let Some(mut o) = local else {
            debug!("Ignoring broker order {} (not placed from this app)", po.order_id);
            return Ok(());
        };
        if !is_terminal(&o.status) {
            // Price / trigger edits made elsewhere.
            if po.price > 0.0 {
                o.price = po.price;
            }
            if po.trigger_price > 0.0 {
                o.trigger_price = po.trigger_price;
            }
        }
        self.apply_broker_state(o, &po.order_status, po.filled_qty, po.average_traded_price, &po.oms_error_description, &po.order_id)
            .await?;
        Ok(())
    }

    /// Re-read the broker's order book and apply every order.
    pub async fn sync_dhan(&self, account_id: &str) -> Result<usize> {
        let conn = self.hub.get(account_id).await.ok_or_else(|| anyhow!("Not connected"))?;
        let orders = conn.orders().await.map_err(|e| anyhow!("{e}"))?;
        let n = orders.len();
        for po in &orders {
            if let Err(e) = self.apply_proxy_order(account_id, po).await {
                warn!("Order book entry {} skipped: {e}", po.order_id);
            }
        }
        Ok(n)
    }

    /// After a place call with an unknown outcome: ask the broker by
    /// correlation id a few times, then leave the order as UNKNOWN.
    fn resolve_unknown_later(&self, order_id: String) {
        let tx = self.dhan_tx.clone();
        let pool = self.pool.clone();
        // A `Resync` re-reads the whole order book, which matches orders by
        // correlation id, so no direct lookup is needed here.
        tokio::spawn(async move {
            for delay in [2u64, 5, 10] {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                let Ok(Some(o)) = store::get_order(&pool, &order_id).await else { return };
                if o.status != "UNKNOWN" {
                    return;
                }
                let _ = tx.send(DhanEvent::Resync { account_id: o.account_id.clone() });
            }
        });
    }

    // ------------------------------------------------------- cancel / modify ---

    pub async fn cancel(&self, id: &str) -> Result<OrderRow> {
        let o = store::get_order(&self.pool, id).await?.ok_or_else(|| anyhow!("Order not found"))?;
        if !is_open_status(&o.status) {
            return Err(anyhow!("The order is already {}", o.status));
        }
        if o.account_kind == "paper" {
            return self.cancel_paper(id, "Cancelled").await;
        }
        let bid = o
            .broker_order_id
            .clone()
            .ok_or_else(|| anyhow!("The broker has not confirmed this order yet; try again in a moment"))?;
        let conn = self.dhan_conn(&o.account_id).await?;
        let ack = conn.cancel(&bid).await.map_err(|e| anyhow!("{e}"))?;
        let status = if ack.order_status.is_empty() { o.status.clone() } else { ack.order_status.clone() };
        let row = self
            .apply_broker_state(o, &status, 0, 0.0, "Cancel requested", &ack.order_id)
            .await?;
        self.hub.ensure_ws(&conn, &self.dhan_tx).await;
        Ok(row)
    }

    pub async fn modify(&self, id: &str, price: f64, trigger_price: Option<f64>) -> Result<OrderRow> {
        let mut o = store::get_order(&self.pool, id).await?.ok_or_else(|| anyhow!("Order not found"))?;
        if !is_open_status(&o.status) {
            return Err(anyhow!("The order is already {}", o.status));
        }
        let trigger = trigger_price.unwrap_or(o.trigger_price);
        if !(price.is_finite() && price > 0.0) || !on_tick(price) {
            return Err(anyhow!("Price must be greater than 0 and a multiple of 0.05"));
        }
        if o.order_type == "STOP_LOSS" && !(trigger > 0.0 && on_tick(trigger)) {
            return Err(anyhow!("A STOP_LOSS order needs a trigger price (multiple of 0.05)"));
        }
        if o.filled_qty > 0 {
            return Err(anyhow!("A partly filled order cannot be modified; cancel the rest instead"));
        }
        self.require_server_auth().await?;

        if o.account_kind == "paper" {
            o.price = price;
            o.trigger_price = if o.order_type == "STOP_LOSS" { trigger } else { 0.0 };
            store::save_order_state(&self.pool, &o).await?;
            let row = store::get_order(&self.pool, id).await?.unwrap_or(o);
            self.book.update(row.clone());
            self.emit_order(&row);
            self.match_one(id).await;
            return Ok(store::get_order(&self.pool, id).await?.unwrap_or(row));
        }

        let bid = o
            .broker_order_id
            .clone()
            .ok_or_else(|| anyhow!("The broker has not confirmed this order yet; try again in a moment"))?;
        let conn = self.dhan_conn(&o.account_id).await?;
        let body = ModifyBody {
            order_type: o.order_type.clone(),
            quantity: o.qty,
            price,
            trigger_price: if o.order_type == "STOP_LOSS" { trigger } else { 0.0 },
            validity: o.validity.clone(),
        };
        let ack = conn.modify(&bid, &body).await.map_err(|e| anyhow!("{e}"))?;
        o.price = price;
        o.trigger_price = body.trigger_price;
        let status = if ack.order_status.is_empty() { o.status.clone() } else { ack.order_status.clone() };
        self.apply_broker_state(o, &status, 0, 0.0, "Modified", &ack.order_id).await
    }

    // ----------------------------------------------------------- read models ---

    fn quote(&self, key: &str) -> Option<(f64, bool)> {
        let q = self.ltp.get(key)?;
        let age = market_clock::get_ist_now().timestamp() - q.at;
        Some((q.ltp, age <= 120))
    }

    pub async fn dashboard(&self, account: Option<&str>) -> Result<Dashboard> {
        let today = today_ist();
        let all = accounts::list(&self.pool).await?;
        let accts: Vec<_> = all
            .into_iter()
            .filter(|a| (a.kind == "paper" || a.kind == "dhan") && account.map(|x| x == a.id).unwrap_or(true))
            .collect();
        let names: HashMap<String, (String, String)> =
            accts.iter().map(|a| (a.id.clone(), (a.name.clone(), a.kind.clone()))).collect();

        let open = store::open_positions(&self.pool, account, &today).await?;
        let totals = store::day_totals(&self.pool, &today).await?;
        let order_counts = store::order_counts(&self.pool, &today).await?;

        let mut positions = Vec::new();
        for p in open {
            let Some((name, kind)) = names.get(&p.account_id).cloned() else { continue };
            let (ltp, source) = match self.quote(&p.instrument) {
                Some((v, true)) => (Some(v), "live"),
                Some((v, false)) => (Some(v), "last"),
                None => match store::last_close(&self.pool, &p.instrument).await {
                    Some(v) => (Some(v), "last"),
                    None => (None, "none"),
                },
            };
            positions.push(Position {
                account_id: p.account_id,
                account_name: name,
                account_kind: kind,
                instrument: p.instrument,
                symbol: p.symbol,
                expiry: p.expiry,
                strike: p.strike,
                is_call: p.is_call,
                product: p.product,
                lot_size: p.lot_size,
                net_qty: p.net_qty,
                avg_price: r2(p.avg_price),
                unrealised: ltp.map(|l| r2((l - p.avg_price) * p.net_qty as f64)),
                ltp,
                ltp_source: source.into(),
            });
        }

        let summaries = accts
            .iter()
            .map(|a| {
                let (realised, charges, trades) = totals.get(&a.id).copied().unwrap_or((0.0, 0.0, 0));
                let mine: Vec<&Position> = positions.iter().filter(|p| p.account_id == a.id).collect();
                let unrealised: f64 = mine.iter().filter_map(|p| p.unrealised).sum();
                AccountSummary {
                    account_id: a.id.clone(),
                    name: a.name.clone(),
                    kind: a.kind.clone(),
                    starting_capital: if a.kind == "paper" {
                        a.meta.get("startingCapital").and_then(Value::as_f64)
                    } else {
                        None
                    },
                    realised_today: r2(realised),
                    charges_today: r2(charges),
                    unrealised: r2(unrealised),
                    net_today: r2(realised + unrealised - charges),
                    open_positions: mine.len() as i64,
                    trades_today: trades,
                    orders_today: order_counts.get(&a.id).copied().unwrap_or(0),
                }
            })
            .collect();

        Ok(Dashboard { as_of: now_ts(), trade_date: today, accounts: summaries, positions })
    }

    pub async fn diary(&self, account: Option<&str>, from: &str, to: &str) -> Result<Vec<DiaryDay>> {
        store::diary(&self.pool, account, from, to).await
    }

    pub async fn list_orders(&self, account: Option<&str>, date: Option<&str>) -> Result<Vec<OrderRow>> {
        store::list_orders(&self.pool, account, date).await
    }

    pub async fn list_trades(&self, account: Option<&str>, date: Option<&str>) -> Result<Vec<TradeRow>> {
        store::list_trades(&self.pool, account, date).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_size_is_five_paise() {
        assert!(on_tick(100.05));
        assert!(on_tick(100.0));
        assert!(on_tick(0.05));
        assert!(!on_tick(100.01));
        assert!(!on_tick(99.97));
    }

    #[test]
    fn statuses_are_normalised() {
        assert_eq!(norm_status(""), "PENDING");
        assert_eq!(norm_status(" traded "), "TRADED");
        assert_eq!(norm_status("CONFIRM"), "PENDING");
        assert!(is_terminal("CANCELLED") && !is_terminal("PART_TRADED"));
    }
}
