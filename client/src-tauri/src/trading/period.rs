//! Dashboard performance: round-trip trades and per-period aggregation.
//!
//! * **Attribution**: every fill carries its own realised P&L (booked when it
//!   closes part of a position) and its own charges, and both belong to the IST
//!   hour / day / month of that fill. Open positions (unrealised) are not part
//!   of it.
//! * **Trade** = one round trip: it starts when a position goes from flat to
//!   held and ends when it is flat again. A fill that flips the position closes
//!   one trade and opens the next. It counts when it closes.
//! * **ROI** = net P&L / entry premium, the value (price x qty) of the fills
//!   that opened or added to a position.

use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, Timelike, Utc};
use serde::Serialize;
use sqlx::SqlitePool;

use super::pricing::r2;
use super::store::{self, apply_fill, sign, Lot};
use super::TradeRow;

const IST_SECS: i64 = 5 * 3600 + 30 * 60;
/// Hour a settlement booked on a later day is shown in (the market close).
const SETTLE_HOUR: u32 = 15;

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// (IST date, IST hour) of an RFC 3339 timestamp.
fn ist_parts(ts: &str) -> Option<(String, u32)> {
    let t = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc) + Duration::seconds(IST_SECS);
    Some((t.format("%Y-%m-%d").to_string(), t.hour()))
}

/// The bucket of a record: its trade date, and the hour of `ts` when `ts` falls
/// on that date (an expiry settlement booked on a later day goes to the close).
fn when(trade_date: &str, ts: &str) -> (String, u32) {
    match ist_parts(ts) {
        Some((d, h)) if d == trade_date => (d, h),
        _ => (trade_date.to_string(), SETTLE_HOUR),
    }
}

// ------------------------------------------------------------ round trips ---

/// What one fill adds to the totals.
#[derive(Debug, Clone)]
pub struct Fact {
    pub account_id: String,
    pub date: String,
    pub hour: u32,
    pub realised: f64,
    pub charges: f64,
    /// Price x qty of the part that opened or added to a position.
    pub entry_value: f64,
    /// 1 when this fill flattened a position (a trade closed).
    pub closed: i64,
}

/// One round trip.
#[derive(Debug, Clone)]
pub struct Trip {
    pub id: String,
    pub account_id: String,
    pub symbol: String,
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    pub product: String,
    pub long: bool,
    pub entry_qty: i64,
    pub entry_value: f64,
    pub exit_qty: i64,
    pub exit_value: f64,
    pub gross: f64,
    pub charges: f64,
    pub entry_ts: String,
    pub exit_ts: Option<String>,
    pub closed: bool,
    /// Where the trade is listed: its closing fill (or its last fill while open).
    pub date: String,
    pub hour: u32,
}

impl Trip {
    fn start(t: &TradeRow, long: bool) -> Trip {
        Trip {
            id: t.id.clone(),
            account_id: t.account_id.clone(),
            symbol: t.symbol.clone(),
            expiry: t.expiry.clone(),
            strike: t.strike,
            is_call: t.is_call,
            product: t.product.clone(),
            long,
            entry_qty: 0,
            entry_value: 0.0,
            exit_qty: 0,
            exit_value: 0.0,
            gross: 0.0,
            charges: 0.0,
            entry_ts: t.ts.clone(),
            exit_ts: None,
            closed: false,
            date: t.trade_date.clone(),
            hour: 0,
        }
    }
}

/// Walk the fills of every (account, contract, product) in order and return
/// one `Fact` per fill and the round trips. `trades` must be oldest first.
pub fn fold(trades: &[TradeRow]) -> (Vec<Fact>, Vec<Trip>) {
    let mut by_key: HashMap<(&str, &str, &str), Vec<&TradeRow>> = HashMap::new();
    for t in trades {
        by_key.entry((&t.account_id, &t.instrument, &t.product)).or_default().push(t);
    }
    let mut facts = Vec::with_capacity(trades.len());
    let mut trips = Vec::new();

    for fills in by_key.into_values() {
        let mut pos = Lot::default();
        let mut cur: Option<Trip> = None;
        let mut last = (String::new(), 0u32);
        for t in fills {
            let (date, hour) = when(&t.trade_date, &t.ts);
            let dir = sign(&t.side);
            let before = pos.qty;
            let (after, _) = apply_fill(pos, dir, t.qty, t.price);
            pos = after;
            let closing = if before != 0 && before.signum() != dir { before.abs().min(t.qty) } else { 0 };
            let opening = t.qty - closing;
            let share = |q: i64| if t.qty > 0 { t.charges * q as f64 / t.qty as f64 } else { 0.0 };
            let mut closed_now = 0;

            if closing > 0 {
                let tr = cur.get_or_insert_with(|| Trip::start(t, before > 0));
                tr.exit_qty += closing;
                tr.exit_value += t.price * closing as f64;
                tr.gross += t.realised;
                tr.charges += share(closing);
                tr.exit_ts = Some(t.ts.clone());
                if after.qty == 0 || opening > 0 {
                    tr.closed = true;
                    tr.date = date.clone();
                    tr.hour = hour;
                    trips.push(cur.take().expect("trip in progress"));
                    closed_now = 1;
                }
            }
            if opening > 0 {
                let tr = cur.get_or_insert_with(|| Trip::start(t, dir > 0));
                tr.entry_qty += opening;
                tr.entry_value += t.price * opening as f64;
                tr.charges += share(opening);
            }
            last = (date.clone(), hour);
            facts.push(Fact {
                account_id: t.account_id.clone(),
                date,
                hour,
                realised: t.realised,
                charges: t.charges,
                entry_value: t.price * opening as f64,
                closed: closed_now,
            });
        }
        if let Some(mut tr) = cur {
            tr.date = last.0;
            tr.hour = last.1;
            trips.push(tr);
        }
    }
    (facts, trips)
}

/// Round trips closed on an IST date, per account.
pub async fn closed_on(pool: &SqlitePool, date: &str) -> Result<HashMap<String, i64>> {
    let trades = store::trades_for_window(pool, None, date, date).await?;
    let (facts, _) = fold(&trades);
    let mut out: HashMap<String, i64> = HashMap::new();
    for f in facts.into_iter().filter(|f| f.date == date && f.closed > 0) {
        *out.entry(f.account_id).or_default() += f.closed;
    }
    Ok(out)
}

// ----------------------------------------------------------------- layout ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Daily,
    Monthly,
    Yearly,
}

impl View {
    pub fn parse(s: &str) -> Result<View> {
        match s {
            "daily" => Ok(View::Daily),
            "monthly" => Ok(View::Monthly),
            "yearly" => Ok(View::Yearly),
            other => bail!("unknown view '{other}'"),
        }
    }
}

#[derive(Debug, Clone)]
struct Col {
    key: String,
    label: String,
    from: NaiveDate,
    to: NaiveDate,
}

#[derive(Debug)]
struct Layout {
    cols: Vec<Col>,
    /// Range of the cards and the diary.
    sum_from: NaiveDate,
    sum_to: NaiveDate,
}

fn month_cols(first: NaiveDate, n: u32) -> Vec<Col> {
    (0..n)
        .rev()
        .filter_map(|back| {
            let from = first.checked_sub_months(Months::new(back))?;
            let to = from.checked_add_months(Months::new(1))?.pred_opt()?;
            Some(Col { key: from.format("%Y-%m").to_string(), label: from.format("%b %Y").to_string(), from, to })
        })
        .collect()
}

/// Columns for a view. Daily: `anchor` is `YYYY-MM-DD`, 4 days ending on it.
/// Monthly: `anchor` is `YYYY-MM`, 4 months ending on it. Yearly: the same
/// anchor, 12 months ending on it (the cards cover all 12).
fn layout(view: View, anchor: &str) -> Result<Layout> {
    match view {
        View::Daily => {
            let day = NaiveDate::parse_from_str(anchor, "%Y-%m-%d").map_err(|_| anyhow!("bad date '{anchor}'"))?;
            let cols: Vec<Col> = (0..4)
                .rev()
                .filter_map(|back| {
                    let d = day.checked_sub_signed(Duration::days(back))?;
                    Some(Col { key: iso(d), label: d.format("%a %d %b").to_string(), from: d, to: d })
                })
                .collect();
            Ok(Layout { cols, sum_from: day, sum_to: day })
        }
        View::Monthly | View::Yearly => {
            let first = NaiveDate::parse_from_str(&format!("{anchor}-01"), "%Y-%m-%d")
                .map_err(|_| anyhow!("bad month '{anchor}'"))?;
            let cols = month_cols(first, if view == View::Monthly { 4 } else { 12 });
            let (a, b) = (cols.first().ok_or_else(|| anyhow!("no columns"))?, cols.last().unwrap());
            let sum_from = if view == View::Monthly { b.from } else { a.from };
            Ok(Layout { sum_from, sum_to: b.to, cols: cols.clone() })
        }
    }
}

// ----------------------------------------------------------------- output ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub gross: f64,
    pub charges: f64,
    pub net: f64,
    /// Closed round trips.
    pub trades: i64,
    pub fills: i64,
    pub entry_premium: f64,
    /// Net / entry premium, percent. `None` without any entry.
    pub roi: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    pub label: String,
    /// `YYYY-MM-DD` (`YYYY-MM-DD HH:00` for an hour), for tooltips.
    pub date: String,
    pub net: f64,
    pub fills: i64,
    pub trades: i64,
    /// `profit | loss | flat | none`.
    pub state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Column {
    pub key: String,
    pub label: String,
    pub net: f64,
    pub trades: i64,
    pub fills: i64,
    /// Empty cells before the first one in a Monday-first calendar (0 for days).
    pub lead_blank: u32,
    pub current: bool,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TripRow {
    pub id: String,
    pub account_id: String,
    pub account_name: String,
    pub symbol: String,
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    pub product: String,
    pub long: bool,
    pub qty: i64,
    pub entry_price: f64,
    pub exit_price: Option<f64>,
    pub entry_ts: String,
    pub exit_ts: Option<String>,
    pub gross: f64,
    pub charges: f64,
    pub net: f64,
    pub closed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub key: String,
    pub label: String,
    pub orders: i64,
    pub trades: i64,
    pub fills: i64,
    pub gross: f64,
    pub charges: f64,
    pub net: f64,
    pub rows: Vec<TripRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Performance {
    pub view: String,
    pub from: String,
    pub to: String,
    pub summary: Summary,
    pub columns: Vec<Column>,
    /// Hour (daily), day (monthly) or month (yearly) groups, newest first.
    pub groups: Vec<Group>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Agg {
    realised: f64,
    charges: f64,
    fills: i64,
    closed: i64,
    premium: f64,
}

impl Agg {
    fn add(&mut self, f: &Fact) {
        self.realised += f.realised;
        self.charges += f.charges;
        self.fills += 1;
        self.closed += f.closed;
        self.premium += f.entry_value;
    }
    fn merge(&mut self, o: &Agg) {
        self.realised += o.realised;
        self.charges += o.charges;
        self.fills += o.fills;
        self.closed += o.closed;
        self.premium += o.premium;
    }
    fn net(&self) -> f64 {
        r2(self.realised - self.charges)
    }
}

fn cell(label: String, date: String, a: &Agg) -> Cell {
    let net = a.net();
    let state = if a.fills == 0 {
        "none"
    } else if net > 0.0 {
        "profit"
    } else if net < 0.0 {
        "loss"
    } else {
        "flat"
    };
    Cell { label, date, net, fills: a.fills, trades: a.closed, state }
}

/// Group key and label of a bucket for a view.
fn group_of(view: View, date: &str, hour: u32) -> (String, String) {
    match view {
        View::Daily => (format!("{hour:02}"), format!("{hour:02}:00 – {:02}:00", hour + 1)),
        View::Monthly => {
            let label = NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .map(|d| d.format("%a %d %b").to_string())
                .unwrap_or_else(|_| date.to_string());
            (date.to_string(), label)
        }
        View::Yearly => {
            let key = date.get(..7).unwrap_or(date).to_string();
            let label = NaiveDate::parse_from_str(&format!("{key}-01"), "%Y-%m-%d")
                .map(|d| d.format("%b %Y").to_string())
                .unwrap_or_else(|_| key.clone());
            (key, label)
        }
    }
}

fn blank_group(key: String, label: String) -> Group {
    Group { key, label, orders: 0, trades: 0, fills: 0, gross: 0.0, charges: 0.0, net: 0.0, rows: Vec::new() }
}

/// Pure part of the dashboard: `trades` oldest first (see
/// `store::trades_for_window`), `orders` = (trade date, created at).
pub fn compute(
    view: View,
    anchor: &str,
    trades: &[TradeRow],
    orders: &[(String, String)],
    names: &HashMap<String, String>,
) -> Result<Performance> {
    let lay = layout(view, anchor)?;
    let win_from = iso(lay.cols.first().map(|c| c.from).unwrap_or(lay.sum_from));
    let (sum_from, sum_to) = (iso(lay.sum_from), iso(lay.sum_to));
    let (facts, trips) = fold(trades);

    let mut by_day: HashMap<String, Agg> = HashMap::new();
    let mut by_hour: HashMap<(String, u32), Agg> = HashMap::new();
    let mut summary = Agg::default();
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for f in facts.iter().filter(|f| f.date >= win_from && f.date <= sum_to) {
        by_day.entry(f.date.clone()).or_default().add(f);
        by_hour.entry((f.date.clone(), f.hour)).or_default().add(f);
        if f.date >= sum_from {
            summary.add(f);
            let (key, label) = group_of(view, &f.date, f.hour);
            let g = groups.entry(key.clone()).or_insert_with(|| blank_group(key, label));
            g.trades += f.closed;
            g.fills += 1;
            g.gross += f.realised;
            g.charges += f.charges;
        }
    }
    for (date, created) in orders.iter().filter(|(d, _)| *d >= sum_from && *d <= sum_to) {
        let (d, h) = when(date, created);
        let (key, label) = group_of(view, &d, h);
        groups.entry(key.clone()).or_insert_with(|| blank_group(key, label)).orders += 1;
    }
    for t in trips.iter().filter(|t| t.date >= sum_from && t.date <= sum_to) {
        let (key, label) = group_of(view, &t.date, t.hour);
        let g = groups.entry(key.clone()).or_insert_with(|| blank_group(key, label));
        let gross = r2(t.gross);
        let charges = r2(t.charges);
        g.rows.push(TripRow {
            id: t.id.clone(),
            account_id: t.account_id.clone(),
            account_name: names.get(&t.account_id).cloned().unwrap_or_default(),
            symbol: t.symbol.clone(),
            expiry: t.expiry.clone(),
            strike: t.strike,
            is_call: t.is_call,
            product: t.product.clone(),
            long: t.long,
            qty: t.entry_qty,
            entry_price: if t.entry_qty > 0 { r2(t.entry_value / t.entry_qty as f64) } else { 0.0 },
            exit_price: (t.exit_qty > 0).then(|| r2(t.exit_value / t.exit_qty as f64)),
            entry_ts: t.entry_ts.clone(),
            exit_ts: t.exit_ts.clone(),
            gross,
            charges,
            net: r2(gross - charges),
            closed: t.closed,
        });
    }
    let mut groups: Vec<Group> = groups.into_values().collect();
    for g in groups.iter_mut() {
        g.gross = r2(g.gross);
        g.charges = r2(g.charges);
        g.net = r2(g.gross - g.charges);
        g.rows.sort_by(|a, b| a.entry_ts.cmp(&b.entry_ts));
    }
    groups.reverse();

    let mut columns = Vec::new();
    let last = lay.cols.len().saturating_sub(1);
    for (i, c) in lay.cols.iter().enumerate() {
        let mut total = Agg::default();
        let mut cells = Vec::new();
        if view == View::Daily {
            let date = iso(c.from);
            for h in 0..24u32 {
                let a = by_hour.get(&(date.clone(), h)).copied().unwrap_or_default();
                cells.push(cell(format!("{h:02}"), format!("{date} {h:02}:00"), &a));
            }
            total = by_day.get(&date).copied().unwrap_or_default();
        } else {
            let mut d = c.from;
            while d <= c.to {
                let key = iso(d);
                let a = by_day.get(&key).copied().unwrap_or_default();
                total.merge(&a);
                cells.push(cell(d.day().to_string(), key, &a));
                d = match d.succ_opt() {
                    Some(n) => n,
                    None => break,
                };
            }
        }
        columns.push(Column {
            key: c.key.clone(),
            label: c.label.clone(),
            net: total.net(),
            trades: total.closed,
            fills: total.fills,
            lead_blank: if view == View::Daily { 0 } else { c.from.weekday().num_days_from_monday() },
            current: i == last,
            cells,
        });
    }

    let net = summary.net();
    Ok(Performance {
        view: match view {
            View::Daily => "daily",
            View::Monthly => "monthly",
            View::Yearly => "yearly",
        }
        .into(),
        from: sum_from,
        to: sum_to,
        summary: Summary {
            gross: r2(summary.realised),
            charges: r2(summary.charges),
            net,
            trades: summary.closed,
            fills: summary.fills,
            entry_premium: r2(summary.premium),
            roi: (summary.premium > 0.0).then(|| r2(net / summary.premium * 100.0)),
        },
        columns,
        groups,
    })
}

/// Load what a view needs and compute it.
pub async fn load(
    pool: &SqlitePool,
    view: &str,
    anchor: &str,
    account: Option<&str>,
    names: &HashMap<String, String>,
) -> Result<Performance> {
    let view = View::parse(view)?;
    let lay = layout(view, anchor)?;
    let from = iso(lay.cols.first().map(|c| c.from).unwrap_or(lay.sum_from));
    let to = iso(lay.sum_to);
    let trades = store::trades_for_window(pool, account, &from, &to).await?;
    let orders = store::order_times(pool, account, &iso(lay.sum_from), &to).await?;
    compute(view, anchor, &trades, &orders, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fill of account `a` on contract X; `ts` is UTC, so 04:00Z = 09:30 IST.
    fn fill(id: &str, side: &str, qty: i64, price: f64, realised: f64, charges: f64, date: &str, ts: &str) -> TradeRow {
        TradeRow {
            id: id.into(),
            account_id: "a".into(),
            order_id: id.into(),
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

    #[test]
    fn partial_exits_are_one_trade_and_a_flip_starts_the_next() {
        let t = [
            fill("1", "BUY", 100, 50.0, 0.0, 10.0, "2026-11-09", "2026-11-09T04:00:00Z"),
            fill("2", "SELL", 40, 60.0, 400.0, 10.0, "2026-11-09", "2026-11-09T04:30:00Z"),
            fill("3", "SELL", 60, 55.0, 300.0, 10.0, "2026-11-09", "2026-11-09T05:00:00Z"),
            // short 30 after flipping: closes nothing here, opens a short
            fill("4", "SELL", 30, 70.0, 0.0, 10.0, "2026-11-09", "2026-11-09T05:30:00Z"),
        ];
        let (facts, trips) = fold(&t);
        assert_eq!(facts.iter().map(|f| f.closed).sum::<i64>(), 1);
        assert_eq!(trips.len(), 2);
        let done = trips.iter().find(|t| t.closed).unwrap();
        assert_eq!((done.entry_qty, done.exit_qty, done.long), (100, 100, true));
        assert!((done.gross - 700.0).abs() < 1e-9 && (done.charges - 30.0).abs() < 1e-9);
        assert_eq!(done.exit_ts.as_deref(), Some("2026-11-09T05:00:00Z"));
        let open = trips.iter().find(|t| !t.closed).unwrap();
        assert_eq!((open.entry_qty, open.long), (30, false));
        // entry premium counts only the opening fills: 100 x 50 and 30 x 70
        assert!((facts.iter().map(|f| f.entry_value).sum::<f64>() - 7100.0).abs() < 1e-9);
    }

    #[test]
    fn a_flipping_fill_closes_one_trade_and_opens_another() {
        let t = [
            fill("1", "BUY", 50, 100.0, 0.0, 4.0, "2026-11-09", "2026-11-09T04:00:00Z"),
            fill("2", "SELL", 80, 110.0, 500.0, 8.0, "2026-11-09", "2026-11-09T04:30:00Z"),
        ];
        let (facts, trips) = fold(&t);
        assert_eq!(facts[1].closed, 1);
        assert!((facts[1].entry_value - 30.0 * 110.0).abs() < 1e-9);
        let done = trips.iter().find(|t| t.closed).unwrap();
        assert!((done.charges - (4.0 + 8.0 * 50.0 / 80.0)).abs() < 1e-9);
        let open = trips.iter().find(|t| !t.closed).unwrap();
        assert_eq!((open.entry_qty, open.long), (30, false));
        assert!((open.charges - 8.0 * 30.0 / 80.0).abs() < 1e-9);
    }

    #[test]
    fn settlement_booked_later_is_dated_the_expiry_day_at_the_close() {
        assert_eq!(when("2026-11-24", "2026-11-25T03:00:00Z"), ("2026-11-24".into(), 15));
        assert_eq!(when("2026-11-09", "2026-11-09T04:00:00Z"), ("2026-11-09".into(), 9));
    }

    #[test]
    fn layouts_cover_the_expected_ranges() {
        let d = layout(View::Daily, "2026-11-11").unwrap();
        assert_eq!(d.cols.len(), 4);
        assert_eq!(d.cols[0].key, "2026-11-08");
        assert_eq!((iso(d.sum_from), iso(d.sum_to)), ("2026-11-11".into(), "2026-11-11".into()));
        let m = layout(View::Monthly, "2026-02").unwrap();
        assert_eq!(m.cols.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), ["2025-11", "2025-12", "2026-01", "2026-02"]);
        assert_eq!((iso(m.sum_from), iso(m.sum_to)), ("2026-02-01".into(), "2026-02-28".into()));
        let y = layout(View::Yearly, "2026-11").unwrap();
        assert_eq!((y.cols.len(), y.cols[0].key.as_str(), y.cols[11].key.as_str()), (12, "2025-12", "2026-11"));
        assert_eq!((iso(y.sum_from), iso(y.sum_to)), ("2025-12-01".into(), "2026-11-30".into()));
        assert!(layout(View::Monthly, "nonsense").is_err());
    }

    #[test]
    fn daily_view_colours_hours_and_sums_the_day() {
        let t = [
            fill("1", "BUY", 100, 50.0, 0.0, 10.0, "2026-11-11", "2026-11-11T04:00:00Z"), // 09:30
            fill("2", "SELL", 100, 60.0, 1000.0, 10.0, "2026-11-11", "2026-11-11T05:40:00Z"), // 11:10
            fill("3", "BUY", 10, 5.0, 0.0, 0.0, "2026-11-10", "2026-11-10T04:00:00Z"),
            fill("4", "SELL", 10, 5.0, 0.0, 0.0, "2026-11-10", "2026-11-10T04:10:00Z"),
        ];
        let orders = vec![("2026-11-11".to_string(), "2026-11-11T04:00:00Z".to_string())];
        let p = compute(View::Daily, "2026-11-11", &t, &orders, &HashMap::new()).unwrap();
        assert_eq!((p.summary.net, p.summary.gross, p.summary.charges, p.summary.trades), (980.0, 1000.0, 20.0, 1));
        assert_eq!(p.summary.roi, Some(r2(980.0 / 5000.0 * 100.0)));
        let today = p.columns.last().unwrap();
        assert!(today.current && today.cells.len() == 24);
        assert_eq!(today.cells[9].state, "loss"); // entry charges only
        assert_eq!(today.cells[11].state, "profit");
        assert_eq!(today.cells[3].state, "none");
        let yday = &p.columns[2];
        assert_eq!(yday.cells[9].state, "flat"); // fills at zero net
        assert_eq!(p.groups.len(), 2);
        assert_eq!((p.groups[0].key.as_str(), p.groups[0].trades), ("11", 1)); // newest first
        assert_eq!(p.groups[1].orders, 1);
        assert_eq!(p.groups[0].rows.len(), 1);
    }

    #[test]
    fn monthly_view_pads_to_monday_and_groups_by_day() {
        let t = [
            fill("1", "BUY", 100, 50.0, 0.0, 10.0, "2026-11-03", "2026-11-03T04:00:00Z"),
            fill("2", "SELL", 100, 40.0, -1000.0, 10.0, "2026-11-04", "2026-11-04T04:00:00Z"),
        ];
        let p = compute(View::Monthly, "2026-11", &t, &[], &HashMap::new()).unwrap();
        let nov = p.columns.last().unwrap();
        assert_eq!((nov.lead_blank, nov.cells.len()), (6, 30)); // 1 Nov 2026 is a Sunday
        assert_eq!(nov.cells[2].state, "loss");
        assert_eq!(nov.cells[3].state, "loss");
        assert_eq!(p.groups.len(), 2);
        assert_eq!(p.groups[0].key, "2026-11-04");
        // The trade is listed on the day it closed, with its full result.
        assert_eq!(p.groups[0].rows[0].net, -1020.0);
        assert_eq!(p.summary.net, -1020.0);
    }
}
