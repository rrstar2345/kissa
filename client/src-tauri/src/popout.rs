//! Extra app windows ("pop-outs"). The main window keeps the label `main`;
//! pop-outs are `pop-1`, `pop-2`, ... and show one page without the sidebar.
//!
//! All windows share this one backend. Events are broadcast to every window,
//! except the option chain, which is per window (see `chain::runner`).
//! Open pop-outs (route, position, size) are saved in the `windows.open`
//! setting and reopened at launch.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, Window, WindowEvent,
};
use tracing::warn;

use crate::chain::runner;
use crate::db;
use crate::state::SharedState;

const KEY: &str = "windows.open";
const PREFIX: &str = "pop-";
const MAX_POPOUTS: usize = 8;

/// Set when the main window starts closing, so the pop-outs closed with it
/// stay in the saved list and reopen next time.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static SAVE_GEN: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedWindow {
    label: String,
    route: String,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default)]
    maximized: bool,
}

/// `/chain` is fine; anything with a query, a fragment or `..` is not.
fn clean_route(route: &str) -> Option<String> {
    let path = route.split(['?', '#']).next()?.trim();
    if !path.starts_with('/') || path.contains("..") || path.contains("//") {
        return None;
    }
    Some(path.to_string())
}

fn is_popout(label: &str) -> bool {
    label.starts_with(PREFIX)
}

fn next_label(app: &AppHandle) -> Option<String> {
    let open = app.webview_windows();
    (1..=MAX_POPOUTS).map(|n| format!("{PREFIX}{n}")).find(|l| !open.contains_key(l))
}

fn build(app: &AppHandle, saved: &SavedWindow, title: &str) -> Result<WebviewWindow, String> {
    let route = clean_route(&saved.route).ok_or("invalid route")?;
    let file = route.trim_start_matches('/');
    let url = if file.is_empty() { "index.html".to_string() } else { file.to_string() };
    let win = WebviewWindowBuilder::new(app, &saved.label, WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(1100.0, 720.0)
        .min_inner_size(640.0, 420.0)
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    place(app, &win, saved);
    let _ = win.show();
    Ok(win)
}

/// Applies the saved size and position. A position that is no longer on any
/// connected monitor (unplugged screen) is ignored and the window is centred.
fn place(app: &AppHandle, win: &WebviewWindow, saved: &SavedWindow) {
    let monitors = app.available_monitors().unwrap_or_default();
    if let (Some(w), Some(h)) = (saved.width, saved.height) {
        let (mut w, mut h) = (w, h);
        if let Some(m) = monitors.first() {
            w = w.min(m.size().width);
            h = h.min(m.size().height);
        }
        let _ = win.set_size(PhysicalSize::new(w.max(320), h.max(240)));
    }
    let on_screen = |x: i32, y: i32| {
        monitors.iter().any(|m| {
            let (p, s) = (m.position(), m.size());
            x >= p.x - 100
                && x < p.x + s.width as i32 - 100
                && y >= p.y
                && y < p.y + s.height as i32 - 60
        })
    };
    match (saved.x, saved.y) {
        (Some(x), Some(y)) if on_screen(x, y) => {
            let _ = win.set_position(PhysicalPosition::new(x, y));
        }
        _ => {
            let _ = win.center();
        }
    }
    if saved.maximized {
        let _ = win.maximize();
    }
}

fn snapshot(win: &WebviewWindow) -> Option<SavedWindow> {
    // A minimised window reports a meaningless position; keep the last save.
    if win.is_minimized().unwrap_or(false) {
        return None;
    }
    let route = win.url().ok().map(|u| u.path().to_string()).and_then(|p| clean_route(&p)).unwrap_or_else(|| "/".into());
    let maximized = win.is_maximized().unwrap_or(false);
    let pos = win.outer_position().ok();
    let size = win.inner_size().ok();
    Some(SavedWindow {
        label: win.label().to_string(),
        route,
        x: pos.map(|p| p.x),
        y: pos.map(|p| p.y),
        width: size.map(|s| s.width),
        height: size.map(|s| s.height),
        maximized,
    })
}

async fn save_now(app: &AppHandle) {
    let Some(state) = app.try_state::<SharedState>() else { return };
    let mut wins: Vec<WebviewWindow> =
        app.webview_windows().into_values().filter(|w| is_popout(w.label())).collect();
    wins.sort_by(|a, b| a.label().cmp(b.label()));
    let mut list = Vec::new();
    for w in &wins {
        match snapshot(w) {
            Some(s) => list.push(s),
            None => return,
        }
    }
    match serde_json::to_string(&list) {
        Ok(json) => {
            if let Err(e) = db::set_setting(&state.db, KEY, &json).await {
                warn!("could not save open windows: {e}");
            }
        }
        Err(e) => warn!("could not save open windows: {e}"),
    }
}

/// Saves the open pop-outs shortly after the last move, resize, open or close.
fn schedule_save(app: &AppHandle) {
    let n = SAVE_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        if SAVE_GEN.load(Ordering::SeqCst) == n && !SHUTTING_DOWN.load(Ordering::SeqCst) {
            save_now(&app).await;
        }
    });
}

/// Window events for every window (registered on the builder in `lib.rs`).
pub fn on_window_event(window: &Window, event: &WindowEvent) {
    let app = window.app_handle();
    let label = window.label();
    if label == "main" {
        if let WindowEvent::CloseRequested { .. } = event {
            // The app ends with the main window: remember the pop-outs, then close them.
            if !SHUTTING_DOWN.swap(true, Ordering::SeqCst) {
                tauri::async_runtime::block_on(save_now(app));
                for (l, w) in app.webview_windows() {
                    if is_popout(&l) {
                        let _ = w.close();
                    }
                }
            }
        }
        return;
    }
    if !is_popout(label) {
        return;
    }
    match event {
        WindowEvent::Moved(_) | WindowEvent::Resized(_) => schedule_save(app),
        WindowEvent::Destroyed => {
            // Stop this window's option chain; the window will not do it itself.
            let (app2, label) = (app.clone(), label.to_string());
            tauri::async_runtime::spawn(async move {
                if let Some(state) = app2.try_state::<SharedState>() {
                    runner::stop_window_chain(&state, &label).await;
                }
            });
            schedule_save(app);
        }
        _ => {}
    }
}

/// Reopens the pop-outs saved by the last session.
pub fn restore(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Some(state) = app.try_state::<SharedState>() else { return };
        let raw = db::get_setting(&state.db, KEY).await.ok().flatten();
        let saved: Vec<SavedWindow> = raw.and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default();
        for s in saved.into_iter().filter(|s| is_popout(&s.label)).take(MAX_POPOUTS) {
            if app.get_webview_window(&s.label).is_some() {
                continue;
            }
            if let Err(e) = build(&app, &s, "KISSA") {
                warn!("could not restore window {}: {e}", s.label);
            }
        }
    });
}

/// Opens `route` (for example `/charts`) in a new window and returns its label.
#[tauri::command]
pub async fn open_window(app: AppHandle, route: String, title: Option<String>) -> Result<String, String> {
    let route = clean_route(&route).ok_or("Cannot open that page in a new window.")?;
    let label = next_label(&app).ok_or_else(|| format!("At most {MAX_POPOUTS} extra windows can be open."))?;
    let saved = SavedWindow { label: label.clone(), route, x: None, y: None, width: None, height: None, maximized: false };
    let title = title.map(|t| format!("KISSA - {t}")).unwrap_or_else(|| "KISSA".into());
    build(&app, &saved, &title)?;
    schedule_save(&app);
    Ok(label)
}

/// Lets a page show its name in its own window's title bar.
#[tauri::command]
pub fn set_window_title(window: WebviewWindow, title: String) {
    let _ = window.set_title(&title);
}
