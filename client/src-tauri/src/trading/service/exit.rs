//! Exit one position / exit everything.
//!
//! Both cancel the open orders of the contract first (so a resting stop
//! cannot reopen the position), then place a LIMIT order on the opposite
//! side at the last price plus `trading.exit_buffer_ticks`. They go through
//! `place`, so the server gate, funds and broker rules apply as usual.

use anyhow::{anyhow, Result};
use serde_json::json;

use super::TradingService;
use crate::audit::{self, Category, Entry};
use crate::trading::pricing::exit_price;
use crate::trading::{store, today_ist, ExitAllResult, OrderRow, PlaceOrderInput, Position};

/// Side, whole lots and LIMIT price needed to flatten a position.
/// Pure so the maths can be tested without a database.
fn plan_exit(net_qty: i64, lot_size: i64, ltp: Option<f64>, buffer_ticks: u32) -> Result<(&'static str, i64, f64)> {
    let ltp = ltp.filter(|l| l.is_finite() && *l > 0.0).ok_or_else(|| anyhow!("no price available yet"))?;
    if lot_size <= 0 || net_qty == 0 || net_qty.abs() % lot_size != 0 {
        return Err(anyhow!("quantity is not a whole number of lots"));
    }
    let lots = net_qty.abs() / lot_size;
    let side = if net_qty > 0 { "SELL" } else { "BUY" };
    Ok((side, lots, exit_price(side, ltp, buffer_ticks)))
}

fn contract_label(p: &Position) -> String {
    format!("{} {} {} {}", p.symbol, p.expiry, p.strike, if p.is_call { "CE" } else { "PE" })
}

impl TradingService {
    /// Close one position. Returns the exit order and how many open orders
    /// were cancelled first.
    async fn exit_one(&self, p: &Position) -> Result<(OrderRow, usize)> {
        self.exit_one_with(p, true).await
    }

    /// `exit_one`; with `propagate` false the exit is not replicated by the
    /// trade copier (an algo whose copier switch is off).
    pub(super) async fn exit_one_with(&self, p: &Position, propagate: bool) -> Result<(OrderRow, usize)> {
        // Let in-flight trade-copier copies finish, then act on the position as
        // it is now: Exit all on the master and its copies must not exit twice.
        drop(self.copier_lock.lock().await);
        let now = store::open_positions(&self.pool, Some(&p.account_id), &today_ist())
            .await?
            .into_iter()
            .find(|x| x.instrument == p.instrument && x.product == p.product)
            .ok_or_else(|| anyhow!("position already closed"))?;
        let mut p = p.clone();
        p.net_qty = now.net_qty;
        let p = &p;
        let (side, lots, price) = plan_exit(p.net_qty, p.lot_size, p.ltp, self.cfg().trading.exit_buffer_ticks)?;

        let mut cancelled = 0;
        for o in store::open_orders(&self.pool, None).await? {
            if o.account_id == p.account_id && o.instrument == p.instrument && o.product == p.product {
                self.cancel(&o.id)
                    .await
                    .map_err(|e| anyhow!("could not cancel the open {} order first ({e})", o.side))?;
                cancelled += 1;
            }
        }

        let input = PlaceOrderInput {
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
            bracket: None,
        };
        let order = if propagate { self.place(input).await? } else { self.place_inner(input).await? };
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
        if out.placed > 0 || !out.skipped.is_empty() || out.cancelled > 0 {
            audit::record(
                Entry::new(
                    Category::Order,
                    "exit_all",
                    format!(
                        "Exit all{}: {} exit order(s) placed, {} open order(s) cancelled, {} skipped",
                        if account.is_some() { " (one account)" } else { "" },
                        out.placed,
                        out.cancelled,
                        out.skipped.len()
                    ),
                )
                .account(account.unwrap_or(""))
                .detail(json!({ "placed": out.placed, "cancelled": out.cancelled, "skipped": out.skipped })),
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_position_sells_below_ltp() {
        let (side, lots, price) = plan_exit(130, 65, Some(100.0), 2).unwrap();
        assert_eq!((side, lots), ("SELL", 2));
        assert!((price - 99.90).abs() < 1e-9);
    }

    #[test]
    fn short_position_buys_above_ltp() {
        let (side, lots, price) = plan_exit(-30, 30, Some(100.0), 2).unwrap();
        assert_eq!((side, lots), ("BUY", 1));
        assert!((price - 100.10).abs() < 1e-9);
    }

    #[test]
    fn zero_buffer_rounds_ltp_to_tick() {
        let (_, _, price) = plan_exit(65, 65, Some(100.03), 0).unwrap();
        assert!((price - 100.05).abs() < 1e-9);
    }

    #[test]
    fn price_floors_at_one_tick() {
        let (_, _, price) = plan_exit(65, 65, Some(0.10), 10).unwrap();
        assert!((price - 0.05).abs() < 1e-9);
    }

    #[test]
    fn missing_or_bad_price_is_rejected() {
        for ltp in [None, Some(0.0), Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            assert!(plan_exit(65, 65, ltp, 2).is_err(), "{ltp:?}");
        }
    }

    #[test]
    fn partial_lots_flat_or_bad_lot_size_are_rejected() {
        assert!(plan_exit(100, 65, Some(100.0), 2).is_err());
        assert!(plan_exit(0, 65, Some(100.0), 2).is_err());
        assert!(plan_exit(65, 0, Some(100.0), 2).is_err());
        assert!(plan_exit(65, -65, Some(100.0), 2).is_err());
    }
}
