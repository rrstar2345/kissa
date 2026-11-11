//! Optional debug counter for UI events. Off by default; start the app with
//! `KISSA_EMIT_STATS=1` to log emits per second per event every 10 s, so CPU
//! and event rates can be compared before and after a change.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static ENABLED: AtomicBool = AtomicBool::new(false);
static COUNTS: OnceLock<Mutex<HashMap<&'static str, u64>>> = OnceLock::new();

/// Count one emit of `event`. A single atomic load when the counter is off.
pub fn count(event: &'static str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut m) = COUNTS.get_or_init(Default::default).lock() {
        *m.entry(event).or_insert(0) += 1;
    }
}

/// Start the periodic log when `KISSA_EMIT_STATS` is set (to anything but 0).
pub fn spawn() {
    if !std::env::var("KISSA_EMIT_STATS").map(|v| v != "0" && !v.is_empty()).unwrap_or(false) {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    tauri::async_runtime::spawn(async move {
        const SECS: u64 = 10;
        let mut tick = tokio::time::interval(Duration::from_secs(SECS));
        tick.tick().await;
        loop {
            tick.tick().await;
            let taken = COUNTS
                .get_or_init(Default::default)
                .lock()
                .map(|mut m| std::mem::take(&mut *m))
                .unwrap_or_default();
            let mut rows: Vec<_> = taken.into_iter().collect();
            rows.sort();
            let line = rows
                .iter()
                .map(|(k, n)| format!("{k}={:.1}/s", *n as f64 / SECS as f64))
                .collect::<Vec<_>>()
                .join(" ");
            tracing::info!("UI emits: {}", if line.is_empty() { "none".into() } else { line });
        }
    });
}
