//! Risk controls wired into the order flow.
//!
//! * `check_risk` runs inside `place`, so manual orders, Exit, and later the
//!   copier and algos all pass through it. Orders that only reduce a position
//!   are always allowed (see `trading::risk`).
//! * `enforce_risk` runs on every sweeper tick: when an account's net P&L for
//!   the day reaches its loss limit the account is halted until the next IST
//!   day, its resting entry orders are cancelled and, if asked, every position
//!   is exited.
//! * The kill switch halts every account until it is released.
//!
//! For algos: `halt_reason(account)` says whether new entries are blocked
//! right now (kill switch or daily-loss halt). Disarm on `Some`.

use anyhow::{anyhow, Result};
use chrono::Timelike;
use std::collections::HashSet;
use tauri::Emitter;
use tracing::{info, warn};

use super::TradingService;
use crate::accounts::{self, Account};
use crate::market::market_clock;
use crate::trading::risk::{
    self, AccountRisk, HaltRow, KillResult, OrderCheck, RiskLimits, RiskOverview, GLOBAL, HALT_KILL, HALT_LOSS,
};
use crate::trading::{now_ts, store, today_ist, ROLE_STOP, ROLE_TARGET};

const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;

impl TradingService {
    fn emit_risk(&self) {
        let _ = self.app.emit("risk-update", true);
    }

    /// Why new entries are blocked for the account right now: the kill
    /// switch or an active daily-loss halt. `None` = allowed.
    pub async fn halt_reason(&self, account_id: &str) -> Option<String> {
        let today = today_ist();
        if let Ok(Some(k)) = risk::get_halt(&self.pool, GLOBAL).await {
            if k.active(&today) {
                return Some("kill switch is on".into());
            }
        }
        match risk::get_halt(&self.pool, account_id).await {
            Ok(Some(h)) if h.active(&today) => Some(h.reason),
            _ => None,
        }
    }

    /// Today's net P&L of one account (realised + open positions at the last
    /// price - charges), the same figure the dashboard shows.
    async fn net_today(&self, account_id: &str) -> Result<f64> {
        let dash = self.dashboard(Some(account_id)).await?;
        Ok(dash.accounts.first().map(|a| a.net_today).unwrap_or(0.0))
    }

    // ----------------------------------------------------------- order check ---

    /// Refuse the order when it breaks a limit, a halt or the kill switch.
    /// Called by `place` before anything is stored or sent.
    pub(super) async fn check_risk(
        &self,
        acct: &Account,
        side: &str,
        product: &str,
        instrument: &str,
        lots: i64,
        qty: i64,
    ) -> Result<()> {
        let today = today_ist();
        let positions = store::open_positions(&self.pool, Some(&acct.id), &today).await?;
        let net_qty = positions
            .iter()
            .find(|p| p.instrument == instrument && p.product == product)
            .map(|p| p.net_qty)
            .unwrap_or(0);
        if risk::is_reducing(side, qty, net_qty) {
            return Ok(()); // never trap the user in a position
        }

        let limits = risk::get_limits(&self.pool, &acct.id).await?;
        let kill = risk::get_halt(&self.pool, GLOBAL).await?.map(|h| h.active(&today)).unwrap_or(false);
        let loss_halt = risk::get_halt(&self.pool, &acct.id).await?;
        let halt_reason = loss_halt.as_ref().filter(|h| h.active(&today)).map(|h| h.reason.clone());
        // A daily-loss halt row of today (even one the user lifted) means the
        // breach was handled: do not block on the loss figure again.
        let loss_handled = loss_halt.as_ref().map(|h| h.trade_date == today).unwrap_or(false);

        // Contracts the account holds, plus contracts with a resting entry order.
        let mut keys: HashSet<(String, String)> =
            positions.iter().map(|p| (p.instrument.clone(), p.product.clone())).collect();
        for o in store::open_orders(&self.pool, None).await? {
            if o.account_id == acct.id && o.role != ROLE_STOP && o.role != ROLE_TARGET {
                keys.insert((o.instrument, o.product));
            }
        }
        let key_is_open = keys.contains(&(instrument.to_string(), product.to_string()));

        let net_today = if limits.enabled && limits.max_daily_loss > 0.0 && !loss_handled {
            self.net_today(&acct.id).await?
        } else {
            0.0
        };
        let ist = market_clock::get_ist_now() + chrono::Duration::seconds(IST_OFFSET_SECS);
        let check = OrderCheck {
            side,
            lots,
            qty,
            net_qty,
            open_keys: keys.len(),
            key_is_open,
            minutes_now: ist.hour() * 60 + ist.minute(),
            net_today,
        };
        risk::check(&limits, kill, halt_reason.as_deref(), &check).map_err(|m| {
            warn!("Order refused ({}): {m}", acct.name);
            anyhow!("{m}")
        })
    }

    // --------------------------------------------------------- loss monitor ---

    /// Sweeper hook (every 5 s): halt accounts that reached their daily loss.
    pub(super) async fn enforce_risk(&self) {
        if let Err(e) = self.enforce_daily_loss().await {
            warn!("Risk monitor failed: {e}");
        }
    }

    async fn enforce_daily_loss(&self) -> Result<()> {
        let today = today_ist();
        let mut watch: Vec<(Account, RiskLimits)> = Vec::new();
        for a in accounts::list(&self.pool).await? {
            if !self.brokers.has(&a.kind) {
                continue;
            }
            let limits = risk::get_limits(&self.pool, &a.id).await?;
            if !(limits.enabled && limits.max_daily_loss > 0.0) {
                continue;
            }
            // A halt row of today (active or lifted by the user) = already handled.
            if let Some(h) = risk::get_halt(&self.pool, &a.id).await? {
                if h.kind == HALT_LOSS && h.trade_date == today {
                    continue;
                }
            }
            watch.push((a, limits));
        }
        if watch.is_empty() {
            return Ok(());
        }
        let dash = self.dashboard(None).await?;
        for (a, limits) in watch {
            let Some(s) = dash.accounts.iter().find(|s| s.account_id == a.id) else { continue };
            if s.net_today > -limits.max_daily_loss {
                continue;
            }
            let reason = format!(
                "daily loss limit of {:.2} reached (net P&L {:.2})",
                limits.max_daily_loss, s.net_today
            );
            self.halt_account(&a, &limits, &reason).await;
        }
        Ok(())
    }

    async fn halt_account(&self, a: &Account, limits: &RiskLimits, reason: &str) {
        let row = HaltRow {
            account_id: a.id.clone(),
            kind: HALT_LOSS.into(),
            trade_date: today_ist(),
            reason: reason.into(),
            at: now_ts(),
            released: false,
        };
        if let Err(e) = risk::set_halt(&self.pool, &row).await {
            warn!("Could not save the halt of {}: {e}", a.name);
            return;
        }
        warn!("Risk: {} halted, {reason}", a.name);
        self.emit_risk();

        let cancelled = self.cancel_entries(Some(&a.id)).await;
        let mut exited = 0;
        if limits.square_off_on_breach {
            match self.exit_all(Some(&a.id)).await {
                Ok(r) => {
                    exited = r.placed;
                    for s in &r.skipped {
                        warn!("Risk square-off of {} skipped: {s}", a.name);
                    }
                }
                Err(e) => warn!("Risk square-off of {} failed: {e}", a.name),
            }
        }
        info!("Risk: {} halted; {cancelled} entry order(s) cancelled, {exited} exit order(s) placed", a.name);
        self.alert_risk(
            format!("Risk halt: {}", a.name),
            format!(
                "{reason}. {cancelled} entry order(s) cancelled, {exited} exit order(s) placed. New positions are blocked until tomorrow."
            ),
        );
    }

    /// Cancel resting orders that would add exposure (entries). Stop-loss and
    /// target legs and orders that only reduce a position are left alone.
    async fn cancel_entries(&self, account: Option<&str>) -> usize {
        let today = today_ist();
        let positions = match store::open_positions(&self.pool, account, &today).await {
            Ok(p) => p,
            Err(e) => {
                warn!("Could not read positions to cancel entries: {e}");
                return 0;
            }
        };
        let open = match store::open_orders(&self.pool, None).await {
            Ok(o) => o,
            Err(e) => {
                warn!("Could not read open orders to cancel entries: {e}");
                return 0;
            }
        };
        let mut cancelled = 0;
        for o in open {
            if let Some(acc) = account {
                if o.account_id != acc {
                    continue;
                }
            }
            if o.role == ROLE_STOP || o.role == ROLE_TARGET {
                continue;
            }
            let net = positions
                .iter()
                .find(|p| p.account_id == o.account_id && p.instrument == o.instrument && p.product == o.product)
                .map(|p| p.net_qty)
                .unwrap_or(0);
            if risk::is_reducing(&o.side, o.qty - o.filled_qty, net) {
                continue;
            }
            match self.cancel(&o.id).await {
                Ok(_) => cancelled += 1,
                Err(e) => warn!("Could not cancel entry order {}: {e}", o.id),
            }
        }
        cancelled
    }

    // -------------------------------------------------------------- commands ---

    pub async fn risk_overview(&self) -> Result<RiskOverview> {
        let today = today_ist();
        let dash = self.dashboard(None).await?;
        let kill_switch =
            risk::get_halt(&self.pool, GLOBAL).await?.filter(|h| h.active(&today)).map(|h| h.info());
        let mut out = Vec::new();
        for a in accounts::list(&self.pool).await? {
            if !self.brokers.has(&a.kind) {
                continue;
            }
            let limits = risk::get_limits(&self.pool, &a.id).await?;
            let halt = risk::get_halt(&self.pool, &a.id).await?.filter(|h| h.active(&today)).map(|h| h.info());
            let summary = dash.accounts.iter().find(|s| s.account_id == a.id);
            out.push(AccountRisk {
                net_today: summary.map(|s| s.net_today).unwrap_or(0.0),
                open_positions: summary.map(|s| s.open_positions).unwrap_or(0),
                account_id: a.id,
                name: a.name,
                kind: a.kind,
                limits,
                halt,
            });
        }
        Ok(RiskOverview { kill_switch, accounts: out })
    }

    pub async fn save_risk_limits(&self, account_id: &str, limits: RiskLimits) -> Result<RiskLimits> {
        limits.validate().map_err(|e| anyhow!("{e}"))?;
        accounts::get(&self.pool, account_id).await?;
        risk::save_limits(&self.pool, account_id, &limits).await?;
        self.emit_risk();
        Ok(limits)
    }

    /// Turn the kill switch on or off. On: every account stops taking new
    /// positions and resting entry orders are cancelled; with `flatten` every
    /// open position is exited as well. Off: trading is allowed again (a
    /// daily-loss halt of an account stays until the day ends or it is resumed).
    pub async fn set_kill_switch(&self, on: bool, flatten: bool) -> Result<KillResult> {
        let mut out = KillResult::default();
        if !on {
            risk::clear_halt(&self.pool, GLOBAL).await?;
            info!("Risk: kill switch released");
            self.emit_risk();
            self.alert_risk("Kill switch released".into(), "Trading is allowed again.".into());
            return Ok(out);
        }
        let row = HaltRow {
            account_id: GLOBAL.into(),
            kind: HALT_KILL.into(),
            trade_date: String::new(),
            reason: "kill switch".into(),
            at: now_ts(),
            released: false,
        };
        risk::set_halt(&self.pool, &row).await?;
        warn!("Risk: kill switch ON");
        self.emit_risk();

        out.cancelled = self.cancel_entries(None).await;
        if flatten {
            let r = self.exit_all(None).await?;
            out.placed = r.placed;
            out.cancelled += r.cancelled;
            out.skipped = r.skipped;
        }
        self.alert_risk(
            "Kill switch ON".into(),
            format!(
                "No new positions on any account. {} order(s) cancelled{}.",
                out.cancelled,
                if flatten { format!(", {} exit order(s) placed", out.placed) } else { String::new() }
            ),
        );
        Ok(out)
    }

    /// Lift today's daily-loss halt of an account. The breach is not raised
    /// again until the next day.
    pub async fn resume_account(&self, account_id: &str) -> Result<()> {
        if let Some(mut h) = risk::get_halt(&self.pool, account_id).await? {
            if h.kind == HALT_LOSS {
                h.released = true;
                risk::set_halt(&self.pool, &h).await?;
                info!("Risk: halt of account {account_id} lifted for today");
                self.emit_risk();
            }
        }
        Ok(())
    }
}
