//! Dhan through the user's broker-proxy.
//!
//! `proxy` is the raw REST / WebSocket client. This file adapts it to the
//! `Broker` trait: it keeps one proxy session per Dhan account, turns proxy
//! order events into `BrokerEvent`s, and maps proxy errors onto
//! rejected / unknown outcomes.

pub mod proxy;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::mpsc::unbounded_channel;

use super::{Broker, BrokerError, BrokerEvent, BrokerResult, Contract, EventTx, OrderUpdate, Reply};
use crate::accounts;
use crate::secrets;
use crate::settings::TradingConfig;
use crate::trading::charges;
use crate::trading::{OrderRow, SEGMENT_FNO};
use proxy::{Ack, DhanConn, DhanEvent, Hub, ModifyBody, PlaceBody, ProxyError, ProxyOrder};

pub struct DhanBroker {
    pool: SqlitePool,
    cfg: TradingConfig,
    hub: Hub,
    /// Proxy events go here and are translated to `BrokerEvent`s.
    proxy_tx: tokio::sync::mpsc::UnboundedSender<DhanEvent>,
    tx: EventTx,
}

fn to_update(account_id: String, po: ProxyOrder) -> OrderUpdate {
    OrderUpdate {
        account_id,
        local_id: None,
        broker_order_id: po.order_id,
        correlation_id: po.correlation_id,
        status: po.order_status,
        filled_qty: po.filled_qty,
        avg_price: po.average_traded_price,
        message: po.oms_error_description,
        price: po.price,
        trigger_price: po.trigger_price,
    }
}

fn translate(ev: DhanEvent) -> BrokerEvent {
    match ev {
        DhanEvent::Order { account_id, order } => BrokerEvent::Order(to_update(account_id, order)),
        DhanEvent::Resync { account_id } => BrokerEvent::Resync { account_id },
        DhanEvent::Status { account_id, state, detail } => BrokerEvent::Status { account_id, state, detail },
    }
}

fn map_err(e: ProxyError) -> BrokerError {
    BrokerError { message: e.to_string(), unknown: e.unknown }
}

fn reply_from(ack: Ack, fallback_status: &str, message: &str) -> Reply {
    match &ack.order {
        Some(po) => Reply {
            broker_order_id: ack.order_id.clone(),
            status: if po.order_status.is_empty() { ack.order_status.clone() } else { po.order_status.clone() },
            filled_qty: po.filled_qty,
            avg_price: po.average_traded_price,
            message: if po.oms_error_description.is_empty() { message.to_string() } else { po.oms_error_description.clone() },
        },
        None => Reply {
            broker_order_id: ack.order_id.clone(),
            status: if ack.order_status.is_empty() { fallback_status.to_string() } else { ack.order_status.clone() },
            filled_qty: 0,
            avg_price: 0.0,
            message: message.to_string(),
        },
    }
}

impl DhanBroker {
    pub fn new(pool: SqlitePool, cfg: &TradingConfig, tx: EventTx) -> Arc<Self> {
        let (proxy_tx, mut proxy_rx) = unbounded_channel::<DhanEvent>();
        let out = tx.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = proxy_rx.recv().await {
                if out.send(translate(ev)).is_err() {
                    break;
                }
            }
        });
        Arc::new(Self { pool, cfg: cfg.clone(), hub: Hub::default(), proxy_tx, tx })
    }

    /// The cached connection for an account, rebuilt when credentials change.
    async fn conn(&self, account_id: &str) -> Result<Arc<DhanConn>> {
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
        let conn = DhanConn::new(account_id.to_string(), base, api_key, client_id, token, self.cfg.request_timeout_secs)?;
        self.hub.insert(conn.clone()).await;
        Ok(conn)
    }
}

#[async_trait]
impl Broker for DhanBroker {
    fn kind(&self) -> &'static str {
        "dhan"
    }

    fn needs_instrument_ids(&self) -> bool {
        true
    }

    /// Fail early on missing credentials, before anything is stored.
    async fn pre_trade(&self, o: &OrderRow, contract: &Contract) -> Result<()> {
        if contract.security_id.is_none() {
            return Err(anyhow!("Dhan securityId is missing"));
        }
        self.conn(&o.account_id).await.map(|_| ())
    }

    async fn place(&self, o: &OrderRow, contract: &Contract) -> BrokerResult<Reply> {
        let conn = self.conn(&o.account_id).await?;
        let security_id =
            contract.security_id.clone().ok_or_else(|| BrokerError::rejected("Dhan securityId is missing"))?;
        let body = PlaceBody {
            transaction_type: o.side.clone(),
            exchange_segment: SEGMENT_FNO.into(),
            product_type: o.product.clone(),
            order_type: o.order_type.clone(),
            validity: o.validity.clone(),
            security_id,
            trading_symbol: contract.trading_symbol.clone().unwrap_or_default(),
            quantity: o.qty,
            price: o.price,
            trigger_price: o.trigger_price,
            correlation_id: o.correlation_id.clone(),
        };
        let result = conn.place(&body).await;
        self.hub.ensure_ws(&conn, &self.proxy_tx).await;
        result.map(|ack| reply_from(ack, "TRANSIT", "")).map_err(map_err)
    }

    async fn modify(&self, o: &OrderRow, price: f64, trigger_price: f64) -> BrokerResult<Reply> {
        let bid = o
            .broker_order_id
            .clone()
            .ok_or_else(|| BrokerError::rejected("The broker has not confirmed this order yet; try again in a moment"))?;
        let conn = self.conn(&o.account_id).await?;
        let body = ModifyBody {
            order_type: o.order_type.clone(),
            quantity: o.qty,
            price,
            trigger_price: if o.order_type == "STOP_LOSS" { trigger_price } else { 0.0 },
            validity: o.validity.clone(),
        };
        let ack = conn.modify(&bid, &body).await.map_err(map_err)?;
        Ok(reply_from(ack, &o.status, "Modified"))
    }

    async fn cancel(&self, o: &OrderRow) -> BrokerResult<Reply> {
        let bid = o
            .broker_order_id
            .clone()
            .ok_or_else(|| BrokerError::rejected("The broker has not confirmed this order yet; try again in a moment"))?;
        let conn = self.conn(&o.account_id).await?;
        let ack = conn.cancel(&bid).await.map_err(map_err)?;
        self.hub.ensure_ws(&conn, &self.proxy_tx).await;
        // A cancel ack carries no fill information: report none.
        let mut reply = reply_from(ack, &o.status, "Cancel requested");
        reply.filled_qty = 0;
        reply.avg_price = 0.0;
        reply.message = "Cancel requested".into();
        Ok(reply)
    }

    async fn brokerage(&self, _o: &OrderRow, price: f64, qty: i64) -> f64 {
        charges::dhan_brokerage(self.cfg.dhan_brokerage_per_order, self.cfg.dhan_brokerage_cap_pct, price * qty as f64)
    }

    async fn connect(&self, account_id: &str) -> Result<()> {
        let conn = self.conn(account_id).await?;
        conn.ensure_session().await.map_err(|e| anyhow!("{e}"))?;
        self.hub.ensure_ws(&conn, &self.proxy_tx).await;
        Ok(())
    }

    async fn sync(&self, account_id: &str) -> Result<usize> {
        let conn = self.hub.get(account_id).await.ok_or_else(|| anyhow!("Not connected"))?;
        let orders = conn.orders().await.map_err(|e| anyhow!("{e}"))?;
        let n = orders.len();
        for po in orders {
            let _ = self.tx.send(BrokerEvent::Order(to_update(account_id.to_string(), po)));
        }
        Ok(n)
    }

    async fn forget(&self, account_id: &str) {
        self.hub.drop_account(account_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_events_are_translated() {
        let po = ProxyOrder { order_id: "9".into(), order_status: "TRADED".into(), filled_qty: 65, ..ProxyOrder::default() };
        match translate(DhanEvent::Order { account_id: "acc".into(), order: po }) {
            BrokerEvent::Order(u) => {
                assert_eq!((u.account_id.as_str(), u.broker_order_id.as_str(), u.filled_qty), ("acc", "9", 65));
                assert!(u.local_id.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(translate(DhanEvent::Resync { account_id: "a".into() }), BrokerEvent::Resync { .. }));
    }

    #[test]
    fn ack_without_order_uses_the_fallback_status() {
        let ack = Ack { order_id: "1".into(), ..Ack::default() };
        let r = reply_from(ack, "TRANSIT", "x");
        assert_eq!((r.status.as_str(), r.broker_order_id.as_str(), r.message.as_str()), ("TRANSIT", "1", "x"));
    }
}
