//! Alert settings. Non-secret fields live in SQLite (`app_settings`, key
//! `alerts.config`); the Telegram bot token and the SMTP password live in the
//! OS keychain (account id `alerts`) and are never sent to the frontend.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;

use super::Kind;

pub const SETTING_KEY: &str = "alerts.config";
/// Keychain account id and fields for the alert secrets.
pub const SECRET_ACCOUNT: &str = "alerts";
pub const TELEGRAM_TOKEN: &str = "telegramToken";
pub const SMTP_PASSWORD: &str = "smtpPassword";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TelegramConfig {
    pub enabled: bool,
    /// Numeric chat ids (users, groups) or `@channelname`.
    pub chat_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct EmailConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// `starttls` (usually port 587) | `tls` (usually 465) | `none` (localhost only).
    pub security: String,
    /// Blank = no authentication.
    pub username: String,
    pub from: String,
    pub to: Vec<String>,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 587,
            security: "starttls".into(),
            username: String::new(),
            from: String::new(),
            to: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct AlertConfig {
    pub telegram: TelegramConfig,
    pub email: EmailConfig,
    /// Event kind (`Kind::as_str`) -> on / off. Missing = on.
    pub events: HashMap<String, bool>,
    /// The same alert (same event and key) is sent at most once per this many
    /// seconds; repeats are recorded as suppressed. 0 = no de-duplication.
    pub dedupe_secs: u64,
    /// Daily P&L summary time, IST `HH:MM` (weekdays, only on days with activity).
    pub summary_time: String,
    /// Remind this many hours before a broker token is expected to expire.
    pub token_warn_hours: u32,
    /// How long a saved Fyers / Dhan access token is assumed to last (hours).
    /// 0 = no reminders for that broker.
    pub fyers_token_hours: u32,
    pub dhan_token_hours: u32,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            telegram: TelegramConfig::default(),
            email: EmailConfig::default(),
            events: HashMap::new(),
            dedupe_secs: 60,
            summary_time: "15:45".into(),
            token_warn_hours: 2,
            fyers_token_hours: 24,
            dhan_token_hours: 24,
        }
    }
}

/// `HH:MM` as minutes since midnight.
pub fn hhmm_minutes(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

fn clean_list(items: &mut Vec<String>) {
    let mut seen = Vec::new();
    for s in items.drain(..) {
        let s = s.trim().to_string();
        if !s.is_empty() && !seen.contains(&s) {
            seen.push(s);
        }
    }
    *items = seen;
}

impl AlertConfig {
    pub fn event_on(&self, kind: Kind) -> bool {
        self.events.get(kind.as_str()).copied().unwrap_or(true)
    }

    /// Summary time as minutes since midnight IST (15:45 if unreadable).
    pub fn summary_minutes(&self) -> u32 {
        hhmm_minutes(&self.summary_time).unwrap_or(15 * 60 + 45)
    }

    /// Trim whitespace and drop blank / duplicate list entries.
    pub fn normalise(&mut self) {
        clean_list(&mut self.telegram.chat_ids);
        clean_list(&mut self.email.to);
        self.email.host = self.email.host.trim().to_string();
        self.email.username = self.email.username.trim().to_string();
        self.email.from = self.email.from.trim().to_string();
        self.email.security = self.email.security.trim().to_lowercase();
        self.summary_time = self.summary_time.trim().to_string();
        // Only keep toggles for events that exist.
        self.events.retain(|k, _| Kind::EVENTS.iter().any(|e| e.as_str() == k.as_str()));
    }

    /// `token_set` / `password_set`: the secret is already stored or is being
    /// saved together with this config.
    pub fn validate(&self, token_set: bool, password_set: bool) -> Result<(), String> {
        if self.telegram.enabled {
            if !token_set {
                return Err("Telegram: enter the bot token".into());
            }
            if self.telegram.chat_ids.is_empty() {
                return Err("Telegram: add at least one chat ID".into());
            }
            for id in &self.telegram.chat_ids {
                let named_channel = id.starts_with('@') && id.len() > 1 && !id.contains(char::is_whitespace);
                if !(named_channel || id.parse::<i64>().is_ok()) {
                    return Err(format!("Telegram: “{id}” is not a chat ID (a number, or @channelname)"));
                }
            }
        }
        if self.email.enabled {
            let e = &self.email;
            if e.host.is_empty() {
                return Err("Email: SMTP host is required".into());
            }
            if e.port == 0 {
                return Err("Email: SMTP port is required".into());
            }
            match e.security.as_str() {
                "starttls" | "tls" => {}
                "none" => {
                    let h = e.host.to_lowercase();
                    if !(h == "localhost" || h == "127.0.0.1") {
                        return Err("Email: unencrypted SMTP is only allowed for localhost; use STARTTLS or TLS".into());
                    }
                }
                _ => return Err("Email: security must be starttls, tls or none".into()),
            }
            if !e.username.is_empty() && !password_set {
                return Err("Email: enter the SMTP password".into());
            }
            if !e.from.contains('@') {
                return Err("Email: the From address is not valid".into());
            }
            if e.to.is_empty() {
                return Err("Email: add at least one recipient".into());
            }
            if let Some(bad) = e.to.iter().find(|a| !a.contains('@')) {
                return Err(format!("Email: “{bad}” is not an email address"));
            }
        }
        if self.dedupe_secs > 3600 {
            return Err("De-duplication window can be at most 3600 seconds".into());
        }
        if hhmm_minutes(&self.summary_time).is_none() {
            return Err("Daily summary time must be HH:MM (24-hour IST), e.g. 15:45".into());
        }
        if self.token_warn_hours > 72 {
            return Err("Token reminder lead time can be at most 72 hours".into());
        }
        if self.fyers_token_hours > 24 * 90 || self.dhan_token_hours > 24 * 90 {
            return Err("Token lifetime can be at most 2160 hours (90 days)".into());
        }
        Ok(())
    }
}

pub async fn load(pool: &SqlitePool) -> AlertConfig {
    crate::db::get_setting(pool, SETTING_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub async fn save(pool: &SqlitePool, cfg: &AlertConfig) -> Result<()> {
    crate::db::set_setting(pool, SETTING_KEY, &serde_json::to_string(cfg)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_default_on_and_can_be_switched_off() {
        let mut c = AlertConfig::default();
        assert!(c.event_on(Kind::Fill));
        c.events.insert("fill".into(), false);
        assert!(!c.event_on(Kind::Fill));
        assert!(c.event_on(Kind::Rejection));
    }

    #[test]
    fn summary_time_parses_or_falls_back() {
        let mut c = AlertConfig::default();
        assert_eq!(c.summary_minutes(), 15 * 60 + 45);
        c.summary_time = "09:05".into();
        assert_eq!(c.summary_minutes(), 9 * 60 + 5);
        c.summary_time = "99:99".into();
        assert_eq!(c.summary_minutes(), 15 * 60 + 45);
        assert!(c.validate(false, false).is_err());
    }

    #[test]
    fn telegram_needs_token_and_valid_chat_ids() {
        let mut c = AlertConfig::default();
        c.telegram.enabled = true;
        assert!(c.validate(false, false).is_err());
        assert!(c.validate(true, false).is_err()); // no chat id
        c.telegram.chat_ids = vec!["abc".into()];
        assert!(c.validate(true, false).is_err());
        c.telegram.chat_ids = vec!["-1001234".into(), "@mychannel".into()];
        assert!(c.validate(true, false).is_ok());
    }

    #[test]
    fn email_rules() {
        let mut c = AlertConfig::default();
        c.email = EmailConfig {
            enabled: true,
            host: "smtp.example.com".into(),
            from: "kissa@example.com".into(),
            to: vec!["me@example.com".into()],
            ..EmailConfig::default()
        };
        assert!(c.validate(false, false).is_ok());
        c.email.username = "user".into();
        assert!(c.validate(false, false).is_err()); // password missing
        assert!(c.validate(false, true).is_ok());
        c.email.security = "none".into();
        assert!(c.validate(false, true).is_err()); // plain SMTP to a remote host
        c.email.host = "localhost".into();
        assert!(c.validate(false, true).is_ok());
    }

    #[test]
    fn normalise_trims_and_dedupes() {
        let mut c = AlertConfig::default();
        c.telegram.chat_ids = vec![" 1 ".into(), "1".into(), "".into(), "2".into()];
        c.events.insert("bogus".into(), true);
        c.email.security = " TLS ".into();
        c.normalise();
        assert_eq!(c.telegram.chat_ids, vec!["1", "2"]);
        assert!(c.events.is_empty());
        assert_eq!(c.email.security, "tls");
    }
}
