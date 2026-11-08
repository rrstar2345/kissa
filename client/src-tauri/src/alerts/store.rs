//! Alert history (`alert_outbox`): every alert that was sent, failed,
//! suppressed as a duplicate or skipped because no channel was enabled.

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::trading::now_ts;

pub const SENDING: &str = "sending";
pub const SENT: &str = "sent";
pub const PARTIAL: &str = "partial";
pub const FAILED: &str = "failed";
pub const SUPPRESSED: &str = "suppressed";
pub const SKIPPED: &str = "skipped";

const KEEP_ROWS: i64 = 2000;
const KEEP_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AlertRow {
    pub id: i64,
    pub at: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    /// `sending` | `sent` | `partial` | `failed` | `suppressed` | `skipped`
    pub status: String,
    /// Per-target result lines.
    pub detail: String,
    pub attempts: i64,
}

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS alert_outbox (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            at TEXT NOT NULL,
            kind TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL,
            detail TEXT NOT NULL DEFAULT '',
            attempts INTEGER NOT NULL DEFAULT 0
        );"#,
    )
    .execute(pool)
    .await?;
    // Deliveries cut short by closing the app can no longer finish.
    sqlx::query(
        "UPDATE alert_outbox SET status = 'failed', detail = 'The app was closed before delivery finished.' WHERE status = 'sending'",
    )
    .execute(pool)
    .await?;
    prune(pool).await
}

/// Keep the last 2000 rows and 30 days.
pub async fn prune(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM alert_outbox WHERE id NOT IN (SELECT id FROM alert_outbox ORDER BY id DESC LIMIT ?)")
        .bind(KEEP_ROWS)
        .execute(pool)
        .await?;
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(KEEP_DAYS)).to_rfc3339();
    sqlx::query("DELETE FROM alert_outbox WHERE at < ?").bind(cutoff).execute(pool).await?;
    Ok(())
}

pub async fn insert(pool: &SqlitePool, kind: &str, title: &str, body: &str, status: &str, detail: &str) -> Result<i64> {
    let res = sqlx::query("INSERT INTO alert_outbox (at, kind, title, body, status, detail) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(now_ts())
        .bind(kind)
        .bind(title)
        .bind(body)
        .bind(status)
        .bind(detail)
        .execute(pool)
        .await?;
    Ok(res.last_insert_rowid())
}

pub async fn finish(pool: &SqlitePool, id: i64, status: &str, detail: &str, attempts: i64) -> Result<()> {
    sqlx::query("UPDATE alert_outbox SET status = ?, detail = ?, attempts = ? WHERE id = ?")
        .bind(status)
        .bind(detail)
        .bind(attempts)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list(pool: &SqlitePool, limit: i64) -> Result<Vec<AlertRow>> {
    Ok(sqlx::query_as("SELECT id, at, kind, title, body, status, detail, attempts FROM alert_outbox ORDER BY id DESC LIMIT ?")
        .bind(limit.clamp(1, 500))
        .fetch_all(pool)
        .await?)
}

pub async fn clear(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM alert_outbox").execute(pool).await?;
    Ok(())
}
