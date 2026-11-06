//! Latest traded price per instrument, fed by whichever stream is running
//! (the option-chain page, or the background feed for open positions).
//!
//! Instrument keys are the ones from `charts::instrument` (`OPT:...`).
//! Keys on the *watch list* (pending paper orders) also push a notification
//! so the paper engine can re-evaluate them immediately.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::market::market_clock;

#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub ltp: f64,
    /// Epoch seconds of the update (NSE-synced clock).
    pub at: i64,
}

pub struct LtpStore {
    quotes: RwLock<HashMap<String, Quote>>,
    watched: RwLock<HashSet<String>>,
    notify: UnboundedSender<String>,
}

impl LtpStore {
    pub fn new() -> (Arc<Self>, UnboundedReceiver<String>) {
        let (tx, rx) = unbounded_channel();
        let store = Self {
            quotes: RwLock::new(HashMap::new()),
            watched: RwLock::new(HashSet::new()),
            notify: tx,
        };
        (Arc::new(store), rx)
    }

    pub fn set(&self, key: &str, ltp: f64) {
        if !ltp.is_finite() || ltp <= 0.0 {
            return;
        }
        let at = market_clock::get_ist_now().timestamp();
        let changed = match self.quotes.write() {
            Ok(mut m) => {
                let changed = m.get(key).map(|q| (q.ltp - ltp).abs() > 1e-9).unwrap_or(true);
                m.insert(key.to_string(), Quote { ltp, at });
                changed
            }
            Err(_) => return,
        };
        if changed && self.watched.read().map(|w| w.contains(key)).unwrap_or(false) {
            let _ = self.notify.send(key.to_string());
        }
    }

    pub fn get(&self, key: &str) -> Option<Quote> {
        self.quotes.read().ok()?.get(key).copied()
    }

    pub fn ltp(&self, key: &str) -> Option<f64> {
        self.get(key).map(|q| q.ltp)
    }

    /// Replace the set of keys that trigger a notification on change.
    pub fn watch(&self, keys: HashSet<String>) {
        if let Ok(mut w) = self.watched.write() {
            *w = keys;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_and_notifies_only_watched_changes() {
        let (store, mut rx) = LtpStore::new();
        store.set("OPT:A", 10.0);
        assert!(rx.try_recv().is_err()); // not watched yet
        store.watch(["OPT:A".to_string()].into_iter().collect());
        store.set("OPT:A", 10.0); // unchanged
        assert!(rx.try_recv().is_err());
        store.set("OPT:A", 11.0);
        assert_eq!(rx.try_recv().unwrap(), "OPT:A");
        store.set("OPT:A", -1.0); // ignored
        assert_eq!(store.ltp("OPT:A"), Some(11.0));
        assert_eq!(store.ltp("OPT:B"), None);
    }
}
