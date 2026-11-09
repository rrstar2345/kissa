//! Trade copier wired into the order flow.
//!
//! * `place` (see `orders`) calls `copy_job` before placing and `spawn_copy`
//!   after a master order that was not rejected: every enabled child gets
//!   the order, scaled by its factor, through `place_inner` (so the server
//!   gate, risk checks and broker rules apply to each child order).
//! * `after_state` calls `copier_follow` on every order update: when a master
//!   order is cancelled / expires / is rejected, its unfilled child orders are
//!   cancelled; when its price or trigger changes (and it is still open), the
//!   child orders are modified to match.
//! * One child failing never stops the others. Failures are recorded in the
//!   link table and alerted (`Kind::Copier`).
//!
//! Copies run in a background task, serialised by `copier_lock`, so the
//! master order returns at once and follow-ups never overtake the copies
//! they refer to.
//!
//! Not copied: stop-loss / target legs and expiry settlement of the master
//! (they are not placed through `place`). A paper child gets the master's
//! bracket on its own copy of the entry; a Dhan child does not.

use anyhow::{anyhow, Result};
use futures::future::join_all;
use tauri::Emitter;
use tokio::sync::OwnedMutexGuard;
use tracing::warn;

use super::TradingService;
use crate::accounts;
use crate::alerts::{Alert, Kind};
use crate::charts::instrument::option_key;
use crate::trading::copier::{
    self, ChildConfig, CopierAccount, CopierConfig, CopierOverview, Intent, LinkRow, Plan, LINK_FAILED, LINK_PLACED,
    LINK_SKIPPED,
};
use crate::trading::{is_open_status, now_ts, store, today_ist, OrderRow, PlaceOrderInput, ROLE_ENTRY};

/// A master order about to be placed, and who it goes to.
pub(super) struct CopyJob {
    master_account_id: String,
    children: Vec<ChildConfig>,
    input: PlaceOrderInput,
    intent: Intent,
}

/// `BUY NIFTY 24000 CE 2026-11-24`
fn label(o: &OrderRow) -> String {
    format!("{} {} {} {} {}", o.side, o.symbol, o.strike, if o.is_call { "CE" } else { "PE" }, o.expiry)
}

impl TradingService {
    fn emit_copier(&self) {
        let _ = self.app.emit("copier-update", true);
    }

    // ------------------------------------------------------------ settings ---

    pub async fn copier_overview(&self) -> Result<CopierOverview> {
        let config = copier::load_config(&self.pool).await?;
        let mut accts = Vec::new();
        for a in accounts::list(&self.pool).await? {
            if self.brokers.has(&a.kind) {
                accts.push(CopierAccount { id: a.id, name: a.name, kind: a.kind });
            }
        }
        let mut links = copier::list_views(&self.pool, 300).await?;
        for l in &mut links {
            l.child_account_name = accts
                .iter()
                .find(|a| a.id == l.child_account_id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| "(deleted account)".into());
        }
        Ok(CopierOverview { config, accounts: accts, links })
    }

    pub async fn save_copier_config(&self, mut cfg: CopierConfig) -> Result<CopierConfig> {
        cfg.master_account_id = cfg.master_account_id.trim().to_string();
        for c in &mut cfg.children {
            c.account_id = c.account_id.trim().to_string();
        }
        cfg.validate().map_err(|e| anyhow!(e))?;
        let usable = |kind: &str| self.brokers.has(kind);
        if !cfg.master_account_id.is_empty() {
            let m = accounts::get(&self.pool, &cfg.master_account_id)
                .await
                .map_err(|_| anyhow!("The master account no longer exists"))?;
            if !usable(&m.kind) {
                return Err(anyhow!("The master must be a paper or Dhan account"));
            }
        }
        for c in &cfg.children {
            let a = accounts::get(&self.pool, &c.account_id)
                .await
                .map_err(|_| anyhow!("A child account no longer exists"))?;
            if !usable(&a.kind) {
                return Err(anyhow!("{} is not a paper or Dhan account, so it cannot receive copies", a.name));
            }
        }
        copier::save_config(&self.pool, &cfg).await?;
        self.emit_copier();
        Ok(cfg)
    }

    // ----------------------------------------------------------- replicate ---

    /// Called by `place` before the master order is sent: `Some` when the
    /// account is the master of an enabled copier with children. Captures
    /// what the order does to the master's position (open / reduce / exit),
    /// which must be read before the order can change it.
    pub(super) async fn copy_job(&self, input: &PlaceOrderInput) -> Option<CopyJob> {
        let cfg = match copier::load_config(&self.pool).await {
            Ok(c) => c,
            Err(e) => {
                warn!("Could not read the copier settings: {e}");
                return None;
            }
        };
        if !cfg.enabled || cfg.master_account_id != input.account_id {
            return None;
        }
        let children = cfg.active_children();
        if children.is_empty() {
            return None;
        }
        let instrument =
            option_key(&input.symbol.trim().to_uppercase(), &input.expiry, input.strike, input.is_call);
        let product = input.product.trim().to_uppercase();
        let side = input.side.trim().to_uppercase();
        let held = match store::open_positions(&self.pool, Some(&input.account_id), &today_ist()).await {
            Ok(p) => p.into_iter().find(|p| p.instrument == instrument && p.product == product),
            Err(e) => {
                warn!("Could not read the master position for the copier: {e}");
                return None;
            }
        };
        let intent = match held {
            Some(p) if p.lot_size > 0 => {
                let qty = input.lots * p.lot_size;
                if crate::trading::risk::is_reducing(&side, qty, p.net_qty) {
                    Intent::Reduce { full: qty == p.net_qty.abs() }
                } else {
                    Intent::Open
                }
            }
            _ => Intent::Open,
        };
        Some(CopyJob { master_account_id: cfg.master_account_id, children, input: input.clone(), intent })
    }

    /// Replicate in the background (the master order has already returned).
    pub(super) fn spawn_copy(&self, job: CopyJob, master: OrderRow, guard: OwnedMutexGuard<()>) {
        let Some(me) = self.me.upgrade() else { return };
        tokio::spawn(async move { me.run_copy(job, master, guard).await });
    }

    /// `_guard` is the copier lock, taken by `place` before the master order was sent.
    async fn run_copy(&self, job: CopyJob, master: OrderRow, _guard: OwnedMutexGuard<()>) {
        join_all(job.children.iter().map(|c| self.copy_one(&job, &master, c))).await;
        // The master may have been cancelled or edited while the copies were placed.
        if let Ok(Some(m)) = store::get_order(&self.pool, &master.id).await {
            self.sync_children(&m).await;
        }
        self.emit_copier();
    }

    /// Lots the child holds that this order would reduce.
    async fn closable_lots(&self, child_id: &str, master: &OrderRow) -> i64 {
        let positions = match store::open_positions(&self.pool, Some(child_id), &today_ist()).await {
            Ok(p) => p,
            Err(_) => return 0,
        };
        let Some(p) = positions.into_iter().find(|p| p.instrument == master.instrument && p.product == master.product)
        else {
            return 0;
        };
        let lot = if p.lot_size > 0 { p.lot_size } else { master.lot_size.max(1) };
        let closable = if master.side == "BUY" { (-p.net_qty).max(0) } else { p.net_qty.max(0) };
        closable / lot
    }

    async fn record(&self, job: &CopyJob, link: &LinkRow) {
        if let Err(e) = copier::insert_link(&self.pool, link, &job.master_account_id).await {
            warn!("Could not record the copy of {} to {}: {e}", link.master_order_id, link.child_account_id);
        }
    }

    async fn copy_one(&self, job: &CopyJob, master: &OrderRow, child: &ChildConfig) {
        let master_lots = job.input.lots;
        let mut link = LinkRow {
            id: uuid::Uuid::new_v4().to_string(),
            master_order_id: master.id.clone(),
            child_account_id: child.account_id.clone(),
            child_order_id: None,
            master_lots,
            child_lots: 0,
            status: LINK_FAILED.into(),
            message: String::new(),
            created_at: now_ts(),
            trade_date: today_ist(),
        };

        let acct = match accounts::get(&self.pool, &child.account_id).await {
            Ok(a) => a,
            Err(_) => {
                link.message = "The child account no longer exists".into();
                self.record(job, &link).await;
                self.alert_copy_failed(master, &child.account_id, &link).await;
                return;
            }
        };

        let closable = match job.intent {
            Intent::Reduce { .. } => self.closable_lots(&child.account_id, master).await,
            Intent::Open => 0,
        };
        let lots = match copier::plan_lots(master_lots, job.intent, child, closable) {
            Plan::Place(l) => l,
            Plan::Skip(why) => {
                link.status = LINK_SKIPPED.into();
                link.message = why;
                self.record(job, &link).await;
                return;
            }
        };
        link.child_lots = lots;

        let mut input = job.input.clone();
        input.account_id = child.account_id.clone();
        input.lots = lots;
        if input.bracket.is_some() && acct.kind != "paper" {
            input.bracket = None;
            link.message = "Stop-loss / target not copied (paper accounts only)".into();
        }

        // A full exit also drops the child's own resting orders in the
        // contract (its stop-loss / target legs), like Exit does on the master.
        if matches!(job.intent, Intent::Reduce { full: true }) {
            if let Ok(open) = store::open_orders(&self.pool, None).await {
                for o in open {
                    if o.account_id == child.account_id && o.instrument == master.instrument && o.product == master.product {
                        if let Err(e) = self.cancel(&o.id).await {
                            link.message = format!("Could not cancel the child's open {} order first: {e}", o.side);
                            self.record(job, &link).await;
                            self.alert_copy_failed(master, &child.account_id, &link).await;
                            return;
                        }
                    }
                }
            }
        }

        match self.place_inner(input).await {
            Ok(o) => {
                link.child_order_id = Some(o.id.clone());
                if o.status == "REJECTED" {
                    // The Rejection alert has already gone out for this order.
                    link.status = LINK_FAILED.into();
                    link.message = o.message.clone();
                } else {
                    link.status = LINK_PLACED.into();
                }
                self.record(job, &link).await;
            }
            Err(e) => {
                link.message = e.to_string();
                self.record(job, &link).await;
                self.alert_copy_failed(master, &child.account_id, &link).await;
            }
        }
        self.emit_copier();
    }

    async fn alert_copy_failed(&self, master: &OrderRow, child_account_id: &str, link: &LinkRow) {
        let name = self.account_name(child_account_id).await;
        let master_name = self.account_name(&master.account_id).await;
        self.alerts.notify(Alert::new(
            Kind::Copier,
            link.id.clone(),
            format!("Copy failed: {} → {name}", label(master)),
            format!("Master {master_name} · {} lot\n{}", link.master_lots, link.message),
        ));
    }

    // -------------------------------------------------------------- follow ---

    /// After a master order's state changed: bring its copies in line.
    /// Cheap for every other order (one indexed lookup in the background).
    pub(super) fn copier_follow(&self, o: &OrderRow) {
        if !(o.role.is_empty() || o.role == ROLE_ENTRY) {
            return;
        }
        if o.status == "TRADED" {
            return;
        }
        let Some(me) = self.me.upgrade() else { return };
        let id = o.id.clone();
        tokio::spawn(async move {
            let _guard = me.copier_lock.lock().await;
            // Read the order afresh: updates may have arrived while waiting.
            if let Ok(Some(m)) = store::get_order(&me.pool, &id).await {
                me.sync_children(&m).await;
            }
        });
    }

    /// Cancel the unfilled copies of a master order that ended without
    /// filling, or move their price / trigger to the master's.
    async fn sync_children(&self, master: &OrderRow) {
        let links = match copier::placed_links(&self.pool, &master.id).await {
            Ok(l) if !l.is_empty() => l,
            Ok(_) => return,
            Err(e) => {
                warn!("Could not read the copies of {}: {e}", master.id);
                return;
            }
        };
        let ended = matches!(master.status.as_str(), "CANCELLED" | "EXPIRED" | "REJECTED");
        let working = is_open_status(&master.status) && master.status != "UNKNOWN";
        let mut changed = false;

        for l in links {
            let Some(child_id) = l.child_order_id.clone() else { continue };
            let Ok(Some(child)) = store::get_order(&self.pool, &child_id).await else { continue };
            if !is_open_status(&child.status) {
                continue;
            }
            let (action, result): (&str, Result<String>) = if ended {
                ("cancel", self.cancel(&child_id).await.map(|_| format!("Cancelled with the master order ({})", master.status)))
            } else if working
                && child.filled_qty == 0
                && ((child.price - master.price).abs() > 1e-9 || (child.trigger_price - master.trigger_price).abs() > 1e-9)
            {
                let trigger = (master.order_type == "STOP_LOSS").then_some(master.trigger_price);
                (
                    "modify",
                    self.modify(&child_id, master.price, trigger)
                        .await
                        .map(|_| format!("Price follows the master: {:.2}", master.price)),
                )
            } else {
                continue;
            };
            changed = true;
            match result {
                Ok(note) => {
                    let _ = copier::set_link_message(&self.pool, &l.id, &note).await;
                }
                Err(e) => {
                    let msg = format!("Could not {action} the child order: {e}");
                    let _ = copier::set_link_message(&self.pool, &l.id, &msg).await;
                    let name = self.account_name(&l.child_account_id).await;
                    self.alerts.notify(Alert::new(
                        Kind::Copier,
                        format!("{}:{action}", l.id),
                        format!("Copy out of step: {} → {name}", label(master)),
                        format!("The master order changed ({}) but the copy could not follow.\n{e}", master.status),
                    ));
                }
            }
        }
        if changed {
            self.emit_copier();
        }
    }
}
