//! Bracket orders for paper accounts: an entry order with a stop-loss and / or
//! a target attached.
//!
//! * The entry is an ordinary order with `role = ENTRY`; the legs wait in its
//!   `bracket` column (JSON of `BracketInput`).
//! * When the entry fills (fully, or partly before it ended), the legs are
//!   placed for the filled quantity: a LIMIT target and a STOP_LOSS
//!   (stop-limit) on the opposite side, both with `group_id` = the entry's id.
//! * One-cancels-other: when a leg is fully executed, the other leg is cancelled.
//! * An entry that ends without a fill drops its legs.
//!
//! The legs are closing orders, so they skip the market-hours and funds checks
//! of a new order; they rest in the paper book until the market allows a fill.

use anyhow::{anyhow, Result};
use std::future::Future;
use std::pin::Pin;
use tracing::warn;

use super::{is_terminal, TradingService};
use crate::trading::broker::Contract;
use crate::trading::pricing::{exit_price, on_tick};
use crate::trading::{
    is_open_status, now_ts, store, BracketInput, OrderRow, ROLE_ENTRY, ROLE_STOP, ROLE_TARGET,
};

/// Check the legs against the entry and fill in the stop-loss limit price.
/// `None` when no leg was asked for. `entry_price` is the entry's limit price.
pub(super) fn validate_bracket(
    entry_side: &str,
    entry_price: f64,
    b: &BracketInput,
    buffer_ticks: u32,
) -> Result<Option<BracketInput>> {
    if b.target.is_none() && b.stop_trigger.is_none() {
        return Ok(None);
    }
    let buy = entry_side.eq_ignore_ascii_case("BUY");
    let exit_side = if buy { "SELL" } else { "BUY" };
    let ok_price = |p: f64| p.is_finite() && p > 0.0 && on_tick(p);

    if let Some(t) = b.target {
        if !ok_price(t) {
            return Err(anyhow!("Target must be greater than 0 and a multiple of 0.05"));
        }
        if (buy && t <= entry_price) || (!buy && t >= entry_price) {
            return Err(anyhow!(
                "Target must be {} the entry price ({entry_price:.2})",
                if buy { "above" } else { "below" }
            ));
        }
    }

    let mut out = BracketInput { target: b.target, stop_trigger: None, stop_price: None };
    if let Some(trig) = b.stop_trigger {
        if !ok_price(trig) {
            return Err(anyhow!("Stop-loss trigger must be greater than 0 and a multiple of 0.05"));
        }
        if (buy && trig >= entry_price) || (!buy && trig <= entry_price) {
            return Err(anyhow!(
                "Stop-loss trigger must be {} the entry price ({entry_price:.2})",
                if buy { "below" } else { "above" }
            ));
        }
        let limit = match b.stop_price {
            Some(p) => {
                if !ok_price(p) {
                    return Err(anyhow!("Stop-loss limit price must be greater than 0 and a multiple of 0.05"));
                }
                // A sell stop must not rest above its trigger, a buy stop not below it.
                if (!buy && p < trig) || (buy && p > trig) {
                    return Err(anyhow!(
                        "Stop-loss limit price must be {} its trigger ({trig:.2})",
                        if buy { "at or below" } else { "at or above" }
                    ));
                }
                p
            }
            None => exit_price(exit_side, trig, buffer_ticks),
        };
        out.stop_trigger = Some(trig);
        out.stop_price = Some(limit);
    } else if b.stop_price.is_some() {
        return Err(anyhow!("A stop-loss limit price needs a stop-loss trigger"));
    }
    Ok(Some(out))
}

impl TradingService {
    /// Follow-up work after an order's state was saved: place the legs of a
    /// bracket entry, or cancel the other leg after a leg executed. Boxed
    /// because placing a leg goes back through `apply_state`.
    pub(super) fn after_state<'a>(&'a self, o: &'a OrderRow) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // Trade copier: master orders drag their copies along.
            self.copier_follow(o);
            match o.role.as_str() {
                ROLE_ENTRY if !o.bracket.is_empty() => self.entry_ended_or_filled(o).await,
                ROLE_TARGET | ROLE_STOP => self.leg_changed(o).await,
                _ => {}
            }
        })
    }

    async fn entry_ended_or_filled(&self, entry: &OrderRow) {
        let ended = is_terminal(&entry.status);
        let filled = entry.status == "TRADED" || (ended && entry.filled_qty > 0);
        if !filled && !ended {
            return; // still working
        }
        // Only one caller wins the legs, however many events arrive.
        let json = match store::take_bracket(&self.pool, &entry.id).await {
            Ok(Some(j)) => j,
            Ok(None) => return,
            Err(e) => {
                warn!("Could not read the bracket of {}: {e}", entry.id);
                return;
            }
        };
        if !filled {
            return; // never filled: no position to protect
        }
        let Ok(b) = serde_json::from_str::<BracketInput>(&json) else {
            warn!("Bracket of {} is unreadable: {json}", entry.id);
            return;
        };
        if let Err(e) = self.place_legs(entry, &b).await {
            warn!("Could not place the bracket legs of {}: {e}", entry.id);
            let _ = store::set_order_message(&self.pool, &entry.id, &format!("Filled, but the stop-loss / target could not be placed: {e}")).await;
        }
    }

    fn leg(&self, entry: &OrderRow, role: &str, order_type: &str, price: f64, trigger: f64) -> OrderRow {
        let uid = uuid::Uuid::new_v4();
        let now = now_ts();
        OrderRow {
            id: uid.to_string(),
            account_id: entry.account_id.clone(),
            account_kind: entry.account_kind.clone(),
            correlation_id: format!("b{}", &uid.simple().to_string()[..20]),
            broker_order_id: None,
            instrument: entry.instrument.clone(),
            symbol: entry.symbol.clone(),
            expiry: entry.expiry.clone(),
            strike: entry.strike,
            is_call: entry.is_call,
            security_id: entry.security_id.clone(),
            lot_size: entry.lot_size,
            side: if entry.side.eq_ignore_ascii_case("BUY") { "SELL" } else { "BUY" }.into(),
            product: entry.product.clone(),
            order_type: order_type.into(),
            validity: "DAY".into(),
            qty: entry.filled_qty,
            price,
            trigger_price: trigger,
            status: "TRANSIT".into(),
            filled_qty: 0,
            avg_price: 0.0,
            message: String::new(),
            created_at: now.clone(),
            updated_at: now,
            trade_date: crate::trading::today_ist(),
            group_id: entry.id.clone(),
            role: role.into(),
            bracket: String::new(),
        }
    }

    async fn place_legs(&self, entry: &OrderRow, b: &BracketInput) -> Result<()> {
        let broker = self.broker_for_kind(&entry.account_kind)?;
        let contract = Contract { lot_size: entry.lot_size, security_id: entry.security_id.clone(), trading_symbol: None };

        let mut legs = Vec::new();
        if let Some(t) = b.target {
            legs.push(self.leg(entry, ROLE_TARGET, "LIMIT", t, 0.0));
        }
        if let Some(trig) = b.stop_trigger {
            let limit = b.stop_price.unwrap_or(trig);
            legs.push(self.leg(entry, ROLE_STOP, "STOP_LOSS", limit, trig));
        }
        // Both legs exist before either is sent, so one that fills at once can cancel the other.
        for l in &legs {
            store::insert_order(&self.pool, l).await?;
            self.emit_order(l);
        }
        for l in &legs {
            // The other leg may already have executed and cancelled this one.
            match store::get_order(&self.pool, &l.id).await? {
                Some(cur) if is_open_status(&cur.status) => {}
                _ => continue,
            }
            match broker.place(l, &contract).await {
                Ok(reply) => {
                    self.apply_reply(l.clone(), reply).await?;
                }
                Err(e) => {
                    let mut rejected = l.clone();
                    rejected.status = "REJECTED".into();
                    rejected.message = e.to_string();
                    store::save_order_state(&self.pool, &rejected).await?;
                    if let Some(row) = store::get_order(&self.pool, &l.id).await? {
                        self.emit_order(&row);
                    }
                }
            }
        }
        Ok(())
    }

    /// One-cancels-other: a leg that executed in full cancels its sibling.
    async fn leg_changed(&self, leg: &OrderRow) {
        if leg.group_id.is_empty() || leg.status != "TRADED" {
            return;
        }
        let siblings = match store::open_bracket_legs(&self.pool, &leg.group_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Could not look up the other leg of {}: {e}", leg.group_id);
                return;
            }
        };
        let which = if leg.role == ROLE_TARGET { "target" } else { "stop-loss" };
        for s in siblings.into_iter().filter(|s| s.id != leg.id) {
            match self.cancel(&s.id).await {
                Ok(_) => {
                    let _ = store::set_order_message(&self.pool, &s.id, &format!("Cancelled: the {which} executed")).await;
                    if let Ok(Some(row)) = store::get_order(&self.pool, &s.id).await {
                        self.emit_order(&row);
                    }
                }
                Err(e) => warn!("Could not cancel the other bracket leg {}: {e}", s.id),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(target: Option<f64>, stop: Option<f64>, limit: Option<f64>) -> BracketInput {
        BracketInput { target, stop_trigger: stop, stop_price: limit }
    }

    #[test]
    fn no_legs_means_no_bracket() {
        assert_eq!(validate_bracket("BUY", 100.0, &b(None, None, None), 2).unwrap(), None);
    }

    #[test]
    fn long_entry_needs_target_above_and_stop_below() {
        let ok = validate_bracket("BUY", 100.0, &b(Some(120.0), Some(90.0), None), 2).unwrap().unwrap();
        assert_eq!(ok.target, Some(120.0));
        assert_eq!(ok.stop_trigger, Some(90.0));
        assert_eq!(ok.stop_price, Some(89.9)); // trigger less 2 ticks
        assert!(validate_bracket("BUY", 100.0, &b(Some(95.0), None, None), 2).is_err());
        assert!(validate_bracket("BUY", 100.0, &b(None, Some(105.0), None), 2).is_err());
    }

    #[test]
    fn short_entry_is_mirrored() {
        let ok = validate_bracket("SELL", 100.0, &b(Some(80.0), Some(110.0), None), 2).unwrap().unwrap();
        assert_eq!(ok.stop_price, Some(110.1)); // trigger plus 2 ticks
        assert!(validate_bracket("SELL", 100.0, &b(Some(105.0), None, None), 2).is_err());
        assert!(validate_bracket("SELL", 100.0, &b(None, Some(95.0), None), 2).is_err());
    }

    #[test]
    fn stop_limit_price_must_sit_beyond_the_trigger() {
        // Long entry: the stop sells, so its limit may not be above the trigger.
        assert!(validate_bracket("BUY", 100.0, &b(None, Some(90.0), Some(91.0)), 2).is_err());
        let ok = validate_bracket("BUY", 100.0, &b(None, Some(90.0), Some(88.0)), 2).unwrap().unwrap();
        assert_eq!(ok.stop_price, Some(88.0));
        // A limit without a trigger is meaningless.
        assert!(validate_bracket("BUY", 100.0, &b(Some(120.0), None, Some(88.0)), 2).is_err());
    }

    #[test]
    fn off_tick_prices_are_refused() {
        assert!(validate_bracket("BUY", 100.0, &b(Some(120.03), None, None), 2).is_err());
        assert!(validate_bracket("BUY", 100.0, &b(None, Some(90.02), None), 2).is_err());
    }
}
