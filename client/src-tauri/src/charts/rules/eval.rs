//! Ruleset evaluation over bars: variable series, plotted lines and
//! entry/exit signals. Pure (no I/O), so it can be reused by alerts, algos
//! and backtests.

use std::collections::HashMap;

use super::{has_conditions, is_number, Cond, Group, Node, Ruleset, Var};
use crate::charts::indicators::{self as ind, Line, Point, Series, Signal};
use crate::charts::Bar;

#[derive(Debug, Default)]
pub struct RuleOutput {
    pub lines: Vec<Line>,
    pub signals: Vec<Signal>,
}

enum Operand<'a> {
    Const(f64),
    Series(&'a Series),
}

impl Operand<'_> {
    fn at(&self, i: usize) -> Option<f64> {
        match self {
            Operand::Const(c) => Some(*c),
            Operand::Series(s) => s.get(i).copied().flatten(),
        }
    }
}

fn operand<'a>(name: &str, ctx: &'a HashMap<String, Series>) -> Option<Operand<'a>> {
    if let Some(c) = is_number(name) {
        return Some(Operand::Const(c));
    }
    ctx.get(name.trim()).map(Operand::Series)
}

fn sources(bars: &[Bar]) -> HashMap<String, Series> {
    let mk = |f: &dyn Fn(&Bar) -> f64| -> Series { bars.iter().map(|b| Some(f(b))).collect() };
    let mut m = HashMap::new();
    m.insert("open".to_string(), mk(&|b| b.open));
    m.insert("high".to_string(), mk(&|b| b.high));
    m.insert("low".to_string(), mk(&|b| b.low));
    m.insert("close".to_string(), mk(&|b| b.close));
    m.insert("hl2".to_string(), mk(&|b| (b.high + b.low) / 2.0));
    m.insert("hlc3".to_string(), mk(&|b| (b.high + b.low + b.close) / 3.0));
    let ohlc4 = mk(&|b| (b.open + b.high + b.low + b.close) / 4.0);
    m.insert("avg".to_string(), ohlc4.clone());
    m.insert("ohlc4".to_string(), ohlc4);
    m
}

/// Run a plain-slice indicator over a series that may start with `None`s
/// (a chained source). Gaps after the first value carry the last value.
fn over<F: Fn(&[f64]) -> Series>(s: &Series, f: F) -> Series {
    let mut out: Series = vec![None; s.len()];
    let Some(first) = s.iter().position(|x| x.is_some()) else { return out };
    let mut prev = 0.0;
    let vals: Vec<f64> = s[first..]
        .iter()
        .map(|x| {
            if let Some(v) = x {
                prev = *v;
            }
            prev
        })
        .collect();
    for (i, v) in f(&vals).into_iter().enumerate() {
        out[first + i] = v;
    }
    out
}

fn compute_var(v: &Var, bars: &[Bar], ctx: &HashMap<String, Series>) -> Result<Series, String> {
    let src_name = v.src.as_deref().unwrap_or("close").trim();
    let src = || ctx.get(src_name).ok_or_else(|| format!("{}: unknown source '{src_name}'", v.id));
    let len = |d: usize| v.length.unwrap_or(d).max(1);
    Ok(match v.kind.as_str() {
        "sma" => {
            let n = len(20);
            over(src()?, |x| ind::sma(x, n))
        }
        "ema" => {
            let n = len(20);
            over(src()?, |x| ind::ema(x, n))
        }
        "rsi" => {
            let n = len(14);
            over(src()?, |x| ind::rsi(x, n))
        }
        "macd" => {
            let (f, s, g) = (v.fast.unwrap_or(12).max(1), v.slow.unwrap_or(26).max(1), v.signal.unwrap_or(9).max(1));
            let which = v.output.clone().unwrap_or_else(|| "line".into());
            over(src()?, move |x| {
                let m = ind::macd(x, f, s, g);
                match which.as_str() {
                    "signal" => m.signal,
                    "hist" => m.hist,
                    _ => m.macd,
                }
            })
        }
        "bb" => {
            let n = len(20);
            let k = v.mult.unwrap_or(2.0);
            let which = v.output.clone().unwrap_or_else(|| "mid".into());
            over(src()?, move |x| {
                let (m, u, l) = ind::bollinger(x, n, k);
                match which.as_str() {
                    "upper" => u,
                    "lower" => l,
                    _ => m,
                }
            })
        }
        "atr" => ind::atr(bars, len(14)),
        "vwap" => ind::vwap(bars),
        "supertrend" => {
            let st = ind::supertrend(bars, len(10), v.mult.unwrap_or(3.0));
            if v.output.as_deref() == Some("dir") {
                st.dir.iter().map(|d| d.map(|x| x as f64)).collect()
            } else {
                (0..bars.len()).map(|i| st.up[i].or(st.down[i])).collect()
            }
        }
        "formula" => {
            let l = operand(v.left.as_deref().unwrap_or(""), ctx).ok_or_else(|| format!("{}: bad left operand", v.id))?;
            let r = operand(v.right.as_deref().unwrap_or(""), ctx).ok_or_else(|| format!("{}: bad right operand", v.id))?;
            let op = v.op.clone().unwrap_or_default();
            (0..bars.len())
                .map(|i| {
                    let (a, b) = (l.at(i)?, r.at(i)?);
                    match op.as_str() {
                        "+" => Some(a + b),
                        "-" => Some(a - b),
                        "*" => Some(a * b),
                        "/" => (b != 0.0).then(|| a / b),
                        _ => None,
                    }
                })
                .collect()
        }
        other => return Err(format!("{}: unknown kind '{other}'", v.id)),
    })
}

/// Chart pane for a variable: it follows its source so a chained variable
/// (EMA of RSI) shares the pane of what it is built on.
fn pane_of(v: &Var, panes: &HashMap<String, String>) -> String {
    let follow = |name: Option<&str>| name.and_then(|n| panes.get(n.trim()).cloned());
    match v.kind.as_str() {
        "rsi" => "rsi".into(),
        "macd" => "macd".into(),
        "atr" => "atr".into(),
        "supertrend" if v.output.as_deref() == Some("dir") => "st_dir".into(),
        "sma" | "ema" | "bb" => follow(v.src.as_deref()).unwrap_or_else(|| "price".into()),
        "formula" => follow(v.left.as_deref()).or_else(|| follow(v.right.as_deref())).unwrap_or_else(|| "price".into()),
        _ => "price".into(),
    }
}

fn eval_cond(c: &Cond, ctx: &HashMap<String, Series>, i: usize) -> bool {
    let (Some(l), Some(r)) = (operand(&c.left, ctx), operand(&c.right, ctx)) else { return false };
    let (a, b) = match (l.at(i), r.at(i)) {
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    match c.op.as_str() {
        ">" => a > b,
        ">=" => a >= b,
        "<" => a < b,
        "<=" => a <= b,
        "=" => (a - b).abs() < 1e-9,
        "<>" => (a - b).abs() >= 1e-9,
        "crosses_above" | "crosses_below" => {
            if i == 0 {
                return false;
            }
            let (Some(a0), Some(b0)) = (l.at(i - 1), r.at(i - 1)) else { return false };
            if c.op == "crosses_above" {
                a0 <= b0 && a > b
            } else {
                a0 >= b0 && a < b
            }
        }
        _ => false,
    }
}

fn eval_group(g: &Group, ctx: &HashMap<String, Series>, i: usize) -> bool {
    let mut results = g.items.iter().map(|n| match n {
        Node::Cond(c) => eval_cond(c, ctx, i),
        Node::Group(inner) => eval_group(inner, ctx, i),
    });
    if g.op == "or" {
        results.any(|x| x)
    } else {
        !g.items.is_empty() && results.all(|x| x)
    }
}

/// Price sources plus every variable of `rs` as series, and the chart pane of
/// each variable.
fn context(rs: &Ruleset, bars: &[Bar]) -> Result<(HashMap<String, Series>, HashMap<String, String>), String> {
    let mut ctx = sources(bars);
    let mut panes: HashMap<String, String> = HashMap::new();
    for v in &rs.vars {
        let s = compute_var(v, bars, &ctx)?;
        panes.insert(v.id.clone(), pane_of(v, &panes));
        ctx.insert(v.id.clone(), s);
    }
    Ok((ctx, panes))
}

/// What a ruleset says to do on the latest closed bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Entry,
    Exit,
}

/// Live decision for an algo: look at the latest closed bar (`closed - 1`)
/// only. The caller knows whether a position is really open (`in_pos`), so
/// unlike `evaluate` the answer does not depend on how much history was
/// loaded: flat -> `Entry` when the entry conditions hold; in a position ->
/// `Exit` when the exit conditions hold (never when there are none).
pub fn decide(rs: &Ruleset, bars: &[Bar], closed: usize, in_pos: bool) -> Result<Option<Decision>, String> {
    let errs = rs.validate();
    if !errs.is_empty() {
        return Err(errs.join("; "));
    }
    let closed = closed.min(bars.len());
    if closed == 0 {
        return Ok(None);
    }
    let (ctx, _) = context(rs, bars)?;
    let i = closed - 1;
    Ok(if in_pos {
        (has_conditions(&rs.exit) && eval_group(&rs.exit, &ctx, i)).then_some(Decision::Exit)
    } else {
        eval_group(&rs.entry, &ctx, i).then_some(Decision::Entry)
    })
}

/// Evaluate `rs` over `bars`. Only bars `[..closed]` can produce signals (the
/// forming candle is excluded so signals never repaint). Entry fires only when
/// flat and Exit only when in a position; with no Exit conditions Entry is not
/// gated and fires on every bar where it is true.
pub fn evaluate(rs: &Ruleset, bars: &[Bar], closed: usize) -> Result<RuleOutput, String> {
    let errs = rs.validate();
    if !errs.is_empty() {
        return Err(errs.join("; "));
    }
    let mut out = RuleOutput::default();
    if bars.is_empty() {
        return Ok(out);
    }
    let (ctx, panes) = context(rs, bars)?;

    let rid = if rs.id.is_empty() { "draft" } else { rs.id.as_str() };
    for v in rs.vars.iter().filter(|v| v.plot) {
        let s = &ctx[&v.id];
        out.lines.push(Line {
            id: format!("rs:{rid}:{}", v.id),
            label: v.id.clone(),
            pane: panes[&v.id].clone(),
            style: "line".into(),
            tone: None,
            points: bars.iter().zip(s).map(|(b, x)| Point { time: b.ts, value: *x }).collect(),
        });
    }

    let gated = has_conditions(&rs.exit);
    let mut in_pos = false;
    let mk = |i: usize, buy: bool| Signal {
        time: bars[i].ts,
        side: if buy { "buy" } else { "sell" }.into(),
        price: bars[i].close,
        source: format!("{}: {}", rs.name, if buy { "Entry" } else { "Exit" }),
    };
    for i in 0..closed.min(bars.len()) {
        if gated && in_pos {
            if eval_group(&rs.exit, &ctx, i) {
                out.signals.push(mk(i, false));
                in_pos = false;
            }
        } else if eval_group(&rs.entry, &ctx, i) {
            out.signals.push(mk(i, true));
            in_pos = gated;
        }
    }
    Ok(out)
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

    fn cond(l: &str, op: &str, r: &str) -> Node {
        Node::Cond(Cond { left: l.into(), op: op.into(), right: r.into() })
    }

    fn var(id: &str, kind: &str) -> Var {
        Var { id: id.into(), kind: kind.into(), ..Default::default() }
    }

    fn rs(vars: Vec<Var>, entry: Vec<Node>, exit: Vec<Node>) -> Ruleset {
        Ruleset {
            id: "t".into(),
            name: "T".into(),
            vars,
            entry: Group { op: "and".into(), items: entry },
            exit: Group { op: "and".into(), items: exit },
        }
    }

    fn zigzag() -> Vec<f64> {
        let mut c: Vec<f64> = (0..30).map(|i| 100.0 - i as f64).collect();
        c.extend((0..40).map(|i| 70.0 + i as f64 * 2.0));
        c.extend((0..40).map(|i| 150.0 - i as f64 * 3.0));
        c
    }

    #[test]
    fn close_crossing_sma_gives_alternating_signals() {
        let mut sma = var("s", "sma");
        sma.length = Some(10);
        sma.plot = true;
        let r = rs(vec![sma], vec![cond("close", "crosses_above", "s")], vec![cond("close", "crosses_below", "s")]);
        let b = bars_from(&zigzag());
        let out = evaluate(&r, &b, b.len()).unwrap();
        assert!(out.signals.len() >= 2);
        for (i, s) in out.signals.iter().enumerate() {
            assert_eq!(s.side, if i % 2 == 0 { "buy" } else { "sell" });
        }
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].pane, "price");
    }

    #[test]
    fn chained_ema_of_rsi_shares_rsi_pane() {
        let mut rsi = var("rsi1", "rsi");
        rsi.length = Some(14);
        let mut ema = var("ema1", "ema");
        ema.length = Some(9);
        ema.src = Some("rsi1".into());
        ema.plot = true;
        let r = rs(vec![rsi, ema], vec![cond("rsi1", "crosses_above", "ema1")], vec![]);
        let b = bars_from(&zigzag());
        let out = evaluate(&r, &b, b.len()).unwrap();
        assert_eq!(out.lines[0].pane, "rsi");
        // RSI needs 14 bars, then the EMA needs 9 more values.
        let first = out.lines[0].points.iter().position(|p| p.value.is_some()).unwrap();
        assert_eq!(first, 14 + 8);
    }

    #[test]
    fn ungated_entry_fires_repeatedly_and_closed_limit_applies() {
        let r = rs(vec![], vec![cond("close", ">", "100")], vec![]);
        let b = bars_from(&[99.0, 101.0, 102.0, 103.0]);
        assert_eq!(evaluate(&r, &b, 4).unwrap().signals.len(), 3);
        assert_eq!(evaluate(&r, &b, 3).unwrap().signals.len(), 2);
    }

    #[test]
    fn decide_looks_at_the_latest_closed_bar_and_the_real_position() {
        let r = rs(vec![], vec![cond("close", ">", "100")], vec![cond("close", "<", "100")]);
        let b = bars_from(&[99.0, 101.0, 99.0]);
        // Flat: entry is true on bar 1 (index 1), false on bar 2.
        assert_eq!(decide(&r, &b, 2, false).unwrap(), Some(Decision::Entry));
        assert_eq!(decide(&r, &b, 3, false).unwrap(), None);
        // In a position: exit is true on the last bar, entry is not asked.
        assert_eq!(decide(&r, &b, 3, true).unwrap(), Some(Decision::Exit));
        assert_eq!(decide(&r, &b, 2, true).unwrap(), None);
        assert_eq!(decide(&r, &b, 0, false).unwrap(), None);
        // No exit conditions: a position is never exited by the signal.
        let ungated = rs(vec![], vec![cond("close", ">", "100")], vec![]);
        assert_eq!(decide(&ungated, &b, 3, true).unwrap(), None);
        // Invalid rulesets are refused.
        assert!(decide(&rs(vec![], vec![], vec![]), &b, 3, false).is_err());
    }

    #[test]
    fn equality_operators() {
        let r = rs(vec![], vec![cond("close", "=", "101")], vec![]);
        let b = bars_from(&[100.0, 101.0, 102.0]);
        assert_eq!(evaluate(&r, &b, 3).unwrap().signals.len(), 1);
        let r = rs(vec![], vec![cond("close", "<>", "101")], vec![]);
        assert_eq!(evaluate(&r, &b, 3).unwrap().signals.len(), 2);
    }

    #[test]
    fn or_group_and_nesting() {
        let nested = Node::Group(Group { op: "or".into(), items: vec![cond("close", "<", "100"), cond("close", ">", "102")] });
        let r = rs(vec![], vec![cond("open", ">", "0"), nested], vec![]);
        let b = bars_from(&[99.0, 101.0, 103.0]);
        assert_eq!(evaluate(&r, &b, 3).unwrap().signals.len(), 2);
    }

    #[test]
    fn formula_and_divide_by_zero() {
        let mut f = var("d", "formula");
        f.left = Some("close".into());
        f.op = Some("-".into());
        f.right = Some("open".into());
        let mut z = var("z", "formula");
        z.left = Some("close".into());
        z.op = Some("/".into());
        z.right = Some("0".into());
        z.plot = true;
        let r = rs(vec![f, z], vec![cond("d", "=", "0")], vec![]);
        let b = bars_from(&[1.0, 2.0]);
        let out = evaluate(&r, &b, 2).unwrap();
        assert_eq!(out.signals.len(), 2);
        assert!(out.lines[0].points.iter().all(|p| p.value.is_none()));
    }

    #[test]
    fn validation_catches_problems() {
        let mut ema = var("e", "ema");
        ema.src = Some("later".into());
        let r = rs(vec![ema, var("later", "rsi")], vec![cond("e", "bogus", "x")], vec![]);
        let errs = r.validate();
        assert!(errs.iter().any(|e| e.contains("source 'later'")));
        assert!(errs.iter().any(|e| e.contains("unknown operator")));
        assert!(errs.iter().any(|e| e.contains("right operand 'x'")));
        let empty = rs(vec![], vec![], vec![]);
        assert!(empty.validate().iter().any(|e| e.contains("Entry needs")));
        let dup = rs(vec![var("a", "vwap"), var("a", "vwap")], vec![cond("a", ">", "1")], vec![]);
        assert!(dup.validate().iter().any(|e| e.contains("duplicate")));
        let bad_id = rs(vec![var("close", "vwap"), var("1x", "vwap"), var("nan", "vwap")], vec![cond("close", ">", "1")], vec![]);
        assert_eq!(bad_id.validate().iter().filter(|e| e.contains("id must be")).count(), 3);
    }

    #[test]
    fn json_roundtrip_and_spec_example() {
        let json = r#"{
          "name":"RSI-EMA",
          "vars":[
            {"id":"rsi1","kind":"rsi","length":14,"src":"close"},
            {"id":"ema1","kind":"ema","length":9,"src":"rsi1"},
            {"id":"sma20","kind":"sma","length":20,"src":"close"},
            {"id":"sma50","kind":"sma","length":50,"src":"close"}],
          "entry":{"op":"and","items":[
            {"type":"cond","left":"rsi1","op":"crosses_above","right":"ema1"},
            {"type":"cond","left":"close","op":">","right":"sma20"},
            {"type":"cond","left":"close","op":">","right":"sma50"}]},
          "exit":{"op":"or","items":[
            {"type":"cond","left":"rsi1","op":"crosses_below","right":"ema1"}]}
        }"#;
        let r: Ruleset = serde_json::from_str(json).unwrap();
        assert!(r.validate().is_empty(), "{:?}", r.validate());
        let again: Ruleset = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(again.entry.items.len(), 3);
        let b = bars_from(&zigzag());
        assert!(evaluate(&r, &b, b.len()).is_ok());
    }
}
