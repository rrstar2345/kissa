//! Paper INTRADAY auto square-off, like a real broker's cut-off: at the
//! cut-off time open INTRADAY orders are cancelled and open INTRADAY
//! positions are closed at the last price (with the configured slippage).
//! Live brokers do this themselves, so only paper accounts are touched.
//!
//! The sweeper also runs it once at start-up, so positions left open when
//! the app was closed over the cut-off are closed on the next launch.

use anyhow::Result;
use chrono::Timelike;
use std::collections::HashSet;
use tracing::{debug, info, warn};

use super::TradingService;
use crate::accounts;
use crate::market::market_clock;
use crate::trading::pricing::slipped;
use crate::trading::store::{self, OpenPos};
use crate::trading::{now_ts, today_ist, OrderRow};

const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;

impl TradingService {
    pub(super) async fn auto_square_off(&self, catch_up: bool) {
        let Some(cutoff) = self.cfg.trading.square_off_minutes() else { return };
        let ist = market_clock::get_ist_now() + chrono::Duration::seconds(IST_OFFSET_SECS);
        let past_cutoff = ist.hour() * 60 + ist.minute() >= cutoff;
        if !past_cutoff && !catch_up {
            return;
        }
        if let Err(e) = self.square_off_due(past_cutoff).await {
            warn!("Auto square-off failed: {e}");
        }
    }

    async fn square_off_due(&self, past_cutoff: bool) -> Result<()> {
        let today = today_ist();
        let paper: HashSet<String> =
            accounts::list(&self.pool).await?.into_iter().filter(|a| a.kind == "paper").map(|a| a.id).collect();
        for p in store::open_positions(&self.pool, None, &today).await? {
            let due = past_cutoff || p.opened_on.as_str() < today.as_str();
            if p.product == "INTRADAY" && paper.contains(&p.account_id) && due {
                if let Err(e) = self.square_off_one(&p).await {
                    warn!("Could not square off {}: {e}", p.instrument);
                }
            }
        }
        Ok(())
    }

    async fn square_off_one(&self, p: &OpenPos) -> Result<()> {
        // A price first: without one nothing is cancelled or closed (retried next sweep).
        let ltp = match self.quote(&p.instrument) {
            Some((v, _)) => v,
            None => match store::last_close(&self.pool, &p.instrument).await {
                Some(v) => v,
                None => {
                    debug!("No price to square off {}", p.instrument);
                    return Ok(());
                }
            },
        };

        for o in store::open_orders(&self.pool, Some("paper")).await? {
            if o.account_id == p.account_id && o.instrument == p.instrument && o.product == p.product {
                if let Err(e) = self.cancel(&o.id).await {
                    warn!("Could not cancel {} before square-off: {e}", o.id);
                }
            }
        }

        let buy = p.net_qty < 0;
        let price = slipped(buy, ltp, self.cfg.trading.paper.slippage());
        let uid = uuid::Uuid::new_v4();
        let now = now_ts();
        let order = OrderRow {
            id: uid.to_string(),
            account_id: p.account_id.clone(),
            account_kind: "paper".into(),
            correlation_id: format!("sq{}", &uid.simple().to_string()[..20]),
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
            trade_date: today_ist(),
        };
        store::insert_order(&self.pool, &order).await?;
        let qty = order.qty;
        self.apply_state(order, "TRADED", qty, price, "Auto square-off", "").await?;
        info!("Auto square-off: closed {} ({} units) at {price}", p.instrument, qty);
        Ok(())
    }
}
