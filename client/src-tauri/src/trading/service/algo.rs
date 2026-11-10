//! Hooks for the algo runner (`crate::algo::runner`).
//!
//! * `set_algo_watch`: the (symbol, expiry) pairs armed algos trade, so the
//!   background price feed keeps their option prices live.
//! * `place_algo` / `algo_close`: orders through the normal flow (server gate,
//!   risk checks, broker rules). `propagate` decides whether the trade
//!   copier replicates the order from a master account: an algo with its
//!   copier switch off must never trigger copies.

use anyhow::{anyhow, Result};
use std::collections::HashSet;

use super::TradingService;
use crate::trading::{OrderRow, PlaceOrderInput};

impl TradingService {
    /// Replace the contracts armed algos need priced.
    pub fn set_algo_watch(&self, set: HashSet<(String, String)>) {
        if let Ok(mut w) = self.algo_watch.write() {
            *w = set;
        }
    }

    /// Place an algo order. `propagate`: let the trade copier replicate it.
    pub async fn place_algo(&self, input: PlaceOrderInput, propagate: bool) -> Result<OrderRow> {
        if propagate {
            self.place(input).await
        } else {
            self.place_inner(input).await
        }
    }

    /// Close one position of an account: cancel the contract's open orders,
    /// then a LIMIT order at the last price plus the exit buffer (as Exit does).
    pub async fn algo_close(&self, account_id: &str, instrument: &str, product: &str, propagate: bool) -> Result<OrderRow> {
        let dash = self.dashboard(Some(account_id)).await?;
        let p = dash
            .positions
            .into_iter()
            .find(|p| p.instrument == instrument && p.product == product)
            .ok_or_else(|| anyhow!("there is no open position"))?;
        Ok(self.exit_one_with(&p, propagate).await?.0)
    }
}
