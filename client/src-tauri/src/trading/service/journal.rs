//! Audit journal entries for the order flow (see `crate::audit`). The actor
//! is taken from the task-local scope: Tauri commands run as the user, the
//! algo runner and the trade copier in their own scopes, the rest as system.

use serde_json::json;

use crate::audit::{self, Actor, Category, Entry};
use crate::trading::{OrderRow, PlaceOrderInput, ROLE_SETTLE, ROLE_STOP, ROLE_TARGET};

/// `BUY NIFTY 24000 CE 2026-11-24`
fn contract(side: &str, symbol: &str, strike: f64, is_call: bool, expiry: &str) -> String {
    format!("{side} {symbol} {strike} {} {expiry}", if is_call { "CE" } else { "PE" })
}

fn order_what(o: &OrderRow) -> String {
    contract(&o.side, &o.symbol, o.strike, o.is_call, &o.expiry)
}

fn lots_text(o: &OrderRow) -> String {
    if o.lot_size > 0 {
        let lots = o.qty / o.lot_size;
        format!("{lots} lot{} ({})", if lots == 1 { "" } else { "s" }, o.qty)
    } else {
        o.qty.to_string()
    }
}

fn role_label(role: &str) -> &'static str {
    match role {
        ROLE_STOP => "Stop-loss",
        ROLE_TARGET => "Target",
        ROLE_SETTLE => "Expiry settlement",
        _ => "Order",
    }
}

fn price_text(o: &OrderRow) -> String {
    if o.trigger_price > 0.0 {
        format!("{} @ {:.2} (trigger {:.2})", o.order_type, o.price, o.trigger_price)
    } else {
        format!("{} @ {:.2}", o.order_type, o.price)
    }
}

fn order_detail(o: &OrderRow) -> serde_json::Value {
    json!({
        "orderId": o.id,
        "correlationId": o.correlation_id,
        "brokerOrderId": o.broker_order_id,
        "accountKind": o.account_kind,
        "instrument": o.instrument,
        "side": o.side,
        "product": o.product,
        "orderType": o.order_type,
        "validity": o.validity,
        "qty": o.qty,
        "lotSize": o.lot_size,
        "price": o.price,
        "triggerPrice": o.trigger_price,
        "status": o.status,
        "role": o.role,
        "groupId": o.group_id,
    })
}

fn order_entry(o: &OrderRow, action: &str, summary: String) -> Entry {
    Entry::new(Category::Order, action, summary)
        .account(o.account_id.clone())
        .subject(o.id.clone())
        .detail(order_detail(o))
}

/// An order (or a stop-loss / target leg) was created and sent to the broker.
pub(super) fn placed(o: &OrderRow) {
    audit::record(order_entry(
        o,
        "placed",
        format!("{} placed: {} · {} · {} · {}", role_label(&o.role), order_what(o), lots_text(o), price_text(o), o.product),
    ));
}

/// The request never became an order (validation, risk, funds, server gate…).
pub(super) fn place_failed(input: &PlaceOrderInput, reason: &str) {
    let what = contract(&input.side.to_uppercase(), &input.symbol, input.strike, input.is_call, &input.expiry);
    audit::record(
        Entry::new(
            Category::Order,
            "place_failed",
            format!("Order not placed: {what} · {} lot(s) @ {:.2} · {reason}", input.lots, input.price),
        )
        .account(input.account_id.clone())
        .detail(json!({
            "side": input.side, "product": input.product, "orderType": input.order_type,
            "lots": input.lots, "price": input.price, "triggerPrice": input.trigger_price, "reason": reason,
        })),
    );
}

/// A fill was booked: `qty` units at `price`, `total` of the order so far.
pub(super) fn fill(o: &OrderRow, qty: i64, price: f64, total: i64) {
    let what = match o.role.as_str() {
        ROLE_STOP => "Stop-loss hit",
        ROLE_TARGET => "Target hit",
        ROLE_SETTLE => "Expiry settlement",
        _ => "Filled",
    };
    let progress = if total < o.qty { format!(" · {total} of {} filled", o.qty) } else { String::new() };
    audit::record(
        order_entry(o, "fill", format!("{what}: {} · {qty} @ {price:.2}{progress}", order_what(o)))
            .actor(Actor::Broker)
            .detail(json!({
                "orderId": o.id, "side": o.side, "instrument": o.instrument, "role": o.role,
                "fillQty": qty, "fillPrice": price, "totalFilled": total, "orderQty": o.qty,
            })),
    );
}

/// Cancelled, rejected, expired or unknown. Rejections and expiries come from
/// the broker; a cancel keeps the actor of whoever asked for it.
pub(super) fn status(o: &OrderRow, action: &str) {
    let label = match action {
        "cancelled" => "cancelled",
        "rejected" => "rejected",
        "expired" => "expired",
        "unknown" => "outcome unknown",
        other => other,
    };
    let reason = if o.message.trim().is_empty() { String::new() } else { format!(" · {}", o.message.trim()) };
    let mut e = order_entry(o, action, format!("{} {label}: {} · {}{reason}", role_label(&o.role), order_what(o), lots_text(o)));
    if matches!(action, "rejected" | "expired") {
        e = e.actor(Actor::Broker);
    }
    audit::record(e);
}

pub(super) fn cancel_failed(o: &OrderRow, reason: &str) {
    audit::record(order_entry(
        o,
        "cancel_failed",
        format!("Cancel failed: {} · {reason}", order_what(o)),
    ));
}

pub(super) fn modified(o: &OrderRow, old_price: f64, old_trigger: f64) {
    let trig = if o.trigger_price > 0.0 || old_trigger > 0.0 {
        format!(" · trigger {old_trigger:.2} → {:.2}", o.trigger_price)
    } else {
        String::new()
    };
    audit::record(
        order_entry(o, "modified", format!("Order modified: {} · price {old_price:.2} → {:.2}{trig}", order_what(o), o.price))
            .detail(json!({
                "orderId": o.id, "oldPrice": old_price, "newPrice": o.price,
                "oldTrigger": old_trigger, "newTrigger": o.trigger_price,
            })),
    );
}

pub(super) fn modify_failed(o: &OrderRow, reason: &str) {
    audit::record(order_entry(o, "modify_failed", format!("Modify failed: {} · {reason}", order_what(o))));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_text() {
        assert_eq!(contract("BUY", "NIFTY", 24000.0, true, "2026-11-24"), "BUY NIFTY 24000 CE 2026-11-24");
        assert_eq!(contract("SELL", "BANKNIFTY", 52500.5, false, "2026-11-24"), "SELL BANKNIFTY 52500.5 PE 2026-11-24");
    }

    #[test]
    fn role_labels() {
        assert_eq!(role_label(ROLE_STOP), "Stop-loss");
        assert_eq!(role_label(ROLE_TARGET), "Target");
        assert_eq!(role_label(""), "Order");
    }
}
