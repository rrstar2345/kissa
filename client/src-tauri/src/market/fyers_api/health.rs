//! Which data APIs are failing. Fed by the Fyers send wrapper and by the NSE
//! status / stream calls; read by the header status (`MarketStatus.apiHealth`).
//!
//! One blip must not flicker the header: an endpoint counts as failing after
//! `FAIL_THRESHOLD` failures in a row, or at once for a rate limit or a
//! rejected token. The next success clears it. A failure nobody has repeated
//! for `STALE_MS` is ignored (the endpoint may simply not be called any more).

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::alerts::{Alert, AlertService, Kind};

const FAIL_THRESHOLD: u32 = 3;
const STALE_MS: i64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Nse,
    Fyers,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Nse => "NSE",
            Source::Fyers => "Fyers",
        }
    }
}

/// Ordered by how much the user needs to know: Auth first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    Auth,
    RateLimited,
    Server,
    Network,
}

impl ErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::Auth => "Login rejected",
            ErrorKind::RateLimited => "Rate limited",
            ErrorKind::Server => "Server error",
            ErrorKind::Network => "Network error",
        }
    }

    /// Reported on the first failure, without waiting for repeats.
    fn immediate(self) -> bool {
        matches!(self, ErrorKind::Auth | ErrorKind::RateLimited)
    }
}

#[derive(Debug, Clone, Default)]
struct Endpoint {
    last_ok_ms: Option<i64>,
    consecutive: u32,
    kind: Option<ErrorKind>,
    message: String,
    /// Start of the current run of failures.
    since_ms: i64,
    last_fail_ms: i64,
    retry_after: Option<u64>,
}

impl Endpoint {
    fn active(&self, now_ms: i64) -> bool {
        let Some(kind) = self.kind else { return false };
        self.consecutive > 0
            && (kind.immediate() || self.consecutive >= FAIL_THRESHOLD)
            && now_ms - self.last_fail_ms <= STALE_MS
    }
}

/// What the header shows.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiIssue {
    pub source: Source,
    pub endpoint: String,
    pub kind: ErrorKind,
    pub message: String,
    /// UTC epoch ms of the first failure of this run.
    pub since_ms: i64,
    pub retry_after_secs: Option<u64>,
}

impl ApiIssue {
    /// Text for the feed indicator tooltip.
    pub fn detail(&self) -> String {
        let mut s = format!("{} {}: {}", self.source.label(), self.endpoint, self.message);
        if let Some(r) = self.retry_after_secs {
            s.push_str(&format!(" (retry in {r} s)"));
        }
        s
    }
}

pub struct ApiHealth {
    endpoints: Mutex<HashMap<(Source, String), Endpoint>>,
    alerts: Option<Arc<AlertService>>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl ApiHealth {
    pub fn new(alerts: Option<Arc<AlertService>>) -> Self {
        Self { endpoints: Mutex::new(HashMap::new()), alerts }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(Source, String), Endpoint>> {
        self.endpoints.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ok(&self, source: Source, endpoint: &str) {
        self.ok_at(source, endpoint, now_ms());
    }

    pub fn fail(&self, source: Source, endpoint: &str, kind: ErrorKind, message: &str, retry_after: Option<u64>) {
        self.fail_at(source, endpoint, kind, message, retry_after, now_ms());
    }

    fn ok_at(&self, source: Source, endpoint: &str, at: i64) {
        let mut g = self.lock();
        let e = g.entry((source, endpoint.to_string())).or_default();
        e.last_ok_ms = Some(at);
        e.consecutive = 0;
        e.kind = None;
        e.retry_after = None;
    }

    fn fail_at(&self, source: Source, endpoint: &str, kind: ErrorKind, message: &str, retry_after: Option<u64>, at: i64) {
        let issue = {
            let mut g = self.lock();
            let e = g.entry((source, endpoint.to_string())).or_default();
            let was_active = e.active(at);
            if e.consecutive == 0 {
                e.since_ms = at;
            }
            e.consecutive = e.consecutive.saturating_add(1);
            e.kind = Some(kind);
            e.message = message.chars().take(240).collect();
            e.last_fail_ms = at;
            e.retry_after = retry_after;
            (!was_active && e.active(at)).then(|| (e.message.clone(), e.since_ms))
        };
        // Only the moment an endpoint turns bad raises an alert (the alert
        // service also de-duplicates per endpoint).
        if let Some((msg, _)) = &issue {
            crate::audit::record(
                crate::audit::Entry::new(
                    crate::audit::Category::System,
                    "api_failing",
                    format!("{} API {}: {endpoint} · {msg}", source.label(), kind.label()),
                )
                .actor(crate::audit::Actor::System)
                .subject(format!("{}:{endpoint}", source.label())),
            );
        }
        if let (Some((msg, _)), Some(alerts)) = (issue, &self.alerts) {
            let mut body = format!("{} {endpoint}: {msg}", source.label());
            if let Some(r) = retry_after {
                body.push_str(&format!("\nRetry in about {r} s."));
            }
            alerts.notify(Alert::new(
                Kind::Api,
                format!("{}:{endpoint}", source.label()),
                format!("{} API: {}", source.label(), kind.label()),
                body,
            ));
        }
    }

    /// The failure to show for `source`, if any: the most important one, then the latest.
    pub fn current(&self, source: Source) -> Option<ApiIssue> {
        self.current_at(source, now_ms())
    }

    fn current_at(&self, source: Source, at: i64) -> Option<ApiIssue> {
        let g = self.lock();
        g.iter()
            .filter(|((s, _), e)| *s == source && e.active(at))
            .min_by_key(|((_, name), e)| (e.kind, -e.last_fail_ms, name.clone()))
            .map(|((s, name), e)| ApiIssue {
                source: *s,
                endpoint: name.clone(),
                kind: e.kind.unwrap_or(ErrorKind::Network),
                message: e.message.clone(),
                since_ms: e.since_ms,
                retry_after_secs: e.retry_after,
            })
    }

    /// Forget a source's failures (a new Fyers login, say).
    pub fn clear_source(&self, source: Source) {
        self.lock().retain(|(s, _), _| *s != source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> ApiHealth {
        ApiHealth::new(None)
    }

    #[test]
    fn network_blips_need_three_in_a_row() {
        let h = h();
        h.fail_at(Source::Fyers, "chain", ErrorKind::Network, "timeout", None, 1_000);
        h.fail_at(Source::Fyers, "chain", ErrorKind::Network, "timeout", None, 2_000);
        assert_eq!(h.current_at(Source::Fyers, 2_000), None);
        h.fail_at(Source::Fyers, "chain", ErrorKind::Network, "timeout", None, 3_000);
        let i = h.current_at(Source::Fyers, 3_000).unwrap();
        assert_eq!((i.endpoint.as_str(), i.kind, i.since_ms), ("chain", ErrorKind::Network, 1_000));
    }

    #[test]
    fn a_success_clears_and_resets_the_count() {
        let h = h();
        for t in 1..=3 {
            h.fail_at(Source::Fyers, "chain", ErrorKind::Server, "500", None, t * 1_000);
        }
        assert!(h.current_at(Source::Fyers, 3_000).is_some());
        h.ok_at(Source::Fyers, "chain", 4_000);
        assert_eq!(h.current_at(Source::Fyers, 4_000), None);
        h.fail_at(Source::Fyers, "chain", ErrorKind::Server, "500", None, 5_000);
        assert_eq!(h.current_at(Source::Fyers, 5_000), None);
    }

    #[test]
    fn rate_limit_and_auth_show_at_once() {
        let h = h();
        h.fail_at(Source::Fyers, "quotes", ErrorKind::RateLimited, "429", Some(60), 1_000);
        let i = h.current_at(Source::Fyers, 1_000).unwrap();
        assert_eq!((i.kind, i.retry_after_secs), (ErrorKind::RateLimited, Some(60)));
        h.fail_at(Source::Fyers, "history", ErrorKind::Auth, "token", None, 2_000);
        assert_eq!(h.current_at(Source::Fyers, 2_000).unwrap().kind, ErrorKind::Auth);
    }

    #[test]
    fn sources_are_separate_and_old_failures_expire() {
        let h = h();
        h.fail_at(Source::Nse, "marketStatus", ErrorKind::RateLimited, "429", None, 1_000);
        assert_eq!(h.current_at(Source::Fyers, 1_000), None);
        assert!(h.current_at(Source::Nse, 1_000).is_some());
        assert_eq!(h.current_at(Source::Nse, 1_000 + STALE_MS + 1), None);
    }

    #[test]
    fn clearing_a_source_drops_its_failures() {
        let h = h();
        h.fail_at(Source::Fyers, "chain", ErrorKind::Auth, "token", None, 1_000);
        h.clear_source(Source::Fyers);
        assert_eq!(h.current_at(Source::Fyers, 1_000), None);
    }
}
