//! Price helpers shared by validation, the paper broker and exit orders.

/// NSE option prices move in steps of 0.05.
pub const TICK: f64 = 0.05;

/// Is `p` a multiple of the tick size?
pub fn on_tick(p: f64) -> bool {
    ((p * 100.0).round() as i64) % 5 == 0
}

/// Round to the nearest tick.
pub fn round_to_tick(p: f64) -> f64 {
    (p * 20.0).round() / 20.0
}

/// Round to paise.
pub fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The last price moved against the order's side by `slip` rupees
/// (BUY pays more, SELL receives less, never below one tick).
pub fn slipped(buy: bool, ltp: f64, slip: f64) -> f64 {
    r2(if buy { ltp + slip } else { (ltp - slip).max(TICK) })
}

/// Limit price for closing a position: the last price pushed `buffer_ticks`
/// ticks in the unfavourable direction (SELL lower, BUY higher), so the
/// order is marketable without being a market order. Never below one tick.
pub fn exit_price(side: &str, ltp: f64, buffer_ticks: u32) -> f64 {
    let buffer = buffer_ticks as f64 * TICK;
    let raw = if side.eq_ignore_ascii_case("SELL") { ltp - buffer } else { ltp + buffer };
    round_to_tick(raw).max(TICK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_size_is_five_paise() {
        assert!(on_tick(100.05));
        assert!(on_tick(100.0));
        assert!(on_tick(0.05));
        assert!(!on_tick(100.01));
        assert!(!on_tick(99.97));
    }

    #[test]
    fn rounds_to_the_nearest_tick() {
        assert!((round_to_tick(100.02) - 100.0).abs() < 1e-9);
        assert!((round_to_tick(100.03) - 100.05).abs() < 1e-9);
    }

    #[test]
    fn exit_price_moves_against_the_exiting_side() {
        assert!((exit_price("SELL", 100.0, 2) - 99.90).abs() < 1e-9);
        assert!((exit_price("BUY", 100.0, 2) - 100.10).abs() < 1e-9);
        assert!((exit_price("SELL", 100.02, 0) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn slippage_is_never_below_one_tick() {
        assert_eq!(slipped(false, 0.10, 0.50), TICK);
        assert_eq!(slipped(true, 100.0, 0.10), 100.10);
    }

    #[test]
    fn exit_price_never_drops_below_one_tick() {
        assert!((exit_price("SELL", 0.05, 4) - TICK).abs() < 1e-9);
    }
}
