//! Loopback-only control API for a headless instance.
//!
//! Plain HTTP/1.1 on `127.0.0.1:<port>` (default 8787), JSON in and out, one
//! request per connection. Every request needs `Authorization: Bearer <token>`.
//! The token is `KISSA_CONTROL_TOKEN` (at least 16 characters) or, when that
//! is unset, a random one kept with the other secrets (OS keychain or the
//! encrypted file); `kissa --show-control-token` prints it. Requests that
//! carry an `Origin` header (a browser) or a Host other than 127.0.0.1 /
//! localhost are refused. Reach it from elsewhere through an SSH tunnel.
//!
//! * `GET  /v1/status`                     feeds, market, risk, algos, Fyers login
//! * `GET  /v1/accounts`                   id, kind, name
//! * `GET  /v1/algos`                      every algo with its run state
//! * `POST /v1/algos/{id}/arm | /disarm`   same checks as the Algos page
//! * `GET  /v1/risk`                       limits, halts, kill switch
//! * `POST /v1/kill`                       `{"on":true,"flatten":false}`
//! * `POST /v1/resume`                     `{"accountId":"..."}` lifts a daily-loss halt
//! * `GET  /v1/fyers/login-url?account=ID` login link (account optional: the data account)
//! * `POST /v1/fyers/login`                `{"code":"<auth code or pasted redirect URL>","accountId":"..."}`
//! * `POST /v1/shutdown`                   clean stop
//!
//! Every state change goes through the normal services as `Actor::User` (order
//! gate, risk limits and the audit journal all apply) and is journaled with
//! `via: control-api`. Secrets are never returned.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::audit::{self, Actor, Category, Entry};
use crate::market::fyers_api::Source;
use crate::state::SharedState;

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// The control token: env var, else the stored one (created on first use).
pub fn control_token() -> Result<String> {
    if let Ok(t) = std::env::var("KISSA_CONTROL_TOKEN") {
        let t = t.trim().to_string();
        if t.len() < 16 {
            return Err(anyhow!("KISSA_CONTROL_TOKEN must be at least 16 characters"));
        }
        return Ok(t);
    }
    if let Some(t) = crate::secrets::get("control", "token")?.filter(|t| t.len() >= 16) {
        return Ok(t);
    }
    let t = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    crate::secrets::set("control", "token", &t)?;
    Ok(t)
}

pub fn spawn(state: SharedState, app: AppHandle, port: u16) {
    tauri::async_runtime::spawn(async move {
        let token = match control_token() {
            Ok(t) => t,
            Err(e) => {
                warn!("Control API not started: {e}");
                return;
            }
        };
        let listener = match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                warn!("Control API not started: cannot listen on 127.0.0.1:{port}: {e}");
                return;
            }
        };
        info!("Control API on http://127.0.0.1:{port} (bearer token; `kissa --show-control-token` prints it)");
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    warn!("Control API accept failed: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let (state, app, token) = (state.clone(), app.clone(), token.clone());
            tauri::async_runtime::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(60), handle(sock, state, app, token, port)).await;
            });
        }
    });
}

// ------------------------------------------------------------------ HTTP ---

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

async fn read_request(sock: &mut TcpStream) -> Result<Request> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(anyhow!("request head too large"));
        }
        let n = tokio::time::timeout(IO_TIMEOUT, sock.read(&mut chunk)).await??;
        if n == 0 {
            return Err(anyhow!("connection closed"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("").to_uppercase();
    let target = first.next().unwrap_or("").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(anyhow!("body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = tokio::time::timeout(IO_TIMEOUT, sock.read(&mut chunk)).await??;
        if n == 0 {
            return Err(anyhow!("connection closed"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Request { method, path, query, headers, body })
}

async fn respond(sock: &mut TcpStream, code: u16, body: &Value) {
    let text = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let payload = body.to_string();
    let head = format!(
        "HTTP/1.1 {code} {text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(payload.as_bytes()).await;
    let _ = sock.shutdown().await;
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn host_ok(host: &str, port: u16) -> bool {
    let host = host.trim();
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}") || host == "127.0.0.1" || host == "localhost"
}

async fn handle(mut sock: TcpStream, state: SharedState, app: AppHandle, token: String, port: u16) {
    let req = match read_request(&mut sock).await {
        Ok(r) => r,
        Err(e) => {
            respond(&mut sock, 400, &json!({ "error": e.to_string() })).await;
            return;
        }
    };
    if req.header("origin").is_some() || !req.header("host").map(|h| host_ok(h, port)).unwrap_or(false) {
        respond(&mut sock, 403, &json!({ "error": "forbidden" })).await;
        return;
    }
    let given = req.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    if !ct_eq(given.as_bytes(), token.as_bytes()) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        respond(&mut sock, 401, &json!({ "error": "missing or wrong bearer token" })).await;
        return;
    }
    let (code, body) = match route(&req, &state, &app).await {
        Ok(v) => (200, v),
        Err((c, m)) => (c, json!({ "error": m })),
    };
    respond(&mut sock, code, &body).await;
}

// ---------------------------------------------------------------- routes ---

type Failure = (u16, String);

fn bad<E: std::fmt::Display>(e: E) -> Failure {
    (400, e.to_string())
}

fn body_json(req: &Request) -> Result<Value, Failure> {
    if req.body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&req.body).map_err(|e| bad(format!("invalid JSON body: {e}")))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| {
        urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_else(|_| v.to_string())
    })
}

fn journal(action: &str, summary: String, detail: Value) {
    audit::record(
        Entry::new(Category::System, action, summary)
            .actor(Actor::User)
            .detail(json!({ "via": "control-api", "info": detail })),
    );
}

async fn route(req: &Request, state: &SharedState, app: &AppHandle) -> Result<Value, Failure> {
    let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), parts.as_slice()) {
        ("GET", ["v1", "status"]) => status(state).await.map_err(bad),
        ("GET", ["v1", "accounts"]) => {
            let list = crate::accounts::list(&state.db).await.map_err(bad)?;
            Ok(json!(list.iter().map(|a| json!({ "id": a.id, "kind": a.kind, "name": a.name })).collect::<Vec<_>>()))
        }
        ("GET", ["v1", "algos"]) => {
            let v = crate::algo::service::list_views(&state.db).await.map_err(bad)?;
            serde_json::to_value(v).map_err(bad)
        }
        ("POST", ["v1", "algos", id, action @ ("arm" | "disarm")]) => {
            let armed = *action == "arm";
            audit::scope(Actor::User, crate::algo::service::set_armed(state, id, armed)).await.map_err(bad)?;
            let _ = app.emit("algo-update", true);
            journal("algo_control", format!("Algo {id} {action}ed"), json!({ "algoId": id }));
            let v = crate::algo::service::view(&state.db, id).await.map_err(bad)?;
            serde_json::to_value(v).map_err(bad)
        }
        ("GET", ["v1", "risk"]) => {
            let r = state.trading.risk_overview().await.map_err(bad)?;
            serde_json::to_value(r).map_err(bad)
        }
        ("POST", ["v1", "kill"]) => {
            let b = body_json(req)?;
            let on = b.get("on").and_then(Value::as_bool).ok_or_else(|| bad("body needs \"on\": true|false"))?;
            let flatten = b.get("flatten").and_then(Value::as_bool).unwrap_or(false);
            let r = audit::scope(Actor::User, state.trading.set_kill_switch(on, flatten)).await.map_err(bad)?;
            journal("kill_control", format!("Kill switch {} via control API", if on { "on" } else { "off" }), json!({ "flatten": flatten }));
            serde_json::to_value(r).map_err(bad)
        }
        ("POST", ["v1", "resume"]) => {
            let b = body_json(req)?;
            let id = b.get("accountId").and_then(Value::as_str).ok_or_else(|| bad("body needs \"accountId\""))?;
            audit::scope(Actor::User, state.trading.resume_account(id)).await.map_err(bad)?;
            Ok(json!({ "ok": true }))
        }
        ("GET", ["v1", "fyers", "login-url"]) => {
            let id = fyers_account(state, query_param(&req.query, "account")).await?;
            let url = crate::accounts::fyers_login_url(&state.db, &state.config, &id).await.map_err(bad)?;
            Ok(json!({ "accountId": id, "url": url }))
        }
        ("POST", ["v1", "fyers", "login"]) => {
            let b = body_json(req)?;
            let code = b.get("code").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty());
            let code = code.ok_or_else(|| bad("body needs \"code\" (the auth code or the pasted redirect URL)"))?;
            let id = fyers_account(state, b.get("accountId").and_then(Value::as_str).map(str::to_string)).await?;
            let result = crate::accounts::fyers_login(&state.db, &state.config, &id, code).await;
            let ok = result.is_ok();
            audit::record(
                Entry::new(
                    Category::Account,
                    if ok { "login" } else { "login_failed" },
                    if ok { "Fyers login succeeded (control API)".to_string() } else { "Fyers login failed (control API)".to_string() },
                )
                .actor(Actor::User)
                .account(id.clone())
                .subject(id.clone()),
            );
            result.map_err(bad)?;
            // Same follow-up as the desktop UI: forget old failures, test, wake the data socket.
            state.fyers_api.health.clear_source(Source::Fyers);
            let test = crate::accounts::test(&state.db, &state.config, &id).await;
            let (test_ok, message) = match test {
                Ok(t) => (t.ok, t.message),
                Err(e) => (false, e.to_string()),
            };
            if test_ok {
                state.fyers_hub.token_renewed();
            }
            Ok(json!({ "accountId": id, "loggedIn": true, "testOk": test_ok, "message": message }))
        }
        ("POST", ["v1", "shutdown"]) => {
            let (state, app) = (state.clone(), app.clone());
            tauri::async_runtime::spawn(async move {
                // Let the reply go out first.
                tokio::time::sleep(Duration::from_millis(300)).await;
                super::shutdown(&state, &app, "control API").await;
            });
            Ok(json!({ "ok": true, "message": "shutting down" }))
        }
        (_, ["v1", ..]) if req.method != "GET" && req.method != "POST" => Err((405, "method not allowed".into())),
        _ => Err((404, "not found".into())),
    }
}

/// The account to log in: the one given, else the current Fyers data account.
async fn fyers_account(state: &SharedState, given: Option<String>) -> Result<String, Failure> {
    if let Some(id) = given.map(|g| g.trim().to_string()).filter(|g| !g.is_empty()) {
        return Ok(id);
    }
    crate::accounts::fyers_auth(&state.db)
        .await
        .account_id
        .ok_or_else(|| bad("no Fyers data account is selected: pass the account id (see GET /v1/accounts)"))
}

async fn status(state: &SharedState) -> Result<Value> {
    let market = crate::commands::build_market_status(state).await;
    let auth = crate::accounts::fyers_auth(&state.db).await;
    let states = crate::algo::store::list_states(&state.db).await?;
    let (indices, feeds) = super::demand();
    let risk = state.trading.risk_overview().await?;
    Ok(json!({
        "mode": if super::is_headless() { "headless" } else { "desktop" },
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSecs": super::uptime_secs(),
        "secrets": crate::secrets::backend_name(),
        "demand": { "indexFeed": indices, "marketPolling": feeds },
        "algos": {
            "armed": states.values().filter(|s| s.armed).count(),
            "inPosition": states.values().filter(|s| s.in_position()).count(),
        },
        "fyersAuth": { "state": auth.state, "accountId": auth.account_id },
        "market": market,
        "risk": risk,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison() {
        assert!(ct_eq(b"abcdef", b"abcdef"));
        assert!(!ct_eq(b"abcdef", b"abcdeg"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn host_header_must_be_loopback() {
        assert!(host_ok("127.0.0.1:8787", 8787));
        assert!(host_ok("localhost:8787", 8787));
        assert!(!host_ok("evil.example:8787", 8787));
        assert!(!host_ok("127.0.0.1:9999", 8787));
        assert!(!host_ok("", 8787));
    }

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(query_param("account=a%20b&x=1", "account").as_deref(), Some("a b"));
        assert_eq!(query_param("x=1", "account"), None);
    }
}
