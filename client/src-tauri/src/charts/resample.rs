//! Interval parsing and bar resampling, aligned to the NSE session open
//! (09:15 IST) so 15m/60m buckets match what brokers show.

use super::Bar;

const IST: i64 = 5 * 3600 + 30 * 60;
const SESSION_OPEN: i64 = 9 * 3600 + 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Interval {
    Min(i64),
    Day,
}

pub fn parse_interval(s: &str) -> Option<Interval> {
    match s {
        "1d" | "1D" | "D" => Some(Interval::Day),
        _ => {
            let n: i64 = s.strip_suffix('m')?.parse().ok()?;
            (n > 0 && n <= 240).then_some(Interval::Min(n))
        }
    }
}

pub fn bucket_start(ts: i64, iv: Interval) -> i64 {
    match iv {
        Interval::Day => (ts + IST).div_euclid(86_400) * 86_400 - IST,
        Interval::Min(n) => {
            let step = n * 60;
            (ts + IST - SESSION_OPEN).div_euclid(step) * step + SESSION_OPEN - IST
        }
    }
}

/// IST calendar day number (days since epoch) of a timestamp.
pub fn ist_day(ts: i64) -> i64 {
    (ts + IST).div_euclid(86_400)
}

/// Resample ascending bars into `iv` buckets.
pub fn resample(bars: &[Bar], iv: Interval) -> Vec<Bar> {
    let mut out: Vec<Bar> = Vec::new();
    for b in bars {
        let bucket = bucket_start(b.ts, iv);
        match out.last_mut() {
            Some(last) if last.ts == bucket => {
                last.high = last.high.max(b.high);
                last.low = last.low.min(b.low);
                last.close = b.close;
                last.volume += b.volume;
                if b.oi.is_some() {
                    last.oi = b.oi;
                }
            }
            _ => out.push(Bar { ts: bucket, ..b.clone() }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-05 09:15 IST = 03:45 UTC.
    const OPEN: i64 = 1_793_850_300;

    fn bar(ts: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> Bar {
        Bar { ts, open: o, high: h, low: l, close: c, volume: v, oi: None }
    }

    #[test]
    fn aligns_to_session_open() {
        assert_eq!(bucket_start(OPEN, Interval::Min(15)), OPEN);
        assert_eq!(bucket_start(OPEN + 14 * 60 + 59, Interval::Min(15)), OPEN);
        assert_eq!(bucket_start(OPEN + 15 * 60, Interval::Min(15)), OPEN + 900);
        assert_eq!(bucket_start(OPEN + 3600, Interval::Min(60)), OPEN + 3600);
        assert_eq!(bucket_start(OPEN, Interval::Day), OPEN - 9 * 3600 - 15 * 60);
    }

    #[test]
    fn folds_ohlcv() {
        let bars = vec![
            bar(OPEN, 10.0, 12.0, 9.0, 11.0, 5.0),
            bar(OPEN + 60, 11.0, 15.0, 10.0, 14.0, 7.0),
            bar(OPEN + 900, 14.0, 14.5, 13.0, 13.5, 1.0),
        ];
        let r = resample(&bars, Interval::Min(15));
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].open, r[0].high, r[0].low, r[0].close, r[0].volume), (10.0, 15.0, 9.0, 14.0, 12.0));
        assert_eq!(r[1].ts, OPEN + 900);
    }

    #[test]
    fn parses_intervals() {
        assert_eq!(parse_interval("5m"), Some(Interval::Min(5)));
        assert_eq!(parse_interval("1d"), Some(Interval::Day));
        assert_eq!(parse_interval("0m"), None);
        assert_eq!(parse_interval("x"), None);
    }
}
