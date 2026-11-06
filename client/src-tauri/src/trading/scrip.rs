//! Dhan instrument list ("scrip master"): maps an option contract
//! (underlying, expiry, strike, CE/PE) to the numeric `securityId` Dhan
//! orders need, plus its lot size. Only NSE index options are kept.
//!
//! The detailed CSV is read by *column name* so the column order doesn't
//! matter; the compact CSV's `SEM_*` names are accepted too.

use anyhow::{anyhow, Result};
use chrono::{Duration as ChronoDuration, NaiveDate, Utc};
use sqlx::SqlitePool;
use std::time::Duration;

use crate::db;
use crate::settings::TradingConfig;

#[derive(Debug, Clone, PartialEq)]
pub struct ScripRow {
    pub security_id: String,
    pub symbol: String,
    /// ISO `YYYY-MM-DD`.
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    pub lot_size: i64,
    pub trading_symbol: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Contract {
    pub security_id: String,
    pub lot_size: i64,
    pub trading_symbol: String,
}

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS scrip (
            security_id TEXT PRIMARY KEY,
            symbol TEXT NOT NULL,
            expiry TEXT NOT NULL,
            strike REAL NOT NULL,
            opt TEXT NOT NULL,
            lot_size INTEGER NOT NULL,
            trading_symbol TEXT NOT NULL DEFAULT ''
        );"#,
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_scrip_lookup ON scrip(symbol, expiry, opt, strike);")
        .execute(pool)
        .await?;
    Ok(())
}

fn clean(h: &str) -> &str {
    h.trim().trim_start_matches('\u{feff}')
}

fn find(headers: &csv::StringRecord, names: &[&str]) -> Option<usize> {
    names.iter().find_map(|n| headers.iter().position(|h| clean(h).eq_ignore_ascii_case(n)))
}

fn find_all(headers: &csv::StringRecord, names: &[&str]) -> Vec<usize> {
    names.iter().filter_map(|n| headers.iter().position(|h| clean(h).eq_ignore_ascii_case(n))).collect()
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let head: String = s.trim().chars().take(10).collect();
    ["%Y-%m-%d", "%d-%m-%Y", "%d/%m/%Y", "%Y/%m/%d"]
        .iter()
        .find_map(|f| NaiveDate::parse_from_str(&head, f).ok())
}

/// Keep NSE index options (`OPTIDX`) expiring on or after `min_expiry`.
pub fn parse_csv(text: &str, min_expiry: NaiveDate) -> Result<Vec<ScripRow>> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(text.as_bytes());
    let headers = rdr.headers()?.clone();

    let c_exch = find(&headers, &["EXCH_ID", "SEM_EXM_EXCH_ID"]);
    let c_seg = find(&headers, &["SEGMENT", "SEM_SEGMENT"]);
    let c_id = find(&headers, &["SECURITY_ID", "SEM_SMST_SECURITY_ID"])
        .ok_or_else(|| anyhow!("instrument list has no security id column"))?;
    let c_inst = find(&headers, &["INSTRUMENT", "SEM_INSTRUMENT_NAME"])
        .ok_or_else(|| anyhow!("instrument list has no instrument column"))?;
    let c_lot = find(&headers, &["LOT_SIZE", "SEM_LOT_UNITS"])
        .ok_or_else(|| anyhow!("instrument list has no lot size column"))?;
    let c_exp = find(&headers, &["SM_EXPIRY_DATE", "SEM_EXPIRY_DATE"])
        .ok_or_else(|| anyhow!("instrument list has no expiry column"))?;
    let c_strike = find(&headers, &["STRIKE_PRICE", "SEM_STRIKE_PRICE"])
        .ok_or_else(|| anyhow!("instrument list has no strike column"))?;
    let c_opt = find(&headers, &["OPTION_TYPE", "SEM_OPTION_TYPE"])
        .ok_or_else(|| anyhow!("instrument list has no option type column"))?;
    let c_under = find_all(&headers, &["UNDERLYING_SYMBOL", "SM_SYMBOL_NAME", "SYMBOL_NAME"]);
    let c_tsym = find_all(&headers, &["SEM_TRADING_SYMBOL", "DISPLAY_NAME", "SEM_CUSTOM_SYMBOL"]);

    let mut out = Vec::new();
    for rec in rdr.records() {
        let Ok(rec) = rec else { continue };
        let get = |i: usize| rec.get(i).unwrap_or("").trim();

        if let Some(i) = c_exch {
            if !get(i).eq_ignore_ascii_case("NSE") {
                continue;
            }
        }
        if let Some(i) = c_seg {
            if !get(i).eq_ignore_ascii_case("D") {
                continue;
            }
        }
        if !get(c_inst).eq_ignore_ascii_case("OPTIDX") {
            continue;
        }
        let is_call = match get(c_opt).to_uppercase().as_str() {
            "CE" => true,
            "PE" => false,
            _ => continue,
        };
        let Some(expiry) = parse_date(get(c_exp)) else { continue };
        if expiry < min_expiry {
            continue;
        }
        let Ok(strike) = get(c_strike).parse::<f64>() else { continue };
        let Ok(lot) = get(c_lot).parse::<f64>() else { continue };
        let lot_size = lot.round() as i64;
        let id = get(c_id);
        if strike <= 0.0 || lot_size <= 0 || id.is_empty() {
            continue;
        }
        let trading_symbol = c_tsym.iter().map(|&i| get(i)).find(|s| !s.is_empty()).unwrap_or("").to_string();
        let symbol = c_under
            .iter()
            .map(|&i| get(i))
            .find(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| trading_symbol.split(|c: char| c == '-' || c == ' ').next().map(str::to_string))
            .unwrap_or_default()
            .to_uppercase();
        if symbol.is_empty() {
            continue;
        }
        out.push(ScripRow {
            security_id: id.to_string(),
            symbol,
            expiry: expiry.format("%Y-%m-%d").to_string(),
            strike,
            is_call,
            lot_size,
            trading_symbol,
        });
    }
    Ok(out)
}

/// Download the instrument list and replace the local copy.
pub async fn refresh(pool: &SqlitePool, cfg: &TradingConfig) -> Result<usize> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(180)).build()?;
    let resp = client
        .get(&cfg.scrip_master_url)
        .send()
        .await
        .map_err(|e| anyhow!("Could not download the Dhan instrument list: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("Dhan instrument list returned HTTP {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| anyhow!("Could not read the Dhan instrument list: {e}"))?;
    let min_expiry = Utc::now().date_naive() - ChronoDuration::days(2);
    let rows = tokio::task::spawn_blocking(move || parse_csv(&text, min_expiry))
        .await
        .map_err(|e| anyhow!("{e}"))??;
    if rows.is_empty() {
        return Err(anyhow!("The Dhan instrument list contained no NSE index options"));
    }

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM scrip").execute(&mut *tx).await?;
    for r in &rows {
        sqlx::query(
            "INSERT OR REPLACE INTO scrip (security_id, symbol, expiry, strike, opt, lot_size, trading_symbol) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.security_id)
        .bind(&r.symbol)
        .bind(&r.expiry)
        .bind(r.strike)
        .bind(if r.is_call { "CE" } else { "PE" })
        .bind(r.lot_size)
        .bind(&r.trading_symbol)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    db::set_setting(pool, "scrip.refreshed_at", &Utc::now().to_rfc3339()).await?;
    Ok(rows.len())
}

/// True when the list was never downloaded or is older than `hours`.
pub async fn is_stale(pool: &SqlitePool, hours: i64) -> bool {
    match db::get_setting(pool, "scrip.refreshed_at").await {
        Ok(Some(s)) => chrono::DateTime::parse_from_rfc3339(&s)
            .map(|t| Utc::now().signed_duration_since(t.with_timezone(&Utc)).num_hours() >= hours)
            .unwrap_or(true),
        _ => true,
    }
}

pub async fn lookup(
    pool: &SqlitePool,
    symbol: &str,
    expiry_iso: &str,
    strike: f64,
    is_call: bool,
) -> Result<Option<Contract>> {
    Ok(sqlx::query_as::<_, Contract>(
        "SELECT security_id, lot_size, trading_symbol FROM scrip
         WHERE symbol = ? AND expiry = ? AND opt = ? AND ABS(strike - ?) < 0.005 LIMIT 1",
    )
    .bind(symbol.to_uppercase())
    .bind(expiry_iso)
    .bind(if is_call { "CE" } else { "PE" })
    .bind(strike)
    .fetch_optional(pool)
    .await?)
}

/// Lot size of the nearest listed expiry for an underlying.
pub async fn lot_size_for_symbol(pool: &SqlitePool, symbol: &str) -> Result<Option<i64>> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT lot_size FROM scrip WHERE symbol = ? ORDER BY expiry ASC LIMIT 1")
            .bind(symbol.to_uppercase())
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "\u{feff}EXCH_ID,SEGMENT,SECURITY_ID,ISIN,INSTRUMENT,UNDERLYING_SECURITY_ID,UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,INSTRUMENT_TYPE,SERIES,LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE\n\
NSE,D,51234,NA,OPTIDX,13,NIFTY,NIFTY,NIFTY 24 NOV 24000 CALL,OP,NA,65.0,2026-11-24 14:30:00,24000.0,CE,5.0\n\
NSE,D,51235,NA,OPTIDX,13,NIFTY,NIFTY,NIFTY 24 NOV 24000 PUT,OP,NA,65.0,2026-11-24 14:30:00,24000.0,PE,5.0\n\
NSE,D,51236,NA,OPTIDX,13,NIFTY,NIFTY,NIFTY 24 NOV 24000 CALL,OP,NA,65.0,2020-01-01 14:30:00,24000.0,CE,5.0\n\
NSE,D,70001,NA,FUTIDX,13,NIFTY,NIFTY,NIFTY NOV FUT,FUT,NA,65.0,2026-11-24 14:30:00,-0.01,XX,5.0\n\
NSE,E,11536,INE467B01029,EQUITY,,TCS,TCS,TCS,ES,EQ,1.0,,0,,5.0\n";

    #[test]
    fn keeps_only_current_nse_index_options() {
        let min = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let rows = parse_csv(CSV, min).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].security_id, "51234");
        assert_eq!(rows[0].symbol, "NIFTY");
        assert_eq!(rows[0].expiry, "2026-11-24");
        assert_eq!(rows[0].lot_size, 65);
        assert_eq!(rows[0].strike, 24000.0);
        assert!(rows[0].is_call);
        assert!(!rows[1].is_call);
    }

    #[test]
    fn rejects_files_without_the_expected_columns() {
        let min = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        assert!(parse_csv("A,B\n1,2\n", min).is_err());
    }

    #[test]
    fn parses_several_date_formats() {
        assert_eq!(parse_date("2026-11-24 14:30:00"), NaiveDate::from_ymd_opt(2026, 11, 24));
        assert_eq!(parse_date("24-11-2026"), NaiveDate::from_ymd_opt(2026, 11, 24));
        assert_eq!(parse_date("garbage"), None);
    }
}
