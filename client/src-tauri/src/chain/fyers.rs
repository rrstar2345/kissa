//! Fyers v3 option-chain client (user supplies their own API credentials).
//! Reference: https://fyers.in/api-docs/v3/api/market-data/option-chain/

use anyhow::{anyhow, Result};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use std::time::Duration;

use super::engine::years_to_expiry;
use super::greeks;
use super::{pct, ChainSnapshot, ExpiryItem, Leg, StrikeRow};
use crate::settings::FyersConfig;

pub struct FyersCreds {
    pub app_id: String,
    pub access_token: String,
}

#[derive(Debug, Deserialize, Default)]
struct Resp {
    s: Option<String>,
    message: Option<String>,
    data: Option<Data>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct Data {
    #[serde(rename = "callOi")]
    call_oi: Option<f64>,
    #[serde(rename = "putOi")]
    put_oi: Option<f64>,
    #[serde(rename = "expiryData")]
    expiry_data: Vec<ExpiryEntry>,
    #[serde(rename = "indiavixData")]
    vix: Option<Item>,
    #[serde(rename = "optionsChain")]
    options_chain: Vec<Item>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ExpiryEntry {
    date: String,
    expiry: serde_json::Value,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Item {
    ltp: Option<f64>,
    ltpch: Option<f64>,
    ltpchp: Option<f64>,
    bid: Option<f64>,
    ask: Option<f64>,
    oi: Option<f64>,
    oich: Option<f64>,
    volume: Option<f64>,
    option_type: String,
    strike_price: Option<f64>,
    symbol: String,
    greeks: Option<G>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct G {
    delta: Option<f64>,
    gamma: Option<f64>,
    theta: Option<f64>,
    vega: Option<f64>,
    iv: Option<f64>,
}

/// Map an NSE F&O underlying to Fyers' symbol format.
pub fn fyers_symbol(symbol: &str) -> String {
    match symbol {
        "NIFTY" => "NSE:NIFTY50-INDEX".into(),
        "BANKNIFTY" => "NSE:NIFTYBANK-INDEX".into(),
        "FINNIFTY" => "NSE:FINNIFTY-INDEX".into(),
        "MIDCPNIFTY" => "NSE:MIDCPNIFTY-INDEX".into(),
        "NIFTYNXT50" => "NSE:NIFTYNXT50-INDEX".into(),
        other => format!("NSE:{}-EQ", other),
    }
}

pub async fn fetch(
    cfg: &FyersConfig,
    creds: &FyersCreds,
    symbol: &str,
    strike_count: u32,
    expiry_epoch: Option<&str>,
) -> Result<Data> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
    let mut query = vec![
        ("symbol", fyers_symbol(symbol)),
        ("strikecount", strike_count.clamp(1, 50).to_string()),
        ("greeks", "1".to_string()),
    ];
    if let Some(ts) = expiry_epoch {
        query.push(("timestamp", ts.to_string()));
    }
    let resp = client
        .get(&cfg.chain_url)
        .header("Authorization", format!("{}:{}", creds.app_id, creds.access_token))
        .query(&query)
        .send()
        .await
        .map_err(|e| anyhow!("Fyers request failed: {}", e))?;
    let status = resp.status();
    let body = resp.text().await?;
    let parsed: Resp = serde_json::from_str(&body).map_err(|_| {
        anyhow!("Fyers returned an unexpected response (HTTP {}): {}", status, &body[..body.len().min(200)])
    })?;
    if parsed.s.as_deref() != Some("ok") {
        return Err(anyhow!(
            "Fyers error: {}",
            parsed.message.filter(|m| !m.is_empty()).unwrap_or_else(|| format!("HTTP {}", status))
        ));
    }
    parsed.data.ok_or_else(|| anyhow!("Fyers response had no data"))
}

pub fn expiries(data: &Data) -> Vec<ExpiryItem> {
    data.expiry_data
        .iter()
        .filter_map(|e| {
            let d = NaiveDate::parse_from_str(&e.date, "%d-%m-%Y").ok()?;
            let epoch = match &e.expiry {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => return None,
            };
            Some(ExpiryItem {
                value: epoch,
                label: d.format("%d-%b-%Y").to_string(),
                date: d.format("%Y-%m-%d").to_string(),
            })
        })
        .collect()
}

pub fn to_snapshot(data: &Data, symbol: &str, expiry: &ExpiryItem, rate: f64) -> ChainSnapshot {
    let underlying = data.options_chain.iter().find(|i| i.option_type.is_empty());
    let spot = underlying.and_then(|u| u.ltp);
    let t = NaiveDate::parse_from_str(&expiry.date, "%Y-%m-%d")
        .map(years_to_expiry)
        .unwrap_or(0.0);
    let forward = spot.map(|s| s * (rate * t).exp());

    let mut rows: std::collections::BTreeMap<i64, StrikeRow> = Default::default();
    let mut any_broker_greeks = false;
    for it in data.options_chain.iter().filter(|i| i.option_type == "CE" || i.option_type == "PE") {
        let Some(strike) = it.strike_price else { continue };
        let is_call = it.option_type == "CE";
        let row = rows
            .entry((strike * 100.0).round() as i64)
            .or_insert_with(|| StrikeRow { strike, ..Default::default() });
        let leg: &mut Leg = if is_call { &mut row.ce } else { &mut row.pe };
        leg.ltp = it.ltp;
        leg.change = it.ltpch;
        leg.change_pct = it.ltpchp.or_else(|| pct(it.ltpch, it.ltp));
        leg.oi = it.oi;
        leg.oi_change = it.oich;
        leg.volume = it.volume;
        leg.bid = it.bid;
        leg.ask = it.ask;
        if let Some(g) = &it.greeks {
            any_broker_greeks = true;
            leg.iv = g.iv;
            leg.delta = g.delta;
            leg.gamma = g.gamma;
            leg.theta = g.theta;
            leg.vega = g.vega;
        } else if let (Some(f), Some(px)) = (forward, it.ltp.filter(|p| *p > 0.0)) {
            if let Some(g) = greeks::greeks(is_call, px, f, strike, t, rate) {
                leg.iv = Some(g.iv);
                leg.delta = Some(g.delta);
                leg.gamma = Some(g.gamma);
                leg.theta = Some(g.theta);
                leg.vega = Some(g.vega);
            }
        }
        let _ = &it.symbol;
    }

    let rows: Vec<StrikeRow> = rows.into_values().collect();
    let atm = spot.and_then(|s| {
        rows.iter()
            .map(|r| r.strike)
            .min_by(|a, b| (a - s).abs().partial_cmp(&(b - s).abs()).unwrap())
    });
    let call_oi = data.call_oi.unwrap_or_else(|| rows.iter().filter_map(|r| r.ce.oi).sum());
    let put_oi = data.put_oi.unwrap_or_else(|| rows.iter().filter_map(|r| r.pe.oi).sum());

    ChainSnapshot {
        source: "fyers".into(),
        symbol: symbol.into(),
        expiry: expiry.label.clone(),
        underlying_name: underlying.map(|u| u.symbol.clone()),
        underlying: spot,
        underlying_change: underlying.and_then(|u| u.ltpch),
        underlying_change_pct: underlying.and_then(|u| u.ltpchp),
        vix: data.vix.as_ref().and_then(|v| v.ltp),
        atm,
        call_oi,
        put_oi,
        pcr: (call_oi > 0.0).then(|| put_oi / call_oi),
        greeks_source: if any_broker_greeks { "broker" } else { "computed" }.into(),
        updated_at: Utc::now().to_rfc3339(),
        rows,
    }
}
