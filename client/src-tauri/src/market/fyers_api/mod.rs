//! The one way to call the Fyers REST APIs.
//!
//! Every call site builds its request with [`get`] / [`post`] and sends it with
//! [`send`]; nothing else talks to Fyers over HTTP. The wrapper
//!   1. takes a slot from the [`FyersLimiter`] (waits, or refuses, see `limiter`),
//!   2. sends the request,
//!   3. reads the answer, spots a rate limit (HTTP 429 / code -429), a rejected
//!      token and server errors, and tells [`ApiHealth`] and the limiter.
//! Normal answers, including Fyers' own `"s":"error"` replies, are returned as
//! they are for the caller to parse.
//!
//! The service lives in `AppState.fyers_api`; the same `Arc` is installed as a
//! process global so that deep call sites (`chain::fyers::fetch`, ...) need no
//! extra argument. Without it (unit tests) calls go out unthrottled.

pub mod health;
pub mod limiter;

use reqwest::header::RETRY_AFTER;
use reqwest::RequestBuilder;
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub use health::{ApiHealth, ApiIssue, ErrorKind, Source};
pub use limiter::{Category, CategoryUsage, FyersLimiter, PollerGuard, Priority, Throttled};

use crate::alerts::AlertService;
use crate::chain::fyers::FyersCreds;
use crate::settings::FyersConfig;

pub struct FyersApi {
    pub limiter: Arc<FyersLimiter>,
    pub health: ApiHealth,
}

impl FyersApi {
    /// Loads today's counters and starts the saver. Call once from `setup`.
    pub async fn new(cfg: &FyersConfig, pool: SqlitePool, alerts: Arc<AlertService>) -> Arc<Self> {
        let limiter = Arc::new(FyersLimiter::new(cfg.limits()));
        limiter.load(&pool).await;
        limiter.spawn_flush(pool);
        Arc::new(Self { limiter, health: ApiHealth::new(Some(alerts)) })
    }
}

static GLOBAL: OnceLock<Arc<FyersApi>> = OnceLock::new();

pub fn install(api: Arc<FyersApi>) {
    let _ = GLOBAL.set(api);
}

pub fn global() -> Option<&'static Arc<FyersApi>> {
    GLOBAL.get()
}

/// Health of the NSE calls (status poll, stream connects), same service as Fyers.
pub fn nse_ok(endpoint: &str) {
    if let Some(a) = global() {
        a.health.ok(Source::Nse, endpoint);
    }
}

pub fn nse_fail(endpoint: &str, message: &str) {
    if let Some(a) = global() {
        let kind = classify_text(message);
        a.health.fail(Source::Nse, endpoint, kind, message, None);
    }
}

/// NSE failures arrive as text; this picks the kind from it.
fn classify_text(m: &str) -> ErrorKind {
    let l = m.to_ascii_lowercase();
    if l.contains("429") || l.contains("too many") {
        ErrorKind::RateLimited
    } else if l.contains("http 5") || l.contains("status 5") || l.contains("502") || l.contains("503") {
        ErrorKind::Server
    } else {
        ErrorKind::Network
    }
}

/// How a polling loop should wait: `base` stretched to fit the rest of today's
/// call budget (see `limiter::pace`).
pub fn paced(cat: Category, base: Duration) -> Duration {
    global().map_or(base, |a| a.limiter.paced_interval(cat, base))
}

/// Keeps a polling loop counted for pacing while the guard lives.
pub fn poller() -> Option<PollerGuard> {
    global().map(|a| a.limiter.poller())
}

// --------------------------------------------------------------- requests ---

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default())
}

fn auth_value(creds: &FyersCreds) -> String {
    format!("{}:{}", creds.app_id, creds.access_token)
}

/// GET with the Fyers `Authorization` header.
pub fn get(url: &str, creds: &FyersCreds) -> RequestBuilder {
    client().get(url).header("Authorization", auth_value(creds))
}

/// POST with the Fyers `Authorization` header.
pub fn post(url: &str, creds: &FyersCreds) -> RequestBuilder {
    client().post(url).header("Authorization", auth_value(creds))
}

/// POST without credentials (the login token exchange).
pub fn post_anonymous(url: &str) -> RequestBuilder {
    client().post(url)
}

/// What came back.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

#[derive(Debug)]
pub enum ApiError {
    /// Not sent: the rate budget refused it.
    Throttled(Throttled),
    /// Fyers answered 429 / code -429.
    RateLimited { retry_after: Option<u64> },
    /// The request failed before an answer (connection, timeout).
    Network(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Throttled(t) => write!(f, "{t}"),
            ApiError::RateLimited { retry_after: Some(s) } => {
                write!(f, "Fyers rate limit reached; calls pause for a while (retry in about {s} s)")
            }
            ApiError::RateLimited { retry_after: None } => write!(f, "Fyers rate limit reached; calls pause for a while"),
            ApiError::Network(m) => write!(f, "Fyers request failed: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Fyers error codes that mean the token or app id is not accepted
/// (`-8` expired, `-15` bad app id, `-16` / `-17` cannot authenticate).
/// Unverified against live Fyers; HTTP 401 / 403 count too.
const AUTH_CODES: [i64; 4] = [-8, -15, -16, -17];
/// Error bodies are tiny; a big body is data and is not parsed here.
const MAX_ERROR_BODY: usize = 4096;

#[derive(Debug, PartialEq)]
enum Outcome {
    Ok,
    RateLimited,
    Auth(String),
    Server(String),
}

fn classify(status: u16, body: &str) -> Outcome {
    if status == 429 {
        return Outcome::RateLimited;
    }
    let json: Option<Value> = (body.len() < MAX_ERROR_BODY).then(|| serde_json::from_str(body).ok()).flatten();
    let code = json.as_ref().and_then(|v| v.get("code")).and_then(Value::as_i64);
    let message = json
        .as_ref()
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map_or_else(|| format!("HTTP {status}"), str::to_string);
    if code == Some(-429) {
        return Outcome::RateLimited;
    }
    if status == 401 || status == 403 || code.is_some_and(|c| AUTH_CODES.contains(&c)) {
        return Outcome::Auth(message);
    }
    if status >= 500 {
        return Outcome::Server(message);
    }
    Outcome::Ok
}

/// Sends one Fyers request through the limiter. `endpoint` names the call in the
/// health report ("chain", "quotes", ...).
pub async fn send(endpoint: &'static str, cat: Category, prio: Priority, req: RequestBuilder) -> Result<Reply, ApiError> {
    send_with(endpoint, cat, prio, req, true).await
}

/// `judge_auth = false` for the login exchange: a wrong auth code is not a
/// rejected token.
pub async fn send_with(
    endpoint: &'static str,
    cat: Category,
    prio: Priority,
    req: RequestBuilder,
    judge_auth: bool,
) -> Result<Reply, ApiError> {
    let api = global();
    // A token Fyers already refused is not tried again by background calls; a
    // user action (or a new login) that succeeds lifts this.
    if judge_auth && prio == Priority::Background && crate::accounts::fyers_rejected() {
        return Err(ApiError::Throttled(Throttled { message: "Fyers login was rejected; log in again".into() }));
    }
    if let Some(a) = api {
        a.limiter.acquire(cat, prio).await.map_err(ApiError::Throttled)?;
    }
    let fail = |kind: ErrorKind, msg: &str, retry: Option<u64>| {
        if let Some(a) = api {
            a.health.fail(Source::Fyers, endpoint, kind, msg, retry);
        }
    };

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let msg = if e.is_timeout() { "request timed out".to_string() } else { e.to_string() };
            fail(ErrorKind::Network, &msg, None);
            return Err(ApiError::Network(msg));
        }
    };
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("could not read the response: {e}");
            fail(ErrorKind::Network, &msg, None);
            return Err(ApiError::Network(msg));
        }
    };

    match classify(status, &body) {
        Outcome::Ok => {
            if let Some(a) = api {
                a.health.ok(Source::Fyers, endpoint);
            }
            if judge_auth {
                crate::accounts::clear_fyers_rejected();
            }
        }
        Outcome::RateLimited => {
            let pause = api.map(|a| a.limiter.rate_limited(cat, retry_after));
            tracing::warn!("Fyers {endpoint}: rate limited (HTTP {status}); calls paused");
            let shown = retry_after.or(pause.filter(|p| *p != u64::MAX));
            fail(ErrorKind::RateLimited, "Fyers rate limit reached", shown);
            return Err(ApiError::RateLimited { retry_after: shown });
        }
        Outcome::Auth(msg) => {
            if judge_auth {
                fail(ErrorKind::Auth, &msg, None);
                crate::accounts::mark_fyers_rejected();
            } else if let Some(a) = api {
                a.health.ok(Source::Fyers, endpoint);
            }
        }
        Outcome::Server(msg) => fail(ErrorKind::Server, &msg, None),
    }
    Ok(Reply { status, body })
}

// ------------------------------------------------------- shared requests ---

type Slot = Arc<tokio::sync::Mutex<Option<(Instant, Arc<String>)>>>;

fn shared_slots() -> &'static Mutex<HashMap<String, Slot>> {
    static S: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Runs `f` unless the same `key` was answered less than `ttl` ago, in which
/// case that answer is reused; callers asking while one is in flight wait for
/// it. Chains of several windows on one symbol share one request this way.
/// Errors are not kept.
pub async fn shared<F, Fut>(key: String, ttl: Duration, f: F) -> anyhow::Result<Arc<String>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let slot = {
        let mut m = shared_slots().lock().unwrap_or_else(|e| e.into_inner());
        if m.len() > 16 {
            m.retain(|_, s| {
                Arc::strong_count(s) > 1
                    || s.try_lock()
                        .map(|g| g.as_ref().is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(30)))
                        .unwrap_or(true)
            });
        }
        m.entry(key).or_default().clone()
    };
    let mut guard = slot.lock().await;
    if let Some((at, v)) = guard.as_ref() {
        if at.elapsed() < ttl {
            return Ok(v.clone());
        }
    }
    let v = Arc::new(f().await?);
    *guard = Some((Instant::now(), v.clone()));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_is_spotted_in_status_and_body() {
        assert_eq!(classify(429, ""), Outcome::RateLimited);
        assert_eq!(classify(200, r#"{"s":"error","code":-429,"message":"limit"}"#), Outcome::RateLimited);
        assert_eq!(classify(400, r#"{"s":"error","code":-429}"#), Outcome::RateLimited);
    }

    #[test]
    fn rejected_tokens_are_auth_failures() {
        assert_eq!(classify(401, "nope"), Outcome::Auth("HTTP 401".into()));
        assert_eq!(
            classify(200, r#"{"s":"error","code":-8,"message":"token expired"}"#),
            Outcome::Auth("token expired".into())
        );
    }

    #[test]
    fn ordinary_errors_are_not_api_failures() {
        // A bad symbol is the caller's problem; the API itself is fine.
        assert_eq!(classify(200, r#"{"s":"error","code":-300,"message":"Invalid symbol"}"#), Outcome::Ok);
        assert_eq!(classify(200, r#"{"s":"ok","d":[]}"#), Outcome::Ok);
        assert_eq!(classify(502, "<html>bad gateway</html>"), Outcome::Server("HTTP 502".into()));
    }

    #[test]
    fn big_bodies_are_not_parsed() {
        let body = format!(r#"{{"s":"ok","code":-429,"pad":"{}"}}"#, "x".repeat(MAX_ERROR_BODY));
        assert_eq!(classify(200, &body), Outcome::Ok);
    }

    #[test]
    fn nse_text_is_classified() {
        assert_eq!(classify_text("HTTP 429 Too Many Requests"), ErrorKind::RateLimited);
        assert_eq!(classify_text("HTTP 503: unavailable"), ErrorKind::Server);
        assert_eq!(classify_text("request failed: dns"), ErrorKind::Network);
    }

    #[tokio::test]
    async fn shared_answers_are_reused_within_ttl() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let run = |calls: Arc<AtomicU32>| async move {
            shared("k-test".into(), Duration::from_secs(5), || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok("body".to_string())
            })
            .await
        };
        let (a, b) = tokio::join!(run(calls.clone()), run(calls.clone()));
        assert_eq!((a.unwrap().as_str(), b.unwrap().as_str()), ("body", "body"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
