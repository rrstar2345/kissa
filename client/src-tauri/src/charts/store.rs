//! DuckDB persistence for bars (see `duck`): upserts, chart queries, daily
//! rollup and retention.
//!
//! One table, `bars(instrument, tf, ts)`, with `tf` = `1m` | `1d`. Live
//! ticks are folded into 1m bars in memory (see `recorder`) so no raw tick
//! table is kept. Other intervals are resampled on read.

use anyhow::Result;
use chrono::Utc;
use duckdb::{params, Connection};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::{error, info};

use super::duck::BarStore;
use super::instrument::{self, Instrument};
use super::resample::{self, Interval};
use super::{Bar, InstrumentList, OptionExpiry};
use crate::settings::ChartsConfig;

const BAR_COLS: &str = "ts, open, high, low, close, volume, oi";

fn bar_from_row(r: &duckdb::Row<'_>) -> duckdb::Result<Bar> {
    Ok(Bar {
        ts: r.get(0)?,
        open: r.get(1)?,
        high: r.get(2)?,
        low: r.get(3)?,
        close: r.get(4)?,
        volume: r.get(5)?,
        oi: r.get::<_, Option<f64>>(6)?,
    })
}

/// Merge live 1m bars into the table. A restart mid-minute keeps the
/// earlier open/high/low instead of overwriting them (the batch holds at most
/// one bar per instrument and minute).
pub async fn upsert_live(store: &BarStore, batch: &[(String, Bar)]) -> Result<()> {
    let rows: Vec<(String, Bar, Option<String>)> =
        batch.iter().map(|(i, b)| (i.clone(), b.clone(), instrument::expiry_of(i))).collect();
    store
        .run(move |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM bars_stage", [])?;
            {
                let mut st = tx.prepare("INSERT INTO bars_stage VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")?;
                for (inst, b, expiry) in &rows {
                    st.execute(params![inst, b.ts, b.open, b.high, b.low, b.close, b.volume, b.oi, expiry])?;
                }
            }
            // Merge with what is stored (staged rows are new or newer), then
            // replace by primary key. Reading and writing `bars` in one
            // statement is avoided on purpose.
            tx.execute_batch(
                r#"CREATE OR REPLACE TEMP TABLE bars_merge AS
                   SELECT s.instrument AS instrument, '1m' AS tf, s.ts AS ts,
                          COALESCE(b.open, s.open) AS open,
                          GREATEST(COALESCE(b.high, s.high), s.high) AS high,
                          LEAST(COALESCE(b.low, s.low), s.low) AS low,
                          s.close AS close,
                          GREATEST(COALESCE(b.volume, s.volume), s.volume) AS volume,
                          COALESCE(s.oi, b.oi) AS oi,
                          COALESCE(b.src, 'live') AS src,
                          COALESCE(b.expiry_date, s.expiry_date) AS expiry_date
                   FROM bars_stage s
                   LEFT JOIN bars b ON b.instrument = s.instrument AND b.tf = '1m' AND b.ts = s.ts;
                   INSERT OR REPLACE INTO bars
                   SELECT instrument, tf, ts, open, high, low, close, volume, oi, src, expiry_date FROM bars_merge;
                   DROP TABLE bars_merge;"#,
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// Insert bars without touching rows that already exist; returns how many were new.
fn insert_bars(conn: &Connection, inst: &str, tf: &str, bars: &[Bar], src: &str) -> Result<usize> {
    let expiry = instrument::expiry_of(inst);
    let count = |c: &Connection| -> Result<i64> {
        Ok(c.query_row(
            "SELECT count(*) FROM bars WHERE instrument = ? AND tf = ?",
            params![inst, tf],
            |r| r.get::<_, i64>(0),
        )?)
    };
    let before = count(conn)?;
    let mut st = conn.prepare(
        "INSERT OR IGNORE INTO bars (instrument, tf, ts, open, high, low, close, volume, oi, src, expiry_date) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    for b in bars {
        st.execute(params![inst, tf, b.ts, b.open, b.high, b.low, b.close, b.volume, b.oi, src, expiry])?;
    }
    Ok((count(conn)? - before).max(0) as usize)
}

/// Insert historical bars without touching rows that already exist.
pub async fn insert_backfill(store: &BarStore, inst: &str, tf: &str, bars: &[Bar]) -> Result<usize> {
    let (inst, tf, bars) = (inst.to_string(), tf.to_string(), bars.to_vec());
    store
        .run(move |c| {
            let tx = c.transaction()?;
            let n = insert_bars(&tx, &inst, &tf, &bars, "fyers")?;
            tx.commit()?;
            Ok(n)
        })
        .await
}

/// Latest `limit` bars of `tf`, oldest first.
async fn latest(store: &BarStore, inst: &str, tf: &str, limit: i64) -> Result<Vec<Bar>> {
    let (inst, tf) = (inst.to_string(), tf.to_string());
    store
        .run(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {BAR_COLS} FROM bars WHERE instrument = ? AND tf = ? ORDER BY ts DESC LIMIT ?"
            ))?;
            let mut bars = st
                .query_map(params![inst, tf, limit], bar_from_row)?
                .collect::<duckdb::Result<Vec<_>>>()?;
            bars.reverse();
            Ok(bars)
        })
        .await
}

async fn since(store: &BarStore, inst: &str, tf: &str, from_ts: i64) -> Result<Vec<Bar>> {
    let (inst, tf) = (inst.to_string(), tf.to_string());
    store
        .run(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {BAR_COLS} FROM bars WHERE instrument = ? AND tf = ? AND ts >= ? ORDER BY ts ASC"
            ))?;
            Ok(st.query_map(params![inst, tf, from_ts], bar_from_row)?.collect::<duckdb::Result<Vec<_>>>()?)
        })
        .await
}

/// Bars for charting at any supported interval, oldest first.
pub async fn chart_bars(
    store: &BarStore,
    inst: &str,
    interval: Interval,
    limit: usize,
) -> Result<Vec<Bar>> {
    let limit = limit.clamp(1, 20_000);
    match interval {
        Interval::Min(1) => latest(store, inst, "1m", limit as i64).await,
        Interval::Min(n) => {
            let raw = latest(store, inst, "1m", (limit as i64 * n).min(150_000)).await?;
            let mut out = resample::resample(&raw, interval);
            if out.len() > limit {
                out.drain(..out.len() - limit);
            }
            Ok(out)
        }
        Interval::Day => {
            let mut daily = latest(store, inst, "1d", limit as i64).await?;
            // Days not rolled up yet (today, or recent days) come from 1m.
            let from = daily.last().map(|b| b.ts).unwrap_or(0);
            let minutes = since(store, inst, "1m", from).await?;
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

/// Close of the last bar of `tf` with `start <= ts < end`, when it is a usable price.
pub async fn close_in_range(store: &BarStore, inst: &str, tf: &str, start: i64, end: i64) -> Option<f64> {
    let (inst, tf) = (inst.to_string(), tf.to_string());
    let row = store
        .run(move |c| {
            let mut st = c.prepare(
                "SELECT close FROM bars WHERE instrument = ? AND tf = ? AND ts >= ? AND ts < ? ORDER BY ts DESC LIMIT 1",
            )?;
            let mut rows = st.query_map(params![inst, tf, start, end], |r| r.get::<_, f64>(0))?;
            Ok(rows.next().transpose()?)
        })
        .await
        .ok()
        .flatten();
    row.filter(|c| c.is_finite() && *c > 0.0)
}

/// Settlement price of an expired option: the close of the contract's own
/// last 1-minute bar on its expiry day (IST). A stored daily bar of that day
/// is used only when no 1-minute bar exists. `None` when nothing was recorded.
pub async fn expiry_close(store: &BarStore, instrument: &str, expiry_iso: &str) -> Option<f64> {
    const IST: i64 = 5 * 3600 + 30 * 60;
    let day = chrono::NaiveDate::parse_from_str(expiry_iso, "%Y-%m-%d").ok()?;
    let start = day.and_hms_opt(0, 0, 0)?.and_utc().timestamp() - IST;
    let end = start + 86_400;
    for tf in ["1m", "1d"] {
        if let Some(c) = close_in_range(store, instrument, tf, start, end).await {
            return Some(c);
        }
    }
    None
}

/// Most recent stored bar close for an instrument (used when no live price).
pub async fn last_close(store: &BarStore, instrument: &str) -> Option<f64> {
    let inst = instrument.to_string();
    store
        .run(move |c| {
            let mut st = c.prepare("SELECT close FROM bars WHERE instrument = ? ORDER BY ts DESC LIMIT 1")?;
            let mut rows = st.query_map(params![inst], |r| r.get::<_, f64>(0))?;
            Ok(rows.next().transpose()?)
        })
        .await
        .ok()
        .flatten()
}

/// Instruments with stored data, for the chart pickers.
pub async fn list_instruments(store: &BarStore) -> Result<InstrumentList> {
    let keys: Vec<String> = store
        .run(|c| {
            let mut st = c.prepare("SELECT DISTINCT instrument FROM bars ORDER BY instrument")?;
            Ok(st.query_map([], |r| r.get::<_, String>(0))?.collect::<duckdb::Result<Vec<_>>>()?)
        })
        .await?;
    let mut indices = Vec::new();
    let mut opts: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for key in keys {
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
pub async fn rollup_daily(store: &BarStore, lookback_days: i64) -> Result<usize> {
    let now = Utc::now().timestamp();
    let today = resample::ist_day(now);
    let since_ts = now - lookback_days * 86_400;
    store
        .run(move |c| {
            let tx = c.transaction()?;
            // `//` is integer division in DuckDB (`/` would give a float).
            let pairs: Vec<(String, i64)> = {
                let mut st = tx.prepare(
                    r#"SELECT m.instrument, m.d
                       FROM (SELECT instrument, (ts + 19800) // 86400 AS d FROM bars
                             WHERE tf = '1m' AND ts >= ? GROUP BY instrument, d) m
                       LEFT JOIN bars b ON b.instrument = m.instrument AND b.tf = '1d'
                            AND b.ts = m.d * 86400 - 19800
                       WHERE m.d < ? AND b.ts IS NULL"#,
                )?;
                let rows = st.query_map(params![since_ts, today], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                rows.collect::<duckdb::Result<Vec<_>>>()?
            };
            let mut made = 0usize;
            for (inst, day) in pairs {
                let day_start = day * 86_400 - 19_800;
                let minutes: Vec<Bar> = {
                    let mut st = tx.prepare(&format!(
                        "SELECT {BAR_COLS} FROM bars WHERE instrument = ? AND tf = '1m' AND ts >= ? AND ts < ? ORDER BY ts ASC"
                    ))?;
                    let rows = st.query_map(params![inst, day_start, day_start + 86_400], bar_from_row)?;
                    rows.collect::<duckdb::Result<Vec<_>>>()?
                };
                let daily = resample::resample(&minutes, Interval::Day);
                made += insert_bars(&tx, &inst, "1d", &daily, "live")?;
            }
            tx.commit()?;
            Ok(made)
        })
        .await
}

pub async fn purge(store: &BarStore, cfg: &ChartsConfig) -> Result<()> {
    let now = Utc::now();
    let cutoff_1m = now.timestamp() - cfg.bars_1m_keep_days * 86_400;
    let expired = (now - chrono::Duration::days(cfg.option_expiry_grace_days))
        .format("%Y-%m-%d")
        .to_string();
    let (old, exp) = store
        .run(move |c| {
            let old = c.execute("DELETE FROM bars WHERE tf = '1m' AND ts < ?", params![cutoff_1m])?;
            let exp = c.execute(
                "DELETE FROM bars WHERE expiry_date IS NOT NULL AND expiry_date < ?",
                params![expired],
            )?;
            Ok((old, exp))
        })
        .await?;
    info!("Bars purge: {old} old 1m, {exp} expired option rows");
    Ok(())
}

/// Hourly: roll up finished days, then purge old data.
pub fn spawn_maintenance(store: BarStore, cfg: ChartsConfig) {
    tauri::async_runtime::spawn(async move {
        let mut lookback = cfg.bars_1m_keep_days.max(7);
        loop {
            match rollup_daily(&store, lookback).await {
                Ok(n) if n > 0 => info!("Rolled up {n} daily bar(s)"),
                Ok(_) => {}
                Err(e) => error!("Daily rollup failed: {e}"),
            }
            if let Err(e) = purge(&store, &cfg).await {
                error!("Bars purge failed: {e}");
            }
            lookback = 7; // after the first full pass only recent days need checking
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(ts: i64, o: f64, h: f64, l: f64, c: f64, v: f64, oi: Option<f64>) -> Bar {
        Bar { ts, open: o, high: h, low: l, close: c, volume: v, oi }
    }

    #[tokio::test]
    async fn live_upsert_keeps_open_and_widens_range() {
        let s = BarStore::open_memory().unwrap();
        let k = "NIFTY".to_string();
        upsert_live(&s, &[(k.clone(), bar(60, 10.0, 12.0, 9.0, 11.0, 5.0, None))]).await.unwrap();
        // Restart mid-minute: a later version of the same bar with a narrower range.
        upsert_live(&s, &[(k.clone(), bar(60, 11.0, 11.5, 10.5, 10.8, 3.0, Some(7.0)))]).await.unwrap();
        let got = latest(&s, "NIFTY", "1m", 10).await.unwrap();
        assert_eq!(got, vec![bar(60, 10.0, 12.0, 9.0, 10.8, 5.0, Some(7.0))]);
        assert_eq!(s.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn live_upsert_handles_many_bars_and_oi_is_kept_when_missing() {
        let s = BarStore::open_memory().unwrap();
        let k = "NIFTY".to_string();
        upsert_live(&s, &[(k.clone(), bar(60, 1.0, 1.0, 1.0, 1.0, 0.0, Some(9.0)))]).await.unwrap();
        upsert_live(
            &s,
            &[
                (k.clone(), bar(60, 1.0, 2.0, 1.0, 2.0, 0.0, None)),
                (k.clone(), bar(120, 2.0, 2.0, 2.0, 2.0, 0.0, None)),
            ],
        )
        .await
        .unwrap();
        let got = latest(&s, "NIFTY", "1m", 10).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].oi, Some(9.0));
        assert_eq!(got[1].ts, 120);
    }

    #[tokio::test]
    async fn backfill_counts_only_new_rows() {
        let s = BarStore::open_memory().unwrap();
        let bars = vec![bar(60, 1.0, 1.0, 1.0, 1.0, 0.0, None), bar(120, 1.0, 1.0, 1.0, 1.0, 0.0, None)];
        assert_eq!(insert_backfill(&s, "NIFTY", "1m", &bars).await.unwrap(), 2);
        let more = vec![bar(120, 9.0, 9.0, 9.0, 9.0, 0.0, None), bar(180, 1.0, 1.0, 1.0, 1.0, 0.0, None)];
        assert_eq!(insert_backfill(&s, "NIFTY", "1m", &more).await.unwrap(), 1);
        // The existing bar was not overwritten.
        assert_eq!(latest(&s, "NIFTY", "1m", 10).await.unwrap()[1].close, 1.0);
    }

    #[tokio::test]
    async fn rollup_makes_a_daily_bar_once() {
        let s = BarStore::open_memory().unwrap();
        // 2026-11-02 (Monday) 09:15 IST and 09:16 IST.
        let open = 1_793_591_100;
        let mins = vec![bar(open, 10.0, 12.0, 9.0, 11.0, 5.0, None), bar(open + 60, 11.0, 15.0, 10.0, 14.0, 9.0, None)];
        upsert_live(&s, &mins.iter().map(|b| ("NIFTY".to_string(), b.clone())).collect::<Vec<_>>())
            .await
            .unwrap();
        assert_eq!(rollup_daily(&s, 100_000).await.unwrap(), 1);
        assert_eq!(rollup_daily(&s, 100_000).await.unwrap(), 0);
        let daily = latest(&s, "NIFTY", "1d", 10).await.unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!((daily[0].open, daily[0].high, daily[0].low, daily[0].close), (10.0, 15.0, 9.0, 14.0));
    }

    #[tokio::test]
    async fn expiry_close_is_the_last_bar_of_the_expiry_day() {
        let s = BarStore::open_memory().unwrap();
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        let start = day.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() - 5 * 3600 - 30 * 60;
        let a = "OPT:NIFTY:2026-11-24:24000:CE";
        let put = |inst: &str, tf: &'static str, ts: i64, close: f64| {
            let inst = inst.to_string();
            let s = s.clone();
            async move {
                s.run(move |c| {
                    c.execute(
                        "INSERT INTO bars (instrument, tf, ts, open, high, low, close) VALUES (?, ?, ?, 1, 1, 1, ?)",
                        params![inst, tf, ts, close],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
            }
        };
        put(a, "1m", start - 600, 99.0).await; // the day before: ignored
        put(a, "1m", start + 9 * 3600 + 15 * 60, 40.0).await;
        put(a, "1m", start + 15 * 3600 + 29 * 60, 12.5).await; // last bar of the day
        put(a, "1m", start + 86_400 + 3600, 77.0).await; // the day after: ignored
        assert_eq!(expiry_close(&s, a, "2026-11-24").await, Some(12.5));

        let b = "OPT:NIFTY:2026-11-24:24500:PE";
        put(b, "1d", start, 3.0).await; // only a daily bar: used as the fallback
        assert_eq!(expiry_close(&s, b, "2026-11-24").await, Some(3.0));

        assert_eq!(expiry_close(&s, "OPT:NIFTY:2026-11-24:25000:PE", "2026-11-24").await, None);
        assert_eq!(expiry_close(&s, a, "not-a-date").await, None);
        assert_eq!(last_close(&s, a).await, Some(77.0));
    }
}
