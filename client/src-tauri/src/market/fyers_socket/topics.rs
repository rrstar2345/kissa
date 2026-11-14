//! Topic names for the Fyers data socket, the access-token claims, and the
//! symbol-token lookup used when the chain response has no `fyToken`.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::time::Duration;

use crate::chain::fyers::FyersCreds;
use crate::settings::FyersConfig;

/// Segment name from the first four digits of a `fyToken`.
pub fn segment(prefix: &str) -> Option<&'static str> {
    Some(match prefix {
        "1010" => "nse_cm",
        "1011" => "nse_fo",
        "1120" => "mcx_fo",
        "1210" => "bse_cm",
        "1211" => "bse_fo",
        "1212" => "bcs_fo",
        "1012" => "cde_fo",
        "1020" => "nse_com",
        _ => return None,
    })
}

/// (Fyers symbol, name the socket answers to, name the NSE index stream uses).
/// The socket names come from the official SDK's `map.json`.
const INDICES: [(&str, &str, &str); 6] = [
    ("NSE:NIFTY50-INDEX", "Nifty 50", "NIFTY 50"),
    ("NSE:NIFTYBANK-INDEX", "Nifty Bank", "NIFTY BANK"),
    ("NSE:FINNIFTY-INDEX", "Nifty Fin Service", "NIFTY FINANCIAL SERVICES"),
    ("NSE:MIDCPNIFTY-INDEX", "NIFTY MID SELECT", "NIFTY MIDCAP SELECT"),
    ("NSE:NIFTYNXT50-INDEX", "Nifty Next 50", "NIFTY NEXT 50"),
    ("NSE:INDIAVIX-INDEX", "India VIX", "INDIA VIX"),
];

/// Fyers symbol of the India VIX index.
pub const VIX_SYMBOL: &str = "NSE:INDIAVIX-INDEX";

/// Topic for an index symbol such as `NSE:NIFTY50-INDEX`.
pub fn index_topic(symbol: &str) -> Option<String> {
    let seg = if symbol.starts_with("NSE:") {
        "nse_cm"
    } else if symbol.starts_with("BSE:") {
        "bse_cm"
    } else {
        return None;
    };
    let name = match INDICES.iter().find(|i| i.0 == symbol) {
        Some(i) => i.1.to_string(),
        None => symbol.split(':').nth(1)?.strip_suffix("-INDEX")?.to_string(),
    };
    Some(format!("if|{seg}|{name}"))
}

/// Topic for a derivative/equity contract from its 15-digit `fyToken`.
pub fn token_topic(fytoken: &str) -> Option<String> {
    let seg = segment(fytoken.get(..4)?)?;
    let id = fytoken.get(10..).filter(|s| !s.is_empty())?;
    Some(format!("sf|{seg}|{id}"))
}

/// Name the NSE index stream (and so the header ticker and `IndexStore`) uses
/// for an index topic, matched without regard to case.
pub fn nse_index_name(topic: &str) -> Option<String> {
    let name = topic.split('|').nth(2)?;
    INDICES
        .iter()
        .find(|i| i.1.eq_ignore_ascii_case(name))
        .map(|i| i.2.to_string())
}

/// Fyers symbols of the indices the header shows.
pub fn header_index_symbols() -> Vec<&'static str> {
    INDICES.iter().map(|i| i.0).collect()
}

/// Name the NSE index stream uses for a Fyers index symbol.
pub fn nse_name_for_symbol(symbol: &str) -> Option<String> {
    INDICES.iter().find(|i| i.0 == symbol).map(|i| i.2.to_string())
}

/// Topics of the indices the header shows.
pub fn header_index_topics() -> Vec<String> {
    INDICES.iter().filter_map(|i| index_topic(i.0)).collect()
}

// ------------------------------------------------------------------ JWT ---

#[derive(Debug, PartialEq)]
pub struct Jwt {
    pub hsm_key: String,
    /// Expiry, seconds since the epoch.
    pub exp: Option<i64>,
}

fn b64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Reads `hsm_key` and `exp` from an access token (an optional `appid:`
/// prefix is ignored). The signature is not checked; Fyers does that.
pub fn parse_jwt(token: &str) -> Option<Jwt> {
    let token = token.rsplit(':').next()?;
    let payload = token.split('.').nth(1)?;
    let v: serde_json::Value = serde_json::from_slice(&b64url(payload)?).ok()?;
    let hsm_key = v.get("hsm_key")?.as_str()?.to_string();
    if hsm_key.is_empty() {
        return None;
    }
    Some(Jwt { hsm_key, exp: v.get("exp").and_then(|e| e.as_i64()) })
}

// ---------------------------------------------------------- symbol-token ---

/// Resolves Fyers symbols to socket topics through the symbol-token API.
/// Symbols the API does not know are left out of the result.
pub async fn resolve(
    cfg: &FyersConfig,
    creds: &FyersCreds,
    symbols: &[String],
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    if symbols.is_empty() {
        return Ok(out);
    }
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
    for chunk in symbols.chunks(200) {
        let resp = client
            .post(&cfg.symbol_token_url)
            .header("Authorization", format!("{}:{}", creds.app_id, creds.access_token))
            .json(&serde_json::json!({ "symbols": chunk }))
            .send()
            .await
            .map_err(|e| anyhow!("Fyers symbol lookup failed: {e}"))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("Fyers symbol lookup returned an unexpected response: {e}"))?;
        if body.get("s").and_then(|s| s.as_str()) != Some("ok") {
            let m = body.get("message").and_then(|m| m.as_str()).unwrap_or("error");
            return Err(anyhow!("Fyers symbol lookup: {m}"));
        }
        if let Some(map) = body.get("validSymbol").and_then(|v| v.as_object()) {
            for (sym, tok) in map {
                let topic = if sym.ends_with("-INDEX") {
                    index_topic(sym)
                } else {
                    tok.as_str().and_then(token_topic)
                };
                if let Some(t) = topic {
                    out.insert(sym.clone(), t);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_from_tokens_and_indices() {
        assert_eq!(token_topic("101110000035003").as_deref(), Some("sf|nse_fo|35003"));
        assert_eq!(token_topic("1011"), None);
        assert_eq!(token_topic("999900000035003"), None);
        assert_eq!(index_topic("NSE:NIFTY50-INDEX").as_deref(), Some("if|nse_cm|Nifty 50"));
        assert_eq!(index_topic("NSE:NIFTYIT-INDEX").as_deref(), Some("if|nse_cm|NIFTYIT"));
        assert_eq!(index_topic("MCX:GOLD"), None);
    }

    #[test]
    fn header_symbols_map_to_nse_names() {
        assert_eq!(header_index_symbols().len(), 6);
        assert_eq!(nse_name_for_symbol("NSE:NIFTYBANK-INDEX").as_deref(), Some("NIFTY BANK"));
        assert_eq!(nse_name_for_symbol("NSE:NIFTYIT-INDEX"), None);
    }

    #[test]
    fn nse_names_match_case_insensitively() {
        assert_eq!(nse_index_name("if|nse_cm|NIFTY BANK").as_deref(), Some("NIFTY BANK"));
        assert_eq!(nse_index_name("if|nse_cm|India VIX").as_deref(), Some("INDIA VIX"));
        assert_eq!(nse_index_name("sf|nse_fo|35003"), None);
    }

    #[test]
    fn jwt_claims() {
        let t = "eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjE5MDAwMDAwMDAsImhzbV9rZXkiOiJhYmMxMjNfa2V5Iiwic3ViIjoieD4_In0.sig";
        let j = parse_jwt(t).unwrap();
        assert_eq!(j, Jwt { hsm_key: "abc123_key".into(), exp: Some(1_900_000_000) });
        assert_eq!(parse_jwt(&format!("APP-100:{t}")), Some(j));
        assert_eq!(parse_jwt("not.a.jwt"), None);
        assert_eq!(parse_jwt("nodots"), None);
    }
}