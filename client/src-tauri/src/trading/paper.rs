//! Paper trading: pending orders are kept in memory (and in SQLite) and
//! matched against live prices from the `LtpStore`.
//!
//! Matching rules (whole quantity only, no partial fills):
//! * LIMIT BUY fills when LTP <= limit, at min(LTP, limit); LIMIT SELL fills
//!   when LTP >= limit, at max(LTP, limit).
//! * STOP_LOSS (stop-limit) is armed when LTP reaches the trigger (BUY: LTP >=
//!   trigger, SELL: LTP <= trigger) and then behaves like a LIMIT order.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use super::OrderRow;

#[derive(Debug, PartialEq)]
pub enum Decision {
    Wait,
    /// Stop price reached but the limit is not marketable yet.
    Trigger,
    Fill(f64),
}

pub fn evaluate(o: &OrderRow, triggered: bool, ltp: f64) -> Decision {
    if !ltp.is_finite() || ltp <= 0.0 {
        return Decision::Wait;
    }
    let buy = o.side.eq_ignore_ascii_case("BUY");
    let mut armed = triggered;
    if o.order_type == "STOP_LOSS" && !armed {
        let hit = if buy { ltp >= o.trigger_price } else { ltp <= o.trigger_price };
        if !hit {
            return Decision::Wait;
        }
        armed = true;
    }
    let marketable = if buy { ltp <= o.price } else { ltp >= o.price };
    if marketable {
        Decision::Fill(if buy { ltp.min(o.price) } else { ltp.max(o.price) })
    } else if armed && !triggered {
        Decision::Trigger
    } else {
        Decision::Wait
    }
}

#[derive(Debug, Clone)]
pub struct Pending {
    pub order: OrderRow,
    pub triggered: bool,
}

/// Open paper orders. Small and short-lived, so a std mutex is fine.
#[derive(Default)]
pub struct PaperBook {
    inner: Mutex<HashMap<String, Pending>>,
}

impl PaperBook {
    pub fn add(&self, order: OrderRow) {
        let triggered = order.status == "TRIGGERED";
        if let Ok(mut m) = self.inner.lock() {
            m.insert(order.id.clone(), Pending { order, triggered });
        }
    }

    pub fn remove(&self, id: &str) {
        if let Ok(mut m) = self.inner.lock() {
            m.remove(id);
        }
    }

    /// Remove and return an order. Only one caller can win, which makes
    /// fills and cancels race-free.
    pub fn take(&self, id: &str) -> Option<Pending> {
        self.inner.lock().ok()?.remove(id)
    }

    pub fn get(&self, id: &str) -> Option<Pending> {
        self.inner.lock().ok()?.get(id).cloned()
    }

    /// Replace the stored order (e.g. after a modify), keeping the trigger state.
    pub fn update(&self, order: OrderRow) {
        if let Ok(mut m) = self.inner.lock() {
            if let Some(p) = m.get_mut(&order.id) {
                p.order = order;
            }
        }
    }

    pub fn set_triggered(&self, id: &str) {
        if let Ok(mut m) = self.inner.lock() {
            if let Some(p) = m.get_mut(id) {
                p.triggered = true;
                p.order.status = "TRIGGERED".into();
            }
        }
    }

    pub fn for_instrument(&self, key: &str) -> Vec<Pending> {
        self.inner
            .lock()
            .map(|m| m.values().filter(|p| p.order.instrument == key).cloned().collect())
            .unwrap_or_default()
    }

    pub fn all(&self) -> Vec<Pending> {
        self.inner.lock().map(|m| m.values().cloned().collect()).unwrap_or_default()
    }

    /// Instrument keys with a pending order (watched for price changes).
    pub fn instruments(&self) -> HashSet<String> {
        self.all().into_iter().map(|p| p.order.instrument).collect()
    }

    /// (symbol, expiry ISO) pairs the background feed must keep priced.
    pub fn contracts(&self) -> HashSet<(String, String)> {
        self.all().into_iter().map(|p| (p.order.symbol, p.order.expiry)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(side: &str, kind: &str, price: f64, trigger: f64) -> OrderRow {
        OrderRow {
            id: "o".into(),
            account_id: "a".into(),
            account_kind: "paper".into(),
            correlation_id: "c".into(),
            broker_order_id: None,
            instrument: "OPT:NIFTY:2099-01-29:24000:CE".into(),
            symbol: "NIFTY".into(),
            expiry: "2099-01-29".into(),
            strike: 24000.0,
            is_call: true,
            security_id: None,
            lot_size: 65,
            side: side.into(),
            product: "INTRADAY".into(),
            order_type: kind.into(),
            validity: "DAY".into(),
            qty: 65,
            price,
            trigger_price: trigger,
            status: "PENDING".into(),
            filled_qty: 0,
            avg_price: 0.0,
            message: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            trade_date: String::new(),
        }
    }

    #[test]
    fn limit_buy_fills_at_the_better_price() {
        let o = order("BUY", "LIMIT", 100.0, 0.0);
        assert_eq!(evaluate(&o, false, 101.0), Decision::Wait);
        assert_eq!(evaluate(&o, false, 100.0), Decision::Fill(100.0));
        assert_eq!(evaluate(&o, false, 97.5), Decision::Fill(97.5));
    }

    #[test]
    fn limit_sell_fills_at_the_better_price() {
        let o = order("SELL", "LIMIT", 100.0, 0.0);
        assert_eq!(evaluate(&o, false, 99.0), Decision::Wait);
        assert_eq!(evaluate(&o, false, 103.0), Decision::Fill(103.0));
    }

    #[test]
    fn stop_loss_sell_arms_then_fills() {
        // Protect a long: trigger 90, limit 88.
        let o = order("SELL", "STOP_LOSS", 88.0, 90.0);
        assert_eq!(evaluate(&o, false, 95.0), Decision::Wait);
        assert_eq!(evaluate(&o, false, 90.0), Decision::Fill(90.0)); // limit 88 is marketable
        // Price gaps through the limit: armed but not fillable.
        assert_eq!(evaluate(&o, false, 85.0), Decision::Trigger);
        assert_eq!(evaluate(&o, true, 85.0), Decision::Wait);
        assert_eq!(evaluate(&o, true, 88.5), Decision::Fill(88.5));
    }

    #[test]
    fn stop_loss_buy_arms_on_rise() {
        // Protect a short: trigger 110, limit 112.
        let o = order("BUY", "STOP_LOSS", 112.0, 110.0);
        assert_eq!(evaluate(&o, false, 105.0), Decision::Wait);
        assert_eq!(evaluate(&o, false, 111.0), Decision::Fill(111.0));
        assert_eq!(evaluate(&o, false, 115.0), Decision::Trigger);
    }

    #[test]
    fn book_tracks_orders_and_contracts() {
        let book = PaperBook::default();
        book.add(order("BUY", "LIMIT", 100.0, 0.0));
        assert_eq!(book.instruments().len(), 1);
        assert_eq!(book.contracts().len(), 1);
        assert_eq!(book.for_instrument("OPT:NIFTY:2099-01-29:24000:CE").len(), 1);
        book.set_triggered("o");
        assert!(book.get("o").unwrap().triggered);
        book.remove("o");
        assert!(book.all().is_empty());
    }
}
