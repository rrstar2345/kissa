//! Alerts: Telegram and email notifications for what happens in the app.
//!
//! Anything can call `AlertService::notify` (it never blocks or fails); a
//! background worker then
//!   1. drops events the user switched off,
//!   2. suppresses repeats of the same event + key inside the de-dup window,
//!   3. delivers to every enabled channel, retrying failures with backoff,
//!   4. records the outcome in the history (`alert_outbox`).
//!
//! Events: fills, rejections, risk halts and the kill switch, connection
//! loss, token expiry reminders and the daily P&L summary. To add one: add a
//! `Kind`, then call `notify` where it happens (see `trading::service::alerts`).

pub mod channels;
pub mod config;
pub mod store;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, warn};

use channels::Target;
use config::{AlertConfig, SECRET_ACCOUNT, SMTP_PASSWORD, TELEGRAM_TOKEN};

/// Delivery attempts per alert; the wait before attempt n+1 is `BACKOFF_SECS[n-1]`.
const MAX_ATTEMPTS: usize = 3;
const BACKOFF_SECS: [u64; 2] = [5, 20];

// ------------------------------------------------------------------ kinds ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fill,
    Rejection,
    Risk,
    Connection,
    TokenExpiry,
    DailySummary,
    /// The Test button. Not a user-selectable event.
    Test,
}

impl Kind {
    /// The events the user can switch on and off.
    pub const EVENTS: [Kind; 6] =
        [Kind::Fill, Kind::Rejection, Kind::Risk, Kind::Connection, Kind::TokenExpiry, Kind::DailySummary];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Fill => "fill",
            Kind::Rejection => "rejection",
            Kind::Risk => "risk",
            Kind::Connection => "connection",
            Kind::TokenExpiry => "token_expiry",
            Kind::DailySummary => "daily_summary",
            Kind::Test => "test",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Fill => "Fills",
            Kind::Rejection => "Rejections",
            Kind::Risk => "Risk halts",
            Kind::Connection => "Connection loss",
            Kind::TokenExpiry => "Token expiry",
            Kind::DailySummary => "Daily P&L summary",
            Kind::Test => "Test",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Kind::Fill => "An order was filled, fully or in part (including stop-loss and target legs).",
            Kind::Rejection => "An order was rejected by the broker or the paper engine.",
            Kind::Risk => "The daily loss limit halted an account, or the kill switch was turned on or off.",
            Kind::Connection => "A broker connection or price feed went down, and when it came back.",
            Kind::TokenExpiry => "A saved Fyers or Dhan access token is about to expire, or probably has.",
            Kind::DailySummary => "Net P&L of every account after the close, on days with activity.",
            Kind::Test => "",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventInfo {
    pub kind: String,
    pub label: String,
    pub description: String,
}

/// One alert to send.
#[derive(Debug, Clone)]
pub struct Alert {
    pub kind: Kind,
    /// Identifies the occurrence: the same kind + key within the de-dup
    /// window is sent once.
    pub key: String,
    pub title: String,
    pub body: String,
}

impl Alert {
    pub fn new(kind: Kind, key: impl Into<String>, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self { kind, key: key.into(), title: title.into(), body: body.into() }
    }
}

/// What the Alerts page loads. Secret values are never included.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertSettings {
    pub config: AlertConfig,
    pub events: Vec<EventInfo>,
    pub telegram_token_set: bool,
    pub smtp_password_set: bool,
}

// --------------------------------------------------------- token reminders ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenPhase {
    Fresh,
    /// Inside the reminder lead time.
    Soon,
    Expired,
}

impl TokenPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenPhase::Fresh => "fresh",
            TokenPhase::Soon => "soon",
            TokenPhase::Expired => "expired",
        }
    }
}

pub fn token_phase(now: DateTime<Utc>, expires: DateTime<Utc>, warn: ChronoDuration) -> TokenPhase {
    if now >= expires {
        TokenPhase::Expired
    } else if now >= expires - warn {
        TokenPhase::Soon
    } else {
        TokenPhase::Fresh
    }
}

// ----------------------------------------------------------------- service ---

enum Msg {
    Alert(Alert),
    Status { account_id: String, state: String, detail: String },
}

pub struct AlertService {
    pool: SqlitePool,
    app: AppHandle,
    http: reqwest::Client,
    tx: UnboundedSender<Msg>,
    config: RwLock<AlertConfig>,
    last_sent: Mutex<HashMap<String, Instant>>,
}

impl AlertService {
    /// Load the saved settings and start the worker.
    pub async fn new(pool: SqlitePool, app: AppHandle) -> Arc<Self> {
        let cfg = config::load(&pool).await;
        let (tx, rx) = unbounded_channel();
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default();
        let svc = Arc::new(Self { pool, app, http, tx, config: RwLock::new(cfg), last_sent: Mutex::new(HashMap::new()) });
        tauri::async_runtime::spawn(svc.clone().run(rx));
        svc
    }

    pub fn config(&self) -> AlertConfig {
        self.config.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// At least one channel is enabled and fully configured.
    pub fn has_channel(&self) -> bool {
        !channels::targets(&self.config()).is_empty()
    }

    /// Queue an alert. Never blocks and never fails the caller.
    pub fn notify(&self, alert: Alert) {
        let _ = self.tx.send(Msg::Alert(alert));
    }

    /// Feed a broker status change; the worker turns the first loss into an
    /// alert (and the recovery into another) and ignores repeats.
    pub fn connection(&self, account_id: &str, state: &str, detail: &str) {
        let _ = self.tx.send(Msg::Status {
            account_id: account_id.to_string(),
            state: state.to_string(),
            detail: detail.to_string(),
        });
    }

    fn emit(&self) {
        let _ = self.app.emit("alert-update", true);
    }

    // -------------------------------------------------------------- worker ---

    async fn run(self: Arc<Self>, mut rx: UnboundedReceiver<Msg>) {
        let mut down: HashSet<String> = HashSet::new();
        while let Some(msg) = rx.recv().await {
            let alert = match msg {
                Msg::Alert(a) => a,
                Msg::Status { account_id, state, detail } => {
                    match self.status_alert(&mut down, &account_id, &state, &detail).await {
                        Some(a) => a,
                        None => continue,
                    }
                }
            };
            self.dispatch(alert).await;
        }
    }

    async fn status_alert(
        &self,
        down: &mut HashSet<String>,
        account_id: &str,
        state: &str,
        detail: &str,
    ) -> Option<Alert> {
        let lost = matches!(state, "disconnected" | "feed_down" | "error");
        if !lost && state != "connected" {
            return None;
        }
        if lost && !down.insert(account_id.to_string()) {
            return None; // already reported; wait for the recovery
        }
        if !lost && !down.remove(account_id) {
            return None; // connected without a reported loss
        }
        let name = crate::accounts::get(&self.pool, account_id)
            .await
            .map(|a| a.name)
            .unwrap_or_else(|_| account_id.to_string());
        let detail = detail.trim();
        Some(if lost {
            Alert::new(
                Kind::Connection,
                format!("{account_id}:lost"),
                format!("Connection lost: {name}"),
                if detail.is_empty() { state.to_string() } else { format!("{state}: {detail}") },
            )
        } else {
            Alert::new(Kind::Connection, format!("{account_id}:restored"), format!("Connection restored: {name}"), "")
        })
    }

    async fn dispatch(self: &Arc<Self>, alert: Alert) {
        let cfg = self.config();
        let kind = alert.kind.as_str();
        if !cfg.event_on(alert.kind) {
            debug!("Alert “{}” skipped: {kind} alerts are off", alert.title);
            return;
        }
        let dedupe_key = format!("{kind}:{}", alert.key);
        if cfg.dedupe_secs > 0 && self.is_duplicate(&dedupe_key, cfg.dedupe_secs) {
            self.record(&alert, store::SUPPRESSED, "Same alert sent in the last minute(s); suppressed.").await;
            return;
        }
        let targets = channels::targets(&cfg);
        if targets.is_empty() {
            self.record(&alert, store::SKIPPED, "No channel is enabled and configured.").await;
            return;
        }
        let id = match store::insert(&self.pool, kind, &alert.title, &alert.body, store::SENDING, "").await {
            Ok(id) => id,
            Err(e) => {
                warn!("Could not record alert “{}”: {e}", alert.title);
                return;
            }
        };
        self.emit();
        // Delivery can take a while (retries): do not hold up other alerts.
        let me = self.clone();
        tauri::async_runtime::spawn(async move { me.deliver(id, alert, cfg, targets).await });
    }

    /// True when the key was sent inside the window; otherwise remembers it now.
    fn is_duplicate(&self, key: &str, window_secs: u64) -> bool {
        let Ok(mut map) = self.last_sent.lock() else { return false };
        let now = Instant::now();
        map.retain(|_, t| now.duration_since(*t) < Duration::from_secs(3600));
        if let Some(t) = map.get(key) {
            if now.duration_since(*t) < Duration::from_secs(window_secs) {
                return true;
            }
        }
        map.insert(key.to_string(), now);
        false
    }

    async fn record(&self, alert: &Alert, status: &str, detail: &str) {
        if let Err(e) = store::insert(&self.pool, alert.kind.as_str(), &alert.title, &alert.body, status, detail).await {
            warn!("Could not record alert “{}”: {e}", alert.title);
        }
        self.emit();
    }

    async fn send_one(&self, target: &Target, cfg: &AlertConfig, alert: &Alert) -> Result<(), String> {
        match target {
            Target::Telegram(chat) => {
                let text = if alert.body.is_empty() {
                    format!("KISSA: {}", alert.title)
                } else {
                    format!("KISSA: {}\n{}", alert.title, alert.body)
                };
                channels::telegram(&self.http, chat, &text).await
            }
            Target::Email => channels::email(&cfg.email, &format!("[KISSA] {}", alert.title), &alert.body).await,
        }
    }

    /// Send to every target, retrying only the ones that failed.
    async fn deliver(self: Arc<Self>, id: i64, alert: Alert, cfg: AlertConfig, targets: Vec<Target>) {
        let mut pending = targets;
        let mut done: Vec<String> = Vec::new();
        let mut errors: HashMap<String, String> = HashMap::new();
        let mut attempts = 1;
        for attempt in 1..=MAX_ATTEMPTS {
            attempts = attempt;
            let mut failed = Vec::new();
            for t in pending {
                match self.send_one(&t, &cfg, &alert).await {
                    Ok(()) => {
                        errors.remove(&t.label());
                        done.push(t.label());
                    }
                    Err(e) => {
                        errors.insert(t.label(), e);
                        failed.push(t);
                    }
                }
            }
            pending = failed;
            if pending.is_empty() || attempt == MAX_ATTEMPTS {
                break;
            }
            tokio::time::sleep(Duration::from_secs(BACKOFF_SECS[attempt - 1])).await;
        }
        let status = if pending.is_empty() {
            store::SENT
        } else if done.is_empty() {
            store::FAILED
        } else {
            store::PARTIAL
        };
        let mut lines: Vec<String> = done.iter().map(|l| format!("{l}: sent")).collect();
        lines.extend(pending.iter().map(|t| format!("{}: {}", t.label(), errors.get(&t.label()).cloned().unwrap_or_default())));
        if status != store::SENT {
            warn!("Alert “{}” was not fully delivered: {}", alert.title, lines.join("; "));
        }
        if let Err(e) = store::finish(&self.pool, id, status, &lines.join("\n"), attempts as i64).await {
            warn!("Could not update alert history: {e}");
        }
        self.emit();
    }

    // ------------------------------------------------------ settings, test ---

    pub async fn settings(&self) -> AlertSettings {
        AlertSettings {
            config: self.config(),
            events: Kind::EVENTS
                .iter()
                .map(|k| EventInfo { kind: k.as_str().into(), label: k.label().into(), description: k.description().into() })
                .collect(),
            telegram_token_set: channels::secret_set(TELEGRAM_TOKEN),
            smtp_password_set: channels::secret_set(SMTP_PASSWORD),
        }
    }

    /// Validate and save. A blank secret keeps the stored one.
    pub async fn save_settings(
        &self,
        mut cfg: AlertConfig,
        telegram_token: Option<String>,
        smtp_password: Option<String>,
    ) -> Result<AlertSettings> {
        cfg.normalise();
        let token = telegram_token.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let password = smtp_password.as_deref().map(str::trim).filter(|s| !s.is_empty());
        cfg.validate(token.is_some() || channels::secret_set(TELEGRAM_TOKEN), password.is_some() || channels::secret_set(SMTP_PASSWORD))
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(t) = token {
            crate::secrets::set(SECRET_ACCOUNT, TELEGRAM_TOKEN, t)?;
        }
        if let Some(p) = password {
            crate::secrets::set(SECRET_ACCOUNT, SMTP_PASSWORD, p)?;
        }
        config::save(&self.pool, &cfg).await?;
        if let Ok(mut c) = self.config.write() {
            *c = cfg;
        }
        Ok(self.settings().await)
    }

    /// Send a test message through one channel (`telegram` | `email`) with the
    /// saved settings, once and without retries. The channel only has to be
    /// configured, not switched on.
    pub async fn send_test(&self, channel: &str) -> Result<String> {
        let cfg = self.config();
        let alert = Alert::new(Kind::Test, "test", "Test alert", "If you can read this, alerts from KISSA reach you.");
        let (label, result) = match channel {
            "telegram" => {
                if cfg.telegram.chat_ids.is_empty() {
                    return Err(anyhow!("Add at least one chat ID and save first"));
                }
                let mut errors = Vec::new();
                for chat in &cfg.telegram.chat_ids {
                    if let Err(e) = self.send_one(&Target::Telegram(chat.clone()), &cfg, &alert).await {
                        errors.push(format!("{chat}: {e}"));
                    }
                }
                let ok = format!("Test message sent to {} Telegram chat(s).", cfg.telegram.chat_ids.len());
                ("Telegram", if errors.is_empty() { Ok(ok) } else { Err(errors.join("; ")) })
            }
            "email" => {
                if cfg.email.host.is_empty() || cfg.email.from.is_empty() || cfg.email.to.is_empty() {
                    return Err(anyhow!("Fill in the SMTP host, From and recipients and save first"));
                }
                let r = self
                    .send_one(&Target::Email, &cfg, &alert)
                    .await
                    .map(|_| format!("Test email sent to {}.", cfg.email.to.join(", ")));
                ("Email", r)
            }
            other => return Err(anyhow!("Unknown channel: {other}")),
        };
        let (status, detail) = match &result {
            Ok(m) => (store::SENT, m.clone()),
            Err(e) => (store::FAILED, e.clone()),
        };
        self.record(&alert, status, &format!("{label}: {detail}")).await;
        result.map_err(|e| anyhow!("{label}: {e}"))
    }

    pub async fn history(&self, limit: i64) -> Result<Vec<store::AlertRow>> {
        store::list(&self.pool, limit).await
    }

    pub async fn clear_history(&self) -> Result<()> {
        store::clear(&self.pool).await?;
        self.emit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_phases() {
        let expires = Utc::now() + ChronoDuration::hours(10);
        let warn = ChronoDuration::hours(2);
        assert_eq!(token_phase(Utc::now(), expires, warn), TokenPhase::Fresh);
        assert_eq!(token_phase(expires - ChronoDuration::minutes(90), expires, warn), TokenPhase::Soon);
        assert_eq!(token_phase(expires + ChronoDuration::seconds(1), expires, warn), TokenPhase::Expired);
    }

    #[test]
    fn every_event_has_a_unique_name() {
        let mut names: Vec<&str> = Kind::EVENTS.iter().map(|k| k.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Kind::EVENTS.len());
        assert!(Kind::EVENTS.iter().all(|k| !k.label().is_empty() && !k.description().is_empty()));
    }
}
