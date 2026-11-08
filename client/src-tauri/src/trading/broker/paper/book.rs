//! Open paper orders, in memory (and in SQLite via the order rows).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::trading::OrderRow;

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

    /// Replace the stored order (e.g. after a modify), keeping the trigger
    /// state. Returns false if the order is no longer in the book.
    pub fn update(&self, order: OrderRow) -> bool {
        match self.inner.lock() {
            Ok(mut m) => match m.get_mut(&order.id) {
                Some(p) => {
                    p.order = order;
                    true
                }
                None => false,
            },
            Err(_) => false,
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
pub fn sample_order(side: &str, kind: &str, price: f64, trigger: f64) -> OrderRow {
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
        group_id: String::new(),
        role: String::new(),
        bracket: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_tracks_orders_and_contracts() {
        let book = PaperBook::default();
        book.add(sample_order("BUY", "LIMIT", 100.0, 0.0));
        assert_eq!(book.instruments().len(), 1);
        assert_eq!(book.contracts().len(), 1);
        assert_eq!(book.for_instrument("OPT:NIFTY:2099-01-29:24000:CE").len(), 1);
        book.set_triggered("o");
        assert!(book.get("o").unwrap().triggered);
        assert!(book.update(sample_order("BUY", "LIMIT", 101.0, 0.0)));
        assert!(book.get("o").unwrap().triggered); // trigger state survives a modify
        book.remove("o");
        assert!(book.all().is_empty());
        assert!(!book.update(sample_order("BUY", "LIMIT", 99.0, 0.0)));
    }

    #[test]
    fn take_hands_the_order_to_one_caller() {
        let book = PaperBook::default();
        book.add(sample_order("SELL", "LIMIT", 100.0, 0.0));
        assert!(book.take("o").is_some());
        assert!(book.take("o").is_none());
    }
}
