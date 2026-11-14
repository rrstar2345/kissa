//! Fyers v3 option-chain client (user supplies their own API credentials).
//! Reference: https://fyers.in/api-docs/v3/api/market-data/option-chain/

use anyhow::{anyhow, Result};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

use super::engine::years_to_expiry;
use super::greeks;
use super::{pct, ChainSnapshot, ExpiryItem, Leg, StrikeRow};
use crate::market::fyers_socket::topics;
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
    #[serde(rename = "fyToken")]
    fy_token: String,
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

/// Last-session figures of one index from the quotes endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub symbol: String,
    pub ltp: f64,
    pub prev_close: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
}

/// Reads a `/data/quotes` response. Symbols that errored or have no price are left out.
pub fn parse_quotes(body: &str) -> Result<Vec<Quote>> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| anyhow!("Fyers returned an unexpected response: {}", body.chars().take(200).collect::<String>()))?;
    if v.get("s").and_then(|s| s.as_str()) != Some("ok") {
        let m = v.get("message").and_then(|m| m.as_str()).filter(|m| !m.is_empty()).unwrap_or("error");
        return Err(anyhow!("Fyers error: {m}"));
    }
    let num = |o: &serde_json::Value, k: &str| o.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let mut out = Vec::new();
    for item in v.get("d").and_then(|d| d.as_array()).into_iter().flatten() {
        if item.get("s").and_then(|s| s.as_str()).is_some_and(|s| s != "ok") {
            continue;
        }
        let (Some(symbol), Some(q)) = (item.get("n").and_then(|n| n.as_str()), item.get("v")) else {
            continue;
        };
        let ltp = num(q, "lp");
        if ltp <= 0.0 {
            continue;
        }
        out.push(Quote {
            symbol: symbol.to_string(),
            ltp,
            prev_close: num(q, "prev_close_price"),
            open: num(q, "open_price"),
            high: num(q, "high_price"),
            low: num(q, "low_price"),
        });
    }
    Ok(out)
}

/// Quotes for Fyers symbols (at most 50 per call).
pub async fn fetch_quotes(cfg: &FyersConfig, creds: &FyersCreds, symbols: &[&str]) -> Result<Vec<Quote>> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
    let resp = client
        .get(&cfg.quotes_url)
        .header("Authorization", format!("{}:{}", creds.app_id, creds.access_token))
        .query(&[("symbols", symbols.join(","))])
        .send()
        .await
        .map_err(|e| anyhow!("Fyers request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await?;
    parse_quotes(&body).map_err(|e| anyhow!("{e} (HTTP {status})"))
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

// ------------------------------------------------- streaming (data socket) ---

/// One option contract of the chain, as the REST call reported it.
#[derive(Debug, Clone)]
pub struct Contract {
    pub symbol: String,
    pub fytoken: String,
    pub strike: f64,
    pub is_call: bool,
    pub ltp: Option<f64>,
    pub change: Option<f64>,
    pub oi: Option<f64>,
    pub oich: Option<f64>,
    pub volume: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
}

/// The underlying row of the chain call.
#[derive(Debug, Clone)]
pub struct Under {
    pub symbol: String,
    pub fytoken: String,
    pub ltp: Option<f64>,
    pub change: Option<f64>,
}

pub fn contracts(data: &Data) -> Vec<Contract> {
    data.options_chain
        .iter()
        .filter(|i| i.option_type == "CE" || i.option_type == "PE")
        .filter_map(|i| {
            Some(Contract {
                symbol: i.symbol.clone(),
                fytoken: i.fy_token.clone(),
                strike: i.strike_price?,
                is_call: i.option_type == "CE",
                ltp: i.ltp,
                change: i.ltpch,
                oi: i.oi,
                oich: i.oich,
                volume: i.volume,
                bid: i.bid,
                ask: i.ask,
            })
        })
        .collect()
}

pub fn underlying(data: &Data) -> Option<Under> {
    data.options_chain.iter().find(|i| i.option_type.is_empty()).map(|u| Under {
        symbol: u.symbol.clone(),
        fytoken: u.fy_token.clone(),
        ltp: u.ltp,
        change: u.ltpch,
    })
}

pub fn vix_ltp(data: &Data) -> Option<f64> {
    data.vix.as_ref().and_then(|v| v.ltp)
}

/// What to subscribe on the data socket for one chain.
pub struct StreamPlan {
    /// Option contracts by socket topic (`sf|nse_fo|<token>`).
    pub options: HashMap<String, Contract>,
    /// Underlying and its socket topic (original case: index names are case-sensitive).
    pub underlying: Option<(String, Under)>,
    pub vix_topic: Option<String>,
}

impl StreamPlan {
    /// Topics in subscription priority: indices, then strikes nearest the spot.
    pub fn topics(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let spot = self.underlying.as_ref().and_then(|(_, u)| u.ltp).unwrap_or(0.0);
        if let Some((t, _)) = &self.underlying {
            out.push(t.clone());
        }
        out.extend(self.vix_topic.clone());
        let mut opts: Vec<(&String, f64)> =
            self.options.iter().map(|(t, c)| (t, (c.strike - spot).abs())).collect();
        opts.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        out.extend(opts.into_iter().map(|(t, _)| t.clone()));
        out
    }
}

/// Works out the socket topics for a chain response. Contracts without a
/// `fyToken` are resolved through the symbol-token API (best effort: those
/// that cannot be resolved just keep their REST prices).
pub async fn stream_plan(cfg: &FyersConfig, creds: &FyersCreds, data: &Data) -> StreamPlan {
    let mut options = HashMap::new();
    let mut missing: Vec<Contract> = Vec::new();
    for c in contracts(data) {
        match topics::token_topic(&c.fytoken) {
            Some(t) => {
                options.insert(t, c);
            }
            None => missing.push(c),
        }
    }
    let under = underlying(data);
    let mut under_topic = under.as_ref().and_then(|u| {
        if u.symbol.ends_with("-INDEX") { topics::index_topic(&u.symbol) } else { topics::token_topic(&u.fytoken) }
    });
    let mut need: Vec<String> = missing.iter().map(|c| c.symbol.clone()).collect();
    if let (Some(u), None) = (&under, &under_topic) {
        need.push(u.symbol.clone());
    }
    if !need.is_empty() {
        match topics::resolve(cfg, creds, &need).await {
            Ok(map) => {
                for c in missing {
                    if let Some(t) = map.get(&c.symbol) {
                        options.insert(t.clone(), c);
                    }
                }
                if under_topic.is_none() {
                    under_topic = under.as_ref().and_then(|u| map.get(&u.symbol).cloned());
                }
            }
            Err(e) => tracing::warn!("Fyers socket: {e}"),
        }
    }
    StreamPlan {
        options,
        underlying: under_topic.zip(under),
        vix_topic: topics::index_topic(topics::VIX_SYMBOL),
    }
}

#[cfg(test)]
mod quote_tests {
    use super::*;

    #[test]
    fn parses_quotes_and_skips_bad_items() {
        let body = r#"{"s":"ok","code":200,"d":[
            {"n":"NSE:NIFTY50-INDEX","s":"ok","v":{"lp":24500.5,"prev_close_price":24400.0,"open_price":24410.0,"high_price":24550.0,"low_price":24390.0}},
            {"n":"NSE:BAD-INDEX","s":"error","v":{"errmsg":"Invalid symbol"}},
            {"n":"NSE:INDIAVIX-INDEX","s":"ok","v":{"lp":0}}
        ]}"#;
        let q = parse_quotes(body).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].symbol, "NSE:NIFTY50-INDEX");
        assert_eq!(q[0].prev_close, 24400.0);
        assert_eq!(q[0].high, 24550.0);
    }

    #[test]
    fn quotes_error_and_garbage() {
        assert!(parse_quotes(r#"{"s":"error","message":"token expired"}"#).unwrap_err().to_string().contains("token expired"));
        assert!(parse_quotes("<html>").is_err());
    }
}