//! F&O symbol / expiry resolution from NSE (ported from kstocks_egui).
//! Extended to keep the index names NSE uses so a symbol like `NIFTY` can be
//! matched to the `NIFTY 50` entry on the indices stream.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

use crate::market::http;
use crate::settings::AppConfig;

#[derive(Debug, Deserialize, Clone)]
struct IndexEntry {
    #[serde(rename = "fnoIndexName")]
    fno_index_name: Option<String>,
    #[serde(rename = "indicesLongName")]
    indices_long_name: Option<String>,
    #[serde(rename = "indicesShortName")]
    indices_short_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IndexTrackerResponse {
    data: HashMap<String, IndexEntry>,
}

#[derive(Debug, Deserialize)]
struct OptionInfoResponse {
    #[serde(rename = "expiryDates")]
    expiry_dates: Vec<String>,
}

/// An F&O underlying (e.g. `NIFTY`) plus the names it may carry on the
/// indices stream, in priority order.
#[derive(Debug, Clone, Serialize)]
pub struct FnoIndex {
    pub symbol: String,
    pub index_names: Vec<String>,
}

pub fn builtin_index_names(symbol: &str) -> &'static [&'static str] {
    match symbol {
        "NIFTY" => &["NIFTY 50"],
        "BANKNIFTY" => &["NIFTY BANK"],
        "FINNIFTY" => &["NIFTY FINANCIAL SERVICES", "NIFTY FIN SERVICE"],
        "MIDCPNIFTY" => &["NIFTY MIDCAP SELECT", "NIFTY MID SELECT"],
        "NIFTYNXT50" => &["NIFTY NEXT 50"],
        _ => &[],
    }
}

/// Fetch all F&O underlyings with their candidate stream names.
pub async fn fetch_fno_indices(config: &AppConfig) -> Result<Vec<FnoIndex>> {
    let url = &config.system.indices_info.base;
    let response = http::get(url)
        .send()
        .await
        .map_err(|e| anyhow!("Failed to fetch indices info: {}", e))?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "indices_info failed with status {}: {}",
            status,
            &body[..body.len().min(300)]
        ));
    }
    let parsed: IndexTrackerResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow!("Failed to parse indices info: {}", e))?;

    let mut by_symbol: HashMap<String, Vec<String>> = HashMap::new();
    for (key, entry) in parsed.data {
        let Some(sym) = entry.fno_index_name.clone() else { continue };
        let names = by_symbol.entry(sym.clone()).or_default();
        for n in builtin_index_names(&sym) {
            if !names.iter().any(|x| x.eq_ignore_ascii_case(n)) {
                names.push((*n).to_string());
            }
        }
        for n in [Some(key), entry.indices_long_name, entry.indices_short_name]
            .into_iter()
            .flatten()
        {
            if !names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                names.push(n);
            }
        }
    }

    let mut out: Vec<FnoIndex> = by_symbol
        .into_iter()
        .map(|(symbol, index_names)| FnoIndex { symbol, index_names })
        .collect();
    out.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    info!("Resolved {} F&O symbols", out.len());
    Ok(out)
}

/// All expiry dates (NSE format, e.g. `28-Apr-2026`) for an F&O symbol.
pub async fn fetch_expiries(config: &AppConfig, symbol: &str) -> Result<Vec<String>> {
    let url = format!(
        "{}?symbol={}",
        config.system.option_info.base,
        urlencoding::encode(symbol)
    );
    let response = http::get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("Failed to fetch option info for {}: {}", symbol, e))?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "option_info failed with status {} for {}: {}",
            status,
            symbol,
            &body[..body.len().min(300)]
        ));
    }
    let parsed: OptionInfoResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow!("Failed to parse option info for {}: {}", symbol, e))?;
    if parsed.expiry_dates.is_empty() {
        return Err(anyhow!("No expiry dates returned for {}", symbol));
    }
    Ok(parsed.expiry_dates)
}
