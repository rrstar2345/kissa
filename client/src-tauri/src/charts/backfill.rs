//! Historical backfill from Fyers (`/data/history`). Only fills gaps: rows
//! already stored (live or earlier backfill) are never overwritten.
//!
//! Limits (Fyers docs/community): intraday resolutions up to 100 days per
//! request, daily up to 366 days. Expired option contracts usually have no
//! history, so a "no data" result for them is normal.

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::Value;
use sqlx::SqlitePool;
use std::time::Duration;

use super::{instrument, store, Bar, BackfillResult};
use crate::chain::fyers::FyersCreds;
use crate::settings::FyersConfig;

struct Fetched {
    bars: Vec<Bar>,
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn parse_candles(body: &Value) -> Result<Fetched> {
    match body.get("s").and_then(Value::as_str) {
        Some("ok") => {}
        Some("no_data") => return Ok(Fetched { bars: vec![] }),
        _ => {
            let msg = body.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(anyhow!("Fyers history error: {msg}"));
        }
    }
    let candles = body.get("candles").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut bars = Vec::with_capacity(candles.len());
    for c in candles {
        let Some(a) = c.as_array() else { continue };
        if a.len() < 6 {
            continue;
        }
        let (Some(ts), Some(o), Some(h), Some(l), Some(cl)) =
            (a[0].as_i64().or_else(|| num(&a[0]).map(|x| x as i64)), num(&a[1]), num(&a[2]), num(&a[3]), num(&a[4]))
        else {
            continue;
        };
        bars.push(Bar {
            ts,
            open: o,
            high: h,
            low: l,
            close: cl,
            volume: num(&a[5]).unwrap_or(0.0),
            oi: a.get(6).and_then(num),
        });
    }
    Ok(Fetched { bars })
}

async fn fetch_window(
    client: &reqwest::Client,
    cfg: &FyersConfig,
    creds: &FyersCreds,
    symbol: &str,
    resolution: &str,
    from: i64,
    to: i64,
) -> Result<Fetched> {
    let (from_s, to_s) = (from.to_string(), to.to_string());
    let resp = client
        .get(&cfg.history_url)
        .header("Authorization", format!("{}:{}", creds.app_id, creds.access_token))
        .query(&[
            ("symbol", symbol),
            ("resolution", resolution),
            ("date_format", "0"),
            ("range_from", from_s.as_str()),
            ("range_to", to_s.as_str()),
            ("cont_flag", "1"),
            ("oi_flag", "1"),
        ])
        .send()
        .await
        .map_err(|e| anyhow!("Fyers history request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    let body: Value = serde_json::from_str(&text).map_err(|_| {
        anyhow!("Fyers returned an unexpected response (HTTP {status}): {}", &text[..text.len().min(200)])
    })?;
    parse_candles(&body)
}

/// Backfill `days` of `interval` (`1m` or `1d`) for an instrument.
pub async fn backfill(
    cfg: &FyersConfig,
    creds: &FyersCreds,
    pool: &SqlitePool,
    inst: &str,
    interval: &str,
    days: u32,
) -> Result<BackfillResult> {
    let (resolution, tf, window_days) = match interval {
        "1m" => ("1", "1m", 90i64),
        "1d" => ("D", "1d", 300i64),
        _ => return Err(anyhow!("Backfill supports 1m and 1d only (other intervals are resampled)")),
    };
    let candidates = instrument::fyers_symbols(inst);
    if candidates.is_empty() {
        return Err(anyhow!("Unrecognised instrument: {inst}"));
    }
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let now = Utc::now().timestamp();
    let start = now - days.max(1) as i64 * 86_400;

    let mut last_err: Option<anyhow::Error> = None;
    for symbol in candidates {
        let mut all: Vec<Bar> = Vec::new();
        let mut to = now;
        while to > start {
            let from = (to - window_days * 86_400).max(start);
            match fetch_window(&client, cfg, creds, &symbol, resolution, from, to).await {
                Ok(f) => all.extend(f.bars),
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
            to = from;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if all.is_empty() {
            continue; // try the next symbol format
        }
        if tf == "1d" {
            // Normalise daily candles to the IST day start used everywhere else.
            for b in all.iter_mut() {
                b.ts = super::resample::bucket_start(b.ts, super::resample::Interval::Day);
            }
        }
        all.sort_by_key(|b| b.ts);
        all.dedup_by_key(|b| b.ts);
        let inserted = store::insert_backfill(pool, inst, tf, &all).await?;
        return Ok(BackfillResult { symbol, fetched: all.len(), inserted });
    }
    match last_err {
        Some(e) => Err(e),
        None => Err(anyhow!("Fyers has no history for this instrument (expired or unknown contract?)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_candles_with_and_without_oi() {
        let body: Value = serde_json::json!({
            "s": "ok",
            "candles": [[1793850300, 100.0, 101.0, 99.5, 100.5, 1200], [1793850360, 100.5, 102.0, 100.0, 101.0, 900, 5500]]
        });
        let f = parse_candles(&body).unwrap();
        assert_eq!(f.bars.len(), 2);
        assert_eq!(f.bars[0].oi, None);
        assert_eq!(f.bars[1].oi, Some(5500.0));
        assert_eq!(f.bars[1].volume, 900.0);
    }

    #[test]
    fn no_data_is_empty_and_error_is_error() {
        assert!(parse_candles(&serde_json::json!({"s": "no_data", "candles": []})).unwrap().bars.is_empty());
        assert!(parse_candles(&serde_json::json!({"s": "error", "message": "bad token"})).is_err());
    }
}
