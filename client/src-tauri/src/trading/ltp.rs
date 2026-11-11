//! Latest traded price per instrument, fed by whichever stream is running
//! (the option-chain page, or the background feed for open positions).
//!
//! Instrument keys are the ones from `charts::instrument` (`OPT:...`).
//! Keys on the *watch list* (pending paper orders) also push a notification
//! so the paper engine can re-evaluate them immediately.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

use crate::market::market_clock;

#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub ltp: f64,
    /// Epoch seconds of the update (NSE-synced clock).
    pub at: i64,
}

/// One changed price, as pushed to the UI on the `quotes` event.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuoteTick {
    /// Instrument key.
    pub k: String,
    /// Last traded price.
    pub p: f64,
    /// Epoch seconds of the update.
    pub t: i64,
}

pub struct LtpStore {
    quotes: RwLock<HashMap<String, Quote>>,
    watched: RwLock<HashSet<String>>,
    notify: UnboundedSender<String>,
    /// Prices that changed since the last `drain_changed` (latest per key),
    /// so the UI gets one batched event instead of one per tick.
    changed: Mutex<HashMap<String, Quote>>,
    /// Woken when a price changes, so the UI emitter sleeps while idle.
    ui_wake: Arc<Notify>,
}

impl LtpStore {
    pub fn new() -> (Arc<Self>, UnboundedReceiver<String>) {
        let (tx, rx) = unbounded_channel();
        let store = Self {
            quotes: RwLock::new(HashMap::new()),
            watched: RwLock::new(HashSet::new()),
            notify: tx,
            changed: Mutex::new(HashMap::new()),
            ui_wake: Arc::new(Notify::new()),
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
        if changed {
            if let Ok(mut c) = self.changed.lock() {
                c.insert(key.to_string(), Quote { ltp, at });
            }
            self.ui_wake.notify_one();
            if self.watched.read().map(|w| w.contains(key)).unwrap_or(false) {
                let _ = self.notify.send(key.to_string());
            }
        }
    }

    /// Signalled whenever a price changes (a permit is kept if nobody waits).
    pub fn ui_wake(&self) -> Arc<Notify> {
        self.ui_wake.clone()
    }

    /// Take the prices that changed since the last call (latest per key).
    pub fn drain_changed(&self) -> Vec<QuoteTick> {
        let Ok(mut c) = self.changed.lock() else { return vec![] };
        c.drain().map(|(k, q)| QuoteTick { k, p: q.ltp, t: q.at }).collect()
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

    #[tokio::test]
    async fn changed_prices_are_batched_for_the_ui() {
        let (store, _rx) = LtpStore::new();
        store.set("OPT:A", 10.0);
        store.set("OPT:A", 10.5); // only the latest per key is kept
        store.set("OPT:B", 7.0);
        store.set("OPT:B", 7.0); // unchanged: not queued again
        let mut batch = store.drain_changed();
        batch.sort_by(|a, b| a.k.cmp(&b.k));
        assert_eq!(batch.len(), 2);
        assert_eq!((batch[0].k.as_str(), batch[0].p), ("OPT:A", 10.5));
        assert_eq!((batch[1].k.as_str(), batch[1].p), ("OPT:B", 7.0));
        assert!(store.drain_changed().is_empty());
        store.set("OPT:A", 10.5); // same price again: nothing to push
        assert!(store.drain_changed().is_empty());
    }
}
