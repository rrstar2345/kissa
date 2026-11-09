//! Paper broker: simulates an exchange + broker on live prices.
//!
//! * Orders rest in an in-memory book and are matched against the `LtpStore`
//!   (see `matching` for the rules, including slippage).
//! * `pre_trade` enforces what a real broker would: market hours, the
//!   INTRADAY cut-off and available funds (see `funds`).
//! * Optional latency delays the acknowledgement, so the order is matched
//!   against the price after the delay.
//! * Fills, triggers and day-order expiry that happen later are pushed as
//!   `BrokerEvent`s, the same way a live broker's WebSocket would.

mod book;
mod funds;
mod matching;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Timelike;
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::warn;

use super::{Broker, BrokerError, BrokerEvent, BrokerResult, Contract, EventTx, OrderUpdate, Reply};
use crate::accounts;
use crate::charts::recorder::in_session;
use crate::market::market_clock;
use crate::settings::{PaperConfig, TradingConfig};
use crate::trading::ltp::LtpStore;
use crate::trading::{store, today_ist, OrderRow};
use book::PaperBook;
use matching::Decision;

const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;
const MARKET_CLOSE_MINUTES: u32 = 15 * 60 + 30;
const DEFAULT_BROKERAGE: f64 = 20.0;

/// What happened when an order was checked against the current price.
enum Step {
    Nothing,
    Triggered,
    Filled(f64),
}

pub struct PaperBroker {
    pool: SqlitePool,
    cfg: PaperConfig,
    allow_off_hours: bool,
    cutoff: Option<u32>,
    ltp: Arc<LtpStore>,
    tx: EventTx,
    book: PaperBook,
}

fn ist_minutes() -> u32 {
    let t = market_clock::get_ist_now() + chrono::Duration::seconds(IST_OFFSET_SECS);
    t.hour() * 60 + t.minute()
}

fn hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

impl PaperBroker {
    pub fn new(pool: SqlitePool, cfg: &TradingConfig, ltp: Arc<LtpStore>, tx: EventTx) -> Arc<Self> {
        Arc::new(Self {
            pool,
            cfg: cfg.paper.clone(),
            allow_off_hours: cfg.paper_allow_off_hours,
            cutoff: cfg.square_off_minutes(),
            ltp,
            tx,
            book: PaperBook::default(),
        })
    }

    /// Load open orders from the last run, then match on every price change
    /// and expire day orders after the close.
    pub fn start(self: &Arc<Self>, mut ltp_rx: UnboundedReceiver<String>) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            me.load_book().await;
            while let Some(key) = ltp_rx.recv().await {
                for p in me.book.for_instrument(&key) {
                    me.step_and_emit(&p.order.id);
                }
            }
        });
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                me.expire_day_orders();
            }
        });
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

    fn refresh_watch(&self) {
        self.ltp.watch(self.book.instruments());
    }

    fn market_open(&self) -> bool {
        self.allow_off_hours || in_session(market_clock::get_ist_now().timestamp())
    }

    // ------------------------------------------------------------ matching ---

    /// Check one order against the latest price and update the book.
    fn step(&self, id: &str) -> Step {
        let Some(p) = self.book.get(id) else { return Step::Nothing };
        if !self.market_open() {
            return Step::Nothing;
        }
        let Some(ltp) = self.ltp.ltp(&p.order.instrument) else { return Step::Nothing };
        match matching::evaluate(&p.order, p.triggered, ltp, self.cfg.slippage()) {
            Decision::Wait => Step::Nothing,
            Decision::Trigger => {
                self.book.set_triggered(id);
                Step::Triggered
            }
            Decision::Fill(px) => {
                // `take` is the arbiter: only one of fill / cancel wins.
                if self.book.take(id).is_some() {
                    self.refresh_watch();
                    Step::Filled(px)
                } else {
                    Step::Nothing
                }
            }
        }
    }

    fn update_for(&self, o: &OrderRow, status: &str, filled: i64, avg: f64, message: &str) -> OrderUpdate {
        OrderUpdate {
            account_id: o.account_id.clone(),
            local_id: Some(o.id.clone()),
            broker_order_id: o.id.clone(),
            correlation_id: o.correlation_id.clone(),
            status: status.into(),
            filled_qty: filled,
            avg_price: avg,
            message: message.into(),
            price: 0.0,
            trigger_price: 0.0,
        }
    }

    /// Step an order that is already resting and push the result as an event.
    fn step_and_emit(&self, id: &str) {
        let Some(p) = self.book.get(id) else { return };
        let o = p.order;
        let update = match self.step(id) {
            Step::Nothing => return,
            Step::Triggered => self.update_for(&o, "TRIGGERED", 0, 0.0, ""),
            Step::Filled(px) => self.update_for(&o, "TRADED", o.qty, px, ""),
        };
        let _ = self.tx.send(BrokerEvent::Order(update));
    }

    fn expire_day_orders(&self) {
        let today = today_ist();
        let after_close = !self.allow_off_hours && ist_minutes() >= MARKET_CLOSE_MINUTES;
        let mut changed = false;
        for p in self.book.all() {
            let stale = p.order.trade_date.as_str() < today.as_str();
            if (stale || after_close) && self.book.take(&p.order.id).is_some() {
                changed = true;
                let u = self.update_for(&p.order, "EXPIRED", 0, 0.0, "Day order expired");
                let _ = self.tx.send(BrokerEvent::Order(u));
            }
        }
        if changed {
            self.refresh_watch();
        }
    }

    // --------------------------------------------------------------- funds ---

    async fn account_capital(&self, account_id: &str) -> Option<f64> {
        let acct = accounts::get(&self.pool, account_id).await.ok()?;
        acct.meta.get("startingCapital").and_then(Value::as_f64).filter(|c| *c > 0.0)
    }

    /// Reject the order if the account cannot fund it. `exclude` is an order
    /// being modified (its own reservation is replaced by the new one).
    async fn check_funds(&self, o: &OrderRow, exclude: Option<&str>) -> Result<()> {
        if !self.cfg.enforce_margin {
            return Ok(());
        }
        let Some(capital) = self.account_capital(&o.account_id).await else { return Ok(()) };
        let pct = self.cfg.short_margin_pct;

        let (realised, charges) = store::account_pnl(&self.pool, &o.account_id).await?;
        let cash = capital + realised - charges;

        let positions = store::open_positions(&self.pool, Some(&o.account_id), &today_ist()).await?;
        let mut position_qty: HashMap<(String, String), i64> = HashMap::new();
        let mut blocked = 0.0;
        for p in &positions {
            position_qty.insert((p.instrument.clone(), p.product.clone()), p.net_qty);
            blocked += funds::position_block(p.net_qty, p.avg_price, p.strike, pct);
        }

        let mut reserved = 0.0;
        for other in store::open_orders(&self.pool, Some("paper")).await? {
            if other.account_id != o.account_id || Some(other.id.as_str()) == exclude || other.id == o.id {
                continue;
            }
            let pos = position_qty.get(&(other.instrument.clone(), other.product.clone())).copied().unwrap_or(0);
            let remaining = other.qty - other.filled_qty;
            reserved += funds::requirement(other.side == "BUY", remaining, other.price, other.strike, pos, pct);
        }

        let pos = position_qty.get(&(o.instrument.clone(), o.product.clone())).copied().unwrap_or(0);
        let needed = funds::requirement(o.side == "BUY", o.qty, o.price, o.strike, pos, pct);
        let available = cash - blocked - reserved;
        // An order that only closes exposure needs nothing, however over-committed the account is.
        if needed > 0.0 && needed > available + 1e-6 {
            return Err(anyhow!(
                "Insufficient funds: this order needs ₹{needed:.0} but ₹{:.0} is available (capital ₹{cash:.0}, \
                 blocked in positions ₹{blocked:.0}, reserved by open orders ₹{reserved:.0}).",
                available.max(0.0)
            ));
        }
        Ok(())
    }

    fn reply(&self, o: &OrderRow, step: Step) -> Reply {
        let (status, filled, avg) = match step {
            Step::Filled(px) => ("TRADED", o.qty, px),
            Step::Triggered => ("TRIGGERED", 0, 0.0),
            Step::Nothing => ("PENDING", 0, 0.0),
        };
        Reply { broker_order_id: o.id.clone(), status: status.into(), filled_qty: filled, avg_price: avg, message: String::new() }
    }
}

#[async_trait]
impl Broker for PaperBroker {
    fn kind(&self) -> &'static str {
        "paper"
    }

    async fn pre_trade(&self, o: &OrderRow, _contract: &Contract) -> Result<()> {
        if !self.market_open() {
            return Err(anyhow!(
                "The market is closed (09:15–15:30 IST, Mon–Fri), so paper orders are rejected. To practise after hours, set trading.paper_allow_off_hours to true in settings.json."
            ));
        }
        if let Some(cutoff) = self.cutoff {
            if o.product == "INTRADAY" && ist_minutes() >= cutoff {
                return Err(anyhow!(
                    "New INTRADAY orders are not accepted after {} IST (auto square-off). Use the Margin product to carry overnight.",
                    hhmm(cutoff)
                ));
            }
        }
        self.check_funds(o, None).await
    }

    async fn place(&self, o: &OrderRow, _contract: &Contract) -> BrokerResult<Reply> {
        if self.cfg.latency_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.cfg.latency_ms.min(10_000))).await;
        }
        self.book.add(o.clone());
        self.refresh_watch();
        let step = self.step(&o.id);
        if o.validity == "IOC" && !matches!(step, Step::Filled(_)) {
            self.book.remove(&o.id);
            self.refresh_watch();
            return Ok(Reply {
                broker_order_id: o.id.clone(),
                status: "CANCELLED".into(),
                message: "IOC order could not be executed immediately".into(),
                ..Reply::default()
            });
        }
        Ok(self.reply(o, step))
    }

    async fn modify(&self, o: &OrderRow, price: f64, trigger_price: f64) -> BrokerResult<Reply> {
        let mut changed = o.clone();
        changed.price = price;
        changed.trigger_price = trigger_price;
        self.check_funds(&changed, Some(&o.id)).await?;
        if self.cfg.latency_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.cfg.latency_ms.min(10_000))).await;
        }
        if !self.book.update(changed.clone()) {
            return Err(BrokerError::rejected("The order is no longer pending"));
        }
        let step = self.step(&o.id);
        let mut reply = self.reply(&changed, step);
        reply.message = "Modified".into();
        Ok(reply)
    }

    async fn cancel(&self, o: &OrderRow) -> BrokerResult<Reply> {
        self.book.remove(&o.id);
        self.refresh_watch();
        Ok(Reply { broker_order_id: o.id.clone(), status: "CANCELLED".into(), message: "Cancelled".into(), ..Reply::default() })
    }

    async fn brokerage(&self, o: &OrderRow, _price: f64, _qty: i64) -> f64 {
        accounts::get(&self.pool, &o.account_id)
            .await
            .ok()
            .and_then(|a| a.meta.get("brokeragePerOrder").and_then(Value::as_f64))
            .unwrap_or(DEFAULT_BROKERAGE)
    }

    fn watched_contracts(&self) -> HashSet<(String, String)> {
        self.book.contracts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_text_is_formatted_back() {
        assert_eq!(hhmm(15 * 60 + 20), "15:20");
        assert_eq!(hhmm(9 * 60 + 5), "09:05");
    }
}
