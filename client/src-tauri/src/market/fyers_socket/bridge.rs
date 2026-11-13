//! Feeds the header indices (and so the chain's underlying lookups and the
//! index charts) from the Fyers socket while Fyers is the data source.
//! The NSE index stream keeps running until roadmap #14 gates it by source.

use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

use super::{fyers_is_source, topics, Tick};
use crate::market::events::{IndexSink, IndexTick};
use crate::state::SharedState;

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
        let mut check = tokio::time::interval(Duration::from_secs(5));
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = check.tick() => {
                    let now = fyers_is_source(&state).await;
                    if now != on {
                        on = now;
                        lease.set(if on { wanted.clone() } else { Vec::new() });
                    }
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
