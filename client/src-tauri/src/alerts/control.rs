//! Telegram control buttons (phase 2 of the Telegram bot): arm / disarm an
//! algo, kill switch on / off, lift a daily-loss halt, exit one position, exit
//! all.
//!
//! Safety rules, all enforced here:
//!   * Off unless "Allow control buttons" is on (`telegram.controls`, default
//!     off) together with the listener.
//!   * Only private chats on the allow-list whose sender id equals the chat id
//!     (so a group, a channel or a forwarded button never acts).
//!   * Everything except *disarm* asks first: the bot sends a confirmation
//!     message with its own buttons that works once, in the same chat and for
//!     the same person, for `CONFIRM_SECS`.
//!   * Buttons on messages older than `BUTTON_MAX_AGE_SECS` are refused; the
//!     user is pointed to `/control` for fresh ones.
//!   * Every action runs through the normal services under `Actor::Telegram`,
//!     so the order gate, risk limits and journal all still apply, and each
//!     run adds its own audit entry.
//!
//! Callback data is limited to 64 bytes by Telegram. Buttons therefore carry
//! only a short action code plus an id (`arm:<algo id>`, `exit:<position
//! key>`); the confirmation step keeps its state in memory (`Prompts`).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tracing::debug;

use super::bot::is_allowed;
use super::channels;
use super::config::TelegramConfig;
use super::render::{cut, ist_stamp, rupees, Card};
use crate::audit::{self, Actor, Category, Entry};
use crate::state::SharedState;
use crate::trading::Position;

/// How long a confirmation message stays valid.
pub const CONFIRM_SECS: u64 = 30;
/// Buttons on messages older than this are refused.
pub const BUTTON_MAX_AGE_SECS: i64 = 6 * 3600;
/// Telegram's limit for callback data.
const MAX_DATA: usize = 64;
const BUTTON_TEXT_MAX: usize = 40;
/// Open confirmations kept in memory.
const MAX_PROMPTS: usize = 50;
/// Buttons listed per kind on /positions, /algos and /control.
const MAX_ROWS: usize = 10;

const AMBER: &str = "#d99100";
const GREEN: &str = "#2e9e5b";
const RED: &str = "#d64545";
const BLUE: &str = "#2f6fdb";

// ----------------------------------------------------------------- buttons ---

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub text: String,
    pub data: String,
}

impl Button {
    pub fn new(text: impl AsRef<str>, data: impl Into<String>) -> Self {
        Self { text: cut(text.as_ref(), BUTTON_TEXT_MAX), data: data.into() }
    }
}

/// Rows of buttons.
pub type Keyboard = Vec<Vec<Button>>;

/// The Bot API `reply_markup`. Buttons whose data is too long are dropped;
/// `None` when nothing is left.
pub fn markup(kb: &Keyboard) -> Option<Value> {
    let rows: Vec<Value> = kb
        .iter()
        .map(|row| {
            row.iter()
                .filter(|b| b.data.len() <= MAX_DATA)
                .map(|b| json!({ "text": b.text, "callback_data": b.data }))
                .collect::<Vec<Value>>()
        })
        .filter(|row| !row.is_empty())
        .map(Value::from)
        .collect();
    (!rows.is_empty()).then(|| json!({ "inline_keyboard": rows }))
}

/// Buttons for an alert sent to `chat`: only when controls are on and the
/// target is a private chat (user ids are positive; groups are negative and
/// channels are `@names`).
pub fn alert_markup(cfg: &TelegramConfig, chat: &str, kb: &Keyboard) -> Option<Value> {
    if !(cfg.enabled && cfg.listen && cfg.controls) {
        return None;
    }
    match chat.trim().parse::<i64>() {
        Ok(id) if id > 0 => markup(kb),
        _ => None,
    }
}

/// May this chat and sender use the control buttons?
pub fn controls_allowed(cfg: &TelegramConfig, chat_id: i64, chat_kind: &str, sender: Option<i64>) -> bool {
    cfg.enabled
        && cfg.listen
        && cfg.controls
        && chat_kind == "private"
        && chat_id > 0
        && sender == Some(chat_id)
        && is_allowed(&cfg.chat_ids, chat_id)
}

/// Buttons on messages older than the limit (or without a date) are refused.
pub fn button_too_old(now: i64, sent: i64) -> bool {
    sent <= 0 || now - sent > BUTTON_MAX_AGE_SECS
}

// ----------------------------------------------------------------- actions ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Arm(String),
    Disarm(String),
    /// Lift today's daily-loss halt of an account (account id).
    Resume(String),
    Kill { flatten: bool },
    KillOff,
    /// Exit one position, identified by `pos_key`.
    ExitOne(String),
    ExitAll,
}

impl Action {
    /// Only disarming (it can only reduce activity) runs without asking.
    pub fn needs_confirm(&self) -> bool {
        !matches!(self, Action::Disarm(_))
    }

    /// Short name for the audit entry.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Arm(_) => "arm",
            Action::Disarm(_) => "disarm",
            Action::Resume(_) => "resume",
            Action::Kill { flatten: false } => "kill",
            Action::Kill { flatten: true } => "kill_flatten",
            Action::KillOff => "kill_off",
            Action::ExitOne(_) => "exit",
            Action::ExitAll => "exit_all",
        }
    }

    /// Callback data of the first-level button. Kill is a menu (`Press::Kill`)
    /// and has no data of its own.
    pub fn data(&self) -> String {
        match self {
            Action::Arm(id) => format!("arm:{id}"),
            Action::Disarm(id) => format!("dis:{id}"),
            Action::Resume(id) => format!("res:{id}"),
            Action::Kill { .. } => KILL.to_string(),
            Action::KillOff => "unkill".to_string(),
            Action::ExitOne(key) => format!("exit:{key}"),
            Action::ExitAll => "exall".to_string(),
        }
    }
}

const KILL: &str = "kill";

/// What a pressed button means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Press {
    Act(Action),
    /// Opens the kill switch confirmation (kill only, or kill and exit all).
    Kill,
    /// A confirmation button: prompt id and option index.
    Confirm(String, usize),
    Cancel(String),
}

impl Press {
    pub fn parse(data: &str) -> Option<Press> {
        let rest = |prefix: &str| data.strip_prefix(prefix).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        match data {
            KILL => return Some(Press::Kill),
            "unkill" => return Some(Press::Act(Action::KillOff)),
            "exall" => return Some(Press::Act(Action::ExitAll)),
            _ => {}
        }
        if let Some(r) = rest("arm:") {
            return Some(Press::Act(Action::Arm(r)));
        }
        if let Some(r) = rest("dis:") {
            return Some(Press::Act(Action::Disarm(r)));
        }
        if let Some(r) = rest("res:") {
            return Some(Press::Act(Action::Resume(r)));
        }
        if let Some(r) = rest("exit:") {
            return Some(Press::Act(Action::ExitOne(r)));
        }
        if let Some(r) = rest("ok:") {
            let (id, idx) = r.split_once(':')?;
            return Some(Press::Confirm(id.to_string(), idx.parse().ok()?));
        }
        rest("no:").map(Press::Cancel)
    }
}

/// Stable short id of a position for callback data (account + instrument +
/// product). Resolved against the open positions when the button is pressed.
pub fn pos_key(account_id: &str, instrument: &str, product: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{account_id}|{instrument}|{product}").as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn position_key(p: &Position) -> String {
    pos_key(&p.account_id, &p.instrument, &p.product)
}

/// `NIFTY 24000 CE`
fn contract_text(p: &Position) -> String {
    format!("{} {} {}", p.symbol, p.strike, if p.is_call { "CE" } else { "PE" })
}

// ----------------------------------------------------- button constructors ---

pub fn btn_arm(algo_id: &str, name: &str) -> Button {
    Button::new(format!("▶️ Arm {name}"), Action::Arm(algo_id.into()).data())
}

pub fn btn_disarm(algo_id: &str, name: &str) -> Button {
    Button::new(format!("⏸ Disarm {name}"), Action::Disarm(algo_id.into()).data())
}

pub fn btn_resume(account_id: &str, name: &str) -> Button {
    Button::new(format!("▶️ Resume {name}"), Action::Resume(account_id.into()).data())
}

pub fn btn_kill() -> Button {
    Button::new("🛑 Kill switch", KILL)
}

pub fn btn_kill_off() -> Button {
    Button::new("✅ Release kill switch", Action::KillOff.data())
}

pub fn btn_exit_all() -> Button {
    Button::new("⏏ Exit all positions", Action::ExitAll.data())
}

fn btn_exit(p: &Position) -> Button {
    Button::new(format!("⏏ Exit {} · {}", contract_text(p), p.account_name), Action::ExitOne(position_key(p)).data())
}

// ---------------------------------------------------------- confirmations ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeError {
    /// Unknown, already used or removed.
    Gone,
    Expired,
    /// Pressed from another chat or by another person.
    Mismatch,
}

struct Prompt {
    chat: i64,
    user: i64,
    at: Instant,
    options: Vec<Action>,
}

/// Confirmations waiting for a press. In memory only: a restart drops them,
/// which is the safe direction.
#[derive(Default)]
pub struct Prompts(Mutex<HashMap<String, Prompt>>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Prompts {
    /// Remember a confirmation and return its id.
    pub fn add(&self, chat: i64, user: i64, options: Vec<Action>, now: Instant) -> String {
        let mut map = lock(&self.0);
        let ttl = Duration::from_secs(CONFIRM_SECS);
        map.retain(|_, p| now.saturating_duration_since(p.at) <= ttl);
        while map.len() >= MAX_PROMPTS {
            let Some(oldest) = map.iter().min_by_key(|(_, p)| p.at).map(|(k, _)| k.clone()) else { break };
            map.remove(&oldest);
        }
        let id: String = uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect();
        map.insert(id.clone(), Prompt { chat, user, at: now, options });
        id
    }

    /// Use option `idx` of a confirmation. It works once: a valid press
    /// removes the prompt, so do the other options.
    pub fn take(&self, id: &str, chat: i64, user: i64, idx: usize, now: Instant) -> Result<Action, TakeError> {
        let mut map = lock(&self.0);
        let Some(p) = map.get(id) else { return Err(TakeError::Gone) };
        if p.chat != chat || p.user != user {
            return Err(TakeError::Mismatch);
        }
        let expired = now.saturating_duration_since(p.at) > Duration::from_secs(CONFIRM_SECS);
        let Some(p) = map.remove(id) else { return Err(TakeError::Gone) };
        if expired {
            return Err(TakeError::Expired);
        }
        p.options.get(idx).cloned().ok_or(TakeError::Gone)
    }

    /// Drop a confirmation (Cancel). Returns whether the press was valid.
    pub fn cancel(&self, id: &str, chat: i64, user: i64) -> bool {
        let mut map = lock(&self.0);
        match map.get(id) {
            Some(p) if p.chat == chat && p.user == user => {
                map.remove(id);
                true
            }
            _ => false,
        }
    }
}

// --------------------------------------------------------------- execution ---

/// Who pressed the button, for the audit entry.
#[derive(Debug, Clone, Copy)]
struct Who {
    chat: i64,
    user: i64,
}

/// The open position behind a `pos_key`.
async fn find_position(state: &SharedState, key: &str) -> anyhow::Result<Position> {
    let dash = state.trading.dashboard(None).await?;
    dash.positions
        .into_iter()
        .find(|p| position_key(p) == key)
        .ok_or_else(|| anyhow::anyhow!("That position is no longer open"))
}

async fn account_name(state: &SharedState, id: &str) -> String {
    crate::accounts::get(&state.db, id).await.map(|a| a.name).unwrap_or_else(|_| id.to_string())
}

async fn algo_name(state: &SharedState, id: &str) -> anyhow::Result<String> {
    Ok(crate::algo::service::view(&state.db, id).await?.algo.name)
}

/// One line saying what the action will do (confirmation text and audit summary).
async fn describe(state: &SharedState, action: &Action) -> anyhow::Result<String> {
    Ok(match action {
        Action::Arm(id) => format!("Arm algo “{}”", algo_name(state, id).await?),
        Action::Disarm(id) => format!("Disarm algo “{}”", algo_name(state, id).await?),
        Action::Resume(id) => format!("Lift today's loss halt of {}", account_name(state, id).await),
        Action::Kill { flatten: false } => "Kill switch: no new positions anywhere, resting entry orders cancelled".to_string(),
        Action::Kill { flatten: true } => "Kill switch and exit every open position".to_string(),
        Action::KillOff => "Release the kill switch (trading is allowed again)".to_string(),
        Action::ExitOne(key) => {
            let p = find_position(state, key).await?;
            format!("Exit {} ({}), {} lot(s)", contract_text(&p), p.account_name, if p.lot_size > 0 { p.net_qty.abs() / p.lot_size } else { 0 })
        }
        Action::ExitAll => "Exit every open position on every account".to_string(),
    })
}

/// Do the action through the normal services. Returns the card lines.
async fn run(state: &SharedState, action: &Action) -> anyhow::Result<Vec<String>> {
    match action {
        Action::Arm(id) | Action::Disarm(id) => {
            let armed = matches!(action, Action::Arm(_));
            crate::algo::service::set_armed(state, id, armed).await?;
            state.alerts.emit_event("algo-update");
            let name = algo_name(state, id).await?;
            Ok(vec![if armed {
                format!("“{name}” is armed. Signals are acted on from the next bar close.")
            } else {
                format!("“{name}” is disarmed. No new entries; an open position is still managed.")
            }])
        }
        Action::Resume(id) => {
            let name = account_name(state, id).await;
            let overview = state.trading.risk_overview().await?;
            let halted = overview.accounts.iter().any(|a| a.account_id == *id && a.halt.is_some());
            if !halted {
                return Err(anyhow::anyhow!("{name} has no daily-loss halt to lift"));
            }
            state.trading.resume_account(id).await?;
            Ok(vec![format!("{name} may trade again today.")])
        }
        Action::Kill { flatten } => {
            let r = state.trading.set_kill_switch(true, *flatten).await?;
            let mut lines = vec![format!("Kill switch is ON. {} order(s) cancelled.", r.cancelled)];
            if *flatten {
                lines.push(format!("{} exit order(s) placed.", r.placed));
            }
            lines.extend(r.skipped.iter().map(|s| format!("Skipped: {s}")));
            Ok(lines)
        }
        Action::KillOff => {
            state.trading.set_kill_switch(false, false).await?;
            Ok(vec!["Kill switch released. Trading is allowed again.".to_string()])
        }
        Action::ExitOne(key) => {
            let p = find_position(state, key).await?;
            let o = state.trading.exit_position(&p.account_id, &p.instrument, &p.product).await?;
            Ok(vec![format!("{} ({}): {} {} @ {:.2} placed.", contract_text(&p), p.account_name, o.side, o.qty, o.price)])
        }
        Action::ExitAll => {
            let r = state.trading.exit_all(None).await?;
            let mut lines = vec![format!("{} exit order(s) placed, {} open order(s) cancelled.", r.placed, r.cancelled)];
            lines.extend(r.skipped.iter().map(|s| format!("Skipped: {s}")));
            Ok(lines)
        }
    }
}

/// Run an action as `Actor::Telegram`, write the audit entry and build the reply card.
async fn execute(state: &SharedState, action: &Action, who: Who) -> Card {
    let what = describe(state, action).await.unwrap_or_else(|_| action.kind().to_string());
    let result = audit::scope(Actor::Telegram, run(state, action)).await;
    let (ok, error) = match &result {
        Ok(_) => (true, String::new()),
        Err(e) => (false, e.to_string()),
    };
    audit::record(
        Entry::new(
            Category::System,
            if ok { "telegram_control" } else { "telegram_control_failed" },
            if ok { format!("Telegram: {what}") } else { format!("Telegram: {what} failed: {error}") },
        )
        .actor(Actor::Telegram)
        .detail(json!({ "via": "telegram", "action": action.kind(), "chatId": who.chat, "userId": who.user, "error": error })),
    );
    match result {
        Ok(lines) => {
            let mut card = Card::new("✅", GREEN, "Control", format!("Done: {what}"));
            for l in lines {
                card.add_text(l);
            }
            card
        }
        Err(e) => Card::new("⚠️", RED, "Control", format!("Not done: {what}")).text(e.to_string()),
    }
}

// ---------------------------------------------------------------- callbacks ---

fn note(title: &str, text: &str) -> Card {
    Card::new("ℹ️", BLUE, "Control", title).text(text)
}

/// Handle one `callback_query` update. Silent for chats that are not on the
/// allow-list; every other refusal is answered on the button itself.
pub async fn handle_callback(state: &SharedState, http: &reqwest::Client, cq: &Value) {
    let Some(query_id) = cq.get("id").and_then(Value::as_str) else { return };
    let (Some(chat), Some(message_id)) = (
        cq.pointer("/message/chat/id").and_then(Value::as_i64),
        cq.pointer("/message/message_id").and_then(Value::as_i64),
    ) else {
        return;
    };
    let kind = cq.pointer("/message/chat/type").and_then(Value::as_str).unwrap_or("");
    let sender = cq.pointer("/from/id").and_then(Value::as_i64);
    let sent = cq.pointer("/message/date").and_then(Value::as_i64).unwrap_or(0);
    let data = cq.get("data").and_then(Value::as_str).unwrap_or("");

    let cfg = state.alerts.config();
    if !(cfg.telegram.enabled && cfg.telegram.listen) || !is_allowed(&cfg.telegram.chat_ids, chat) {
        return;
    }
    if !controls_allowed(&cfg.telegram, chat, kind, sender) {
        channels::answer_callback(http, query_id, "Control buttons are off, or not allowed in this chat.", true).await;
        return;
    }
    let who = Who { chat, user: sender.unwrap_or(chat) };
    if button_too_old(chrono::Utc::now().timestamp(), sent) {
        channels::answer_callback(http, query_id, "This message is too old. Send /control for fresh buttons.", true).await;
        return;
    }
    let Some(press) = Press::parse(data) else {
        channels::answer_callback(http, query_id, "Unknown button.", true).await;
        return;
    };
    let prompts = &state.alerts.bot.prompts;
    match press {
        Press::Act(action) if !action.needs_confirm() => {
            channels::answer_callback(http, query_id, "Working…", false).await;
            let card = execute(state, &action, who).await;
            send_card(http, chat, &card, None).await;
        }
        Press::Act(action) => ask(state, http, query_id, who, vec![action]).await,
        Press::Kill => ask(state, http, query_id, who, vec![Action::Kill { flatten: false }, Action::Kill { flatten: true }]).await,
        Press::Confirm(id, idx) => match prompts.take(&id, who.chat, who.user, idx, Instant::now()) {
            Ok(action) => {
                channels::answer_callback(http, query_id, "Working…", false).await;
                let card = execute(state, &action, who).await;
                edit_card(http, chat, message_id, &card).await;
            }
            Err(TakeError::Expired) => {
                channels::answer_callback(http, query_id, "Expired. Nothing was done.", true).await;
                edit_card(http, chat, message_id, &note("Expired", "Nothing was done. Press the button again if you still want it.")).await;
            }
            Err(TakeError::Gone) => {
                channels::answer_callback(http, query_id, "This confirmation was already used or has expired.", true).await;
                edit_card(http, chat, message_id, &note("No longer valid", "Nothing was done.")).await;
            }
            Err(TakeError::Mismatch) => {
                channels::answer_callback(http, query_id, "This confirmation belongs to someone else.", true).await;
            }
        },
        Press::Cancel(id) => {
            if prompts.cancel(&id, who.chat, who.user) {
                channels::answer_callback(http, query_id, "Cancelled.", false).await;
            } else {
                channels::answer_callback(http, query_id, "Nothing to cancel.", false).await;
            }
            edit_card(http, chat, message_id, &note("Cancelled", "Nothing was done.")).await;
        }
    }
}

/// Send the confirmation message for one or more options.
async fn ask(state: &SharedState, http: &reqwest::Client, query_id: &str, who: Who, options: Vec<Action>) {
    let mut lines = Vec::new();
    for option in &options {
        match describe(state, option).await {
            Ok(line) => lines.push(line),
            Err(e) => {
                channels::answer_callback(http, query_id, &cut(&e.to_string(), 180), true).await;
                return;
            }
        }
    }
    channels::answer_callback(http, query_id, &format!("Confirm below within {CONFIRM_SECS} seconds."), false).await;
    let id = state.alerts.bot.prompts.add(who.chat, who.user, options.clone(), Instant::now());
    let single = options.len() == 1;
    let title = if single { format!("Confirm: {}", lines[0]) } else { "Confirm the kill switch".to_string() };
    let mut card = Card::new("❓", AMBER, "Confirm", title);
    if !single {
        for l in &lines {
            card.add_text(l.clone());
        }
    }
    card.add_text(format!("Press a button within {CONFIRM_SECS} seconds. Otherwise nothing happens."));
    let mut kb: Keyboard = Vec::new();
    for (idx, option) in options.iter().enumerate() {
        let text = match option {
            Action::Kill { flatten: false } => "🛑 Kill switch only".to_string(),
            Action::Kill { flatten: true } => "🛑 Kill switch + exit all".to_string(),
            _ => "✅ Confirm".to_string(),
        };
        kb.push(vec![Button::new(text, format!("ok:{id}:{idx}"))]);
    }
    kb.push(vec![Button::new("✖ Cancel", format!("no:{id}"))]);
    send_card(http, who.chat, &card, Some(&kb)).await;
}

pub async fn send_card(http: &reqwest::Client, chat: i64, card: &Card, kb: Option<&Keyboard>) {
    let markup = kb.and_then(markup);
    let chat = chat.to_string();
    if let Err(e) = channels::telegram_with(http, &chat, &card.telegram_html(&ist_stamp()), &card.plain(), markup.as_ref()).await {
        debug!("Telegram: could not send a control reply to chat {chat}: {e}");
    }
}

async fn edit_card(http: &reqwest::Client, chat: i64, message_id: i64, card: &Card) {
    if let Err(e) = channels::edit_message(http, &chat.to_string(), message_id, &card.telegram_html(&ist_stamp()), &card.plain()).await {
        debug!("Telegram: could not edit message {message_id}: {e}");
    }
}

// ----------------------------------------------- /positions, /algos, /control ---

/// Buttons for /positions: one exit per position and Exit all.
pub fn position_buttons(positions: &[Position]) -> Keyboard {
    if positions.is_empty() {
        return Vec::new();
    }
    let mut kb: Keyboard = positions.iter().take(MAX_ROWS).map(|p| vec![btn_exit(p)]).collect();
    kb.push(vec![btn_exit_all()]);
    kb
}

/// Buttons for /algos: arm or disarm each algo.
pub fn algo_buttons(views: &[crate::algo::AlgoView]) -> Keyboard {
    views
        .iter()
        .take(MAX_ROWS)
        .map(|v| vec![if v.armed { btn_disarm(&v.algo.id, &v.algo.name) } else { btn_arm(&v.algo.id, &v.algo.name) }])
        .collect()
}

/// The /control panel: kill switch state, halts, and the matching buttons.
pub async fn panel(state: &SharedState, controls: bool) -> anyhow::Result<(Card, Keyboard)> {
    if !controls {
        let card = Card::new("🎛", BLUE, "Control", "Control buttons are off").text(
            "Turn on “Allow control buttons” on the Alerts page. They work only in a private chat that is on the allow-list.",
        );
        return Ok((card, Vec::new()));
    }
    let risk = state.trading.risk_overview().await?;
    let views = crate::algo::service::list_views(&state.db).await?;
    let dash = state.trading.dashboard(None).await?;
    let mut card = Card::new("🎛", BLUE, "Control", "Controls");
    let unrealised: f64 = dash.positions.iter().filter_map(|p| p.unrealised).sum();
    card.add_field(
        "Kill switch",
        match &risk.kill_switch {
            Some(h) => format!("ON since {}", h.since),
            None => "off".into(),
        },
    );
    card.add_field("Open positions", format!("{} · {}", dash.positions.len(), rupees(unrealised)));
    card.add_field("Algos armed", format!("{} of {}", views.iter().filter(|v| v.armed).count(), views.len()));
    let mut kb: Keyboard = Vec::new();
    for a in &risk.accounts {
        if let Some(h) = &a.halt {
            card.add_field(format!("Halted: {}", a.name), h.reason.clone());
            kb.push(vec![btn_resume(&a.account_id, &a.name)]);
        }
    }
    kb.push(vec![if risk.kill_switch.is_some() { btn_kill_off() } else { btn_kill() }]);
    if !dash.positions.is_empty() {
        kb.push(vec![btn_exit_all()]);
    }
    for v in views.iter().filter(|v| v.armed).take(MAX_ROWS) {
        kb.push(vec![btn_disarm(&v.algo.id, &v.algo.name)]);
    }
    card.add_text("Everything except disarming asks for a second press.");
    Ok((card, kb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(controls: bool) -> TelegramConfig {
        TelegramConfig { enabled: true, chat_ids: vec!["123".into(), "-100".into()], listen: true, controls }
    }

    #[test]
    fn presses_round_trip() {
        for a in [
            Action::Arm("a-1".into()),
            Action::Disarm("a-1".into()),
            Action::Resume("acct".into()),
            Action::KillOff,
            Action::ExitOne("0123456789ab".into()),
            Action::ExitAll,
        ] {
            assert_eq!(Press::parse(&a.data()), Some(Press::Act(a.clone())), "{a:?}");
        }
        assert_eq!(Press::parse("kill"), Some(Press::Kill));
        assert_eq!(Press::parse("ok:abcd1234:1"), Some(Press::Confirm("abcd1234".into(), 1)));
        assert_eq!(Press::parse("no:abcd1234"), Some(Press::Cancel("abcd1234".into())));
    }

    #[test]
    fn bad_data_is_not_a_press() {
        for data in ["", "arm:", "arm", "ok:abc", "ok:abc:x", "no:", "exit: ", "nuke", "kill:1"] {
            assert_eq!(Press::parse(data), None, "{data:?}");
        }
    }

    #[test]
    fn only_disarm_skips_confirmation() {
        assert!(!Action::Disarm("a".into()).needs_confirm());
        for a in [
            Action::Arm("a".into()),
            Action::Resume("a".into()),
            Action::Kill { flatten: false },
            Action::Kill { flatten: true },
            Action::KillOff,
            Action::ExitOne("k".into()),
            Action::ExitAll,
        ] {
            assert!(a.needs_confirm(), "{a:?}");
        }
    }

    #[test]
    fn position_keys_are_stable_short_and_distinct() {
        let a = pos_key("acct-1", "NIFTY24NOV24000CE", "INTRADAY");
        assert_eq!(a, pos_key("acct-1", "NIFTY24NOV24000CE", "INTRADAY"));
        assert_ne!(a, pos_key("acct-2", "NIFTY24NOV24000CE", "INTRADAY"));
        assert_ne!(a, pos_key("acct-1", "NIFTY24NOV24000CE", "MARGIN"));
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // A uuid id still fits Telegram's 64 bytes.
        let long = Action::Disarm(uuid::Uuid::new_v4().to_string()).data();
        assert!(long.len() <= MAX_DATA);
    }

    #[test]
    fn markup_drops_long_data_and_empty_rows() {
        let kb: Keyboard = vec![
            vec![Button::new("ok", "exall")],
            vec![Button::new("too long", "x".repeat(65))],
            vec![Button::new("a", "kill"), Button::new("b", "unkill")],
        ];
        let m = markup(&kb).unwrap();
        let rows = m["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0]["callback_data"], "exall");
        assert_eq!(rows[1].as_array().unwrap().len(), 2);
        assert!(markup(&Vec::new()).is_none());
        assert!(markup(&vec![vec![Button::new("x", "y".repeat(80))]]).is_none());
    }

    #[test]
    fn button_text_is_capped() {
        assert!(Button::new("a".repeat(100), "kill").text.chars().count() <= BUTTON_TEXT_MAX);
    }

    #[test]
    fn controls_need_the_toggle_a_private_listed_chat_and_the_same_sender() {
        assert!(controls_allowed(&cfg(true), 123, "private", Some(123)));
        assert!(!controls_allowed(&cfg(false), 123, "private", Some(123)), "toggle off");
        assert!(!controls_allowed(&cfg(true), 123, "private", Some(999)), "other sender");
        assert!(!controls_allowed(&cfg(true), 123, "private", None), "no sender");
        assert!(!controls_allowed(&cfg(true), 123, "group", Some(123)), "not private");
        assert!(!controls_allowed(&cfg(true), 456, "private", Some(456)), "not on the allow-list");
        assert!(!controls_allowed(&cfg(true), -100, "supergroup", Some(-100)), "group id");
        let mut off = cfg(true);
        off.listen = false;
        assert!(!controls_allowed(&off, 123, "private", Some(123)), "listener off");
        off.listen = true;
        off.enabled = false;
        assert!(!controls_allowed(&off, 123, "private", Some(123)), "telegram off");
    }

    #[test]
    fn alert_buttons_only_go_to_private_chats_with_controls_on() {
        let kb: Keyboard = vec![vec![btn_exit_all()]];
        assert!(alert_markup(&cfg(true), "123", &kb).is_some());
        assert!(alert_markup(&cfg(true), " 123 ", &kb).is_some());
        assert!(alert_markup(&cfg(false), "123", &kb).is_none());
        assert!(alert_markup(&cfg(true), "-100", &kb).is_none());
        assert!(alert_markup(&cfg(true), "@channel", &kb).is_none());
        assert!(alert_markup(&cfg(true), "123", &Vec::new()).is_none());
    }

    #[test]
    fn old_buttons_are_refused() {
        assert!(!button_too_old(10_000, 9_000));
        assert!(!button_too_old(100_000, 100_000 - BUTTON_MAX_AGE_SECS));
        assert!(button_too_old(100_000, 100_000 - BUTTON_MAX_AGE_SECS - 1));
        assert!(button_too_old(100_000, 0), "no date means unknown age");
    }

    #[test]
    fn confirmation_works_once() {
        let prompts = Prompts::default();
        let t0 = Instant::now();
        let id = prompts.add(1, 1, vec![Action::ExitAll], t0);
        assert_eq!(prompts.take(&id, 1, 1, 0, t0 + Duration::from_secs(5)), Ok(Action::ExitAll));
        assert_eq!(prompts.take(&id, 1, 1, 0, t0 + Duration::from_secs(6)), Err(TakeError::Gone));
    }

    #[test]
    fn confirmation_expires() {
        let prompts = Prompts::default();
        let t0 = Instant::now();
        let id = prompts.add(1, 1, vec![Action::ExitAll], t0);
        assert_eq!(prompts.take(&id, 1, 1, 0, t0 + Duration::from_secs(CONFIRM_SECS + 1)), Err(TakeError::Expired));
        assert_eq!(prompts.take(&id, 1, 1, 0, t0 + Duration::from_secs(1)), Err(TakeError::Gone), "an expired press removes it");
        let ok = prompts.add(1, 1, vec![Action::ExitAll], t0);
        assert!(prompts.take(&ok, 1, 1, 0, t0 + Duration::from_secs(CONFIRM_SECS)).is_ok(), "exactly at the limit still works");
    }

    #[test]
    fn confirmation_is_bound_to_chat_and_person() {
        let prompts = Prompts::default();
        let t0 = Instant::now();
        let id = prompts.add(1, 1, vec![Action::KillOff], t0);
        assert_eq!(prompts.take(&id, 2, 1, 0, t0), Err(TakeError::Mismatch));
        assert_eq!(prompts.take(&id, 1, 2, 0, t0), Err(TakeError::Mismatch));
        assert!(!prompts.cancel(&id, 2, 1));
        assert_eq!(prompts.take(&id, 1, 1, 0, t0), Ok(Action::KillOff), "a wrong press does not use it up");
    }

    #[test]
    fn one_press_uses_up_every_option() {
        let prompts = Prompts::default();
        let t0 = Instant::now();
        let id = prompts.add(1, 1, vec![Action::Kill { flatten: false }, Action::Kill { flatten: true }], t0);
        assert_eq!(prompts.take(&id, 1, 1, 1, t0), Ok(Action::Kill { flatten: true }));
        assert_eq!(prompts.take(&id, 1, 1, 0, t0), Err(TakeError::Gone));
    }

    #[test]
    fn unknown_option_and_cancel() {
        let prompts = Prompts::default();
        let t0 = Instant::now();
        let id = prompts.add(1, 1, vec![Action::ExitAll], t0);
        assert_eq!(prompts.take(&id, 1, 1, 5, t0), Err(TakeError::Gone));
        let id = prompts.add(1, 1, vec![Action::ExitAll], t0);
        assert!(prompts.cancel(&id, 1, 1));
        assert_eq!(prompts.take(&id, 1, 1, 0, t0), Err(TakeError::Gone));
        assert!(!prompts.cancel("nope", 1, 1));
    }

    #[test]
    fn prompts_are_pruned_and_capped() {
        let prompts = Prompts::default();
        let t0 = Instant::now();
        let old = prompts.add(1, 1, vec![Action::ExitAll], t0);
        let later = t0 + Duration::from_secs(CONFIRM_SECS + 5);
        for _ in 0..(MAX_PROMPTS + 10) {
            prompts.add(1, 1, vec![Action::ExitAll], later);
        }
        assert_eq!(prompts.take(&old, 1, 1, 0, later), Err(TakeError::Gone));
        assert!(lock(&prompts.0).len() <= MAX_PROMPTS);
    }
}
