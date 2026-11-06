//! Client for the user's broker-proxy (which fronts Dhan).
//!
//! * REST for commands, one session per Dhan account (`POST /v1/sessions`).
//! * One WebSocket per session for live order updates; reconnects resume
//!   with `?since=<last seq>` and ask for a resync on a `gap`.
//! * Orders carry a `correlationId`. When the outcome of a place call is
//!   unknown (timeout, 409 `outcome_unknown`, 5xx) the caller looks the order
//!   up by correlation id instead of blindly retrying.
//!
//! No market orders: the proxy only accepts LIMIT and STOP_LOSS.

use anyhow::{anyhow, Result};
use futures::StreamExt;
use reqwest::Method;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

// ----------------------------------------------------------------- types ---

/// `null` or missing becomes the type's default.
fn nd<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// The parts of a proxy/Dhan order the client needs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxyOrder {
    #[serde(deserialize_with = "nd")]
    pub order_id: String,
    #[serde(deserialize_with = "nd")]
    pub correlation_id: String,
    #[serde(deserialize_with = "nd")]
    pub order_status: String,
    #[serde(deserialize_with = "nd")]
    pub quantity: i64,
    #[serde(deserialize_with = "nd")]
    pub price: f64,
    #[serde(deserialize_with = "nd")]
    pub trigger_price: f64,
    #[serde(deserialize_with = "nd")]
    pub filled_qty: i64,
    #[serde(deserialize_with = "nd")]
    pub average_traded_price: f64,
    #[serde(deserialize_with = "nd")]
    pub oms_error_description: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Ack {
    #[serde(deserialize_with = "nd")]
    pub order_id: String,
    #[serde(deserialize_with = "nd")]
    pub order_status: String,
    #[serde(deserialize_with = "nd")]
    pub correlation_id: String,
    pub order: Option<ProxyOrder>,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// Body of `POST /v1/orders` (the proxy rejects unknown fields).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceBody {
    pub transaction_type: String,
    pub exchange_segment: String,
    pub product_type: String,
    pub order_type: String,
    pub validity: String,
    pub security_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub trading_symbol: String,
    pub quantity: i64,
    pub price: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub trigger_price: f64,
    pub correlation_id: String,
}

/// Body of `PUT /v1/orders/{id}`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModifyBody {
    pub order_type: String,
    pub quantity: i64,
    pub price: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub trigger_price: f64,
    pub validity: String,
}

#[derive(Debug, Clone)]
pub struct ProxyError {
    /// HTTP status, 0 for transport errors.
    pub status: u16,
    pub code: String,
    pub message: String,
    /// The request may or may not have reached the broker.
    pub unknown: bool,
    /// The proxy no longer knows our session (restart / expiry).
    pub session_lost: bool,
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.code.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

impl std::error::Error for ProxyError {}

type PResult<T> = std::result::Result<T, ProxyError>;

/// What the WebSocket task reports to the trading service.
#[derive(Debug, Clone)]
pub enum DhanEvent {
    Order { account_id: String, order: ProxyOrder },
    /// Re-read the order book (first subscribe, or events were missed).
    Resync { account_id: String },
    Status { account_id: String, state: String, detail: String },
}

fn parse_error(status: u16, body: &str) -> ProxyError {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let err = v.as_ref().and_then(|v| v.get("error"));
    let code = err.and_then(|e| e.get("code")).and_then(Value::as_str).unwrap_or("").to_string();
    let mut message = err.and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("").to_string();
    if message.is_empty() {
        message = format!("HTTP {status}: {}", body.chars().take(200).collect::<String>());
    }
    // Dhan's own reason, if the proxy passed it through.
    if let Some(detail) = err.and_then(|e| e.get("detail")) {
        let reason = detail
            .get("errorMessage")
            .or_else(|| detail.get("message"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if let Some(r) = reason {
            message = format!("{message} ({r})");
        }
    }
    ProxyError {
        status,
        session_lost: status == 404 && message.to_lowercase().contains("session"),
        unknown: (status == 409 && code == "outcome_unknown") || status >= 500,
        code,
        message,
    }
}

fn parse_body<T: DeserializeOwned>(text: &str) -> PResult<T> {
    serde_json::from_str(text).map_err(|e| ProxyError {
        status: 0,
        code: "bad_response".into(),
        message: format!("Unexpected proxy response: {e}"),
        unknown: false,
        session_lost: false,
    })
}

// ------------------------------------------------------------ connection ---

pub struct DhanConn {
    pub account_id: String,
    /// Changes when the URL, key or credentials change (not logged).
    pub creds_key: String,
    base: String,
    api_key: String,
    client_id: String,
    access_token: String,
    http: reqwest::Client,
    session: Mutex<Option<String>>,
    last_seq: AtomicU64,
}

impl DhanConn {
    pub fn new(
        account_id: String,
        base: String,
        api_key: String,
        client_id: String,
        access_token: String,
        timeout_secs: u64,
    ) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(timeout_secs.max(5))).build()?;
        let creds_key = format!("{base}|{api_key}|{client_id}|{access_token}");
        Ok(Arc::new(Self {
            account_id,
            creds_key,
            base: base.trim_end_matches('/').to_string(),
            api_key,
            client_id,
            access_token,
            http,
            session: Mutex::new(None),
            last_seq: AtomicU64::new(0),
        }))
    }

    fn ws_url(&self) -> String {
        let rest = self
            .base
            .strip_prefix("https://")
            .map(|r| ("wss://", r))
            .or_else(|| self.base.strip_prefix("http://").map(|r| ("ws://", r)));
        match rest {
            Some((scheme, r)) => format!("{scheme}{r}/v1/ws"),
            None => format!("{}/v1/ws", self.base),
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>, session: Option<&str>) -> PResult<String> {
        let mut req = self.http.request(method, format!("{}{}", self.base, path)).header("X-API-Key", &self.api_key);
        if let Some(s) = session {
            req = req.header("X-Session-Id", s);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| ProxyError {
            status: 0,
            code: "network".into(),
            message: format!("Could not reach the proxy: {e}"),
            // A failed connect means nothing was sent; anything else may have been.
            unknown: !e.is_connect(),
            session_lost: false,
        })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| ProxyError {
            status,
            code: "network".into(),
            message: format!("Could not read the proxy response: {e}"),
            unknown: true,
            session_lost: false,
        })?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(parse_error(status, &text))
        }
    }

    async fn open_session(&self) -> PResult<String> {
        let body = serde_json::json!({
            "broker": "dhan",
            "credentials": { "clientId": self.client_id, "accessToken": self.access_token }
        });
        let text = self.send(Method::POST, "/v1/sessions", Some(&body), None).await.map_err(|mut e| {
            e.unknown = false; // nothing was placed
            e
        })?;
        let v: Value = parse_body(&text)?;
        v.get("sessionId").and_then(Value::as_str).map(str::to_string).ok_or_else(|| ProxyError {
            status: 0,
            code: "bad_response".into(),
            message: "Proxy response had no sessionId".into(),
            unknown: false,
            session_lost: false,
        })
    }

    /// The current session id, opening one if needed.
    pub async fn ensure_session(&self) -> PResult<String> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let sid = self.open_session().await?;
        self.last_seq.store(0, Ordering::Relaxed);
        *guard = Some(sid.clone());
        Ok(sid)
    }

    async fn forget_session(&self, bad: &str) {
        let mut guard = self.session.lock().await;
        if guard.as_deref() == Some(bad) {
            *guard = None;
        }
    }

    /// Session-scoped request. A lost session is reopened and, when
    /// `retry` is set, the call is repeated once.
    async fn call(&self, method: Method, path: &str, body: Option<&Value>, retry: bool) -> PResult<String> {
        let mut attempts = 0;
        loop {
            let sid = self.ensure_session().await?;
            match self.send(method.clone(), path, body, Some(&sid)).await {
                Err(e) if e.session_lost => {
                    self.forget_session(&sid).await;
                    if retry && attempts == 0 {
                        attempts += 1;
                        continue;
                    }
                    return Err(e);
                }
                other => return other,
            }
        }
    }

    pub async fn place(&self, body: &PlaceBody) -> PResult<Ack> {
        let json = serde_json::to_value(body).map_err(|e| ProxyError {
            status: 0,
            code: "bad_request".into(),
            message: e.to_string(),
            unknown: false,
            session_lost: false,
        })?;
        match self.call(Method::POST, "/v1/orders", Some(&json), false).await {
            Ok(text) => parse_body(&text),
            Err(e) if e.session_lost => {
                // The proxy forgot the session, and with it its idempotency
                // memory: ask the broker whether the order already exists
                // before sending it again.
                if let Some(po) = self.by_correlation(&body.correlation_id).await? {
                    return Ok(Ack {
                        order_id: po.order_id.clone(),
                        order_status: po.order_status.clone(),
                        correlation_id: po.correlation_id.clone(),
                        order: Some(po),
                    });
                }
                parse_body(&self.call(Method::POST, "/v1/orders", Some(&json), false).await?)
            }
            Err(e) => Err(e),
        }
    }

    pub async fn modify(&self, order_id: &str, body: &ModifyBody) -> PResult<Ack> {
        let json = serde_json::to_value(body).unwrap_or(Value::Null);
        let path = format!("/v1/orders/{}", urlencoding::encode(order_id));
        parse_body(&self.call(Method::PUT, &path, Some(&json), true).await?)
    }

    pub async fn cancel(&self, order_id: &str) -> PResult<Ack> {
        let path = format!("/v1/orders/{}", urlencoding::encode(order_id));
        parse_body(&self.call(Method::DELETE, &path, None, true).await?)
    }

    /// The broker's order book.
    pub async fn orders(&self) -> PResult<Vec<ProxyOrder>> {
        let text = self.call(Method::GET, "/v1/orders", None, true).await?;
        if text.trim().is_empty() || text.trim() == "null" {
            return Ok(Vec::new());
        }
        parse_body(&text)
    }

    /// `None` when the broker has no order with this correlation id.
    pub async fn by_correlation(&self, corr: &str) -> PResult<Option<ProxyOrder>> {
        let path = format!("/v1/orders/external/{}", urlencoding::encode(corr));
        match self.call(Method::GET, &path, None, true).await {
            Ok(text) => {
                if let Ok(o) = serde_json::from_str::<ProxyOrder>(&text) {
                    if !o.order_id.is_empty() {
                        return Ok(Some(o));
                    }
                }
                let list: Vec<ProxyOrder> = parse_body(&text)?;
                Ok(list.into_iter().next())
            }
            Err(e) if e.status == 404 && !e.session_lost => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn close(&self) {
        let sid = self.session.lock().await.take();
        if let Some(sid) = sid {
            let _ = self.send(Method::DELETE, &format!("/v1/sessions/{sid}"), None, None).await;
        }
    }
}

// -------------------------------------------------------------- websocket ---

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    order: Option<ProxyOrder>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    detail: String,
}

fn status(account_id: &str, state: &str, detail: &str) -> DhanEvent {
    DhanEvent::Status { account_id: account_id.into(), state: state.into(), detail: detail.into() }
}

fn handle_text(conn: &DhanConn, tx: &UnboundedSender<DhanEvent>, text: &str) {
    let Ok(ev) = serde_json::from_str::<Envelope>(text) else {
        debug!("Ignoring unparsable proxy event");
        return;
    };
    if ev.seq > 0 {
        conn.last_seq.fetch_max(ev.seq, Ordering::Relaxed);
    }
    let id = &conn.account_id;
    match ev.kind.as_str() {
        "order_update" => {
            if let Some(order) = ev.order {
                let _ = tx.send(DhanEvent::Order { account_id: id.clone(), order });
            }
        }
        "feed_status" => {
            let (state, detail) = if ev.state == "connected" {
                ("connected", "Dhan order feed connected".to_string())
            } else {
                ("feed_down", format!("Dhan order feed {}: {}", ev.state, ev.detail))
            };
            let _ = tx.send(status(id, state, &detail));
        }
        "session" if ev.state == "subscribed" || ev.state == "gap" => {
            let _ = tx.send(DhanEvent::Resync { account_id: id.clone() });
        }
        _ => {}
    }
}

async fn run_ws_once(conn: &Arc<DhanConn>, tx: &UnboundedSender<DhanEvent>) -> Result<()> {
    let sid = conn.ensure_session().await.map_err(|e| anyhow!("{e}"))?;
    let since = conn.last_seq.load(Ordering::Relaxed);
    let mut url = conn.ws_url();
    if since > 0 {
        url.push_str(&format!("?since={since}"));
    }
    let mut request = url.as_str().into_client_request().map_err(|e| anyhow!("Invalid proxy URL: {e}"))?;
    {
        let h = request.headers_mut();
        h.insert("X-API-Key", HeaderValue::from_str(&conn.api_key).map_err(|e| anyhow!("Bad API key header: {e}"))?);
        h.insert("X-Session-Id", HeaderValue::from_str(&sid).map_err(|e| anyhow!("Bad session header: {e}"))?);
    }
    let (mut ws, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(ok) => ok,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("404") {
                // Session gone: start a fresh one on the next attempt.
                conn.forget_session(&sid).await;
                conn.last_seq.store(0, Ordering::Relaxed);
            }
            return Err(anyhow!("WebSocket connection failed: {msg}"));
        }
    };
    let _ = tx.send(status(&conn.account_id, "connected", "Live order updates connected"));

    loop {
        // The proxy pings every ~25 s, so silence for 90 s means a dead link.
        let next = tokio::time::timeout(Duration::from_secs(90), ws.next()).await;
        let msg = match next {
            Err(_) => return Err(anyhow!("no data from the proxy for 90 s")),
            Ok(None) => return Ok(()),
            Ok(Some(m)) => m?,
        };
        match msg {
            Message::Text(t) => handle_text(conn, tx, &t),
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

/// Keep a WebSocket to the proxy alive, reconnecting with backoff.
pub fn spawn_ws(conn: Arc<DhanConn>, tx: UnboundedSender<DhanEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = 2u64;
        loop {
            let _ = tx.send(status(&conn.account_id, "connecting", ""));
            match run_ws_once(&conn, &tx).await {
                Ok(()) => {
                    backoff = 2;
                    let _ = tx.send(status(&conn.account_id, "disconnected", "Connection closed"));
                }
                Err(e) => {
                    warn!("Proxy WebSocket [{}]: {e}", conn.account_id);
                    let _ = tx.send(status(&conn.account_id, "error", &e.to_string()));
                }
            }
            if tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    })
}

// -------------------------------------------------------------------- hub ---

/// Live connections by account id.
#[derive(Default)]
pub struct Hub {
    conns: Mutex<HashMap<String, Arc<DhanConn>>>,
    sockets: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl Hub {
    pub async fn get(&self, account_id: &str) -> Option<Arc<DhanConn>> {
        self.conns.lock().await.get(account_id).cloned()
    }

    pub async fn insert(&self, conn: Arc<DhanConn>) {
        self.conns.lock().await.insert(conn.account_id.clone(), conn);
    }

    /// Start the WebSocket for an account unless one is already running.
    pub async fn ensure_ws(&self, conn: &Arc<DhanConn>, tx: &UnboundedSender<DhanEvent>) {
        let mut sockets = self.sockets.lock().await;
        let running = sockets.get(&conn.account_id).map(|h| !h.is_finished()).unwrap_or(false);
        if !running {
            sockets.insert(conn.account_id.clone(), spawn_ws(conn.clone(), tx.clone()));
        }
    }

    /// Stop the socket and close the session (credentials changed or removed).
    pub async fn drop_account(&self, account_id: &str) {
        if let Some(h) = self.sockets.lock().await.remove(account_id) {
            h.abort();
        }
        if let Some(c) = self.conns.lock().await.remove(account_id) {
            c.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_errors_and_flags_unknown_outcomes() {
        let e = parse_error(409, r#"{"error":{"code":"outcome_unknown","message":"earlier attempt timed out"}}"#);
        assert!(e.unknown && !e.session_lost);
        assert_eq!(e.code, "outcome_unknown");

        let e = parse_error(404, r#"{"error":{"code":"not_found","message":"session not found or expired"}}"#);
        assert!(e.session_lost && !e.unknown);

        let e = parse_error(400, r#"{"error":{"code":"market_order_not_allowed","message":"no market orders"}}"#);
        assert!(!e.unknown && !e.session_lost);
        assert_eq!(e.to_string(), "market_order_not_allowed: no market orders");

        let e = parse_error(
            424,
            r#"{"error":{"code":"broker_unauthorized","message":"token rejected","detail":{"errorMessage":"Invalid Token"}}}"#,
        );
        assert!(e.message.contains("Invalid Token"));

        assert!(parse_error(502, "<html>bad gateway</html>").unknown);
    }

    #[test]
    fn place_body_omits_empty_optionals() {
        let b = PlaceBody {
            transaction_type: "BUY".into(),
            exchange_segment: "NSE_FNO".into(),
            product_type: "INTRADAY".into(),
            order_type: "LIMIT".into(),
            validity: "DAY".into(),
            security_id: "51234".into(),
            trading_symbol: String::new(),
            quantity: 65,
            price: 100.5,
            trigger_price: 0.0,
            correlation_id: "k123".into(),
        };
        let v = serde_json::to_value(&b).unwrap();
        assert!(v.get("tradingSymbol").is_none());
        assert!(v.get("triggerPrice").is_none());
        assert_eq!(v["securityId"], "51234");
        assert_eq!(v["correlationId"], "k123");
    }

    #[test]
    fn tolerates_nulls_in_order_payloads() {
        let o: ProxyOrder = serde_json::from_str(
            r#"{"orderId":"1","orderStatus":"PENDING","filledQty":null,"averageTradedPrice":null,"price":101.5,"extra":1}"#,
        )
        .unwrap();
        assert_eq!((o.order_id.as_str(), o.filled_qty, o.price), ("1", 0, 101.5));
    }

    #[test]
    fn websocket_url_follows_the_proxy_scheme() {
        let c = DhanConn::new("a".into(), "https://proxy.example.com:8443/".into(), "k".into(), "c".into(), "t".into(), 30).unwrap();
        assert_eq!(c.ws_url(), "wss://proxy.example.com:8443/v1/ws");
        let c = DhanConn::new("a".into(), "http://localhost:8443".into(), "k".into(), "c".into(), "t".into(), 30).unwrap();
        assert_eq!(c.ws_url(), "ws://localhost:8443/v1/ws");
    }

    #[test]
    fn events_are_routed_by_type() {
        let c = DhanConn::new("acc".into(), "http://localhost:1".into(), "k".into(), "c".into(), "t".into(), 30).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        handle_text(&c, &tx, r#"{"type":"order_update","seq":7,"order":{"orderId":"9","orderStatus":"TRADED","filledQty":65}}"#);
        handle_text(&c, &tx, r#"{"type":"session","state":"gap"}"#);
        handle_text(&c, &tx, "not json");
        match rx.try_recv().unwrap() {
            DhanEvent::Order { account_id, order } => {
                assert_eq!(account_id, "acc");
                assert_eq!((order.order_id.as_str(), order.filled_qty), ("9", 65));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(rx.try_recv().unwrap(), DhanEvent::Resync { .. }));
        assert!(rx.try_recv().is_err());
        assert_eq!(c.last_seq.load(Ordering::Relaxed), 7);
    }
}
