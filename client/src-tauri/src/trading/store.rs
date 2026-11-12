//! SQLite persistence for orders and trades, plus the position / P&L maths.
//!
//! Positions are not stored: they are folded from the trades table
//! (average-cost method, per account + instrument + product), which keeps the
//! ledger the single source of truth. Realised P&L and charges are booked on
//! each trade row when it is recorded.

use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use sqlx::{AssertSqlSafe, SqlitePool};
use std::collections::{BTreeMap, HashMap};

use super::charges;
use super::{ist_date, now_ts, OrderRow, TradeRow};
use crate::settings::ChargesConfig;

/// If `table` exists but lacks `trade_date` (a leftover from a different
/// schema), rename it aside so the current schema can be created. Nothing is
/// deleted; the old rows stay in `<table>_legacy_<unix-ts>`.
async fn set_aside_stale(pool: &SqlitePool, table: &str) -> Result<()> {
    let cols: Vec<(String,)> =
        sqlx::query_as(AssertSqlSafe(format!("SELECT name FROM pragma_table_info('{table}')")))
            .fetch_all(pool)
            .await?;
    if cols.is_empty() || cols.iter().any(|(n,)| n == "trade_date") {
        return Ok(());
    }
    let new_name = format!("{table}_legacy_{}", Utc::now().timestamp());
    tracing::warn!("table `{table}` has an old layout (no trade_date); renaming to `{new_name}`");
    // Drop its indexes first: index names are global and would clash with ours.
    let idx: Vec<(String,)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = '{table}' AND sql IS NOT NULL"
    )))
    .fetch_all(pool)
    .await?;
    for (name,) in idx {
        sqlx::query(AssertSqlSafe(format!("DROP INDEX IF EXISTS \"{name}\""))).execute(pool).await?;
    }
    sqlx::query(AssertSqlSafe(format!("ALTER TABLE {table} RENAME TO {new_name}")))
        .execute(pool)
        .await?;
    Ok(())
}

/// Add a column to an existing table if it is missing (`CREATE TABLE IF NOT
/// EXISTS` does not change a table from an earlier version). Only constants
/// are interpolated.
async fn ensure_column(pool: &SqlitePool, table: &str, column: &str, decl: &str) -> Result<()> {
    let cols: Vec<(String,)> =
        sqlx::query_as(AssertSqlSafe(format!("SELECT name FROM pragma_table_info('{table}')")))
            .fetch_all(pool)
            .await?;
    if cols.iter().any(|(n,)| n == column) {
        return Ok(());
    }
    sqlx::query(AssertSqlSafe(format!("ALTER TABLE {table} ADD COLUMN {column} {decl}")))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn init(pool: &SqlitePool) -> Result<()> {
    set_aside_stale(pool, "orders").await?;
    set_aside_stale(pool, "trades").await?;
    let stmts = [
        r#"CREATE TABLE IF NOT EXISTS orders (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL,
            account_kind TEXT NOT NULL,
            correlation_id TEXT NOT NULL UNIQUE,
            broker_order_id TEXT,
            instrument TEXT NOT NULL,
            symbol TEXT NOT NULL,
            expiry TEXT NOT NULL,
            strike REAL NOT NULL,
            is_call INTEGER NOT NULL,
            security_id TEXT,
            lot_size INTEGER NOT NULL,
            side TEXT NOT NULL,
            product TEXT NOT NULL,
            order_type TEXT NOT NULL,
            validity TEXT NOT NULL,
            qty INTEGER NOT NULL,
            price REAL NOT NULL,
            trigger_price REAL NOT NULL DEFAULT 0,
            status TEXT NOT NULL,
            filled_qty INTEGER NOT NULL DEFAULT 0,
            avg_price REAL NOT NULL DEFAULT 0,
            message TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            trade_date TEXT NOT NULL,
            group_id TEXT NOT NULL DEFAULT '',
            role TEXT NOT NULL DEFAULT '',
            bracket TEXT NOT NULL DEFAULT ''
        );"#,
        "CREATE INDEX IF NOT EXISTS idx_orders_acct_date ON orders(account_id, trade_date);",
        "CREATE INDEX IF NOT EXISTS idx_orders_broker ON orders(account_id, broker_order_id);",
        "CREATE INDEX IF NOT EXISTS idx_orders_status ON orders(status);",
        r#"CREATE TABLE IF NOT EXISTS trades (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL,
            order_id TEXT NOT NULL,
            instrument TEXT NOT NULL,
            symbol TEXT NOT NULL,
            expiry TEXT NOT NULL,
            strike REAL NOT NULL,
            is_call INTEGER NOT NULL,
            lot_size INTEGER NOT NULL,
            side TEXT NOT NULL,
            product TEXT NOT NULL,
            qty INTEGER NOT NULL,
            price REAL NOT NULL,
            charges REAL NOT NULL DEFAULT 0,
            charge_detail TEXT NOT NULL DEFAULT '{}',
            realised REAL NOT NULL DEFAULT 0,
            ts TEXT NOT NULL,
            trade_date TEXT NOT NULL,
            ext_id TEXT
        );"#,
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_trades_ext ON trades(account_id, ext_id) WHERE ext_id IS NOT NULL;",
        "CREATE INDEX IF NOT EXISTS idx_trades_pos ON trades(account_id, instrument, product, ts);",
        "CREATE INDEX IF NOT EXISTS idx_trades_date ON trades(trade_date);",
        "CREATE INDEX IF NOT EXISTS idx_trades_order ON trades(order_id);",
    ];
    for s in stmts {
        sqlx::query(s).execute(pool).await?;
    }
    // Orders table from before bracket orders / expiry settlement.
    ensure_column(pool, "orders", "group_id", "TEXT NOT NULL DEFAULT ''").await?;
    ensure_column(pool, "orders", "role", "TEXT NOT NULL DEFAULT ''").await?;
    ensure_column(pool, "orders", "bracket", "TEXT NOT NULL DEFAULT ''").await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_orders_group ON orders(group_id) WHERE group_id <> '';")
        .execute(pool)
        .await?;
    Ok(())
}

const ORDER_COLS: &str = "id, account_id, account_kind, correlation_id, broker_order_id, instrument, symbol, expiry, strike, is_call, security_id, lot_size, side, product, order_type, validity, qty, price, trigger_price, status, filled_qty, avg_price, message, created_at, updated_at, trade_date, group_id, role, bracket";

const TRADE_COLS: &str = "id, account_id, order_id, instrument, symbol, expiry, strike, is_call, lot_size, side, product, qty, price, charges, charge_detail, realised, ts, trade_date, ext_id";

const OPEN_SQL: &str = "status IN ('TRANSIT','PENDING','TRIGGERED','PART_TRADED','UNKNOWN')";

// ---------------------------------------------------------------- orders ---

pub async fn insert_order(pool: &SqlitePool, o: &OrderRow) -> Result<()> {
    let marks = vec!["?"; 29].join(", ");
    sqlx::query(AssertSqlSafe(format!("INSERT INTO orders ({ORDER_COLS}) VALUES ({marks})")))
        .bind(&o.id)
        .bind(&o.account_id)
        .bind(&o.account_kind)
        .bind(&o.correlation_id)
        .bind(&o.broker_order_id)
        .bind(&o.instrument)
        .bind(&o.symbol)
        .bind(&o.expiry)
        .bind(o.strike)
        .bind(o.is_call)
        .bind(&o.security_id)
        .bind(o.lot_size)
        .bind(&o.side)
        .bind(&o.product)
        .bind(&o.order_type)
        .bind(&o.validity)
        .bind(o.qty)
        .bind(o.price)
        .bind(o.trigger_price)
        .bind(&o.status)
        .bind(o.filled_qty)
        .bind(o.avg_price)
        .bind(&o.message)
        .bind(&o.created_at)
        .bind(&o.updated_at)
        .bind(&o.trade_date)
        .bind(&o.group_id)
        .bind(&o.role)
        .bind(&o.bracket)
        .execute(pool)
        .await?;
    Ok(())
}

/// Persist the fields that change after placement; bumps `updated_at`.
pub async fn save_order_state(pool: &SqlitePool, o: &OrderRow) -> Result<()> {
    sqlx::query(
        "UPDATE orders SET broker_order_id = ?, status = ?, filled_qty = ?, avg_price = ?, message = ?,
                qty = ?, price = ?, trigger_price = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&o.broker_order_id)
    .bind(&o.status)
    .bind(o.filled_qty)
    .bind(o.avg_price)
    .bind(&o.message)
    .bind(o.qty)
    .bind(o.price)
    .bind(o.trigger_price)
    .bind(now_ts())
    .bind(&o.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Claim the waiting bracket legs of an entry order: returns their JSON and
/// clears the column, or `None` when there is nothing left (already claimed).
/// The single UPDATE makes sure only one caller gets them.
pub async fn take_bracket(pool: &SqlitePool, id: &str) -> Result<Option<String>> {
    let json: Option<(String,)> = sqlx::query_as("SELECT bracket FROM orders WHERE id = ? AND bracket <> ''")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some((json,)) = json else { return Ok(None) };
    let claimed = sqlx::query("UPDATE orders SET bracket = '' WHERE id = ? AND bracket <> ''")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(if claimed == 1 { Some(json) } else { None })
}

/// Replace just the message of an order (e.g. a note about a bracket leg).
pub async fn set_order_message(pool: &SqlitePool, id: &str, message: &str) -> Result<()> {
    sqlx::query("UPDATE orders SET message = ?, updated_at = ? WHERE id = ?")
        .bind(message)
        .bind(now_ts())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Open orders of one bracket group (the entry's id), legs only.
pub async fn open_bracket_legs(pool: &SqlitePool, group_id: &str) -> Result<Vec<OrderRow>> {
    Ok(sqlx::query_as::<_, OrderRow>(AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} FROM orders
         WHERE group_id = ? AND role IN ('TARGET', 'STOP') AND {OPEN_SQL} ORDER BY created_at"
    )))
    .bind(group_id)
    .fetch_all(pool)
    .await?)
}

pub async fn get_order(pool: &SqlitePool, id: &str) -> Result<Option<OrderRow>> {
    Ok(sqlx::query_as::<_, OrderRow>(AssertSqlSafe(format!("SELECT {ORDER_COLS} FROM orders WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn find_order_by_broker_id(pool: &SqlitePool, account_id: &str, broker_id: &str) -> Result<Option<OrderRow>> {
    Ok(sqlx::query_as::<_, OrderRow>(AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} FROM orders WHERE account_id = ? AND broker_order_id = ? LIMIT 1"
    )))
    .bind(account_id)
    .bind(broker_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn find_order_by_correlation(pool: &SqlitePool, account_id: &str, corr: &str) -> Result<Option<OrderRow>> {
    Ok(sqlx::query_as::<_, OrderRow>(AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} FROM orders WHERE account_id = ? AND correlation_id = ? LIMIT 1"
    )))
    .bind(account_id)
    .bind(corr)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_orders(pool: &SqlitePool, account: Option<&str>, date: Option<&str>) -> Result<Vec<OrderRow>> {
    Ok(sqlx::query_as::<_, OrderRow>(AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} FROM orders
         WHERE (? IS NULL OR account_id = ?) AND (? IS NULL OR trade_date = ?)
         ORDER BY created_at DESC LIMIT 1000"
    )))
    .bind(account)
    .bind(account)
    .bind(date)
    .bind(date)
    .fetch_all(pool)
    .await?)
}

/// Orders that can still change. `kind` limits to `paper` or `dhan`.
pub async fn open_orders(pool: &SqlitePool, kind: Option<&str>) -> Result<Vec<OrderRow>> {
    Ok(sqlx::query_as::<_, OrderRow>(AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} FROM orders WHERE {OPEN_SQL} AND (? IS NULL OR account_kind = ?) ORDER BY created_at"
    )))
    .bind(kind)
    .bind(kind)
    .fetch_all(pool)
    .await?)
}

// ---------------------------------------------------------------- trades ---

/// Signed position in units with its average price.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Lot {
    pub qty: i64,
    pub avg: f64,
}

pub fn sign(side: &str) -> i64 {
    if side.eq_ignore_ascii_case("SELL") {
        -1
    } else {
        1
    }
}

/// Apply a fill (`dir` = +1 buy, -1 sell) to a position using average cost.
/// Returns the new position and the P&L realised by the closing part.
pub fn apply_fill(pos: Lot, dir: i64, qty: i64, price: f64) -> (Lot, f64) {
    if qty <= 0 {
        return (pos, 0.0);
    }
    let q = dir * qty;
    if pos.qty == 0 || pos.qty.signum() == q.signum() {
        let new_qty = pos.qty + q;
        let avg = (pos.qty.abs() as f64 * pos.avg + qty as f64 * price) / new_qty.abs() as f64;
        return (Lot { qty: new_qty, avg }, 0.0);
    }
    let closing = pos.qty.abs().min(qty);
    let realised = closing as f64 * (price - pos.avg) * pos.qty.signum() as f64;
    let new_qty = pos.qty + q;
    let avg = if new_qty == 0 {
        0.0
    } else if new_qty.signum() != pos.qty.signum() {
        price
    } else {
        pos.avg
    };
    (Lot { qty: new_qty, avg }, realised)
}

pub struct Fill<'a> {
    pub order: &'a OrderRow,
    pub qty: i64,
    pub price: f64,
    /// Brokerage to charge if this is the order's first fill.
    pub brokerage: f64,
    /// Exchange/broker trade id; a repeat is ignored.
    pub ext_id: Option<String>,
}

/// Book one fill: compute realised P&L against the existing position and the
/// charges, then insert the trade. Returns `None` for a duplicate `ext_id`.
/// Callers must serialise calls (see `TradingService::ledger`).
pub async fn record_fill(pool: &SqlitePool, cfg: &ChargesConfig, f: Fill<'_>) -> Result<Option<TradeRow>> {
    let o = f.order;
    if let Some(ext) = &f.ext_id {
        let dup: Option<(String,)> = sqlx::query_as("SELECT id FROM trades WHERE account_id = ? AND ext_id = ?")
            .bind(&o.account_id)
            .bind(ext)
            .fetch_optional(pool)
            .await?;
        if dup.is_some() {
            return Ok(None);
        }
    }

    let prior: Vec<(String, i64, f64)> = sqlx::query_as(
        "SELECT side, qty, price FROM trades WHERE account_id = ? AND instrument = ? AND product = ? ORDER BY ts, rowid",
    )
    .bind(&o.account_id)
    .bind(&o.instrument)
    .bind(&o.product)
    .fetch_all(pool)
    .await?;
    let mut pos = Lot::default();
    for (side, qty, price) in prior {
        pos = apply_fill(pos, sign(&side), qty, price).0;
    }
    let (_, realised) = apply_fill(pos, sign(&o.side), f.qty, f.price);

    let (n_prior,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM trades WHERE order_id = ?")
        .bind(&o.id)
        .fetch_one(pool)
        .await?;
    let brokerage = if n_prior == 0 { f.brokerage } else { 0.0 };
    let ch = charges::compute(cfg, brokerage, o.side.eq_ignore_ascii_case("SELL"), f.price * f.qty as f64);

    let now = Utc::now();
    let row = TradeRow {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: o.account_id.clone(),
        order_id: o.id.clone(),
        instrument: o.instrument.clone(),
        symbol: o.symbol.clone(),
        expiry: o.expiry.clone(),
        strike: o.strike,
        is_call: o.is_call,
        lot_size: o.lot_size,
        side: o.side.clone(),
        product: o.product.clone(),
        qty: f.qty,
        price: f.price,
        charges: ch.total,
        charge_detail: json!(ch).to_string(),
        realised,
        ts: now_ts(),
        // An expiry settlement belongs to the expiry day, even when it is
        // booked later (app closed over expiry, or the bars were flushed late).
        trade_date: if o.role == super::ROLE_SETTLE { o.trade_date.clone() } else { ist_date(now) },
        ext_id: f.ext_id,
    };
    let marks = vec!["?"; 19].join(", ");
    sqlx::query(AssertSqlSafe(format!("INSERT INTO trades ({TRADE_COLS}) VALUES ({marks})")))
        .bind(&row.id)
        .bind(&row.account_id)
        .bind(&row.order_id)
        .bind(&row.instrument)
        .bind(&row.symbol)
        .bind(&row.expiry)
        .bind(row.strike)
        .bind(row.is_call)
        .bind(row.lot_size)
        .bind(&row.side)
        .bind(&row.product)
        .bind(row.qty)
        .bind(row.price)
        .bind(row.charges)
        .bind(&row.charge_detail)
        .bind(row.realised)
        .bind(&row.ts)
        .bind(&row.trade_date)
        .bind(&row.ext_id)
        .execute(pool)
        .await?;
    Ok(Some(row))
}

pub async fn list_trades(pool: &SqlitePool, account: Option<&str>, date: Option<&str>) -> Result<Vec<TradeRow>> {
    Ok(sqlx::query_as::<_, TradeRow>(AssertSqlSafe(format!(
        "SELECT {TRADE_COLS} FROM trades
         WHERE (? IS NULL OR account_id = ?) AND (? IS NULL OR trade_date = ?)
         ORDER BY ts DESC, rowid DESC LIMIT 2000"
    )))
    .bind(account)
    .bind(account)
    .bind(date)
    .bind(date)
    .fetch_all(pool)
    .await?)
}

// ------------------------------------------------------------- positions ---

#[derive(Debug, Clone, sqlx::FromRow)]
struct PosTrade {
    account_id: String,
    instrument: String,
    symbol: String,
    expiry: String,
    strike: f64,
    is_call: bool,
    lot_size: i64,
    side: String,
    product: String,
    qty: i64,
    price: f64,
    trade_date: String,
}

#[derive(Debug, Clone)]
pub struct OpenPos {
    pub account_id: String,
    pub instrument: String,
    pub symbol: String,
    pub expiry: String,
    pub strike: f64,
    pub is_call: bool,
    pub lot_size: i64,
    pub product: String,
    pub net_qty: i64,
    pub avg_price: f64,
    /// IST date the position was last opened from flat.
    pub opened_on: String,
}

const POS_COLS: &str =
    "account_id, instrument, symbol, expiry, strike, is_call, lot_size, side, product, qty, price, trade_date";

/// Fold a time-ordered trade list into the open (non-zero) positions.
fn fold_positions(trades: Vec<PosTrade>) -> Vec<OpenPos> {
    let mut map: BTreeMap<(String, String, String), (PosTrade, Lot, String)> = BTreeMap::new();
    for t in trades {
        let key = (t.account_id.clone(), t.instrument.clone(), t.product.clone());
        let entry = map.entry(key).or_insert_with(|| (t.clone(), Lot::default(), t.trade_date.clone()));
        if entry.1.qty == 0 {
            entry.2 = t.trade_date.clone();
        }
        entry.1 = apply_fill(entry.1, sign(&t.side), t.qty, t.price).0;
        entry.0 = t;
    }
    map.into_values()
        .filter(|(_, lot, _)| lot.qty != 0)
        .map(|(t, lot, opened_on)| OpenPos {
            account_id: t.account_id,
            instrument: t.instrument,
            symbol: t.symbol,
            expiry: t.expiry,
            strike: t.strike,
            is_call: t.is_call,
            lot_size: t.lot_size,
            product: t.product,
            net_qty: lot.qty,
            avg_price: lot.avg,
            opened_on,
        })
        .collect()
}

/// Open (non-zero) positions in contracts that have not expired yet.
pub async fn open_positions(pool: &SqlitePool, account: Option<&str>, today: &str) -> Result<Vec<OpenPos>> {
    let trades: Vec<PosTrade> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT {POS_COLS} FROM trades
         WHERE expiry >= ? AND (? IS NULL OR account_id = ?)
         ORDER BY account_id, instrument, product, ts, rowid"
    )))
    .bind(today)
    .bind(account)
    .bind(account)
    .fetch_all(pool)
    .await?;
    Ok(fold_positions(trades))
}

/// Positions still open in contracts that expire before `before` (an ISO
/// date): they are waiting to be settled.
pub async fn expired_positions(pool: &SqlitePool, before: &str) -> Result<Vec<OpenPos>> {
    let trades: Vec<PosTrade> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT {POS_COLS} FROM trades
         WHERE expiry < ?
         ORDER BY account_id, instrument, product, ts, rowid"
    )))
    .bind(before)
    .fetch_all(pool)
    .await?;
    Ok(fold_positions(trades))
}

/// All-time (realised P&L, charges) of an account.
pub async fn account_pnl(pool: &SqlitePool, account: &str) -> Result<(f64, f64)> {
    let (realised, charges): (f64, f64) = sqlx::query_as(
        "SELECT COALESCE(SUM(realised), 0.0), COALESCE(SUM(charges), 0.0) FROM trades WHERE account_id = ?",
    )
    .bind(account)
    .fetch_one(pool)
    .await?;
    Ok((realised, charges))
}

/// Per account: (realised, charges, trade count) for a trade date.
pub async fn day_totals(pool: &SqlitePool, date: &str) -> Result<HashMap<String, (f64, f64, i64)>> {
    let rows: Vec<(String, f64, f64, i64)> = sqlx::query_as(
        "SELECT account_id, COALESCE(SUM(realised), 0.0), COALESCE(SUM(charges), 0.0), COUNT(*)
         FROM trades WHERE trade_date = ? GROUP BY account_id",
    )
    .bind(date)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(a, r, c, n)| (a, (r, c, n))).collect())
}

pub async fn order_counts(pool: &SqlitePool, date: &str) -> Result<HashMap<String, i64>> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT account_id, COUNT(*) FROM orders WHERE trade_date = ? GROUP BY account_id")
            .bind(date)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Settlement price of an expired option: the close of the contract's own
/// last 1-minute bar on its expiry day (IST). A stored daily bar of that day
/// is used only when no 1-minute bar exists. `None` when nothing was recorded.
pub async fn expiry_close(pool: &SqlitePool, instrument: &str, expiry_iso: &str) -> Option<f64> {
    const IST: i64 = 5 * 3600 + 30 * 60;
    let day = chrono::NaiveDate::parse_from_str(expiry_iso, "%Y-%m-%d").ok()?;
    let start = day.and_hms_opt(0, 0, 0)?.and_utc().timestamp() - IST;
    let end = start + 86_400;
    for tf in ["1m", "1d"] {
        let row = sqlx::query_as::<_, (f64,)>(
            "SELECT close FROM bars WHERE instrument = ? AND tf = ? AND ts >= ? AND ts < ? ORDER BY ts DESC LIMIT 1",
        )
        .bind(instrument)
        .bind(tf)
        .bind(start)
        .bind(end)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
        if let Some((close,)) = row {
            if close.is_finite() && close > 0.0 {
                return Some(close);
            }
        }
    }
    None
}

/// Most recent stored bar close for an instrument (used when no live price).
pub async fn last_close(pool: &SqlitePool, instrument: &str) -> Option<f64> {
    sqlx::query_as::<_, (f64,)>("SELECT close FROM bars WHERE instrument = ? ORDER BY ts DESC LIMIT 1")
        .bind(instrument)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.0)
}

// ------------------------------------------------------------ dashboard ---

/// Every fill up to `to` of the (account, contract, product) positions that
/// traded between `from` and `to` (inclusive IST dates), oldest first. The
/// earlier fills are needed to know whether a fill opened or closed a trade.
pub async fn trades_for_window(pool: &SqlitePool, account: Option<&str>, from: &str, to: &str) -> Result<Vec<TradeRow>> {
    Ok(sqlx::query_as::<_, TradeRow>(AssertSqlSafe(format!(
        "SELECT {TRADE_COLS} FROM trades
         WHERE (? IS NULL OR account_id = ?) AND trade_date <= ?
           AND (account_id, instrument, product) IN (
                SELECT account_id, instrument, product FROM trades
                WHERE (? IS NULL OR account_id = ?) AND trade_date >= ? AND trade_date <= ?)
         ORDER BY ts, rowid"
    )))
    .bind(account)
    .bind(account)
    .bind(to)
    .bind(account)
    .bind(account)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?)
}

/// (trade date, created at) of every order between two IST dates.
pub async fn order_times(pool: &SqlitePool, account: Option<&str>, from: &str, to: &str) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT trade_date, created_at FROM orders
         WHERE (? IS NULL OR account_id = ?) AND trade_date >= ? AND trade_date <= ?",
    )
    .bind(account)
    .bind(account)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lot(qty: i64, avg: f64) -> Lot {
        Lot { qty, avg }
    }

    #[test]
    fn long_position_averages_and_realises() {
        let (p, r) = apply_fill(Lot::default(), 1, 100, 50.0);
        assert_eq!((p, r), (lot(100, 50.0), 0.0));
        let (p, r) = apply_fill(p, 1, 100, 60.0);
        assert_eq!(p, lot(200, 55.0));
        assert_eq!(r, 0.0);
        let (p, r) = apply_fill(p, -1, 50, 65.0);
        assert_eq!(p, lot(150, 55.0));
        assert!((r - 500.0).abs() < 1e-9);
        let (p, r) = apply_fill(p, -1, 150, 50.0);
        assert_eq!(p.qty, 0);
        assert!((r - (-750.0)).abs() < 1e-9);
    }

    #[test]
    fn short_position_realises_when_bought_back_lower() {
        let (p, _) = apply_fill(Lot::default(), -1, 75, 100.0);
        assert_eq!(p, lot(-75, 100.0));
        let (p, r) = apply_fill(p, 1, 75, 80.0);
        assert_eq!(p.qty, 0);
        assert!((r - 1500.0).abs() < 1e-9);
    }

    #[test]
    fn flipping_sides_resets_average_to_the_fill_price() {
        let (p, r) = apply_fill(lot(50, 100.0), -1, 80, 110.0);
        assert!((r - 500.0).abs() < 1e-9); // 50 closed at +10
        assert_eq!(p, lot(-30, 110.0));
    }

    #[test]
    fn zero_quantity_is_ignored() {
        assert_eq!(apply_fill(lot(10, 5.0), 1, 0, 9.0), (lot(10, 5.0), 0.0));
    }

    #[tokio::test]
    async fn expiry_close_is_the_last_bar_of_the_expiry_day() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::charts::store::init(&pool).await.unwrap();
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        let start = day.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() - 5 * 3600 - 30 * 60;
        let put = |inst: &'static str, tf: &'static str, ts: i64, close: f64| {
            let pool = pool.clone();
            async move {
                sqlx::query("INSERT INTO bars (instrument, tf, ts, open, high, low, close) VALUES (?, ?, ?, 1, 1, 1, ?)")
                    .bind(inst)
                    .bind(tf)
                    .bind(ts)
                    .bind(close)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        let a = "OPT:NIFTY:2026-11-24:24000:CE";
        put(a, "1m", start - 600, 99.0).await; // the day before: ignored
        put(a, "1m", start + 9 * 3600 + 15 * 60, 40.0).await;
        put(a, "1m", start + 15 * 3600 + 29 * 60, 12.5).await; // last bar of the day
        put(a, "1m", start + 86_400 + 3600, 77.0).await; // the day after: ignored
        assert_eq!(expiry_close(&pool, a, "2026-11-24").await, Some(12.5));

        let b = "OPT:NIFTY:2026-11-24:24500:PE";
        put(b, "1d", start, 3.0).await; // only a daily bar: used as the fallback
        assert_eq!(expiry_close(&pool, b, "2026-11-24").await, Some(3.0));

        assert_eq!(expiry_close(&pool, "OPT:NIFTY:2026-11-24:25000:PE", "2026-11-24").await, None);
        assert_eq!(expiry_close(&pool, a, "not-a-date").await, None);
    }

    #[tokio::test]
    async fn expired_positions_wait_for_settlement_and_leave_once_closed() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        init(&pool).await.unwrap();
        let cfg = ChargesConfig::default();
        let mk = |id: &str, side: &str, role: &str, trade_date: &str| OrderRow {
            id: id.into(),
            account_id: "acc".into(),
            account_kind: "paper".into(),
            correlation_id: id.into(),
            broker_order_id: None,
            instrument: "OPT:NIFTY:2099-01-29:24000:CE".into(),
            symbol: "NIFTY".into(),
            expiry: "2099-01-29".into(),
            strike: 24000.0,
            is_call: true,
            security_id: None,
            lot_size: 65,
            side: side.into(),
            product: "MARGIN".into(),
            order_type: "LIMIT".into(),
            validity: "DAY".into(),
            qty: 65,
            price: 100.0,
            trigger_price: 0.0,
            status: "TRADED".into(),
            filled_qty: 65,
            avg_price: 100.0,
            message: String::new(),
            created_at: now_ts(),
            updated_at: now_ts(),
            trade_date: trade_date.into(),
            group_id: String::new(),
            role: role.into(),
            bracket: String::new(),
        };
        let buy = mk("b1", "BUY", "", &ist_date(Utc::now()));
        insert_order(&pool, &buy).await.unwrap();
        record_fill(&pool, &cfg, Fill { order: &buy, qty: 65, price: 100.0, brokerage: 20.0, ext_id: Some("b1:65".into()) })
            .await
            .unwrap();

        // Not due before the expiry date has passed.
        assert!(expired_positions(&pool, "2099-01-29").await.unwrap().is_empty());
        let due = expired_positions(&pool, "2099-01-30").await.unwrap();
        assert_eq!((due.len(), due[0].net_qty), (1, 65));

        // The settlement closes it and is dated the expiry day, not today.
        let settle = mk("s1", "SELL", super::super::ROLE_SETTLE, "2099-01-29");
        insert_order(&pool, &settle).await.unwrap();
        record_fill(&pool, &cfg, Fill { order: &settle, qty: 65, price: 12.5, brokerage: 0.0, ext_id: Some("s1:65".into()) })
            .await
            .unwrap();
        assert!(expired_positions(&pool, "2099-01-30").await.unwrap().is_empty());
        let (day, realised): (String, f64) =
            sqlx::query_as("SELECT trade_date, realised FROM trades WHERE order_id = 's1'").fetch_one(&pool).await.unwrap();
        assert_eq!(day, "2099-01-29");
        assert!((realised - (12.5 - 100.0) * 65.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn records_fills_and_derives_positions_and_charges() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        init(&pool).await.unwrap();
        let cfg = ChargesConfig::default();

        let mut order = OrderRow {
            id: "o1".into(),
            account_id: "acc".into(),
            account_kind: "paper".into(),
            correlation_id: "c1".into(),
            broker_order_id: None,
            instrument: "OPT:NIFTY:2099-01-29:24000:CE".into(),
            symbol: "NIFTY".into(),
            expiry: "2099-01-29".into(),
            strike: 24000.0,
            is_call: true,
            security_id: None,
            lot_size: 65,
            side: "BUY".into(),
            product: "INTRADAY".into(),
            order_type: "LIMIT".into(),
            validity: "DAY".into(),
            qty: 65,
            price: 100.0,
            trigger_price: 0.0,
            status: "PENDING".into(),
            filled_qty: 0,
            avg_price: 0.0,
            message: String::new(),
            created_at: now_ts(),
            updated_at: now_ts(),
            trade_date: ist_date(Utc::now()),
            group_id: String::new(),
            role: String::new(),
            bracket: String::new(),
        };
        insert_order(&pool, &order).await.unwrap();

        let buy = record_fill(&pool, &cfg, Fill { order: &order, qty: 65, price: 100.0, brokerage: 20.0, ext_id: Some("e1".into()) })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(buy.realised, 0.0);
        assert!(buy.charges > 20.0);
        // Same exchange id again: ignored.
        let dup = record_fill(&pool, &cfg, Fill { order: &order, qty: 65, price: 100.0, brokerage: 20.0, ext_id: Some("e1".into()) })
            .await
            .unwrap();
        assert!(dup.is_none());

        let open = open_positions(&pool, Some("acc"), "2026-01-01").await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!((open[0].net_qty, open[0].avg_price), (65, 100.0));
        assert_eq!(open[0].opened_on, ist_date(Utc::now()));

        order.id = "o2".into();
        order.correlation_id = "c2".into();
        order.side = "SELL".into();
        insert_order(&pool, &order).await.unwrap();
        let sell = record_fill(&pool, &cfg, Fill { order: &order, qty: 65, price: 110.0, brokerage: 20.0, ext_id: None })
            .await
            .unwrap()
            .unwrap();
        assert!((sell.realised - 650.0).abs() < 1e-9);
        assert!(sell.charges > buy.charges); // STT applies on the sell side
        assert!(open_positions(&pool, Some("acc"), "2026-01-01").await.unwrap().is_empty());

        let (realised_all, charges_all) = account_pnl(&pool, "acc").await.unwrap();
        assert!((realised_all - 650.0).abs() < 1e-9);
        assert!((charges_all - (buy.charges + sell.charges)).abs() < 0.011);

        let today = ist_date(Utc::now());
        let totals = day_totals(&pool, &today).await.unwrap();
        let (realised, charges, n) = totals["acc"];
        assert!((realised - 650.0).abs() < 1e-9);
        assert!((charges - (buy.charges + sell.charges)).abs() < 0.011);
        assert_eq!(n, 2);

        let fills = trades_for_window(&pool, None, &today, &today).await.unwrap();
        assert_eq!(fills.len(), 2);
        assert_eq!(order_times(&pool, None, &today, &today).await.unwrap().len(), 2);
    }
}