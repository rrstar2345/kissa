//! Configurable signal rules.
//!
//! A [`Ruleset`] is stored as JSON in SQLite. It has *variables* (indicators,
//! optionally chained: the source of one can be another variable) and two
//! condition trees: `entry` (BUY) and `exit` (SELL). `eval` runs a ruleset over
//! bars and is independent of the chart, so alerts, algos and backtests can
//! reuse it.

pub mod eval;
pub mod store;

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Price sources usable as an operand or indicator source. `avg` = `ohlc4`.
pub const SOURCES: [&str; 8] = ["open", "high", "low", "close", "hl2", "hlc3", "ohlc4", "avg"];
pub const CMP_OPS: [&str; 8] = ["crosses_above", "crosses_below", ">", ">=", "<", "<=", "=", "<>"];
const MAX_DEPTH: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Ruleset {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub vars: Vec<Var>,
    #[serde(default)]
    pub entry: Group,
    #[serde(default)]
    pub exit: Group,
}

/// One rule variable. `kind`: sma | ema | rsi | macd | bb | atr | vwap |
/// supertrend | formula. Unused fields stay `None`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Var {
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
    /// Price source or the id of an earlier variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mult: Option<f64>,
    /// macd: line|signal|hist; bb: upper|mid|lower; supertrend: line|dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// formula: `left op right` with op in + - * /; operands as in conditions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<String>,
    /// Draw this variable on the chart.
    #[serde(default)]
    pub plot: bool,
}

/// AND/OR group of conditions and nested groups. An empty group never fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    #[serde(default = "and")]
    pub op: String,
    #[serde(default)]
    pub items: Vec<Node>,
}

fn and() -> String {
    "and".into()
}

impl Default for Group {
    fn default() -> Self {
        Group { op: and(), items: Vec::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Node {
    Cond(Cond),
    Group(Group),
}

/// `left op right`. Operands are a price source, a variable id or a number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cond {
    pub left: String,
    pub op: String,
    pub right: String,
}

pub fn is_number(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|x| x.is_finite())
}

pub fn is_source(s: &str) -> bool {
    SOURCES.contains(&s.trim())
}

fn valid_id(id: &str) -> bool {
    let mut c = id.chars();
    let ok = match c.next() {
        Some(f) if f.is_ascii_alphabetic() || f == '_' => c.all(|x| x.is_ascii_alphanumeric() || x == '_'),
        _ => false,
    };
    ok && id.len() <= 24 && id.parse::<f64>().is_err() && !is_source(id)
}

fn count_conds(g: &Group) -> usize {
    g.items
        .iter()
        .map(|n| match n {
            Node::Cond(_) => 1,
            Node::Group(g) => count_conds(g),
        })
        .sum()
}

pub fn has_conditions(g: &Group) -> bool {
    count_conds(g) > 0
}

impl Ruleset {
    /// Human-readable problems; empty when the ruleset is usable.
    pub fn validate(&self) -> Vec<String> {
        let mut errs: Vec<String> = Vec::new();
        if self.name.trim().is_empty() {
            errs.push("Name is required".into());
        }
        let mut known: HashSet<String> = HashSet::new();
        for (idx, v) in self.vars.iter().enumerate() {
            let label = if v.id.is_empty() { format!("Variable #{}", idx + 1) } else { format!("Variable {}", v.id) };
            if !valid_id(&v.id) {
                errs.push(format!("{label}: id must be letters, digits or _ (not starting with a digit, not a price source)"));
            } else if known.contains(&v.id) {
                errs.push(format!("{label}: duplicate id"));
            }
            validate_var(v, &label, &known, &mut errs);
            known.insert(v.id.clone());
        }
        for (name, g) in [("Entry", &self.entry), ("Exit", &self.exit)] {
            validate_group(g, name, &known, 0, &mut errs);
        }
        if !has_conditions(&self.entry) {
            errs.push("Entry needs at least one condition".into());
        }
        errs
    }
}

fn range_ok(v: Option<usize>, what: &str, label: &str, errs: &mut Vec<String>) {
    if let Some(n) = v {
        if n == 0 || n > 1000 {
            errs.push(format!("{label}: {what} must be 1 to 1000"));
        }
    }
}

fn mult_ok(v: Option<f64>, label: &str, errs: &mut Vec<String>) {
    if let Some(m) = v {
        if !m.is_finite() || m <= 0.0 {
            errs.push(format!("{label}: multiplier must be above 0"));
        }
    }
}

fn operand_ok(s: &str, known: &HashSet<String>) -> bool {
    is_number(s).is_some() || is_source(s) || known.contains(s.trim())
}

fn validate_var(v: &Var, label: &str, known: &HashSet<String>, errs: &mut Vec<String>) {
    let src_check = |errs: &mut Vec<String>| {
        let s = v.src.as_deref().unwrap_or("close");
        if !(is_source(s) || known.contains(s.trim())) {
            errs.push(format!("{label}: source '{s}' is not a price source or an earlier variable"));
        }
    };
    let out_check = |allowed: &[&str], errs: &mut Vec<String>| {
        if let Some(o) = v.output.as_deref() {
            if !allowed.contains(&o) {
                errs.push(format!("{label}: output must be one of {}", allowed.join(", ")));
            }
        }
    };
    match v.kind.as_str() {
        "sma" | "ema" | "rsi" => {
            range_ok(v.length, "length", label, errs);
            src_check(errs);
        }
        "macd" => {
            range_ok(v.fast, "fast", label, errs);
            range_ok(v.slow, "slow", label, errs);
            range_ok(v.signal, "signal", label, errs);
            if v.fast.unwrap_or(12) >= v.slow.unwrap_or(26) {
                errs.push(format!("{label}: fast must be less than slow"));
            }
            out_check(&["line", "signal", "hist"], errs);
            src_check(errs);
        }
        "bb" => {
            range_ok(v.length, "length", label, errs);
            mult_ok(v.mult, label, errs);
            out_check(&["upper", "mid", "lower"], errs);
            src_check(errs);
        }
        "atr" => range_ok(v.length, "length", label, errs),
        "vwap" => {}
        "supertrend" => {
            range_ok(v.length, "length", label, errs);
            mult_ok(v.mult, label, errs);
            out_check(&["line", "dir"], errs);
        }
        "formula" => {
            let (l, r) = (v.left.as_deref().unwrap_or(""), v.right.as_deref().unwrap_or(""));
            if !matches!(v.op.as_deref(), Some("+" | "-" | "*" | "/")) {
                errs.push(format!("{label}: operator must be + - * or /"));
            }
            for (side, s) in [("left", l), ("right", r)] {
                if !operand_ok(s, known) {
                    errs.push(format!("{label}: {side} operand '{s}' is not a number, price source or earlier variable"));
                }
            }
        }
        other => errs.push(format!("{label}: unknown kind '{other}'")),
    }
}

fn validate_group(g: &Group, path: &str, known: &HashSet<String>, depth: usize, errs: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        errs.push(format!("{path}: groups are nested too deeply"));
        return;
    }
    if g.op != "and" && g.op != "or" {
        errs.push(format!("{path}: group operator must be and/or"));
    }
    for (i, n) in g.items.iter().enumerate() {
        let here = format!("{path} #{}", i + 1);
        match n {
            Node::Group(inner) => validate_group(inner, &here, known, depth + 1, errs),
            Node::Cond(c) => {
                if !CMP_OPS.contains(&c.op.as_str()) {
                    errs.push(format!("{here}: unknown operator '{}'", c.op));
                }
                for (side, s) in [("left", &c.left), ("right", &c.right)] {
                    if !operand_ok(s, known) {
                        errs.push(format!("{here}: {side} operand '{s}' is not a number, price source or variable"));
                    }
                }
            }
        }
    }
}
