//! Audit journal: one searchable log of what happened in the app (orders,
//! risk halts, the kill switch, algo arm / disarm, copier actions, logins,
//! settings changes), shown on the Journal page.
//!
//! Anything can call `audit::record(Entry::new(..))`. It never blocks and
//! never fails the caller; a worker task resolves the account name, writes
//! the row, prunes old rows (90 days) and emits `audit-update`.
//!
//! The actor (who caused it) comes from a task-local set with `scope`: the
//! Tauri commands that place / cancel / modify orders run in `Actor::User`,
//! the algo runner and trade copier in their own scopes, and everything
//! else defaults to `Actor::System`. Secrets are never logged: callers do
//! not pass them and `Entry::detail` masks token / key / password fields.

pub mod events;
pub mod store;

use serde_json::Value;
use sqlx::SqlitePool;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::warn;

const MAX_DETAIL_BYTES: usize = 8 * 1024;
const BATCH: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Order,
    Risk,
    Algo,
    Copier,
    Account,
    Settings,
    System,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Order => "order",
            Category::Risk => "risk",
            Category::Algo => "algo",
            Category::Copier => "copier",
            Category::Account => "account",
            Category::Settings => "settings",
            Category::System => "system",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    User,
    System,
    Algo,
    Copier,
    Broker,
}

impl Actor {
    pub fn as_str(self) -> &'static str {
        match self {
            Actor::User => "user",
            Actor::System => "system",
            Actor::Algo => "algo",
            Actor::Copier => "copier",
            Actor::Broker => "broker",
        }
    }
}

tokio::task_local! {
    static ACTOR: Actor;
}

/// Run `f` with `actor` as the default actor of every entry it records.
/// Not inherited by tasks spawned inside `f`: scope those separately.
pub async fn scope<F: Future>(actor: Actor, f: F) -> F::Output {
    ACTOR.scope(actor, f).await
}

pub fn current_actor() -> Actor {
    ACTOR.try_with(|a| *a).unwrap_or(Actor::System)
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub category: Category,
    pub action: String,
    pub actor: Actor,
    pub account_id: Option<String>,
    pub subject: String,
    pub summary: String,
    pub detail: Option<Value>,
}

impl Entry {
    /// `action` is a short verb such as `placed`, `fill`, `armed`.
    pub fn new(category: Category, action: &str, summary: impl Into<String>) -> Self {
        Entry {
            category,
            action: action.to_string(),
            actor: current_actor(),
            account_id: None,
            subject: String::new(),
            summary: summary.into(),
            detail: None,
        }
    }

    pub fn actor(mut self, actor: Actor) -> Self {
        self.actor = actor;
        self
    }

    pub fn account(mut self, id: impl Into<String>) -> Self {
        let id = id.into();
        self.account_id = if id.trim().is_empty() || id == "*" { None } else { Some(id) };
        self
    }

    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = subject.into();
        self
    }

    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    /// The details as JSON text, secrets masked and size limited.
    pub(crate) fn detail_json(&self) -> Option<String> {
        let mut v = self.detail.clone()?;
        scrub(&mut v);
        let text = v.to_string();
        if text.len() <= MAX_DETAIL_BYTES {
            Some(text)
        } else {
            Some(serde_json::json!({ "truncated": clip(&text, 2000) }).to_string())
        }
    }
}

fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "apikey", "api_key", "api-key", "authorization", "authcode", "auth_code"]
        .iter()
        .any(|s| k.contains(s))
}

/// Replace the value of every secret-looking key, at any depth, with `***`.
pub(crate) fn scrub(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if is_secret_key(k) {
                    *val = Value::String("***".into());
                } else {
                    scrub(val);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(scrub),
        _ => {}
    }
}

/// At most `max` characters, ending in `…` when cut.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------- service ---

pub struct AuditService {
    tx: UnboundedSender<Entry>,
}

static GLOBAL: OnceLock<Arc<AuditService>> = OnceLock::new();

/// Queue an entry. A no-op until the service is installed (unit tests).
pub fn record(entry: Entry) {
    if let Some(s) = GLOBAL.get() {
        let _ = s.tx.send(entry);
    }
}

impl AuditService {
    /// Start the writer and make `record` work from anywhere.
    pub fn start(pool: SqlitePool, app: AppHandle) -> Arc<Self> {
        let (tx, rx) = unbounded_channel();
        let svc = Arc::new(AuditService { tx });
        let _ = GLOBAL.set(svc.clone());
        tauri::async_runtime::spawn(run(pool, app, rx));
        svc
    }
}

async fn run(pool: SqlitePool, app: AppHandle, mut rx: UnboundedReceiver<Entry>) {
    // The first tick is immediate: old rows are pruned at startup, then daily.
    let mut prune_tick = tokio::time::interval(Duration::from_secs(24 * 3600));
    loop {
        tokio::select! {
            _ = prune_tick.tick() => {
                if let Err(e) = store::prune(&pool).await {
                    warn!("Audit journal prune failed: {e}");
                }
            }
            first = rx.recv() => {
                let Some(first) = first else { break };
                let mut batch = vec![first];
                while batch.len() < BATCH {
                    match rx.try_recv() {
                        Ok(e) => batch.push(e),
                        Err(_) => break,
                    }
                }
                for e in &batch {
                    let name = match e.account_id.as_deref() {
                        Some(id) => crate::accounts::get(&pool, id).await.map(|a| a.name).ok(),
                        None => None,
                    };
                    if let Err(err) = store::insert(&pool, e, name.as_deref()).await {
                        warn!("Audit journal write failed: {err}");
                    }
                }
                crate::emit_stats::count("audit-update");
                let _ = app.emit("audit-update", true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_fields_are_masked_at_any_depth() {
        let mut v = json!({
            "url": "https://x", "apiKey": "k1", "Access_Token": "t",
            "list": [{ "password": "p", "n": 1 }], "deep": { "clientSecret": "s", "ok": true }
        });
        scrub(&mut v);
        assert_eq!(v["url"], "https://x");
        assert_eq!(v["apiKey"], "***");
        assert_eq!(v["Access_Token"], "***");
        assert_eq!(v["list"][0]["password"], "***");
        assert_eq!(v["list"][0]["n"], 1);
        assert_eq!(v["deep"]["clientSecret"], "***");
        assert_eq!(v["deep"]["ok"], true);
    }

    #[test]
    fn clip_is_char_safe() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "ab…");
        assert_eq!(clip("₹₹₹₹", 3).chars().count(), 3);
        assert_eq!(clip("", 0), "");
    }

    #[test]
    fn oversized_detail_is_truncated() {
        let e = Entry::new(Category::System, "t", "s").detail(json!({ "big": "x".repeat(20_000) }));
        let text = e.detail_json().unwrap();
        assert!(text.len() < 4000);
        assert!(text.contains("truncated"));
        assert!(Entry::new(Category::System, "t", "s").detail_json().is_none());
    }

    #[test]
    fn account_star_and_blank_mean_no_account() {
        assert!(Entry::new(Category::Risk, "t", "s").account("*").account_id.is_none());
        assert!(Entry::new(Category::Risk, "t", "s").account(" ").account_id.is_none());
        assert_eq!(Entry::new(Category::Risk, "t", "s").account("a1").account_id.as_deref(), Some("a1"));
    }

    #[tokio::test]
    async fn actor_comes_from_the_scope() {
        assert_eq!(current_actor(), Actor::System);
        let inside = scope(Actor::Algo, async { Entry::new(Category::Order, "t", "s").actor }).await;
        assert_eq!(inside, Actor::Algo);
        let nested = scope(Actor::User, async { scope(Actor::Copier, async { current_actor() }).await }).await;
        assert_eq!(nested, Actor::Copier);
        // An explicit actor wins.
        let e = scope(Actor::User, async { Entry::new(Category::Order, "t", "s").actor(Actor::Broker) }).await;
        assert_eq!(e.actor, Actor::Broker);
    }
}
