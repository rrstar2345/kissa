//! Delivery channels: Telegram bot messages and SMTP email. Each function
//! sends one message to one target and returns a readable error. Secrets are
//! read from the keychain and never appear in error text.

use lettre::message::{header::ContentType, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::{json, Value};
use std::time::Duration;

use super::config::{AlertConfig, EmailConfig, SECRET_ACCOUNT, SMTP_PASSWORD, TELEGRAM_TOKEN};
use crate::secrets;

/// Telegram refuses messages over 4096 characters.
const TELEGRAM_MAX: usize = 3500;

/// One place a message is delivered to. Telegram chats are separate targets
/// so a retry never repeats a message to a chat that already got it.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Telegram(String),
    Email,
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Telegram(chat) => format!("telegram {chat}"),
            Target::Email => "email".into(),
        }
    }
}

pub fn secret_set(field: &str) -> bool {
    matches!(secrets::get(SECRET_ACCOUNT, field), Ok(Some(v)) if !v.is_empty())
}

/// Targets of every enabled and fully configured channel.
pub fn targets(cfg: &AlertConfig) -> Vec<Target> {
    let mut out = Vec::new();
    if cfg.telegram.enabled && secret_set(TELEGRAM_TOKEN) {
        out.extend(cfg.telegram.chat_ids.iter().map(|c| Target::Telegram(c.clone())));
    }
    if cfg.email.enabled && !cfg.email.host.is_empty() && !cfg.email.from.is_empty() && !cfg.email.to.is_empty() {
        out.push(Target::Email);
    }
    out
}

/// Cut text to `max` characters on a character boundary.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

pub async fn telegram(http: &reqwest::Client, chat_id: &str, text: &str) -> Result<(), String> {
    let token = secrets::get(SECRET_ACCOUNT, TELEGRAM_TOKEN)
        .map_err(|e| e.to_string())?
        .filter(|t| !t.is_empty())
        .ok_or("the Telegram bot token is not set")?;
    // The token is part of the URL: strip the URL from any error text.
    let resp = http
        .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
        .json(&json!({
            "chat_id": chat_id,
            "text": truncate(text, TELEGRAM_MAX),
            "disable_web_page_preview": true,
        }))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    let reason = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("description").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| status.to_string());
    Err(format!("Telegram said: {reason}"))
}

pub async fn email(cfg: &EmailConfig, subject: &str, body: &str) -> Result<(), String> {
    let from: Mailbox = cfg.from.parse().map_err(|e| format!("From address: {e}"))?;
    let mut msg = Message::builder().from(from).subject(subject);
    for to in &cfg.to {
        let mbox: Mailbox = to.parse().map_err(|e| format!("recipient “{to}”: {e}"))?;
        msg = msg.to(mbox);
    }
    let msg = msg.header(ContentType::TEXT_PLAIN).body(body.to_string()).map_err(|e| e.to_string())?;

    let host = cfg.host.as_str();
    let builder = match cfg.security.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
        _ => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)),
    }
    .map_err(|e| e.to_string())?;
    let mut builder = builder.port(cfg.port).timeout(Some(Duration::from_secs(20)));
    if !cfg.username.is_empty() {
        let password = secrets::get(SECRET_ACCOUNT, SMTP_PASSWORD)
            .map_err(|e| e.to_string())?
            .ok_or("the SMTP password is not set")?;
        builder = builder.credentials(Credentials::new(cfg.username.clone(), password));
    }
    builder.build().send(msg).await.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_characters() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("₹₹₹₹₹", 3), "₹₹…");
    }
}
