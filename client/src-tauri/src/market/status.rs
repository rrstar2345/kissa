//! Exchange status per data source (NSE and Fyers), from each source's own
//! `marketStatus` API, so holidays, pre-open and the close are known instead
//! of guessed from the clock.
//!
//! - `MarketStatusService` holds the latest `SourceStatus` per source in a
//!   `watch` channel: readers get the current value cheaply and can wait for
//!   a change instead of polling.
//! - A background poller (`spawn`) is the only code that calls the status
//!   APIs. It makes no network call outside the 08:30-16:00 IST weekday
//!   window, checks every 5 minutes while the answer is stable (30 s around
//!   pre-open and the close, 60 s early morning) and stops for the day once
//!   the market has opened and closed.
//! - Only the selected data source (`DataSource`, setting `data.source`) is
//!   polled and streamed; the other one reads `NotUsed` and makes no calls.
//! - `SessionState` (market_clock.rs) turns the NSE status into the
//!   NSE streamers' Active/Idle mode (always Idle when Fyers is the source).
//!   Fyers consumers (chain page, price feed, socket) read the Fyers status
//!   directly.
//! - `Unknown` (never checked, or the API keeps failing) falls back to the
//!   old clock-window behaviour, so a broken status API is never worse than
//!   before.
//!
//! Index options trade from 09:15 IST and the pre-open session runs
//! 09:00-09:15. Whatever a source reports, "Open" or "Pre-open" before 09:00
//! is shown as `Closed`, and "Open" from 09:00 to 09:15 as `PreOpen` (see
//! `guard_hours`).

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::chain::fyers::FyersCreds;
use crate::market::{http, market_clock};
use crate::settings::{AppConfig, FyersConfig};
use crate::state::SharedState;

/// Fyers exchange code for NSE and segment code for equity derivatives
/// (https://fyers.in/api-docs/v3/api/appendix/exchanges, .../segments).
const FYERS_NSE: i64 = 10;
const FYERS_EQUITY_DERIVATIVES: i64 = 11;

/// The pre-open session starts at 09:00 IST (minutes since midnight).
const PRE_OPEN_MINUTES: u32 = 9 * 60;
/// Index options start trading at 09:15 IST (minutes since midnight).
const OPTIONS_OPEN_MINUTES: u32 = 9 * 60 + 15;
/// The cash and F&O session ends at 15:30 IST (minutes since midnight).
const SESSION_END_MINUTES: u32 = 15 * 60 + 30;

// ------------------------------------------------------------------ types ---

/// Where live prices and the exchange status come from (setting `data.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DataSource {
    /// NSE public API (the default).
    Nse,
    Fyers,
}

impl DataSource {
    /// `data.source` value: "fyers" selects Fyers, anything else (or unset) NSE.
    pub fn from_setting(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("fyers") => DataSource::Fyers,
            _ => DataSource::Nse,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DataSource::Nse => "NSE",
            DataSource::Fyers => "Fyers",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MarketState {
    Open,
    PreOpen,
    Closed,
    /// Not checked yet, or the status API is failing.
    Unknown,
}

/// Where a `SourceStatus` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Reason {
    /// Inside the trading window, first check not done yet.
    Starting,
    Weekend,
    OutsideHours,
    /// The exchange (via the source's status API) said so.
    Reported,
    /// The status API could not be read.
    Error,
    /// This source is not the selected data source, so it is not checked.
    NotUsed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub state: MarketState,
    pub reason: Reason,
    /// What the source said, or why there is no answer (shown as a tooltip).
    pub message: String,
    /// IST wall clock (`HH:MM:SS`) of the last successful check.
    pub checked_at: Option<String>,
}

impl SourceStatus {
    fn new(state: MarketState, reason: Reason, message: impl Into<String>) -> Self {
        Self { state, reason, message: message.into(), checked_at: None }
    }

    pub fn starting() -> Self {
        Self::new(MarketState::Unknown, Reason::Starting, "Checking the exchange status")
    }

    pub fn not_used(source: DataSource) -> Self {
        Self::new(
            MarketState::Unknown,
            Reason::NotUsed,
            format!("{} is not the selected data source", source.label()),
        )
    }

    fn error(message: impl Into<String>) -> Self {
        Self::new(MarketState::Unknown, Reason::Error, message)
    }

    fn reported(state: MarketState, message: String, now: DateTime<Utc>) -> Self {
        Self { state, reason: Reason::Reported, message, checked_at: Some(hhmmss(now)) }
    }

    /// Should a consumer poll this source's market data right now? Unknown
    /// falls back to the clock window, as before the status API existed.
    pub fn allows_polling(&self, in_window: bool) -> bool {
        // A source that is not selected is never polled, whatever the clock says.
        if self.reason == Reason::NotUsed {
            return false;
        }
        match self.state {
            MarketState::Open => true,
            MarketState::PreOpen | MarketState::Closed => false,
            MarketState::Unknown => in_window,
        }
    }
}

/// A parsed answer from a status API.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub state: MarketState,
    pub message: String,
}

// ---------------------------------------------------------------- service ---

pub struct MarketStatusService {
    nse: watch::Sender<SourceStatus>,
    fyers: watch::Sender<SourceStatus>,
    /// The selected data source, cached from `data.source`. Kept current by
    /// `set_source` (the `set_setting` command calls it), so hot paths read it
    /// without touching the database.
    source: watch::Sender<DataSource>,
}

pub type SharedMarketStatus = Arc<MarketStatusService>;

impl MarketStatusService {
    /// `source` is the saved `data.source`; the other source starts as `NotUsed`.
    pub fn new(source: DataSource) -> SharedMarketStatus {
        let clock = clock_status(market_clock::get_ist_now());
        let (nse_init, fyers_init) = match source {
            DataSource::Nse => (clock, SourceStatus::not_used(DataSource::Fyers)),
            DataSource::Fyers => (SourceStatus::not_used(DataSource::Nse), clock),
        };
        let (nse, _) = watch::channel(nse_init);
        let (fyers, _) = watch::channel(fyers_init);
        let (source, _) = watch::channel(source);
        Arc::new(Self { nse, fyers, source })
    }

    pub fn nse(&self) -> SourceStatus {
        self.nse.borrow().clone()
    }

    pub fn fyers(&self) -> SourceStatus {
        self.fyers.borrow().clone()
    }

    /// The selected data source.
    pub fn source(&self) -> DataSource {
        *self.source.borrow()
    }

    /// Status of the selected data source (what the header badge shows).
    pub fn active(&self) -> SourceStatus {
        match self.source() {
            DataSource::Nse => self.nse(),
            DataSource::Fyers => self.fyers(),
        }
    }

    pub fn subscribe_source(&self) -> watch::Receiver<DataSource> {
        self.source.subscribe()
    }

    /// Record a new data source. The poller notices at once (it waits on this
    /// channel) and moves the statuses and streams over.
    pub fn set_source(&self, source: DataSource) {
        self.source.send_if_modified(|cur| {
            if *cur == source {
                false
            } else {
                info!("Data source: {}", source.label());
                *cur = source;
                true
            }
        });
    }

    /// Cheap read for hot paths (streamers check it per message).
    pub fn nse_state(&self) -> MarketState {
        self.nse.borrow().state
    }

    /// Receivers see a value as "seen" at the time they subscribe, so
    /// subscribe first, then read, then `changed().await`: no change is missed.
    pub fn subscribe_nse(&self) -> watch::Receiver<SourceStatus> {
        self.nse.subscribe()
    }

    pub fn subscribe_fyers(&self) -> watch::Receiver<SourceStatus> {
        self.fyers.subscribe()
    }

    /// Store a new NSE status. Prefer `SessionState::update_nse`, which also
    /// updates the streamers' mode *before* waiters are woken.
    pub(crate) fn publish_nse(&self, status: SourceStatus) {
        publish(&self.nse, status);
    }

    pub(crate) fn publish_fyers(&self, status: SourceStatus) {
        publish(&self.fyers, status);
    }
}

/// Replace the stored status. Waiters are woken only when the state or the
/// reason changed; a new check time or message alone does not wake anyone.
fn publish(tx: &watch::Sender<SourceStatus>, new: SourceStatus) {
    let _ = tx.send_if_modified(|cur| {
        let changed = cur.state != new.state || cur.reason != new.reason;
        if changed {
            info!("Market status: {:?} ({:?}) - {}", new.state, new.reason, new.message);
        }
        *cur = new;
        changed
    });
}

// ------------------------------------------------------------------ clock ---

fn ist(now: DateTime<Utc>) -> DateTime<Utc> {
    now + chrono::Duration::seconds(market_clock::IST_OFFSET_SECS)
}

fn ist_date(now: DateTime<Utc>) -> NaiveDate {
    ist(now).date_naive()
}

fn ist_minutes(now: DateTime<Utc>) -> u32 {
    let t = ist(now);
    t.hour() * 60 + t.minute()
}

fn hhmmss(now: DateTime<Utc>) -> String {
    let t = ist(now);
    format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
}

/// The status implied by the clock alone: closed outside the trading window,
/// "starting" (to be checked) inside it.
pub fn clock_status(now: DateTime<Utc>) -> SourceStatus {
    if market_clock::is_within_trading_window(now) {
        return SourceStatus::starting();
    }
    if matches!(ist(now).weekday(), Weekday::Sat | Weekday::Sun) {
        SourceStatus::new(MarketState::Closed, Reason::Weekend, "Weekend")
    } else {
        SourceStatus::new(MarketState::Closed, Reason::OutsideHours, "Outside 08:30-16:00 IST")
    }
}

/// Fit a reported state to the exchange's hours (IST minutes since midnight):
/// before 09:00 nothing is open yet, so "Open" and "Pre-open" are `Closed`
/// (a source may report the day as open well before the session starts);
/// from 09:00 to 09:15 "Open" is the pre-open session, not options trading.
pub fn guard_hours(state: MarketState, ist_minutes: u32) -> MarketState {
    match state {
        MarketState::Open | MarketState::PreOpen if ist_minutes < PRE_OPEN_MINUTES => MarketState::Closed,
        MarketState::Open if ist_minutes < OPTIONS_OPEN_MINUTES => MarketState::PreOpen,
        other => other,
    }
}

/// True from the options open (09:15) to the session end (15:30) on a weekday
/// (the clock only; holidays are the status API's job).
pub fn is_session_hours(now: DateTime<Utc>) -> bool {
    if matches!(ist(now).weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    (OPTIONS_OPEN_MINUTES..SESSION_END_MINUTES).contains(&ist_minutes(now))
}

/// Seconds until the next status check. `failures` = consecutive failed checks.
pub fn check_interval_secs(ist_minutes: u32, state: MarketState, failures: u32) -> u64 {
    if failures > 0 {
        // 30, 60, 120, 240, then 300 s.
        return (30u64 << (failures - 1).min(4)).min(300);
    }
    match state {
        MarketState::PreOpen | MarketState::Unknown => 30,
        // Tighter near the close, so the streams stop soon after 15:30.
        MarketState::Open if ist_minutes >= 15 * 60 + 28 => 30,
        MarketState::Open => 300,
        // Pre-open is about to start (08:55) until the open has been seen (09:20).
        MarketState::Closed if (8 * 60 + 55..9 * 60 + 20).contains(&ist_minutes) => 30,
        // Early morning: the open is some minutes away. Later: a holiday or after the close.
        MarketState::Closed if ist_minutes < 9 * 60 + 20 => 60,
        MarketState::Closed => 300,
    }
}

/// What the header's feed indicator says: `(live, label)`.
///
/// `Live` means prices are ticking from the selected source in an open
/// session. `connected` = the source's stream is up; `ticking` = it delivered
/// a real tick recently; `errored` = it reports an error; `session_hours` =
/// the clock says the session is on (09:15-15:30 on a weekday), which is only
/// used when the status API gave no answer (`Unknown`).
pub fn feed_label(
    exchange: MarketState,
    connected: bool,
    ticking: bool,
    errored: bool,
    session_hours: bool,
) -> (bool, &'static str) {
    match exchange {
        MarketState::Closed => return (false, "Closed"),
        MarketState::PreOpen => return (false, "Pre-open"),
        MarketState::Unknown if !session_hours => return (false, "Checking"),
        _ => {}
    }
    if ticking {
        (true, "Live")
    } else if connected {
        (false, "No ticks")
    } else if errored {
        (false, "Error")
    } else {
        (false, "Connecting")
    }
}

// ---------------------------------------------------------------- parsers ---

fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

#[derive(Debug, Deserialize, Default)]
struct NseResp {
    #[serde(rename = "marketState", default)]
    market_state: Vec<NseRow>,
}

#[derive(Debug, Deserialize, Default)]
struct NseRow {
    #[serde(default)]
    market: Option<String>,
    #[serde(rename = "marketStatus", default)]
    market_status: Option<String>,
    #[serde(rename = "marketStatusMessage", default)]
    message: Option<String>,
    #[serde(rename = "tradeDate", default)]
    trade_date: Option<String>,
}

fn classify_nse(raw: &str) -> MarketState {
    let l = raw.trim().to_ascii_lowercase();
    if l.contains("pre") {
        MarketState::PreOpen
    } else if l == "open" {
        MarketState::Open
    } else if l.starts_with("close") {
        MarketState::Closed
    } else {
        MarketState::Unknown
    }
}

/// NSE `/api/marketStatus`. F&O follows the Capital Market row (the API has
/// no separate derivatives row), e.g. `{"market":"Capital Market","marketStatus":"Close",...}`.
pub fn parse_nse(body: &str) -> Result<Parsed> {
    let resp: NseResp = serde_json::from_str(body).map_err(|e| anyhow!("unreadable NSE market status: {e}"))?;
    let row = resp
        .market_state
        .iter()
        .find(|r| r.market.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("Capital Market")))
        .ok_or_else(|| anyhow!("NSE market status has no Capital Market row"))?;
    let raw = row.market_status.clone().unwrap_or_default();
    let state = classify_nse(&raw);
    let mut message = format!("NSE Capital Market: {raw}");
    if let Some(d) = row.trade_date.as_deref().filter(|d| !d.is_empty()) {
        message.push_str(&format!(" · last session {d}"));
    }
    if let Some(m) = row.message.as_deref().filter(|m| !m.is_empty()) {
        message.push_str(&format!(" · {m}"));
    }
    Ok(Parsed { state, message })
}

#[derive(Debug, Deserialize, Default)]
struct FyersResp {
    #[serde(default)]
    s: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(rename = "marketStatus", default)]
    market_status: Vec<FyersRow>,
}

#[derive(Debug, Deserialize, Default)]
struct FyersRow {
    #[serde(default)]
    exchange: Value,
    #[serde(default)]
    segment: Value,
    #[serde(default)]
    market_type: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// The Fyers docs list `CLOSE` but their samples show `CLOSED`,
/// `POSTCLOSE_CLOSED` and `PREOPEN_CLOSED`, so every closed-looking value is
/// accepted. Anything unrecognised is `Unknown` (clock fallback), not Closed.
fn classify_fyers(raw: &str) -> MarketState {
    match raw.trim().to_ascii_uppercase().as_str() {
        "OPEN" => MarketState::Open,
        "PREOPEN" => MarketState::PreOpen,
        "CLOSE" | "CLOSED" | "POSTCLOSE_START" | "POSTCLOSE_CLOSED" | "PREOPEN_CLOSED" | "CTS_CLOSE"
        | "CAS_START" | "CAS_MKT_ORD_RESTRICT" | "CAS_END" => MarketState::Closed,
        _ => MarketState::Unknown,
    }
}

/// Fyers `/data/marketStatus`: the NSE (10) equity-derivatives (11) `NORMAL` row.
pub fn parse_fyers(body: &str) -> Result<Parsed> {
    let resp: FyersResp = serde_json::from_str(body).map_err(|e| anyhow!("unreadable Fyers market status: {e}"))?;
    if resp.s.as_deref() != Some("ok") {
        let detail = resp.message.filter(|m| !m.is_empty()).unwrap_or_else(|| "no detail".into());
        return Err(anyhow!("Fyers market status error: {detail}"));
    }
    let row = resp
        .market_status
        .iter()
        .find(|r| {
            as_i64(&r.exchange) == Some(FYERS_NSE)
                && as_i64(&r.segment) == Some(FYERS_EQUITY_DERIVATIVES)
                && r.market_type.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("NORMAL"))
        })
        .ok_or_else(|| anyhow!("Fyers market status has no NSE equity-derivatives row"))?;
    let raw = row.status.clone().unwrap_or_default();
    Ok(Parsed { state: classify_fyers(&raw), message: format!("Fyers NSE derivatives: {raw}") })
}

// ---------------------------------------------------------------- fetches ---

/// GET an NSE JSON endpoint. NSE answers HTML or 401/403 without its session
/// cookies, so on a bad answer warm the cookie jar once and retry.
async fn nse_json(url: &str) -> Result<String> {
    let mut last = String::from("no response");
    for attempt in 0..2 {
        let resp = http::get(url)
            .header("Accept", "application/json, text/plain, */*")
            .header("Referer", format!("{}/", http::NSE_ORIGIN))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| anyhow!("request failed: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| anyhow!("could not read response: {e}"))?;
        if status.is_success() && body.trim_start().starts_with('{') {
            return Ok(body);
        }
        last = format!("HTTP {status}: {}", body.chars().take(200).collect::<String>());
        if attempt == 0 {
            let _ = http::warm_nse_session().await;
        }
    }
    Err(anyhow!("{last}"))
}

pub async fn fetch_nse(cfg: &AppConfig) -> Result<Parsed> {
    parse_nse(&nse_json(&cfg.system.market_status.base).await?)
}

pub async fn fetch_fyers(cfg: &FyersConfig, creds: &FyersCreds) -> Result<Parsed> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
    let resp = client
        .get(&cfg.status_url)
        .header("Authorization", format!("{}:{}", creds.app_id, creds.access_token))
        .send()
        .await
        .map_err(|e| anyhow!("Fyers request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await?;
    parse_fyers(&body).map_err(|e| anyhow!("{e} (HTTP {status})"))
}

// ---------------------------------------------------------------- holidays ---

/// One NSE F&O trading holiday.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct Holiday {
    pub date: NaiveDate,
    pub week_day: String,
    pub description: String,
    /// Set when NSE runs a special session that day (format not verified).
    pub morning_session: Option<String>,
    pub evening_session: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct HolidayResp {
    #[serde(rename = "FO", default)]
    fo: Option<Vec<HolidayRaw>>,
}

#[derive(Debug, Deserialize, Default)]
struct HolidayRaw {
    #[serde(rename = "tradingDate", default)]
    trading_date: String,
    #[serde(rename = "weekDay", default)]
    week_day: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    morning_session: Option<Value>,
    #[serde(default)]
    evening_session: Option<Value>,
}

#[allow(dead_code)]
fn session_text(v: &Option<Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// NSE `/api/holiday-master?type=trading`: the array under the `FO` key.
/// Not used yet (the status APIs already reflect holidays); kept for
/// look-ahead features such as "next trading day".
#[allow(dead_code)]
pub fn parse_holidays(body: &str) -> Result<Vec<Holiday>> {
    let resp: HolidayResp = serde_json::from_str(body).map_err(|e| anyhow!("unreadable NSE holiday list: {e}"))?;
    let fo = resp.fo.ok_or_else(|| anyhow!("NSE holiday list has no FO array"))?;
    Ok(fo
        .into_iter()
        .filter_map(|h| {
            let date = NaiveDate::parse_from_str(h.trading_date.trim(), "%d-%b-%Y").ok()?;
            Some(Holiday {
                date,
                week_day: h.week_day,
                description: h.description,
                morning_session: session_text(&h.morning_session),
                evening_session: session_text(&h.evening_session),
            })
        })
        .collect())
}

#[allow(dead_code)]
pub async fn fetch_holidays(cfg: &AppConfig) -> Result<Vec<Holiday>> {
    parse_holidays(&nse_json(&cfg.system.holidays.base).await?)
}

#[allow(dead_code)]
pub fn is_trading_holiday(list: &[Holiday], date: NaiveDate) -> bool {
    list.iter().any(|h| h.date == date)
}

// ------------------------------------------------------------------ poller ---

#[derive(Default)]
struct Poll {
    /// Unix seconds of the next check.
    due: i64,
    failures: u32,
    /// Day the source was last seen Open.
    opened_on: Option<NaiveDate>,
    /// Day the market was seen to close after opening: no more checks today.
    done_on: Option<NaiveDate>,
}

/// One scheduled check. Returns the new status to publish, or `None` when
/// nothing should change (not due, finished for the day, or a transient
/// failure while a good status is held).
async fn poll_source<F, Fut>(p: &mut Poll, now: DateTime<Utc>, current: &SourceStatus, fetch: F) -> Option<SourceStatus>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Parsed>>,
{
    let date = ist_date(now);
    let minutes = ist_minutes(now);
    let ts = now.timestamp();
    if p.done_on == Some(date) || ts < p.due {
        return None;
    }
    match fetch().await {
        Ok(parsed) => {
            let state = guard_hours(parsed.state, minutes);
            p.failures = 0;
            if state == MarketState::Open {
                p.opened_on = Some(date);
            }
            if state == MarketState::Closed && p.opened_on == Some(date) && minutes >= 15 * 60 {
                p.done_on = Some(date);
            }
            p.due = ts + check_interval_secs(minutes, state, 0) as i64;
            let mut message = parsed.message;
            if state != parsed.state {
                // Keep what the source said visible in the tooltip, and why it is shown differently.
                message.push_str(&format!(" · shown as {state:?} (outside session hours)"));
            }
            Some(SourceStatus::reported(state, message, now))
        }
        Err(e) => {
            p.failures += 1;
            p.due = ts + check_interval_secs(minutes, MarketState::Unknown, p.failures) as i64;
            warn!("Market status check failed ({} in a row): {e}", p.failures);
            // Keep a good answer through a couple of blips so the streams
            // are not disturbed; after that fall back to the clock.
            if current.reason == Reason::Reported && p.failures < 3 {
                None
            } else {
                Some(SourceStatus::error(e.to_string()))
            }
        }
    }
}

fn differs(a: &SourceStatus, b: &SourceStatus) -> bool {
    a.state != b.state || a.reason != b.reason
}

/// The next status for one source, given whether it is the selected data
/// source. A source that is not selected makes no call and reads `NotUsed`.
/// A source that has just been selected starts with a fresh `Poll`, so it is
/// checked at once instead of waiting out an old schedule.
async fn next_status<F, Fut>(
    p: &mut Poll,
    used: bool,
    other: DataSource,
    now: DateTime<Utc>,
    cur: &SourceStatus,
    fetch: F,
) -> Option<SourceStatus>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Parsed>>,
{
    if !used {
        return (cur.reason != Reason::NotUsed).then(|| SourceStatus::not_used(other));
    }
    if cur.reason == Reason::NotUsed {
        *p = Poll::default();
    }
    if market_clock::is_within_trading_window(now) {
        poll_source(p, now, cur, fetch).await
    } else {
        // Outside the window the clock answers; no network call.
        let s = clock_status(now);
        differs(cur, &s).then_some(s)
    }
}

/// Start the background poller (call once, from `setup`).
pub fn spawn(state: SharedState) {
    tauri::async_runtime::spawn(async move {
        let inactive = state.config.market_runtime.inactive_switch_after_secs;
        let mut nse = Poll::default();
        let mut fyers = Poll::default();
        let mut source_changes = state.market_status.subscribe_source();
        loop {
            let now = market_clock::get_ist_now();
            let source = state.market_status.source();

            // NSE: only when it is the selected data source.
            let cur = state.market_status.nse();
            let next = next_status(&mut nse, source == DataSource::Nse, DataSource::Nse, now, &cur, || {
                fetch_nse(&state.config)
            })
            .await;
            if let Some(s) = next {
                // Updates the streamers' mode first, then wakes anything waiting.
                state.session.update_nse(s, inactive).await;
            }

            // Fyers: only when it is the selected data source.
            let cur = state.market_status.fyers();
            let next = next_status(&mut fyers, source == DataSource::Fyers, DataSource::Fyers, now, &cur, || async {
                let creds = crate::commands::active_fyers_creds(&state).await?;
                fetch_fyers(&state.config.fyers, &creds).await
            })
            .await;
            if let Some(s) = next {
                state.market_status.publish_fyers(s);
            }

            // A change of data source does not wait for the next round.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                _ = source_changes.changed() => {}
            }
        }
    });
}

// ------------------------------------------------------------------- tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Real response captured on 10 Nov 2026 (Diwali-Balipratipada, a holiday).
    const NSE_HOLIDAY: &str = r##"{"marketState":[{"market":"Capital Market","marketStatus":"Close","tradeDate":"09-Nov-2026 15:30","index":"NIFTY 50","last":25044.65,"variation":58.25,"percentChange":0.23,"marketStatusMessage":"Market is Closed"},{"market":"Currency","marketStatus":"Close","tradeDate":"09-Nov-2026","index":"","last":"","variation":"","percentChange":"","marketStatusMessage":"Market is Closed"},{"market":"Commodity","marketStatus":"Open","tradeDate":"10-Nov-2026","index":"","last":"","variation":"","percentChange":"","marketStatusMessage":"Market is Closed"},{"market":"Debt","marketStatus":"Close","tradeDate":"09-Nov-2026","index":"","last":"","variation":"","percentChange":"","marketStatusMessage":"Market is Closed"},{"index":"NIFTY50 USD","last":9237.8,"change":19.049999999999272,"percentChange":0.2,"timestamp":"09-Nov-2026 15:40"},{"market":"currencyfuture","marketStatus":"Close","tradeDate":"09-Nov-2026","index":"","last":"94.0000","variation":"","percentChange":"","marketStatusMessage":"Market is Closed","expiryDate":"26-Nov-2026","underlying":"USDINR","updated_time":"09-Nov-2026 17:00","tradeDateFormatted":"09-Nov-2026 17:00","slickclass":"slick-item"}],"marketcap":{"timeStamp":"09-Nov-2026","marketCapinTRDollars":5.35,"marketCapinLACCRRupees":502.21309501290574,"marketCapinCRRupees":50221309.5,"marketCapinCRRupeesFormatted":"50,221,309.50","marketCapinLACCRRupeesFormatted":"502.21","underlying":"Market Cap"},"indicativenifty50":{"change":58.25,"closingValue":25044.65,"dateTime":"09-Nov-2026 15:30","finalClosingValue":25044.65,"indexLast":null,"indexName":"NIFTY 50","indexPercChange":null,"indexTimeVal":null,"indicativeTime":null,"perChange":0.23,"status":"CLOSE"},"giftnifty":{"CONTRACTSTRADED":21060,"DAYCHANGE":196.5,"EXPIRYDATE":"23-Nov-2026","INSTRUMENTTYPE":"FUTIDX","LASTPRICE":25317.5,"OPTIONTYPE":"-","PERCHANGE":0.78,"STRIKEPRICE":0,"SYMBOL":"NIFTY","TIMESTMP":"10-Nov-2026 15:13","id":"0/NIFTYFUTIDX23-Nov-2026--"},"niftyusd":{"index":"NIFTY50 USD","last":9237.8,"change":19.049999999999272,"percentChange":0.2,"timestamp":"09-Nov-2026 15:40"}}"##;

    fn nse(status: &str) -> String {
        format!(r#"{{"marketState":[{{"market":"Commodity","marketStatus":"Open"}},{{"market":"Capital Market","marketStatus":"{status}","marketStatusMessage":"m"}}]}}"#)
    }

    fn fyers(status: &str, extra: &str) -> String {
        format!(
            r#"{{"code":200,"s":"ok","message":"","marketStatus":[
              {{"exchange":10,"market_type":"CALL_AUCTION2","segment":11,"status":"PREOPEN"}},
              {{"exchange":11,"market_type":"NORMAL","segment":20,"status":"OPEN"}},
              {extra}
              {{"exchange":10,"market_type":"NORMAL","segment":11,"status":"{status}"}}]}}"#
        )
    }

    /// IST wall clock to a UTC instant.
    fn at(day: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, h, m, 0).unwrap() - chrono::Duration::seconds(market_clock::IST_OFFSET_SECS)
    }

    #[test]
    fn nse_holiday_sample_is_closed() {
        let p = parse_nse(NSE_HOLIDAY).unwrap();
        assert_eq!(p.state, MarketState::Closed);
        assert!(p.message.contains("09-Nov-2026"), "{}", p.message);
    }

    #[test]
    fn nse_states() {
        assert_eq!(parse_nse(&nse("Open")).unwrap().state, MarketState::Open);
        assert_eq!(parse_nse(&nse("Close")).unwrap().state, MarketState::Closed);
        assert_eq!(parse_nse(&nse("Closed")).unwrap().state, MarketState::Closed);
        assert_eq!(parse_nse(&nse("Pre-Open")).unwrap().state, MarketState::PreOpen);
        assert_eq!(parse_nse(&nse("Preopen")).unwrap().state, MarketState::PreOpen);
        assert_eq!(parse_nse(&nse("Weird")).unwrap().state, MarketState::Unknown);
    }

    #[test]
    fn nse_needs_the_capital_market_row() {
        assert!(parse_nse(r#"{"marketState":[{"market":"Currency","marketStatus":"Open"}]}"#).is_err());
        assert!(parse_nse("<html>blocked</html>").is_err());
    }

    #[test]
    fn fyers_states() {
        assert_eq!(parse_fyers(&fyers("OPEN", "")).unwrap().state, MarketState::Open);
        assert_eq!(parse_fyers(&fyers("PREOPEN", "")).unwrap().state, MarketState::PreOpen);
        for closed in ["CLOSED", "CLOSE", "POSTCLOSE_CLOSED", "POSTCLOSE_START", "PREOPEN_CLOSED"] {
            assert_eq!(parse_fyers(&fyers(closed, "")).unwrap().state, MarketState::Closed, "{closed}");
        }
        assert_eq!(parse_fyers(&fyers("SOMETHING_NEW", "")).unwrap().state, MarketState::Unknown);
    }

    #[test]
    fn fyers_uses_only_the_nse_derivatives_normal_row() {
        // The PREOPEN call-auction row and the MCX OPEN row in `fyers()` must be ignored.
        assert_eq!(parse_fyers(&fyers("CLOSED", "")).unwrap().state, MarketState::Closed);
        // Exchange and segment may arrive as strings.
        let body = r#"{"s":"ok","marketStatus":[{"exchange":"10","market_type":"NORMAL","segment":"11","status":"OPEN"}]}"#;
        assert_eq!(parse_fyers(body).unwrap().state, MarketState::Open);
    }

    #[test]
    fn fyers_errors() {
        assert!(parse_fyers(r#"{"s":"error","message":"token expired","marketStatus":[]}"#).is_err());
        let no_row = r#"{"s":"ok","marketStatus":[{"exchange":11,"market_type":"NORMAL","segment":20,"status":"OPEN"}]}"#;
        assert!(parse_fyers(no_row).is_err());
        assert!(parse_fyers("not json").is_err());
    }

    #[test]
    fn open_before_0915_is_pre_open() {
        assert_eq!(guard_hours(MarketState::Open, 9 * 60 + 5), MarketState::PreOpen);
        assert_eq!(guard_hours(MarketState::Open, 9 * 60), MarketState::PreOpen);
        assert_eq!(guard_hours(MarketState::Open, 9 * 60 + 15), MarketState::Open);
        assert_eq!(guard_hours(MarketState::Closed, 9 * 60 + 5), MarketState::Closed);
        assert_eq!(guard_hours(MarketState::PreOpen, 10 * 60), MarketState::PreOpen);
    }

    #[test]
    fn nothing_is_open_or_pre_open_before_0900() {
        // The reported "Open" / "Pre-open" before 09:00 is not a session yet.
        for m in [8 * 60 + 30, 8 * 60 + 45, 8 * 60 + 59] {
            assert_eq!(guard_hours(MarketState::Open, m), MarketState::Closed, "open at {m}");
            assert_eq!(guard_hours(MarketState::PreOpen, m), MarketState::Closed, "pre-open at {m}");
            assert_eq!(guard_hours(MarketState::Closed, m), MarketState::Closed);
            assert_eq!(guard_hours(MarketState::Unknown, m), MarketState::Unknown);
        }
    }

    #[test]
    fn session_hours_by_clock() {
        assert!(!is_session_hours(at(10, 9, 14))); // Tue
        assert!(is_session_hours(at(10, 9, 15)));
        assert!(is_session_hours(at(10, 15, 29)));
        assert!(!is_session_hours(at(10, 15, 30)));
        assert!(!is_session_hours(at(14, 11, 0))); // Sat
    }

    #[test]
    fn feed_indicator_follows_session_and_ticks() {
        // (exchange, connected, ticking, errored, session_hours)
        let f = feed_label;
        assert_eq!(f(MarketState::Open, true, true, false, true), (true, "Live"));
        assert_eq!(f(MarketState::Open, true, false, false, true), (false, "No ticks"));
        assert_eq!(f(MarketState::Open, false, false, true, true), (false, "Error"));
        assert_eq!(f(MarketState::Open, false, false, false, true), (false, "Connecting"));
        // Pre-open and closed are never Live, even with a connected, ticking stream.
        assert_eq!(f(MarketState::PreOpen, true, true, false, false), (false, "Pre-open"));
        assert_eq!(f(MarketState::Closed, true, true, false, false), (false, "Closed"));
        // No answer from the status API: the clock decides whether ticks count as Live.
        assert_eq!(f(MarketState::Unknown, true, true, false, true), (true, "Live"));
        assert_eq!(f(MarketState::Unknown, true, true, false, false), (false, "Checking"));
    }

    #[test]
    fn data_source_from_setting() {
        assert_eq!(DataSource::from_setting(None), DataSource::Nse);
        assert_eq!(DataSource::from_setting(Some("nse")), DataSource::Nse);
        assert_eq!(DataSource::from_setting(Some("")), DataSource::Nse);
        assert_eq!(DataSource::from_setting(Some("fyers")), DataSource::Fyers);
        assert_eq!(DataSource::from_setting(Some(" Fyers ")), DataSource::Fyers);
    }

    #[test]
    fn only_the_selected_source_is_active() {
        let svc = MarketStatusService::new(DataSource::Fyers);
        assert_eq!(svc.source(), DataSource::Fyers);
        assert_eq!(svc.nse().reason, Reason::NotUsed);
        assert_ne!(svc.fyers().reason, Reason::NotUsed);
        assert_eq!(svc.active().reason, svc.fyers().reason);
        svc.set_source(DataSource::Nse);
        assert_eq!(svc.source(), DataSource::Nse);
        assert_eq!(svc.active().reason, svc.nse().reason);
    }

    #[test]
    fn check_intervals() {
        let m = |h: u32, mm: u32| h * 60 + mm;
        assert_eq!(check_interval_secs(m(9, 0), MarketState::PreOpen, 0), 30);
        assert_eq!(check_interval_secs(m(8, 45), MarketState::Closed, 0), 60);
        // Around the start of pre-open the answer is checked every 30 s.
        assert_eq!(check_interval_secs(m(8, 58), MarketState::Closed, 0), 30);
        assert_eq!(check_interval_secs(m(9, 10), MarketState::Closed, 0), 30);
        assert_eq!(check_interval_secs(m(11, 0), MarketState::Closed, 0), 300);
        assert_eq!(check_interval_secs(m(11, 0), MarketState::Open, 0), 300);
        assert_eq!(check_interval_secs(m(15, 29), MarketState::Open, 0), 30);
        assert_eq!(check_interval_secs(m(11, 0), MarketState::Open, 1), 30);
        assert_eq!(check_interval_secs(m(11, 0), MarketState::Open, 3), 120);
        assert_eq!(check_interval_secs(m(11, 0), MarketState::Open, 9), 300);
    }

    #[test]
    fn clock_status_by_time() {
        // Tue 10:00 IST: inside the window, still to be checked.
        let s = clock_status(at(10, 10, 0));
        assert_eq!((s.state, s.reason), (MarketState::Unknown, Reason::Starting));
        // Tue 20:00 IST.
        let s = clock_status(at(10, 20, 0));
        assert_eq!((s.state, s.reason), (MarketState::Closed, Reason::OutsideHours));
        // Sat 10:00 IST.
        let s = clock_status(at(14, 10, 0));
        assert_eq!((s.state, s.reason), (MarketState::Closed, Reason::Weekend));
    }

    #[test]
    fn allows_polling_rules() {
        let st = |state| SourceStatus::new(state, Reason::Reported, "");
        assert!(st(MarketState::Open).allows_polling(false));
        assert!(!st(MarketState::Closed).allows_polling(true));
        assert!(!st(MarketState::PreOpen).allows_polling(true));
        assert!(st(MarketState::Unknown).allows_polling(true));
        assert!(!st(MarketState::Unknown).allows_polling(false));
        // Not the selected source: never, even inside the window.
        assert!(!SourceStatus::not_used(DataSource::Fyers).allows_polling(true));
    }

    #[tokio::test]
    async fn poll_source_schedules_and_survives_blips() {
        let now = at(10, 10, 0);
        let mut p = Poll::default();
        let ok = |state| async move { Ok(Parsed { state, message: "x".into() }) };

        let first = poll_source(&mut p, now, &SourceStatus::starting(), || ok(MarketState::Open)).await.unwrap();
        assert_eq!((first.state, first.reason), (MarketState::Open, Reason::Reported));

        // Not due yet: nothing happens, the closure is not even used.
        assert!(poll_source(&mut p, now, &first, || ok(MarketState::Closed)).await.is_none());

        // Two failures in a row keep the good answer, the third falls back to Unknown.
        let mut t = now;
        for i in 1..=3 {
            t += chrono::Duration::seconds(301);
            let r = poll_source(&mut p, t, &first, || async { Err(anyhow!("boom")) }).await;
            if i < 3 {
                assert!(r.is_none(), "failure {i}");
            } else {
                let r = r.unwrap();
                assert_eq!((r.state, r.reason), (MarketState::Unknown, Reason::Error));
            }
        }
    }

    #[tokio::test]
    async fn poll_source_stops_for_the_day_after_the_close() {
        let mut p = Poll::default();
        let ok = |state| async move { Ok(Parsed { state, message: "x".into() }) };
        let cur = SourceStatus::starting();
        poll_source(&mut p, at(10, 10, 0), &cur, || ok(MarketState::Open)).await.unwrap();
        let closed = poll_source(&mut p, at(10, 15, 35), &cur, || ok(MarketState::Closed)).await.unwrap();
        assert_eq!(closed.state, MarketState::Closed);
        // Finished for today, however much time passes.
        assert!(poll_source(&mut p, at(10, 15, 59), &cur, || ok(MarketState::Open)).await.is_none());
        // Next day it checks again.
        assert!(poll_source(&mut p, at(11, 8, 31), &cur, || ok(MarketState::Closed)).await.is_some());
    }

    #[tokio::test]
    async fn a_holiday_is_checked_every_five_minutes_not_dropped() {
        // Closed all day, never opened: no "done for the day", keeps checking slowly.
        let mut p = Poll::default();
        let ok = || async { Ok(Parsed { state: MarketState::Closed, message: "x".into() }) };
        let cur = SourceStatus::starting();
        assert!(poll_source(&mut p, at(10, 10, 0), &cur, ok).await.is_some());
        assert!(poll_source(&mut p, at(10, 10, 2), &cur, ok).await.is_none());
        assert!(poll_source(&mut p, at(10, 10, 6), &cur, ok).await.is_some());
    }

    #[tokio::test]
    async fn an_unselected_source_is_never_polled() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let called = AtomicBool::new(false);
        let mut p = Poll::default();
        let now = at(10, 10, 0);
        let fetch = || async {
            called.store(true, Ordering::SeqCst);
            Ok(Parsed { state: MarketState::Open, message: "x".into() })
        };
        let r = next_status(&mut p, false, DataSource::Fyers, now, &SourceStatus::starting(), fetch).await;
        assert_eq!(r.unwrap().reason, Reason::NotUsed);
        // Already NotUsed: nothing to publish, and still no call.
        let cur = SourceStatus::not_used(DataSource::Fyers);
        assert!(next_status(&mut p, false, DataSource::Fyers, now, &cur, fetch).await.is_none());
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_newly_selected_source_is_checked_at_once() {
        // An old schedule (next check far away) must not delay a source that was just selected.
        let mut p = Poll { due: i64::MAX, ..Poll::default() };
        let cur = SourceStatus::not_used(DataSource::Nse);
        let ok = || async { Ok(Parsed { state: MarketState::Closed, message: "x".into() }) };
        let r = next_status(&mut p, true, DataSource::Nse, at(10, 10, 0), &cur, ok).await.unwrap();
        assert_eq!((r.state, r.reason), (MarketState::Closed, Reason::Reported));
    }

    #[tokio::test]
    async fn a_selected_source_outside_the_window_answers_from_the_clock() {
        let mut p = Poll::default();
        let cur = SourceStatus::not_used(DataSource::Nse);
        let never = || async { Err(anyhow!("no call outside the window")) };
        let r = next_status(&mut p, true, DataSource::Nse, at(10, 8, 0), &cur, never).await.unwrap();
        assert_eq!((r.state, r.reason), (MarketState::Closed, Reason::OutsideHours));
    }

    #[tokio::test]
    async fn open_reported_before_0900_is_shown_closed_with_the_reason() {
        let mut p = Poll::default();
        let ok = || async { Ok(Parsed { state: MarketState::Open, message: "NSE Capital Market: Open".into() }) };
        let s = poll_source(&mut p, at(10, 8, 40), &SourceStatus::starting(), ok).await.unwrap();
        assert_eq!(s.state, MarketState::Closed);
        assert!(s.message.contains("Capital Market: Open"), "{}", s.message);
        assert!(s.message.contains("shown as Closed"), "{}", s.message);
        // It did not count as the day's open, so a later close does not end the day's checks.
        assert_eq!(p.opened_on, None);
    }

    #[test]
    fn holiday_master_parses_the_fo_array() {
        let body = r#"{"CBM":[],"CD":[],"FO":[
          {"tradingDate":"15-Jan-2026","weekDay":"Thursday","description":"Municipal Corporation Election - Maharashtra","morning_session":null,"evening_session":null,"Sr_no":1},
          {"tradingDate":"10-Nov-2026","weekDay":"Tuesday","description":"Diwali-Balipratipada","morning_session":null,"evening_session":null,"Sr_no":18},
          {"tradingDate":"bad date","weekDay":"","description":"skipped"}],"IRD":[]}"#;
        let list = parse_holidays(body).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].description, "Diwali-Balipratipada");
        assert_eq!(list[1].morning_session, None);
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        assert!(is_trading_holiday(&list, d(11, 10)));
        assert!(!is_trading_holiday(&list, d(11, 11)));
        assert!(parse_holidays(r#"{"CD":[]}"#).is_err());
    }
}
