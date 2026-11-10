pub mod indices;
pub mod options;

use crate::stats::SharedStats;

/// Real (non-heartbeat) ticks received so far on a stream.
pub(crate) async fn ticks_received(stats: &SharedStats, key: &str) -> u64 {
    stats.read().await.streams.get(key).map(|s| s.ticks_received).unwrap_or(0)
}

/// Delay before reconnecting an Active stream. `quiet_runs` counts
/// connections in a row that ended without a single real tick (the server
/// accepted us, sent nothing useful and closed). The first two retry after
/// `base`; then the delay doubles up to 5 minutes, so a stream that cannot
/// get data never hammers the server, whatever the status API says.
pub(crate) fn reconnect_delay_secs(base: u64, quiet_runs: u32) -> u64 {
    if quiet_runs < 3 {
        return base;
    }
    base.saturating_mul(1u64 << (quiet_runs - 2).min(6)).min(300).max(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backs_off_only_after_quiet_connections() {
        let d = |n| reconnect_delay_secs(5, n);
        assert_eq!((d(0), d(1), d(2)), (5, 5, 5));
        assert_eq!((d(3), d(4), d(5), d(6), d(7)), (10, 20, 40, 80, 160));
        assert_eq!((d(8), d(50)), (300, 300));
        // A base above the cap is never reduced.
        assert_eq!(reconnect_delay_secs(400, 9), 400);
    }
}
