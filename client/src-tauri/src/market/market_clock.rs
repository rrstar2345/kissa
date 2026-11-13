//! Market-hours awareness, driven by NSE's own IST clock rather than the
//! local system clock (so this works correctly regardless of which region
//! the server is deployed in), and by NSE's market status (see `status.rs`).
//!
//! Two session modes:
//! - `Active`: inside the buffered trading window (8:30 AM - 4:00 PM IST,
//!   Mon-Fri) and NSE reports the market Open or Pre-open. When the status is
//!   unknown (never checked, or the status API is failing) the old rule
//!   applies: Active until `inactive_switch_after_secs` pass without a real tick.
//! - `Idle`: outside the window, or NSE reports it closed (a holiday, or after
//!   the close). With a known-closed market the streamers do not connect at all.
//!
//! The indices stream follows `mode()` (index values exist during pre-open);
//! option streams follow `options_mode()` (options trade from 9:15 only).
//!
//! This is the *NSE* session. It applies only while NSE is the selected data
//! source: with Fyers selected the mode is always `Idle` and no NSE stream
//! connects at all (`nse_enabled`). Fyers has its own socket supervisor
//! (`fyers_socket`).

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, NaiveDateTime, Timelike, Utc};
use serde::Deserialize;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::sync::{watch, RwLock};
use tracing::warn;

use crate::market::http;
use crate::market::status::{DataSource, MarketState, SharedMarketStatus, SourceStatus};
use crate::settings::AppConfig;

#[derive(Debug, Deserialize)]
struct CurrentTimeResponse {
    data: CurrentTimeData,
}

#[derive(Debug, Deserialize)]
struct CurrentTimeData {
    #[serde(rename = "currentTime")]
    current_time: String,
}

/// Cached offset between NSE's reported IST time and this process's local
/// `Utc::now()`, stored as milliseconds (NSE_IST_as_UTC - Utc::now()) so we
/// can cheaply derive "current NSE time" between refreshes without hitting
/// the API on every check. Guarded by a coarse staleness check in
/// `get_ist_now`.
static OFFSET_MS: AtomicI64 = AtomicI64::new(i64::MIN);
static LAST_REFRESH_UNIX_MS: AtomicI64 = AtomicI64::new(0);

/// How often to re-sync with the NSE time endpoint. The offset should be
/// essentially constant (modulo NTP drift), so this doesn't need to be
/// frequent, but refreshing periodically protects against clock drift on
/// long-running processes.
const REFRESH_INTERVAL_MS: i64 = 15 * 60 * 1000;

/// IST is UTC+5:30, fixed year-round (no DST).
pub(crate) const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;

async fn fetch_nse_ist_time(config: &AppConfig) -> Result<DateTime<Utc>> {
    let url = &config.system.current_time.base;

    let response = http::get(url)
        .send()
        .await
        .map_err(|e| anyhow!("Failed to fetch NSE current time: {}", e))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| anyhow!("Failed to read NSE current time response body: {}", e))?;

    if !status.is_success() {
        return Err(anyhow!(
            "NSE current time request failed with status {}: {}",
            status,
            &body[..body.len().min(300)]
        ));
    }

    let parsed: CurrentTimeResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow!("Failed to parse NSE current time response: {}", e))?;

    // "2026-07-20 11:05:08" is naive IST wall-clock time; convert to UTC by
    // subtracting the fixed 5:30 offset.
    let naive = NaiveDateTime::parse_from_str(&parsed.data.current_time, "%Y-%m-%d %H:%M:%S")
        .map_err(|e| anyhow!("Failed to parse NSE current time value '{}': {}", parsed.data.current_time, e))?;

    let as_utc = naive - chrono::Duration::seconds(IST_OFFSET_SECS);
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(as_utc, Utc))
}

/// Refresh the cached NSE/local offset. Safe to call frequently; internally
/// only hits the network if the cache is stale or uninitialized.
pub async fn refresh_if_stale(config: &AppConfig) {
    let now_ms = Utc::now().timestamp_millis();
    let last = LAST_REFRESH_UNIX_MS.load(Ordering::Relaxed);
    let initialized = OFFSET_MS.load(Ordering::Relaxed) != i64::MIN;

    if initialized && now_ms - last < REFRESH_INTERVAL_MS {
        return;
    }

    match fetch_nse_ist_time(config).await {
        Ok(nse_utc) => {
            let offset = nse_utc.timestamp_millis() - now_ms;
            OFFSET_MS.store(offset, Ordering::Relaxed);
            LAST_REFRESH_UNIX_MS.store(now_ms, Ordering::Relaxed);
        }
        Err(e) => {
            warn!("Failed to refresh NSE time offset: {} (using previous offset/local clock)", e);
        }
    }
}

/// Current time, as best known from NSE (falls back to local UTC if the NSE
/// endpoint has never been reached successfully yet).
pub fn get_ist_now() -> DateTime<Utc> {
    let offset = OFFSET_MS.load(Ordering::Relaxed);
    let now = Utc::now();
    if offset == i64::MIN {
        now
    } else {
        now + chrono::Duration::milliseconds(offset)
    }
}

/// True if `t` (a UTC instant, interpreted as IST for wall-clock purposes)
/// falls within the buffered trading window: 8:30 AM - 4:00 PM IST, Mon-Fri.
/// The clock cannot know exchange holidays: see `status.rs` for that.
pub fn is_within_trading_window(t: DateTime<Utc>) -> bool {
    let ist = t + chrono::Duration::seconds(IST_OFFSET_SECS);

    // Weekday check (Mon-Fri).
    use chrono::Weekday::*;
    match ist.weekday() {
        Sat | Sun => return false,
        _ => {}
    }

    let minutes_since_midnight = ist.hour() as i64 * 60 + ist.minute() as i64;
    let window_start = 8 * 60 + 30; // 8:30 AM
    let window_end = 16 * 60; // 4:00 PM

    minutes_since_midnight >= window_start && minutes_since_midnight < window_end
}

// ============================================================================
// SESSION MODE
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    Active,
    Idle,
}

impl SessionMode {
    pub fn label(&self) -> &'static str {
        match self {
            SessionMode::Active => "ACTIVE",
            SessionMode::Idle => "IDLE",
        }
    }
}

/// Shared, process-wide session mode plus the timestamp of the last
/// "real" (non-heartbeat) tick seen by any stream. Used by streamers to
/// decide whether to hold an open WSS connection (Active) or stay idle.
pub struct SessionState {
    inner: RwLock<SessionInner>,
    status: SharedMarketStatus,
}

struct SessionInner {
    mode: SessionMode,
    last_activity_at: DateTime<Utc>,
    /// Set once the current trading window has been tried. Stops an
    /// unknown-status Idle from being flipped straight back to Active.
    entered_window: bool,
}

/// The mode for one evaluation. Pure, so it can be tested.
///
/// `quiet_secs` = seconds since the last real tick; `entered` = see
/// `SessionInner::entered_window` (updated here).
fn next_mode(
    in_window: bool,
    nse: MarketState,
    cur: SessionMode,
    quiet_secs: i64,
    inactive_switch_after_secs: i64,
    entered: &mut bool,
) -> SessionMode {
    if !in_window {
        *entered = false;
        return SessionMode::Idle;
    }
    match nse {
        MarketState::Open | MarketState::PreOpen => {
            *entered = true;
            SessionMode::Active
        }
        MarketState::Closed => {
            *entered = true;
            SessionMode::Idle
        }
        // No answer from the status API: the old behaviour. Try Active once
        // when the window opens, drop to Idle after a long quiet spell, and
        // come back only through a real tick seen by an idle poll.
        MarketState::Unknown => {
            if cur == SessionMode::Active {
                if quiet_secs > inactive_switch_after_secs {
                    SessionMode::Idle
                } else {
                    SessionMode::Active
                }
            } else if !*entered {
                *entered = true;
                SessionMode::Active
            } else {
                SessionMode::Idle
            }
        }
    }
}

impl SessionState {
    pub fn new(status: SharedMarketStatus) -> Self {
        Self {
            inner: RwLock::new(SessionInner {
                mode: SessionMode::Idle,
                last_activity_at: get_ist_now(),
                entered_window: false,
            }),
            status,
        }
    }

    /// True while NSE is the selected data source. With Fyers selected the
    /// NSE streams neither connect nor poll, whatever the clock says.
    pub fn nse_enabled(&self) -> bool {
        self.status.source() == DataSource::Nse
    }

    /// Mode for the indices stream: Active through pre-open and the session.
    pub async fn mode(&self) -> SessionMode {
        if !self.nse_enabled() {
            return SessionMode::Idle;
        }
        self.inner.read().await.mode
    }

    /// Mode for option streams: like `mode()`, but Idle during pre-open,
    /// because options only trade from 9:15.
    pub async fn options_mode(&self) -> SessionMode {
        if self.status.nse_state() == MarketState::PreOpen {
            SessionMode::Idle
        } else {
            self.mode().await
        }
    }

    pub fn subscribe_nse(&self) -> watch::Receiver<SourceStatus> {
        self.status.subscribe_nse()
    }

    /// True when there is nothing to stream, so an idle stream should not
    /// even connect: NSE is not the selected data source, or it has said the
    /// market is closed, or (for option streams) it is still in pre-open.
    pub fn known_idle(&self, options: bool) -> bool {
        if !self.nse_enabled() {
            return true;
        }
        match self.status.nse_state() {
            MarketState::Closed => true,
            MarketState::PreOpen => options,
            MarketState::Open | MarketState::Unknown => false,
        }
    }

    /// Record a real (non-heartbeat) tick. Switches to Active if we were
    /// Idle (e.g. an off-hours poll unexpectedly saw live data), unless NSE
    /// has said the market is closed or is not the selected data source.
    pub async fn record_activity(&self) {
        let mut inner = self.inner.write().await;
        inner.last_activity_at = get_ist_now();
        if self.nse_enabled() && self.status.nse_state() != MarketState::Closed {
            inner.mode = SessionMode::Active;
        }
    }

    /// Re-evaluate the mode given the current time and status. Called
    /// periodically by a supervisor task. Returns the (possibly updated) mode.
    pub async fn tick(&self, inactive_switch_after_secs: i64) -> SessionMode {
        self.recompute(self.status.nse_state(), inactive_switch_after_secs).await
    }

    /// Apply a new NSE status: update the mode first, then publish, so a
    /// stream woken by the change already sees the right mode.
    pub async fn update_nse(&self, status: SourceStatus, inactive_switch_after_secs: i64) {
        self.recompute(status.state, inactive_switch_after_secs).await;
        self.status.publish_nse(status);
    }

    async fn recompute(&self, nse: MarketState, inactive_switch_after_secs: i64) -> SessionMode {
        let now = get_ist_now();
        let enabled = self.nse_enabled();
        let mut guard = self.inner.write().await;
        let inner = &mut *guard;
        let quiet = now.signed_duration_since(inner.last_activity_at).num_seconds();
        // Fyers selected: the NSE session is off, as if outside the trading window.
        let next = next_mode(
            enabled && is_within_trading_window(now),
            nse,
            inner.mode,
            quiet,
            inactive_switch_after_secs,
            &mut inner.entered_window,
        );
        if next == SessionMode::Active && inner.mode == SessionMode::Idle {
            // Starting fresh: the quiet timer counts from now.
            inner.last_activity_at = now;
        }
        inner.mode = next;
        next
    }
}

pub type SharedSessionState = std::sync::Arc<SessionState>;

pub fn new_shared_session_state(status: SharedMarketStatus) -> SharedSessionState {
    std::sync::Arc::new(SessionState::new(status))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;

    #[test]
    fn outside_the_window_is_always_idle() {
        let mut entered = true;
        for s in [MarketState::Open, MarketState::PreOpen, MarketState::Closed, MarketState::Unknown] {
            assert_eq!(next_mode(false, s, SessionMode::Active, 0, HOUR, &mut entered), SessionMode::Idle);
            assert!(!entered, "leaving the window resets the one-time attempt");
            entered = true;
        }
    }

    #[test]
    fn exchange_status_decides_inside_the_window() {
        let mut entered = false;
        let mut run = |s, cur| next_mode(true, s, cur, 99_999, HOUR, &mut entered);
        // Open and pre-open stream, however long it has been quiet.
        assert_eq!(run(MarketState::Open, SessionMode::Idle), SessionMode::Active);
        assert_eq!(run(MarketState::PreOpen, SessionMode::Idle), SessionMode::Active);
        // Closed (a holiday, or after the close) idles.
        assert_eq!(run(MarketState::Closed, SessionMode::Active), SessionMode::Idle);
    }

    #[test]
    fn a_holiday_stays_idle() {
        // The old bug: Active -> Idle after an hour of quiet, then straight back
        // to Active on the next supervisor tick, all day. With NSE saying Closed
        // it must stay Idle on every tick.
        let mut entered = false;
        let mut mode = SessionMode::Idle;
        for _ in 0..10 {
            mode = next_mode(true, MarketState::Closed, mode, 0, HOUR, &mut entered);
            assert_eq!(mode, SessionMode::Idle);
        }
    }

    #[tokio::test]
    async fn fyers_as_the_source_switches_the_nse_session_off() {
        use crate::market::status::MarketStatusService;
        let session = SessionState::new(MarketStatusService::new(DataSource::Fyers));
        assert!(!session.nse_enabled());
        assert_eq!(session.tick(HOUR).await, SessionMode::Idle);
        // Nothing connects, not even for the idle price seed, and a stray tick cannot wake it.
        assert!(session.known_idle(false));
        assert!(session.known_idle(true));
        session.record_activity().await;
        assert_eq!(session.mode().await, SessionMode::Idle);
        assert_eq!(session.options_mode().await, SessionMode::Idle);
    }

    #[test]
    fn unknown_status_keeps_the_old_rules_without_flip_flopping() {
        let mut entered = false;
        // The window opens (or the app starts inside it): one try as Active.
        let mut mode = next_mode(true, MarketState::Unknown, SessionMode::Idle, 0, HOUR, &mut entered);
        assert_eq!(mode, SessionMode::Active);
        // Still quiet but under the limit: stays Active.
        mode = next_mode(true, MarketState::Unknown, mode, HOUR - 1, HOUR, &mut entered);
        assert_eq!(mode, SessionMode::Active);
        // Quiet past the limit: Idle...
        mode = next_mode(true, MarketState::Unknown, mode, HOUR + 1, HOUR, &mut entered);
        assert_eq!(mode, SessionMode::Idle);
        // ...and it stays Idle on later ticks (until a real tick is seen).
        for _ in 0..5 {
            mode = next_mode(true, MarketState::Unknown, mode, 0, HOUR, &mut entered);
            assert_eq!(mode, SessionMode::Idle);
        }
    }
}
