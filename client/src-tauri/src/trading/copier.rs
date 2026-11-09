//! Trade copier: configuration, lot scaling and the master -> child link table.
//!
//! One master account, any number of child accounts. An order placed on the
//! master is replicated to every enabled child at the moment it is placed
//! (see `service::copier`). Each child has a factor (lot multiplier) and a
//! rounding rule. Every child order goes through `place`, so the server gate,
//! risk checks and broker rules apply to it like to any other order.
//!
//! * `CopierConfig` is JSON in `app_settings` (key `copier.config`).
//! * `copy_links` records what became of each master order for each child.
//! * `plan_lots` is pure (unit-tested): how many lots a child gets.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashSet;

use crate::db;

pub const SETTING_KEY: &str = "copier.config";
/// Same ceiling as `place` (lots per order).
pub const MAX_LOTS: i64 = 10_000;
pub const MAX_MULTIPLIER: f64 = 1000.0;

// ---------------------------------------------------------------- config ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rounding {
    #[default]
    Down,
    Nearest,
    Up,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChildConfig {
    pub account_id: String,
    pub enabled: bool,
    /// Child lots = master lots x multiplier, then rounded.
    pub multiplier: f64,
    pub rounding: Rounding,
    /// A result below 1 lot becomes 1 lot instead of being skipped.
    pub min_one_lot: bool,
}

impl Default for ChildConfig {
    fn default() -> Self {
        Self { account_id: String::new(), enabled: true, multiplier: 1.0, rounding: Rounding::Down, min_one_lot: false }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CopierConfig {
    /// Master switch. Off = nothing is copied (open copies still follow the master).
    pub enabled: bool,
    pub master_account_id: String,
    pub children: Vec<ChildConfig>,
}

impl CopierConfig {
    /// Structure checks that need no database. Account existence is checked by the service.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.master_account_id.trim().is_empty() {
            return Err("Choose the master account".into());
        }
        let mut seen = HashSet::new();
        for c in &self.children {
            if c.account_id.trim().is_empty() {
                return Err("Every child needs an account".into());
            }
            if c.account_id == self.master_account_id {
                return Err("The master account cannot also be a child".into());
            }
            if !seen.insert(c.account_id.as_str()) {
                return Err("An account can be a child only once".into());
            }
            if !(c.multiplier.is_finite() && c.multiplier > 0.0 && c.multiplier <= MAX_MULTIPLIER) {
                return Err(format!("Multiplier must be greater than 0 and at most {MAX_MULTIPLIER}"));
            }
        }
        Ok(())
    }

    /// Children that receive copies.
    pub fn active_children(&self) -> Vec<ChildConfig> {
        self.children.iter().filter(|c| c.enabled).cloned().collect()
    }
}

pub async fn load_config(pool: &SqlitePool) -> Result<CopierConfig> {
    Ok(db::get_setting(pool, SETTING_KEY)
        .await?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default())
}

pub async fn save_config(pool: &SqlitePool, cfg: &CopierConfig) -> Result<()> {
    db::set_setting(pool, SETTING_KEY, &serde_json::to_string(cfg)?).await
}

// ---------------------------------------------------------------- scaling ---

/// What the master order does to the master's own position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Opens or adds to a position.
    Open,
    /// Only reduces the master's position; `full` = closes all of it.
    Reduce { full: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Place(i64),
    Skip(String),
}

/// Master lots x multiplier, rounded; `None` when that is below 1 lot and the
/// child does not take a minimum of one.
pub fn scale_lots(master_lots: i64, c: &ChildConfig) -> Option<i64> {
    // Trim float noise so 100 x 0.29 is 29, not 28.999999999999996.
    let raw = ((master_lots as f64 * c.multiplier) * 1e6).round() / 1e6;
    let rounded = match c.rounding {
        Rounding::Down => raw.floor(),
        Rounding::Nearest => raw.round(),
        Rounding::Up => raw.ceil(),
    };
    let lots = if rounded.is_finite() { rounded as i64 } else { 0 };
    if lots >= 1 {
        Some(lots.min(MAX_LOTS))
    } else if c.min_one_lot {
        Some(1)
    } else {
        None
    }
}

/// Lots to place on a child. `child_closable_lots` is what the child holds in
/// the direction the order would reduce (0 when it holds nothing or the
/// opposite way).
///
/// Orders that open: the scaled quantity. Orders that reduce the master:
/// a full exit closes the whole child position (so rounding never leaves a
/// remainder behind); a partial reduction is scaled and capped at what the
/// child holds, so a copy never opens a position the master did not.
pub fn plan_lots(master_lots: i64, intent: Intent, c: &ChildConfig, child_closable_lots: i64) -> Plan {
    match intent {
        Intent::Open => match scale_lots(master_lots, c) {
            Some(l) => Plan::Place(l),
            None => Plan::Skip("Rounds to 0 lots with this factor".into()),
        },
        Intent::Reduce { full } => {
            if child_closable_lots <= 0 {
                return Plan::Skip("Child holds no position to reduce".into());
            }
            if full {
                return Plan::Place(child_closable_lots.min(MAX_LOTS));
            }
            match scale_lots(master_lots, c) {
                Some(l) => Plan::Place(l.min(child_closable_lots)),
                None => Plan::Skip("Rounds to 0 lots with this factor".into()),
            }
        }
    }
}

// ----------------------------------------------------------------- links ---

pub const LINK_PLACED: &str = "placed";
pub const LINK_SKIPPED: &str = "skipped";
pub const LINK_FAILED: &str = "failed";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LinkRow {
    pub id: String,
    pub master_order_id: String,
    pub child_account_id: String,
    pub child_order_id: Option<String>,
    pub master_lots: i64,
    pub child_lots: i64,
    pub status: String,
    pub message: String,
    pub created_at: String,
    pub trade_date: String,
}

/// A link joined with the orders on both sides, for the Copier page.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct CopyView {
    pub id: String,
    pub master_order_id: String,
    pub child_account_id: String,
    pub child_order_id: Option<String>,
    pub master_lots: i64,
    pub child_lots: i64,
    /// `placed` | `skipped` | `failed`
    pub status: String,
    pub message: String,
    pub created_at: String,
    pub trade_date: String,
    pub symbol: Option<String>,
    pub expiry: Option<String>,
    pub strike: Option<f64>,
    pub is_call: Option<bool>,
    pub side: Option<String>,
    pub product: Option<String>,
    pub price: Option<f64>,
    /// Current status of the child order (when one exists).
    pub child_status: Option<String>,
    pub child_filled_qty: Option<i64>,
    pub child_qty: Option<i64>,
    #[sqlx(skip)]
    pub child_account_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopierAccount {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// What the Copier page loads.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopierOverview {
    pub config: CopierConfig,
    /// Accounts that can place orders (paper and Dhan).
    pub accounts: Vec<CopierAccount>,
    pub links: Vec<CopyView>,
}

pub async fn init(pool: &SqlitePool) -> Result<()> {
    let stmts = [
        r#"CREATE TABLE IF NOT EXISTS copy_links (
            id TEXT PRIMARY KEY,
            master_order_id TEXT NOT NULL,
            master_account_id TEXT NOT NULL,
            child_account_id TEXT NOT NULL,
            child_order_id TEXT,
            master_lots INTEGER NOT NULL,
            child_lots INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL,
            message TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            trade_date TEXT NOT NULL
        );"#,
        "CREATE INDEX IF NOT EXISTS idx_copy_master ON copy_links(master_order_id);",
        "CREATE INDEX IF NOT EXISTS idx_copy_child ON copy_links(child_order_id) WHERE child_order_id IS NOT NULL;",
        "CREATE INDEX IF NOT EXISTS idx_copy_date ON copy_links(trade_date);",
    ];
    for s in stmts {
        sqlx::query(s).execute(pool).await?;
    }
    Ok(())
}

pub async fn insert_link(pool: &SqlitePool, l: &LinkRow, master_account_id: &str) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO copy_links (id, master_order_id, master_account_id, child_account_id, child_order_id,
             master_lots, child_lots, status, message, created_at, trade_date)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&l.id)
    .bind(&l.master_order_id)
    .bind(master_account_id)
    .bind(&l.child_account_id)
    .bind(&l.child_order_id)
    .bind(l.master_lots)
    .bind(l.child_lots)
    .bind(&l.status)
    .bind(&l.message)
    .bind(&l.created_at)
    .bind(&l.trade_date)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_link_message(pool: &SqlitePool, id: &str, message: &str) -> Result<()> {
    sqlx::query("UPDATE copy_links SET message = ? WHERE id = ?").bind(message).bind(id).execute(pool).await?;
    Ok(())
}

/// Copies that were placed (a child order exists) for a master order.
pub async fn placed_links(pool: &SqlitePool, master_order_id: &str) -> Result<Vec<LinkRow>> {
    Ok(sqlx::query_as::<_, LinkRow>(
        "SELECT id, master_order_id, child_account_id, child_order_id, master_lots, child_lots, status, message,
                created_at, trade_date
         FROM copy_links
         WHERE master_order_id = ? AND status = 'placed' AND child_order_id IS NOT NULL",
    )
    .bind(master_order_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_views(pool: &SqlitePool, limit: i64) -> Result<Vec<CopyView>> {
    Ok(sqlx::query_as::<_, CopyView>(
        "SELECT l.id, l.master_order_id, l.child_account_id, l.child_order_id, l.master_lots, l.child_lots,
                l.status, l.message, l.created_at, l.trade_date,
                m.symbol AS symbol, m.expiry AS expiry, m.strike AS strike, m.is_call AS is_call,
                m.side AS side, m.product AS product, m.price AS price,
                c.status AS child_status, c.filled_qty AS child_filled_qty, c.qty AS child_qty
         FROM copy_links l
         LEFT JOIN orders m ON m.id = l.master_order_id
         LEFT JOIN orders c ON c.id = l.child_order_id
         ORDER BY l.created_at DESC, l.rowid DESC
         LIMIT ?",
    )
    .bind(limit.clamp(1, 1000))
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(mult: f64, rounding: Rounding, min_one: bool) -> ChildConfig {
        ChildConfig { account_id: "c".into(), enabled: true, multiplier: mult, rounding, min_one_lot: min_one }
    }

    #[test]
    fn scaling_rounds_as_configured() {
        assert_eq!(scale_lots(3, &child(0.5, Rounding::Down, false)), Some(1));
        assert_eq!(scale_lots(3, &child(0.5, Rounding::Nearest, false)), Some(2));
        assert_eq!(scale_lots(3, &child(0.5, Rounding::Up, false)), Some(2));
        assert_eq!(scale_lots(2, &child(3.0, Rounding::Down, false)), Some(6));
    }

    #[test]
    fn below_one_lot_is_skipped_or_raised() {
        assert_eq!(scale_lots(1, &child(0.5, Rounding::Down, false)), None);
        assert_eq!(scale_lots(1, &child(0.5, Rounding::Down, true)), Some(1));
        assert_eq!(scale_lots(1, &child(0.4, Rounding::Nearest, false)), None);
    }

    #[test]
    fn float_noise_does_not_lose_a_lot() {
        assert_eq!(scale_lots(100, &child(0.29, Rounding::Down, false)), Some(29));
        assert_eq!(scale_lots(10, &child(0.7, Rounding::Down, false)), Some(7));
    }

    #[test]
    fn scaled_lots_are_capped() {
        assert_eq!(scale_lots(5000, &child(10.0, Rounding::Down, false)), Some(MAX_LOTS));
    }

    #[test]
    fn opening_orders_use_the_factor() {
        let c = child(0.5, Rounding::Down, false);
        assert_eq!(plan_lots(4, Intent::Open, &c, 0), Plan::Place(2));
        assert!(matches!(plan_lots(1, Intent::Open, &c, 0), Plan::Skip(_)));
    }

    #[test]
    fn a_full_exit_closes_the_whole_child_position() {
        let c = child(0.5, Rounding::Down, false);
        assert_eq!(plan_lots(3, Intent::Reduce { full: true }, &c, 5), Plan::Place(5));
    }

    #[test]
    fn a_partial_reduction_never_exceeds_the_child_position() {
        let c = child(1.0, Rounding::Down, false);
        assert_eq!(plan_lots(4, Intent::Reduce { full: false }, &c, 2), Plan::Place(2));
        assert_eq!(plan_lots(1, Intent::Reduce { full: false }, &c, 2), Plan::Place(1));
    }

    #[test]
    fn nothing_to_reduce_is_skipped() {
        let c = child(1.0, Rounding::Down, false);
        assert!(matches!(plan_lots(1, Intent::Reduce { full: true }, &c, 0), Plan::Skip(_)));
        assert!(matches!(plan_lots(1, Intent::Reduce { full: false }, &c, 0), Plan::Skip(_)));
    }

    #[test]
    fn config_is_validated() {
        let mut cfg = CopierConfig {
            enabled: true,
            master_account_id: "m".into(),
            children: vec![ChildConfig { account_id: "a".into(), ..ChildConfig::default() }],
        };
        assert!(cfg.validate().is_ok());
        cfg.children.push(ChildConfig { account_id: "a".into(), ..ChildConfig::default() });
        assert!(cfg.validate().is_err()); // duplicate child
        cfg.children.pop();
        cfg.children[0].account_id = "m".into();
        assert!(cfg.validate().is_err()); // master as child
        cfg.children[0].account_id = "a".into();
        cfg.children[0].multiplier = 0.0;
        assert!(cfg.validate().is_err());
        cfg.children[0].multiplier = 1.0;
        cfg.master_account_id.clear();
        assert!(cfg.validate().is_err()); // enabled without a master
        cfg.enabled = false;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn config_defaults_when_fields_are_missing() {
        let cfg: CopierConfig = serde_json::from_str(r#"{"masterAccountId":"m","children":[{"accountId":"a"}]}"#).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.children[0].multiplier, 1.0);
        assert!(cfg.children[0].enabled);
        assert_eq!(cfg.children[0].rounding, Rounding::Down);
    }
}
