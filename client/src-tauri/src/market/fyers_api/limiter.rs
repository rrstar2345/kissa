//! Rate limiter for the Fyers REST APIs (Standard plan: Data 5/s, 50/min,
//! 5,000/day; Transaction + User Info 10/s, 200/min, 100,000/day). Exceeding the
//! per-minute limit more than 3 times in a day blocks the account for the rest
//! of the day, so a 429 must never be routine.
//!
//! * Sliding windows per second and minute, kept in memory; the day counter is
//!   saved to SQLite (keyed by IST date) so a restart does not reset it.
//! * Everything is scaled by `headroom` (80 % by default).
//! * User calls may use the whole budget. Background calls (chain polls,
//!   backfill, seeds, status) stay inside a share of it, wait at most a few
//!   seconds for a free slot and are then skipped (`Throttled`).
//! * `paced_interval` tells the poll loops how far to stretch their interval so
//!   the rest of the day budget lasts until the close.
//! * A 429 pauses the category; repeated breaches escalate to the rest of the day.

use chrono::{Duration as ChronoDuration, Timelike, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::settings::{CategoryLimits, FyersLimits};

/// Share of the minute budget background calls may use (user calls keep the rest).
const BG_MIN_SHARE: f64 = 0.75;
/// Share of the day budget background calls may use.
const BG_DAY_SHARE: f64 = 0.85;
/// Longest a background call waits for a free slot before it is skipped.
const BG_MAX_WAIT: Duration = Duration::from_secs(5);
/// Longest a user call waits.
const USER_MAX_WAIT: Duration = Duration::from_secs(30);
/// A 429 with a `Retry-After` this short is a per-second burst, not a minute breach.
const BURST_RETRY_SECS: u64 = 2;
/// Pause after the 1st / 2nd per-minute breach of a day; the 3rd pauses the rest of the day.
const BREACH_PAUSE_SECS: [u64; 2] = [300, 1800];
/// NSE session (IST seconds of the day) the day budget is spread over.
const SESSION_OPEN_SOD: i64 = 9 * 3600 + 15 * 60;
const SESSION_CLOSE_SOD: i64 = 15 * 3600 + 30 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    /// Chain, quotes, history, symbol-token, market status.
    Data,
    /// Transaction and user-info APIs (login, and future orders / funds calls).
    Account,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Data => "Data APIs",
            Category::Account => "Account APIs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// A person is waiting for the answer, or an order depends on it.
    User,
    /// Polls, refreshes, seeds, backfill: can wait or be skipped.
    Background,
}

/// The call was not sent.
#[derive(Debug, Clone)]
pub struct Throttled {
    pub message: String,
    /// Seconds until a call may succeed, when known.
    #[allow(dead_code)]
    pub retry_in: Option<u64>,
}

impl fmt::Display for Throttled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Throttled {}

/// Limits after headroom, each at least 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caps {
    pub sec: u32,
    pub min: u32,
    pub day: u32,
}

impl Caps {
    pub fn new(l: &CategoryLimits, headroom: f64) -> Self {
        let h = if headroom.is_finite() { headroom.clamp(0.1, 1.0) } else { 0.8 };
        let scale = |n: u32| ((n as f64 * h).floor() as u32).max(1);
        Self { sec: scale(l.per_sec), min: scale(l.per_min), day: scale(l.per_day) }
    }

    fn min_for(&self, p: Priority) -> u32 {
        match p {
            Priority::User => self.min,
            Priority::Background => ((self.min as f64 * BG_MIN_SHARE).floor() as u32).max(1),
        }
    }

    pub fn day_for(&self, p: Priority) -> u32 {
        match p {
            Priority::User => self.day,
            Priority::Background => ((self.day as f64 * BG_DAY_SHARE).floor() as u32).max(1),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Go,
    Wait(Duration),
    Blocked { message: String, retry_in: Option<u64> },
}

/// Counters of one category.
#[derive(Debug, Default)]
pub struct Lane {
    sec: VecDeque<Instant>,
    min: VecDeque<Instant>,
    day: String,
    day_used: u32,
    breaches: u32,
    paused_until: Option<Instant>,
    paused_day: bool,
    dirty: bool,
}

impl Lane {
    fn roll_day(&mut self, today: &str) {
        if self.day != today {
            self.day = today.to_string();
            self.day_used = 0;
            self.breaches = 0;
            self.paused_until = None;
            self.paused_day = false;
            self.dirty = true;
        }
    }

    fn prune(&mut self, now: Instant) {
        while self.sec.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1)) {
            self.sec.pop_front();
        }
        while self.min.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
            self.min.pop_front();
        }
    }

    pub fn check(&mut self, now: Instant, caps: &Caps, prio: Priority) -> Verdict {
        self.prune(now);
        if self.paused_day {
            return Verdict::Blocked {
                message: "Fyers calls are paused for today after repeated rate-limit errors (Fyers blocks an account that breaches the per-minute limit too often)".into(),
                retry_in: None,
            };
        }
        if let Some(until) = self.paused_until {
            if until > now {
                let secs = until.duration_since(now).as_secs() + 1;
                return Verdict::Blocked {
                    message: format!("Fyers calls are paused for {secs} s after a rate-limit error"),
                    retry_in: Some(secs),
                };
            }
            self.paused_until = None;
        }
        let day_cap = caps.day_for(prio);
        if self.day_used >= day_cap {
            return Verdict::Blocked {
                message: format!("Fyers daily call budget used ({}/{day_cap})", self.day_used),
                retry_in: None,
            };
        }
        if self.sec.len() as u32 >= caps.sec {
            let wait = self.sec.front().map_or(Duration::from_millis(50), |t| {
                (*t + Duration::from_secs(1)).saturating_duration_since(now)
            });
            return Verdict::Wait(wait);
        }
        if self.min.len() as u32 >= caps.min_for(prio) {
            let wait = self.min.front().map_or(Duration::from_secs(1), |t| {
                (*t + Duration::from_secs(60)).saturating_duration_since(now)
            });
            return Verdict::Wait(wait);
        }
        Verdict::Go
    }

    pub fn record(&mut self, now: Instant) {
        self.sec.push_back(now);
        self.min.push_back(now);
        self.day_used = self.day_used.saturating_add(1);
        self.dirty = true;
    }

    /// A 429 came back. Returns the pause that was applied, in seconds
    /// (`u64::MAX` = the rest of the day).
    pub fn rate_limited(&mut self, now: Instant, retry_after: Option<u64>) -> u64 {
        let already = self.paused_until.is_some_and(|u| u > now) || self.paused_day;
        if retry_after.is_some_and(|s| s <= BURST_RETRY_SECS) {
            let s = retry_after.unwrap_or(1).max(1);
            self.paused_until = Some(now + Duration::from_secs(s));
            return s;
        }
        // Several calls in flight can all come back 429: that is one breach.
        if !already {
            self.breaches += 1;
            self.dirty = true;
        }
        if self.breaches as usize > BREACH_PAUSE_SECS.len() {
            self.paused_day = true;
            return u64::MAX;
        }
        let base = BREACH_PAUSE_SECS[self.breaches.max(1) as usize - 1];
        let secs = base.max(retry_after.unwrap_or(0));
        self.paused_until = Some(now + Duration::from_secs(secs));
        secs
    }
}

/// Seconds of the NSE session left, measured from `sod` (IST seconds of the
/// day). Before the open this is the whole session; after the close the next
/// one is assumed.
pub fn session_secs_left(sod: i64) -> i64 {
    let full = SESSION_CLOSE_SOD - SESSION_OPEN_SOD;
    if sod < SESSION_OPEN_SOD || sod >= SESSION_CLOSE_SOD {
        full
    } else {
        (SESSION_CLOSE_SOD - sod).max(60)
    }
}

/// The interval one polling loop should use so that `remaining_calls` last
/// `secs_left` seconds when `pollers` loops share them. Never below `base`.
pub fn pace(base: Duration, secs_left: i64, remaining_calls: u32, pollers: u32) -> Duration {
    // Keep a tenth for the other background callers (status, seeds, backfill).
    let usable = (remaining_calls as f64 * 0.9).floor();
    if usable < 1.0 {
        return base.max(Duration::from_secs(secs_left.max(60) as u64));
    }
    let ideal = secs_left.max(60) as f64 * pollers.max(1) as f64 / usable;
    base.max(Duration::from_secs_f64(ideal))
}

fn ist_date() -> String {
    (Utc::now() + ChronoDuration::seconds(19_800)).format("%Y-%m-%d").to_string()
}

fn ist_second_of_day() -> i64 {
    let t = Utc::now() + ChronoDuration::seconds(19_800);
    (t.hour() * 3600 + t.minute() * 60 + t.second()) as i64
}

// --------------------------------------------------------------- service ---

struct Inner {
    data: Lane,
    account: Lane,
    pollers: u32,
}

impl Inner {
    fn lane(&mut self, c: Category) -> &mut Lane {
        match c {
            Category::Data => &mut self.data,
            Category::Account => &mut self.account,
        }
    }
}

pub struct FyersLimiter {
    limits: FyersLimits,
    inner: Mutex<Inner>,
}

/// Counts for the Settings page.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryUsage {
    pub category: Category,
    pub label: &'static str,
    pub used_sec: u32,
    pub budget_sec: u32,
    pub used_min: u32,
    pub budget_min: u32,
    pub used_day: u32,
    pub budget_day: u32,
    pub plan_day: u32,
    pub breaches_today: u32,
    /// Why calls are paused right now, if they are.
    pub paused: Option<String>,
}

pub async fn init(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS fyers_usage (
            day TEXT NOT NULL,
            category TEXT NOT NULL,
            used INTEGER NOT NULL DEFAULT 0,
            breaches INTEGER NOT NULL DEFAULT 0,
            paused_day INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (day, category)
        )"#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

fn key(c: Category) -> &'static str {
    match c {
        Category::Data => "data",
        Category::Account => "account",
    }
}

impl FyersLimiter {
    pub fn new(limits: FyersLimits) -> Self {
        Self { limits, inner: Mutex::new(Inner { data: Lane::default(), account: Lane::default(), pollers: 0 }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn caps(&self, c: Category) -> Caps {
        let l = match c {
            Category::Data => &self.limits.data,
            Category::Account => &self.limits.account,
        };
        Caps::new(l, self.limits.headroom)
    }

    /// Today's counters from the database (call once at startup).
    pub async fn load(&self, pool: &SqlitePool) {
        let today = ist_date();
        let rows: Vec<(String, i64, i64, i64)> =
            sqlx::query_as("SELECT category, used, breaches, paused_day FROM fyers_usage WHERE day = ?")
                .bind(&today)
                .fetch_all(pool)
                .await
                .unwrap_or_default();
        {
            let mut g = self.lock();
            for c in [Category::Data, Category::Account] {
                let lane = g.lane(c);
                lane.day = today.clone();
                if let Some((_, used, breaches, paused)) = rows.iter().find(|r| r.0 == key(c)) {
                    lane.day_used = (*used).max(0) as u32;
                    lane.breaches = (*breaches).max(0) as u32;
                    lane.paused_day = *paused != 0;
                }
            }
        }
        // Old days are of no use.
        let _ = sqlx::query("DELETE FROM fyers_usage WHERE day < ?").bind(&today).execute(pool).await;
    }

    /// Writes changed counters now and then. Runs for the life of the app.
    pub fn spawn_flush(self: &std::sync::Arc<Self>, pool: SqlitePool) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                me.flush(&pool).await;
            }
        });
    }

    pub async fn flush(&self, pool: &SqlitePool) {
        let rows: Vec<(String, &'static str, u32, u32, bool)> = {
            let mut g = self.lock();
            let mut out = Vec::new();
            for c in [Category::Data, Category::Account] {
                let lane = g.lane(c);
                if lane.dirty && !lane.day.is_empty() {
                    lane.dirty = false;
                    out.push((lane.day.clone(), key(c), lane.day_used, lane.breaches, lane.paused_day));
                }
            }
            out
        };
        for (day, cat, used, breaches, paused) in rows {
            let _ = sqlx::query(
                r#"INSERT INTO fyers_usage (day, category, used, breaches, paused_day) VALUES (?, ?, ?, ?, ?)
                   ON CONFLICT(day, category) DO UPDATE SET used = excluded.used,
                     breaches = excluded.breaches, paused_day = excluded.paused_day"#,
            )
            .bind(day)
            .bind(cat)
            .bind(used as i64)
            .bind(breaches as i64)
            .bind(paused as i64)
            .execute(pool)
            .await;
        }
    }

    /// Waits for a free slot and counts the call. `Err` = the call must not be sent.
    pub async fn acquire(&self, cat: Category, prio: Priority) -> Result<(), Throttled> {
        let caps = self.caps(cat);
        let max_wait = if prio == Priority::User { USER_MAX_WAIT } else { BG_MAX_WAIT };
        let began = Instant::now();
        loop {
            let verdict = {
                let mut g = self.lock();
                let lane = g.lane(cat);
                lane.roll_day(&ist_date());
                let now = Instant::now();
                let v = lane.check(now, &caps, prio);
                if v == Verdict::Go {
                    lane.record(now);
                }
                v
            };
            match verdict {
                Verdict::Go => return Ok(()),
                Verdict::Blocked { message, retry_in } => return Err(Throttled { message, retry_in }),
                Verdict::Wait(d) => {
                    if began.elapsed() + d > max_wait {
                        return Err(Throttled {
                            message: "Fyers call skipped: the rate budget is busy".into(),
                            retry_in: Some(d.as_secs() + 1),
                        });
                    }
                    tokio::time::sleep(d + Duration::from_millis(5)).await;
                }
            }
        }
    }

    /// A call came back 429. Returns the pause in seconds (`u64::MAX` = the day).
    pub fn rate_limited(&self, cat: Category, retry_after: Option<u64>) -> u64 {
        let mut g = self.lock();
        let lane = g.lane(cat);
        lane.roll_day(&ist_date());
        lane.rate_limited(Instant::now(), retry_after)
    }

    /// Registers a polling loop for as long as the guard lives.
    pub fn poller(self: &std::sync::Arc<Self>) -> PollerGuard {
        self.lock().pollers += 1;
        PollerGuard(self.clone())
    }

    /// `base` stretched so the rest of today's background budget lasts until the close.
    pub fn paced_interval(&self, cat: Category, base: Duration) -> Duration {
        let caps = self.caps(cat);
        let (used, pollers) = {
            let mut g = self.lock();
            let pollers = g.pollers.max(1);
            let lane = g.lane(cat);
            lane.roll_day(&ist_date());
            (lane.day_used, pollers)
        };
        let left = caps.day_for(Priority::Background).saturating_sub(used);
        pace(base, session_secs_left(ist_second_of_day()), left, pollers)
    }

    pub fn usage(&self) -> Vec<CategoryUsage> {
        let now = Instant::now();
        let mut out = Vec::new();
        for c in [Category::Data, Category::Account] {
            let caps = self.caps(c);
            let plan_day = match c {
                Category::Data => self.limits.data.per_day,
                Category::Account => self.limits.account.per_day,
            };
            let mut g = self.lock();
            let lane = g.lane(c);
            lane.roll_day(&ist_date());
            lane.prune(now);
            let paused = if lane.paused_day {
                Some("paused for the rest of the day".to_string())
            } else {
                lane.paused_until
                    .filter(|u| *u > now)
                    .map(|u| format!("paused for {} s", u.duration_since(now).as_secs() + 1))
            };
            out.push(CategoryUsage {
                category: c,
                label: c.label(),
                used_sec: lane.sec.len() as u32,
                budget_sec: caps.sec,
                used_min: lane.min.len() as u32,
                budget_min: caps.min,
                used_day: lane.day_used,
                budget_day: caps.day,
                plan_day,
                breaches_today: lane.breaches,
                paused,
            });
        }
        out
    }

    pub fn limits(&self) -> &FyersLimits {
        &self.limits
    }
}

pub struct PollerGuard(std::sync::Arc<FyersLimiter>);

impl Drop for PollerGuard {
    fn drop(&mut self) {
        let mut g = self.0.lock();
        g.pollers = g.pollers.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Caps {
        Caps::new(&CategoryLimits { per_sec: 5, per_min: 50, per_day: 5_000 }, 0.8)
    }

    #[test]
    fn headroom_scales_and_floors() {
        assert_eq!(caps(), Caps { sec: 4, min: 40, day: 4_000 });
        let one = Caps::new(&CategoryLimits { per_sec: 1, per_min: 1, per_day: 1 }, 0.5);
        assert_eq!(one, Caps { sec: 1, min: 1, day: 1 });
        // A silly headroom is clamped.
        assert_eq!(Caps::new(&CategoryLimits { per_sec: 10, per_min: 10, per_day: 10 }, 5.0).sec, 10);
        assert_eq!(Caps::new(&CategoryLimits { per_sec: 10, per_min: 10, per_day: 10 }, f64::NAN).sec, 8);
    }

    #[test]
    fn per_second_window_waits_then_frees() {
        let mut lane = Lane::default();
        lane.day = "d".into();
        let t0 = Instant::now();
        for _ in 0..4 {
            assert_eq!(lane.check(t0, &caps(), Priority::User), Verdict::Go);
            lane.record(t0);
        }
        match lane.check(t0, &caps(), Priority::User) {
            Verdict::Wait(d) => assert!(d <= Duration::from_secs(1) && d > Duration::from_millis(900)),
            v => panic!("expected wait, got {v:?}"),
        }
        let t1 = t0 + Duration::from_millis(1001);
        assert_eq!(lane.check(t1, &caps(), Priority::User), Verdict::Go);
    }

    #[test]
    fn background_keeps_minute_room_for_user_calls() {
        let mut lane = Lane::default();
        lane.day = "d".into();
        let t0 = Instant::now();
        // 30 background calls spread over the minute (cap 40 * 0.75).
        for i in 0..30u64 {
            let t = t0 + Duration::from_millis(i * 1_500);
            assert_eq!(lane.check(t, &caps(), Priority::Background), Verdict::Go, "call {i}");
            lane.record(t);
        }
        let t = t0 + Duration::from_millis(30 * 1_500);
        assert!(matches!(lane.check(t, &caps(), Priority::Background), Verdict::Wait(_)));
        assert_eq!(lane.check(t, &caps(), Priority::User), Verdict::Go);
    }

    #[test]
    fn day_budget_blocks_background_before_user() {
        let mut lane = Lane::default();
        lane.day = "d".into();
        lane.day_used = 3_400; // 4,000 * 0.85
        let now = Instant::now();
        assert!(matches!(lane.check(now, &caps(), Priority::Background), Verdict::Blocked { .. }));
        assert_eq!(lane.check(now, &caps(), Priority::User), Verdict::Go);
        lane.day_used = 4_000;
        assert!(matches!(lane.check(now, &caps(), Priority::User), Verdict::Blocked { .. }));
    }

    #[test]
    fn new_day_resets_counters() {
        let mut lane = Lane::default();
        lane.day = "2026-11-14".into();
        lane.day_used = 4_000;
        lane.breaches = 2;
        lane.paused_day = true;
        lane.roll_day("2026-11-15");
        assert_eq!((lane.day_used, lane.breaches, lane.paused_day), (0, 0, false));
    }

    #[test]
    fn breaches_escalate_to_the_day() {
        let mut lane = Lane::default();
        lane.day = "d".into();
        let t0 = Instant::now();
        assert_eq!(lane.rate_limited(t0, None), 300);
        // Concurrent 429s of the same incident are one breach.
        assert_eq!(lane.rate_limited(t0, None), 300);
        assert_eq!(lane.breaches, 1);
        match lane.check(t0 + Duration::from_secs(10), &caps(), Priority::User) {
            Verdict::Blocked { retry_in: Some(s), .. } => assert!(s > 280 && s <= 300),
            v => panic!("{v:?}"),
        }
        let t1 = t0 + Duration::from_secs(301);
        assert_eq!(lane.check(t1, &caps(), Priority::User), Verdict::Go);
        assert_eq!(lane.rate_limited(t1, Some(900)), 1800);
        assert_eq!(lane.breaches, 2);
        let t2 = t1 + Duration::from_secs(1801);
        assert_eq!(lane.rate_limited(t2, None), u64::MAX);
        assert!(matches!(lane.check(t2, &caps(), Priority::User), Verdict::Blocked { retry_in: None, .. }));
    }

    #[test]
    fn short_retry_after_is_a_burst_not_a_breach() {
        let mut lane = Lane::default();
        lane.day = "d".into();
        let t0 = Instant::now();
        assert_eq!(lane.rate_limited(t0, Some(1)), 1);
        assert_eq!(lane.breaches, 0);
        assert_eq!(lane.check(t0 + Duration::from_millis(1100), &caps(), Priority::User), Verdict::Go);
    }

    #[test]
    fn pacing_stretches_the_interval() {
        // 4,500 REST polls at 5 s do not fit a 22,500 s session on a 3,400 budget.
        let p = pace(Duration::from_secs(5), 22_500, 3_400, 1);
        assert!(p > Duration::from_secs(7) && p < Duration::from_secs(8), "{p:?}");
        // Two loops share the budget.
        assert!(pace(Duration::from_secs(5), 22_500, 3_400, 2) > Duration::from_secs(14));
        // Plenty left: the base interval stands.
        assert_eq!(pace(Duration::from_secs(60), 22_500, 3_400, 1), Duration::from_secs(60));
        // Nothing left: wait out the session.
        assert_eq!(pace(Duration::from_secs(5), 1_000, 0, 1), Duration::from_secs(1_000));
    }

    #[test]
    fn session_clock() {
        assert_eq!(session_secs_left(8 * 3600), 22_500);
        assert_eq!(session_secs_left(12 * 3600), 3 * 3600 + 30 * 60);
        assert_eq!(session_secs_left(16 * 3600), 22_500);
        assert_eq!(session_secs_left(SESSION_CLOSE_SOD - 5), 60);
    }
}
