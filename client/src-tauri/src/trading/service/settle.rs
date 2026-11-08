//! Paper expiry settlement.
//!
//! A position left in a contract that has expired is closed at the close of
//! that contract's own last 1-minute bar on its expiry day. Before this, such
//! positions silently dropped out of the open list and the premium was kept.
//!
//! The settlement is booked as a synthetic order (`role = SETTLE`) that is
//! filled at once, dated the expiry day, with no brokerage. It runs at start-up
//! (positions left over while the app was closed) and then every minute.
//! A contract expiring today is settled once the session is over (15:30 IST).
//! Live brokers settle on their own side, so only paper accounts are touched.

use anyhow::Result;
use chrono::{NaiveDate, Timelike};
use std::collections::HashSet;
use tracing::{info, warn};

use super::TradingService;
use crate::accounts;
use crate::market::market_clock;
use crate::trading::store::{self, OpenPos};
use crate::trading::{now_ts, today_ist, OrderRow, ROLE_SETTLE};

const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;
const MARKET_CLOSE_MINUTES: u32 = 15 * 60 + 30;

/// Contracts expiring *before* this date are due: today once the session is
/// over, otherwise yesterday and earlier.
fn settle_before(today: &str, past_close: bool) -> String {
    if !past_close {
        return today.to_string();
    }
    NaiveDate::parse_from_str(today, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.succ_opt())
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| today.to_string())
}

impl TradingService {
    pub(super) async fn settle_expired(&self) {
        if let Err(e) = self.settle_due().await {
            warn!("Expiry settlement failed: {e}");
        }
    }

    async fn settle_due(&self) -> Result<()> {
        let ist = market_clock::get_ist_now() + chrono::Duration::seconds(IST_OFFSET_SECS);
        let past_close = ist.hour() * 60 + ist.minute() >= MARKET_CLOSE_MINUTES;
        let before = settle_before(&today_ist(), past_close);

        let due = store::expired_positions(&self.pool, &before).await?;
        if due.is_empty() {
            return Ok(());
        }
        let paper: HashSet<String> =
            accounts::list(&self.pool).await?.into_iter().filter(|a| a.kind == "paper").map(|a| a.id).collect();
        for p in due.iter().filter(|p| paper.contains(&p.account_id)) {
            if let Err(e) = self.settle_one(p).await {
                warn!("Could not settle {}: {e}", p.instrument);
            }
        }
        Ok(())
    }

    async fn settle_one(&self, p: &OpenPos) -> Result<()> {
        // The contract's own close on expiry day; failing that, its latest
        // recorded close (the app was not running that day), and the order says so.
        let (price, note) = match store::expiry_close(&self.pool, &p.instrument, &p.expiry).await {
            Some(px) => (px, format!("Expiry settlement at the last 1m close, {}", p.expiry)),
            None => match store::last_close(&self.pool, &p.instrument).await {
                Some(px) => (px, "Expiry settlement at the last recorded close (no bar on the expiry day)".to_string()),
                None => {
                    warn!("No price recorded for {}; it stays open until one exists", p.instrument);
                    return Ok(());
                }
            },
        };

        // Nothing can trade an expired contract: drop its resting orders first.
        for o in store::open_orders(&self.pool, Some("paper")).await? {
            if o.account_id == p.account_id && o.instrument == p.instrument {
                if let Err(e) = self.cancel(&o.id).await {
                    warn!("Could not cancel {} before settlement: {e}", o.id);
                }
            }
        }

        let buy = p.net_qty < 0; // a short position is closed by buying back
        let uid = uuid::Uuid::new_v4();
        let now = now_ts();
        let order = OrderRow {
            id: uid.to_string(),
            account_id: p.account_id.clone(),
            account_kind: "paper".into(),
            correlation_id: format!("st{}", &uid.simple().to_string()[..20]),
            broker_order_id: None,
            instrument: p.instrument.clone(),
            symbol: p.symbol.clone(),
            expiry: p.expiry.clone(),
            strike: p.strike,
            is_call: p.is_call,
            security_id: None,
            lot_size: p.lot_size,
            side: if buy { "BUY" } else { "SELL" }.into(),
            product: p.product.clone(),
            order_type: "LIMIT".into(),
            validity: "DAY".into(),
            qty: p.net_qty.abs(),
            price,
            trigger_price: 0.0,
            status: "TRANSIT".into(),
            filled_qty: 0,
            avg_price: 0.0,
            message: String::new(),
            created_at: now.clone(),
            updated_at: now,
            // Booked on the expiry day, however late it runs.
            trade_date: p.expiry.clone(),
            group_id: String::new(),
            role: ROLE_SETTLE.into(),
            bracket: String::new(),
        };
        store::insert_order(&self.pool, &order).await?;
        let qty = order.qty;
        self.apply_state(order, "TRADED", qty, price, &note, "").await?;
        info!("Expiry settlement: closed {} ({} units) at {price}", p.instrument, qty);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todays_expiry_is_due_only_after_the_close() {
        assert_eq!(settle_before("2026-11-24", false), "2026-11-24");
        assert_eq!(settle_before("2026-11-24", true), "2026-11-25");
        assert_eq!(settle_before("2026-11-30", true), "2026-12-01");
        assert_eq!(settle_before("2026-12-31", true), "2027-01-01");
    }
}
