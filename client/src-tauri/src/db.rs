//! Local SQLite store: accounts (non-secret fields), app settings and
//! column/layout preferences. Secrets live in the OS keychain.

use anyhow::Result;
use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::path::Path;

pub async fn init_pool(db_file: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new().filename(db_file).create_if_missing(true);
    let pool = SqlitePool::connect_with(opts).await?;
    sqlx::query("PRAGMA journal_mode = WAL;").execute(&pool).await?;
    sqlx::query("PRAGMA synchronous = NORMAL;").execute(&pool).await?;
    sqlx::query("PRAGMA foreign_keys = ON;").execute(&pool).await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );"#,
    )
    .execute(&pool)
    .await?;

    // kind: paper | dhan | proxy | fyers. `meta` holds non-secret fields as
    // JSON; `secret_fields` lists which secrets are stored in the keychain.
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS accounts (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL CHECK (kind IN ('paper','dhan','proxy','fyers')),
            name TEXT NOT NULL UNIQUE,
            meta TEXT NOT NULL DEFAULT '{}',
            secret_fields TEXT NOT NULL DEFAULT '[]',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );"#,
    )
    .execute(&pool)
    .await?;
    crate::charts::store::init(&pool).await?;
    crate::charts::rules::store::init(&pool).await?;
    crate::trading::scrip::init(&pool).await?;
    crate::trading::store::init(&pool).await?;
    crate::trading::risk::init(&pool).await?;
    crate::trading::copier::init(&pool).await?;
    crate::alerts::store::init(&pool).await?;
    Ok(pool)
}

pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM app_settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
}

pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO app_settings (key, value, updated_at) VALUES (?, ?, ?)
           ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at"#,
    )
    .bind(key)
    .bind(value)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}
