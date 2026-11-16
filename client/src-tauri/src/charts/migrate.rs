//! One-off move of the `bars` table from `kissa.db` (SQLite) into the DuckDB
//! bars file (Roadmap #23). Runs at startup before anything records bars.
//!
//! Safe by construction: `kissa.db` is copied to a backup first, the SQLite
//! table is dropped only after the row counts match, and the `bars.duckdb_migrated`
//! setting marks the job as done. A failed run leaves the SQLite table alone,
//! and the next start repeats the import from scratch.

use anyhow::{anyhow, Result};
use duckdb::params;
use futures::TryStreamExt;
use sqlx::SqlitePool;
use std::path::Path;
use tracing::info;

use super::duck::BarStore;
use crate::db;

const MARKER: &str = "bars.duckdb_migrated";
const CHUNK: usize = 50_000;

type Row = (String, String, i64, f64, f64, f64, f64, f64, Option<f64>, String, Option<String>);

pub async fn import_from_sqlite(pool: &SqlitePool, bars: &BarStore, db_file: &Path) -> Result<()> {
    if db::get_setting(pool, MARKER).await?.is_some() {
        return Ok(());
    }
    let has_table: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'bars'")
        .fetch_one(pool)
        .await?;
    if has_table.0 == 0 {
        return done(pool, 0).await; // fresh install: nothing to import
    }
    let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bars").fetch_one(pool).await?;
    let total = total.0;

    if total > 0 {
        backup(pool, db_file).await?;
        info!("Moving {total} bars from SQLite to DuckDB");
        // A previous attempt may have stopped half way; start clean.
        bars.run(|c| Ok(c.execute("DELETE FROM bars", []).map(|_| ())?)).await?;

        let mut rows = sqlx::query_as::<_, Row>(
            "SELECT instrument, tf, ts, open, high, low, close, volume, oi, src, expiry_date \
             FROM bars ORDER BY instrument, tf, ts",
        )
        .fetch(pool);
        let mut chunk: Vec<Row> = Vec::with_capacity(CHUNK);
        while let Some(r) = rows.try_next().await? {
            chunk.push(r);
            if chunk.len() >= CHUNK {
                append(bars, std::mem::replace(&mut chunk, Vec::with_capacity(CHUNK))).await?;
            }
        }
        if !chunk.is_empty() {
            append(bars, chunk).await?;
        }
        drop(rows);

        let copied = bars.count().await?;
        if copied != total {
            return Err(anyhow!(
                "bars import stopped: copied {copied} of {total} rows (kissa.db is untouched; backup kept next to it)"
            ));
        }
    }

    // Counts match: the SQLite copy is no longer needed (the backup holds it).
    sqlx::query("DROP TABLE IF EXISTS bars").execute(pool).await?;
    done(pool, total).await
}

async fn done(pool: &SqlitePool, rows: i64) -> Result<()> {
    db::set_setting(pool, MARKER, &rows.to_string()).await?;
    if rows > 0 {
        info!("Bars moved to DuckDB ({rows} rows)");
    }
    Ok(())
}

async fn append(bars: &BarStore, chunk: Vec<Row>) -> Result<()> {
    bars.run(move |c| {
        let mut app = c.appender("bars")?;
        for r in &chunk {
            app.append_row(params![r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10])?;
        }
        app.flush()?;
        Ok(())
    })
    .await
}

/// Copy `kissa.db` next to itself as `kissa.pre-duckdb.bak.db` (kept if one
/// exists already: the first copy is the one taken before any change).
async fn backup(pool: &SqlitePool, db_file: &Path) -> Result<()> {
    let dest = db_file.with_file_name("kissa.pre-duckdb.bak.db");
    if dest.exists() {
        return Ok(());
    }
    // Fold the WAL into the main file so a plain file copy is complete.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(pool).await?;
    std::fs::copy(db_file, &dest).map_err(|e| anyhow!("could not back up {}: {e}", db_file.display()))?;
    info!("Backed up the database to {}", dest.display());
    Ok(())
}
