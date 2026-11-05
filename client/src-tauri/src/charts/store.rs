//! SQLite persistence for bars: schema, upserts, chart queries, daily rollup
//! and retention.
//!
//! One table, `bars(instrument, tf, ts)`, with `tf` = `1m` | `1d`. Live
//! ticks are folded into 1m bars in memory (see `recorder`) so no raw tick
//! table is kept. Other intervals are resampled on read.

use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::{error, info};

use super::instrument::{self, Instrument};
use super::resample::{self, Interval};
use super::{Bar, InstrumentList, OptionExpiry};
use crate::settings::ChartsConfig;

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS bars (
            instrument TEXT NOT NULL,
            tf TEXT NOT NULL CHECK (tf IN ('1m','1d')),
            ts INTEGER NOT NULL,
            open REAL NOT NULL,
            high REAL NOT NULL,
            low REAL NOT NULL,
            close REAL NOT NULL,
            volume REAL NOT NULL DEFAULT 0,
            oi REAL,
            src TEXT NOT NULL DEFAULT 'live',
            expiry_date TEXT,
            PRIMARY KEY (instrument, tf, ts)
        ) WITHOUT ROWID;"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_bars_expiry ON bars(expiry_date) WHERE expiry_date IS NOT NULL;",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Merge live 1m bars into the table. A restart mid-minute keeps the
/// earlier open/high/low instead of overwriting them.
pub async fn upsert_live(pool: &SqlitePool, batch: &[(String, Bar)]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for (inst, b) in batch {
        sqlx::query(
            r#"INSERT INTO bars (instrument, tf, ts, open, high, low, close, volume, oi, src, expiry_date)
               VALUES (?, '1m', ?, ?, ?, ?, ?, ?, ?, 'live', ?)
               ON CONFLICT (instrument, tf, ts) DO UPDATE SET
                 high = MAX(high, excluded.high),
                 low = MIN(low, excluded.low),
                 close = excluded.close,
                 volume = MAX(volume, excluded.volume),
                 oi = COALESCE(excluded.oi, oi)"#,
        )
        .bind(inst)
        .bind(b.ts)
        .bind(b.open)
        .bind(b.high)
        .bind(b.low)
        .bind(b.close)
        .bind(b.volume)
        .bind(b.oi)
        .bind(instrument::expiry_of(inst))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Insert historical bars without touching rows that already exist.
pub async fn insert_backfill(
    pool: &SqlitePool,
    inst: &str,
    tf: &str,
    bars: &[Bar],
) -> Result<usize> {
    let expiry = instrument::expiry_of(inst);
    let mut inserted = 0usize;
    let mut tx = pool.begin().await?;
    for b in bars {
        let r = sqlx::query(
            r#"INSERT OR IGNORE INTO bars
               (instrument, tf, ts, open, high, low, close, volume, oi, src, expiry_date)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'fyers', ?)"#,
        )
        .bind(inst)
        .bind(tf)
        .bind(b.ts)
        .bind(b.open)
        .bind(b.high)
        .bind(b.low)
        .bind(b.close)
        .bind(b.volume)
        .bind(b.oi)
        .bind(&expiry)
        .execute(&mut *tx)
        .await?;
        inserted += r.rows_affected() as usize;
    }
    tx.commit().await?;
    Ok(inserted)
}

fn row_to_bar(r: &sqlx::sqlite::SqliteRow) -> Result<Bar> {
    Ok(Bar {
        ts: r.try_get("ts")?,
        open: r.try_get("open")?,
        high: r.try_get("high")?,
        low: r.try_get("low")?,
        close: r.try_get("close")?,
        volume: r.try_get("volume")?,
        oi: r.try_get("oi")?,
    })
}

/// Latest `limit` bars of `tf`, oldest first.
async fn latest(pool: &SqlitePool, inst: &str, tf: &str, limit: i64) -> Result<Vec<Bar>> {
    let rows = sqlx::query(
        "SELECT ts, open, high, low, close, volume, oi FROM bars \
         WHERE instrument = ? AND tf = ? ORDER BY ts DESC LIMIT ?",
    )
    .bind(inst)
    .bind(tf)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut bars = rows.iter().map(row_to_bar).collect::<Result<Vec<_>>>()?;
    bars.reverse();
    Ok(bars)
}

async fn since(pool: &SqlitePool, inst: &str, tf: &str, from_ts: i64) -> Result<Vec<Bar>> {
    let rows = sqlx::query(
        "SELECT ts, open, high, low, close, volume, oi FROM bars \
         WHERE instrument = ? AND tf = ? AND ts >= ? ORDER BY ts ASC",
    )
    .bind(inst)
    .bind(tf)
    .bind(from_ts)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_bar).collect()
}

/// Bars for charting at any supported interval, oldest first.
pub async fn chart_bars(
    pool: &SqlitePool,
    inst: &str,
    interval: Interval,
    limit: usize,
) -> Result<Vec<Bar>> {
    let limit = limit.clamp(1, 20_000);
    match interval {
        Interval::Min(1) => latest(pool, inst, "1m", limit as i64).await,
        Interval::Min(n) => {
            let raw = latest(pool, inst, "1m", (limit as i64 * n).min(150_000)).await?;
            let mut out = resample::resample(&raw, interval);
            if out.len() > limit {
                out.drain(..out.len() - limit);
            }
            Ok(out)
        }
        Interval::Day => {
            let mut daily = latest(pool, inst, "1d", limit as i64).await?;
            // Days not rolled up yet (today, or recent days) come from 1m.
            let from = daily.last().map(|b| b.ts).unwrap_or(0);
            let minutes = since(pool, inst, "1m", from).await?;
            for d in resample::resample(&minutes, Interval::Day) {
                match daily.last_mut() {
                    Some(last) if last.ts == d.ts => {
                        last.high = last.high.max(d.high);
                        last.low = last.low.min(d.low);
                        last.close = d.close;
                        last.volume = last.volume.max(d.volume);
                        if d.oi.is_some() {
                            last.oi = d.oi;
                        }
                    }
                    _ => daily.push(d),
                }
            }
            if daily.len() > limit {
                daily.drain(..daily.len() - limit);
            }
            Ok(daily)
        }
    }
}

/// Instruments with stored data, for the chart pickers.
pub async fn list_instruments(pool: &SqlitePool) -> Result<InstrumentList> {
    let rows = sqlx::query("SELECT DISTINCT instrument FROM bars ORDER BY instrument")
        .fetch_all(pool)
        .await?;
    let mut indices = Vec::new();
    let mut opts: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for r in rows {
        let key: String = r.try_get("instrument")?;
        match instrument::parse(&key) {
            Some(Instrument::Index(n)) => indices.push(n),
            Some(Instrument::Option { symbol, expiry, strike, .. }) => {
                if let Ok(s) = strike.parse::<f64>() {
                    let v = opts.entry((symbol, expiry.format("%Y-%m-%d").to_string())).or_default();
                    if !v.contains(&s) {
                        v.push(s);
                    }
                }
            }
            None => {}
        }
    }
    let options = opts
        .into_iter()
        .map(|((symbol, expiry), mut strikes)| {
            strikes.sort_by(|a, b| a.partial_cmp(b).unwrap());
            OptionExpiry { symbol, expiry, strikes }
        })
        .collect();
    Ok(InstrumentList { indices, options })
}

// ----------------------------------------------------------- maintenance ---

/// Roll completed IST days of 1m bars up into 1d bars (idempotent; never
/// overwrites an existing daily bar, so Fyers history is kept).
pub async fn rollup_daily(pool: &SqlitePool, lookback_days: i64) -> Result<usize> {
    let now = Utc::now().timestamp();
    let today = resample::ist_day(now);
    let pairs = sqlx::query(
        r#"SELECT m.instrument AS instrument, m.d AS d
           FROM (SELECT instrument, (ts + 19800) / 86400 AS d FROM bars
                 WHERE tf = '1m' AND ts >= ? GROUP BY instrument, d) m
           LEFT JOIN bars b ON b.instrument = m.instrument AND b.tf = '1d'
                AND b.ts = m.d * 86400 - 19800
           WHERE m.d < ? AND b.ts IS NULL"#,
    )
    .bind(now - lookback_days * 86_400)
    .bind(today)
    .fetch_all(pool)
    .await?;

    let mut made = 0usize;
    for p in pairs {
        let inst: String = p.try_get("instrument")?;
        let day: i64 = p.try_get("d")?;
        let day_start = day * 86_400 - 19_800;
        let rows = sqlx::query(
            "SELECT ts, open, high, low, close, volume, oi FROM bars \
             WHERE instrument = ? AND tf = '1m' AND ts >= ? AND ts < ? ORDER BY ts ASC",
        )
        .bind(&inst)
        .bind(day_start)
        .bind(day_start + 86_400)
        .fetch_all(pool)
        .await?;
        let minutes = rows.iter().map(row_to_bar).collect::<Result<Vec<_>>>()?;
        let daily = resample::resample(&minutes, Interval::Day);
        let inserted = insert_backfill(pool, &inst, "1d", &daily).await?;
        // insert_backfill tags rows as 'fyers'; mark these as live rollups.
        if inserted > 0 {
            sqlx::query("UPDATE bars SET src = 'live' WHERE instrument = ? AND tf = '1d' AND ts = ?")
                .bind(&inst)
                .bind(day_start)
                .execute(pool)
                .await?;
        }
        made += inserted;
    }
    Ok(made)
}

pub async fn purge(pool: &SqlitePool, cfg: &ChartsConfig) -> Result<()> {
    let now = Utc::now();
    let cutoff_1m = now.timestamp() - cfg.bars_1m_keep_days * 86_400;
    let r = sqlx::query("DELETE FROM bars WHERE tf = '1m' AND ts < ?")
        .bind(cutoff_1m)
        .execute(pool)
        .await?;
    let expired = (now - chrono::Duration::days(cfg.option_expiry_grace_days))
        .format("%Y-%m-%d")
        .to_string();
    let r2 = sqlx::query("DELETE FROM bars WHERE expiry_date IS NOT NULL AND expiry_date < ?")
        .bind(&expired)
        .execute(pool)
        .await?;
    info!("Bars purge: {} old 1m, {} expired option rows", r.rows_affected(), r2.rows_affected());
    Ok(())
}

/// Hourly: roll up finished days, then purge old data.
pub fn spawn_maintenance(pool: SqlitePool, cfg: ChartsConfig) {
    tauri::async_runtime::spawn(async move {
        let mut lookback = cfg.bars_1m_keep_days.max(7);
        loop {
            match rollup_daily(&pool, lookback).await {
                Ok(n) if n > 0 => info!("Rolled up {n} daily bar(s)"),
                Ok(_) => {}
                Err(e) => error!("Daily rollup failed: {e}"),
            }
            if let Err(e) = purge(&pool, &cfg).await {
                error!("Bars purge failed: {e}");
            }
            lookback = 7; // after the first full pass only recent days need checking
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}
