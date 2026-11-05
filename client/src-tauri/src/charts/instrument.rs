//! Instrument keys used in the `bars` table and the mapping to Fyers symbols.
//!
//! * Index:  `IDX:NIFTY 50`
//! * Option: `OPT:NIFTY:2026-11-24:24000:CE` (symbol : expiry ISO : strike : CE|PE)

use chrono::{Datelike, Duration, NaiveDate};

pub fn index_key(name: &str) -> String {
    format!("IDX:{name}")
}

fn fmt_strike(strike: f64) -> String {
    if strike.fract().abs() < 1e-9 {
        format!("{}", strike as i64)
    } else {
        format!("{strike}")
    }
}

pub fn option_key(symbol: &str, expiry_iso: &str, strike: f64, is_call: bool) -> String {
    format!("OPT:{symbol}:{expiry_iso}:{}:{}", fmt_strike(strike), if is_call { "CE" } else { "PE" })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Instrument {
    Index(String),
    Option { symbol: String, expiry: NaiveDate, strike: String, is_call: bool },
}

pub fn parse(key: &str) -> Option<Instrument> {
    if let Some(name) = key.strip_prefix("IDX:") {
        return (!name.is_empty()).then(|| Instrument::Index(name.to_string()));
    }
    let rest = key.strip_prefix("OPT:")?;
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 4 {
        return None;
    }
    let expiry = NaiveDate::parse_from_str(parts[1], "%Y-%m-%d").ok()?;
    let is_call = match parts[3] {
        "CE" => true,
        "PE" => false,
        _ => return None,
    };
    Some(Instrument::Option {
        symbol: parts[0].to_string(),
        expiry,
        strike: parts[2].to_string(),
        is_call,
    })
}

/// Expiry date (ISO) for option keys, used for retention.
pub fn expiry_of(key: &str) -> Option<String> {
    match parse(key)? {
        Instrument::Option { expiry, .. } => Some(expiry.format("%Y-%m-%d").to_string()),
        _ => None,
    }
}

fn fyers_index(name: &str) -> String {
    match name.to_uppercase().as_str() {
        "NIFTY 50" => "NSE:NIFTY50-INDEX".into(),
        "NIFTY BANK" => "NSE:NIFTYBANK-INDEX".into(),
        "INDIA VIX" => "NSE:INDIAVIX-INDEX".into(),
        "NIFTY FINANCIAL SERVICES" => "NSE:FINNIFTY-INDEX".into(),
        "NIFTY MIDCAP SELECT" => "NSE:MIDCPNIFTY-INDEX".into(),
        "NIFTY NEXT 50" => "NSE:NIFTYNXT50-INDEX".into(),
        other => format!("NSE:{}-INDEX", other.replace(' ', "")),
    }
}

fn month_code(m: u32) -> String {
    match m {
        1..=9 => m.to_string(),
        10 => "O".into(),
        11 => "N".into(),
        _ => "D".into(),
    }
}

const MONTHS: [&str; 12] =
    ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];

/// True if no later date with the same weekday exists in the same month.
fn is_last_of_month(d: NaiveDate) -> bool {
    (d + Duration::days(7)).month() != d.month()
}

/// Candidate Fyers symbols for an instrument, best guess first. Options are
/// either weekly (`NIFTY2611324000CE`) or monthly (`NIFTY26NOV24000CE`); the
/// caller tries each until one returns data.
pub fn fyers_symbols(key: &str) -> Vec<String> {
    match parse(key) {
        Some(Instrument::Index(name)) => vec![fyers_index(&name)],
        Some(Instrument::Option { symbol, expiry, strike, is_call }) => {
            let yy = expiry.year() % 100;
            let ce = if is_call { "CE" } else { "PE" };
            let weekly = format!(
                "NSE:{symbol}{yy:02}{}{:02}{strike}{ce}",
                month_code(expiry.month()),
                expiry.day()
            );
            let monthly = format!(
                "NSE:{symbol}{yy:02}{}{strike}{ce}",
                MONTHS[(expiry.month() - 1) as usize]
            );
            if is_last_of_month(expiry) {
                vec![monthly, weekly]
            } else {
                vec![weekly, monthly]
            }
        }
        None => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_roundtrip() {
        let k = option_key("NIFTY", "2026-11-24", 24000.0, true);
        assert_eq!(k, "OPT:NIFTY:2026-11-24:24000:CE");
        assert!(matches!(parse(&k), Some(Instrument::Option { is_call: true, .. })));
        assert_eq!(expiry_of(&k).as_deref(), Some("2026-11-24"));
        assert_eq!(parse("IDX:NIFTY 50"), Some(Instrument::Index("NIFTY 50".into())));
    }

    #[test]
    fn fyers_symbol_formats() {
        // 2026-11-24 is the last Tuesday of November: monthly first.
        let s = fyers_symbols("OPT:NIFTY:2026-11-24:24000:CE");
        assert_eq!(s[0], "NSE:NIFTY26NOV24000CE");
        assert_eq!(s[1], "NSE:NIFTY26N2424000CE");
        // 2026-11-10 is a mid-month weekly.
        let s = fyers_symbols("OPT:NIFTY:2026-11-10:24000:PE");
        assert_eq!(s[0], "NSE:NIFTY26N1024000PE");
        assert_eq!(fyers_symbols("IDX:NIFTY BANK"), vec!["NSE:NIFTYBANK-INDEX"]);
    }
}
