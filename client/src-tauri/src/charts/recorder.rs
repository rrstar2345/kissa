//! Folds live ticks into in-memory 1m bars, pushes changed bars to the UI
//! four times a second (`bar-update` event) and flushes to SQLite every few
//! seconds.
//! Only trades inside the 09:15-15:30 IST session are recorded.

use chrono::{DateTime, Datelike, Timelike, Weekday};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::Notify;
use tracing::warn;

use super::{store, Bar};

const IST: i64 = 5 * 3600 + 30 * 60;

struct Live {
    bar: Bar,
    last_cum: Option<f64>,
    dirty: bool,
}

#[derive(Default)]
struct Inner {
    live: HashMap<String, Live>,
    /// Bars that rolled over and still need to be sent to the UI.
    finished: Vec<(String, Bar)>,
    /// Bars waiting for the next DB flush (latest version per bar).
    unsaved: HashMap<(String, i64), Bar>,
}

pub struct Recorder {
    pool: Option<SqlitePool>,
    inner: Mutex<Inner>,
    /// Woken when a bar changes, so the UI emitter sleeps while idle.
    wake: Notify,
    /// Instrument keys that are always recorded, even outside the ATM window
    /// (contracts held in an open position, so their chart has bars).
    pinned: RwLock<HashSet<String>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BarEvent {
    instrument: String,
    #[serde(flatten)]
    bar: Bar,
}

pub fn in_session(ts: i64) -> bool {
    let Some(t) = DateTime::from_timestamp(ts + IST, 0) else { return false };
    if matches!(t.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    let m = t.hour() * 60 + t.minute();
    (9 * 60 + 15..15 * 60 + 30).contains(&m)
}

impl Recorder {
    pub fn new(pool: SqlitePool) -> Arc<Self> {
        Arc::new(Self {
            pool: Some(pool),
            inner: Mutex::new(Inner::default()),
            wake: Notify::new(),
            pinned: RwLock::default(),
        })
    }

    /// Replace the set of instrument keys that are recorded regardless of the
    /// strike window (see `pinned`).
    pub fn set_pinned(&self, keys: HashSet<String>) {
        if let Ok(mut p) = self.pinned.write() {
            *p = keys;
        }
    }

    /// True when `instrument` is pinned and must be recorded.
    pub fn is_pinned(&self, instrument: &str) -> bool {
        self.pinned.read().map(|p| p.contains(instrument)).unwrap_or(false)
    }

    /// Record one observation. `cum_volume` is the day's cumulative traded
    /// volume (the per-bar volume is derived from its increments).
    pub fn record(&self, instrument: &str, ts: i64, price: f64, cum_volume: Option<f64>, oi: Option<f64>) {
        if !price.is_finite() || price <= 0.0 || !in_session(ts) {
            return;
        }
        let minute = ts - ts.rem_euclid(60);
        let Ok(mut guard) = self.inner.lock() else { return };
        let inner = &mut *guard;

        if !inner.live.contains_key(instrument) {
            inner.live.insert(
                instrument.to_string(),
                Live { bar: Bar::new(minute, price), last_cum: None, dirty: true },
            );
        }
        let Some(live) = inner.live.get_mut(instrument) else { return };
        if minute < live.bar.ts {
            return; // out-of-order tick
        }

        let delta = match (cum_volume, live.last_cum) {
            (Some(c), Some(p)) if c >= p => c - p,
            _ => 0.0,
        };
        if cum_volume.is_some() {
            live.last_cum = cum_volume;
        }

        if minute > live.bar.ts {
            let done = std::mem::replace(&mut live.bar, Bar::new(minute, price));
            live.bar.oi = done.oi;
            inner.finished.push((instrument.to_string(), done.clone()));
            inner.unsaved.insert((instrument.to_string(), done.ts), done);
        } else {
            live.bar.high = live.bar.high.max(price);
            live.bar.low = live.bar.low.min(price);
            live.bar.close = price;
        }
        live.bar.volume += delta;
        if oi.is_some() {
            live.bar.oi = oi;
        }
        live.dirty = true;
        drop(guard);
        self.wake.notify_one();
    }

    /// Collect bars changed since the last call (for the UI) and stage them
    /// for the next DB flush.
    fn stage(&self) -> Vec<BarEvent> {
        let Ok(mut guard) = self.inner.lock() else { return vec![] };
        let inner = &mut *guard;
        let mut out: Vec<BarEvent> = inner
            .finished
            .drain(..)
            .map(|(instrument, bar)| BarEvent { instrument, bar })
            .collect();
        for (k, l) in inner.live.iter_mut().filter(|(_, l)| l.dirty) {
            l.dirty = false;
            inner.unsaved.insert((k.clone(), l.bar.ts), l.bar.clone());
            out.push(BarEvent { instrument: k.clone(), bar: l.bar.clone() });
        }
        out
    }

    /// Write everything in memory (including the in-progress bars) to the DB
    /// so a chart query sees the latest data.
    pub async fn flush_now(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            let inner = &mut *guard;
            for (k, l) in inner.live.iter() {
                inner.unsaved.insert((k.clone(), l.bar.ts), l.bar.clone());
            }
        }
        let batch = self.take_unsaved();
        if let (false, Some(pool)) = (batch.is_empty(), self.pool.as_ref()) {
            if let Err(e) = store::upsert_live(pool, &batch).await {
                warn!("Bar flush failed ({} bars): {e}", batch.len());
                self.requeue(batch);
            }
        }
    }

    fn take_unsaved(&self) -> Vec<(String, Bar)> {
        let Ok(mut guard) = self.inner.lock() else { return vec![] };
        guard.unsaved.drain().map(|((k, _), b)| (k, b)).collect()
    }

    fn requeue(&self, batch: Vec<(String, Bar)>) {
        if let Ok(mut guard) = self.inner.lock() {
            for (k, b) in batch {
                guard.unsaved.entry((k, b.ts)).or_insert(b);
            }
        }
    }

    pub fn spawn(self: Arc<Self>, app: AppHandle, flush_secs: u64) {
        tauri::async_runtime::spawn(async move {
            // UI pushes are event-driven: sleep until a bar changes, then wait
            // 250 ms so ticks coalesce into one `bar-update`. The DB flush
            // keeps its own seconds-based timer.
            let mut flush = tokio::time::interval(Duration::from_secs(flush_secs.max(1)));
            flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            flush.tick().await;
            loop {
                tokio::select! {
                    _ = self.wake.notified() => {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let events = self.stage();
                        if !events.is_empty() {
                            crate::emit_stats::count("bar-update");
                            let _ = app.emit("bar-update", events);
                        }
                    }
                    _ = flush.tick() => {
                        let batch = self.take_unsaved();
                        if let (false, Some(pool)) = (batch.is_empty(), self.pool.as_ref()) {
                            if let Err(e) = store::upsert_live(pool, &batch).await {
                                warn!("Bar flush failed ({} bars), will retry: {e}", batch.len());
                                self.requeue(batch);
                            }
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-05 (Thursday) 09:15 IST.
    const OPEN: i64 = 1_793_850_300;

    fn rec() -> Recorder {
        Recorder {
            pool: None,
            inner: Mutex::new(Inner::default()),
            wake: Notify::new(),
            pinned: RwLock::default(),
        }
    }

    #[test]
    fn session_window() {
        assert!(in_session(OPEN));
        assert!(!in_session(OPEN - 1));
        assert!(!in_session(OPEN + 6 * 3600 + 15 * 60)); // 15:30
    }

    #[test]
    fn pinned_keys_are_replaced_not_merged() {
        let r = rec();
        assert!(!r.is_pinned("OPT:A"));
        r.set_pinned(HashSet::from(["OPT:A".to_string()]));
        assert!(r.is_pinned("OPT:A"));
        r.set_pinned(HashSet::from(["OPT:B".to_string()]));
        assert!(!r.is_pinned("OPT:A"));
        assert!(r.is_pinned("OPT:B"));
    }

    #[test]
    fn builds_bars_and_volume_deltas() {
        let r = rec();
        r.record("OPT:X", OPEN + 5, 100.0, Some(1000.0), Some(50.0));
        r.record("OPT:X", OPEN + 20, 105.0, Some(1040.0), None);
        r.record("OPT:X", OPEN + 40, 98.0, Some(1050.0), None);
        r.record("OPT:X", OPEN + 65, 99.0, Some(1060.0), Some(55.0)); // next minute
        r.record("OPT:X", OPEN - 600, 1.0, None, None); // out of session: ignored

        let ev = r.stage();
        let first = ev.iter().find(|e| e.bar.ts == OPEN).unwrap();
        assert_eq!((first.bar.open, first.bar.high, first.bar.low, first.bar.close), (100.0, 105.0, 98.0, 98.0));
        assert_eq!(first.bar.volume, 50.0);
        let second = ev.iter().find(|e| e.bar.ts == OPEN + 60).unwrap();
        assert_eq!(second.bar.open, 99.0);
        assert_eq!(second.bar.volume, 10.0);
        assert_eq!(second.bar.oi, Some(55.0));
        assert_eq!(r.take_unsaved().len(), 2);
    }
}
