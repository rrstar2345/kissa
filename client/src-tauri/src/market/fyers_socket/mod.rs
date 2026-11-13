//! Fyers data socket: one shared connection for the whole app.
//!
//! Consumers (the option chain of each window, the background price feed, the
//! header index bridge) take a `Lease` and tell it which topics they want; the
//! hub subscribes the union (capped at `fyers.max_symbols`, indices first) and
//! broadcasts every decoded `Tick`. The supervisor connects only while Fyers
//! is the data source, someone wants topics and the exchange is open, and
//! reconnects with backoff. The wire format is in `proto`, topic names and the
//! access-token claims in `topics`.
//!
//! Debug: `KISSA_FYERS_DEBUG=1` logs the first bytes of every frame.

pub mod bridge;
pub mod proto;
pub mod topics;

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch, Notify};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};

use crate::market::market_clock;
use crate::market::status::{DataSource, SourceStatus};
use crate::settings::FyersConfig;
use crate::state::SharedState;
use proto::{Decoder, Incoming};

pub use proto::Tick;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Topics per subscribe frame.
const CHUNK: usize = 200;
/// No frame at all for this long means the connection is dead.
const STALL: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SocketStatus {
    pub connected: bool,
    pub error: Option<String>,
    /// Topics subscribed on the server / topics wanted by consumers.
    pub subscribed: usize,
    pub wanted: usize,
}

pub struct Hub {
    cfg: FyersConfig,
    tx: broadcast::Sender<Tick>,
    /// Per consumer, topics in priority order.
    wants: Mutex<BTreeMap<u64, Vec<String>>>,
    changed: Notify,
    next_owner: AtomicU64,
    status: watch::Sender<SocketStatus>,
    /// Unix ms of the last decoded tick on the current connection (0 = none yet).
    /// Kept apart from `status`, which would wake its watchers on every tick.
    last_tick_ms: AtomicI64,
}

/// A consumer's claim on topics; released when dropped (also when its task is aborted).
pub struct Lease {
    hub: Arc<Hub>,
    id: u64,
}

impl Lease {
    /// Replace this consumer's topics (priority order).
    pub fn set(&self, topics: Vec<String>) {
        self.hub.set_topics(self.id, topics);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.hub.set_topics(self.id, Vec::new());
    }
}

fn debug_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("KISSA_FYERS_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0"))
}

fn allowed(st: &SourceStatus) -> bool {
    st.allows_polling(market_clock::is_within_trading_window(market_clock::get_ist_now()))
}

/// Fyers is the selected data source (the cached `data.source`, no database read).
async fn fyers_is_source(state: &SharedState) -> bool {
    state.market_status.source() == DataSource::Fyers
}

impl Hub {
    pub fn new(cfg: FyersConfig) -> Arc<Self> {
        let (tx, _) = broadcast::channel(16384);
        let (status, _) = watch::channel(SocketStatus::default());
        Arc::new(Self {
            cfg,
            tx,
            wants: Mutex::new(BTreeMap::new()),
            changed: Notify::new(),
            next_owner: AtomicU64::new(1),
            status,
            last_tick_ms: AtomicI64::new(0),
        })
    }

    /// The socket is connected and subscribed.
    pub fn is_connected(&self) -> bool {
        self.status.borrow().connected
    }

    /// The last connection error, until the next successful connection.
    pub fn last_error(&self) -> Option<String> {
        self.status.borrow().error.clone()
    }

    /// Seconds since the last tick on the current connection (`None`: none yet).
    pub fn last_tick_age_secs(&self) -> Option<i64> {
        let ms = self.last_tick_ms.load(Ordering::Relaxed);
        (ms > 0).then(|| (chrono::Utc::now().timestamp_millis() - ms).max(0) / 1000)
    }

    /// Every decoded tick. A slow receiver misses ticks (`Lagged`) instead of blocking the socket.
    pub fn subscribe(&self) -> broadcast::Receiver<Tick> {
        self.tx.subscribe()
    }

    pub fn status(&self) -> watch::Receiver<SocketStatus> {
        self.status.subscribe()
    }

    pub fn lease(self: &Arc<Self>) -> Lease {
        Lease { hub: self.clone(), id: self.next_owner.fetch_add(1, Ordering::Relaxed) }
    }

    fn set_topics(&self, id: u64, topics: Vec<String>) {
        {
            let mut w = self.wants.lock().unwrap_or_else(|e| e.into_inner());
            if topics.is_empty() {
                w.remove(&id);
            } else {
                w.insert(id, topics);
            }
        }
        self.changed.notify_one();
    }

    fn has_wants(&self) -> bool {
        !self.wants.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Union of all wanted topics, indices first, cut at the symbol cap.
    /// Returns (topics to subscribe, how many were wanted).
    fn desired(&self) -> (Vec<String>, usize) {
        let w = self.wants.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = HashSet::new();
        let (mut idx, mut rest) = (Vec::new(), Vec::new());
        for list in w.values() {
            for t in list {
                if seen.insert(t.to_ascii_lowercase()) {
                    if t.starts_with("if|") { idx.push(t.clone()) } else { rest.push(t.clone()) }
                }
            }
        }
        let total = idx.len() + rest.len();
        idx.extend(rest);
        idx.truncate(self.cfg.max_symbols.max(1));
        (idx, total)
    }

    fn set_status(&self, connected: bool, error: Option<String>, subscribed: usize, wanted: usize) {
        let new = SocketStatus { connected, error, subscribed, wanted };
        self.status.send_if_modified(|s| {
            if *s == new {
                false
            } else {
                *s = new;
                true
            }
        });
    }

    /// Starts the connection supervisor.
    pub fn spawn(self: &Arc<Self>, state: SharedState) {
        let hub = self.clone();
        tauri::async_runtime::spawn(async move { hub.supervise(state).await });
    }

    async fn supervise(self: Arc<Self>, state: SharedState) {
        let mut backoff = 1u64;
        let mut market = state.market_status.subscribe_fyers();
        loop {
            let ready = self.has_wants() && fyers_is_source(&state).await && {
                let ok = allowed(&market.borrow_and_update());
                ok
            };
            if !ready {
                self.set_status(false, None, 0, 0);
                backoff = 1;
                tokio::select! {
                    _ = self.changed.notified() => {}
                    _ = market.changed() => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
                continue;
            }

            let creds = match crate::commands::active_fyers_creds(&state).await {
                Ok(c) => c,
                Err(e) => {
                    self.fail(e.to_string(), 30).await;
                    continue;
                }
            };
            let Some(jwt) = topics::parse_jwt(&creds.access_token) else {
                self.fail("Fyers access token has no socket key. Log in to Fyers again.".into(), 60).await;
                continue;
            };
            if jwt.exp.is_some_and(|e| e <= chrono::Utc::now().timestamp()) {
                self.fail("Fyers access token expired. Log in to Fyers again.".into(), 60).await;
                continue;
            }

            let started = Instant::now();
            match self.session(&state, &jwt, &mut market).await {
                Ok(()) => backoff = 1,
                Err(e) => {
                    warn!("Fyers socket: {e}");
                    if started.elapsed() > Duration::from_secs(30) {
                        backoff = 1;
                    }
                    self.fail(e.to_string(), backoff).await;
                    backoff = (backoff * 2).min(30);
                }
            }
        }
    }

    async fn fail(&self, message: String, wait_secs: u64) {
        let (_, wanted) = self.desired();
        self.set_status(false, Some(message), 0, wanted);
        tokio::time::sleep(Duration::from_secs(wait_secs)).await;
    }

    /// One connection. `Ok` = ended on purpose (not needed any more), `Err` = reconnect.
    async fn session(
        &self,
        state: &SharedState,
        jwt: &topics::Jwt,
        market: &mut watch::Receiver<SourceStatus>,
    ) -> Result<()> {
        let (mut ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(self.cfg.socket_url.as_str()),
        )
        .await
        .map_err(|_| anyhow!("connection timed out"))?
        .map_err(|e| anyhow!("connection failed: {e}"))?;

        let mut dec = Decoder::new();
        self.last_tick_ms.store(0, Ordering::Relaxed);
        send(&mut ws, proto::auth_frame(&jwt.hsm_key, "kissa")).await?;
        let ack_every = self.wait_auth(&mut ws, &mut dec).await?;
        info!("Fyers socket connected");

        send(&mut ws, proto::mode_frame(false)).await?;
        let mut subscribed: HashSet<String> = HashSet::new();
        self.sync(&mut ws, &mut subscribed).await?;

        let mut ping = tokio::time::interval(Duration::from_secs(10));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_rx = Instant::now();
        let mut since_ack = 0u32;
        loop {
            tokio::select! {
                msg = ws.next() => {
                    let msg = match msg {
                        None => return Err(anyhow!("connection closed")),
                        Some(Err(e)) => return Err(anyhow!("connection error: {e}")),
                        Some(Ok(m)) => m,
                    };
                    last_rx = Instant::now();
                    match msg {
                        Message::Binary(b) => {
                            if debug_on() {
                                info!("fyers rx {} bytes: {:02x?}", b.len(), &b[..b.len().min(48)]);
                            }
                            if let Incoming::Data { msg_no, ticks } = dec.decode(&b) {
                                if !ticks.is_empty() {
                                    self.last_tick_ms.store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
                                }
                                for t in ticks {
                                    let _ = self.tx.send(t);
                                }
                                since_ack += 1;
                                if ack_every > 0 && since_ack >= ack_every {
                                    since_ack = 0;
                                    send(&mut ws, proto::ack_frame(msg_no)).await?;
                                }
                            }
                        }
                        Message::Ping(p) => ws.send(Message::Pong(p)).await?,
                        Message::Close(_) => return Err(anyhow!("closed by Fyers")),
                        _ => {}
                    }
                }
                _ = ping.tick() => {
                    if last_rx.elapsed() > STALL {
                        return Err(anyhow!("no data for {} s", STALL.as_secs()));
                    }
                    send(&mut ws, proto::ping_frame()).await?;
                    if !fyers_is_source(state).await {
                        return Ok(());
                    }
                }
                _ = self.changed.notified() => {
                    if !self.has_wants() {
                        return Ok(());
                    }
                    self.sync(&mut ws, &mut subscribed).await?;
                }
                _ = market.changed() => {
                    let ok = allowed(&market.borrow_and_update());
                    if !ok {
                        info!("Fyers socket closing: exchange not open");
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Waits for the auth reply; returns the ack interval.
    async fn wait_auth(&self, ws: &mut Ws, dec: &mut Decoder) -> Result<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = tokio::time::timeout(left, ws.next())
                .await
                .map_err(|_| anyhow!("no reply to authentication"))?
                .ok_or_else(|| anyhow!("closed during authentication"))?
                .map_err(|e| anyhow!("connection error: {e}"))?;
            if let Message::Binary(b) = msg {
                if debug_on() {
                    info!("fyers rx {} bytes: {:02x?}", b.len(), &b[..b.len().min(48)]);
                }
                if let Incoming::Auth { ok, ack_every } = dec.decode(&b) {
                    return if ok {
                        Ok(ack_every)
                    } else {
                        Err(anyhow!("Fyers rejected the socket key. Log in to Fyers again."))
                    };
                }
            }
        }
    }

    /// Brings the server's subscriptions in line with what consumers want.
    async fn sync(&self, ws: &mut Ws, subscribed: &mut HashSet<String>) -> Result<()> {
        let (want, total) = self.desired();
        let want_set: HashSet<String> = want.iter().cloned().collect();
        let remove: Vec<String> = subscribed.difference(&want_set).cloned().collect();
        let add: Vec<String> = want.iter().filter(|t| !subscribed.contains(*t)).cloned().collect();
        for c in remove.chunks(CHUNK) {
            send(ws, proto::unsubscribe_frame(c)).await?;
        }
        for c in add.chunks(CHUNK) {
            send(ws, proto::subscribe_frame(c)).await?;
        }
        if total > want.len() {
            warn!(
                "Fyers socket: {} symbols wanted, limit is {} (fyers.max_symbols); the rest update from chain refreshes",
                total,
                want.len()
            );
        }
        *subscribed = want_set;
        self.set_status(true, None, subscribed.len(), total);
        Ok(())
    }
}

async fn send(ws: &mut Ws, frame: Vec<u8>) -> Result<()> {
    ws.send(Message::Binary(frame.into())).await.map_err(|e| anyhow!("send failed: {e}"))
}
