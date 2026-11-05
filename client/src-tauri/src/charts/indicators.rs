//! Technical indicators and buy/sell signals, computed in Rust.
//!
//! Add an indicator by writing a function over `&[Bar]`, pushing its lines
//! (and optional signals) from `compute`'s `match`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::resample::ist_day;
use super::Bar;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorSpec {
    /// sma | ema | rsi | macd | bb | vwap | supertrend
    pub kind: String,
    #[serde(default)]
    pub period: Option<usize>,
    #[serde(default)]
    pub fast: Option<usize>,
    #[serde(default)]
    pub slow: Option<usize>,
    #[serde(default)]
    pub signal: Option<usize>,
    #[serde(default)]
    pub mult: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Point {
    pub time: i64,
    pub value: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Line {
    pub id: String,
    pub label: String,
    /// `price` (drawn over candles) or a sub-pane id such as `rsi`, `macd`.
    pub pane: String,
    /// `line` | `histogram`
    pub style: String,
    /// Optional colour hint: `up` | `down`; the UI picks a palette otherwise.
    pub tone: Option<String>,
    pub points: Vec<Point>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Signal {
    pub time: i64,
    /// `buy` | `sell`
    pub side: String,
    pub price: f64,
    pub source: String,
}

#[derive(Debug, Serialize, Default)]
pub struct IndicatorOutput {
    pub lines: Vec<Line>,
    pub signals: Vec<Signal>,
}

type Series = Vec<Option<f64>>;

// ------------------------------------------------------------ primitives ---

pub fn sma(v: &[f64], n: usize) -> Series {
    let mut out = vec![None; v.len()];
    if n == 0 || v.len() < n {
        return out;
    }
    let mut sum: f64 = v[..n].iter().sum();
    out[n - 1] = Some(sum / n as f64);
    for i in n..v.len() {
        sum += v[i] - v[i - n];
        out[i] = Some(sum / n as f64);
    }
    out
}

/// EMA seeded with the SMA of the first `n` values.
pub fn ema(v: &[f64], n: usize) -> Series {
    let mut out = vec![None; v.len()];
    if n == 0 || v.len() < n {
        return out;
    }
    let k = 2.0 / (n as f64 + 1.0);
    let mut prev = v[..n].iter().sum::<f64>() / n as f64;
    out[n - 1] = Some(prev);
    for i in n..v.len() {
        prev = v[i] * k + prev * (1.0 - k);
        out[i] = Some(prev);
    }
    out
}

/// EMA over a series that starts with `None`s (used for the MACD signal).
fn ema_opt(s: &Series, n: usize) -> Series {
    let first = s.iter().position(|x| x.is_some());
    let mut out = vec![None; s.len()];
    let Some(first) = first else { return out };
    let vals: Vec<f64> = s[first..].iter().map(|x| x.unwrap_or(0.0)).collect();
    for (i, x) in ema(&vals, n).into_iter().enumerate() {
        out[first + i] = x;
    }
    out
}

pub fn rsi(close: &[f64], n: usize) -> Series {
    let mut out = vec![None; close.len()];
    if n == 0 || close.len() <= n {
        return out;
    }
    let (mut gain, mut loss) = (0.0, 0.0);
    for i in 1..=n {
        let d = close[i] - close[i - 1];
        gain += d.max(0.0);
        loss += (-d).max(0.0);
    }
    gain /= n as f64;
    loss /= n as f64;
    let val = |g: f64, l: f64| if l == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + g / l) };
    out[n] = Some(val(gain, loss));
    for i in n + 1..close.len() {
        let d = close[i] - close[i - 1];
        gain = (gain * (n as f64 - 1.0) + d.max(0.0)) / n as f64;
        loss = (loss * (n as f64 - 1.0) + (-d).max(0.0)) / n as f64;
        out[i] = Some(val(gain, loss));
    }
    out
}

pub struct Macd {
    pub macd: Series,
    pub signal: Series,
    pub hist: Series,
}

pub fn macd(close: &[f64], fast: usize, slow: usize, signal: usize) -> Macd {
    let f = ema(close, fast);
    let s = ema(close, slow);
    let line: Series = f.iter().zip(&s).map(|(a, b)| Some((*a)? - (*b)?)).collect();
    let sig = ema_opt(&line, signal);
    let hist = line.iter().zip(&sig).map(|(m, s)| Some((*m)? - (*s)?)).collect();
    Macd { macd: line, signal: sig, hist }
}

/// Bollinger bands: (middle, upper, lower), population std-dev.
pub fn bollinger(close: &[f64], n: usize, mult: f64) -> (Series, Series, Series) {
    let mid = sma(close, n);
    let mut up = vec![None; close.len()];
    let mut lo = vec![None; close.len()];
    for i in 0..close.len() {
        if let Some(m) = mid[i] {
            let w = &close[i + 1 - n..=i];
            let sd = (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n as f64).sqrt();
            up[i] = Some(m + mult * sd);
            lo[i] = Some(m - mult * sd);
        }
    }
    (mid, up, lo)
}

/// Session VWAP (resets each IST day). Falls back to the running mean of the
/// typical price when there is no volume (indices).
pub fn vwap(bars: &[Bar]) -> Series {
    let mut out = vec![None; bars.len()];
    let (mut day, mut pv, mut vol, mut tp_sum, mut count) = (i64::MIN, 0.0, 0.0, 0.0, 0.0);
    for (i, b) in bars.iter().enumerate() {
        let d = ist_day(b.ts);
        if d != day {
            day = d;
            pv = 0.0;
            vol = 0.0;
            tp_sum = 0.0;
            count = 0.0;
        }
        let tp = (b.high + b.low + b.close) / 3.0;
        pv += tp * b.volume;
        vol += b.volume;
        tp_sum += tp;
        count += 1.0;
        out[i] = Some(if vol > 0.0 { pv / vol } else { tp_sum / count });
    }
    out
}

pub struct Supertrend {
    pub up: Series,
    pub down: Series,
    /// +1 uptrend, -1 downtrend (None during warm-up)
    pub dir: Vec<Option<i8>>,
}

pub fn supertrend(bars: &[Bar], n: usize, mult: f64) -> Supertrend {
    let len = bars.len();
    let mut st = Supertrend { up: vec![None; len], down: vec![None; len], dir: vec![None; len] };
    if n == 0 || len <= n {
        return st;
    }
    let tr: Vec<f64> = (0..len)
        .map(|i| {
            let hl = bars[i].high - bars[i].low;
            if i == 0 {
                hl
            } else {
                let pc = bars[i - 1].close;
                hl.max((bars[i].high - pc).abs()).max((bars[i].low - pc).abs())
            }
        })
        .collect();
    // Wilder ATR, first value at index n.
    let mut atr = tr[1..=n].iter().sum::<f64>() / n as f64;
    let (mut fu, mut fl, mut trend_up) = (0.0, 0.0, true);
    for i in n..len {
        if i > n {
            atr = (atr * (n as f64 - 1.0) + tr[i]) / n as f64;
        }
        let hl2 = (bars[i].high + bars[i].low) / 2.0;
        let (bu, bl) = (hl2 + mult * atr, hl2 - mult * atr);
        if i == n {
            fu = bu;
            fl = bl;
            trend_up = bars[i].close >= hl2;
        } else {
            let pc = bars[i - 1].close;
            fu = if bu < fu || pc > fu { bu } else { fu };
            fl = if bl > fl || pc < fl { bl } else { fl };
            if trend_up && bars[i].close < fl {
                trend_up = false;
            } else if !trend_up && bars[i].close > fu {
                trend_up = true;
            }
        }
        st.dir[i] = Some(if trend_up { 1 } else { -1 });
        if trend_up {
            st.up[i] = Some(fl);
        } else {
            st.down[i] = Some(fu);
        }
    }
    st
}

/// Indices where `a` crosses `b`; `true` = upward cross.
fn crosses(a: &Series, b: &Series) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    for i in 1..a.len().min(b.len()) {
        if let (Some(a0), Some(b0), Some(a1), Some(b1)) = (a[i - 1], b[i - 1], a[i], b[i]) {
            if a0 <= b0 && a1 > b1 {
                out.push((i, true));
            } else if a0 >= b0 && a1 < b1 {
                out.push((i, false));
            }
        }
    }
    out
}

// --------------------------------------------------------------- compute ---

fn points(bars: &[Bar], s: &Series) -> Vec<Point> {
    bars.iter().zip(s).map(|(b, v)| Point { time: b.ts, value: *v }).collect()
}

fn line(bars: &[Bar], id: String, label: String, pane: &str, style: &str, tone: Option<&str>, s: &Series) -> Line {
    Line {
        id,
        label,
        pane: pane.into(),
        style: style.into(),
        tone: tone.map(String::from),
        points: points(bars, s),
    }
}

fn signal(bars: &[Bar], i: usize, buy: bool, source: &str) -> Signal {
    Signal {
        time: bars[i].ts,
        side: if buy { "buy" } else { "sell" }.into(),
        price: bars[i].close,
        source: source.into(),
    }
}

pub fn compute(bars: &[Bar], specs: &[IndicatorSpec]) -> IndicatorOutput {
    let mut out = IndicatorOutput::default();
    if bars.is_empty() {
        return out;
    }
    let close: Vec<f64> = bars.iter().map(|b| b.close).collect();
    // Moving averages of the same kind: the fastest/slowest pair gives signals.
    let mut mas: BTreeMap<&str, Vec<(usize, Series)>> = BTreeMap::new();

    for sp in specs {
        match sp.kind.as_str() {
            k @ ("sma" | "ema") => {
                let n = sp.period.unwrap_or(20).max(1);
                let s = if k == "sma" { sma(&close, n) } else { ema(&close, n) };
                out.lines.push(line(bars, format!("{k}{n}"), format!("{} {n}", k.to_uppercase()), "price", "line", None, &s));
                mas.entry(if k == "sma" { "SMA" } else { "EMA" }).or_default().push((n, s));
            }
            "rsi" => {
                let n = sp.period.unwrap_or(14).max(1);
                let s = rsi(&close, n);
                let (lo, hi): (Series, Series) = (vec![Some(30.0); s.len()], vec![Some(70.0); s.len()]);
                for (i, up) in crosses(&s, &lo) {
                    if up {
                        out.signals.push(signal(bars, i, true, "RSI"));
                    }
                }
                for (i, up) in crosses(&s, &hi) {
                    if !up {
                        out.signals.push(signal(bars, i, false, "RSI"));
                    }
                }
                out.lines.push(line(bars, format!("rsi{n}"), format!("RSI {n}"), "rsi", "line", None, &s));
            }
            "macd" => {
                let (f, s, g) = (sp.fast.unwrap_or(12), sp.slow.unwrap_or(26), sp.signal.unwrap_or(9));
                let m = macd(&close, f.max(1), s.max(1), g.max(1));
                for (i, up) in crosses(&m.macd, &m.signal) {
                    out.signals.push(signal(bars, i, up, "MACD"));
                }
                out.lines.push(line(bars, "macd_hist".into(), "MACD hist".into(), "macd", "histogram", None, &m.hist));
                out.lines.push(line(bars, "macd_line".into(), format!("MACD {f},{s},{g}"), "macd", "line", None, &m.macd));
                out.lines.push(line(bars, "macd_signal".into(), "Signal".into(), "macd", "line", Some("down"), &m.signal));
            }
            "bb" => {
                let n = sp.period.unwrap_or(20).max(1);
                let k = sp.mult.unwrap_or(2.0);
                let (mid, up, lo) = bollinger(&close, n, k);
                // Close back inside the band after a breach.
                for i in 1..close.len() {
                    if let (Some(l0), Some(l1), Some(u0), Some(u1)) = (lo[i - 1], lo[i], up[i - 1], up[i]) {
                        if close[i - 1] < l0 && close[i] >= l1 {
                            out.signals.push(signal(bars, i, true, "BB"));
                        } else if close[i - 1] > u0 && close[i] <= u1 {
                            out.signals.push(signal(bars, i, false, "BB"));
                        }
                    }
                }
                out.lines.push(line(bars, format!("bb_up{n}"), format!("BB {n} upper"), "price", "line", None, &up));
                out.lines.push(line(bars, format!("bb_mid{n}"), format!("BB {n} mid"), "price", "line", None, &mid));
                out.lines.push(line(bars, format!("bb_lo{n}"), format!("BB {n} lower"), "price", "line", None, &lo));
            }
            "vwap" => {
                out.lines.push(line(bars, "vwap".into(), "VWAP".into(), "price", "line", None, &vwap(bars)));
            }
            "supertrend" => {
                let n = sp.period.unwrap_or(10).max(1);
                let m = sp.mult.unwrap_or(3.0);
                let st = supertrend(bars, n, m);
                for i in 1..bars.len() {
                    if let (Some(a), Some(b)) = (st.dir[i - 1], st.dir[i]) {
                        if a != b {
                            out.signals.push(signal(bars, i, b > 0, "Supertrend"));
                        }
                    }
                }
                out.lines.push(line(bars, format!("st_up{n}"), format!("Supertrend {n},{m}"), "price", "line", Some("up"), &st.up));
                out.lines.push(line(bars, format!("st_dn{n}"), "Supertrend (down)".into(), "price", "line", Some("down"), &st.down));
            }
            _ => {}
        }
    }

    for (name, list) in mas {
        if list.len() >= 2 {
            let fast = list.iter().min_by_key(|(n, _)| *n).unwrap();
            let slow = list.iter().max_by_key(|(n, _)| *n).unwrap();
            if fast.0 != slow.0 {
                let src = format!("{name} {}/{}", fast.0, slow.0);
                for (i, up) in crosses(&fast.1, &slow.1) {
                    out.signals.push(signal(bars, i, up, &src));
                }
            }
        }
    }
    out.signals.sort_by_key(|s| s.time);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars_from(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| Bar { ts: 1_793_850_300 + i as i64 * 60, open: *c, high: c + 1.0, low: c - 1.0, close: *c, volume: 10.0, oi: None })
            .collect()
    }

    #[test]
    fn sma_and_ema_basics() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0];
        let s = sma(&v, 3);
        assert_eq!(s, vec![None, None, Some(2.0), Some(3.0), Some(4.0)]);
        let e = ema(&v, 3);
        assert_eq!(e[2], Some(2.0));
        assert!((e[3].unwrap() - 3.0).abs() < 1e-9); // k=0.5: 4*.5+2*.5
    }

    #[test]
    fn rsi_extremes() {
        let up: Vec<f64> = (0..30).map(|i| 100.0 + i as f64).collect();
        assert_eq!(rsi(&up, 14)[29], Some(100.0));
        let down: Vec<f64> = (0..30).map(|i| 100.0 - i as f64).collect();
        assert!(rsi(&down, 14)[29].unwrap() < 1e-9);
    }

    #[test]
    fn bollinger_flat_series_has_zero_width() {
        let (m, u, l) = bollinger(&[5.0; 25], 20, 2.0);
        assert_eq!((m[24], u[24], l[24]), (Some(5.0), Some(5.0), Some(5.0)));
    }

    #[test]
    fn ema_cross_emits_signals() {
        // Falling then rising: fast EMA crosses above slow, later back down.
        let mut c: Vec<f64> = (0..30).map(|i| 100.0 - i as f64).collect();
        c.extend((0..40).map(|i| 70.0 + i as f64 * 2.0));
        c.extend((0..40).map(|i| 150.0 - i as f64 * 3.0));
        let b = bars_from(&c);
        let specs = vec![
            IndicatorSpec { kind: "ema".into(), period: Some(5), fast: None, slow: None, signal: None, mult: None },
            IndicatorSpec { kind: "ema".into(), period: Some(20), fast: None, slow: None, signal: None, mult: None },
        ];
        let out = compute(&b, &specs);
        assert_eq!(out.lines.len(), 2);
        assert!(out.signals.iter().any(|s| s.side == "buy"));
        assert!(out.signals.iter().any(|s| s.side == "sell"));
    }

    #[test]
    fn supertrend_flips_on_reversal() {
        let mut c: Vec<f64> = (0..40).map(|i| 100.0 + i as f64).collect();
        c.extend((0..40).map(|i| 140.0 - i as f64 * 2.0));
        let b = bars_from(&c);
        let st = supertrend(&b, 10, 2.0);
        assert_eq!(st.dir[30], Some(1));
        assert_eq!(*st.dir.last().unwrap(), Some(-1));
        let out = compute(&b, &[IndicatorSpec { kind: "supertrend".into(), period: Some(10), fast: None, slow: None, signal: None, mult: Some(2.0) }]);
        assert!(out.signals.iter().any(|s| s.side == "sell"));
    }

    #[test]
    fn vwap_resets_and_uses_volume() {
        let b = bars_from(&[10.0, 20.0]);
        let v = vwap(&b);
        assert!((v[1].unwrap() - 15.0).abs() < 1e-9);
    }

    #[test]
    fn macd_lengths_line_up() {
        let c: Vec<f64> = (0..60).map(|i| 100.0 + (i as f64 / 5.0).sin() * 5.0).collect();
        let m = macd(&c, 12, 26, 9);
        assert_eq!(m.macd.len(), 60);
        assert!(m.macd[25].is_some() && m.macd[24].is_none());
        assert!(m.hist[33].is_some() && m.hist[32].is_none());
    }
}
