//! SQLite persistence for rulesets (one JSON document per row).

use anyhow::{anyhow, Result};
use chrono::Utc;
use sqlx::SqlitePool;

use super::Ruleset;

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS rulesets (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );"#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// All rulesets by name. Rows that no longer parse are skipped.
pub async fn list(pool: &SqlitePool) -> Result<Vec<Ruleset>> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, json FROM rulesets ORDER BY name COLLATE NOCASE")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, json)| {
            serde_json::from_str::<Ruleset>(&json).ok().map(|mut r| {
                r.id = id;
                r
            })
        })
        .collect())
}

/// Validate and upsert. A blank id creates a new ruleset.
pub async fn save(pool: &SqlitePool, mut rs: Ruleset) -> Result<Ruleset> {
    let errs = rs.validate();
    if !errs.is_empty() {
        return Err(anyhow!(errs.join("; ")));
    }
    rs.name = rs.name.trim().to_string();
    if rs.id.trim().is_empty() {
        rs.id = uuid::Uuid::new_v4().to_string();
    }
    let json = serde_json::to_string(&rs)?;
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        r#"INSERT INTO rulesets (id, name, json, created_at, updated_at) VALUES (?, ?, ?, ?, ?)
           ON CONFLICT(id) DO UPDATE SET name = excluded.name, json = excluded.json, updated_at = excluded.updated_at"#,
    )
    .bind(&rs.id)
    .bind(&rs.name)
    .bind(&json)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(rs)
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM rulesets WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}
