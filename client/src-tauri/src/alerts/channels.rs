//! Delivery channels: Telegram bot messages and SMTP email. Each function
//! sends one message to one target and returns a readable error. Secrets are
//! read from the keychain and never appear in error text.

use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::{json, Value};
use std::time::Duration;

use super::config::{AlertConfig, EmailConfig, SECRET_ACCOUNT, SMTP_PASSWORD, TELEGRAM_TOKEN};
use crate::secrets;

/// Cap for the plain-text Telegram fallback (the API refuses over 4096).
/// The HTML rendering is kept within its own budget by `render::Card`.
const TELEGRAM_PLAIN_MAX: usize = 3500;

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

// ----------------------------------------------------------- Telegram API ---

/// A failed Bot API call. `code` is the HTTP status, or 0 when the request
/// never got an answer (no token, network error).
#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: u16,
    pub message: String,
}

impl ApiError {
    fn local(message: impl Into<String>) -> Self {
        Self { code: 0, message: message.into() }
    }

    /// Text for the user: what Telegram said, or what went wrong locally.
    pub fn describe(&self) -> String {
        if self.code == 0 {
            self.message.clone()
        } else {
            format!("Telegram said: {}", self.message)
        }
    }
}

/// Call one Bot API method with a JSON body and return its `result`. The
/// token is part of the URL, so it is stripped from every error text.
pub async fn api_call(http: &reqwest::Client, method: &str, body: &Value) -> Result<Value, ApiError> {
    let token = secrets::get(SECRET_ACCOUNT, TELEGRAM_TOKEN)
        .map_err(|e| ApiError::local(e.to_string()))?
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::local("the Telegram bot token is not set"))?;
    let resp = http
        .post(format!("https://api.telegram.org/bot{token}/{method}"))
        .json(body)
        .send()
        .await
        .map_err(|e| ApiError::local(e.without_url().to_string()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| ApiError::local(e.without_url().to_string()))?;
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    if status.is_success() {
        return Ok(parsed.and_then(|v| v.get("result").cloned()).unwrap_or(Value::Null));
    }
    let message = parsed
        .as_ref()
        .and_then(|v| v.get("description"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| status.to_string());
    Err(ApiError { code: status.as_u16(), message })
}

/// Telegram could not read our markup (a bug on our side, never a reason to lose the alert).
fn bad_markup(e: &ApiError) -> bool {
    e.code == 400 && e.message.contains("parse entities")
}

/// Send `html` (Telegram HTML). If Telegram rejects the markup the message is
/// sent again as `plain` text, so an alert is never lost to a formatting problem.
pub async fn telegram(http: &reqwest::Client, chat_id: &str, html: &str, plain: &str) -> Result<(), String> {
    let first = api_call(
        http,
        "sendMessage",
        &json!({ "chat_id": chat_id, "text": html, "parse_mode": "HTML", "disable_web_page_preview": true }),
    )
    .await;
    match first {
        Ok(_) => Ok(()),
        Err(e) if bad_markup(&e) => api_call(
            http,
            "sendMessage",
            &json!({ "chat_id": chat_id, "text": truncate(plain, TELEGRAM_PLAIN_MAX), "disable_web_page_preview": true }),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.describe()),
        Err(e) => Err(e.describe()),
    }
}

// ------------------------------------------------------------------ email ---

/// Send a multipart message: `plain` for text-only clients, `html` for the rest.
pub async fn email(cfg: &EmailConfig, subject: &str, plain: &str, html: &str) -> Result<(), String> {
    let from: Mailbox = cfg.from.parse().map_err(|e| format!("From address: {e}"))?;
    let mut msg = Message::builder().from(from).subject(subject);
    for to in &cfg.to {
        let mbox: Mailbox = to.parse().map_err(|e| format!("recipient “{to}”: {e}"))?;
        msg = msg.to(mbox);
    }
    let msg = msg
        .multipart(MultiPart::alternative_plain_html(plain.to_string(), html.to_string()))
        .map_err(|e| e.to_string())?;

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

    #[test]
    fn markup_errors_are_recognised() {
        let e = ApiError { code: 400, message: "Bad Request: can't parse entities: Unsupported start tag \"x\" at byte offset 3".into() };
        assert!(bad_markup(&e));
        let other = ApiError { code: 400, message: "Bad Request: chat not found".into() };
        assert!(!bad_markup(&other));
        let rate = ApiError { code: 429, message: "Too Many Requests: retry after 5".into() };
        assert!(!bad_markup(&rate));
    }

    #[test]
    fn error_text_names_the_source() {
        assert_eq!(ApiError::local("the Telegram bot token is not set").describe(), "the Telegram bot token is not set");
        let e = ApiError { code: 403, message: "Forbidden: bot was blocked by the user".into() };
        assert_eq!(e.describe(), "Telegram said: Forbidden: bot was blocked by the user");
    }
}
