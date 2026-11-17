//! Telegram listener: receives messages sent to the bot (long-polling
//! `getUpdates`, so no public URL is needed), finds chat IDs for the settings
//! page and answers read-only commands.
//!
//! One supervisor task owns `getUpdates` (Telegram allows one consumer per bot):
//!   * **Commands** mode: Telegram enabled, "accept commands" on and at least
//!     one allow-listed chat. Commands are answered only for chats on the
//!     allow-list (the same chat IDs alerts go to); everyone else is ignored
//!     silently, without a reply.
//!   * **Detect** mode: for two minutes after the settings page asks, every chat
//!     that writes to the bot is remembered so its ID can be offered to the user.
//!   * Otherwise it sleeps and makes no network calls.
//!
//! Commands here only read (`/positions`, `/pnl`, `/algos`, `/status`). Anything
//! that changes state (arm, kill switch, exit) is a later phase and will need
//! its own opt-in and confirmation step.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::channels::{self, ApiError};
use super::config::TELEGRAM_TOKEN;
use super::render::{ist_stamp, rupees, Card};
use crate::state::SharedState;
use crate::trading::pricing::r2;
use crate::trading::Position;

/// Long-poll length handed to Telegram.
const POLL_SECS: u64 = 25;
/// Commands older than this are ignored (messages queued while the app was closed).
const STALE_SECS: i64 = 120;
/// How long "Detect chat ID" listens after it is pressed.
const DETECT_SECS: u64 = 120;
/// Chats remembered for the settings page.
const MAX_SEEN: usize = 20;

const BLUE: &str = "#2f6fdb";
const TEAL: &str = "#1c8f8f";
const RED: &str = "#d64545";

/// The command menu Telegram shows next to the message box.
const MENU: [(&str, &str); 5] = [
    ("positions", "Open positions with live P&L"),
    ("pnl", "Today's P&L by account"),
    ("algos", "Algos: armed or not, in a position or flat"),
    ("status", "Market, feeds, risk halts and algos"),
    ("help", "List the commands"),
];

// ------------------------------------------------------------ shared state ---

/// A chat that wrote to the bot, as offered by "Detect chat ID".
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SeenChat {
    /// Kept as text: group IDs are long negative numbers.
    pub id: String,
    /// `private` | `group` | `supergroup` | `channel`
    pub kind: String,
    pub name: String,
    pub username: String,
    /// Unix seconds of the last message seen.
    pub last_seen: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotStatus {
    /// The supervisor is receiving updates right now.
    pub listening: bool,
    /// Why it is not (token rejected, another program using the bot, network).
    pub error: String,
}

/// What the settings page shows while detecting a chat ID.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectState {
    pub active: bool,
    pub chats: Vec<SeenChat>,
    pub listening: bool,
    pub error: String,
}

pub struct BotShared {
    seen: Mutex<Vec<SeenChat>>,
    detect_until: Mutex<Option<Instant>>,
    status: Mutex<BotStatus>,
    /// Wakes the supervisor from its idle sleep (settings saved, detection started).
    pub wake: Notify,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl BotShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            detect_until: Mutex::new(None),
            status: Mutex::new(BotStatus::default()),
            wake: Notify::new(),
        })
    }

    pub fn detecting(&self) -> bool {
        matches!(*lock(&self.detect_until), Some(t) if Instant::now() < t)
    }

    pub fn start_detect(&self) {
        *lock(&self.detect_until) = Some(Instant::now() + Duration::from_secs(DETECT_SECS));
        self.wake.notify_one();
    }

    pub fn stop_detect(&self) {
        *lock(&self.detect_until) = None;
    }

    pub fn status(&self) -> BotStatus {
        lock(&self.status).clone()
    }

    fn set_status(&self, listening: bool, error: impl Into<String>) {
        *lock(&self.status) = BotStatus { listening, error: error.into() };
    }

    pub fn detect_state(&self) -> DetectState {
        let status = self.status();
        DetectState {
            active: self.detecting(),
            chats: lock(&self.seen).clone(),
            listening: status.listening,
            error: status.error,
        }
    }

    /// Remember a chat (newest first, one entry per chat).
    fn record_chat(&self, chat: &Chat, now: i64) {
        let id = chat.id.to_string();
        let mut seen = lock(&self.seen);
        seen.retain(|c| c.id != id);
        seen.insert(
            0,
            SeenChat {
                id,
                kind: chat.kind.clone(),
                name: chat.display_name(),
                username: chat.username.clone().unwrap_or_default(),
                last_seen: now,
            },
        );
        seen.truncate(MAX_SEEN);
    }
}

// ----------------------------------------------------------------- updates ---

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type", default)]
    kind: String,
    title: Option<String>,
    username: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
}

impl Chat {
    fn display_name(&self) -> String {
        if let Some(t) = self.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            return t.to_string();
        }
        let person = format!("{} {}", self.first_name.as_deref().unwrap_or(""), self.last_name.as_deref().unwrap_or(""));
        let person = person.trim();
        if !person.is_empty() {
            return person.to_string();
        }
        self.username.clone().unwrap_or_else(|| "(no name)".into())
    }
}

#[derive(Debug, Deserialize)]
struct Msg {
    #[serde(default)]
    date: i64,
    chat: Chat,
    text: Option<String>,
}

/// `/Positions@MyBot now` -> `positions`.
fn parse_command(text: &str) -> Option<String> {
    let first = text.trim().strip_prefix('/')?.split_whitespace().next()?;
    let name = first.split('@').next().unwrap_or("").to_lowercase();
    (!name.is_empty()).then_some(name)
}

/// The chat is on the allow-list (numeric IDs; `@channel` entries never match a message sender).
fn is_allowed(chat_ids: &[String], chat_id: i64) -> bool {
    let id = chat_id.to_string();
    chat_ids.iter().any(|c| c.trim() == id)
}

fn is_stale(now: i64, sent: i64) -> bool {
    now - sent > STALE_SECS
}

async fn poll(http: &reqwest::Client, offset: Option<i64>) -> Result<Vec<Value>, ApiError> {
    let mut body = json!({ "timeout": POLL_SECS, "allowed_updates": ["message"] });
    if let Some(o) = offset {
        body["offset"] = json!(o);
    }
    let result = channels::api_call(http, "getUpdates", &body).await?;
    Ok(result.as_array().cloned().unwrap_or_default())
}

// -------------------------------------------------------------- supervisor ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Detect,
    Commands,
}

fn mode(state: &SharedState, shared: &BotShared) -> Mode {
    if !channels::secret_set(TELEGRAM_TOKEN) {
        return Mode::Off;
    }
    let cfg = state.alerts.config();
    if cfg.telegram.enabled && cfg.telegram.listen && !cfg.telegram.chat_ids.is_empty() {
        Mode::Commands
    } else if shared.detecting() {
        Mode::Detect
    } else {
        Mode::Off
    }
}

/// What to show and how long to wait after a failed poll.
fn classify(e: &ApiError, failures: u32) -> (String, u64) {
    match e.code {
        401 => ("Telegram rejected the bot token. Check it and save again.".into(), 60),
        409 => (
            "Another program is receiving this bot's messages (a second KISSA using the same bot, or a webhook). Only one can listen at a time.".into(),
            30,
        ),
        429 => ("Telegram asked to slow down; trying again shortly.".into(), 30),
        _ => (e.describe(), (5u64 << failures.saturating_sub(1).min(4)).min(60)),
    }
}

pub fn spawn(state: SharedState) {
    tauri::async_runtime::spawn(run(state));
}

async fn run(state: SharedState) {
    let shared = state.alerts.bot.clone();
    let http = reqwest::Client::builder().timeout(Duration::from_secs(POLL_SECS + 15)).build().unwrap_or_default();
    // Kept across idle periods so confirmed updates are not delivered again.
    let mut offset: Option<i64> = None;
    let mut failures: u32 = 0;
    let mut menu_set = false;
    loop {
        let current = mode(&state, &shared);
        if current == Mode::Off {
            shared.set_status(false, "");
            failures = 0;
            menu_set = false;
            let _ = tokio::time::timeout(Duration::from_secs(30), shared.wake.notified()).await;
            continue;
        }
        match poll(&http, offset).await {
            Ok(updates) => {
                if failures > 0 {
                    info!("Telegram listener recovered");
                }
                failures = 0;
                shared.set_status(true, "");
                if current == Mode::Commands && !menu_set {
                    menu_set = true;
                    set_menu(&http).await;
                }
                for update in &updates {
                    if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                        offset = Some(offset.map_or(id + 1, |o| o.max(id + 1)));
                    }
                    handle(&state, &http, update).await;
                }
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                let (text, wait) = classify(&e, failures);
                if failures == 1 {
                    warn!("Telegram listener: {text}");
                } else {
                    debug!("Telegram listener still failing: {text}");
                }
                shared.set_status(false, text);
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        }
    }
}

/// Register the command menu. Best effort: a failure only means no menu.
async fn set_menu(http: &reqwest::Client) {
    let commands: Vec<Value> = MENU.iter().map(|(c, d)| json!({ "command": c, "description": d })).collect();
    if let Err(e) = channels::api_call(http, "setMyCommands", &json!({ "commands": commands })).await {
        debug!("Telegram command menu not set: {}", e.describe());
    }
}

async fn handle(state: &SharedState, http: &reqwest::Client, update: &Value) {
    let Some(msg) = update.get("message").and_then(|m| serde_json::from_value::<Msg>(m.clone()).ok()) else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    // Always remembered, so "Detect chat ID" finds chats that wrote before it was pressed.
    state.alerts.bot.record_chat(&msg.chat, now);

    let cfg = state.alerts.config();
    if !(cfg.telegram.enabled && cfg.telegram.listen) {
        return;
    }
    if !is_allowed(&cfg.telegram.chat_ids, msg.chat.id) {
        debug!("Telegram: ignored a message from chat {} (not on the allow-list)", msg.chat.id);
        return;
    }
    if is_stale(now, msg.date) {
        return;
    }
    let Some(command) = msg.text.as_deref().and_then(parse_command) else { return };
    let card = answer(state, &command).await;
    let chat = msg.chat.id.to_string();
    if let Err(e) = channels::telegram(http, &chat, &card.telegram_html(&ist_stamp()), &card.plain()).await {
        warn!("Telegram: could not answer /{command} in chat {chat}: {e}");
    }
}

// ---------------------------------------------------------------- commands ---

async fn answer(state: &SharedState, command: &str) -> Card {
    let result = match command {
        "start" | "help" => Ok(help_card()),
        "positions" | "pos" => positions_card(state).await,
        "pnl" => pnl_card(state).await,
        "algos" => algos_card(state).await,
        "status" => status_card(state).await,
        other => Ok(Card::new("❓", BLUE, "Help", "Unknown command").text(format!("/{other} is not a command. Send /help for the list."))),
    };
    result.unwrap_or_else(|e| Card::new("⚠️", RED, "Error", "Could not read that").text(e.to_string()))
}

fn help_card() -> Card {
    let mut card = Card::new("🤖", BLUE, "Help", "KISSA commands");
    for (command, what) in MENU {
        card.add_text(format!("/{command} · {what}"));
    }
    card.add_text("These commands only read; nothing here changes an order, an algo or a limit.");
    card
}

/// `NIFTY 24000 CE 2026-11-24 · +65 (1 lot) @ 101.50 → 112.00 · +₹682.50`
fn position_line(p: &Position) -> String {
    let lots = if p.lot_size > 0 { format!(" ({} lot)", p.net_qty.abs() / p.lot_size) } else { String::new() };
    let price = match p.ltp {
        Some(v) if p.ltp_source == "live" => format!("{v:.2}"),
        Some(v) => format!("{v:.2} (last)"),
        None => "no price".to_string(),
    };
    let pnl = p.unrealised.map(rupees).unwrap_or_else(|| "—".into());
    format!(
        "{} {} {} {} · {:+}{lots} @ {:.2} → {price} · {pnl}",
        p.symbol,
        p.strike,
        if p.is_call { "CE" } else { "PE" },
        p.expiry,
        p.net_qty,
        p.avg_price
    )
}

async fn positions_card(state: &SharedState) -> anyhow::Result<Card> {
    let dash = state.trading.dashboard(None).await?;
    let mut card = Card::new("📋", BLUE, "Positions", "Open positions");
    if dash.positions.is_empty() {
        card.add_text("No open positions.");
        return Ok(card);
    }
    let total: f64 = dash.positions.iter().filter_map(|p| p.unrealised).sum();
    card.add_field("Open", dash.positions.len().to_string());
    card.add_field("Unrealised", rupees(r2(total)));
    for account in &dash.accounts {
        let mine: Vec<&Position> = dash.positions.iter().filter(|p| p.account_id == account.account_id).collect();
        if mine.is_empty() {
            continue;
        }
        card.add_heading(format!("{} ({}) · {}", account.name, account.kind, rupees(account.unrealised)));
        for p in mine {
            card.add_text(position_line(p));
        }
    }
    Ok(card)
}

async fn pnl_card(state: &SharedState) -> anyhow::Result<Card> {
    let dash = state.trading.dashboard(None).await?;
    let mut card = Card::new("📊", BLUE, "P&L", format!("P&L today · {}", dash.trade_date));
    let active: Vec<_> =
        dash.accounts.iter().filter(|a| a.orders_today > 0 || a.trades_today > 0 || a.open_positions > 0).collect();
    if active.is_empty() {
        card.add_text("No activity today.");
        return Ok(card);
    }
    let total: f64 = active.iter().map(|a| a.net_today).sum();
    card.add_field("Net today", rupees(r2(total)));
    for a in active {
        card.add_heading(format!("{} ({})", a.name, a.kind));
        card.add_text(format!(
            "Net {} · realised {} · unrealised {}",
            rupees(a.net_today),
            rupees(a.realised_today),
            rupees(a.unrealised)
        ));
        card.add_text(format!("Charges ₹{:.2} · {} trades · {} open", a.charges_today, a.trades_today, a.open_positions));
    }
    Ok(card)
}

async fn algos_card(state: &SharedState) -> anyhow::Result<Card> {
    let views = crate::algo::service::list_views(&state.db).await?;
    let mut card = Card::new("🤖", TEAL, "Algos", "Algos");
    if views.is_empty() {
        card.add_text("No algos saved.");
        return Ok(card);
    }
    let armed = views.iter().filter(|v| v.armed).count();
    let in_position = views.iter().filter(|v| v.phase == crate::algo::PHASE_IN).count();
    card.add_field("Armed", format!("{armed} of {}", views.len()));
    card.add_field("In a position", in_position.to_string());
    for v in &views {
        let position = if v.phase == crate::algo::PHASE_IN { format!("in position ({} leg(s))", v.legs.len()) } else { "flat".into() };
        card.add_text(format!(
            "{} {} · {} · {} {} · {position}",
            if v.armed { "●" } else { "○" },
            v.algo.name,
            if v.armed { "armed" } else { "off" },
            v.algo.instrument.underlying,
            v.algo.interval
        ));
    }
    Ok(card)
}

async fn status_card(state: &SharedState) -> anyhow::Result<Card> {
    let market = crate::commands::build_market_status(state).await;
    let risk = state.trading.risk_overview().await?;
    let states = crate::algo::store::list_states(&state.db).await?;
    let mut card = Card::new("ℹ️", BLUE, "Status", "KISSA status");
    card.add_field("Market", format!("{} {:?}", market.source.label(), market.exchange.state));
    card.add_field("Feed", market.feed.clone());
    if let Some(auth) = serde_json::to_value(&market.fyers_auth)
        .ok()
        .and_then(|v| v.get("state").and_then(Value::as_str).map(String::from))
        .filter(|s| s != "ok")
    {
        card.add_field("Fyers login", auth);
    }
    if let Some(issue) = &market.api_health {
        card.add_field("Data API", issue.detail());
    }
    card.add_field(
        "Kill switch",
        match &risk.kill_switch {
            Some(h) => format!("ON since {}", h.since),
            None => "off".into(),
        },
    );
    for a in &risk.accounts {
        if let Some(h) = &a.halt {
            card.add_field(format!("Halted: {}", a.name), h.reason.clone());
        }
    }
    card.add_field(
        "Algos",
        format!(
            "{} armed · {} in a position",
            states.values().filter(|s| s.armed).count(),
            states.values().filter(|s| s.in_position()).count()
        ),
    );
    card.add_field(
        "Mode",
        format!(
            "{} · up {}",
            if crate::headless::is_headless() { "headless" } else { "desktop" },
            uptime(crate::headless::uptime_secs())
        ),
    );
    Ok(card)
}

/// `3h 05m`, `12m`, `40s`.
fn uptime(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: i64, kind: &str) -> Chat {
        Chat { id, kind: kind.into(), title: None, username: None, first_name: Some("Asha".into()), last_name: Some("K".into()) }
    }

    fn position(net_qty: i64, ltp: Option<f64>, source: &str) -> Position {
        Position {
            account_id: "a".into(),
            account_name: "Paper 1".into(),
            account_kind: "paper".into(),
            instrument: "NIFTY".into(),
            symbol: "NIFTY".into(),
            expiry: "2026-11-24".into(),
            strike: 24000.0,
            is_call: true,
            product: "INTRADAY".into(),
            lot_size: 65,
            net_qty,
            avg_price: 101.5,
            ltp,
            ltp_source: source.into(),
            unrealised: ltp.map(|l| (l - 101.5) * net_qty as f64),
            exit_price: None,
        }
    }

    #[test]
    fn commands_are_parsed() {
        assert_eq!(parse_command("/positions").as_deref(), Some("positions"));
        assert_eq!(parse_command("  /PnL@KissaBot now ").as_deref(), Some("pnl"));
        assert_eq!(parse_command("/help extra words").as_deref(), Some("help"));
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command("/"), None);
        assert_eq!(parse_command("/@bot"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn only_listed_chats_are_allowed() {
        let ids = vec!["123".to_string(), " -1001234 ".to_string(), "@channel".to_string()];
        assert!(is_allowed(&ids, 123));
        assert!(is_allowed(&ids, -1001234));
        assert!(!is_allowed(&ids, 124));
        assert!(!is_allowed(&ids, 0));
        assert!(!is_allowed(&[], 123));
    }

    #[test]
    fn old_messages_are_stale() {
        assert!(!is_stale(1_000, 950));
        assert!(!is_stale(1_000, 880));
        assert!(is_stale(1_000, 879));
        assert!(is_stale(1_000, 0), "a missing date is treated as old");
    }

    #[test]
    fn chat_names() {
        assert_eq!(chat(1, "private").display_name(), "Asha K");
        let mut c = chat(2, "group");
        c.title = Some("Desk".into());
        assert_eq!(c.display_name(), "Desk");
        let anon = Chat { id: 3, kind: "private".into(), title: None, username: Some("ak".into()), first_name: None, last_name: None };
        assert_eq!(anon.display_name(), "ak");
        let none = Chat { id: 4, kind: "private".into(), title: None, username: None, first_name: None, last_name: None };
        assert_eq!(none.display_name(), "(no name)");
    }

    #[test]
    fn seen_chats_are_unique_newest_first_and_capped() {
        let shared = BotShared::new();
        shared.record_chat(&chat(1, "private"), 10);
        shared.record_chat(&chat(2, "group"), 20);
        shared.record_chat(&chat(1, "private"), 30);
        let ids: Vec<String> = shared.detect_state().chats.into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["1", "2"]);
        for i in 0..50 {
            shared.record_chat(&chat(100 + i, "private"), 40 + i);
        }
        assert_eq!(shared.detect_state().chats.len(), MAX_SEEN);
        assert_eq!(shared.detect_state().chats[0].id, "149");
    }

    #[test]
    fn detection_window() {
        let shared = BotShared::new();
        assert!(!shared.detecting());
        shared.start_detect();
        assert!(shared.detecting());
        assert!(shared.detect_state().active);
        shared.stop_detect();
        assert!(!shared.detecting());
    }

    #[test]
    fn failures_back_off_and_cap() {
        let net = ApiError { code: 0, message: "timeout".into() };
        assert_eq!(classify(&net, 1).1, 5);
        assert_eq!(classify(&net, 2).1, 10);
        assert_eq!(classify(&net, 3).1, 20);
        assert_eq!(classify(&net, 4).1, 40);
        assert_eq!(classify(&net, 5).1, 60);
        assert_eq!(classify(&net, 50).1, 60);
        let conflict = ApiError { code: 409, message: "Conflict".into() };
        assert!(classify(&conflict, 1).0.contains("Another program"));
        let token = ApiError { code: 401, message: "Unauthorized".into() };
        assert!(classify(&token, 1).0.contains("rejected the bot token"));
    }

    #[test]
    fn position_lines() {
        let long = position(65, Some(112.0), "live");
        assert_eq!(position_line(&long), "NIFTY 24000 CE 2026-11-24 · +65 (1 lot) @ 101.50 → 112.00 · +₹682.50");
        let last = position(-130, Some(100.0), "last");
        assert_eq!(position_line(&last), "NIFTY 24000 CE 2026-11-24 · -130 (2 lot) @ 101.50 → 100.00 (last) · +₹195.00");
        let blind = position(65, None, "none");
        assert_eq!(position_line(&blind), "NIFTY 24000 CE 2026-11-24 · +65 (1 lot) @ 101.50 → no price · —");
    }

    #[test]
    fn uptime_text() {
        assert_eq!(uptime(40), "40s");
        assert_eq!(uptime(125), "2m");
        assert_eq!(uptime(3 * 3600 + 5 * 60 + 9), "3h 05m");
    }

    #[test]
    fn help_lists_every_menu_command() {
        let plain = help_card().plain();
        for (command, _) in MENU {
            assert!(plain.contains(&format!("/{command}")), "{command}");
        }
    }
}
