//! Authentication against the user's broker-proxy server.
//!
//! Paper orders are simulated locally, but they are still only allowed for a
//! client the server accepts: before any order is placed the API key is
//! checked against the server. The check reuses one lightweight `paper`
//! session (`POST /v1/sessions`) and re-validates it with an authenticated
//! `GET /v1/orders`, so a revoked or wrong key is caught on every order.

use anyhow::{anyhow, Result};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::secrets;
use crate::settings::ServerConfig;

/// Keychain account id and field for the server API key.
pub const KEY_ACCOUNT: &str = "server";
pub const KEY_FIELD: &str = "apiKey";

pub fn api_key_set() -> bool {
    matches!(secrets::get(KEY_ACCOUNT, KEY_FIELD), Ok(Some(k)) if !k.is_empty())
}

struct Cached {
    /// base URL + key; the session is dropped when either changes.
    fingerprint: String,
    session_id: String,
}

pub struct ServerAuth {
    http: reqwest::Client,
    cached: Mutex<Option<Cached>>,
}

impl ServerAuth {
    pub fn new(timeout_secs: u64) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.clamp(5, 60)))
            .build()
            .unwrap_or_default();
        Self { http, cached: Mutex::new(None) }
    }

    async fn send(
        &self,
        method: Method,
        base: &str,
        path: &str,
        key: &str,
        session: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(StatusCode, String)> {
        let mut req = self.http.request(method, format!("{base}{path}")).header("X-API-Key", key);
        if let Some(s) = session {
            req = req.header("X-Session-Id", s);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| anyhow!("Could not reach the server: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    fn rejected() -> anyhow::Error {
        anyhow!("The server rejected the API key. Check it under Settings > Server connection.")
    }

    fn unexpected(status: StatusCode, body: &str) -> anyhow::Error {
        let msg = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| body.chars().take(160).collect());
        anyhow!("Server check failed (HTTP {}): {msg}", status.as_u16())
    }

    /// Fail unless the configured server accepts the stored API key.
    pub async fn verify(&self, server: &ServerConfig) -> Result<()> {
        if !server.is_set() {
            return Err(anyhow!(
                "Orders need a server connection. Enter the server URL and API key under Settings > Server connection."
            ));
        }
        let base = server.base_url().map_err(|e| anyhow!(e))?;
        let key = secrets::get(KEY_ACCOUNT, KEY_FIELD)?
            .filter(|k| !k.is_empty())
            .ok_or_else(|| anyhow!("The server API key is not set. Enter it under Settings > Server connection."))?;
        let fingerprint = format!("{base}|{key}");

        let mut guard = self.cached.lock().await;
        if let Some(c) = guard.as_ref() {
            if c.fingerprint == fingerprint {
                let (status, body) =
                    self.send(Method::GET, &base, "/v1/orders", &key, Some(&c.session_id), None).await?;
                match status {
                    s if s.is_success() => return Ok(()),
                    StatusCode::UNAUTHORIZED => {
                        *guard = None;
                        return Err(Self::rejected());
                    }
                    // Session expired or the server restarted: open a new one below.
                    StatusCode::NOT_FOUND => *guard = None,
                    s => return Err(Self::unexpected(s, &body)),
                }
            } else {
                *guard = None;
            }
        }

        let body = serde_json::json!({ "broker": "paper" });
        let (status, text) = self.send(Method::POST, &base, "/v1/sessions", &key, None, Some(&body)).await?;
        match status {
            s if s.is_success() => {
                let sid = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("sessionId").and_then(Value::as_str).map(str::to_string))
                    .ok_or_else(|| anyhow!("The server response had no sessionId"))?;
                *guard = Some(Cached { fingerprint, session_id: sid });
                Ok(())
            }
            StatusCode::UNAUTHORIZED => Err(Self::rejected()),
            s => Err(Self::unexpected(s, &text)),
        }
    }

    /// Forget the cached session (server details changed). The old session
    /// idles out on the server.
    pub async fn reset(&self) {
        *self.cached.lock().await = None;
    }
}
