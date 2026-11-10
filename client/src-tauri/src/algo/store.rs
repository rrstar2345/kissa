//! SQLite persistence for algos, their run state and the decision journal.
//!
//! * `algos`: one JSON document per algo; `ruleset_id` is a plain column so
//!   "which algos use this signal" is a cheap query (signal delete guard).
//! * `algo_state`: armed flag, phase and the open legs. Every write is a
//!   targeted UPDATE (not a whole-row save), so the runner and the Arm button
//!   cannot overwrite each other.
//! * `algo_journal`: every decision (signals, entries, exits, skips, errors).

use anyhow::{anyhow, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use std::collections::HashMap;

use super::{Algo, AlgoState, JournalRow, Leg, PHASE_FLAT};

const JOURNAL_MAX_ROWS: i64 = 5000;
const JOURNAL_MAX_DAYS: i64 = 60;

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS algos (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            ruleset_id TEXT NOT NULL,
            json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );"#,
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_algos_ruleset ON algos(ruleset_id);")
        .execute(pool)
        .await?;
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS algo_state (
            algo_id TEXT PRIMARY KEY,
            armed INTEGER NOT NULL DEFAULT 0,
            phase TEXT NOT NULL DEFAULT 'flat',
            legs TEXT NOT NULL DEFAULT '[]',
            last_bar_ts INTEGER NOT NULL DEFAULT 0,
            entry_bar_ts INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT NOT NULL
        );"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS algo_journal (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            algo_id TEXT NOT NULL,
            algo_name TEXT NOT NULL,
            level TEXT NOT NULL,
            kind TEXT NOT NULL,
            message TEXT NOT NULL
        );"#,
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_algo_journal_algo ON algo_journal(algo_id, id);")
        .execute(pool)
        .await?;
    Ok(())
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------- algos ---

fn parse(id: String, json: &str) -> Option<Algo> {
    serde_json::from_str::<Algo>(json).ok().map(|mut a| {
        a.id = id;
        a
    })
}

/// All algos by name. Rows that no longer parse are skipped.
pub async fn list_algos(pool: &SqlitePool) -> Result<Vec<Algo>> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, json FROM algos ORDER BY name COLLATE NOCASE")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().filter_map(|(id, json)| parse(id, &json)).collect())
}

pub async fn get_algo(pool: &SqlitePool, id: &str) -> Result<Option<Algo>> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT id, json FROM algos WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|(id, json)| parse(id, &json)))
}

/// Upsert (the caller has validated). A blank id creates a new algo. Names are unique.
pub async fn save_algo(pool: &SqlitePool, mut algo: Algo) -> Result<Algo> {
    let clash: Option<(String,)> = sqlx::query_as("SELECT id FROM algos WHERE name = ? COLLATE NOCASE AND id <> ?")
        .bind(&algo.name)
        .bind(&algo.id)
        .fetch_optional(pool)
        .await?;
    if clash.is_some() {
        return Err(anyhow!("An algo named “{}” already exists", algo.name));
    }
    if algo.id.trim().is_empty() {
        algo.id = uuid::Uuid::new_v4().to_string();
    }
    let json = serde_json::to_string(&algo)?;
    let ts = now();
    sqlx::query(
        r#"INSERT INTO algos (id, name, ruleset_id, json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)
           ON CONFLICT(id) DO UPDATE SET name = excluded.name, ruleset_id = excluded.ruleset_id,
             json = excluded.json, updated_at = excluded.updated_at"#,
    )
    .bind(&algo.id)
    .bind(&algo.name)
    .bind(&algo.signal_id)
    .bind(&json)
    .bind(&ts)
    .bind(&ts)
    .execute(pool)
    .await?;
    sqlx::query("INSERT OR IGNORE INTO algo_state (algo_id, updated_at) VALUES (?, ?)")
        .bind(&algo.id)
        .bind(&ts)
        .execute(pool)
        .await?;
    Ok(algo)
}

/// Remove the algo and its state (the journal is kept).
pub async fn delete_algo(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM algos WHERE id = ?").bind(id).execute(pool).await?;
    sqlx::query("DELETE FROM algo_state WHERE algo_id = ?").bind(id).execute(pool).await?;
    Ok(())
}

/// Names of the algos that use a signal (empty = free to delete).
pub async fn names_using_signal(pool: &SqlitePool, ruleset_id: &str) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM algos WHERE ruleset_id = ? ORDER BY name COLLATE NOCASE")
        .bind(ruleset_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

// ---------------------------------------------------------------- state ---

type StateRow = (String, i64, String, String, i64, i64, String);

fn to_state(r: StateRow) -> AlgoState {
    AlgoState {
        algo_id: r.0,
        armed: r.1 != 0,
        phase: r.2,
        legs: serde_json::from_str::<Vec<Leg>>(&r.3).unwrap_or_default(),
        last_bar_ts: r.4,
        entry_bar_ts: r.5,
        updated_at: r.6,
    }
}

pub async fn get_state(pool: &SqlitePool, id: &str) -> Result<AlgoState> {
    let row: Option<StateRow> = sqlx::query_as(
        "SELECT algo_id, armed, phase, legs, last_bar_ts, entry_bar_ts, updated_at FROM algo_state WHERE algo_id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(to_state).unwrap_or_else(|| AlgoState::fresh(id)))
}

pub async fn list_states(pool: &SqlitePool) -> Result<HashMap<String, AlgoState>> {
    let rows: Vec<StateRow> = sqlx::query_as(
        "SELECT algo_id, armed, phase, legs, last_bar_ts, entry_bar_ts, updated_at FROM algo_state",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(to_state).map(|s| (s.algo_id.clone(), s)).collect())
}

/// Arm or disarm. Arming forgets the last looked-at bar so the next look
/// only sets the baseline (no trade on a bar that closed before arming).
pub async fn set_armed(pool: &SqlitePool, id: &str, armed: bool) -> Result<()> {
    sqlx::query(
        "UPDATE algo_state SET armed = ?, last_bar_ts = CASE WHEN ? = 1 THEN 0 ELSE last_bar_ts END, updated_at = ?
         WHERE algo_id = ?",
    )
    .bind(armed as i64)
    .bind(armed as i64)
    .bind(now())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_last_bar(pool: &SqlitePool, id: &str, ts: i64) -> Result<()> {
    sqlx::query("UPDATE algo_state SET last_bar_ts = ?, updated_at = ? WHERE algo_id = ?")
        .bind(ts)
        .bind(now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record an open position (`phase` in position, with its legs).
pub async fn set_position(pool: &SqlitePool, id: &str, phase: &str, legs: &[Leg], entry_bar_ts: i64) -> Result<()> {
    sqlx::query("UPDATE algo_state SET phase = ?, legs = ?, entry_bar_ts = ?, updated_at = ? WHERE algo_id = ?")
        .bind(phase)
        .bind(serde_json::to_string(legs)?)
        .bind(entry_bar_ts)
        .bind(now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_flat(pool: &SqlitePool, id: &str) -> Result<()> {
    set_position(pool, id, PHASE_FLAT, &[], 0).await
}

// -------------------------------------------------------------- journal ---

pub async fn journal_add(pool: &SqlitePool, algo_id: &str, algo_name: &str, level: &str, kind: &str, message: &str) {
    let r = sqlx::query(
        "INSERT INTO algo_journal (ts, algo_id, algo_name, level, kind, message) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(now())
    .bind(algo_id)
    .bind(algo_name)
    .bind(level)
    .bind(kind)
    .bind(message)
    .execute(pool)
    .await;
    if let Err(e) = r {
        tracing::warn!("Could not write the algo journal: {e}");
    }
}

/// Newest first, optionally for one algo.
pub async fn journal_list(pool: &SqlitePool, algo_id: Option<&str>, limit: i64) -> Result<Vec<JournalRow>> {
    Ok(sqlx::query_as::<_, JournalRow>(
        "SELECT id, ts, algo_id, algo_name, level, kind, message FROM algo_journal
         WHERE (? IS NULL OR algo_id = ?) ORDER BY id DESC LIMIT ?",
    )
    .bind(algo_id)
    .bind(algo_id)
    .bind(limit.clamp(1, 2000))
    .fetch_all(pool)
    .await?)
}

pub async fn journal_clear(pool: &SqlitePool, algo_id: Option<&str>) -> Result<()> {
    sqlx::query("DELETE FROM algo_journal WHERE (? IS NULL OR algo_id = ?)")
        .bind(algo_id)
        .bind(algo_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Keep the newest rows and nothing older than the retention window.
pub async fn journal_purge(pool: &SqlitePool) -> Result<()> {
    let cutoff = (Utc::now() - chrono::Duration::days(JOURNAL_MAX_DAYS)).to_rfc3339();
    sqlx::query("DELETE FROM algo_journal WHERE ts < ?").bind(cutoff).execute(pool).await?;
    sqlx::query("DELETE FROM algo_journal WHERE id <= (SELECT MAX(id) FROM algo_journal) - ?")
        .bind(JOURNAL_MAX_ROWS)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- scrip ---

/// Upcoming expiries of an underlying in the local Dhan instrument list
/// (empty until the list has been downloaded).
pub async fn scrip_expiries(pool: &SqlitePool, symbol: &str, from_iso: &str) -> Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT expiry FROM scrip WHERE symbol = ? AND expiry >= ? ORDER BY expiry")
            .bind(symbol.to_uppercase())
            .bind(from_iso)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}
