//! Read models: dashboard, diary, order / trade lists and quotes.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;

use super::TradingService;
use crate::accounts;
use crate::charts::instrument::option_key;
use crate::market::market_clock;
use crate::trading::pricing::{exit_price, r2};
use crate::trading::{
    now_ts, store, today_ist, AccountSummary, Dashboard, DiaryDay, OrderRow, Position, QuoteView, TradeRow,
};

/// A price older than this (seconds) is shown as "last", not "live".
const FRESH_SECS: i64 = 120;

impl TradingService {
    /// (price, fresh) from the live store.
    pub(super) fn quote(&self, key: &str) -> Option<(f64, bool)> {
        let q = self.ltp.get(key)?;
        let age = market_clock::get_ist_now().timestamp() - q.at;
        Some((q.ltp, age <= FRESH_SECS))
    }

    /// Latest price of a contract for the order ticket.
    pub fn quote_view(&self, symbol: &str, expiry: &str, strike: f64, is_call: bool) -> QuoteView {
        let key = option_key(&symbol.trim().to_uppercase(), expiry, strike, is_call);
        match self.ltp.get(&key) {
            Some(q) => {
                let age = (market_clock::get_ist_now().timestamp() - q.at).max(0);
                QuoteView { ltp: Some(q.ltp), age_secs: Some(age), live: age <= FRESH_SECS }
            }
            None => QuoteView { ltp: None, age_secs: None, live: false },
        }
    }

    pub async fn dashboard(&self, account: Option<&str>) -> Result<Dashboard> {
        let today = today_ist();
        let all = accounts::list(&self.pool).await?;
        let accts: Vec<_> = all
            .into_iter()
            .filter(|a| self.brokers.has(&a.kind) && account.map(|x| x == a.id).unwrap_or(true))
            .collect();
        let names: HashMap<String, (String, String)> =
            accts.iter().map(|a| (a.id.clone(), (a.name.clone(), a.kind.clone()))).collect();

        let open = store::open_positions(&self.pool, account, &today).await?;
        let totals = store::day_totals(&self.pool, &today).await?;
        let order_counts = store::order_counts(&self.pool, &today).await?;
        let buffer = self.cfg.trading.exit_buffer_ticks;

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
            let exit_side = if p.net_qty > 0 { "SELL" } else { "BUY" };
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
                exit_price: ltp.filter(|l| *l > 0.0).map(|l| exit_price(exit_side, l, buffer)),
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
