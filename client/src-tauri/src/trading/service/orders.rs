//! Place, modify and cancel: validation, then routing to the broker that
//! serves the account.

use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use std::time::Duration;
use tracing::debug;

use super::bracket::validate_bracket;
use super::TradingService;
use crate::accounts;
use crate::charts::instrument::option_key;
use crate::trading::broker::{BrokerEvent, Contract};
use crate::trading::pricing::on_tick;
use crate::trading::{
    is_open_status, now_ts, scrip, store, today_ist, ContractInfo, OrderRow, PlaceOrderInput, ROLE_ENTRY,
};

/// A validated, normalised order request.
struct Checked {
    symbol: String,
    side: String,
    product: String,
    order_type: String,
    validity: String,
    trigger: f64,
}

fn validate(input: &PlaceOrderInput) -> Result<Checked> {
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
    let symbol = input.symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err(anyhow!("Symbol is required"));
    }
    Ok(Checked { symbol, side, product, order_type, validity, trigger })
}

impl TradingService {
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

    /// Lot size, plus the securityId and trading symbol when the broker needs them.
    async fn resolve_contract(
        &self,
        symbol: &str,
        expiry: &str,
        strike: f64,
        is_call: bool,
        need_ids: bool,
    ) -> Result<Contract> {
        let mut found = scrip::lookup(&self.pool, symbol, expiry, strike, is_call).await?;
        if found.is_none() && need_ids {
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
            return Ok(Contract { lot_size: c.lot_size, security_id: Some(c.security_id), trading_symbol: ts });
        }
        let info = self.contract_info(symbol, expiry, strike, is_call).await?;
        let lot_size = info
            .lot_size
            .ok_or_else(|| anyhow!("Lot size for {symbol} is unknown. Refresh the instrument list, or add it to trading.fallback_lot_sizes in settings.json."))?;
        Ok(Contract { lot_size, security_id: None, trading_symbol: None })
    }

    // ---------------------------------------------------------------- place ---

    pub async fn place(&self, input: PlaceOrderInput) -> Result<OrderRow> {
        let c = validate(&input)?;
        if input.expiry.as_str() < today_ist().as_str() {
            return Err(anyhow!("This contract has expired"));
        }

        // No order of any kind (paper included) without an authenticated server.
        self.require_server_auth().await?;

        let acct = accounts::get(&self.pool, &input.account_id).await?;
        let broker = self.broker_for_kind(&acct.kind)?;

        // Stop-loss / target legs (paper only; a live broker keeps its own bracket rules).
        let bracket = match &input.bracket {
            Some(b) => {
                let checked = validate_bracket(&c.side, input.price, b, self.cfg.trading.exit_buffer_ticks)?;
                if checked.is_some() && acct.kind != "paper" {
                    return Err(anyhow!("Bracket orders (stop-loss / target) are available on paper accounts only"));
                }
                checked
            }
            None => None,
        };
        let contract = self
            .resolve_contract(&c.symbol, &input.expiry, input.strike, input.is_call, broker.needs_instrument_ids())
            .await?;

        // Risk controls: kill switch, halts, lot / position / window / loss
        // limits. Orders that only reduce a position always pass.
        self.check_risk(
            &acct,
            &c.side,
            &c.product,
            &option_key(&c.symbol, &input.expiry, input.strike, input.is_call),
            input.lots,
            contract.lot_size * input.lots,
        )
        .await?;

        let uid = uuid::Uuid::new_v4();
        let now = now_ts();
        let mut order = OrderRow {
            id: uid.to_string(),
            account_id: acct.id.clone(),
            account_kind: acct.kind.clone(),
            correlation_id: format!("k{}", &uid.simple().to_string()[..20]),
            broker_order_id: None,
            instrument: option_key(&c.symbol, &input.expiry, input.strike, input.is_call),
            symbol: c.symbol,
            expiry: input.expiry.clone(),
            strike: input.strike,
            is_call: input.is_call,
            security_id: contract.security_id.clone(),
            lot_size: contract.lot_size,
            side: c.side,
            product: c.product,
            order_type: c.order_type,
            validity: c.validity,
            qty: contract.lot_size * input.lots,
            price: input.price,
            trigger_price: c.trigger,
            status: "TRANSIT".into(),
            filled_qty: 0,
            avg_price: 0.0,
            message: String::new(),
            created_at: now.clone(),
            updated_at: now,
            trade_date: today_ist(),
            group_id: if bracket.is_some() { uid.to_string() } else { String::new() },
            role: if bracket.is_some() { ROLE_ENTRY.into() } else { String::new() },
            bracket: match &bracket {
                Some(b) => serde_json::to_string(b)?,
                None => String::new(),
            },
        };

        // Market hours, funds, credentials: a failure leaves no stray order.
        broker.pre_trade(&order, &contract).await?;

        store::insert_order(&self.pool, &order).await?;
        self.emit_order(&order);

        match broker.place(&order, &contract).await {
            Ok(reply) => self.apply_reply(order, reply).await,
            Err(e) if e.unknown => {
                order.status = "UNKNOWN".into();
                order.message = format!("Outcome unknown ({e}). Checking with the broker…");
                store::save_order_state(&self.pool, &order).await?;
                order = store::get_order(&self.pool, &order.id).await?.unwrap_or(order);
                self.emit_order(&order);
                self.resolve_unknown_later(order.id.clone());
                Ok(order)
            }
            Err(e) => {
                order.status = "REJECTED".into();
                order.message = e.to_string();
                store::save_order_state(&self.pool, &order).await?;
                order = store::get_order(&self.pool, &order.id).await?.unwrap_or(order);
                self.emit_order(&order);
                self.alert_rejected(&order).await;
                Ok(order)
            }
        }
    }

    /// After a place call with an unknown outcome: ask the broker a few
    /// times (a `Resync` re-reads the order book, which matches orders by
    /// correlation id), then leave the order as UNKNOWN.
    fn resolve_unknown_later(&self, order_id: String) {
        let tx = self.events_tx.clone();
        let pool = self.pool.clone();
        tokio::spawn(async move {
            for delay in [2u64, 5, 10] {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                let Ok(Some(o)) = store::get_order(&pool, &order_id).await else { return };
                if o.status != "UNKNOWN" {
                    return;
                }
                let _ = tx.send(BrokerEvent::Resync { account_id: o.account_id.clone() });
            }
            debug!("Order {order_id} is still UNKNOWN after retries");
        });
    }

    // ------------------------------------------------------- cancel / modify ---

    pub async fn cancel(&self, id: &str) -> Result<OrderRow> {
        let o = store::get_order(&self.pool, id).await?.ok_or_else(|| anyhow!("Order not found"))?;
        if !is_open_status(&o.status) {
            return Err(anyhow!("The order is already {}", o.status));
        }
        let broker = self.broker_for_kind(&o.account_kind)?;
        let reply = broker.cancel(&o).await?;
        self.apply_reply(o, reply).await
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

        let new_trigger = if o.order_type == "STOP_LOSS" { trigger } else { 0.0 };
        let broker = self.broker_for_kind(&o.account_kind)?;
        let reply = broker.modify(&o, price, new_trigger).await?;
        o.price = price;
        o.trigger_price = new_trigger;
        self.apply_reply(o, reply).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> PlaceOrderInput {
        PlaceOrderInput {
            account_id: "a".into(),
            symbol: " nifty ".into(),
            expiry: "2099-01-29".into(),
            strike: 24000.0,
            is_call: true,
            side: "buy".into(),
            product: "intraday".into(),
            order_type: "limit".into(),
            validity: "day".into(),
            lots: 1,
            price: 100.0,
            trigger_price: None,
            bracket: None,
        }
    }

    #[test]
    fn valid_orders_are_normalised() {
        let c = validate(&input()).unwrap();
        assert_eq!((c.symbol.as_str(), c.side.as_str(), c.product.as_str()), ("NIFTY", "BUY", "INTRADAY"));
    }

    #[test]
    fn bracket_input_is_optional_in_the_request() {
        let json = r#"{"accountId":"a","symbol":"NIFTY","expiry":"2099-01-29","strike":24000,"isCall":true,
            "side":"BUY","product":"INTRADAY","orderType":"LIMIT","validity":"DAY","lots":1,"price":100}"#;
        let plain: PlaceOrderInput = serde_json::from_str(json).unwrap();
        assert!(plain.bracket.is_none());
        let with = json.replace("\"price\":100", "\"price\":100,\"bracket\":{\"target\":120,\"stopTrigger\":90}");
        let b = serde_json::from_str::<PlaceOrderInput>(&with).unwrap().bracket.unwrap();
        assert_eq!((b.target, b.stop_trigger, b.stop_price), (Some(120.0), Some(90.0), None));
    }

    #[test]
    fn bad_orders_are_refused() {
        let mut i = input();
        i.price = 100.01;
        assert!(validate(&i).is_err()); // off tick
        let mut i = input();
        i.order_type = "STOP_LOSS".into();
        assert!(validate(&i).is_err()); // no trigger
        let mut i = input();
        i.order_type = "MARKET".into();
        assert!(validate(&i).is_err());
        let mut i = input();
        i.lots = 0;
        assert!(validate(&i).is_err());
        let mut i = input();
        i.expiry = "29-01-2099".into();
        assert!(validate(&i).is_err());
    }
}
