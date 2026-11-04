//! Account management: paper, Dhan (via broker-proxy), the broker-proxy
//! connection itself, and Fyers data credentials.

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::time::Duration;

use crate::chain::fyers::{self, FyersCreds};
use crate::secrets;
use crate::settings::AppConfig;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
struct Row {
    id: String,
    kind: String,
    name: String,
    meta: String,
    secret_fields: String,
    created_at: String,
}

/// What the frontend sees. Secret *values* are never included, only which
/// secret fields are set.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub meta: Value,
    pub secrets_set: Vec<String>,
    pub created_at: String,
}

impl From<Row> for Account {
    fn from(r: Row) -> Self {
        Account {
            id: r.id,
            kind: r.kind,
            name: r.name,
            meta: serde_json::from_str(&r.meta).unwrap_or(Value::Null),
            secrets_set: serde_json::from_str(&r.secret_fields).unwrap_or_default(),
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveAccountInput {
    pub id: Option<String>,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub meta: Value,
    /// Blank/absent values keep the existing secret.
    #[serde(default)]
    pub secrets: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
}

fn secret_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        "proxy" => &["apiKey"],
        "dhan" | "fyers" => &["accessToken"],
        _ => &[],
    }
}

fn meta_str<'a>(meta: &'a Value, key: &str) -> &'a str {
    meta.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn validate(input: &SaveAccountInput) -> Result<()> {
    if input.name.trim().is_empty() {
        return Err(anyhow!("Name is required"));
    }
    let need = |key: &str, label: &str| -> Result<()> {
        if meta_str(&input.meta, key).is_empty() {
            Err(anyhow!("{label} is required"))
        } else {
            Ok(())
        }
    };
    match input.kind.as_str() {
        "paper" => Ok(()),
        "proxy" => {
            need("baseUrl", "Proxy URL")?;
            let url = meta_str(&input.meta, "baseUrl").to_lowercase();
            let local = url.contains("://localhost") || url.contains("://127.0.0.1");
            if !(url.starts_with("https://") || (url.starts_with("http://") && local)) {
                return Err(anyhow!(
                    "Proxy URL must use https:// (credentials travel in requests); plain http is only allowed for localhost"
                ));
            }
            Ok(())
        }
        "dhan" => {
            need("clientId", "Dhan client ID")?;
            need("proxyId", "Broker-proxy")
        }
        "fyers" => need("appId", "Fyers app ID (e.g. XXXXXXXX-100)"),
        k => Err(anyhow!("Unknown account type: {k}")),
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<Account>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, kind, name, meta, secret_fields, created_at FROM accounts ORDER BY kind, name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Account::from).collect())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Account> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT id, kind, name, meta, secret_fields, created_at FROM accounts WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.map(Account::from).ok_or_else(|| anyhow!("Account not found"))
}

pub async fn save(pool: &SqlitePool, mut input: SaveAccountInput) -> Result<Account> {
    validate(&input)?;
    if input.kind == "proxy" {
        if let Some(u) = input.meta.get("baseUrl").and_then(Value::as_str) {
            let trimmed = u.trim().trim_end_matches('/').to_string();
            input.meta["baseUrl"] = json!(trimmed);
        }
    }
    if input.kind == "paper" && input.meta.get("startingCapital").is_none() {
        input.meta = json!({ "startingCapital": 1_000_000, "brokeragePerOrder": 20 });
    }

    let now = Utc::now().to_rfc3339();
    let id = input.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let existing = if input.id.is_some() { Some(get(pool, &id).await?) } else { None };
    let mut set_fields: Vec<String> =
        existing.as_ref().map(|a| a.secrets_set.clone()).unwrap_or_default();

    let allowed = secret_fields(&input.kind);
    for (field, value) in &input.secrets {
        if !allowed.contains(&field.as_str()) || value.trim().is_empty() {
            continue;
        }
        secrets::set(&id, field, value.trim())?;
        if !set_fields.contains(field) {
            set_fields.push(field.clone());
        }
    }

    sqlx::query(
        r#"INSERT INTO accounts (id, kind, name, meta, secret_fields, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(id) DO UPDATE SET
             name = excluded.name, meta = excluded.meta,
             secret_fields = excluded.secret_fields, updated_at = excluded.updated_at"#,
    )
    .bind(&id)
    .bind(&input.kind)
    .bind(input.name.trim())
    .bind(input.meta.to_string())
    .bind(serde_json::to_string(&set_fields)?)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| {
        if e.to_string().contains("UNIQUE") {
            anyhow!("An account named “{}” already exists", input.name.trim())
        } else {
            e.into()
        }
    })?;
    get(pool, &id).await
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    let acct = get(pool, id).await?;
    for f in secret_fields(&acct.kind) {
        secrets::delete(id, f)?;
    }
    sqlx::query("DELETE FROM accounts WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

pub async fn fyers_creds(pool: &SqlitePool, id: &str) -> Result<FyersCreds> {
    let acct = get(pool, id).await?;
    if acct.kind != "fyers" {
        return Err(anyhow!("Selected data account is not a Fyers account"));
    }
    let token = secrets::get(id, "accessToken")?
        .ok_or_else(|| anyhow!("Fyers access token is not set"))?;
    Ok(FyersCreds { app_id: meta_str(&acct.meta, "appId").to_string(), access_token: token })
}

// ---------------------------------------------------------------------------
// Connection tests
// ---------------------------------------------------------------------------

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?)
}

fn proxy_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let msg = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        let e = v.get("error")?;
        Some(format!(
            "{}: {}",
            e.get("code")?.as_str()?,
            e.get("message").and_then(Value::as_str).unwrap_or("")
        ))
    });
    anyhow!("Proxy returned {}: {}", status.as_u16(), msg.unwrap_or_else(|| body.chars().take(200).collect()))
}

/// Open a session on the proxy (`POST /v1/sessions`), returning its id.
async fn open_session(base: &str, api_key: &str, body: Value) -> Result<String> {
    let resp = http()?
        .post(format!("{base}/v1/sessions"))
        .header("X-API-Key", api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach the proxy: {e}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(proxy_error(status, &text));
    }
    serde_json::from_str::<Value>(&text)?
        .get("sessionId")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("Proxy response had no sessionId"))
}

async fn close_session(base: &str, api_key: &str, session_id: &str) {
    if let Ok(c) = http() {
        let _ = c
            .delete(format!("{base}/v1/sessions/{session_id}"))
            .header("X-API-Key", api_key)
            .send()
            .await;
    }
}

pub async fn test(pool: &SqlitePool, cfg: &AppConfig, id: &str) -> Result<TestResult> {
    let acct = get(pool, id).await?;
    let ok = |m: String| Ok(TestResult { ok: true, message: m });
    match acct.kind.as_str() {
        "paper" => ok("Paper account is local; nothing to connect.".into()),
        "proxy" => {
            let base = meta_str(&acct.meta, "baseUrl").to_string();
            let key = secrets::get(id, "apiKey")?.ok_or_else(|| anyhow!("API key is not set"))?;
            let health = http()?
                .get(format!("{base}/healthz"))
                .send()
                .await
                .map_err(|e| anyhow!("Could not reach the proxy: {e}"))?;
            if !health.status().is_success() {
                return Err(anyhow!("Proxy health check returned {}", health.status()));
            }
            let sid = open_session(&base, &key, json!({ "broker": "paper" })).await?;
            close_session(&base, &key, &sid).await;
            ok("Proxy reachable and API key accepted.".into())
        }
        "dhan" => {
            let proxy = get(pool, meta_str(&acct.meta, "proxyId")).await
                .map_err(|_| anyhow!("The linked broker-proxy account no longer exists"))?;
            let base = meta_str(&proxy.meta, "baseUrl").to_string();
            let key = secrets::get(&proxy.id, "apiKey")?
                .ok_or_else(|| anyhow!("Proxy API key is not set"))?;
            let token = secrets::get(id, "accessToken")?
                .ok_or_else(|| anyhow!("Dhan access token is not set"))?;
            let sid = open_session(
                &base,
                &key,
                json!({ "broker": "dhan",
                        "credentials": { "clientId": meta_str(&acct.meta, "clientId"), "accessToken": token } }),
            )
            .await?;
            close_session(&base, &key, &sid).await;
            ok(format!("Dhan session opened and closed via “{}”.", proxy.name))
        }
        "fyers" => {
            let creds = fyers_creds(pool, id).await?;
            fyers::fetch(&cfg.fyers, &creds, "NIFTY", 1, None).await?;
            ok("Fyers credentials accepted (option chain fetched).".into())
        }
        k => Err(anyhow!("Unknown account type: {k}")),
    }
}
