//! Exit one position / exit everything.
//!
//! Both cancel the open orders of the contract first (so a resting stop
//! cannot reopen the position), then place a LIMIT order on the opposite
//! side at the last price plus `trading.exit_buffer_ticks`. They go through
//! `place`, so the server gate, funds and broker rules apply as usual.

use anyhow::{anyhow, Result};

use super::TradingService;
use crate::trading::pricing::exit_price;
use crate::trading::{store, ExitAllResult, OrderRow, PlaceOrderInput, Position};

fn contract_label(p: &Position) -> String {
    format!("{} {} {} {}", p.symbol, p.expiry, p.strike, if p.is_call { "CE" } else { "PE" })
}

impl TradingService {
    /// Close one position. Returns the exit order and how many open orders
    /// were cancelled first.
    async fn exit_one(&self, p: &Position) -> Result<(OrderRow, usize)> {
        let ltp = p.ltp.filter(|l| l.is_finite() && *l > 0.0).ok_or_else(|| anyhow!("no price available yet"))?;
        if p.lot_size <= 0 || p.net_qty == 0 || p.net_qty.abs() % p.lot_size != 0 {
            return Err(anyhow!("quantity is not a whole number of lots"));
        }
        let lots = p.net_qty.abs() / p.lot_size;
        let side = if p.net_qty > 0 { "SELL" } else { "BUY" };
        let price = exit_price(side, ltp, self.cfg.trading.exit_buffer_ticks);

        let mut cancelled = 0;
        for o in store::open_orders(&self.pool, None).await? {
            if o.account_id == p.account_id && o.instrument == p.instrument && o.product == p.product {
                self.cancel(&o.id)
                    .await
                    .map_err(|e| anyhow!("could not cancel the open {} order first ({e})", o.side))?;
                cancelled += 1;
            }
        }

        let order = self
            .place(PlaceOrderInput {
                account_id: p.account_id.clone(),
                symbol: p.symbol.clone(),
                expiry: p.expiry.clone(),
                strike: p.strike,
                is_call: p.is_call,
                side: side.into(),
                product: p.product.clone(),
                order_type: "LIMIT".into(),
                validity: "DAY".into(),
                lots,
                price,
                trigger_price: None,
            })
            .await?;
        Ok((order, cancelled))
    }

    pub async fn exit_position(&self, account_id: &str, instrument: &str, product: &str) -> Result<OrderRow> {
        let dash = self.dashboard(Some(account_id)).await?;
        let p = dash
            .positions
            .into_iter()
            .find(|p| p.instrument == instrument && p.product == product)
            .ok_or_else(|| anyhow!("There is no open position to exit"))?;
        Ok(self.exit_one(&p).await?.0)
    }

    /// Exit every open position (of one account, or all). Positions that
    /// cannot be exited are listed in `skipped` and the rest still go through.
    pub async fn exit_all(&self, account: Option<&str>) -> Result<ExitAllResult> {
        let dash = self.dashboard(account).await?;
        let mut out = ExitAllResult::default();
        for p in &dash.positions {
            match self.exit_one(p).await {
                Ok((order, cancelled)) => {
                    out.placed += 1;
                    out.cancelled += cancelled;
                    out.orders.push(order);
                }
                Err(e) => out.skipped.push(format!("{} ({}): {e}", contract_label(p), p.account_name)),
            }
        }
        Ok(out)
    }
}
