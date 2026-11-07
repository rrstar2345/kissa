//! Paper matching rules (whole quantity only, no partial fills):
//!
//! * LIMIT BUY fills when LTP <= limit; LIMIT SELL fills when LTP >= limit.
//!   The fill is at the last price moved against you by the slippage, but
//!   never worse than the limit.
//! * STOP_LOSS (stop-limit) is armed when LTP reaches the trigger (BUY: LTP >=
//!   trigger, SELL: LTP <= trigger) and then behaves like a LIMIT order.

use crate::trading::pricing::slipped;
use crate::trading::OrderRow;

#[derive(Debug, PartialEq)]
pub enum Decision {
    Wait,
    /// Stop price reached but the limit is not marketable yet.
    Trigger,
    Fill(f64),
}

pub fn evaluate(o: &OrderRow, triggered: bool, ltp: f64, slip: f64) -> Decision {
    if !ltp.is_finite() || ltp <= 0.0 {
        return Decision::Wait;
    }
    let buy = o.side.eq_ignore_ascii_case("BUY");
    let mut armed = triggered;
    if o.order_type == "STOP_LOSS" && !armed {
        let hit = if buy { ltp >= o.trigger_price } else { ltp <= o.trigger_price };
        if !hit {
            return Decision::Wait;
        }
        armed = true;
    }
    let marketable = if buy { ltp <= o.price } else { ltp >= o.price };
    if marketable {
        let px = slipped(buy, ltp, slip);
        Decision::Fill(if buy { px.min(o.price) } else { px.max(o.price) })
    } else if armed && !triggered {
        Decision::Trigger
    } else {
        Decision::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::super::book::sample_order as order;
    use super::*;

    #[test]
    fn limit_buy_fills_at_the_better_price() {
        let o = order("BUY", "LIMIT", 100.0, 0.0);
        assert_eq!(evaluate(&o, false, 101.0, 0.0), Decision::Wait);
        assert_eq!(evaluate(&o, false, 100.0, 0.0), Decision::Fill(100.0));
        assert_eq!(evaluate(&o, false, 97.5, 0.0), Decision::Fill(97.5));
    }

    #[test]
    fn limit_sell_fills_at_the_better_price() {
        let o = order("SELL", "LIMIT", 100.0, 0.0);
        assert_eq!(evaluate(&o, false, 99.0, 0.0), Decision::Wait);
        assert_eq!(evaluate(&o, false, 103.0, 0.0), Decision::Fill(103.0));
    }

    #[test]
    fn stop_loss_sell_arms_then_fills() {
        // Protect a long: trigger 90, limit 88.
        let o = order("SELL", "STOP_LOSS", 88.0, 90.0);
        assert_eq!(evaluate(&o, false, 95.0, 0.0), Decision::Wait);
        assert_eq!(evaluate(&o, false, 90.0, 0.0), Decision::Fill(90.0)); // limit 88 is marketable
        // Price gaps through the limit: armed but not fillable.
        assert_eq!(evaluate(&o, false, 85.0, 0.0), Decision::Trigger);
        assert_eq!(evaluate(&o, true, 85.0, 0.0), Decision::Wait);
        assert_eq!(evaluate(&o, true, 88.5, 0.0), Decision::Fill(88.5));
    }

    #[test]
    fn stop_loss_buy_arms_on_rise() {
        // Protect a short: trigger 110, limit 112.
        let o = order("BUY", "STOP_LOSS", 112.0, 110.0);
        assert_eq!(evaluate(&o, false, 105.0, 0.0), Decision::Wait);
        assert_eq!(evaluate(&o, false, 111.0, 0.0), Decision::Fill(111.0));
        assert_eq!(evaluate(&o, false, 115.0, 0.0), Decision::Trigger);
    }

    #[test]
    fn slippage_moves_the_fill_against_you_but_not_past_the_limit() {
        let buy = order("BUY", "LIMIT", 100.0, 0.0);
        assert_eq!(evaluate(&buy, false, 99.0, 0.10), Decision::Fill(99.10));
        assert_eq!(evaluate(&buy, false, 99.95, 0.10), Decision::Fill(100.0)); // capped at the limit
        let sell = order("SELL", "LIMIT", 100.0, 0.0);
        assert_eq!(evaluate(&sell, false, 101.0, 0.10), Decision::Fill(100.90));
        assert_eq!(evaluate(&sell, false, 100.05, 0.10), Decision::Fill(100.0)); // floored at the limit
    }

}
