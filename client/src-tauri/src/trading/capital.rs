//! Capital ledger: what an account held when a period began, the denominator
//! of the Dashboard ROI.
//!
//! A broker account has no capital the app can read, so the user records it:
//!
//! * **opening**: "the balance at the start of this day" (a checkpoint; it
//!   already includes anything deposited that morning),
//! * **deposit** / **withdrawal**: money moved after a checkpoint.
//!
//! Opening balance of a period starting on date `D` =
//! the latest checkpoint dated on or before `D`
//! + deposits - withdrawals dated after the checkpoint, up to and including `D`
//! + net P&L (realised - charges) of the fills from the checkpoint day up to the day before `D`.
//!
//! Paper accounts need no entries: their checkpoint is `startingCapital` from
//! the beginning, exactly what the paper funds model uses. Without a
//! checkpoint the balance is unknown and the ROI is not shown.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;

use super::now_ts;
use super::period::Opening;
use super::pricing::r2;
use crate::accounts::{self, Account};

pub const OPENING: &str = "opening";
pub const DEPOSIT: &str = "deposit";
pub const WITHDRAWAL: &str = "withdrawal";

const MAX_AMOUNT: f64 = 1e12;
const MAX_NOTE: usize = 200;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub account_id: String,
    /// IST date `YYYY-MM-DD`.
    pub date: String,
    /// `opening` | `deposit` | `withdrawal`
    pub kind: String,
    /// Always positive; the kind gives the direction.
    pub amount: f64,
    pub note: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewEvent {
    pub account_id: String,
    pub date: String,
    pub kind: String,
    pub amount: f64,
    #[serde(default)]
    pub note: String,
}

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS capital_events (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL,
            date TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('opening','deposit','withdrawal')),
            amount REAL NOT NULL CHECK (amount > 0),
            note TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        );"#,
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_capital_acct_date ON capital_events(account_id, date);")
        .execute(pool)
        .await?;
    Ok(())
}

// ------------------------------------------------------------- pure logic ---

fn signed(e: &Event) -> f64 {
    match e.kind.as_str() {
        WITHDRAWAL => -e.amount,
        _ => e.amount,
    }
}

/// Where an account's balance can be counted from for a period starting on `start`.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    /// First IST date whose P&L is added on top (`""` = from the beginning).
    pub from: String,
    /// Checkpoint balance plus the deposits less withdrawals after it, up to and including `start`.
    pub amount: f64,
}

/// The checkpoint to count from. `base` is the account's own starting capital
/// (paper), used when no `opening` entry is dated on or before `start`.
/// `None` = the balance is not known. `events` belong to one account.
pub fn anchor(base: Option<f64>, events: &[Event], start: &str) -> Option<Anchor> {
    let (from, amount) = events
        .iter()
        .filter(|e| e.kind == OPENING && e.date.as_str() <= start)
        .max_by(|a, b| (a.date.as_str(), a.created_at.as_str()).cmp(&(b.date.as_str(), b.created_at.as_str())))
        .map(|e| (e.date.clone(), e.amount))
        .or_else(|| base.filter(|b| *b > 0.0).map(|b| (String::new(), b)))?;
    let flows: f64 = events
        .iter()
        .filter(|e| e.kind != OPENING && e.date.as_str() > from.as_str() && e.date.as_str() <= start)
        .map(signed)
        .sum();
    Some(Anchor { from, amount: amount + flows })
}

/// Validate an entry; returns the date in its canonical `YYYY-MM-DD` form.
pub fn validate(e: &NewEvent, today: &str) -> Result<String> {
    if ![OPENING, DEPOSIT, WITHDRAWAL].contains(&e.kind.as_str()) {
        bail!("Choose opening balance, deposit or withdrawal");
    }
    if !e.amount.is_finite() || e.amount <= 0.0 {
        bail!("Enter an amount greater than zero");
    }
    if e.amount > MAX_AMOUNT {
        bail!("That amount is too large");
    }
    let date = NaiveDate::parse_from_str(e.date.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow!("Enter the date as YYYY-MM-DD"))?
        .format("%Y-%m-%d")
        .to_string();
    if date.as_str() > today {
        bail!("The date cannot be in the future");
    }
    if e.note.chars().count() > MAX_NOTE {
        bail!("The note is too long (max {MAX_NOTE} characters)");
    }
    Ok(date)
}

/// The starting capital of a paper account.
pub fn base_capital(a: &Account) -> Option<f64> {
    if a.kind != "paper" {
        return None;
    }
    a.meta.get("startingCapital").and_then(Value::as_f64).filter(|c| *c > 0.0)
}

// ---------------------------------------------------------------- storage ---

const COLS: &str = "id, account_id, date, kind, amount, note, created_at";

/// Entries of one account (or all), newest date first.
pub async fn list(pool: &SqlitePool, account: Option<&str>) -> Result<Vec<Event>> {
    Ok(sqlx::query_as::<_, Event>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM capital_events WHERE (? IS NULL OR account_id = ?)
         ORDER BY date DESC, created_at DESC"
    )))
    .bind(account)
    .bind(account)
    .fetch_all(pool)
    .await?)
}

/// Add an entry. Paper accounts are refused (they use their starting capital),
/// and so is anything that does not trade.
pub async fn add(pool: &SqlitePool, input: NewEvent, today: &str) -> Result<Event> {
    let date = validate(&input, today)?;
    let acct = accounts::get(pool, &input.account_id).await?;
    match acct.kind.as_str() {
        "dhan" => {}
        "paper" => bail!("A paper account uses its starting capital; change it by editing the account"),
        _ => bail!("Capital can only be recorded for a trading account"),
    }
    let ev = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: acct.id,
        date,
        kind: input.kind,
        amount: input.amount,
        note: input.note.trim().to_string(),
        created_at: now_ts(),
    };
    sqlx::query("INSERT INTO capital_events (id, account_id, date, kind, amount, note, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&ev.id)
        .bind(&ev.account_id)
        .bind(&ev.date)
        .bind(&ev.kind)
        .bind(ev.amount)
        .bind(&ev.note)
        .bind(&ev.created_at)
        .execute(pool)
        .await?;
    Ok(ev)
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Event>> {
    Ok(sqlx::query_as::<_, Event>(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM capital_events WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM capital_events WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

/// Drop every entry of a deleted account.
pub async fn delete_for_account(pool: &SqlitePool, account_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM capital_events WHERE account_id = ?").bind(account_id).execute(pool).await?;
    Ok(())
}

/// Net P&L (realised - charges) of an account's fills dated `from` (inclusive,
/// `""` = from the beginning) up to the day before `to`.
async fn net_pnl_before(pool: &SqlitePool, account_id: &str, from: &str, to: &str) -> Result<f64> {
    let (realised, charges): (f64, f64) = sqlx::query_as(
        "SELECT COALESCE(SUM(realised), 0.0), COALESCE(SUM(charges), 0.0) FROM trades
         WHERE account_id = ? AND trade_date >= ? AND trade_date < ?",
    )
    .bind(account_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok(realised - charges)
}

/// What each given account held at the start of IST date `start`
/// (`balance` is `None` when it cannot be worked out).
pub async fn openings(pool: &SqlitePool, accounts: &[Account], start: &str) -> Result<Vec<Opening>> {
    let mut by_account: HashMap<String, Vec<Event>> = HashMap::new();
    for e in list(pool, None).await? {
        by_account.entry(e.account_id.clone()).or_default().push(e);
    }
    let mut out = Vec::with_capacity(accounts.len());
    for a in accounts {
        let events = by_account.get(&a.id).map(Vec::as_slice).unwrap_or(&[]);
        let balance = match anchor(base_capital(a), events, start) {
            Some(an) => Some(r2(an.amount + net_pnl_before(pool, &a.id, &an.from, start).await?)),
            None => None,
        };
        out.push(Opening { account_id: a.id.clone(), name: a.name.clone(), balance });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, date: &str, amount: f64, created: &str) -> Event {
        Event {
            id: format!("{kind}{date}{created}"),
            account_id: "a".into(),
            date: date.into(),
            kind: kind.into(),
            amount,
            note: String::new(),
            created_at: created.into(),
        }
    }

    #[test]
    fn unknown_without_a_checkpoint_or_base() {
        assert_eq!(anchor(None, &[], "2026-11-10"), None);
        // a deposit alone is not a balance
        assert_eq!(anchor(None, &[ev(DEPOSIT, "2026-11-01", 5000.0, "1")], "2026-11-10"), None);
        // a checkpoint dated after the period start does not help
        assert_eq!(anchor(None, &[ev(OPENING, "2026-11-12", 5000.0, "1")], "2026-11-10"), None);
    }

    #[test]
    fn paper_counts_from_the_beginning_with_its_starting_capital() {
        let a = anchor(Some(1_000_000.0), &[], "2026-11-10").unwrap();
        assert_eq!((a.from.as_str(), a.amount), ("", 1_000_000.0));
        assert_eq!(anchor(Some(0.0), &[], "2026-11-10"), None);
    }

    #[test]
    fn latest_checkpoint_wins_and_flows_after_it_are_added() {
        let events = [
            ev(OPENING, "2026-10-01", 100_000.0, "1"),
            ev(DEPOSIT, "2026-10-15", 20_000.0, "2"),
            ev(OPENING, "2026-11-01", 130_000.0, "3"), // re-baselined: replaces everything before it
            ev(WITHDRAWAL, "2026-11-05", 10_000.0, "4"),
            ev(DEPOSIT, "2026-11-10", 5_000.0, "5"), // dated the period start: counts
            ev(DEPOSIT, "2026-11-11", 99_000.0, "6"), // after the start: not part of the opening balance
        ];
        let a = anchor(None, &events, "2026-11-10").unwrap();
        assert_eq!((a.from.as_str(), a.amount), ("2026-11-01", 130_000.0 - 10_000.0 + 5_000.0));
        // an earlier period still sees the October checkpoint and its deposit
        let o = anchor(None, &events, "2026-10-20").unwrap();
        assert_eq!((o.from.as_str(), o.amount), ("2026-10-01", 120_000.0));
    }

    #[test]
    fn a_flow_on_the_checkpoint_day_is_already_in_the_checkpoint() {
        let events = [ev(OPENING, "2026-11-01", 50_000.0, "1"), ev(DEPOSIT, "2026-11-01", 7_000.0, "2")];
        assert_eq!(anchor(None, &events, "2026-11-03").unwrap().amount, 50_000.0);
    }

    #[test]
    fn two_checkpoints_on_one_day_use_the_later_entry() {
        let events = [ev(OPENING, "2026-11-01", 50_000.0, "1"), ev(OPENING, "2026-11-01", 52_000.0, "2")];
        assert_eq!(anchor(None, &events, "2026-11-03").unwrap().amount, 52_000.0);
    }

    #[test]
    fn validation_rules() {
        let ok = NewEvent { account_id: "a".into(), date: "2026-11-5".into(), kind: DEPOSIT.into(), amount: 100.0, note: String::new() };
        assert_eq!(validate(&ok, "2026-11-19").unwrap(), "2026-11-05");
        let bad = |f: &dyn Fn(&mut NewEvent)| {
            let mut e = ok.clone();
            f(&mut e);
            validate(&e, "2026-11-19").is_err()
        };
        assert!(bad(&|e| e.kind = "gift".into()));
        assert!(bad(&|e| e.amount = 0.0));
        assert!(bad(&|e| e.amount = -5.0));
        assert!(bad(&|e| e.amount = f64::NAN));
        assert!(bad(&|e| e.date = "yesterday".into()));
        assert!(bad(&|e| e.date = "2026-11-20".into())); // future
        assert!(bad(&|e| e.note = "x".repeat(MAX_NOTE + 1)));
    }
}
