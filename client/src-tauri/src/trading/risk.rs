//! Risk controls: per-account limits, halts and the order check.
//!
//! * `RiskLimits`: max lots per order, max open positions, max daily loss,
//!   an allowed trading window (IST) and whether a loss breach also squares
//!   off. Stored per account (`risk_limits`).
//! * Halts (`risk_halts`): a daily-loss halt lasts until the end of the IST
//!   day; the global kill switch (account id `*`) lasts until released.
//! * `check` is pure. `TradingService::place` calls it (through
//!   `service::risk`), so manual, copier and algo orders are all covered.
//!
//! Orders that only reduce an existing position (Exit, Exit all, brackets,
//! square-off) are always allowed: a halt must never trap you in a position.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// `risk_halts.account_id` of the global kill switch.
pub const GLOBAL: &str = "*";
pub const HALT_KILL: &str = "kill";
pub const HALT_LOSS: &str = "daily_loss";

// ---------------------------------------------------------------- limits ---

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RiskLimits {
    /// Master switch for the limits below. The kill switch works regardless.
    pub enabled: bool,
    /// Largest order, in lots. 0 = no limit.
    pub max_lots_per_order: u32,
    /// Most positions open at once, counting resting entry orders. 0 = no limit.
    pub max_open_positions: u32,
    /// Halt the account for the day when today's net P&L (realised + open
    /// positions at the last price - charges) falls to minus this amount
    /// (rupees). 0 = no limit.
    pub max_daily_loss: f64,
    /// On a loss breach, also exit every position of the account.
    pub square_off_on_breach: bool,
    /// Only allow new positions inside `window_start`..`window_end` (IST).
    pub window_enabled: bool,
    pub window_start: String,
    pub window_end: String,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            enabled: false,
            max_lots_per_order: 0,
            max_open_positions: 0,
            max_daily_loss: 0.0,
            square_off_on_breach: false,
            window_enabled: false,
            window_start: "09:20".into(),
            window_end: "15:15".into(),
        }
    }
}

/// `HH:MM` as minutes since midnight.
fn parse_hhmm(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

fn hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

impl RiskLimits {
    /// (start, end) in minutes since midnight IST, when both parse.
    pub fn window_minutes(&self) -> Option<(u32, u32)> {
        Some((parse_hhmm(&self.window_start)?, parse_hhmm(&self.window_end)?))
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(self.max_daily_loss.is_finite() && self.max_daily_loss >= 0.0) {
            return Err("Max daily loss must be 0 (off) or a positive amount".into());
        }
        if self.window_enabled {
            match self.window_minutes() {
                None => return Err("Trading window times must be HH:MM (24-hour), e.g. 09:20".into()),
                Some((s, e)) if s >= e => return Err("Trading window must start before it ends".into()),
                Some(_) => {}
            }
        }
        Ok(())
    }
}

// ----------------------------------------------------------------- check ---

/// What the check needs to know about an order and the account.
pub struct OrderCheck<'a> {
    pub side: &'a str,
    pub lots: i64,
    /// Order quantity in units.
    pub qty: i64,
    /// Signed quantity already held in this contract and product.
    pub net_qty: i64,
    /// Distinct contract+product pairs the account holds or has resting
    /// entry orders for (this order's own pair included when it is one).
    pub open_keys: usize,
    /// This order's contract+product pair is one of `open_keys`.
    pub key_is_open: bool,
    /// IST time of day, minutes since midnight.
    pub minutes_now: u32,
    /// Today's net P&L of the account (0 when the loss limit is not checked).
    pub net_today: f64,
}

/// An order on the opposite side of a position and no larger than it.
pub fn is_reducing(side: &str, qty: i64, net_qty: i64) -> bool {
    let dir = if side.eq_ignore_ascii_case("SELL") { -1 } else { 1 };
    net_qty != 0 && net_qty.signum() != dir && qty <= net_qty.abs()
}

/// `Err(message)` when the order must be refused. `kill` = the global kill
/// switch is on; `halt` = the account's active halt reason, if any.
pub fn check(limits: &RiskLimits, kill: bool, halt: Option<&str>, o: &OrderCheck) -> Result<(), String> {
    if is_reducing(o.side, o.qty, o.net_qty) {
        return Ok(());
    }
    if kill {
        return Err("Risk: the kill switch is on. Only orders that reduce a position are allowed.".into());
    }
    if let Some(reason) = halt {
        return Err(format!("Risk: trading is halted for this account ({reason}). Only orders that reduce a position are allowed."));
    }
    if !limits.enabled {
        return Ok(());
    }
    if limits.max_lots_per_order > 0 && o.lots > limits.max_lots_per_order as i64 {
        return Err(format!(
            "Risk: {} lots exceeds the limit of {} lots per order.",
            o.lots, limits.max_lots_per_order
        ));
    }
    if limits.max_open_positions > 0 {
        let after = o.open_keys + if o.key_is_open { 0 } else { 1 };
        if after > limits.max_open_positions as usize {
            return Err(format!(
                "Risk: this would open position number {after}; the limit is {} open positions.",
                limits.max_open_positions
            ));
        }
    }
    if limits.window_enabled {
        if let Some((start, end)) = limits.window_minutes() {
            if o.minutes_now < start || o.minutes_now >= end {
                return Err(format!(
                    "Risk: outside the allowed trading window ({} to {} IST).",
                    hhmm(start),
                    hhmm(end)
                ));
            }
        }
    }
    if limits.max_daily_loss > 0.0 && o.net_today <= -limits.max_daily_loss {
        return Err(format!(
            "Risk: today's loss ({:.2}) has reached the daily limit of {:.2}.",
            o.net_today, limits.max_daily_loss
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------- halts ---

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HaltRow {
    /// Account id, or `*` for the kill switch.
    pub account_id: String,
    /// `kill` | `daily_loss`
    pub kind: String,
    /// IST date of a daily-loss halt; empty for the kill switch.
    pub trade_date: String,
    pub reason: String,
    pub at: String,
    /// A daily-loss halt the user lifted for today (it is not raised again).
    pub released: bool,
}

impl HaltRow {
    /// Still stopping new positions on IST date `today`.
    pub fn active(&self, today: &str) -> bool {
        !self.released && (self.kind == HALT_KILL || self.trade_date == today)
    }

    pub fn info(&self) -> HaltInfo {
        HaltInfo { kind: self.kind.clone(), reason: self.reason.clone(), since: self.at.clone() }
    }
}

// ----------------------------------------------------------------- views ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HaltInfo {
    pub kind: String,
    pub reason: String,
    pub since: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRisk {
    pub account_id: String,
    pub name: String,
    pub kind: String,
    pub limits: RiskLimits,
    pub halt: Option<HaltInfo>,
    /// Today's net P&L (realised + unrealised - charges).
    pub net_today: f64,
    pub open_positions: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskOverview {
    pub kill_switch: Option<HaltInfo>,
    pub accounts: Vec<AccountRisk>,
}

/// Outcome of switching the kill switch on.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KillResult {
    /// Resting orders cancelled (entries, and the contracts' orders when flattening).
    pub cancelled: usize,
    /// Exit orders placed (only when asked to flatten).
    pub placed: usize,
    pub skipped: Vec<String>,
}

// ----------------------------------------------------------------- store ---

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS risk_limits (
            account_id TEXT PRIMARY KEY,
            config TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS risk_halts (
            account_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            trade_date TEXT NOT NULL DEFAULT '',
            reason TEXT NOT NULL DEFAULT '',
            at TEXT NOT NULL,
            released INTEGER NOT NULL DEFAULT 0
        );"#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The account's limits; all off when none were saved.
pub async fn get_limits(pool: &SqlitePool, account_id: &str) -> Result<RiskLimits> {
    let row: Option<(String,)> = sqlx::query_as("SELECT config FROM risk_limits WHERE account_id = ?")
        .bind(account_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|(json,)| serde_json::from_str(&json).ok()).unwrap_or_default())
}

pub async fn save_limits(pool: &SqlitePool, account_id: &str, limits: &RiskLimits) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO risk_limits (account_id, config, updated_at) VALUES (?, ?, ?)
           ON CONFLICT(account_id) DO UPDATE SET config = excluded.config, updated_at = excluded.updated_at"#,
    )
    .bind(account_id)
    .bind(serde_json::to_string(limits)?)
    .bind(super::now_ts())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_halt(pool: &SqlitePool, account_id: &str) -> Result<Option<HaltRow>> {
    Ok(sqlx::query_as::<_, HaltRow>(
        "SELECT account_id, kind, trade_date, reason, at, released FROM risk_halts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn set_halt(pool: &SqlitePool, h: &HaltRow) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO risk_halts (account_id, kind, trade_date, reason, at, released) VALUES (?, ?, ?, ?, ?, ?)
           ON CONFLICT(account_id) DO UPDATE SET kind = excluded.kind, trade_date = excluded.trade_date,
             reason = excluded.reason, at = excluded.at, released = excluded.released"#,
    )
    .bind(&h.account_id)
    .bind(&h.kind)
    .bind(&h.trade_date)
    .bind(&h.reason)
    .bind(&h.at)
    .bind(h.released)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_halt(pool: &SqlitePool, account_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM risk_halts WHERE account_id = ?").bind(account_id).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> RiskLimits {
        RiskLimits {
            enabled: true,
            max_lots_per_order: 5,
            max_open_positions: 2,
            max_daily_loss: 1000.0,
            window_enabled: true,
            ..RiskLimits::default()
        }
    }

    /// A 1-lot BUY into a new contract at 10:00, nothing held, no loss.
    fn base() -> OrderCheck<'static> {
        OrderCheck {
            side: "BUY",
            lots: 1,
            qty: 65,
            net_qty: 0,
            open_keys: 0,
            key_is_open: false,
            minutes_now: 10 * 60,
            net_today: 0.0,
        }
    }

    #[test]
    fn reducing_orders_are_recognised() {
        assert!(is_reducing("SELL", 65, 65)); // exact exit
        assert!(is_reducing("sell", 30, 65)); // partial exit
        assert!(is_reducing("BUY", 65, -65)); // cover a short
        assert!(!is_reducing("SELL", 130, 65)); // would flip the position
        assert!(!is_reducing("BUY", 65, 65)); // adds to it
        assert!(!is_reducing("SELL", 65, 0)); // nothing to reduce
    }

    #[test]
    fn a_normal_order_passes_and_limits_off_means_no_checks() {
        assert!(check(&on(), false, None, &base()).is_ok());
        let big = OrderCheck { lots: 500, ..base() };
        assert!(check(&RiskLimits::default(), false, None, &big).is_ok());
    }

    #[test]
    fn max_lots_per_order() {
        assert!(check(&on(), false, None, &OrderCheck { lots: 5, ..base() }).is_ok());
        let err = check(&on(), false, None, &OrderCheck { lots: 6, ..base() }).unwrap_err();
        assert!(err.contains("6 lots") && err.contains("5 lots"));
    }

    #[test]
    fn max_open_positions_counts_new_contracts_only() {
        // Two already open: a third contract is refused ...
        let third = OrderCheck { open_keys: 2, key_is_open: false, ..base() };
        assert!(check(&on(), false, None, &third).is_err());
        // ... but adding to one of the open ones is fine.
        let add = OrderCheck { open_keys: 2, key_is_open: true, net_qty: 65, ..base() };
        assert!(check(&on(), false, None, &add).is_ok());
        // A second contract is still allowed.
        let second = OrderCheck { open_keys: 1, key_is_open: false, ..base() };
        assert!(check(&on(), false, None, &second).is_ok());
    }

    #[test]
    fn trading_window_is_start_inclusive_end_exclusive() {
        let mut l = on();
        l.window_start = "09:20".into();
        l.window_end = "15:15".into();
        let at = |m: u32| OrderCheck { minutes_now: m, ..base() };
        assert!(check(&l, false, None, &at(9 * 60 + 19)).is_err());
        assert!(check(&l, false, None, &at(9 * 60 + 20)).is_ok());
        assert!(check(&l, false, None, &at(15 * 60 + 14)).is_ok());
        let err = check(&l, false, None, &at(15 * 60 + 15)).unwrap_err();
        assert!(err.contains("09:20") && err.contains("15:15"));
        l.window_enabled = false;
        assert!(check(&l, false, None, &at(3 * 60)).is_ok());
    }

    #[test]
    fn daily_loss_blocks_new_positions_at_the_limit() {
        assert!(check(&on(), false, None, &OrderCheck { net_today: -999.0, ..base() }).is_ok());
        assert!(check(&on(), false, None, &OrderCheck { net_today: -1000.0, ..base() }).is_err());
    }

    #[test]
    fn kill_switch_and_halt_block_everything_but_reducing_orders() {
        let l = RiskLimits::default(); // limits off: kill and halt still apply
        assert!(check(&l, true, None, &base()).unwrap_err().contains("kill switch"));
        assert!(check(&l, false, Some("daily loss"), &base()).unwrap_err().contains("halted"));
        let exit = OrderCheck { side: "SELL", net_qty: 65, ..base() };
        assert!(check(&l, true, Some("daily loss"), &exit).is_ok());
    }

    #[test]
    fn limits_are_validated() {
        assert!(RiskLimits::default().validate().is_ok());
        let mut l = on();
        assert!(l.validate().is_ok());
        l.max_daily_loss = -1.0;
        assert!(l.validate().is_err());
        l.max_daily_loss = f64::NAN;
        assert!(l.validate().is_err());
        let mut l = on();
        l.window_start = "9.20".into();
        assert!(l.validate().is_err());
        let mut l = on();
        l.window_start = "15:15".into();
        l.window_end = "09:20".into();
        assert!(l.validate().is_err());
        // An unused window is not validated.
        l.window_enabled = false;
        assert!(l.validate().is_ok());
    }

    #[test]
    fn halts_expire_with_the_day_but_the_kill_switch_does_not() {
        let mk = |kind: &str, date: &str, released: bool| HaltRow {
            account_id: "a".into(),
            kind: kind.into(),
            trade_date: date.into(),
            reason: "r".into(),
            at: "t".into(),
            released,
        };
        assert!(mk(HALT_LOSS, "2026-11-08", false).active("2026-11-08"));
        assert!(!mk(HALT_LOSS, "2026-11-08", false).active("2026-11-09"));
        assert!(!mk(HALT_LOSS, "2026-11-08", true).active("2026-11-08"));
        assert!(mk(HALT_KILL, "", false).active("2030-01-01"));
    }

    #[tokio::test]
    async fn limits_and_halts_are_stored() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        init(&pool).await.unwrap();

        assert_eq!(get_limits(&pool, "a").await.unwrap(), RiskLimits::default());
        save_limits(&pool, "a", &on()).await.unwrap();
        assert_eq!(get_limits(&pool, "a").await.unwrap(), on());

        assert!(get_halt(&pool, GLOBAL).await.unwrap().is_none());
        let mut h = HaltRow {
            account_id: GLOBAL.into(),
            kind: HALT_KILL.into(),
            trade_date: String::new(),
            reason: "kill".into(),
            at: "t".into(),
            released: false,
        };
        set_halt(&pool, &h).await.unwrap();
        assert!(get_halt(&pool, GLOBAL).await.unwrap().unwrap().active("2026-11-08"));
        h.released = true;
        set_halt(&pool, &h).await.unwrap();
        assert!(!get_halt(&pool, GLOBAL).await.unwrap().unwrap().active("2026-11-08"));
        clear_halt(&pool, GLOBAL).await.unwrap();
        assert!(get_halt(&pool, GLOBAL).await.unwrap().is_none());
    }
}
