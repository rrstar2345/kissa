//! CSV export of orders, trades and the trader's diary.
//!
//! * `Date` is the IST trade date the record belongs to (the bucket the
//!   Dashboard uses); times are IST too.
//! * Rows are oldest first and uncapped (the Orders page lists are limited).
//! * The diary reuses `period::fold`, so a row is the same round trip the
//!   Dashboard lists, dated on the day it closed.
//! * Text that comes from outside the app (account names, broker messages and
//!   ids) is defused against spreadsheet formula injection; numbers are never
//!   touched, so negative values stay numbers.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;
use sqlx::SqlitePool;

use super::period::{self, TripRow};
use super::pricing::r2;
use super::{store, OrderRow, TradeRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dataset {
    Orders,
    Trades,
    Diary,
}

impl Dataset {
    pub fn parse(s: &str) -> Result<Dataset> {
        match s {
            "orders" => Ok(Dataset::Orders),
            "trades" => Ok(Dataset::Trades),
            "diary" => Ok(Dataset::Diary),
            other => bail!("unknown export '{other}'"),
        }
    }

    fn stem(self) -> &'static str {
        match self {
            Dataset::Orders => "orders",
            Dataset::Trades => "trades",
            Dataset::Diary => "diary",
        }
    }

    /// Plural noun for messages and the audit journal.
    pub fn label(self) -> &'static str {
        match self {
            Dataset::Orders => "orders",
            Dataset::Trades => "trades",
            Dataset::Diary => "diary trades",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    /// `orders` | `trades` | `diary`
    pub dataset: String,
    /// IST dates, `YYYY-MM-DD`, inclusive.
    pub from: String,
    pub to: String,
    /// `None` or empty = all accounts.
    #[serde(default)]
    pub account_id: Option<String>,
}

pub struct Rendered {
    pub csv: String,
    /// Data rows (the header is not counted).
    pub rows: usize,
    pub file_name: String,
    pub dataset: Dataset,
    pub from: String,
    pub to: String,
}

// ---------------------------------------------------------------- helpers ---

/// Validate the range and return it in canonical `YYYY-MM-DD` form.
fn check_range(from: &str, to: &str) -> Result<(String, String)> {
    let parse = |s: &str, what: &str| {
        NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| anyhow!("{what} must be a date (YYYY-MM-DD)"))
    };
    let (f, t) = (parse(from, "From")?, parse(to, "To")?);
    if f > t {
        bail!("From must not be after To");
    }
    Ok((f.format("%Y-%m-%d").to_string(), t.format("%Y-%m-%d").to_string()))
}

fn file_name(ds: Dataset, from: &str, to: &str) -> String {
    if from == to {
        format!("kissa-{}-{from}.csv", ds.stem())
    } else {
        format!("kissa-{}-{from}_to_{to}.csv", ds.stem())
    }
}

/// IST `YYYY-MM-DD HH:MM:SS` of an RFC 3339 timestamp (the raw text if it cannot be parsed).
fn ist_stamp(ts: &str) -> String {
    match DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (t.with_timezone(&Utc) + Duration::seconds(5 * 3600 + 30 * 60)).format("%Y-%m-%d %H:%M:%S").to_string(),
        Err(_) => ts.to_string(),
    }
}

/// Two decimals, never `-0.00`.
fn money(v: f64) -> String {
    let v = r2(v);
    if v == 0.0 { "0.00".into() } else { format!("{v:.2}") }
}

/// A strike without a trailing `.0` (24000, 24000.5).
fn plain(v: f64) -> String {
    format!("{v}")
}

fn kind(is_call: bool) -> &'static str {
    if is_call { "CE" } else { "PE" }
}

/// Text a spreadsheet could read as a formula gets a leading `'`.
fn text(s: &str) -> String {
    match s.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => format!("'{s}"),
        _ => s.to_string(),
    }
}

fn account_name(names: &HashMap<String, String>, id: &str) -> String {
    match names.get(id) {
        Some(n) if !n.is_empty() => n.clone(),
        _ => id.to_string(),
    }
}

fn write(header: &[&str], rows: Vec<Vec<String>>) -> Result<String> {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(header)?;
    for r in &rows {
        w.write_record(r)?;
    }
    let bytes = w.into_inner().map_err(|e| anyhow!("csv: {}", e.error()))?;
    Ok(String::from_utf8(bytes)?)
}

// ---------------------------------------------------------------- builders ---

const ORDER_HEADER: &[&str] = &[
    "Date", "Time (IST)", "Account", "Account kind", "Order ID", "Broker order ID", "Symbol", "Expiry", "Strike", "Type",
    "Side", "Product", "Order type", "Validity", "Lot size", "Qty", "Price", "Trigger", "Status", "Filled qty",
    "Avg price", "Role", "Message",
];

fn order_record(o: &OrderRow, names: &HashMap<String, String>) -> Vec<String> {
    vec![
        o.trade_date.clone(),
        ist_stamp(&o.created_at),
        text(&account_name(names, &o.account_id)),
        o.account_kind.clone(),
        o.id.clone(),
        text(o.broker_order_id.as_deref().unwrap_or("")),
        o.symbol.clone(),
        o.expiry.clone(),
        plain(o.strike),
        kind(o.is_call).into(),
        o.side.clone(),
        o.product.clone(),
        o.order_type.clone(),
        o.validity.clone(),
        o.lot_size.to_string(),
        o.qty.to_string(),
        money(o.price),
        if o.order_type == "STOP_LOSS" { money(o.trigger_price) } else { String::new() },
        o.status.clone(),
        o.filled_qty.to_string(),
        if o.filled_qty > 0 { money(o.avg_price) } else { String::new() },
        o.role.clone(),
        text(&o.message),
    ]
}

pub fn orders_csv(rows: &[OrderRow], names: &HashMap<String, String>) -> Result<String> {
    write(ORDER_HEADER, rows.iter().map(|o| order_record(o, names)).collect())
}

const TRADE_HEADER: &[&str] = &[
    "Date", "Time (IST)", "Account", "Trade ID", "Order ID", "Broker trade ID", "Symbol", "Expiry", "Strike", "Type", "Side",
    "Product", "Lot size", "Qty", "Price", "Value", "Realised P&L", "Charges", "Net P&L",
];

fn trade_record(t: &TradeRow, names: &HashMap<String, String>) -> Vec<String> {
    vec![
        t.trade_date.clone(),
        ist_stamp(&t.ts),
        text(&account_name(names, &t.account_id)),
        t.id.clone(),
        t.order_id.clone(),
        text(t.ext_id.as_deref().unwrap_or("")),
        t.symbol.clone(),
        t.expiry.clone(),
        plain(t.strike),
        kind(t.is_call).into(),
        t.side.clone(),
        t.product.clone(),
        t.lot_size.to_string(),
        t.qty.to_string(),
        money(t.price),
        money(t.price * t.qty as f64),
        money(t.realised),
        money(t.charges),
        money(t.realised - t.charges),
    ]
}

pub fn trades_csv(rows: &[TradeRow], names: &HashMap<String, String>) -> Result<String> {
    write(TRADE_HEADER, rows.iter().map(|t| trade_record(t, names)).collect())
}

const DIARY_HEADER: &[&str] = &[
    "Date", "Account", "Symbol", "Expiry", "Strike", "Type", "Direction", "Product", "Qty", "Entry price", "Exit price",
    "Entry time (IST)", "Exit time (IST)", "Gross P&L", "Charges", "Net P&L", "Status",
];

fn diary_record(date: &str, r: &TripRow) -> Vec<String> {
    let account = if r.account_name.is_empty() { &r.account_id } else { &r.account_name };
    let direction = if r.long { "Long" } else { "Short" };
    let status = if r.closed { "Closed" } else { "Open" };
    vec![
        date.to_string(),
        text(account),
        r.symbol.clone(),
        r.expiry.clone(),
        plain(r.strike),
        kind(r.is_call).into(),
        direction.to_string(),
        r.product.clone(),
        r.qty.to_string(),
        money(r.entry_price),
        r.exit_price.map(money).unwrap_or_default(),
        ist_stamp(&r.entry_ts),
        r.exit_ts.as_deref().map(ist_stamp).unwrap_or_default(),
        money(r.gross),
        money(r.charges),
        money(r.net),
        status.to_string(),
    ]
}

pub fn diary_csv(rows: &[(String, TripRow)]) -> Result<String> {
    write(DIARY_HEADER, rows.iter().map(|(d, r)| diary_record(d, r)).collect())
}

// ------------------------------------------------------------------ render ---

/// Load the records of a request and render them as CSV text.
pub async fn render(pool: &SqlitePool, req: &ExportRequest, names: &HashMap<String, String>) -> Result<Rendered> {
    let ds = Dataset::parse(&req.dataset)?;
    let (from, to) = check_range(&req.from, &req.to)?;
    let account = req.account_id.as_deref().filter(|a| !a.is_empty());

    let (csv, rows) = match ds {
        Dataset::Orders => {
            let rows = store::orders_between(pool, account, &from, &to).await?;
            (orders_csv(&rows, names)?, rows.len())
        }
        Dataset::Trades => {
            let rows = store::trades_between(pool, account, &from, &to).await?;
            (trades_csv(&rows, names)?, rows.len())
        }
        Dataset::Diary => {
            // Whole history of the contracts traded in the range, so a trip that
            // opened before `from` still closes with the right result.
            let trades = store::trades_for_window(pool, account, &from, &to).await?;
            let rows = period::diary_rows(&trades, &from, &to, names);
            (diary_csv(&rows)?, rows.len())
        }
    };
    Ok(Rendered { csv, rows, file_name: file_name(ds, &from, &to), dataset: ds, from, to })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(csv: &str) -> (Vec<String>, Vec<Vec<String>>) {
        let mut rdr = csv::Reader::from_reader(csv.as_bytes());
        let head = rdr.headers().unwrap().iter().map(String::from).collect();
        let rows = rdr.records().map(|r| r.unwrap().iter().map(String::from).collect()).collect();
        (head, rows)
    }

    fn names() -> HashMap<String, String> {
        HashMap::from([("a".to_string(), "Paper".to_string())])
    }

    fn trade(id: &str, side: &str, qty: i64, price: f64, realised: f64, charges: f64, date: &str, ts: &str) -> TradeRow {
        TradeRow {
            id: id.into(),
            account_id: "a".into(),
            order_id: format!("o{id}"),
            instrument: "X".into(),
            symbol: "NIFTY".into(),
            expiry: "2026-11-24".into(),
            strike: 24000.0,
            is_call: true,
            lot_size: 65,
            side: side.into(),
            product: "INTRADAY".into(),
            qty,
            price,
            charges,
            charge_detail: "{}".into(),
            realised,
            ts: ts.into(),
            trade_date: date.into(),
            ext_id: None,
        }
    }

    fn order() -> OrderRow {
        OrderRow {
            id: "o1".into(),
            account_id: "a".into(),
            account_kind: "paper".into(),
            correlation_id: "c1".into(),
            broker_order_id: None,
            instrument: "X".into(),
            symbol: "NIFTY".into(),
            expiry: "2026-11-24".into(),
            strike: 24000.0,
            is_call: true,
            security_id: None,
            lot_size: 65,
            side: "BUY".into(),
            product: "INTRADAY".into(),
            order_type: "LIMIT".into(),
            validity: "DAY".into(),
            qty: 130,
            price: 101.5,
            trigger_price: 0.0,
            status: "CANCELLED".into(),
            filled_qty: 0,
            avg_price: 0.0,
            message: "=HYPERLINK(\"x\")".into(),
            created_at: "2026-11-09T04:00:00.000Z".into(),
            updated_at: "2026-11-09T04:01:00.000Z".into(),
            trade_date: "2026-11-09".into(),
            group_id: String::new(),
            role: String::new(),
            bracket: String::new(),
        }
    }

    #[test]
    fn orders_are_ist_fixed_decimal_and_match_the_header() {
        let (head, rows) = parse(&orders_csv(&[order()], &names()).unwrap());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), head.len());
        let col = |n: &str| head.iter().position(|h| h == n).unwrap();
        let r = &rows[0];
        assert_eq!(r[col("Time (IST)")], "2026-11-09 09:30:00");
        assert_eq!(r[col("Account")], "Paper");
        assert_eq!((r[col("Strike")].as_str(), r[col("Type")].as_str()), ("24000", "CE"));
        assert_eq!(r[col("Price")], "101.50");
        assert_eq!((r[col("Trigger")].as_str(), r[col("Avg price")].as_str()), ("", ""));
        // a message that starts with "=" must not run as a formula
        assert_eq!(r[col("Message")], "'=HYPERLINK(\"x\")");
    }

    #[test]
    fn trades_compute_value_and_net_and_keep_negatives_numeric() {
        let t = trade("1", "SELL", 130, 50.0, -650.0, 12.34, "2026-11-09", "2026-11-09T04:00:00.000Z");
        let (head, rows) = parse(&trades_csv(&[t], &names()).unwrap());
        assert_eq!(rows[0].len(), head.len());
        let col = |n: &str| head.iter().position(|h| h == n).unwrap();
        assert_eq!(rows[0][col("Value")], "6500.00");
        assert_eq!(rows[0][col("Realised P&L")], "-650.00");
        assert_eq!(rows[0][col("Charges")], "12.34");
        assert_eq!(rows[0][col("Net P&L")], "-662.34");
    }

    #[test]
    fn number_and_text_helpers() {
        assert_eq!(money(-0.001), "0.00");
        assert_eq!(money(-1020.0), "-1020.00");
        assert_eq!(plain(24000.5), "24000.5");
        assert_eq!(text("=1+1"), "'=1+1");
        assert_eq!(text("-cmd"), "'-cmd");
        assert_eq!(text("Paper"), "Paper");
        assert_eq!(text(""), "");
    }

    #[test]
    fn diary_has_one_row_per_round_trip() {
        let t = [
            trade("1", "BUY", 100, 50.0, 0.0, 10.0, "2026-11-09", "2026-11-09T04:00:00.000Z"),
            trade("2", "SELL", 100, 60.0, 1000.0, 10.0, "2026-11-10", "2026-11-10T04:00:00.000Z"),
            trade("3", "BUY", 10, 5.0, 0.0, 0.0, "2026-11-12", "2026-11-12T04:00:00.000Z"),
        ];
        let rows = period::diary_rows(&t, "2026-11-01", "2026-11-30", &names());
        let (head, out) = parse(&diary_csv(&rows).unwrap());
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.len() == head.len()));
        let col = |n: &str| head.iter().position(|h| h == n).unwrap();

        let done = &out[0];
        assert_eq!(done[col("Date")], "2026-11-10");
        assert_eq!((done[col("Direction")].as_str(), done[col("Qty")].as_str()), ("Long", "100"));
        assert_eq!((done[col("Entry price")].as_str(), done[col("Exit price")].as_str()), ("50.00", "60.00"));
        assert_eq!(done[col("Entry time (IST)")], "2026-11-09 09:30:00");
        assert_eq!(done[col("Exit time (IST)")], "2026-11-10 09:30:00");
        assert_eq!((done[col("Gross P&L")].as_str(), done[col("Charges")].as_str()), ("1000.00", "20.00"));
        assert_eq!((done[col("Net P&L")].as_str(), done[col("Status")].as_str()), ("980.00", "Closed"));

        let open = &out[1];
        assert_eq!((open[col("Exit price")].as_str(), open[col("Exit time (IST)")].as_str()), ("", ""));
        assert_eq!(open[col("Status")], "Open");
    }

    #[test]
    fn range_and_file_names() {
        assert_eq!(check_range(" 2026-11-01", "2026-11-16").unwrap(), ("2026-11-01".into(), "2026-11-16".into()));
        assert!(check_range("2026-11-17", "2026-11-16").is_err());
        assert!(check_range("nope", "2026-11-16").is_err());
        assert_eq!(file_name(Dataset::Trades, "2026-11-01", "2026-11-16"), "kissa-trades-2026-11-01_to_2026-11-16.csv");
        assert_eq!(file_name(Dataset::Diary, "2026-11-16", "2026-11-16"), "kissa-diary-2026-11-16.csv");
        assert!(Dataset::parse("diary").is_ok() && Dataset::parse("fills").is_err());
    }
}
