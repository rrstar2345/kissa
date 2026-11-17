//! Headless mode: `kissa --headless` runs every backend service (trading,
//! algo runner, risk, alerts, copier, audit) without a window, on a server or
//! as a background process.
//!
//! * **Same binary.** The Tauri app starts without creating the main window
//!   (`create: false` in `tauri.conf.json`; desktop mode builds it in `setup`).
//!   On a Linux server Tauri still needs the GTK libraries and a display
//!   (Xvfb or similar); macOS and Windows need nothing extra.
//! * **One instance per data folder.** `kissa.lock` holds an exclusive file
//!   lock for the life of the process (desktop and headless alike).
//! * **Logs** go to `kissa-headless.log` in the data folder (10 MB, one
//!   rotated copy `.1`).
//! * **Feeds on demand.** `spawn_demand` works out every 5 s what the running
//!   algos and orders need. No armed algo: no index stream, no header seeds.
//!   Nothing at all to watch (no armed algo, open leg, open position or
//!   pending order): no market-status polling or NSE time sync either. The
//!   price feed (`trading::feed`) was already demand driven and is unchanged.
//!   The index feed stays on while an algo is armed, because the algo reads
//!   the underlying's bars for its strike. Nothing is emitted to a UI.
//! * **Control** is a loopback-only HTTP API with a bearer token (`control`).
//! * **Shutdown** on Ctrl-C, SIGTERM or `POST /v1/shutdown`: recorder flush,
//!   bars checkpoint, audit entry, then exit. Armed algos stay armed.

pub mod control;

use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tracing::{info, warn};

use crate::state::SharedState;

pub const DEFAULT_CONTROL_PORT: u16 = 8787;
const LOG_NAME: &str = "kissa-headless.log";
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

static HEADLESS: AtomicBool = AtomicBool::new(false);
/// An armed algo needs the underlying's index bars.
static WANT_INDICES: AtomicBool = AtomicBool::new(false);
/// Something (algo, leg, position, order) needs market data at all.
static WANT_FEEDS: AtomicBool = AtomicBool::new(false);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static STARTED: OnceLock<Instant> = OnceLock::new();
static LOCK: OnceLock<std::fs::File> = OnceLock::new();

pub fn is_headless() -> bool {
    HEADLESS.load(Ordering::Relaxed)
}

/// Should the index feed (NSE stream / Fyers header topics) run? Always on a desktop.
pub fn indices_wanted() -> bool {
    !is_headless() || WANT_INDICES.load(Ordering::Relaxed)
}

/// Should exchange status and clock sync be polled? Always on a desktop.
pub fn feeds_wanted() -> bool {
    !is_headless() || WANT_FEEDS.load(Ordering::Relaxed)
}

/// Resolves when the index feed is no longer wanted (never on a desktop).
pub async fn indices_lost() {
    if !is_headless() {
        std::future::pending::<()>().await;
    }
    while WANT_INDICES.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub fn uptime_secs() -> u64 {
    STARTED.get().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

pub fn demand() -> (bool, bool) {
    (WANT_INDICES.load(Ordering::Relaxed), WANT_FEEDS.load(Ordering::Relaxed))
}

// ------------------------------------------------------------- arguments ---

#[derive(Debug, Clone)]
pub struct Args {
    pub headless: bool,
    /// `None` = control API off.
    pub control_port: Option<u16>,
    pub show_token: bool,
}

/// Flags: `--headless`, `--control-port <n>`, `--no-control`, `--show-control-token`.
/// Environment: `KISSA_HEADLESS=1`, `KISSA_CONTROL_PORT`. Unknown flags are ignored.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Args {
    let mut out = Args {
        headless: std::env::var("KISSA_HEADLESS").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false),
        control_port: Some(
            std::env::var("KISSA_CONTROL_PORT").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_CONTROL_PORT),
        ),
        show_token: false,
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--headless" => out.headless = true,
            "--no-control" => out.control_port = None,
            "--show-control-token" => out.show_token = true,
            "--control-port" => {
                if let Some(p) = it.next().and_then(|v| v.trim().parse().ok()) {
                    out.control_port = Some(p);
                }
            }
            other => {
                if let Some(p) = other.strip_prefix("--control-port=").and_then(|v| v.trim().parse().ok()) {
                    out.control_port = Some(p);
                }
            }
        }
    }
    out
}

/// Set once, before anything reads `is_headless`.
pub fn set_mode(headless: bool) {
    HEADLESS.store(headless, Ordering::Relaxed);
    let _ = STARTED.set(Instant::now());
}

// ---------------------------------------------------------------- logging ---

pub fn init_logging(headless: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "kissa_lib=info".into());
    if headless {
        match open_log() {
            Ok(file) => {
                tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(Mutex::new(file)).init();
                std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));
                return;
            }
            Err(e) => eprintln!("Could not open the log file ({e}); logging to stderr"),
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn open_log() -> Result<std::fs::File> {
    let root = crate::settings::setup_app_folders()?.root;
    let path = root.join(LOG_NAME);
    if std::fs::metadata(&path).map(|m| m.len() > LOG_MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, root.join(format!("{LOG_NAME}.1")));
    }
    Ok(std::fs::OpenOptions::new().create(true).append(true).open(path)?)
}

// ------------------------------------------------------------------- lock ---

/// Take the exclusive lock of this data folder (held until the process exits).
pub fn acquire_lock(root: &Path) -> Result<()> {
    use std::io::Write;
    let path = root.join("kissa.lock");
    let mut f = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&path)?;
    match f.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(anyhow!(
                "Another Kissa instance is already using {}. Close it first (only one may use a data folder).",
                root.display()
            ));
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(anyhow!("could not lock {}: {e}", path.display())),
    }
    let _ = f.set_len(0);
    let _ = write!(f, "{}", std::process::id());
    let _ = LOCK.set(f);
    Ok(())
}

// ----------------------------------------------------------------- demand ---

async fn compute_demand(state: &SharedState) -> (bool, bool) {
    let states = crate::algo::store::list_states(&state.db).await.unwrap_or_default();
    let armed = states.values().any(|s| s.armed);
    let legs = states.values().any(|s| s.in_position());
    let watched = !state.trading.watched_contracts().is_empty();
    let positions = crate::trading::store::open_positions(&state.db, None, &crate::trading::today_ist())
        .await
        .map(|p| !p.is_empty())
        .unwrap_or(false);
    (armed, armed || legs || watched || positions)
}

/// Keeps the demand flags current (headless only).
pub fn spawn_demand(state: SharedState) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last: Option<(bool, bool)> = None;
        loop {
            tick.tick().await;
            let now = compute_demand(&state).await;
            WANT_INDICES.store(now.0, Ordering::Relaxed);
            WANT_FEEDS.store(now.1, Ordering::Relaxed);
            if last != Some(now) {
                last = Some(now);
                info!(
                    "Headless feeds: index feed {}, market polling {}",
                    if now.0 { "on (armed algo)" } else { "off" },
                    if now.1 { "on" } else { "off (nothing to watch)" }
                );
            }
        }
    });
}

// --------------------------------------------------------------- shutdown ---

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term.as_mut() {
                    Some(t) => { t.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Stop on Ctrl-C / SIGTERM (headless only).
pub fn spawn_signal_handler(state: SharedState, app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        wait_for_signal().await;
        shutdown(&state, &app, "signal").await;
    });
}

/// Flush what is buffered, record the stop and exit. Safe to call twice.
pub async fn shutdown(state: &SharedState, app: &AppHandle, why: &str) {
    if SHUTTING_DOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    info!("Shutting down ({why})");
    crate::audit::record(crate::audit::Entry::new(
        crate::audit::Category::System,
        "app_stop",
        format!("Kissa stopped ({why}, up {} s)", uptime_secs()),
    ));
    state.recorder.flush_now().await;
    if let Err(e) = state.bars.run(|conn| Ok(conn.execute_batch("CHECKPOINT")?)).await {
        warn!("Bars checkpoint failed at shutdown: {e}");
    }
    // Give the audit worker a moment to write the stop entry.
    tokio::time::sleep(Duration::from_millis(500)).await;
    state.db.close().await;
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Args {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn flags_are_parsed() {
        let a = args(&["--headless", "--control-port", "9100"]);
        assert!(a.headless);
        assert_eq!(a.control_port, Some(9100));
        assert_eq!(args(&["--control-port=9200"]).control_port, Some(9200));
        assert_eq!(args(&["--no-control"]).control_port, None);
        assert!(args(&["--show-control-token"]).show_token);
        // Unknown flags and a bad port are ignored.
        let b = args(&["--whatever", "--control-port", "abc"]);
        assert_eq!(b.control_port, Some(DEFAULT_CONTROL_PORT));
    }

    #[test]
    fn instance_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("kissa-lock-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // A second handle on the same file cannot take the lock while the first holds it.
        let first = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(dir.join("kissa.lock")).unwrap();
        first.try_lock().unwrap();
        let second = std::fs::OpenOptions::new().read(true).write(true).open(dir.join("kissa.lock")).unwrap();
        assert!(matches!(second.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        drop(first);
        let _ = std::fs::remove_dir_all(dir);
    }
}
