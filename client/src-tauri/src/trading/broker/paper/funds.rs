//! Paper funds model (a rough stand-in for a broker's margin system).
//!
//! * cash      = starting capital + realised P&L - charges (all time)
//! * a long position blocks its premium (average price x quantity)
//! * a short position blocks `short_margin_pct` % of strike x quantity
//! * a pending order reserves what it would block if it opened exposure
//!   (the part that closes an existing position needs nothing)
//!
//! available = cash - blocked in positions - reserved by pending orders.

/// Quantity of an order that adds to exposure, given the current signed
/// position (`dir` is +1 for BUY, -1 for SELL).
pub fn opening_qty(position_qty: i64, dir: i64, qty: i64) -> i64 {
    if position_qty == 0 || position_qty.signum() == dir {
        qty
    } else {
        (qty - position_qty.abs()).max(0)
    }
}

/// Margin blocked by a short option.
pub fn short_margin(strike: f64, qty: i64, pct: f64) -> f64 {
    strike * qty as f64 * pct / 100.0
}

/// Funds an order needs: BUY blocks the premium at its price, SELL blocks
/// short margin. Only the exposure-increasing part counts.
pub fn requirement(is_buy: bool, qty: i64, price: f64, strike: f64, position_qty: i64, short_pct: f64) -> f64 {
    let dir = if is_buy { 1 } else { -1 };
    let opening = opening_qty(position_qty, dir, qty);
    if opening == 0 {
        0.0
    } else if is_buy {
        opening as f64 * price
    } else {
        short_margin(strike, opening, short_pct)
    }
}

/// What an open position blocks.
pub fn position_block(net_qty: i64, avg_price: f64, strike: f64, short_pct: f64) -> f64 {
    if net_qty > 0 {
        net_qty as f64 * avg_price
    } else {
        short_margin(strike, -net_qty, short_pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exposure_increasing_part_counts() {
        assert_eq!(opening_qty(0, 1, 65), 65);
        assert_eq!(opening_qty(65, 1, 65), 65); // adding to a long
        assert_eq!(opening_qty(65, -1, 65), 0); // closing a long
        assert_eq!(opening_qty(65, -1, 130), 65); // flipping to short
        assert_eq!(opening_qty(-65, 1, 30), 0); // partly covering a short
    }

    #[test]
    fn buy_needs_the_premium() {
        assert!((requirement(true, 65, 100.0, 24000.0, 0, 10.0) - 6500.0).abs() < 1e-9);
    }

    #[test]
    fn sell_needs_short_margin() {
        // 24000 x 65 x 10 % = 156,000
        assert!((requirement(false, 65, 100.0, 24000.0, 0, 10.0) - 156_000.0).abs() < 1e-6);
    }

    #[test]
    fn closing_orders_need_nothing() {
        assert_eq!(requirement(false, 65, 100.0, 24000.0, 65, 10.0), 0.0);
        assert_eq!(requirement(true, 65, 100.0, 24000.0, -65, 10.0), 0.0);
    }

    #[test]
    fn positions_block_premium_or_margin() {
        assert!((position_block(65, 100.0, 24000.0, 10.0) - 6500.0).abs() < 1e-9);
        assert!((position_block(-65, 100.0, 24000.0, 10.0) - 156_000.0).abs() < 1e-6);
    }
}
