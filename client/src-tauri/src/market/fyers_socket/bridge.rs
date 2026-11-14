//! Feeds the header indices (and so the chain's underlying lookups and the
//! index charts) from the Fyers socket while Fyers is the data source. The
//! NSE index stream is idle then (`SessionState::nse_enabled`), so the header
//! shows one source only. While the exchange is closed the socket is down and
//! the header keeps whatever it last showed, so while the socket is down one
//! REST quotes call seeds the header with the last session's figures.
//! When the Fyers token is expired (typically the first launch of a day) and
//! the header is still empty, the NSE index stream seeds it once instead; a
//! new login re-seeds from Fyers (`Hub::token_renewed`).

use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

use super::{fyers_is_source, topics, Tick};
use crate::accounts::{self, FyersAuthState};
use crate::chain::fyers::{fetch_quotes, Quote};
use crate::market::events::{IndexSink, IndexTick};
use crate::market::status::MarketState;
use crate::state::SharedState;
use tracing::{info, warn};

/// Wait before trying the quotes call again after a failure.
const SEED_RETRY: Duration = Duration::from_secs(60);
/// NSE fallback seeds tried per stretch (each one listens for a few seconds).
const NSE_TRIES: u32 = 3;

fn quote_to_index_tick(q: &Quote, open_now: bool) -> Option<IndexTick> {
    let index_name = topics::nse_name_for_symbol(&q.symbol)?;
    let prev = (q.prev_close > 0.0).then_some(q.prev_close);
    let change = prev.map_or(0.0, |p| q.ltp - p);
    Some(IndexTick {
        time: chrono::Utc::now().to_rfc3339(),
        index_name,
        current_price: q.ltp,
        change,
        per_change: prev.map_or(0.0, |p| change / p * 100.0),
        previous_close: prev.unwrap_or(0.0),
        open: q.open,
        high: q.high,
        low: q.low,
        mkt_status: if open_now { "Open" } else { "Closed" }.into(),
    })
}

/// One quotes call; feeds each index to the sink. Silent on failure (the
/// header just stays empty), returns whether any index was sent.
async fn seed_fyers(state: &SharedState, sink: &IndexSink) -> bool {
    let creds = match crate::commands::active_fyers_creds(state).await {
        Ok(c) => c,
        Err(_) => return false,
    };
    let open_now = state.market_status.fyers().state == MarketState::Open;
    match fetch_quotes(&state.config.fyers, &creds, &topics::header_index_symbols()).await {
        // The socket may have connected while the call was running; its prices are fresher.
        Ok(_) if state.fyers_hub.is_connected() => true,
        Ok(quotes) => {
            let mut sent = 0;
            for t in quotes.iter().filter_map(|q| quote_to_index_tick(q, open_now)) {
                sink(t);
                sent += 1;
            }
            if sent > 0 {
                info!("Fyers header indices seeded from quotes ({sent})");
            }
            sent > 0
        }
        Err(e) => {
            warn!("Fyers index quotes: {e}");
            false
        }
    }
}

/// Some header index already has a price.
fn header_has_prices(state: &SharedState) -> bool {
    state.indices.read().map(|m| !m.is_empty()).unwrap_or(false)
}

/// One short listen on the NSE index stream (which is otherwise off while
/// Fyers is the source) to put the last prices in the header.
async fn seed_nse(state: &SharedState, sink: &IndexSink) {
    info!("Seeding header indices from NSE (Fyers login not usable)");
    if let Err(e) = crate::market::streamers::indices::poll_once(&state.config, &state.stats, &state.session, sink).await {
        warn!("NSE index seed: {e}");
    }
}

#[derive(Default)]
struct SeedState {
    /// Done for this stretch of "Fyers selected, socket down".
    done: bool,
    retry_at: Option<tokio::time::Instant>,
    nse_tries: u32,
}

impl SeedState {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Seeds the header when Fyers is selected and the socket is not streaming.
/// A usable token seeds from Fyers; an expired or missing one (or a failed
/// Fyers call) falls back to NSE, only while the header has no prices.
async fn maybe_seed(state: &SharedState, sink: &IndexSink, on: bool, st: &mut SeedState) {
    // Socket streaming: it supplies the prices. A later drop (market close) seeds again.
    if !on || state.fyers_hub.is_connected() {
        st.reset();
        return;
    }
    if st.done || st.retry_at.is_some_and(|t| tokio::time::Instant::now() < t) {
        return;
    }
    let auth = accounts::fyers_auth(&state.db).await.state;
    if auth == FyersAuthState::Ok && seed_fyers(state, sink).await {
        st.done = true;
        return;
    }
    let mut covered = header_has_prices(state);
    if !covered && st.nse_tries < NSE_TRIES {
        st.nse_tries += 1;
        seed_nse(state, sink).await;
        covered = header_has_prices(state);
    }
    // No usable login: the NSE prices stand until the user logs in (that re-seeds).
    // Otherwise (the Fyers call failed) try Fyers again later.
    if auth != FyersAuthState::Ok && covered {
        st.done = true;
    } else {
        st.retry_at = Some(tokio::time::Instant::now() + SEED_RETRY);
    }
}

fn to_index_tick(t: &Tick) -> Option<IndexTick> {
    let index_name = topics::nse_index_name(&t.topic)?;
    let px = t.ltp.filter(|p| *p > 0.0)?;
    let prev = t.prev_close.filter(|p| *p > 0.0);
    let change = prev.map_or(0.0, |p| px - p);
    Some(IndexTick {
        time: chrono::Utc::now().to_rfc3339(),
        index_name,
        current_price: px,
        change,
        per_change: prev.map_or(0.0, |p| change / p * 100.0),
        previous_close: prev.unwrap_or(0.0),
        open: t.open.unwrap_or(0.0),
        high: t.high.unwrap_or(0.0),
        low: t.low.unwrap_or(0.0),
        mkt_status: "Open".into(),
    })
}

pub fn spawn(state: SharedState, sink: IndexSink) {
    tauri::async_runtime::spawn(async move {
        let hub = state.fyers_hub.clone();
        let lease = hub.lease();
        let mut rx = hub.subscribe();
        let wanted = topics::header_index_topics();
        let mut on = false;
        let mut seed = SeedState::default();
        let mut check = tokio::time::interval(Duration::from_secs(5));
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = check.tick() => {
                    let now = fyers_is_source(&state).await;
                    if now != on {
                        on = now;
                        lease.set(if on { wanted.clone() } else { Vec::new() });
                        seed.reset();
                    }
                    maybe_seed(&state, &sink, on, &mut seed).await;
                }
                // A new login: seed from Fyers again, right away.
                _ = hub.seed_wanted.notified() => {
                    seed.reset();
                    maybe_seed(&state, &sink, on, &mut seed).await;
                }
                t = rx.recv() => match t {
                    Ok(t) => {
                        if t.index {
                            if let Some(tick) = to_index_tick(&t) {
                                sink(tick);
                            }
                        }
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_tick_from_quote() {
        let q = Quote { symbol: "NSE:NIFTYBANK-INDEX".into(), ltp: 51_000.0, prev_close: 50_000.0, open: 50_100.0, high: 51_200.0, low: 49_900.0 };
        let i = quote_to_index_tick(&q, false).unwrap();
        assert_eq!(i.index_name, "NIFTY BANK");
        assert!((i.per_change - 2.0).abs() < 1e-9);
        assert_eq!(i.mkt_status, "Closed");
        let no_prev = Quote { prev_close: 0.0, ..q.clone() };
        assert_eq!(quote_to_index_tick(&no_prev, true).unwrap().change, 0.0);
        assert!(quote_to_index_tick(&Quote { symbol: "NSE:X-INDEX".into(), ..q }, true).is_none());
    }

    #[test]
    fn index_tick_from_socket_tick() {
        let t = Tick {
            topic: "if|nse_cm|Nifty Bank".into(),
            index: true,
            ltp: Some(51_000.0),
            prev_close: Some(50_000.0),
            ..Tick::default()
        };
        let i = to_index_tick(&t).unwrap();
        assert_eq!(i.index_name, "NIFTY BANK");
        assert!((i.change - 1000.0).abs() < 1e-9);
        assert!((i.per_change - 2.0).abs() < 1e-9);
        assert!(to_index_tick(&Tick { topic: "if|nse_cm|Unknown".into(), index: true, ltp: Some(1.0), ..Tick::default() }).is_none());
    }
}
